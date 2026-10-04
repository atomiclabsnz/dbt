//! The backend trait, the default in-memory backend, and the process-wide slot
//! a host installs its own backend into.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::memfs::{FileType, Metadata, OpenOptions};

/// A filesystem dbt's run path can live on.
///
/// Every path a backend sees is absolute and lexically normalised (no `.`, no
/// `..`), by [`crate::normalize`]. Semantics follow `std::fs` on Unix: a file is
/// created only inside an existing directory, `remove_dir` refuses a non-empty
/// directory, `rename` replaces a file and moves a directory with its contents.
pub trait Backend: Send + Sync {
    /// `NotFound` when nothing is at `path`.
    fn metadata(&self, path: &Path) -> io::Result<Metadata>;
    /// Open (and per `opts`, create or truncate) the file at `path`.
    fn open(&self, path: &Path, opts: &OpenOptions) -> io::Result<Box<dyn FileHandle>>;
    /// Create one directory; its parent must exist.
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Remove one empty directory.
    fn remove_dir(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// The direct children of a directory, sorted by name.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<(OsString, FileType)>>;
}

/// An open file of a [`Backend`].
pub trait FileHandle: Read + Write + Seek + Send + Sync {
    fn metadata(&self) -> io::Result<Metadata>;
    fn set_len(&self, size: u64) -> io::Result<()>;
    fn try_clone(&self) -> io::Result<Box<dyn FileHandle>>;
    fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }
}

static BACKEND: RwLock<Option<Arc<dyn Backend>>> = RwLock::new(None);

/// Replace the process-wide backend (the default is a fresh [`MemoryBackend`]).
/// Files open on the previous backend keep working against it.
pub fn install(backend: Arc<dyn Backend>) {
    *BACKEND.write().unwrap_or_else(|e| e.into_inner()) = Some(backend);
}

/// The process-wide backend.
pub fn backend() -> Arc<dyn Backend> {
    if let Some(b) = BACKEND.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return b.clone();
    }
    let mut slot = BACKEND.write().unwrap_or_else(|e| e.into_inner());
    slot.get_or_insert_with(|| Arc::new(MemoryBackend::new()))
        .clone()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Modification times come from a counter, never the wall clock: every
/// mutation moves it one second past `2020-01-01T00:00:00Z`, so two writes
/// always differ (also at one-second resolution) and a run is reproducible.
const EPOCH_OFFSET_SECS: u64 = 1_577_836_800;

fn tick_time(tick: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(EPOCH_OFFSET_SECS + tick)
}

#[derive(Debug)]
struct FileData {
    bytes: Mutex<Vec<u8>>,
    mtime: AtomicU64,
}

#[derive(Debug, Clone)]
enum Node {
    File(Arc<FileData>),
    Dir { mtime: u64 },
}

/// The default backend: a `BTreeMap` from normalised absolute path to node.
///
/// Directories are explicit nodes, and also implicit: any path that is a
/// strict ancestor of a node is a directory.
#[derive(Debug)]
pub struct MemoryBackend {
    tree: Mutex<BTreeMap<PathBuf, Node>>,
    clock: Arc<AtomicU64>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

enum Kind {
    File(Arc<FileData>),
    Dir(u64),
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("No such file or directory (vfs): {}", path.display()),
    )
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            tree: Mutex::new(BTreeMap::new()),
            clock: Arc::new(AtomicU64::new(0)),
        }
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn kind(tree: &BTreeMap<PathBuf, Node>, path: &Path) -> Option<Kind> {
        if let Some(node) = tree.get(path) {
            return Some(match node {
                Node::File(f) => Kind::File(f.clone()),
                Node::Dir { mtime } => Kind::Dir(*mtime),
            });
        }
        if path.parent().is_none() {
            return Some(Kind::Dir(0)); // the root
        }
        let implicit = tree
            .range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
            .next()
            .is_some_and(|(k, _)| k.starts_with(path));
        implicit.then_some(Kind::Dir(0))
    }

    fn is_dir(tree: &BTreeMap<PathBuf, Node>, path: &Path) -> bool {
        matches!(Self::kind(tree, path), Some(Kind::Dir(_)))
    }

    fn parent_must_be_dir(tree: &BTreeMap<PathBuf, Node>, path: &Path) -> io::Result<()> {
        match path.parent() {
            None => Ok(()),
            Some(p) => match Self::kind(tree, p) {
                Some(Kind::Dir(_)) => Ok(()),
                Some(Kind::File(_)) => Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("Not a directory (vfs): {}", p.display()),
                )),
                None => Err(not_found(path)),
            },
        }
    }

    /// Every key strictly below `path`.
    fn descendants(tree: &BTreeMap<PathBuf, Node>, path: &Path) -> Vec<PathBuf> {
        tree.range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(path))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

