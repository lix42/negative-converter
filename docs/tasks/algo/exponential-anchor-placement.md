# Anchor Placement for the Exponential Curve

## Goal

Give the exponential density curve an anchor-placement rule, as the sigmoid has, so
its contrast knob stops fighting its endpoint placement. The chosen direction is to
**pin the black end at the film base** rather than the white end at `Dmax`.

Today the exponential renders `10^(contrast·(D′ − Dmax))`: display white is pinned at
`Dmax`, there is **no** placement rule, and raising contrast pivots the line *around
white*, dragging everything below it down.
`docs/reports/sigmoid-reference-baseline.md` measured that trade on user-confirmed
shadow patches: `contrast = 2.0` takes the black floor from 72/255 to 12/255 — the
"pale, not dark" defect it was raised for — while costing **2.75 EV of midtone
placement**. One knob cannot deliver the first without the second.

## Why the black end

**It anchors to the reliable measurement.** `Dmax` from a leader is uncontrolled —
`film-base/dmax-anchor-reliability` records two rolls of one stock **0.295 density
apart** while their base agrees to **0.0005**. Pinning the end that is measured ~500x
more precisely and letting the uncertain end float inverts the error budget in the
right direction, and makes the curve `Dmax`-free.

**The base is already the zero point.** Stage 1 is
`D_c = −log10(max(scan_c, EPS) / base_c)`, so the film base divides out to `D′ = 0`
(modulo `density.offset` and regional balance, which move it per channel on purpose).
Dmin is therefore already the black *reference*; what is missing is a stated black
*value*. Today the base lands at whatever `10^(−contrast·Dmax)` happens to give.

**It is not a new idea, and it already reviewed well.** `algo/reference-anchored-sigmoid`
tested black-pinning as candidate 5. Its first rejection was a parameter error — black
at 0.00061 with contrast 2.0 implies an anchor of 1.607, *above* every roll's `Dmax`
of 1.28–1.38, so nothing reached white. Retested at consistent targets (0.002 → anchor
1.349, 0.005 → 1.151), the log records it as "as legitimate as white- or mid-pinning …
and **Dmax-free**", and the user ranked **5b "most likely GO"**, behind only candidates
3 and 8. This task is the shippable form of that result.

## Design

Not predetermined. The obvious shape reuses the sigmoid's vocabulary rather than
inventing a second one:

- Add an `anchor` field to `ExponentialParams` carrying an `AnchorPlacement`, so both
  curves answer the same question with the same recipe spelling and one CLI flag family
  covers both. `"white-at-dmax"` keeps today's behaviour; the new variant pins the base
  to a stated floor. `{"mid-at-dmax-fraction": <f>}` remains available and is no longer
  the headline — the rename from `exponential-mid-grey-anchor` records that change of
  direction.
- The arithmetic is a **gain swap, not a new curve**. `density.rs` already notes that
  `10^(contrast·(D′−Dmax))` "factors into `10^(γ·D')` times a constant gain
  `10^(−γ·Dmax)`, so the anchor composes with `print_exposure` as one multiplicative
  scalar." A black pin replaces that emergent gain with a stated floor:
  `lin = floor · 10^(contrast·D′)`. Expressed as an anchor density it is
  `A = −log10(floor)/contrast`, which is why the retest above had to move the target
  and the anchor together.
- With `anchor = "white-at-dmax"` the rendered pixels must be **bit-identical** to
  today's, pinned by a golden — that equivalence is what keeps the straight line usable
  as the debuggable reference.
- Decide the **default** placement deliberately and separately from the mechanism. A
  default change needs a `pipeline_version` bump, a `PIPELINE_FINGERPRINTS` row and a
  measured report. Shipping the mechanism with `white-at-dmax` as the default and moving
  the default in a second step keeps both reviewable.

Things to keep straight while doing it:

- Per CLAUDE.md a new knob spans four coupled spots — the CLI `*Overrides` field, the
  recipe `*Params` field, a `merge` arm and a `validate` check — plus a merge test.
  `--sigmoid-mid-fraction` / `--sigmoid-white-at-d-max` currently reject a resolved
  *exponential* curve as a usage error; whatever flag surface this adds has to leave
  those cross-curve rejections coherent rather than half-lifted.
- The exponential's `dmax = "none"` case has no anchor today and a black pin needs
  none — worth checking that the two states stay distinguishable rather than collapsing.
- `D′base` is per channel. Pinning per channel is algebraically a per-channel gain, so
  it silently does white-balance work; pinning on the gray mean preserves the base's
  colour cast. Related reasoning in `film-base/dmax-per-channel-reduction`.
- design-spec §7.3 and §9 describe `anchor` as sigmoid-only; both need updating, and
  §9's `deny_unknown_fields` structs must move with them.

## Open questions

- **What is the black floor, and in which domain is it stated?** A linear value against
  the 203-nit reference white is not an sRGB code value; picking it in the wrong domain
  puts the floor visibly off. Candidate 5's retest used 0.002 and 0.005.
