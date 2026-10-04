//! `std::fs`, in memory: the same names and signatures (the subset dbt uses),
//! over the installed [`Backend`](crate::Backend).

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::memory::{FileHandle, backend};
use crate::normalize;

// ---------------------------------------------------------------------------
// Metadata, FileType, Permissions

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    File,
    Dir,
}

/// What a node is. There are no symlinks in the VFS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(Kind);

impl FileType {
    pub fn file() -> Self {
        Self(Kind::File)
    }
    pub fn dir() -> Self {
        Self(Kind::Dir)
    }
    pub fn is_file(&self) -> bool {
        self.0 == Kind::File
    }
    pub fn is_dir(&self) -> bool {
        self.0 == Kind::Dir
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
}

/// Always writable: the VFS has no permission bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    readonly: bool,
}

impl Permissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    file_type: FileType,
    len: u64,
    modified: SystemTime,
}

impl Metadata {
    /// For [`Backend`](crate::Backend) implementations.
    pub fn new(file_type: FileType, len: u64, modified: SystemTime) -> Self {
        Self {
            file_type,
            len,
            modified,
        }
    }
    pub fn file_type(&self) -> FileType {
        self.file_type
    }
    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }
    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn permissions(&self) -> Permissions {
        Permissions { readonly: false }
    }
}

// ---------------------------------------------------------------------------
// OpenOptions, File

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) append: bool,
    pub(crate) truncate: bool,
    pub(crate) create: bool,
    pub(crate) create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, v: bool) -> &mut Self {
        self.read = v;
        self
    }
    pub fn write(&mut self, v: bool) -> &mut Self {
        self.write = v;
        self
    }
    pub fn append(&mut self, v: bool) -> &mut Self {
        self.append = v;
        self
    }
    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.truncate = v;
        self
    }
    pub fn create(&mut self, v: bool) -> &mut Self {
        self.create = v;
        self
    }
    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.create_new = v;
        self
    }
    pub fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        let path = normalize(path.as_ref());
        let inner = backend().open(&path, self)?;
        Ok(File { inner, path })
    }

    /// The combinations `std::fs` refuses with `InvalidInput`.
    pub fn validate(&self) -> io::Result<()> {
        let writes = self.write || self.append;
        let creates = self.truncate || self.create || self.create_new;
        let bad = if writes {
            self.truncate && self.append
        } else {
            !self.read || creates
        };
        if bad {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid open options (vfs): {self:?}"),
            ));
        }
        Ok(())
    }
}

/// An open file in the VFS.
pub struct File {
    inner: Box<dyn FileHandle>,
    path: PathBuf,
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("path", &self.path).finish()
    }
}

impl File {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
    pub fn create_new<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.inner.metadata()
    }
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size)
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    pub fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    pub fn try_clone(&self) -> io::Result<File> {
        Ok(File {
            inner: self.inner.try_clone()?,
            path: self.path.clone(),
        })
    }
    pub fn set_permissions(&self, _perm: Permissions) -> io::Result<()> {
        Ok(())
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

// ---------------------------------------------------------------------------
// ReadDir, DirEntry

#[derive(Debug, Clone)]
pub struct DirEntry {
    path: PathBuf,
    name: OsString,
    file_type: FileType,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.file_type)
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata(&self.path)
    }
}

/// The entries of one directory, in name order (`std::fs` promises no order;
/// the VFS is deterministic).
#[derive(Debug)]
pub struct ReadDir(std::vec::IntoIter<DirEntry>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Ok)
    }
}

// ---------------------------------------------------------------------------
// Free functions

pub fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    backend().metadata(&normalize(path.as_ref()))
}

/// There are no symlinks in the VFS: the same as [`metadata`].
pub fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    metadata(path)
}

pub fn exists<P: AsRef<Path>>(path: P) -> io::Result<bool> {
    match metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)?;
    Ok(out)
}

pub fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    File::create(path)?.write_all(contents.as_ref())
}

pub fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    backend().create_dir(&normalize(path.as_ref()))
}

pub fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = normalize(path.as_ref());
    let b = backend();
    let mut missing = Vec::new();
    let mut cur: Option<&Path> = Some(&path);
    while let Some(p) = cur {
        match b.metadata(p) {
            Ok(m) if m.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("File exists (vfs): {}", p.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(p),
            Err(e) => return Err(e),
        }
        cur = p.parent();
    }
    for p in missing.into_iter().rev() {
        match b.create_dir(p) {
            Ok(()) => {}
            // Created concurrently: as std, fine when it is a directory.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && b.metadata(p)?.is_dir() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    backend().remove_file(&normalize(path.as_ref()))
}

pub fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    backend().remove_dir(&normalize(path.as_ref()))
}

pub fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = normalize(path.as_ref());
    let b = backend();
    if !b.metadata(&path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("Not a directory (vfs): {}", path.display()),
        ));
    }
    fn rm(b: &dyn crate::memory::Backend, dir: &Path) -> io::Result<()> {
        for (name, ft) in b.read_dir(dir)? {
            let child = dir.join(name);
            if ft.is_dir() {
                rm(b, &child)?;
            } else {
                b.remove_file(&child)?;
            }
        }
        b.remove_dir(dir)
    }
    rm(b.as_ref(), &path)
}

pub fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    backend().rename(&normalize(from.as_ref()), &normalize(to.as_ref()))
}

pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    let from = normalize(from.as_ref());
    if !metadata(&from)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the source path is not a file (vfs): {}", from.display()),
        ));
    }
    let bytes = read(&from)?;
    write(to, &bytes)?;
    Ok(bytes.len() as u64)
}

pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let path = normalize(path.as_ref());
    let entries = backend()
        .read_dir(&path)?
        .into_iter()
        .map(|(name, file_type)| DirEntry {
            path: path.join(&name),
            name,
            file_type,
        })
        .collect::<Vec<_>>();
    Ok(ReadDir(entries.into_iter()))
}

/// Absolute and lexically normalised; like `std::fs::canonicalize` it fails
/// when nothing is there. (There are no symlinks to resolve.)
pub fn canonicalize<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    let path = normalize(path.as_ref());
    backend().metadata(&path)?;
    Ok(path)
}

/// There are no symlinks in the VFS.
pub fn read_link<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    let path = normalize(path.as_ref());
    backend().metadata(&path)?;
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("not a symbolic link (vfs): {}", path.display()),
    ))
}

/// The VFS has no permission bits; succeeds when the path exists.
pub fn set_permissions<P: AsRef<Path>>(path: P, _perm: Permissions) -> io::Result<()> {
    metadata(path).map(|_| ())
}

/// There are no hard links in the VFS: a copy.
pub fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    copy(original, link).map(|_| ())
}

// ---------------------------------------------------------------------------
// parquet: what `std::fs::File` gets from the parquet crate

#[cfg(feature = "parquet")]
impl parquet::file::reader::Length for File {
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(feature = "parquet")]
impl parquet::file::reader::ChunkReader for File {
    type T = io::BufReader<File>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let mut f = self.try_clone()?;
        f.seek(SeekFrom::Start(start))?;
        Ok(io::BufReader::new(f))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<bytes::Bytes> {
        let mut buffer = Vec::with_capacity(length);
        let mut reader = self.try_clone()?;
        reader.seek(SeekFrom::Start(start))?;
        let read = reader.take(length as u64).read_to_end(&mut buffer)?;
        if read != length {
            return Err(parquet::errors::ParquetError::EOF(format!(
                "Expected to read {length} bytes, read only {read}"
            )));
        }
        Ok(buffer.into())
    }
}
