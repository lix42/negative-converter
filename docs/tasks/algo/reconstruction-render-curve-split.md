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

- **What does "modified exponential" have to change?** The plain exponential has
  no `AnchorPlacement`, so raising contrast pivots around white — measured at 2.75
  EV of midtone displacement for the floor fix `gamma = 2.0` buys. That is the
  problem `algo/exponential-mid-grey-anchor` already exists for; this task may
  subsume it, depend on it, or want something different. Settle that early rather
  than duplicating it.
- **What should reconstruction still own?** The floor and the `Dmax` anchoring are
  plausible keepers even if the S-shape moves. "Everything" and "nothing" are both
  probably wrong.
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

**Unknown:** whether the split improves the *picture* at all, whether the
exponential can be made well-behaved enough to serve as the reconstruction curve,
and whether `film-master` survives the change intact.

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
