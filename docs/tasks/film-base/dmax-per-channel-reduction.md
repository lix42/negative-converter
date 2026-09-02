# Per-channel Dmax and the gray-mean reduction

## Goal

Establish whether reducing the measured per-channel reference `Dmax` to a scalar
**gray mean** discards information that changes the render, and if so, decide where
a per-channel term belongs. This is an **investigation with a quantified impact
verdict** — "the scalar is justified, close it" is a valid and useful outcome. No
behaviour change is presumed, and none ships from this task without a separate
decision.

## Design

### What happens today

`algo::density::reference_dmax` measures the reference region's per-channel
base-relative densities `D_c = -log10(t_c / base_c)`, then reduces them to one
scalar by a plain gray mean `(r + g + b) / 3`. It retains `per_channel` **only** to
run the weakest-channel plausibility check — the code comment is explicit that a
coloured/wrong region can average to a plausible gray density while one channel
sits at the base. The curve then subtracts that scalar identically in every
channel.

Because `D_c` is already base-relative, the scalar carries no per-channel
structure *by construction*: `Dmin`'s per-channel ratio is fully consumed by the
division at step 1. The open question is whether the **highlight** end has the same
colour cast as the base, which is what a scalar anchor asserts.

### The measured spread — it is not zero

From the committed leader-uniformity data in
[`reports/sigmoid-reference-baseline.md`](../../reports/sigmoid-reference-baseline.md)
(per-channel base-relative densities at the leader — i.e. per-channel `Dmax`):

| Roll | R | G | B | gray mean | fixture `Dmax` | spread | in stops | widest |
|---|---|---|---|---|---|---|---|---|
| Gold 200 | 1.2242 | 1.2340 | 1.3628 | 1.2737 | 1.2758 | 0.1386 | 0.46 | B |
| Ektar | 1.2724 | 1.2865 | 1.3201 | 1.2930 | 1.2933 | 0.0477 | 0.16 | B |
| Portra 160 | 1.4402 | 1.3297 | 1.3807 | 1.3835 | 1.3816 | 0.1105 | 0.37 | R |

The gray mean reproduces each fixture `Dmax` to ~4 decimals, confirming the
reduction. Two things to note: the spread is **0.05–0.14 density (0.16–0.46
stops)**, and its **direction is not consistent** (blue densest on two rolls, red
on the third), so it is not a fixed cast that one constant could absorb.

Rendering the leader through the scalar anchor at `gamma = 1` gives, for Gold 200,
`R 0.892 / G 0.913 / B 1.228` — a visibly blue "white" with blue clipping.

### Why it may still not matter — and the one case where it does

**Exponential curve: algebraically redundant.** A per-channel anchor is exactly a
per-channel constant gain:

```
10^(γ(D'_c − Dmax_c)) = 10^(γ(D'_c − Dmax_s)) · 10^(−γ(Dmax_c − Dmax_s))
                        └── scalar anchor ──┘   └── constant per-channel gain ──┘
```

nc already exposes two per-channel knobs that span that space —
`print.white_balance` (gain, downstream) and `reconstruction.density.offset`
(density, upstream). So under the exponential curve, scalar `Dmax` + WB loses no
expressive power; the information is relocated, not destroyed.

**Sigmoid curve: not redundant.** With `t = contrast·(D′ − A)`, a per-channel
anchor places each channel at a *different point on the toe and shoulder*. A gain
applied afterwards cannot reproduce that — the curve is nonlinear. Since the
reference-anchored sigmoid is the intended product default, this is the case that
decides the task, and a 0.46-stop channel offset lands where the shoulder is
compressing.

### The questions to answer

1. **Magnitude under sigmoid.** Render the fixture frames with the scalar anchor
   versus a per-channel anchor and measure the difference. The existing
   `shadow_metrics` harness and the frozen fixtures already provide the frames and
   the measurement surface.
2. **Is the ratio a stable stock property?** The level is known to be uncontrolled
   (`film-base/dmax-anchor-reliability`: same-stock rolls 0.295 density apart while
   their red base agrees to 0.0005). **Level and ratio are different claims** — the
   level depends on how much light struck the leader, while the inter-channel ratio
   may be characteristic of the dye layers. Test it on the same-stock sibling
   pairs already in the asset set (`Portra400` vs `Portra400-leica-flaw`; the
   Portra 160 pair). This is the cheapest experiment and it gates the answer to 3.
3. **Where the term belongs, if anywhere.** Three outcomes, each with a different
   home:
   - *Absorbed* — the sigmoid delta is negligible, or WB is an adequate remedy in
     practice. Record the justification and close; the scalar becomes a decision
     rather than an inheritance.
   - *Stable per stock* — belongs with the per-stock constants in
     `algo/film-stock-profiles`, not in the measurement.
   - *Per-roll* — needs a measured per-channel anchor, which is a **schema change**
     (`reconstruction.curve.dmax` scalar → per-channel) plus a
     `pipeline_version` bump owned by `core/conversion-versioning`. File that as a
     follow-up task; do not ship it here.

