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
    match &cli.data_root {
        Some(root) => Paths::rooted(root),
        None => Paths::from_env(),
    }
}
