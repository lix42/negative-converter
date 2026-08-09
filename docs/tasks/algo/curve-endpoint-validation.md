# Curve endpoint validation (does this config place its endpoints usefully?)

## Goal

Warn, **before decoding**, when a resolved density curve places its tonal
endpoints so badly that the render cannot approach display white or paper black —
the failure mode that produces a washed-out image from a config passing every
existing check. The endpoints are functions of the resolved curve parameters
alone, so this needs no pixels and no image statistics. Covers **both** curves:
exponential and sigmoid.

## Design

### The failure mode, and why nothing catches it

The shipped sigmoid defect (`algo/reference-anchored-sigmoid`) was exactly this:
white pinned at `Dmax` with `contrast = 1.0` left the paper-black asymptote at
0.053 linear — the darkest confirmed shadow patch sat at 72/255, which is why the
complaint was "pale", not "dark". It took a blind visual review plus a
purpose-built metric harness to find. **The black endpoint was computable from the
config the whole time.**

The same hole is open on the *default* curve. Exponential renders
`10^(γ·(D′ − Dmax))`, so the **film base** — the darkest tone in a normal frame —
renders to `10^(γ·(D′base − Dmax))`, and a user who measures `Dmax` from a leader
and passes it with the default `γ = 1.0` gets (at the default `D′base = 0`):

| `γ` | `Dmax` | base renders to | 8-bit | verdict |
|---|---|---|---|---|
| 1.0 | 2.0 (nominal default) | 0.010 | 25/255 | ok |
| 1.0 | 1.2758 (measured Gold 200) | 0.053 | 65/255 | pale blacks |
| 1.0 | 0.391 | **0.406** | 171/255 | badly broken |

That last row is not hypothetical — it is a real invocation made while writing
`docs/using-nc.md`. nc reported `output lost 126296 clipped … (18.15%)` and said
**nothing** about the base rendering at 40%. The existing loss counter is an
*encode-side symptom* check: it sees highlights leaving the top of the range but is
structurally blind to a black end that never approaches the bottom.

### The two endpoints, stated precisely

Both come from the resolved curve params. **Evaluate them through the renderer's
own curve function, not a re-derived closed form** — the closed forms below are for
explaining the result in the message, not for gating.

**Exponential** (`10^(γ·(D′ − Dmax))`):

- *Black* — **there is no asymptotic floor**: the curve continues toward 0 as
  `D′ → −∞`. The meaningful quantity is the **film base endpoint**, `D′base`, which
  is `density.offset` — *not* 0 — because `D′ = scale·D + offset` and the base sits
  at `D = 0`. Regional balance can shift it further. So the rule is
  `10^(γ·(D′base − Dmax))`, and `10^(−γ·Dmax)` is only its `offset = 0` special
  case. Using the special case as the rule would both falsely warn a config whose
  corrected base reaches deep black and miss one whose base is lifted.
- *White* — reached **exactly** at `D′ = Dmax` by construction; content above
  `Dmax` exceeds 1.0 and clips. Real content is known to measure above a
  leader-derived `Dmax` (`film-base/dmax-anchor-reliability`), so some clipping is
  expected rather than a defect, and the encode loss counter already reports its
  magnitude. The exponential half of this task is therefore the **black endpoint**.

**Sigmoid** (`t = contrast·(D′ − A)` through toe then shoulder):

- *Black* — the true asymptote is `s_curve(D′ → −∞)`, which is the pre-shoulder
  floor `10^(−contrast·A)` **passed through the shoulder soft-min**. The two agree
  for the shipped `shoulder = 0.6` (0.008621 vs 0.008623) and are equal only at
  `shoulder = 0`, but they diverge materially inside the valid range
  (`SIGMOID_KNEE_MAX = 10`): the naive form over-states the floor by 1.75× at
  `shoulder = 3` and 5× at `shoulder = 5`, which would warn on configs that in fact
  render a *deeper* black. Take the value from the renderer.
- *White* — evaluate `s_curve(R)`: what a pixel **at the reference density**
  renders to.

### What the white check does and does not claim

