# Scanner Density Calibration

> **Needs re-evaluation before pickup (2026-09-13).** The file carries two designs that
> disagree. The `Design` section specifies a **transmission step wedge** and a profile "keyed
> by scanner + scan settings, a different axis from the film stock". The 2026-09-08 input says
> the wedge is blind to dye cross-talk and the fit is a **3×3 + offset from a ColorChecker
> through film**, which is scanner × film × development and not separable into a scanner
> profile. Settle which instrument and which model this task delivers, and rename the
> output if it is not a scanner profile. It also overlaps
> `color/optional-color-correction-profiles` (chart fitting, profile format, provenance);
> decide where correction lives before either builds tooling.



## Goal

Establish what a scanner's numbers mean in **absolute** density, so that densities
published by film manufacturers can be used by reconstruction. Today
`io/input-data-semantics` resolves an input's *transfer* and *meaning* but not its
*absolute normalisation*, which leaves a real gap: a datasheet-derived parameter is
only usable if our density scale can be related to the densitometry the datasheet
used.

## Input from `algo/film-stock-profiles` (2026-09-08, that task's close-out)

This task is now **unblocked and load-bearing**: it owns the largest known colour gap in the
renderer, with measurements to aim at.

- **The gap, quantified.** Inverting each stock's published curve removes blue's
  exposure-dependent cast (drift +1.26 → +0.09 stops per unit corrected density, against
  +1.29 predicted) but leaves **green at +0.40 mean, +1.00 on the Ektar roll** across 21
  frames. Blue transfers; green does not.
- **Added 2026-09-09: a per-channel gain now ships as a default, and it is a placeholder
  this task should replace.** `density.scale` defaults to `[1, 0.84, 0.73]` under the
  parametric curves — `[1, 0.90, 0.86]` at `pipeline_version` 4, re-calibrated at **5**
  (2026-09-16) from 31 hand-marked neutral patches over five rolls. Two measurements make
  it this task's business rather than the curve's:
  - **The scan's green and blue slopes against red are nearly equal** (1.115 and 1.183),
    where the datasheets say they are far apart (green barely steeper, blue 16%). An excess
    landing on *both* channels against red is not film chemistry — it is the signature of
    something in the scan/decode/base path, i.e. exactly this task's subject.
  - **The gain nulls the corpus mean, not any roll.** Per-roll residuals still span ±0.5
    stop per density on the green–magenta axis (`curve_probe::sigmoid_scale`, see below), and no single
    scale improves it — even the scan-derived corpus mean only moves \|green–magenta\| from
    0.60 to 0.56. A scale-shaped correction has no generic setting worth shipping, which is
    the negative result arguing for the 3×3 below.
  - The (since retired) `characteristic` curve kept the **identity** gain so this residual
    stayed visible rather than half-absorbed: its own solved gain (`[1, 0.938, 0.985]`)
    measured *worse* than identity on real frames (0.047 against 0.039).
- **The form to fit is a 3×3 + offset, not per-channel gains.** ACES applies exactly that
  (`CDD → CID`) *before* its per-channel curves, and nc is the same chain minus that stage.
  SMPTE ST 2065-2 NOTE 3 says the conversion between scanner density and a standard density
  metric is "3 × 3 matrix transformations followed by an offset", is **product specific**,
  and is "likely imperfect". A per-channel gain cannot be the whole answer: it cancels in
  nc's base division.
- **No datasheet correction substitutes for a measurement.** Ektar 100's sheet is the corpus
  outlier (it draws R and G nearly parallel, ratio 1.002 against everyone else's 1.02–1.05,
  and disagrees with its own aim table by 11%) — yet replacing its channel relationship with
  the corpus consensus would move its residual only +1.00 → **+0.87**. The dominant term is
  in the scan, not the sheet.
