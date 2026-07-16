//! `nc` library target — exists so the criterion benches (`benches/`) and any
//! future integration harness can reach the pipeline internals; the `nc` binary
//! (`main.rs`) remains the only shipped, supported interface (design-spec §8).
//!
//! Note on hygiene: with a library target, `pub` items count as reachable, so
//! rustc's dead-code lint no longer flags unused public API. Don't add public
//! surface here casually — every module below is `pub` because the binary or
//! the benches genuinely consume it.

pub mod algo;
pub mod cli;
pub mod io;
pub mod pipeline;
pub mod types;