impl Backend for MemoryBackend {
    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let tree = lock(&self.tree);
        match Self::kind(&tree, path) {
            Some(Kind::File(f)) => Ok(file_metadata(&f)),
            Some(Kind::Dir(mtime)) => Ok(Metadata::new(FileType::dir(), 0, tick_time(mtime))),
            None => Err(not_found(path)),
        }
    }

    fn open(&self, path: &Path, opts: &OpenOptions) -> io::Result<Box<dyn FileHandle>> {
        opts.validate()?;
        let mut tree = lock(&self.tree);
        let data = match Self::kind(&tree, path) {
            Some(Kind::Dir(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    format!("Is a directory (vfs): {}", path.display()),
                ));
            }
            Some(Kind::File(f)) => {
                if opts.create_new {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("File exists (vfs): {}", path.display()),
                    ));
                }
                if opts.truncate {
                    lock(&f.bytes).clear();
                    f.mtime.store(self.tick(), Ordering::SeqCst);
                }
                f
            }
            None => {
                if !(opts.create || opts.create_new) {
                    return Err(not_found(path));
                }
                Self::parent_must_be_dir(&tree, path)?;
                let f = Arc::new(FileData {
                    bytes: Mutex::new(Vec::new()),
                    mtime: AtomicU64::new(self.tick()),
                });
                tree.insert(path.to_path_buf(), Node::File(f.clone()));
                f
            }
        };
        Ok(Box::new(MemHandle {
            data,
            pos: 0,
            read: opts.read,
            write: opts.write || opts.append,
            append: opts.append,
            clock: self.clock.clone(),
        }))
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let mut tree = lock(&self.tree);
        if Self::kind(&tree, path).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("File exists (vfs): {}", path.display()),
            ));
        }
        Self::parent_must_be_dir(&tree, path)?;
        let mtime = self.tick();
        tree.insert(path.to_path_buf(), Node::Dir { mtime });
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut tree = lock(&self.tree);
        match Self::kind(&tree, path) {
            Some(Kind::File(_)) => {
                tree.remove(path);
                Ok(())
            }
            Some(Kind::Dir(_)) => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("Is a directory (vfs): {}", path.display()),
            )),
            None => Err(not_found(path)),
        }
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let mut tree = lock(&self.tree);
        match Self::kind(&tree, path) {
            Some(Kind::Dir(_)) => {
                if path.parent().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "cannot remove the root (vfs)",
                    ));
                }
                if !Self::descendants(&tree, path).is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::DirectoryNotEmpty,
                        format!("Directory not empty (vfs): {}", path.display()),
                    ));
                }
                tree.remove(path);
                Ok(())
            }
            Some(Kind::File(_)) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("Not a directory (vfs): {}", path.display()),
            )),
            None => Err(not_found(path)),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut tree = lock(&self.tree);
        if from == to {
            return Self::kind(&tree, from)
                .map(|_| ())
                .ok_or_else(|| not_found(from));
        }
        match Self::kind(&tree, from) {
            None => Err(not_found(from)),
            Some(Kind::File(f)) => {
                if Self::is_dir(&tree, to) {
                    return Err(io::Error::new(
                        io::ErrorKind::IsADirectory,
                        format!("Is a directory (vfs): {}", to.display()),
                    ));
                }
                Self::parent_must_be_dir(&tree, to)?;
                tree.remove(from);
                tree.insert(to.to_path_buf(), Node::File(f));
                Ok(())
            }
            Some(Kind::Dir(mtime)) => {
                if to.starts_with(from) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "cannot move {} into itself (vfs): {}",
                            from.display(),
                            to.display()
                        ),
                    ));
                }
                match Self::kind(&tree, to) {
                    Some(Kind::File(_)) => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            format!("Not a directory (vfs): {}", to.display()),
                        ));
                    }
                    Some(Kind::Dir(_)) if !Self::descendants(&tree, to).is_empty() => {
                        return Err(io::Error::new(
                            io::ErrorKind::DirectoryNotEmpty,
                            format!("Directory not empty (vfs): {}", to.display()),
                        ));
                    }
                    _ => {}
                }
                Self::parent_must_be_dir(&tree, to)?;
                let moved = Self::descendants(&tree, from);
                tree.remove(from);
                tree.insert(to.to_path_buf(), Node::Dir { mtime });
                for old in moved {
                    if let Some(node) = tree.remove(&old) {
                        let rel = old.strip_prefix(from).unwrap_or(&old);
                        tree.insert(to.join(rel), node);
                    }
                }
                Ok(())
            }
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<(OsString, FileType)>> {
        let tree = lock(&self.tree);
        match Self::kind(&tree, path) {
            None => return Err(not_found(path)),
            Some(Kind::File(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("Not a directory (vfs): {}", path.display()),
                ));
            }
            Some(Kind::Dir(_)) => {}
        }
        let depth = path.components().count();
        let mut out: Vec<(OsString, FileType)> = Vec::new();
        for (k, node) in tree
            .range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(path))
        {
            let Some(name) = k.components().nth(depth) else {
                continue;
            };
            let name = name.as_os_str().to_os_string();
            let direct = k.components().count() == depth + 1;
            let ft = match node {
                Node::File(_) if direct => FileType::file(),
                _ => FileType::dir(),
            };
            match out.last_mut() {
                Some((last, _)) if *last == name => {}
                _ => out.push((name, ft)),
            }
        }
        Ok(out)
    }
}

