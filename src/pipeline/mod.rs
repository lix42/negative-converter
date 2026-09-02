//! Pure pipeline stages between decode and encode: film-base estimation, color
//! transforms, and the stage wiring that threads them together.

pub mod color;
pub mod colorimetry;
pub mod display_tone;
pub mod film_base;
pub mod gain_map;
pub mod hdr;
pub mod input_semantics;
pub mod memory;
pub mod render_split;
pub mod sdr;
/// Test-only diagnostic harness for `algo/reference-anchored-sigmoid`. `cfg(test)` so it
/// never reaches the shipped binary; its asset-dependent entries are `#[ignore]`d.
#[cfg(test)]
pub mod shadow_metrics;
pub mod stages;
pub mod working_space;
