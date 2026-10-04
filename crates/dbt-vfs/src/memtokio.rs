//! `tokio::fs`, in memory: the subset dbt uses. Every operation completes
//! inline — an in-memory read never blocks, and wasm has no blocking pool.

use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use crate::memfs;

pub async fn create_dir(p: impl AsRef<Path>) -> io::Result<()> {
    memfs::create_dir(p)
}
pub async fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    memfs::create_dir_all(p)
}
pub async fn remove_dir(p: impl AsRef<Path>) -> io::Result<()> {
    memfs::remove_dir(p)
}
pub async fn remove_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    memfs::remove_dir_all(p)
}
pub async fn read_to_string(p: impl AsRef<Path>) -> io::Result<String> {
    memfs::read_to_string(p)
}
pub async fn read(p: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    memfs::read(p)
}
pub async fn write(p: impl AsRef<Path>, c: impl AsRef<[u8]>) -> io::Result<()> {
    memfs::write(p, c)
}
pub async fn copy(a: impl AsRef<Path>, b: impl AsRef<Path>) -> io::Result<u64> {
    memfs::copy(a, b)
}
pub async fn metadata(p: impl AsRef<Path>) -> io::Result<memfs::Metadata> {
    memfs::metadata(p)
}
pub async fn symlink_metadata(p: impl AsRef<Path>) -> io::Result<memfs::Metadata> {
    memfs::symlink_metadata(p)
}
pub async fn try_exists(p: impl AsRef<Path>) -> io::Result<bool> {
    memfs::exists(p)
}
pub async fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
    memfs::remove_file(p)
}
pub async fn read_link(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    memfs::read_link(p)
}
pub async fn rename(a: impl AsRef<Path>, b: impl AsRef<Path>) -> io::Result<()> {
    memfs::rename(a, b)
}
pub async fn canonicalize(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    memfs::canonicalize(p)
}
pub async fn read_dir(p: impl AsRef<Path>) -> io::Result<ReadDir> {
    memfs::read_dir(p).map(ReadDir)
}
/// There are no symlinks in the VFS.
pub async fn symlink(_target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "symlinks are not supported (vfs): {}",
            link.as_ref().display()
        ),
    ))
}
/// There are no symlinks in the VFS.
pub async fn symlink_dir(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    symlink(target, link).await
}

/// The subset of `tokio::fs::ReadDir` dbt uses.
#[derive(Debug)]
pub struct ReadDir(memfs::ReadDir);

impl ReadDir {
    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        self.0.next().transpose().map(|e| e.map(DirEntry))
    }
}

/// The subset of `tokio::fs::DirEntry` dbt uses.
#[derive(Debug)]
pub struct DirEntry(memfs::DirEntry);

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.0.path()
    }
    pub fn file_name(&self) -> OsString {
        self.0.file_name()
    }
    pub async fn file_type(&self) -> io::Result<memfs::FileType> {
        self.0.file_type()
    }
    pub async fn metadata(&self) -> io::Result<memfs::Metadata> {
        self.0.metadata()
    }
}

/// `tokio::fs::OpenOptions`.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions(memfs::OpenOptions);

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, v: bool) -> &mut Self {
        self.0.read(v);
        self
    }
    pub fn write(&mut self, v: bool) -> &mut Self {
        self.0.write(v);
        self
    }
    pub fn append(&mut self, v: bool) -> &mut Self {
        self.0.append(v);
        self
    }
    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.0.truncate(v);
        self
    }
    pub fn create(&mut self, v: bool) -> &mut Self {
        self.0.create(v);
        self
    }
    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.0.create_new(v);
        self
    }
    /// Unix permission bits: the VFS has none, so a no-op.
    pub fn mode(&mut self, _mode: u32) -> &mut Self {
        self
    }
    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        self.0.open(path).map(File::from_std)
    }
}

/// `tokio::fs::File` over a VFS file; reads, writes and seeks complete inline.
#[derive(Debug)]
pub struct File {
    inner: memfs::File,
    seek_result: Option<io::Result<u64>>,
}

impl File {
    pub async fn open(path: impl AsRef<Path>) -> io::Result<File> {
        memfs::File::open(path).map(File::from_std)
    }
    pub async fn create(path: impl AsRef<Path>) -> io::Result<File> {
        memfs::File::create(path).map(File::from_std)
    }
    pub async fn create_new(path: impl AsRef<Path>) -> io::Result<File> {
        memfs::File::create_new(path).map(File::from_std)
    }
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }
    pub fn from_std(inner: memfs::File) -> File {
        File {
            inner,
            seek_result: None,
        }
    }
    pub async fn into_std(self) -> memfs::File {
        self.inner
    }
    pub async fn metadata(&self) -> io::Result<memfs::Metadata> {
        self.inner.metadata()
    }
    pub async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size)
    }
    pub async fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    pub async fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
}

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let n = this.inner.read(buf.initialize_unfilled())?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.get_mut().inner.write(buf))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.get_mut().inner.flush())
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for File {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        this.seek_result = Some(this.inner.seek(position));
        Ok(())
    }
    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        match this.seek_result.take() {
            Some(r) => Poll::Ready(r),
            None => Poll::Ready(this.inner.stream_position()),
        }
    }
}