- **What a calibration frame has to contain** (see that task's close-out discussion): a
  *neutral series*, not a single patch — the residual is a **slope**, so one grey card at one
  exposure fits an offset and cannot separate offset from slope. Neutrals alone constrain
  only the matrix diagonal and the offsets; the **off-diagonal terms need coloured patches**,
  because cross-channel contamination is a property of the dye spectra. A transmissive step
  wedge isolates the scanner but is blind to dye cross-talk for the same reason.
- **Sample size matters more than it looks.** Per-frame residuals scatter at sd 0.3–1.6, and
  one fixture roll spans −1.88…+2.03. Resolving a 0.3 stops/density difference needs ~11
  frames of the same condition. Two per-roll/per-stock conclusions were retracted during
  that task for reading n=3–4 too confidently.
- Diagnostic, **no longer in-tree**: `algo::curve_probe::channel_drift` (asset-gated,
  `#[ignore]`d) reported scan / predicted / residual drift per channel, per stock and per
  roll, with scatter; `sigmoid_scale`, `whole_roll_scale` and `whole_roll_white_point`
  measured the scalar path's per-channel slopes. The module read the published curves
  through the `characteristic` inversion and was deleted with it
  (`nf-retire/characteristic`); recover it with `git show 9b34848:src/algo/curve_probe.rs`
  (and `src/algo/characteristic.rs` from the same commit) to score a candidate matrix.
  Its roll names predate the asset folder's rename, so it needs re-pointing first.

### Shooting the calibration frames

The protocol agreed with the user 2026-09-08 now lives in
[capture the calibration frames](../analysis/calibration-frame-capture.md), which owns
producing them. Kept here only as the reason each requirement exists:

- a **neutral series**, not one patch — the residual is a *slope*, so one grey card at one
  exposure fits an offset and cannot separate offset from slope;
- **coloured patches** on top of it — neutrals constrain only the matrix diagonal and the
  offsets, while the off-diagonal terms are dye cross-talk;
- a **bracket**, which makes the fit illuminant-independent where it matters: a one-stop
  change multiplies exposure by 2 in *every* channel whatever the light, so relative log
  exposures are known exactly and each channel's response shape is recovered absolutely,
  leaving one unknown constant per channel — which the "+ offset" term absorbs and white
  balance handles downstream. Only channel *balance*, not channel *shape*, depends on the
  illuminant;
- **two rolls**, because per-frame residuals scatter at sd 0.3–1.6 and one roll cannot
  separate "the film and scanner" from "that roll" — the fork this diagnosis sits on;
- a **shared development batch and scanning session** with real frames, SilverFast's
  per-frame adjustments off or locked — a per-frame-adjusting scanner makes a calibration
  from one frame untransferable to the others.

A transmissive step wedge would isolate the scanner from the film — useful if that
distinction ever matters, but blind to dye cross-talk for the same spectral reason a
neutral target cannot constrain the off-diagonal terms.

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
  copy — which is why that task is a prerequisite. Duplicating datasheet values across
  two modules is exactly the silent-drift risk the `pipeline/colorimetry/` pattern
  exists to prevent.
- The diagnostic *measurement* on real scans is performed by
  [reference-anchored sigmoid](../algo/reference-anchored-sigmoid.md)'s baseline
  harness, which is why this task depends on it: this task productises the result
  (a reportable, reusable profile), it does not perform the first measurement.

## How to Verify

- Tier 1's logic is covered by a **synthetic committed fixture** (a known scan value in,
  the expected `−log10(scan)` out), so a clean checkout can verify it with no assets.
- On the **external** real rolls — which live in the machine-local, uncommitted
  `../nc-assets` and must be identified by their `manifest.json` entry (roll + frame +
  `sha256`), not assumed present — tier 1 reports measured `−log10(scan)` per channel
  beside the nominal published `D-min`, driven through the
  `scripts/real-scan-verify/` harness. The report **explicitly states** that the
  comparison cannot establish absolute scale without an open-gate reference. This half
  cannot run in CI and must skip with a clear message when the assets are absent.
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
- [Film-stock profiles](../algo/film-stock-profiles.md) — supplies the per-stock nominal
  reference densities this task must not duplicate
- [Capture the calibration frames](../analysis/calibration-frame-capture.md) — produces the
  ColorChecker bracket the calibrating tiers fit against. A **hard** edge, not prose, because
  this task's goal is absolute density and tier 1 is explicitly non-calibrating: no amount of
  tier-1 work reaches the goal. (Contrast `film-base/dmax-anchor-reliability`, where the holder
  prerequisite is recorded in prose because it blocks one of four directions, not the goal.)
  Tier 1 remains implementable early if a baseline diagnostic is wanted before the frames

`algo/reference-anchored-sigmoid` is now **transitive** via `algo/film-stock-profiles`.

---

**2026-09-13 — the calibration shoot is wanted by four tasks; plan it once.**
[`analysis/calibration-frame-capture`](../analysis/calibration-frame-capture.md) (filed
2026-09-12) owns producing them and is the dependency edge above; this note is the reasoning
that independently reached the same conclusion. The target this task specifies (a ColorChecker Classic: neutral series for the diagonal and offsets, coloured
patches for the off-diagonal terms) overlaps two other open needs, and the user has it on
their roadmap:

- [`algo/sigmoid-parameter-calibration`](../algo/sigmoid-parameter-calibration.md) wants a
  **bracketed** roll (one subject at −2 … +2 EV) with a **grey card in frame**;
- [`film-base/dmax-per-channel-reduction`](../film-base/dmax-per-channel-reduction.md) parked
  on 2026-09-13 for want of exactly this — its roll-scoped measurement assumes scene colour is
  uncorrelated with density, which the available rolls (one trip, one palette) violate.

A ColorChecker **plus** a bracket, on a roll that also carries a grey card, satisfies all
three — four with `algo/split-default-migration`'s release gate. Shooting for only one of them
wastes the others. The protocol lives in the capture task; do not restate it here.
