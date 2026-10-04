use std::path::Path;
use std::time::SystemTime;

// ferrion-wasm: tokio refuses its `fs` feature on every wasm target (it is
// `std::fs` behind `spawn_blocking`, and wasm has no blocking pool). On wasm the
// same wrappers run `std::fs` inline. That COMPILES on wasm32-unknown-unknown,
// where every call returns `Unsupported` at runtime; an embedder must supply the
// project through a virtual filesystem above this layer (or via wasip1 preopens).
#[cfg(not(target_arch = "wasm32"))]
use tokio::fs as afs;

#[cfg(target_arch = "wasm32")]
mod afs {
    use std::io;
    use std::path::{Path, PathBuf};

    pub async fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }
    pub async fn remove_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
        std::fs::remove_dir_all(p)
    }
    pub async fn read_to_string(p: impl AsRef<Path>) -> io::Result<String> {
        std::fs::read_to_string(p)
    }
    pub async fn read(p: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }
    pub async fn write(p: impl AsRef<Path>, c: impl AsRef<[u8]>) -> io::Result<()> {
        std::fs::write(p, c)
    }
    pub async fn copy(a: impl AsRef<Path>, b: impl AsRef<Path>) -> io::Result<u64> {
        std::fs::copy(a, b)
    }
    pub async fn metadata(p: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
        std::fs::metadata(p)
    }
    pub async fn try_exists(p: impl AsRef<Path>) -> io::Result<bool> {
        std::fs::exists(p)
    }
    pub async fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
        std::fs::remove_file(p)
    }
    pub async fn read_link(p: impl AsRef<Path>) -> io::Result<PathBuf> {
        std::fs::read_link(p)
    }
    pub async fn rename(a: impl AsRef<Path>, b: impl AsRef<Path>) -> io::Result<()> {
        std::fs::rename(a, b)
    }
}

use crate::error::LiftableResult;
use crate::{FsResult, ectx};

/// Wrapper around [`tokio::fs::create_dir_all`] that returns a useful error in case of failure.
pub async fn create_dir_all(path: impl AsRef<Path>) -> FsResult<()> {
    let path = path.as_ref();
    afs::create_dir_all(path)
        .await
        .lift(ectx!("Failed to create directory: {}", path.display()))
}

/// Wrapper around [`tokio::fs::remove_dir_all`] that returns a useful error in case of failure.
pub async fn remove_dir_all(path: impl AsRef<Path>) -> FsResult<()> {
    let path = path.as_ref();
    afs::remove_dir_all(path)
        .await
        .lift(ectx!("Failed to delete directory: {}", path.display()))
}

/// Wrapper around [`tokio::fs::read_to_string`] that returns a useful error in case of failure.
pub async fn read_to_string<P: AsRef<Path>>(path: P) -> FsResult<String> {
    let path = path.as_ref();
    afs::read_to_string(path)
        .await
        .lift(ectx!("Failed to read file: {}", path.display()))
}

/// Wrapper around [`tokio::fs::read`] that returns a useful error in case of failure.
pub async fn read(path: impl AsRef<Path>) -> FsResult<Vec<u8>> {
    let path = path.as_ref();
    afs::read(path)
        .await
        .lift(ectx!("Failed to read file: {}", path.display()))
}

/// Wrapper around [`tokio::fs::write`] that returns a useful error in case of failure.
pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> FsResult<()> {
    let path = path.as_ref();
    afs::write(path, contents)
        .await
        .lift(ectx!("Failed to write file: {}", path.display()))
}

/// Wrapper around [`tokio::fs::copy`] that returns a useful error in case of failure.
pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> FsResult<u64> {
    let from = from.as_ref();
    let to = to.as_ref();
    afs::copy(from, to).await.lift(ectx!(
        "Failed to copy file {} to {}",
        from.display(),
        to.display()
    ))
}

/// Wrapper around [`tokio::fs::metadata`] + [`Metadata::modified`] that returns a useful error in case of failure.
pub async fn last_modified<P: AsRef<Path>>(path: P) -> FsResult<SystemTime> {
    let path = path.as_ref();
    afs::metadata(path)
        .await
        .and_then(|metadata| metadata.modified())
        .lift(ectx!(
            "Failed to get last modified time of: {}",
            path.display()
        ))
}

/// Wrapper around [`tokio::fs::metadata`] that returns a useful error in case of failure.
pub async fn metadata(path: impl AsRef<Path>) -> FsResult<std::fs::Metadata> {
    let path = path.as_ref();
    afs::metadata(path)
        .await
        .lift(ectx!("Failed to get metadata for: {}", path.display()))
}

/// Check if a path exists (follows symlinks). Returns false on any error.
pub async fn path_exists(path: impl AsRef<Path>) -> bool {
    afs::metadata(path.as_ref()).await.is_ok()
}

/// Check if a path exists, returning false on permission errors rather than panicking.
pub async fn try_exists(path: impl AsRef<Path>) -> bool {
    afs::try_exists(path.as_ref()).await.unwrap_or(false)
}

/// Wrapper around [`tokio::fs::remove_file`] that returns a useful error in case of failure.
pub async fn remove_file(path: impl AsRef<Path>) -> FsResult<()> {
    let path = path.as_ref();
    afs::remove_file(path)
        .await
        .lift(ectx!("Failed to remove file: {}", path.display()))
}

/// Wrapper around [`tokio::fs::read_link`] that returns a useful error in case of failure.
pub async fn read_link(path: impl AsRef<Path>) -> FsResult<std::path::PathBuf> {
    let path = path.as_ref();
    afs::read_link(path)
        .await
        .lift(ectx!("Failed to read symlink: {}", path.display()))
}

#[cfg(not(target_arch = "wasm32"))]
/// Wrapper around [`tokio::fs::symlink`] (Unix) or [`tokio::fs::symlink_dir`] (Windows).
pub async fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> FsResult<()> {
    let target = target.as_ref();
    let link = link.as_ref();
    #[cfg(unix)]
    {
        tokio::fs::symlink(target, link).await.lift(ectx!(
            "Failed to create symlink from {} to {}",
            link.display(),
            target.display()
        ))
    }
    #[cfg(windows)]
    {
        tokio::fs::symlink_dir(target, link).await.lift(ectx!(
            "Failed to create symlink from {} to {}",
            link.display(),
            target.display()
        ))
    }
}

/// Wrapper around [`tokio::fs::rename`] that returns a useful error in case of failure.
pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> FsResult<()> {
    let from = from.as_ref();
    let to = to.as_ref();
    afs::rename(from, to).await.lift(ectx!(
        "Failed to rename file {} to {}",
        from.display(),
        to.display()
    ))
}

#[cfg(not(target_arch = "wasm32"))]
/// Wrapper around [`tokio::fs::read_dir`] that returns a useful error in case of failure.
pub async fn read_dir(path: impl AsRef<Path>) -> FsResult<tokio::fs::ReadDir> {
    let path = path.as_ref();
    tokio::fs::read_dir(path)
        .await
        .lift(ectx!("Failed to read directory: {}", path.display()))
}

pub struct File {}
#[cfg(not(target_arch = "wasm32"))]
impl File {
    /// Wrapper around [`tokio::fs::File::create`] that returns a useful error in case of failure.
    pub async fn create<P: AsRef<Path>>(path: P) -> FsResult<tokio::fs::File> {
        let path = path.as_ref();
        tokio::fs::File::create(path)
            .await
            .lift(ectx!("Failed to create file: {}", path.display()))
    }
}
