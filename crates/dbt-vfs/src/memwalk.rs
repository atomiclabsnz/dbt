//! `walkdir`, over the VFS: the API dbt uses (`new`, `min_depth`,
//! `max_depth`, `follow_links`, `sort_by*`, `into_iter`, `filter_entry`,
//! `skip_current_dir`). A walk is pre-order and, unless sorted otherwise, in
//! file-name order. There are no symlinks, so `follow_links` changes nothing.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::memfs::{self, FileType, Metadata};

pub type Result<T> = std::result::Result<T, Error>;

type Sorter = Box<dyn FnMut(&DirEntry, &DirEntry) -> Ordering + Send + Sync + 'static>;

pub struct WalkDir {
    root: PathBuf,
    min_depth: usize,
    max_depth: usize,
    sorter: Option<Sorter>,
}

impl fmt::Debug for WalkDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalkDir")
            .field("root", &self.root)
            .field("min_depth", &self.min_depth)
            .field("max_depth", &self.max_depth)
            .finish()
    }
}

impl WalkDir {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            min_depth: 0,
            max_depth: usize::MAX,
            sorter: None,
        }
    }
    pub fn min_depth(mut self, depth: usize) -> Self {
        self.min_depth = depth;
        self.max_depth = self.max_depth.max(depth);
        self
    }
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self.min_depth = self.min_depth.min(depth);
        self
    }
    /// No symlinks in the VFS: a no-op.
    pub fn follow_links(self, _yes: bool) -> Self {
        self
    }
    /// No symlinks in the VFS: a no-op.
    pub fn follow_root_links(self, _yes: bool) -> Self {
        self
    }
    /// One filesystem: a no-op.
    pub fn same_file_system(self, _yes: bool) -> Self {
        self
    }
    pub fn sort_by<F>(mut self, cmp: F) -> Self
    where
        F: FnMut(&DirEntry, &DirEntry) -> Ordering + Send + Sync + 'static,
    {
        self.sorter = Some(Box::new(cmp));
        self
    }
    pub fn sort_by_key<K, F>(self, mut key: F) -> Self
    where
        F: FnMut(&DirEntry) -> K + Send + Sync + 'static,
        K: Ord,
    {
        self.sort_by(move |a, b| key(a).cmp(&key(b)))
    }
    pub fn sort_by_file_name(self) -> Self {
        self.sort_by(|a, b| a.file_name().cmp(b.file_name()))
    }
}

impl IntoIterator for WalkDir {
    type Item = Result<DirEntry>;
    type IntoIter = IntoIter;
    fn into_iter(self) -> IntoIter {
        IntoIter {
            root: Some(self.root),
            min_depth: self.min_depth,
            max_depth: self.max_depth,
            sorter: self.sorter,
            stack: Vec::new(),
            last_pushed: false,
        }
    }
}

pub struct IntoIter {
    root: Option<PathBuf>,
    min_depth: usize,
    max_depth: usize,
    sorter: Option<Sorter>,
    /// One frame per open directory: its remaining entries, in reverse.
    stack: Vec<Vec<Result<DirEntry>>>,
    /// Whether the entry yielded last was a directory whose children were pushed.
    last_pushed: bool,
}

impl fmt::Debug for IntoIter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntoIter")
            .field("depth", &self.stack.len())
            .finish()
    }
}

impl IntoIter {
    /// Skip the rest of the directory just yielded (or, after a file, the rest
    /// of its parent).
    pub fn skip_current_dir(&mut self) {
        if !self.stack.is_empty() {
            self.stack.pop();
        }
        self.last_pushed = false;
    }

    pub fn filter_entry<P>(self, predicate: P) -> FilterEntry<Self, P>
    where
        P: FnMut(&DirEntry) -> bool,
    {
        FilterEntry {
            it: self,
            predicate,
        }
    }

