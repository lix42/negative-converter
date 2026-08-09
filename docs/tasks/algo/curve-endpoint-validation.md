# Curve endpoint validation (can this config reach black and white?)

## Goal

Warn, **before decoding**, when a resolved density curve cannot actually reach
display white or paper black — the two failure modes that produce a washed-out
render from a config that passes every existing check. Both endpoints are
closed-form functions of the resolved curve parameters, so this needs no pixels
and no image statistics. Covers **both** curves: exponential and sigmoid.

## Design

### The failure mode, and why nothing catches it

The shipped sigmoid defect (`algo/reference-anchored-sigmoid`) was exactly this:
white pinned at `Dmax` with `contrast = 1.0` left the paper-black floor at 0.053
linear — the darkest confirmed shadow patch sat at 72/255, which is why the
complaint was "pale", not "dark". It took a blind visual review plus a purpose-built
metric harness to find. **The floor was computable from the config the whole
time**: `10^(−contrast·A)`.

The same hole is open on the *default* curve. Exponential renders
`10^(γ·(D′ − Dmax))`, so its floor is `10^(−γ·Dmax)` — and a user who measures
`Dmax` from a leader and passes it with the default `γ = 1.0` gets:

| `γ` | `Dmax` | floor | ≈ 8-bit | verdict |
|---|---|---|---|---|
| 1.0 | 2.0 (nominal default) | 0.010 | 25/255 | ok |
| 1.0 | 1.2758 (measured Gold 200) | 0.053 | 65/255 | pale blacks |
| 1.0 | 0.391 | **0.406** | 171/255 | badly broken |

That last row is not hypothetical — it is a real invocation made while writing
`docs/using-nc.md`. nc reported `output lost 126296 clipped … (18.15%)` and said
**nothing** about the 0.407 black floor. The existing loss counter is an
*encode-side symptom* check: it sees highlights leaving the top of the range but is
structurally blind to a floor that never approaches the bottom.

### The two metrics

Both are pure functions of the resolved curve params.

**Exponential** (`10^(γ·(D′ − Dmax))`):

- *Black* — floor `= 10^(−γ·Dmax)`. Rises toward 1.0 as `γ·Dmax → 0`.
- *White* — reached **exactly** at `D′ = Dmax` by construction, and content above
  `Dmax` exceeds 1.0 and clips. Since real content is known to measure above a
  leader-derived `Dmax` (`film-base/dmax-anchor-reliability`), some clipping is
  expected rather than a defect; the existing encode loss counter already reports
  its magnitude. So the exponential half of this task is primarily the **floor**.

**Sigmoid** (`t = contrast·(D′ − A)` through toe then shoulder):

- *Black* — floor `= 10^(−contrast·A)`, same shape, where
  `A = f·R + 0.745/contrast` for `MidAtDmaxFraction(f)` and `A = R` for
  `WhiteAtDmax`.
- *White* — evaluate `s_curve(R)`: what a fully-exposed pixel actually renders to.

The healthy and broken configurations, computed from the shipped formulas:

| Config | `A` | `A − R` | `s_curve(R)` | floor | verdict |
|---|---|---|---|---|---|
| **shipped** mid@0.5, c = 2.069 | 0.998 | −0.278 | 0.939 | 0.009 | ok |
| mid@0.5, c = 1.15 | 1.287 | +0.012 | 0.650 | 0.033 | white unreachable |
| mid@0.5, c = 1.0 | 1.383 | +0.107 | 0.576 | 0.041 | white unreachable |
| **the old defect** white@Dmax, c = 1.0 | 1.276 | 0.000 | 0.660 | 0.053 | floor too high |
| white@Dmax, c = 2.0 | 1.276 | 0.000 | 0.660 | 0.003 | ok |

(`R = 1.2758`, Gold 200. `toe = 0.2`, `shoulder = 0.6`.)

### Two traps the test must respect

**1. Do not demand that white be reached exactly.** With `shoulder > 0` the sigmoid
approaches 1.0 asymptotically and *never* reaches it for any finite density — that
is deliberate, and it is what makes u16 highlight clipping impossible. A rule of
"fully-exposed film must render to 1.0" would fail every valid sigmoid config.

