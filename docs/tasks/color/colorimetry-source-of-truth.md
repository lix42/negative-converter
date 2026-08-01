# Colorimetry Source of Truth and Update Workflow

## Goal

Make every standards-based colorimetry coefficient auditable and maintainable
without changing current pixels. Centralize the authoritative color-space
definitions and pinned derived artifacts, migrate the existing transforms, and
provide a reproducible workflow for adding or updating a color space later.

This is deliberately deferred until the gain-map implementation is complete.
It records accepted technical debt and does not block the current gain-map
implementation. Future lossless HDR TIFF work depends on this source of truth so
that it does not introduce another generation of duplicated coefficients.

## Design

Add `src/pipeline/colorimetry/` as the single source of truth for the
project's standards-based RGB colorimetry. Give the module small explicit data
types for chromaticities, white points, RGB primaries, 3×3 matrices, and named
color-space definitions. Cover every space and adaptation currently used by the
pipeline, including Rec.709/sRGB, Display P3, BT.2020, ACEScg/AP1, D65, the ACES
white point, and the selected chromatic-adaptation method.

Keep four kinds of numbers visibly separate:

1. **standard definitions** — primaries, white points, transfer-function
   constants, and normative identifiers, each documented with its standard,
   edition/version, table or clause when available, and a stable source link or
   bibliographic reference;
2. **derived artifacts** — RGB↔XYZ transforms, chromatic-adaptation matrices,
   composed RGB↔RGB transforms, and luma coefficients;
3. **product policy** — NC choices such as reference white, peak luminance,
   shoulder, gamut policy, and gain-map limits;
4. **verification values** — tolerances, independent reference vectors, and
   calibration measurements.

Runtime rendering must use reviewed, checked-in coefficients rather than
deriving them on every run or depending on an installed ICC/CMM
implementation. Preserve the existing coefficient precision and operation
order so this refactor is bit-identical. If re-derivation exposes a real
coefficient correction, stop and split that behavioral change into a separate
task with the appropriate pipeline-version and output-baseline review; do not
hide it inside this refactor.

Move or replace the duplicated standards-based matrices and luma weights in
`pipeline/working_space.rs`, `pipeline/sdr.rs`, `pipeline/hdr.rs`,
`pipeline/gain_map.rs`, and relevant `pipeline/color.rs` tests. Names such as
`BT2020_TO_DISPLAY_P3`, `DISPLAY_P3_LUMA`, and their peers should resolve to the
central definitions, with module documentation explaining what direction,
encoding domain, white point, and coefficient precision each artifact uses.
Product-policy constants remain with the stage that owns the policy, but should
refer to the named color-space definition instead of repeating colorimetry.

Implement the derivation and verification math in Rust using `f64`: normalized
primary matrices, matrix inversion/composition, the selected chromatic
adaptation transform, and derivation of luma weights. Tests compare those
results with the pinned runtime coefficients. At least one independent
standards-derived reference vector per transform must prevent a shared mistake
in the generator and the generated values from validating itself.

Create `docs/colorimetry-maintenance.md` with the future update workflow:

1. identify the exact standard revision and record the changed source data;
2. edit the named source definition, never an unexplained matrix literal;
3. run a checked-in, documented Rust command in check or regeneration mode;
4. inspect and review the coefficient diff rather than rewriting it at build
   time;
5. run invariant tests, independent vectors, before/after pixel comparisons,
   and the full Rust quality gates;
6. determine whether the change is a representation-only refactor or a pixel
   change requiring pipeline-version, fingerprint, baseline/report, and design
   updates;
7. record the decision and source revision in the task/progress history.

The update command must be deterministic, idempotent, and CI-checkable. It may
be a small Rust developer tool or a repository test harness with an explicit
regeneration mode, but it must not be a Python-only path that CI never
exercises, a build script that silently rewrites source, or runtime derivation.

This task does not change transfer functions, tone mapping, gamut mapping,
reference-white/peak policy, or the output preset behavior. It does not replace
Little CMS where ICC transforms are the intended mechanism.

## How to Verify

- Each supported color space has one authoritative definition with precise
  provenance, and searches find no remaining unexplained duplicate matrices or
  luma vectors in the migrated modules.
- `f64` derivation tests reproduce every pinned transform and luma vector within
  an explicitly justified tolerance.
- Tests cover white/neutral preservation, primary and independent colored
  reference vectors, inverse/round-trip behavior, matrix direction, white-point
  adaptation, and luma-row consistency.
- The maintenance command's check mode passes on a clean tree; regeneration is
  deterministic and a second run produces no diff.
- Curated same-machine before/after comparisons show bit-identical pixels for
  every migrated runtime transform, and the existing golden/drift gates remain
  unchanged.
- A fixture that intentionally changes a source definition makes the check mode
  or pinned-coefficient tests fail, proving that future source updates cannot
  leave stale derived artifacts unnoticed.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, and `cargo test` pass.
- The refactor does not change the current pipeline fingerprint. Any discovered
  numerical correction is reported and moved to a separately reviewed
  behavioral task.

**As built:** the module is a directory rather than one file, because the
generated audit artifact and the `#[cfg(test)]` derivation want their own files:
`definitions.rs` (category 1), `pinned.rs` (category 2), `derive.rs`,
`audit.rs`, `tests.rs`, and the generated `derived-artifacts.txt`. The module
path `pipeline::colorimetry` is unchanged.

## Dependencies

- [ISO gain-map HDR output](../output/gain-map-hdr-output.md)
