// wasm32 only: after the sqlparser 0.62 / DataFusion 55 bump, proving the
// compile future Send/Sync (through sqlparser AST types) overflows the default 128.
#![recursion_limit = "256"]

/// Support for graceful shutdown on Ctrl+C or fail-fast trigger.
pub mod ctrl_c;

// Re-export the main library functionality
pub mod compilation;
pub mod dbt_lib;
pub mod driver;
pub use driver::{DbtCompilationDriver, DbtTaskExecutionDriver};
pub mod retry;
pub mod source_freshness;
pub mod vars;

pub mod install_method;
pub mod version_check;

pub use dbt_clap_core::from_lib;

pub mod partial_parse;
pub mod uninstall;
// ferrion-wasm: self-update (download + untar + replace the binary) is native only.
#[cfg(not(target_arch = "wasm32"))]
pub mod update;
mod utils;

mod main_impl;
pub use main_impl::{prepare_cli_or_exit, print_trimmed_error, run_cli, run_cli_with_code};