fn file_metadata(f: &FileData) -> Metadata {
    Metadata::new(
        FileType::file(),
        lock(&f.bytes).len() as u64,
        tick_time(f.mtime.load(Ordering::SeqCst)),
    )
}

/// An open file of the [`MemoryBackend`]: its bytes are shared with the tree,
/// so a write is visible to every other handle at once (as on a real disk).
struct MemHandle {
    data: Arc<FileData>,
    pos: u64,
    read: bool,
    write: bool,
    append: bool,
    clock: Arc<AtomicU64>,
}

impl MemHandle {
    fn touch(&self) {
        let t = self.clock.fetch_add(1, Ordering::SeqCst) + 1;
        self.data.mtime.store(t, Ordering::SeqCst);
    }
}

impl Read for MemHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.read {
            return Err(io::Error::other("file not opened for reading (vfs)"));
        }
        let bytes = lock(&self.data.bytes);
        let start = (self.pos as usize).min(bytes.len());
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for MemHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.write {
            return Err(io::Error::other("file not opened for writing (vfs)"));
        }
        {
            let mut bytes = lock(&self.data.bytes);
            if self.append {
                self.pos = bytes.len() as u64;
            }
            let start = self.pos as usize;
            if bytes.len() < start {
                bytes.resize(start, 0);
            }
            let overlap = (bytes.len() - start).min(buf.len());
            bytes[start..start + overlap].copy_from_slice(&buf[..overlap]);
            bytes.extend_from_slice(&buf[overlap..]);
            self.pos += buf.len() as u64;
        }
        self.touch();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for MemHandle {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = lock(&self.data.bytes).len() as i128;
        let next = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => len + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if next < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative position (vfs)",
            ));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

impl FileHandle for MemHandle {
    fn metadata(&self) -> io::Result<Metadata> {
        Ok(file_metadata(&self.data))
    }

    fn set_len(&self, size: u64) -> io::Result<()> {
        if !self.write {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file not opened for writing (vfs)",
            ));
        }
        lock(&self.data.bytes).resize(size as usize, 0);
        self.touch();
        Ok(())
    }

    fn try_clone(&self) -> io::Result<Box<dyn FileHandle>> {
        Ok(Box::new(MemHandle {
            data: self.data.clone(),
            pos: self.pos,
            read: self.read,
            write: self.write,
            append: self.append,
            clock: self.clock.clone(),
        }))
    }
}
