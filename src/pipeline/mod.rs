//! Pure pipeline stages between decode and encode: film-base estimation, color
//! transforms, and the stage wiring that threads them together.
//!
//! **Two chains live here during the migration** (`docs/nf-migration.md`). The
//! shipped one runs `stages::render_display_source` (`render_split`) → `sdr`/`hdr`,
//! or `stages::render_film_master` for the master; the one
//! `--new-flow` selects is [`chain`], composing [`scene_correction`] → [`look`] →
//! [`fit_range`] → [`fit_gamut`] over the shared buffer in [`working_image`].
//! The new stages are named for the job they do rather than for the migration, so
//! that retiring the old path is a deletion and not a rename.
//!
//! `--new-flow` reaches the new chain (`nf-core/minimal-end-to-end`): the fixed decode
//! (`algo::fixed`) feeds it, and it renders into the destination `crate::destination`
//! resolves. Scene correction applies white balance and exposure, the look highlight
//! desaturation, fit range compresses the scene's range against the destination's peak,
//! and fit gamut maps into the destination's gamut; the stage epics fill the rest.
//! [`white_balance`] holds the white-balance statistics: the current chain's per-frame
//! estimators and the roll measurement's levels ([`roll_white`]).

/// The SDR/HDR branch contract on real frames (`nf-display-stages/branch-contract`).
#[cfg(test)]
mod branch_probe;
pub mod chain;
/// Goldens for the new flow's stages (`nf-verification/stage-goldens`).
#[cfg(test)]
mod chain_golden;
pub mod color;
pub mod colorimetry;
pub mod display_tone;
pub mod film_base;
pub mod fit_gamut;
pub mod fit_range;
pub mod gain_map;
pub mod gain_ratio;
pub mod hdr;
pub mod input_semantics;
pub mod look;
pub mod memory;
pub mod pixels;
pub mod render_split;
pub mod roll_white;
pub mod scene_correction;
pub mod sdr;
/// Test-only diagnostic harness from `algo/reference-anchored-sigmoid`. `cfg(test)` so it
/// never reaches the shipped binary; its asset-dependent entries are `#[ignore]`d.
#[cfg(test)]
pub mod shadow_metrics;
pub mod stages;
pub mod white_balance;
pub mod working_image;
pub mod working_space;
