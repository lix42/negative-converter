//! Build script: capture the compile target triple into an env var the crate can
//! read at runtime.
//!
//! Cargo sets `TARGET` for build scripts but not for the crate compile itself, so
//! the telemetry record's `target` field (design-spec §9) would otherwise be
//! unavailable without a dependency. Re-exporting it as `NC_TARGET` keeps that
//! field dependency-free (`env!("NC_TARGET")`).

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=NC_TARGET={target}");
}