## Implementation Suggestion

- Start with question 2 — it is a pure re-read of existing measurements and can
  invalidate the expensive parts of 1 and 3 before they are run.
- `reference_dmax` already returns `ReferenceDmax { scalar, per_channel }`, so the
  per-channel values need no new measurement code; they are measured and then
  dropped. A temporary `#[ignore]` test that prints the derived numbers is enough
  for the investigation — do not read sample pixels into an agent context.
- Reproduce the fixture measurements with the committed harness:
  `cargo test --release --bin nc shadow_metrics::measure_candidates -- --ignored --nocapture --test-threads=1`
  (fixtures: `scripts/sigmoid-baseline/fixtures.json`).
- Watch the confound: the leader is a *uniform field at an uncontrolled level*. If
  the three layers have different contrast, the per-channel ratio measured at an
  unknown exposure level is not the ratio at a *different* level — so a ratio that
  looks unstable across rolls may be reporting exposure variation rather than
  stock variation. Comparing same-stock pairs is what separates those.
- Blue is the least spatially uniform channel in every leader measured (tile range
  2–4× red's), so sample the interior and use a robust central measure.

## How to Verify

A written finding lands in `docs/reports/` (a new report, or a section appended to
the sigmoid baseline) containing:

- Per-channel `Dmax` and the spread for **every** roll in the asset set, not only
  the three in the current table.
- Measured scalar-versus-per-channel render deltas on the fixture frames under the
  **sigmoid** default, reported the way the baseline report does it (mid/shadow
  medians and highlight saturation share), so the two are directly comparable.
- The same-stock ratio comparison for at least one sibling pair, with an explicit
  statement of whether the ratio is stable, unstable, or undecidable at this n.
- An explicit verdict — *absorbed* / *stable per stock* / *per-roll* — with the
  follow-up task filed if a change is warranted.

Verified as complete when the verdict is recorded and either (a) it is "absorbed"
and the rationale is written down, or (b) the follow-up task exists with its home
epic chosen. **This task changes no pixels**; any render change is a separate task
carrying its own `pipeline_version` bump.

## Dependencies

- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
- [Reference-anchored sigmoid calibration and redesign](../algo/reference-anchored-sigmoid.md)

Related but deliberately **not** dependencies:
[Dmax anchor reliability](dmax-anchor-reliability.md) examines the same anchor on a
different axis (its *level* rather than its per-channel *ratio*) and will read the
same leader measurements — coordinate if both run at once, but neither blocks the
other. [Film-stock profiles](../algo/film-stock-profiles.md) is the likely home if
the ratio proves stock-stable.

---

**2026-08-31 — the premise below shifted.** This task's "why it may not matter" argument
rests on the per-channel term being redundant under the **exponential** but not under the
**sigmoid, which is the intended default**. `output/display-tone-mapping` is measuring a
shoulder-less exponential reconstruction paired with a real display tone mapper, and the
user's visual review preferred it. **Confirmed 2026-09-02:** that pairing was preferred on
the HDR review too, and `output/display-tone-mapping` closed with the operator shipped — but
opt-in, with no default moved, and the reconstruction half still open in
`algo/reconstruction-render-curve-split`. So the premise is firmer and the conditional below
is unchanged: it turns on the *default* adopting it, which has not happened.
If that direction lands, the redundancy argument applies
to the *default* path, and this task changes from "investigate whether the scalar is
justified" to "the correction the default render needs" — the sigmoid's shoulder is what
was washing the error toward white and hiding it. The measured leader spread here
(0.05–0.14 density, inconsistent direction) becomes **17–83% off neutral** on a grey target
at that path's `gamma = 2.03`. Worth re-reading before either task starts.

**User's objection, and it is the crux of whether this can work at all (2026-08-31):** the
per-channel numbers come from the leader, and *the leader `Dmax` is not accurate* —
`film-base/dmax-anchor-reliability` records two rolls of one stock **0.295 density apart**
while their bases agree to 0.0005. At `gamma = 2.03` a 0.295 error is a **4x** render
error, so a correction derived from an unreliable anchor could be worse than no correction.

That does not sink the task, but it renames its central question. The measured quantity is
a **difference between channels** (`D_b − D_g`), not an absolute level, and a difference can
be stable while the level it is measured from drifts — the two rolls disagreeing on *where*
the leader sits says nothing yet about whether they disagree on the *spread*. **Is the
per-channel spread reproducible across rolls of one stock, when the absolute `Dmax` is
not?** If yes, the spread is usable and the unreliable level cancels. If no, the leader
cannot source this correction and a different reference is needed. Answer that first; it is
cheap, and every other question here is downstream of it.

User's framing: this stays an **investigation with a verdict**, not a presumed fix — decide
when the number is in.
