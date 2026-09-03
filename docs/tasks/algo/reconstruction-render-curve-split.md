# Reconstruction / Render Curve Split

## Goal

Move the tone shaping out of **reconstruction** and into **rendering**, and decide
from measured results whether that is a better split than today's
reference-anchored sigmoid, which does both jobs in one curve.

**Verdict reached 2026-09-02: yes.** The split is confirmed on seven real frames at
matched lightness, and the direction had already been chosen by a user visual
verdict on `output/display-tone-mapping`. What is left is scope-limited — see
"Where it stands".

## Where it stands

Scope agreed with the user: **verdict + reconstruction shape + `film-master`
reconciliation**. The default migration is *not* in this task — it inherits a
calibration and a colour-model fix this task does not own, and is filed separately.

- **The reconstruction curve is the shipped sigmoid with both knees off.** The
  reviewed `s0-reinhard` config is `--sigmoid-shoulder 0 --display-tone reinhard`;
  chunk A then measured the toe as buying nothing and costing 1–2 code values of
  black depth, so reconstruction sheds it too. `toe = shoulder = 0` is bit-exactly
  the exponential, which rescopes `algo/exponential-anchor-placement`'s negative
  verdict: that measured the curve under the *old fixed-ceiling knee* at anchor
  0.875, and the pairing was what failed, not the curve.
- **`film-master` is reconciled and was never the constraint it looked like.** Its
  contract is the configured reconstruction, not a curve shape; it renders the
  shoulder-less form at exit 0 with nothing clipped, and its report prose already
  names the stage generically.
- **The black point is a per-frame control, not a default.** 0.019 — the value
  measured as dominating every reconstruction-side form — crushes 0.69–8.66% of
  every frame to code 0, which its metrics could not see. ~0.005 is the largest
  fixed value safe on all seven frames.
- **The anchor placement is deferred with a reason**, not left open: this probe
  cannot adjudicate it, because its target is matched to a `Dmax`-reading render.
  It needs a grey card on a bracketed roll — `algo/sigmoid-parameter-calibration`'s
  recorded precondition.

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

- ~~**What should the reconstruction curve be?**~~ **Answered 2026-09-02: the shipped
  sigmoid with `toe = shoulder = 0`**, which is bit-exactly the exponential
  (`convert_with_knees_off_matches_exponential_bit_exactly`). The paragraph below is kept as
  the reasoning that led here, and its conclusion is **rescoped, not reversed**: the
  exponential lost under the *old fixed-ceiling knee* at its own anchor. Under the shipped
  unbounded operator at lightness-matched anchors it measures 3.89-5.95% blown against the
  sigmoid's 6.11-6.87 on the same seven frames. Original note follows. This task was filed assuming a
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
- ~~**What should reconstruction still own?**~~ **Answered 2026-09-02: the density
  conversion, the contrast and the anchor — and neither knee.** "Everything" and "nothing"
  were both wrong, as guessed, but the floor was *not* a keeper: the toe changes `blown%` by
  less than 0.01pp and separation by under 0.2 code values on every frame, while costing 1-2
  code values of black depth. Black placement belongs to the display stage.
- **The HDR-headroom question is already answered — the shoulder owns it, not the anchor.**
  Measured 2026-08-28: shoulder 0.6 → `GainMapMax` 1.000x, shoulder 0.2 → 1.000x, shoulder
  0.0 → 4.866x, and the exponential reads 4.866x under `white-at-dmax` *and* under
  `black-at-base` — identical across completely different anchors. The sigmoid's shoulder
  runs during **reconstruction** and removes every above-white value before either display
  branch sees it, so SDR and HDR receive identical input and their ratio is 1.0 by
  construction. This is this task's premise stated mechanically. Caution: 4.866x is 98.8% of
  the declared 4.926 headroom, so turning the shoulder off does not buy graceful HDR — it
  saturates the ceiling.
  **Resolved 2026-09-02, and it names a candidate pairing for this task.** That caution holds
  only while the *display* stage keeps the fixed-ceiling knee. `output/display-tone-mapping`
  shipped an unbounded display operator, and shoulder-less reconstruction under it puts
  **0.26–0.61%** of the frame on the top gain code against the knee's **6.6–15.2%** — the
  saturation is a property of the knee, not of removing the shoulder. On a four-frame visual
  review that pairing was **preferred** over both the shipped default and shoulder-less
  reconstruction under the old knee (user verdict, 2026-09-02).
  So this task has a working half already: *reconstruction without a shoulder, character
  supplied at the display stage* is exactly the split, and it now has a reviewed rendition
  rather than only an argument. Two things it does **not** settle — which reconstruction
  curve (`algo/exponential-anchor-placement` closed the exponential negatively, so that is
  still open), and the operator's 1.000-stop cost at diffuse white, which is a
  rendering-intent question inherited along with it. Note `GainMapMax` is the wrong
  instrument for judging any of this: it reads 4.87x vs 4.79x for the two, identical on
  every frame.
