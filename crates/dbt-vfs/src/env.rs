//! ferrion-wasm: the process-environment seam.
//!
//! On `wasm32-unknown-unknown` there is no process environment, and std splits
//! the missing pieces into two kinds:
//!
//! - **survivable**: `var`/`var_os` say `NotPresent`, `current_dir` is an
//!   `Err`, `args` is empty;
//! - **panicking**: `vars`/`vars_os` and `temp_dir` panic ("not supported on
//!   this platform"), and `set_var`/`remove_var` panic because the platform
//!   setter returns an error.
//!
//! dbt both writes the environment and reads it back (`apply_engine_env_var_aliases`
//! copies `DBT_ENGINE_*` onto `DBT_*`, then the CLI reads `DBT_*`), so on wasm
//! this module is one in-memory environment that every call agrees on.
//! `scripts/wasm-runtime-rewrite.py` routes the run-path crates' `std::env`
//! through it.
//!
//! | item                                 | native        | wasm32                           |
//! |--------------------------------------|---------------|----------------------------------|
//! | [`var`] / [`var_os`]                 | `std::env`    | the in-memory map                |
//! | [`vars`] / [`vars_os`]               | `std::env`    | a snapshot of the map, key order |
//! | [`set_var`] / [`remove_var`]         | `std::env`    | the map (`unsafe` as in std)     |
//! | [`current_dir`] / [`set_current_dir`]| `std::env`    | an in-memory cwd, default `/`    |
//! | [`temp_dir`]                         | `std::env`    | `/tmp` (a VFS path)              |
//! | [`process_id`]                       | `process::id` | `1`                              |
//! | [`home_dir`] (shadows std's)         | `dirs`        | `$HOME`, else `/home`            |
//! | everything else                      | `std::env`    | `std::env`                       |
//!
//! Natively this module *is* `std::env` (a glob re-export); on wasm32 the items
//! above shadow the glob. A host seeds the wasm environment with [`set_var`]
//! (or [`extend`]) before it runs dbt.

pub use std::env::*;

#[cfg(target_arch = "wasm32")]
pub use self::mem::{
    Vars, VarsOs, current_dir, extend, remove_var, set_current_dir, set_var, temp_dir, var, var_os,
    vars, vars_os,
};

/// `std::process::id()`, which panics on wasm32-unknown-unknown ("no pids on
/// this platform"); there one wasm instance is process `1`.
#[inline]
pub fn process_id() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        1
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::process::id()
    }
}

/// `dirs::home_dir()`. On wasm32 `dirs` has no home (`None`), and dbt
/// `.expect()`s one for its lease files and looks under it for `~/.dbt`; there
/// it is the in-memory `HOME`, else `/home` (a VFS path).
#[cfg(not(target_arch = "wasm32"))]
#[inline]
pub fn home_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir()
}

/// `dirs::home_dir()`. On wasm32 `dirs` has no home (`None`), and dbt
/// `.expect()`s one for its lease files and looks under it for `~/.dbt`; there
/// it is the in-memory `HOME`, else `/home` (a VFS path).
#[cfg(target_arch = "wasm32")]
pub fn home_dir() -> Option<std::path::PathBuf> {
    Some(
        var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/home")),
    )
}

/// Natively: set every `(key, value)` in the process environment.
///
/// # Safety
///
/// As [`std::env::set_var`]: no other thread may read or write the environment
/// concurrently.
#[cfg(not(target_arch = "wasm32"))]
pub unsafe fn extend<K, V>(pairs: impl IntoIterator<Item = (K, V)>)
where
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    for (k, v) in pairs {
        // SAFETY: the caller's contract.
        unsafe { std::env::set_var(k, v) };
    }
}

#[cfg(any(target_arch = "wasm32", test))]
mod mem {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    use std::env::VarError;

    struct State {
        vars: BTreeMap<OsString, OsString>,
        cwd: PathBuf,
    }

    static STATE: Mutex<State> = Mutex::new(State {
        vars: BTreeMap::new(),
        cwd: PathBuf::new(),
    });

    fn state() -> MutexGuard<'static, State> {
        // A panic while holding the lock leaves the map consistent (every
        // mutation is one map call), so a poisoned lock is still usable.
        STATE.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn var_os<K: AsRef<OsStr>>(key: K) -> Option<OsString> {
        state().vars.get(key.as_ref()).cloned()
    }

    pub fn var<K: AsRef<OsStr>>(key: K) -> Result<String, VarError> {
        match var_os(key) {
            None => Err(VarError::NotPresent),
            Some(v) => v.into_string().map_err(VarError::NotUnicode),
        }
    }

