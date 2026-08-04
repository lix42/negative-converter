# Sigmoid Parameter Calibration

## Goal

Turn the **provisional** sigmoid parameters into calibrated ones. `reference-anchored-sigmoid`
deliberately filtered *forms* rather than tuning values, on samples that were chosen at
random and whose exposure the photographer could not reliably recall. This task fixes the
numbers once assets exist that can actually support it.

## Design

**What is provisional and why:**

- **contrast ≈ 2.07** — derived as `0.745 / Δ` from the datasheet mid→diffuse-white Δ of
  0.36. The derivation is sound; Δ itself rests on manufacturer *tabulated* values, so this
  is the firmest of the three.
- **shoulder ≈ 0.6** — chosen because it begins bending at `D′` 0.70, essentially at
  mid-grey (0.67), where a print shoulder belongs; 1.0 begins at 0.45 and flattens the whole
  upper range. Judged on ten frames by eye.
- **the per-stock anchor offsets** — the weakest. They derive from **chart-read `D-min`**
  values, which `algo/film-stock-profiles` records as *not* true Status M densities. Residuals
  against user preference were **systematic per stock** (Ektar ≈ +0.6 EV, Portra 160 ≈ 0,
  Gold 200 ≈ −1.0), which is exactly the signature of constants each off by a fixed amount.
- **the no-roll-Dmax fallback** — `NOMINAL_DMAX = 2.0` is badly wrong against measured rolls
  (0.90–1.74, median ≈1.34); ~1.35 is provisional pending more rolls.

**What the calibration needs, and why more frames alone will not do it.** Per-frame exposure
preference cannot select a parameter: asking which EV looks best *is* frame optimisation, and
the whole point is to be honest to the film. Only central tendency is usable. So the assets
must supply **known** references rather than more guesses:

- a **bracketed roll** (one subject at −2 … +2 EV) so exposure labels are true by
  construction rather than by recall — this is what makes exposure-preservation *verifiable*
  instead of merely "consistent with";
- a **grey card in frame**, giving a real ~18% reference under the same illumination as a
  diffuse white, which is what the datasheet Δ actually specifies and what only 2 of 10
  existing frames could approximate;
- ideally the **calibrated transmission step wedge** from
  [scanner density calibration](../io/scanner-density-calibration.md), which turns the
  scale question from an assumption into a measurement.

Reuse `pipeline::shadow_metrics` and the review-page tooling; this task should need no new
measurement machinery, only better inputs.

## Implementation Suggestion

- Fix the parameters **one at a time**, in order of firmness: anchor offsets (weakest), then
  shoulder, then contrast. Changing several at once makes a regression unattributable.
- Any parameter change alters default pixels once sigmoid is the default curve, so coordinate
  the `pipeline_version` row with whoever owns activation.
- Prefer correcting a stock's *published* constant over adding a fudge term — a per-stock
  offset that cannot be traced to a publication is exactly the drift the registry exists to
  prevent.

## How to Verify

- Exposure preservation is **verified**, not assumed: the bracketed roll's known ±EV steps
  come out in the correct order with the expected spacing under one frozen recipe.
- The grey card lands at 0.18 within a stated tolerance across stocks, with **no** per-frame
  correction.
- Per-stock residuals lose their systematic sign, which is what would show the constants
  rather than the form were at fault.
- The report states which parameters moved, by how much, and on what evidence.
- Full CI gate passes; golden fixtures recaptured deliberately where pixels move.

## Dependencies

- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)
- [Film-stock profiles](film-stock-profiles.md)
- [Scanner density calibration](../io/scanner-density-calibration.md)
