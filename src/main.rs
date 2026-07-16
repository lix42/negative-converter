//! `nc` — film-negative → positive converter.
//!
//! `main` is a thin entry point: it delegates to [`cli::run`] and maps any
//! [`NcError`](nc::types::NcError) to its stable process exit code (design-spec
//! §11). All real work happens in the pure pipeline stages; `main`/`cli` are the
//! only orchestrators. The modules live in the library target (`lib.rs`) so the
//! criterion benches can reach them.

use std::process::ExitCode;

use nc::cli;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code() as u8)
        }
    }
}
