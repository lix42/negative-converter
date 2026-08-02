# Scanner Density Calibration

## Goal

Establish what a scanner's numbers mean in **absolute** density, so that densities
published by film manufacturers can be used by reconstruction. Today
`io/input-data-semantics` resolves an input's *transfer* and *meaning* but not its
*absolute normalisation*, which leaves a real gap: a datasheet-derived parameter is
only usable if our density scale can be related to the densitometry the datasheet
used.

## Design

### What a scan value actually is

`io::decode`'s `normalize_u16` maps 16-bit samples to `f32` by dividing by 65535
(`src/io/decode.rs`). So a scan value is a **code-value ratio against full scale**,
not transmitted intensity over a measured reference intensity. Optical density is
`−log10(I/I₀)`, and `I₀` here is unknown: scanner exposure time and per-channel gains
shift the reference level arbitrarily between scans and between channels.

Everything below follows from that. **Absolute density requires a same-settings
open-gate (clear-gate) reference measurement** — a scan of the empty gate with
identical exposure and gain — to supply `I₀`. Without it there is no absolute scale,
only relative differences within one scan.

### Tier 1 — unexposed frame only: a diagnostic, **not** a calibration

The workflow already needs an unexposed frame or rebate for `Dmin`, so this tier is
free. On that frame, report `−log10(scan)` per channel beside the stock's published
nominal `D-min`.

**State plainly what this cannot do.** It cannot test absolute normalisation and
cannot define a correction, because an arbitrary per-channel offset sits between the
two quantities. A perfectly linear scan can disagree with the published `D-min`
purely from exposure and gain settings. Tier 1's value is as a **non-calibrating
diagnostic**: it establishes reproducibility across scans of the same roll, flags
gross anomalies (a channel near clipping, a wildly different scan setting between
frames of one roll), and records what the scanner reported so later work has a
baseline.

**Do not infer a density slope from the cross-channel spread.** The three channel
readings are one point on **three different response curves**, each with its own gain
and spectral sensitivity — not three points sampled from one curve. A compressed or
stretched spread can arise from channel gains alone, before any question of Status M
mismatch, and no individual channel has a second point from which a slope could be
identified. Classifying the spread as "scale compressed" or deriving a correction from
it would corrupt colour. Slope requires a second known density **in each channel**.

### Tier 2 — a calibrated transmission target

To determine offset *and* slope per channel, the second sample must be a **known
density**, which means a **calibrated transmission step wedge** measured through the
same scan settings (or a fully specified sensitometric procedure: controlled exposure
onto the stock, documented process, then densitometry of the result).

**A photographed grey card is not a known density.** The developed negative density of
a photographed card depends on illumination, exposure, processing, and the stock's
characteristic curve — so an unexposed frame plus an ordinary grey-card frame cannot
determine offset and slope, and this task must not promise that it can.

Tier 2 asks for a target most users will not have, so it must be strictly optional and
never a precondition for conversion.

### A mismatch is not fatal

Manufacturer data supplies the *relationship* between landmarks (mid-grey to diffuse
white); a locally measured difference supplies the scale in our own units. Relative
differences are usable without an absolute anchor because the unknown offset cancels.
So an uncalibrated scanner does not block anything — it means deriving the parameter
from the locally measured difference rather than from a published absolute value. The
profile is therefore a **correction to apply when available**, never a gate.

### Output shape

A scanner profile is keyed by scanner + scan settings, a different axis from the film
stock — the two multiply and neither substitutes for the other. Whatever is applied to
pixels must be reported with provenance, and the uncalibrated path stays the default so
existing conversions do not silently change.

Related but distinct:
[scanner ICC before-density experiment](../color/scanner-profile-before-density-experiment.md)
concerns applying a *colour* transform before density conversion; this task concerns the
*density scale*. Do not conflate them.

## Implementation Suggestion

- Run tier 1 as a measurement/report first and look at real numbers before designing any
  correction — but frame the report as a diagnostic, not a verdict on absolute scale.
- The published `D-min` values available today are **chart readings, not Status M
  measurements** (see [film-stock profiles](../algo/film-stock-profiles.md) for why
  single-wavelength sampling of a spectral-density curve is not a Status M density).
  Treat them as nominal and do not build a correction on them until a properly derived
  or manufacturer-tabulated Status M value exists.
- Reuse per-stock reference data from
  [film-stock profiles](../algo/film-stock-profiles.md) rather than keeping a second
  copy. Neither task blocks the other.
- The diagnostic *measurement* on real scans is performed by
  [reference-anchored sigmoid](../algo/reference-anchored-sigmoid.md)'s baseline
  harness, which is why this task depends on it: this task productises the result
  (a reportable, reusable profile), it does not perform the first measurement.

## How to Verify

- On the committed fixture rolls, tier 1 reports measured `−log10(scan)` per channel
  beside the nominal published `D-min`, and the report **explicitly states** that the
  comparison cannot establish absolute scale without an open-gate reference.
- The report does not classify the cross-channel spread as a scale error, and contains
  no correction derived from it.
- Tier 2, if implemented, recovers a known offset and slope per channel from a
  calibrated transmission step wedge (verifiable on a synthetic two-density input), and
  the docs do not claim a photographed grey card suffices.
- The default conversion path is byte-identical with no profile selected.
- With a profile applied, the resolved report names it and the correction, and the same
  profile reapplied reproduces the output bit-exactly.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, `cargo test` pass.

## Dependencies

- [Input data semantics and validation](input-data-semantics.md)
- [Reference-anchored sigmoid calibration and redesign](../algo/reference-anchored-sigmoid.md)
