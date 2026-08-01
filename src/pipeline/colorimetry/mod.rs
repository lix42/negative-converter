//! The single source of truth for NC's standards-based RGB colorimetry.
//!
//! Every standards-derived matrix and luma vector the pipeline multiplies by
//! lives here, next to the source data it came from and a test that re-derives
//! it. Before this module the same coefficients were spread across
//! `working_space`, `sdr`, `hdr`, and `gain_map` as bare literals with no
//! recorded provenance, and two of them had silently been derived with different
//! chromatic-adaptation conventions.
//!
//! ## Four kinds of number, kept visibly apart
//!
//! 1. **Standard definitions** ([`definitions`]) — primaries, white points,
//!    cone-response matrices, transfer constants, and normatively *tabulated*
//!    vectors, each with its standard and edition. Editing one of these is how a
//!    colour-space update starts.
//! 2. **Derived artifacts** ([`pinned`]) — the reviewed, checked-in RGB↔RGB
//!    matrices and luma weights the runtime actually uses.
//! 3. **Product policy** — reference white, peak luminance, shoulder, gamut
//!    policy, gain-map limits. These deliberately **stay with the stage that owns
//!    them** (`pipeline::hdr`, `pipeline::gain_map`); they merely refer to the
//!    named colour space here instead of repeating its colorimetry.
//! 4. **Verification values** — tolerances and independent reference vectors,
//!    which live in the test modules. Independent references are deliberately
//!    *not* centralized: a reference that shares a source with the thing it
//!    checks validates nothing.
//!
//! ## The runtime never derives
//!
//! [`derive`] — the binary64 derivation math — is `#[cfg(test)]`, so rendering
//! cannot accidentally start computing coefficients per run, and NC stays
//! independent of an installed ICC/CMM for these transforms. The shipping binary
//! contains only reviewed literals.
//!
//! ## Changing something here
//!
//! Follow `docs/colorimetry-maintenance.md`. The short version: edit a *named
//! source definition*, never a matrix literal; run the check/regeneration
//! harness; review the coefficient diff; then decide explicitly whether the
//! change is representation-only or a pixel change needing a pipeline-version,
//! fingerprint, and baseline review.

pub mod definitions;
pub mod pinned;

#[cfg(test)]
pub mod derive;

#[cfg(test)]
mod audit;

#[cfg(test)]
mod tests;
