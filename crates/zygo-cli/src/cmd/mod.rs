//! Command implementations.

pub mod agent;
pub mod api;
pub mod backend;
pub mod bench;
pub mod doctor;
pub mod image;
pub mod logs;
pub mod otlp;
pub mod run;
pub mod shell;
pub mod spec;
pub mod supervisor;

use zygo_core::Paths;

use crate::cli::Cli;

/// Directory layout for this invocation, honouring `--data-root`.
pub fn paths(cli: &Cli) -> Paths {
    let paths = match &cli.data_root {
        Some(root) => Paths::rooted(root),
        None => Paths::from_env(),
    };
    // `--data-root` decides where everything lives, *except* the runtime
    // directory when it has been named outright. Without this the two halves
    // can be reproduced separately and disagree — see `Paths::with_runtime`.
    match std::env::var_os("ZYGO_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => paths.with_runtime(std::path::PathBuf::from(dir)),
        None => paths,
    }
}
