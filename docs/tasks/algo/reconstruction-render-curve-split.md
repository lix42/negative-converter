# Reconstruction / Render Curve Split

## Goal

Try moving the tone shaping out of **reconstruction** and into **rendering**:
a modified exponential as the density→linear reconstruction, with the sigmoid
character applied by the display stage instead. Decide from measured results
whether that is a better split than today's reference-anchored sigmoid, which
does both jobs in one curve.

This is an experiment with a verdict, not a refactor with a known destination.

## Why

Two observations, both measured, point the same way.

**It is closer to the architecture's own rule.** CLAUDE.md's core fidelity rule is
that density conversion and print rendering are *separate sub-stages* that must
not be collapsed. The reference-anchored sigmoid places floor, midtone and
shoulder during reconstruction — real tone shaping, upstream of the ACEScg
boundary. Moving the shaping downstream would restore the separation the design
asks for, and `pipeline::sdr` / `pipeline::hdr` already apply a
reference-white-preserving shoulder, so part of the machinery exists.

**It explains the headroom result.** On one Gold 200 frame, same base and same
`Dmax`, only the curve differing: the sigmoid's HDR rendition peaks at *exactly*
the 203-nit reference white (`GainMapMax` 1.0x), while the exponential reaches
2.2827 log2 ≈ **4.87x**. The exponential pins white at `Dmax` with no placement
rule, so contrast pushes values past reference white; the sigmoid places diffuse
white *at* it by construction. Whatever range reaches the display stage is decided
here, which is why this task and the HDR question are the same question.

Note what this is *not*: the film is not the limitation. Negative stock carries
wide latitude. The print rendering decides whether output exceeds diffuse white.

## Open questions

- **What should the reconstruction curve be?** Open again. This task was filed assuming a
  *modified exponential*, with `algo/exponential-anchor-placement` settling what "modified"
  meant. That task finished 2026-08-29 and **ruled the exponential out**: measured on ten
  real frames, it is not competitive at any anchor — given the sigmoid's own anchor (0.875)
  it blows **21.4%** of the frame to absolute white with **zero** top-decile separation,
  against 6.9% / 19 code values for the sigmoid, because it has no shoulder and therefore
  hard-clips wherever the anchor is put. At high anchors it merely converges on the sigmoid.
  So the anchor was never its problem, and a placement rule does not rescue it. Whatever
  reconstruction curve this task adopts, "the straight line plus a better anchor" is not it.
- **What that task did establish, which sizes this one:** every single-anchor form sits on
  one measured frontier — anchor 1.293 → 0.906 moves |EV| 2.75 → 0.03, the base 10 → 38, and
  highlight separation 122 → 19 code values, monotonically. That is the two-points-not-three
  constraint measured rather than argued, and it is the quantified case that a second stage
  is needed at all.
- **What should reconstruction still own?** The floor and the `Dmax` anchoring are
  plausible keepers even if the S-shape moves. "Everything" and "nothing" are both
  probably wrong.
- **The HDR-headroom question is already answered — the shoulder owns it, not the anchor.**
  Measured 2026-08-28: shoulder 0.6 → `GainMapMax` 1.000x, shoulder 0.2 → 1.000x, shoulder
  0.0 → 4.866x, and the exponential reads 4.866x under `white-at-dmax` *and* under
  `black-at-base` — identical across completely different anchors. The sigmoid's shoulder
  runs during **reconstruction** and removes every above-white value before either display
  branch sees it, so SDR and HDR receive identical input and their ratio is 1.0 by
  construction. This is this task's premise stated mechanically. Caution: 4.866x is 98.8% of
  the declared 4.926 headroom, so turning the shoulder off does not buy graceful HDR — it
  saturates the ceiling.
- **A display-stage black point already does what a toe cannot**, which is evidence for the
  split: `print.black_point = 0.019` over mid@base+0.508 gives |EV| 0.13 with the base at
  1/255, dominating every single-anchor form on both axes at once, and it lands in the
  display stage so `film-master` keeps the unclipped rendering. Note the counterpart is
  **refuted**: widening the toe 0.2 → 0.4 → 0.6 moved the base 38 → 41 → 44, so "reconstruction
  places mid, a toe recovers black" is dead on arrival.
- **Is the existing display shoulder enough**, or does the display stage need a
  real parameterised curve? If the latter, whose knobs are they — `print.*`, or a
  new render-curve object — and how does that interact with `film-master`, which
  bypasses print controls entirely and would then get an *unshaped* image?
- **What happens to `film-master`?** It is defined as the intentional film
  rendering *including* the curve. If the curve moves downstream, the master's
  meaning changes. This may be the sharpest constraint in the task.
- **How is "better" judged?** Per-frame preference is frame optimisation and cannot
  select a parameter (the lesson `algo/sigmoid-parameter-calibration` records).
  Decide the evidence standard before generating results.

## Known vs unknown

**Known:** the 1.0x vs 4.87x measurement is real and reproducible; the display
stages already carry a shoulder; the sigmoid's current placement is
`MidAtDmaxFraction(0.5)` by default; `pipeline_version` would move if any default
render changes, with the golden drift gate and a before/after report.

**Unknown:** whether the split improves the *picture* at all, what reconstruction curve
replaces the ruled-out exponential, and whether `film-master` survives the change intact.

**Before generating results, read the metric warning** in the `exponential-anchor-placement`
progress entry: three measures in the committed `pipeline::shadow_metrics` harness mislead
(`sat%`, `flat%`, and highlight separation as a linear ratio, which inverts against visual
review). The surviving pair is `blown%` and separation in **code values**.

## How to Verify

- A written verdict either way, with measurements on real rolls — "we tried it and
  the sigmoid stays" is a complete outcome and must be recorded as one.
- If it lands: a `pipeline_version` bump with a before/after report, and
  `film-master`'s definition reconciled explicitly rather than left to drift.
- Whatever the outcome, the HDR headroom question is answered as a side effect —
  record what the chosen split does to it.

## Dependencies

- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)
- [Film-master and shared display pipeline](../color/film-master-render-pipeline.md)
