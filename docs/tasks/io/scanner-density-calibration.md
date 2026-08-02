# Scanner Density Calibration

## Goal

Establish what a scanner's numbers mean in **absolute** density, so that densities
published by film manufacturers can be used directly by reconstruction. Today
`io/input-data-semantics` resolves an input's *transfer* and *meaning* but not its
*absolute normalisation*, which leaves a real gap: a datasheet-derived parameter is
only usable if our density scale matches the densitometry the datasheet used
(Status M).

## Design

Two tiers, deliberately separated by what they demand of the user.

**Tier 1 — unexposed frame only (no new user action).** nc's workflow already needs
an unexposed frame or rebate to derive `Dmin`. On that frame, compute
`−log10(scan)` per channel *without* dividing by `Dmin`, giving base density on the
scanner's own scale, and compare it to the stock's published `D-min` (e.g. Ektar 100:
red ≈0.20, green ≈0.56, blue ≈0.77). This tests absolute normalisation and
per-channel alignment on a *uniform* patch rather than on interpreted picture
content.

**What tier 1 can and cannot conclude — state this plainly in the report.** One
known density fixes a zero point, not a slope. And nc already computes
`D = −log10(scan/Dmin)`, so it is *already* anchored at the base: a base measurement
supplies no new zero point. What it does supply is **three** known densities at once
— one per channel, spanning ≈0.57 for Ektar 100 — so if the scan reproduces that
spread the scale is aligned, and if it compresses it that *is* a slope signal. The
irreducible ambiguity is that a mismatch may be a wrong scale **or** a scanner filter
whose spectral response differs from Status M; one measurement cannot separate them.
Report the ambiguity rather than picking a side.

**Tier 2 — unexposed frame plus a grey card or step wedge.** Two or more known
densities give offset *and* slope per channel, resolving what tier 1 cannot. It asks
for a shot most users will not take, so it must be strictly optional and never a
precondition for conversion.

**A mismatch is not fatal, and the design must not treat it as one.** Manufacturer
data supplies the *relationship* between landmarks (mid-grey to diffuse white,
`Δ ≈ 0.36`); a locally measured `Δ` supplies the scale. An off-scale scanner just
means deriving contrast from the measured `Δ` instead of the published one — a
different number, the same method. The profile is therefore a *correction* to apply,
not a gate that blocks conversion.

**Output shape.** A scanner profile is keyed by scanner + scan settings, which is a
different axis from the film stock — the two multiply, and neither substitutes for
the other. Whatever is applied to pixels must be reported with provenance, and the
uncalibrated path must stay the default so existing conversions do not silently
change. Related but distinct:
[scanner ICC before-density experiment](../color/scanner-profile-before-density-experiment.md)
concerns applying a *colour* transform before density conversion; this task concerns
the *density scale* and should not be conflated with it.

## Implementation Suggestion

- Run tier 1 as a measurement/report first and look at real numbers before designing
  any correction. If SilverFast output turns out to be normalised to a clear gate,
  tier 1 numbers will line up and little correction is needed; if it is
  auto-normalised they will not — and *that* is itself the answer to the open
  normalisation question, so an unexpected result is a finding, not a failure.
- The published `D-min` values come from the Spectral-Dye-Density charts and are
  chart readings (±0.05, sensitive to the assumed Status M wavelength). Do not treat
  agreement to two decimals as meaningful.
- Reuse the per-stock reference densities from
  [film-stock profiles](../algo/film-stock-profiles.md) rather than duplicating a
  second copy of datasheet data. Neither task blocks the other; if that task has not
  landed, read the two or three values needed inline and leave a pointer.

## How to Verify

- On the committed fixture rolls, tier 1 reports measured base density per channel
  beside the published `D-min`, with the difference and the per-channel spread — and
  states explicitly whether the scan appears clear-gate-normalised.
- The report distinguishes "scale aligned", "scale compressed/stretched", and
  "cannot distinguish scale from spectral response" rather than asserting one.
- The default conversion path is byte-identical with no profile selected.
- With a profile applied, the resolved report names it and the correction, and the
  same profile reapplied reproduces the output bit-exactly.
- Tier 2, if implemented, recovers a known slope from a synthetic two-density input.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, `cargo test` pass.

## Dependencies

- [Input data semantics and validation](input-data-semantics.md)
- [Reference-anchored sigmoid calibration and redesign](../algo/reference-anchored-sigmoid.md)