**It is a reference-placement check, not a reachability check.** The sigmoid is
monotonic and approaches 1.0 asymptotically, so *sufficiently dense content always
renders arbitrarily close to white* — no configuration makes white literally
unattainable. `s_curve(R) = 0.576` says the reference tone renders well below
white, i.e. the anchor is placed such that the roll's own reference material comes
out dim. That is the defect worth reporting; phrasing it as "white is unreachable"
would be false and would warn on frames whose content legitimately exceeds `R`.
Word the message accordingly.

The healthy and broken configurations, computed from the shipped formulas:

| Config | `A` | `A − R` | `s_curve(R)` | black asymptote | verdict |
|---|---|---|---|---|---|
| **shipped** mid@0.5, c = 2.069 | 0.998 | −0.278 | 0.939 | 0.009 | ok |
| mid@0.5, c = 1.15 | 1.287 | +0.012 | 0.650 | 0.033 | reference renders dim |
| mid@0.5, c = 1.0 | 1.383 | +0.107 | 0.576 | 0.041 | reference renders dim |
| **the old defect** white@Dmax, c = 1.0 | 1.276 | 0.000 | 0.660 | 0.053 | black end too high |
| white@Dmax, c = 2.0 | 1.276 | 0.000 | 0.660 | 0.003 | ok |

(`R = 1.2758`, Gold 200. `toe = 0.2`, `shoulder = 0.6`.)

### Two further traps

**1. Do not demand that white be reached exactly.** With `shoulder > 0` the sigmoid
never reaches 1.0 for any finite density — that is deliberate, and it is what makes
u16 highlight clipping impossible. A rule of "fully-exposed film must render to
1.0" would fail every valid sigmoid config.

**2. `A ≤ R` is only a proxy, not the metric.** The `white@Dmax` rows have `A = R`
exactly yet render `R` to 0.660, because the shoulder is already compressing there.
Test `s_curve(R)` itself. The closed-form condition
`contrast ≥ 0.745 / ((1−f)·R)` is still worth deriving as an *explanatory* number
for the message ("this contrast is below the X your reference implies"), but it must
not be the gate.

### Where the check can run, per `Dmax` source

`cli::validate` sees only the resolved config, which is enough for `Fixed` and
`Explicit` but **not** for `Auto`:

| `DmaxSource` | Numeric `R` known pre-decode? | Where the check runs |
|---|---|---|
| `Fixed` (default) | yes — the nominal constant | `cli::validate`, before decode |
| `Explicit(d)` | yes | `cli::validate`, before decode |
| `Auto` | **no** — `resolve_dmax` computes it inside `density::reconstruct` from the *post-balance* corrected densities | either skip it, or evaluate where the resolved value exists and report through the same warning channel |
| `None` (`--no-d-max`) | n/a | exempt (exponential only — see below) |

Decide `Auto` explicitly rather than by omission: silently letting the pre-decode
path evaluate a placeholder for `Auto` would be worse than skipping it. Note `Auto`
is already an opt-in mode that `film-master` rejects outright, so skipping it in the
pre-decode gate is a defensible first cut — but say so in the task's outcome.

### Exemptions

- **`--no-d-max` is an exponential-only exemption.** On exponential it resolves the
  anchor to `0.0`, the base renders to white and exposed detail sits above it — the
  intended scene-referred mode for HDR f32 workflows, so the black rule must not
  fire. On **sigmoid it is already a hard usage error** and must stay one:
  > `usage: the sigmoid curve needs a display-white anchor (the default fixed
  > anchor, --d-max <d>, or --auto-d-max); --no-d-max / curve.dmax = none is only
  > supported by the exponential curve`

  Do not turn that existing hard failure into a soft exemption.
- **`simple` reconstruction** has no curve, no `Dmax` and no anchor. Out of scope;
  say so rather than leaving it ambiguous.
- A deliberately flat **diagnostic** render is legitimate — notably
  `--sigmoid-white-at-d-max`, retained precisely so the original defect can be
  reproduced on demand. So this is a **report warning** (`--strict` promotes), never
  a hard error. Refusing to render the diagnostic that exists to show the bug would
  be self-defeating.

