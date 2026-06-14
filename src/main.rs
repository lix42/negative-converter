//! `nc` — film-negative → positive converter.
//!
//! `main` is a thin entry point: it delegates to [`cli::run`] and maps any
//! [`NcError`] to its stable process exit code (design-spec §11). All real work
//! happens in the pure pipeline stages; `main`/`cli` are the only orchestrators.

// Scaffolding: the stage stubs aren't wired together until `pipeline-orchestration`,
// so most public items are unused for now. Remove this once that task lands and the
// pipeline references every stage.
#![allow(dead_code)]

mod algo;
mod cli;
mod io;
mod pipeline;
mod types;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code() as u8)
        }
    }
}