**2. `A ≤ R` is only a proxy, not the metric.** The `white@Dmax` rows have `A = R`
exactly yet render `Dmax` to 0.660, because the shoulder is already compressing
there. Test `s_curve(R)` itself. The closed-form condition
`contrast ≥ 0.745 / ((1−f)·R)` is still worth deriving as an *explanatory* number
for the message ("this contrast is below the X your reference implies"), but it must
not be the gate.

### Exemptions

- **`DmaxSource::None` (`--no-d-max`)** resolves the anchor to `0.0`, so the floor
  formula yields `1.0` — the film base renders to white and exposed detail sits
  above it. That is the *intended* scene-referred mode for HDR f32 workflows, not a
  defect. It must be exempt from the floor rule, not merely tolerated.
- **`simple` reconstruction** has no curve, no `Dmax` and no anchor. Out of scope;
  say so rather than leaving it ambiguous.
- A deliberately flat **diagnostic** render is legitimate — notably
  `--sigmoid-white-at-d-max`, retained precisely so the original defect can be
  reproduced on demand. So this is a **report warning** (`--strict` promotes), never
  a hard error. Refusing to render the diagnostic that exists to show the bug would
  be self-defeating.

## Implementation Suggestion

- A pure function in `algo` taking the resolved curve (plus the resolved `Dmax`
  source) and returning the two endpoint values; `cli::validate` calls it and
  pushes warnings. Params-only, so it runs before decode and costs nothing.
- `s_curve` is currently private to `algo::sigmoid`. Expose a narrow
  `pub(crate)` endpoint helper rather than making the curve itself public, so the
  validator and the renderer cannot drift apart — computing white a second time by
  hand is exactly how these checks go stale.
- **The thresholds need calibrating, and the report already supplies the labels.**
  `reports/sigmoid-reference-baseline.md`'s candidate table is a ready-made
  fixture: the rejected candidate 1 (floor 0.053) must trip, the shipped default
  (0.009) must not. Pick the boundary from those, and state it in the message
  rather than only the verdict.
- Message should name the knob that would fix it — the whole point is that the user
  sees "raise `--sigmoid-contrast`" instead of discovering pale output by eye.
- `validate` is shared verbatim by `convert` and `roll` (and each per-frame
  override), so this lands on every path for free — but that also means a noisy rule
  fires once per frame across a whole roll. Prefer one clear warning over two
  marginal ones.

## How to Verify

`cargo test` covering, at minimum:

- **The regression the project already paid for:** the old-defect config
  (`white-at-dmax`, `contrast = 1.0`, `R = 1.2758`) trips the floor rule. This is
  the acceptance test — the check exists to have caught that from config alone.
- The three shipped rolls under the shipped defaults emit **nothing** (the
  falsifiable control; a rule that always fires is worthless).
- `mid@0.5` with `contrast = 1.0` trips the white rule, and the message names
  `--sigmoid-contrast`.
- Exponential `γ = 1.0` with `--d-max 0.391` trips the floor rule; with the nominal
  `Dmax = 2.0` it does not.
- `--no-d-max` emits nothing on either curve (the exemption is deliberate, so it
  needs its own test, not just absence of coverage).
- `simple` reconstruction emits nothing.
- `--strict` promotes the warning to exit 1, using the IR-free fixture
  `tests/fixtures/hdr-48bit.tif` plus a no-override control run, per the project's
  strict-assertion convention.

**No pixel change.** This adds report warnings only; `PIPELINE_FINGERPRINTS` must be
untouched and no `pipeline_version` bump is owed.

## Dependencies

- [Density-domain algorithm](density.md)
- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)

**Boundary vs [Density safety bounds](density-safety-bounds.md)** — deliberately
complementary, and the split is by *mechanism*. That task enforces per-parameter
physical bounds on `density_scale`/`offset`/`gamma`, plus a **post-render
histogram** collapse check. This one is a **closed-form, pre-decode** check on the
*joint* endpoint behaviour: `contrast = 1.0` and `shoulder = 0.6` are each entirely
reasonable in isolation and pass every per-parameter bound — only their combination
with the resolved `R` is broken. Coordinate the warning vocabulary if both land, so
a degenerate render does not report two differently-worded warnings for one cause.