    /// # Safety
    ///
    /// Safe on wasm (one thread, one lock); `unsafe` only so call sites are
    /// identical to [`std::env::set_var`] under edition 2024.
    pub unsafe fn set_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(key: K, value: V) {
        state()
            .vars
            .insert(key.as_ref().to_owned(), value.as_ref().to_owned());
    }

    /// # Safety
    ///
    /// As [`set_var`].
    pub unsafe fn remove_var<K: AsRef<OsStr>>(key: K) {
        state().vars.remove(key.as_ref());
    }

    /// Set every `(key, value)`; the host's way to seed the environment.
    ///
    /// # Safety
    ///
    /// As [`set_var`].
    pub unsafe fn extend<K, V>(pairs: impl IntoIterator<Item = (K, V)>)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut s = state();
        for (k, v) in pairs {
            s.vars.insert(k.as_ref().to_owned(), v.as_ref().to_owned());
        }
    }

    /// `std::env::Vars`' shape: `(String, String)` pairs. Like std, it panics
    /// on a non-Unicode key or value.
    #[derive(Debug)]
    pub struct Vars(std::vec::IntoIter<(OsString, OsString)>);

    impl Iterator for Vars {
        type Item = (String, String);
        fn next(&mut self) -> Option<(String, String)> {
            self.0.next().map(|(k, v)| {
                (
                    k.into_string()
                        .unwrap_or_else(|k| panic!("non-Unicode env key {k:?}")),
                    v.into_string()
                        .unwrap_or_else(|v| panic!("non-Unicode env value {v:?}")),
                )
            })
        }
    }

    /// `std::env::VarsOs`' shape.
    #[derive(Debug)]
    pub struct VarsOs(std::vec::IntoIter<(OsString, OsString)>);

    impl Iterator for VarsOs {
        type Item = (OsString, OsString);
        fn next(&mut self) -> Option<(OsString, OsString)> {
            self.0.next()
        }
    }

    fn snapshot() -> std::vec::IntoIter<(OsString, OsString)> {
        state()
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn vars() -> Vars {
        Vars(snapshot())
    }

    pub fn vars_os() -> VarsOs {
        VarsOs(snapshot())
    }

    /// The in-memory working directory (`/` until a host sets one).
    pub fn current_dir() -> io::Result<PathBuf> {
        let cwd = &state().cwd;
        Ok(if cwd.as_os_str().is_empty() {
            PathBuf::from("/")
        } else {
            cwd.clone()
        })
    }

    /// Set the in-memory working directory; a relative path is taken against
    /// the current one. Unlike std, the directory need not exist (the VFS may
    /// be mounted afterwards).
    pub fn set_current_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let next = crate::lexical(&current_dir()?.join(path.as_ref()));
        state().cwd = next;
        Ok(())
    }

    /// A VFS path: `/tmp`.
    pub fn temp_dir() -> PathBuf {
        PathBuf::from("/tmp")
    }
}

#[cfg(test)]
mod tests {
    use super::mem;
    use std::path::PathBuf;

    // One test: the in-memory state is process-global.
    #[test]
    fn set_then_read_back_and_iterate() {
        assert!(mem::var("__VFS_ENV_A").is_err());
        unsafe {
            mem::set_var("__VFS_ENV_B", "2");
            mem::set_var("__VFS_ENV_A", "1");
        }
        assert_eq!(mem::var("__VFS_ENV_A").as_deref(), Ok("1"));
        let all: Vec<_> = mem::vars()
            .filter(|(k, _)| k.starts_with("__VFS_ENV_"))
            .collect();
        assert_eq!(
            all,
            vec![
                ("__VFS_ENV_A".to_string(), "1".to_string()),
                ("__VFS_ENV_B".to_string(), "2".to_string())
            ]
        );
        unsafe { mem::remove_var("__VFS_ENV_A") };
        assert!(mem::var_os("__VFS_ENV_A").is_none());
        unsafe { mem::extend([("__VFS_ENV_C", "3")]) };
        assert_eq!(
            mem::vars_os().filter(|(k, _)| k == "__VFS_ENV_C").count(),
            1
        );

        assert_eq!(mem::current_dir().unwrap(), PathBuf::from("/"));
        mem::set_current_dir("/project").unwrap();
        mem::set_current_dir("sub/..").unwrap();
        assert_eq!(mem::current_dir().unwrap(), PathBuf::from("/project"));
        assert_eq!(mem::temp_dir(), PathBuf::from("/tmp"));
    }
}
