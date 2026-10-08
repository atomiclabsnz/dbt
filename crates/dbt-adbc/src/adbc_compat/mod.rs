//! ADBC 0.24 compatibility: the pieces of dbt's arrow-adbc fork
//! (dbt-labs/arrow-adbc @ d2808cf, ADBC 22) that upstream 0.24 does not expose.
//!
//! Moving to upstream adbc 0.24 is what lets dbt link the same arrow (59) as
//! the rest of the build. Upstream 0.24 made the driver-search helpers
//! `pub(crate)` and never carried the fork's `adbc_ffi::signal` module, so the
//! few functions dbt's driver manager needs are reproduced here from the fork
//! (Apache-2.0, same as upstream).

use std::borrow::Cow;
use std::ffi::{OsStr, c_void};

use adbc_core::error::{Error, Result, Status};
use adbc_core::options::AdbcVersion;
use adbc_ffi::options::check_status;
use adbc_ffi::{FFI_AdbcDriver, FFI_AdbcDriverInitFunc, FFI_AdbcError};

pub(crate) mod signal;

/// Initialize a driver from a statically linked init function
/// (the fork's `DriverLibrary::from_static_init(init).init_driver(version)`).
pub(crate) fn init_static_driver(
    init: &FFI_AdbcDriverInitFunc,
    version: AdbcVersion,
) -> Result<FFI_AdbcDriver> {
    let mut error = FFI_AdbcError::default();
    let mut driver = FFI_AdbcDriver::default();
    // SAFETY: `init` is an ADBC driver init function; `driver` and `error`
    // are valid, default-initialised FFI structs that outlive the call.
    let status = unsafe {
        init(
            version.into(),
            &mut driver as *mut FFI_AdbcDriver as *mut c_void,
            &mut error,
        )
    };
    check_status(status, error)?;
    Ok(driver)
}

/// Load a dynamic library by its platform-agnostic name
/// (`adbc_driver_sqlite` -> `libadbc_driver_sqlite.so`).
pub(crate) fn load_library_from_name(name: impl AsRef<str>) -> Result<libloading::Library> {
    let filename = libloading::library_filename(name.as_ref());
    adbc_driver_manager::search::DriverLibrary::load_library(&filename)
}

/// The explicit entrypoint, or the default one derived from a library path.
pub(crate) fn derive_entrypoint(
    entrypoint: Option<&[u8]>,
    driver_path: impl AsRef<OsStr>,
) -> Cow<'_, [u8]> {
    match entrypoint {
        Some(entrypoint) => Cow::Borrowed(entrypoint),
        None => Cow::Owned(default_entrypoint(driver_path).into_bytes()),
    }
}

/// The explicit entrypoint, or the default one derived from a library name.
pub(crate) fn derive_entrypoint_from_name<'b>(
    entrypoint: Option<&'b [u8]>,
    name: &str,
) -> Cow<'b, [u8]> {
    match entrypoint {
        Some(entrypoint) => Cow::Borrowed(entrypoint),
        None => Cow::Owned(default_entrypoint_from_name(name).into_bytes()),
    }
}

fn default_entrypoint(driver_path: impl AsRef<OsStr>) -> String {
    // - libadbc_driver_sqlite.so.2.0.0 -> AdbcDriverSqliteInit
    // - adbc_driver_sqlite.dll -> AdbcDriverSqliteInit
    // - proprietary_driver.dll -> AdbcProprietaryDriverInit
    let filename = driver_path.as_ref().to_str().unwrap_or_default();
    let filename = filename
        .rfind(['/', '\\'])
        .map_or(filename, |pos| &filename[pos + 1..]);
    let basename = filename
        .find('.')
        .map_or_else(|| filename, |pos| &filename[..pos]);
    let name = basename
        .strip_prefix(dbt_vfs::env::consts::DLL_PREFIX)
        .unwrap_or(basename);
    default_entrypoint_from_name(name)
}

fn default_entrypoint_from_name(name: &str) -> String {
    let entrypoint = name
        .split(&['-', '_'][..])
        .map(|s| {
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join("");
    if entrypoint.starts_with("Adbc") {
        format!("{entrypoint}Init")
    } else {
        format!("Adbc{entrypoint}Init")
    }
}

/// Split a driver URI into `(driver, uri)` — the fork's tuple-returning
/// `parse_driver_uri` (upstream 0.24 returns a private `DriverLocator` and
/// adds `profile://`, which dbt does not use).
pub(crate) fn parse_driver_uri(uri: &str) -> Result<(&str, &str)> {
    let idx = uri.find(':').ok_or(Error::with_message_and_status(
        format!("Invalid URI: {uri}"),
        Status::InvalidArguments,
    ))?;

    let driver = &uri[..idx];
    if uri.len() <= idx + 2 {
        return Ok((driver, uri));
    }

    #[cfg(target_os = "windows")]
    if let Ok(true) = std::fs::exists(uri) {
        return Ok((uri, ""));
    }

    if &uri[idx..idx + 2] == ":/" {
        // scheme is also driver
        return Ok((driver, uri));
    }

    Ok((driver, &uri[idx + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrypoints_match_the_fork() {
        assert_eq!(
            default_entrypoint("libadbc_driver_sqlite.so.2.0.0"),
            "AdbcDriverSqliteInit"
        );
        assert_eq!(
            default_entrypoint("/x/adbc_driver_sqlite.dll"),
            "AdbcDriverSqliteInit"
        );
        assert_eq!(
            default_entrypoint("proprietary_driver.dll"),
            "AdbcProprietaryDriverInit"
        );
    }

    #[test]
    fn driver_uri_split_matches_the_fork() {
        assert_eq!(
            parse_driver_uri("sqlite:file.db").unwrap(),
            ("sqlite", "file.db")
        );
        assert_eq!(
            parse_driver_uri("postgresql://h/db").unwrap(),
            ("postgresql", "postgresql://h/db")
        );
        assert!(parse_driver_uri("nocolon").is_err());
    }
}
