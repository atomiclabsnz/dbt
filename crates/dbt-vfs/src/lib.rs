//! ferrion-wasm: the filesystem seam for dbt.
//!
//! On `wasm32-unknown-unknown` every `std::fs` call returns `Unsupported`, so
//! every project read and write in dbt's run path goes through this crate
//! instead (`scripts/vfs-rewrite.py` does the rewriting).
//!
//! | build                         | `fs`        | `walkdir`  | `tokio_fs`   |
//! |-------------------------------|-------------|------------|--------------|
//! | native, no `memory` feature   | `std::fs`   | `walkdir`  | `tokio::fs`  |
//! | wasm32, or `memory` feature   | in memory   | in memory  | in memory    |
//!
//! In memory mode the filesystem is a [`Backend`]; the default is
//! [`MemoryBackend`] (a `BTreeMap` of normalised absolute paths) and a host can
//! replace it with [`install`]. A host puts a project in with [`mount`] and
//! takes dbt's output back with [`snapshot`].
//!
//! Paths are normalised lexically (`.`/`..` resolved without the OS); there are
//! no symlinks in memory mode, so [`fs::canonicalize`] is "absolute + lexical,
//! and it must exist".

#![cfg_attr(all(test, not(vfs_memory)), allow(dead_code, unused_imports))]

use std::io;
use std::path::{Component, Path, PathBuf};

// The in-memory implementation is always compiled for unit tests, so
// `cargo test -p dbt-vfs` exercises it with or without the `memory` feature.

#[cfg(any(vfs_memory, test))]
mod memfs;
#[cfg(any(vfs_memory, test))]
mod memory;
#[cfg(any(vfs_memory, test))]
mod memtokio;
#[cfg(any(vfs_memory, test))]
mod memwalk;

#[cfg(vfs_memory)]
pub use memory::{Backend, FileHandle, MemoryBackend, backend, install};

/// `true` when [`fs`] is the in-memory filesystem rather than `std::fs`.
pub const IN_MEMORY: bool = cfg!(vfs_memory);

/// A `std::fs`-shaped API. Natively without the `memory` feature this is
/// literally `std::fs`.
pub mod fs {
    #[cfg(not(vfs_memory))]
    pub use std::fs::*;

    #[cfg(vfs_memory)]
    pub use crate::memfs::*;
}

/// A `walkdir`-shaped API. Natively without the `memory` feature this is
/// literally the `walkdir` crate.
pub mod walkdir {
    #[cfg(not(vfs_memory))]
    pub use ::walkdir::*;

    #[cfg(vfs_memory)]
    pub use crate::memwalk::*;
}

/// A `tokio::fs`-shaped API (the subset dbt uses). Natively without the
/// `memory` feature this is literally `tokio::fs`; in memory mode every call
/// completes inline (an in-memory read never blocks).
pub mod tokio_fs {
    #[cfg(not(vfs_memory))]
    pub use tokio::fs::*;

    #[cfg(vfs_memory)]
    pub use crate::memtokio::*;
}

/// Resolve `.` and `..` lexically and make `path` absolute, without the OS.
///
/// A relative path is taken against the process's current directory when it
/// has one (native), and `/` when it does not (wasm32-unknown-unknown).
pub fn normalize(path: &Path) -> PathBuf {
    let base;
    let joined: &Path = if path.has_root() {
        path
    } else {
        base = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path);
        &base
    };
    lexical(joined)
}

/// `.`/`..` resolution only; `..` at the root stays at the root.
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if out.parent().is_some() {
                    out.pop();
                }
            }
            Component::Normal(n) => out.push(n),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Component::RootDir.as_os_str());
    }
    out
}

/// Put `files` (paths relative to `root`, or absolute under it) into the
/// filesystem below `root`, creating directories as needed. Existing files are
/// overwritten. Natively without `memory` this writes to the real disk.
pub fn mount(root: &Path, files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>) -> io::Result<()> {
    fs::create_dir_all(root)?;
    for (rel, bytes) in files {
        let path = if rel.is_absolute() {
            rel
        } else {
            root.join(rel)
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, bytes)?;
    }
    Ok(())
}

/// Every file below `root`, as (path relative to `root`, contents), in path
/// order. A missing `root` is an empty snapshot.
pub fn snapshot(root: &Path) -> io::Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut out = Vec::new();
    if !root.vfs_is_dir() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        for path in entries.into_iter().rev() {
            if path.vfs_is_dir() {
                stack.push(path);
            } else {
                let bytes = fs::read(&path)?;
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.push((rel, bytes));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The filesystem-touching methods of [`Path`], through the VFS.
///
/// `path.exists()` reads the real disk even on wasm (where it is always
/// `false`); `path.vfs_exists()` reads whatever [`fs`] is.
pub trait PathExt {
    fn vfs_exists(&self) -> bool;
    fn vfs_try_exists(&self) -> io::Result<bool>;
    fn vfs_is_file(&self) -> bool;
    fn vfs_is_dir(&self) -> bool;
    fn vfs_metadata(&self) -> io::Result<fs::Metadata>;
    fn vfs_symlink_metadata(&self) -> io::Result<fs::Metadata>;
    fn vfs_read_dir(&self) -> io::Result<fs::ReadDir>;
    fn vfs_canonicalize(&self) -> io::Result<PathBuf>;
}

impl PathExt for Path {
    fn vfs_exists(&self) -> bool {
        fs::metadata(self).is_ok()
    }
    fn vfs_try_exists(&self) -> io::Result<bool> {
        fs::exists(self)
    }
    fn vfs_is_file(&self) -> bool {
        fs::metadata(self).map(|m| m.is_file()).unwrap_or(false)
    }
    fn vfs_is_dir(&self) -> bool {
        fs::metadata(self).map(|m| m.is_dir()).unwrap_or(false)
    }
    fn vfs_metadata(&self) -> io::Result<fs::Metadata> {
        fs::metadata(self)
    }
    fn vfs_symlink_metadata(&self) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(self)
    }
    fn vfs_read_dir(&self) -> io::Result<fs::ReadDir> {
        fs::read_dir(self)
    }
    fn vfs_canonicalize(&self) -> io::Result<PathBuf> {
        fs::canonicalize(self)
    }
}

#[cfg(test)]
mod tests;