    fn children(&mut self, dir: &DirEntry) -> Vec<Result<DirEntry>> {
        let depth = dir.depth + 1;
        let read = match memfs::read_dir(&dir.path) {
            Ok(r) => r,
            Err(e) => return vec![Err(Error::new(Some(dir.path.clone()), depth, e))],
        };
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for e in read {
            match e.and_then(|e| Ok((e.path(), e.file_type()?))) {
                Ok((path, file_type)) => entries.push(DirEntry {
                    path,
                    file_type,
                    depth,
                }),
                Err(err) => errors.push(Err(Error::new(Some(dir.path.clone()), depth, err))),
            }
        }
        if let Some(sorter) = self.sorter.as_mut() {
            entries.sort_by(|a, b| sorter(a, b));
        }
        let mut frame: Vec<Result<DirEntry>> = entries.into_iter().map(Ok).collect();
        frame.extend(errors);
        frame.reverse();
        frame
    }
}

impl Iterator for IntoIter {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Result<DirEntry>> {
        loop {
            let item = if let Some(root) = self.root.take() {
                match memfs::metadata(&root) {
                    Ok(m) => Ok(DirEntry {
                        path: root,
                        file_type: m.file_type(),
                        depth: 0,
                    }),
                    Err(e) => Err(Error::new(Some(root), 0, e)),
                }
            } else {
                let frame = self.stack.last_mut()?;
                match frame.pop() {
                    Some(item) => item,
                    None => {
                        self.stack.pop();
                        continue;
                    }
                }
            };
            let entry = match item {
                Ok(entry) => entry,
                Err(e) => {
                    self.last_pushed = false;
                    return Some(Err(e));
                }
            };
            self.last_pushed = false;
            if entry.file_type.is_dir() && entry.depth < self.max_depth {
                let frame = self.children(&entry);
                self.stack.push(frame);
                self.last_pushed = true;
            }
            if entry.depth >= self.min_depth {
                return Some(Ok(entry));
            }
        }
    }
}

/// `walkdir::FilterEntry`: a directory the predicate rejects is not descended.
pub struct FilterEntry<I, P> {
    it: I,
    predicate: P,
}

impl<P> FilterEntry<IntoIter, P>
where
    P: FnMut(&DirEntry) -> bool,
{
    pub fn skip_current_dir(&mut self) {
        self.it.skip_current_dir();
    }
    pub fn filter_entry(self, predicate: P) -> FilterEntry<Self, P> {
        FilterEntry {
            it: self,
            predicate,
        }
    }
}

impl<P> Iterator for FilterEntry<IntoIter, P>
where
    P: FnMut(&DirEntry) -> bool,
{
    type Item = Result<DirEntry>;
    fn next(&mut self) -> Option<Result<DirEntry>> {
        loop {
            let entry = match self.it.next()? {
                Ok(e) => e,
                Err(e) => return Some(Err(e)),
            };
            if !(self.predicate)(&entry) {
                if self.it.last_pushed {
                    self.it.skip_current_dir();
                }
                continue;
            }
            return Some(Ok(entry));
        }
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    path: PathBuf,
    file_type: FileType,
    depth: usize,
}

impl DirEntry {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn into_path(self) -> PathBuf {
        self.path
    }
    pub fn path_is_symlink(&self) -> bool {
        false
    }
    pub fn metadata(&self) -> Result<Metadata> {
        memfs::metadata(&self.path).map_err(|e| Error::new(Some(self.path.clone()), self.depth, e))
    }
    pub fn file_type(&self) -> FileType {
        self.file_type
    }
    pub fn file_name(&self) -> &OsStr {
        self.path
            .file_name()
            .unwrap_or_else(|| self.path.as_os_str())
    }
    pub fn depth(&self) -> usize {
        self.depth
    }
}

#[derive(Debug)]
pub struct Error {
    path: Option<PathBuf>,
    depth: usize,
    err: io::Error,
}

impl Error {
    fn new(path: Option<PathBuf>, depth: usize, err: io::Error) -> Self {
        Self { path, depth, err }
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn depth(&self) -> usize {
        self.depth
    }
    /// No symlinks in the VFS, so no loops.
    pub fn loop_ancestor(&self) -> Option<&Path> {
        None
    }
    pub fn io_error(&self) -> Option<&io::Error> {
        Some(&self.err)
    }
    pub fn into_io_error(self) -> Option<io::Error> {
        Some(self.err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "IO error for operation on {}: {}", p.display(), self.err),
            None => write!(f, "IO error: {}", self.err),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.err)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        io::Error::new(e.err.kind(), e)
    }
}
