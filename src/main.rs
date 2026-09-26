//! `nc` — film-negative → positive converter.
//!
//! `main` is a thin entry point: it delegates to [`cli::run`] and maps any
//! [`NcError`](types::NcError) to its stable process exit code (design-spec §11). All real work
//! happens in the pure pipeline stages; `main`/`cli` are the only orchestrators.

mod algo;
mod cli;
mod destination;
mod film_stock;
mod flow;
mod io;
mod pipeline;
mod recipe;
mod telemetry;
mod types;
mod version;

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
