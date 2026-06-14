//! CLI orchestration — arg structs, recipe load/merge, report emit.
//!
//! Stub: filled in by the `cli-framework` task. The signature is fixed now so
//! `main` can compile against it.

use crate::types::Result;

/// Parse arguments and run the requested subcommand. The single entry point the
/// binary's `main` calls.
pub fn run() -> Result<()> {
    todo!("cli-framework: parse args, load/merge recipe, dispatch subcommand")
}