## Implementation Suggestion

- A pure function in `algo` taking the resolved curve and the resolved `Dmax`,
  returning the two endpoint values; `cli::validate` calls it and pushes warnings.
  Params-only, so it runs before decode and costs nothing.
- **Compute both endpoints through the renderer's own curve**, not a second
  closed-form implementation. `s_curve` is private to `algo::sigmoid`; expose a
  narrow `pub(crate)` endpoint helper rather than making the curve public, so the
  validator and the renderer cannot drift apart — a hand-rolled duplicate is exactly
  how these checks go stale, and it is what makes the shoulder mistake above easy.
- **The thresholds need calibrating, and the report already supplies the labels.**
  `reports/sigmoid-reference-baseline.md`'s candidate table is a ready-made
  fixture: the rejected candidate 1 (0.053) must trip, the shipped default (0.009)
  must not. Pick the boundary from those, and state it in the message rather than
  only the verdict.
- Message should name the knob that would fix it — the whole point is that the user
  sees "raise `--sigmoid-contrast`" instead of discovering pale output by eye.
- `validate` is shared verbatim by `convert` and `roll` (and each per-frame
  override), so this lands on every path for free — but that also means a noisy rule
  fires once per frame across a whole roll. Prefer one clear warning over two
  marginal ones.

## How to Verify

`cargo test` covering, at minimum:

- **The regression the project already paid for:** the old-defect config
  (`white-at-dmax`, `contrast = 1.0`, `R = 1.2758`) trips the black rule. This is
  the acceptance test — the check exists to have caught that from config alone.
- The three shipped rolls under the shipped defaults emit **nothing** (the
  falsifiable control; a rule that always fires is worthless).
- `mid@0.5` with `contrast = 1.0` trips the white rule, and the message names
  `--sigmoid-contrast`.
- Exponential `γ = 1.0` with `--d-max 0.391` trips the black rule; with the nominal
  `Dmax = 2.0` it does not.
- **A non-zero `density.offset` moves the exponential black endpoint** — a config
  whose `10^(−γ·Dmax)` looks bad but whose actual `D′base` renders deep must *not*
  warn, and vice versa. This is the test that pins the `offset = 0` special case as
  a special case.
- **Shoulder sensitivity:** a large-but-valid `--sigmoid-shoulder` (e.g. 3.0) whose
  naive pre-shoulder floor would trip the threshold but whose true asymptote does
  not must emit nothing.
- `--no-d-max` emits nothing on **exponential**; on **sigmoid** it remains a usage
  error (exit 2) with the existing message — assert both, so the exemption cannot
  silently swallow the hard failure.
- `simple` reconstruction emits nothing.
- Whatever `Auto` resolution is chosen is asserted explicitly — skipped or
  evaluated — rather than left to fall out of the implementation.
- `--strict` promotes the warning to exit 1, using the IR-free fixture
  `tests/fixtures/hdr-48bit.tif` plus a no-override control run, per the project's
  strict-assertion convention.

**No pixel change.** This adds report warnings only; `PIPELINE_FINGERPRINTS` must be
untouched and no `pipeline_version` bump is owed.

## Dependencies

- [Density-domain algorithm](density.md)
- [Reference-anchored sigmoid calibration and redesign](reference-anchored-sigmoid.md)
- [Pipeline orchestration](../core/pipeline-orchestration.md)

**Boundary vs [Density safety bounds](density-safety-bounds.md)** — deliberately
complementary, and the split is by *mechanism*. That task enforces per-parameter
physical bounds on `density_scale`/`offset`/`gamma`, plus a **post-render
histogram** collapse check. This one is a **closed-form, pre-decode** check on the
*joint* endpoint behaviour: `contrast = 1.0` and `shoulder = 0.6` are each entirely
reasonable in isolation and pass every per-parameter bound — only their combination
with the resolved `R` is broken. Coordinate the warning vocabulary if both land, so
a degenerate render does not report two differently-worded warnings for one cause.