- **The display-stage black point is real but its measured value is not.** `0.019` crushes
  **0.69-8.66% of every frame to code 0** — invisible to |EV| and to a floor percentile, which
  is why the original note below reads as an unqualified win. Swept: ~0.005 is the largest
  fixed value safe on all seven frames, and the linear subtraction trades floor against
  crushing roughly 1:1, so this is a per-frame grading control rather than a default. Original
  note follows. A display-stage black point already does what a toe cannot, which is evidence for the
  split: `print.black_point = 0.019` over mid@base+0.508 gives |EV| 0.13 with the base at
  1/255, dominating every single-anchor form on both axes at once, and it lands in the
  display stage so `film-master` keeps the unclipped rendering. Note the counterpart is
  **refuted**: widening the toe 0.2 → 0.4 → 0.6 moved the base 38 → 41 → 44, so "reconstruction
  places mid, a toe recovers black" is dead on arrival.
- **Is the existing display shoulder enough**, or does the display stage need a
  real parameterised curve? If the latter, whose knobs are they — `print.*`, or a
  new render-curve object — and how does that interact with `film-master`, which
  bypasses print controls entirely and would then get an *unshaped* image?
- ~~**What happens to `film-master`?**~~ **Answered 2026-09-02, and it was not the
  constraint it looked like.** Its contract is the configured reconstruction, not a
  curve shape — `render_split::film_master` is a pure unwrap that takes no
  `PrintParams` — so the branch already varies with every curve knob. It renders the
  shoulder-less form at exit 0 with nothing clipped, and its report prose names
  "density curve" generically. What the sentence "defined as the intentional film
  rendering *including* the curve" described was the current default's shape, not the
  definition.
- ~~**How is "better" judged?**~~ **Settled before generating results, as asked:** every
  candidate matched to the benchmark's mean encoded lightness (an unmatched comparison is
  evidence for a different claim), scored on `blown%` and separation in **code values** on
  clamped output, with `crushed%` added as the shadow counterpart. Each row asserts the render
  hit the statistic it solved for, and that guard was verified to fail.

## Known vs unknown

**Known:** the 1.0x vs 4.87x measurement is real and reproducible; the display
stages already carry a shoulder; the sigmoid's current placement is
`MidAtDmaxFraction(0.5)` by default; `pipeline_version` would move if any default
render changes, with the golden drift gate and a before/after report.

**All three unknowns are now answered** (2026-09-02) and are kept here as the record of
what the task set out not knowing: the split *does* improve the picture (both metrics, all
seven frames, plus a user visual verdict); the reconstruction curve is the shipped sigmoid
with **both knees off**, which is bit-exactly the exponential — so the ruled-out curve was
ruled out as a *pairing*, not as a curve; and `film-master` survives intact, because its
contract is the configured reconstruction rather than a curve shape.

**Metric warning, now five entries.** Three measures in `pipeline::shadow_metrics` mislead
(`sat%`, `flat%`, and highlight separation as a **linear ratio**, which inverts against
visual review); `output/display-tone-mapping` added a fourth (separation taken over
*unclamped* samples is unbounded); and this task added `crushed%` because neither `|EV|` nor
a floor percentile can see shadow clipping — which is how `black_point = 0.019` was recorded
as dominating on both axes while pinning up to 8.66% of a frame to code 0. The surviving set
is `blown%`, separation in **code values** on clamped output, and `crushed%`.

## How to Verify

- A written verdict either way, with measurements on real rolls — "we tried it and
  the sigmoid stays" is a complete outcome and must be recorded as one.
- If it lands: a `pipeline_version` bump with a before/after report, and
  `film-master`'s definition reconciled explicitly rather than left to drift.
  **Half-met, deliberately.** The reconciliation is done (chunk B). The version bump is
  **not** — it moved to `algo/split-default-migration` with the rest of the default
  activation, because it inherits a colour-model blocker this task does not own. Closing
  this task with that bullet outstanding is the agreed scope, not an oversight.
- Whatever the outcome, the HDR headroom question is answered as a side effect —
  record what the chosen split does to it.

## Dependencies

- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)
- [Film-master and shared display pipeline](../color/film-master-render-pipeline.md)