- **What does `DmaxSource` mean on this curve afterwards?** It stops being the anchor.
  Leaving a knob that no longer places anything is worse than removing it or
  redefining it. Not urgent — the user's read is that this matters *less* here than it
  does under white-pinning, where the unreliable anchor sets the white point.
- **Should contrast be derived from the density range instead of chosen?** Pinning both
  endpoints determines it: `contrast = (log10(white) − log10(floor)) / range`. On a 1.3
  roll with a floor of 0.0025 that yields **2.00** — reproducing the value the shipped
  default reached by eye, which is some evidence the formulation is right. **But** the
  `reference-anchored-sigmoid` log calls this "adaptive contrast, already rejected"; a
  search found only that back-reference and no recorded rationale, and it sits inside a
  sentence later retracted as false. So the status is *unexamined*, not settled. The
  substantive objection that does survive: deriving contrast from `Dmax` routes that
  anchor's unreliability into the slope, which the whole sigmoid task worked to avoid.
  If it is pursued, the range must come from the roll-fixed leader (a process variable)
  and never from content percentiles (which would be per-roll auto-levels).
- **Which stage places the picture — the sharpest question left.** A straight line
  places two points, not three, so with both endpoints pinned mid-grey is forced to the
  geometric mean. Two positions, both `Dmin`-referenced and mathematically one free
  parameter; the disagreement is only about ownership:
  **(A)** reconstruction pins the reliable end (black) and the *render* stage places
  white — which under a black pin at contrast 2.0 means supplying a ~2.8-stop gain
  before its toe does anything;
  **(B)** reconstruction places the picture (`Dmin` + offset) and the render stage only
  adapts to display limits — a toe pulling the base down, which *is* display adaptation.
  (B) is candidate 8, the best-measured form (0.78 EV, 27/255). The objection to (A) is
  that a gain of that size is *fixing* reconstruction rather than optimising for a
  display. Undecided; the enum carries both so the comparison needs no further knob work.

## Known vs unknown

**Known:** the 72/255 → 12/255 versus 2.75 EV trade is measured; the base is already
`D′ = 0`; the anchor already factors as one scalar gain, so the code change is small;
black-pinning reviewed at "most likely GO" as a *form*.

**Unknown:** the floor value, whether a derived contrast is defensible or was rejected
for a reason not recorded, and how far mid-grey actually falls once both ends are
pinned on real frames.

## Status

**Done — merged 2026-08-29 as #98** (see `docs/progress/algo.md`). All four placements
exist on both curves behind one curve-neutral `--anchor-*` flag family, the exponential
default stays `white-at-dmax`, and the default render is byte-identical to the previous
build — so no `pipeline_version` bump was owed.

**The rendering question above is closed, and the answer is negative.** Measured on ten
real frames: the exponential is not competitive at *any* anchor — given the sigmoid's own
anchor it blows 21.4% of the frame to white with zero top-decile separation, because it
has no shoulder. Its problem was never the anchor, so no default moves, and the black pin
this task was filed for (candidate 5b) is *dominated* by the shipped default when judged
as a whole picture rather than on shadow numbers. The floor therefore stays a
user-supplied parameter with no default to pick.

The A/B question — which stage places the picture — is deliberately **not** answered here
and is **not** refiled. It belongs to
[`algo/reconstruction-render-curve-split`](reconstruction-render-curve-split.md), which
owns the stage boundary; the enum carries both positions, so the comparison needs no
further knob work.

## How to Verify

- With `anchor = "white-at-dmax"` the rendered pixels are **bit-identical** to the
  current exponential, pinned by a `pipeline::stages::golden` vector.
- Under a black pin, raising contrast lowers the black floor **without** the 2.75 EV
  midtone shift, measured on the same frozen shadow patches
  `docs/reports/sigmoid-reference-baseline.md` used — that report is the acceptance
  metric, not a visual preference.
- Where mid-grey lands is **measured across a roll**, not judged on one frame; per-frame
  preference is frame optimisation and cannot select a parameter (the lesson
  `algo/sigmoid-parameter-calibration` records).
- Any change to the *default* placement carries a `pipeline_version` bump, a new
  `PIPELINE_FINGERPRINTS` row, and a measured report; a neutral-default opt-in refreshes
  only the current row's `recipe` hash.
- Full CI gate passes.

## Dependencies

- [Negative reconstruction and density curves](negative-reconstruction-density-curves.md)

Blocks nothing formally: the exponential is the explicit diagnostic straight line since
the sigmoid became the default curve.
[`algo/reconstruction-render-curve-split`](reconstruction-render-curve-split.md) consumed
this task's outcome and **closed 2026-09-02**.

**Downstream note — this task's verdict was rescoped, not reversed.** "Not competitive at
any anchor" was measured under the *shipped fixed-ceiling knee*. The curve-split task then
measured the same straight line under the unbounded display operator at lightness-matched
anchors and got **3.89-5.95% blown with 10.9-120.1 code separation**, against the shipped
sigmoid's 6.11-6.87. So the pairing failed, not the curve — and since `toe = shoulder = 0`
is bit-exactly this curve, the split's own endpoint *is* a straight-line reconstruction.
The anchor findings recorded here are unaffected.
