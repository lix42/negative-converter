# Reference-anchored sigmoid — baseline and candidate filtering

Evidence for [`algo/reference-anchored-sigmoid`](../tasks/algo/reference-anchored-sigmoid.md).
Records the shipped sigmoid's measured defect and filters the candidate anchoring forms.

**Scope, as reduced with the user on 2026-08-03:** this report **filters methods** — which
anchoring forms deserve support and which do not. It deliberately does **not** tune a
parameter. Establishing that contrast ≈2 ≫ 1.0 is sufficient; choosing between 2.07 and 2.5
belongs to a follow-up task once deliberately correctly-exposed samples exist. There is no
requirement that a single winner emerge.

## Fixtures

Frozen in [`scripts/sigmoid-baseline/fixtures.json`](../../scripts/sigmoid-baseline/fixtures.json)
(schema 1) — three rolls, ten `real` frames, with each patch's rectangle *and its
user-confirmed semantics*. Reproduce with:

```sh
cargo test --release --bin nc shadow_metrics::measure_candidates -- --ignored --nocapture --test-threads=1
```

| Roll | Stock | Dmin (r,g,b) | Dmax |
|---|---|---|---|
| `2026-07-24-Gold200` | Kodak Gold 200 | 0.6001831, 0.27512017, 0.14776836 | 1.2758015 |
| `Ektar` | Kodak Ektar 100 | 0.51679254, 0.2768597, 0.18973067 | 1.2933096 |
| `Portra160-2026-07-22` | Kodak Portra 160 | 0.49988556, 0.24776074, 0.14920272 | 1.3816013 |

**Patch validity is the load-bearing part of the fixture.** Auto-proposal ranks on density
and knows nothing about content, so semantics were confirmed by review:

- **valid diffuse whites: 2 of 10** — G2 (white lily), P4 (white painted sign). The rest are
  specular (E2, E3), sky (G3, P1), fog (E1, P2), a bright window opening (G1), or white but
  *sunlit and above Dmax* (P3). E1's is additionally contaminated by a scanning dust speck,
  which blocks light, is therefore dense in the negative, and renders as a false highlight.
- **valid mids: 7 of 10**; **valid shadows: 9 of 10**.
- **usable for the datasheet Δ: 2 frames** (G2, P4), since Δ needs a valid white *and* a
  valid mid *in the same illumination* — the datasheet specifies "receiving same
  illumination as subject". P3 has the best white in the set but pairs it with a shaded wall,
  which disqualifies the pair.

Invalid patches are **skipped**, not averaged in.

## The measured baseline defect

Every candidate reduces to one number — the sigmoid anchor `A` (`curve.dmax`) — plus a
contrast, since `t = contrast·(D′ − A)` is unchanged and only the rule for choosing `A`
differs. Measured on the **display path** (`pipeline::sdr::render`, Display P3), no exposure
applied, medians across frames:

| # | Candidate | ship | mid median | \|EV\| to 0.18 | shadow median | sat median |
|---|---|---|---|---|---|---|
| 1 | white@Dmax, c=1.0 — **shipped** | yes | 0.1634 | **0.14** | **72/255** | 6.39% |
| 2 | white@Dmax, c=2.0 | yes | 0.0267 | 2.75 | 12/255 | 6.82% |
| 3 | mid@0.5·Dmax, c=2.0 | yes | 0.0944 | 0.93 | 30/255 | 8.35% |
| 4 | auto (content-driven), c=2.0 | **no** | 0.0003 | 9.12 | 0/255 | 0.00% |
| 5a | black@0.002, c=2.0 | yes | 0.0220 | 3.04 | 9/255 | 6.53% |
| 5b | black@0.005, c=2.0 | yes | 0.0549 | 1.71 | 20/255 | 7.62% |
| 7 | auto (content-driven), c=0.745/Δ | **no** | 0.0002 | 9.51 | 0/255 | 0.00% |
| 8 | mid@(Dmin+datasheet), c=0.745/Δ | yes | 0.1049 | **0.78** | 27/255 | 8.98% |

Regenerated 2026-08-03 from the committed harness, replacing an earlier table that a
reviewer correctly identified as unreproducible: it predated splitting candidate 5 into
5a/5b and predated switching 4/7 from a "valid white patch" gate to `DmaxSource::Auto`.
**Rows 1, 2, 3 and 8 reproduce to the digit**, so every headline claim about the defect and
about the two leading forms was unaffected; only 4, 5 and 7 moved.

`sat median` replaced a `clip` column that read `0.00%` for every candidate — and
**structurally had to**: `sdr::render` *errors* on any sample outside `[0, 1]`, so a
returned image cannot contain one, and the old column was measuring nothing. The
replacement counts samples at or above 0.999, i.e. highlight separation the shoulder has
compressed against white.

**It validates against the blind visual review, in exact rank order.** P3 is the frame the
user singled out unprompted — "3 and 8 lost all the details on the curtain, 5b lost some
details, 1, 2, 5a keep the details" — recorded before this metric existed. Its P3 saturation
shares:

| candidate | 8 | 3 | 5b | 5a | 2 | 1 |
|---|---|---|---|---|---|---|
| sat% on P3 | 26.88 | 25.29 | 21.97 | 11.15 | 9.91 | 7.64 |
| visual verdict | lost all | lost all | lost some | kept | kept | kept |

The ordering matches across all six candidates, and the *three-way grouping* falls out too:
~25–27% for "lost all", ~22% for "lost some", ~8–11% for "kept". An independent numeric
statistic reproducing a visual judgement that precisely is the strongest single piece of
validation in this report — and it is exactly what the old column, pinned at 0.00%, could
never have supplied. Across all ten frames the medians are 8.35% (3) and 8.98% (8) against
the shipped default'"'"'s 6.39%, so mid-anchoring'"'"'s highlight cost is real but concentrated in
the high-dynamic-range frames rather than spread evenly.

4/7 saturate exactly 0.00% — not a virtue but a symptom, since their contaminated anchor
renders the whole frame near black.

**The defect, stated precisely: the shipped default gets midtones nearly right and blacks
badly wrong.** Candidate 1 needs only 0.14 EV to place a mid-grey, yet its darkest confirmed
shadow patch sits at **72/255**. That is why the complaint is "pale", not "dark" — and it is
reproduced here on user-confirmed shadow patches rather than inferred from arithmetic.

The mechanism is a pivot: with white pinned at Dmax, raising contrast steepens the line
*around white*, so everything below is dragged down. Contrast 2.0 fixes the floor
(72 → 12/255) but costs 2.75 EV of midtone placement. The two knobs fight, which is exactly
what an anchor other than white avoids.

## Filtering

**Reject — candidate 1 (contrast 1.0).** Fails the black gate at 72/255. This is the defect.

**Reject — candidate 5 (black-pinned at a fixed contrast).** Needs **+3.04 EV** at
`black@0.002` and **+1.71 EV** at `black@0.005`. Pinning black alone leaves white and mid
unplaced, so at any fixed contrast everything else lands arbitrarily — and the residual
tracking the black target that closely *is* that arbitrariness, not a tuning opportunity.
Making it work requires pinning a *second* point, which forces per-roll adaptive contrast —
already rejected, since contrast is film character worth preserving. (5b was the user's
"most likely go" on the visual review; it is rejected here as a **default**, which is a
different question from whether it renders acceptably.)

**Diagnostic only — candidates 4 and 7.** Their anchor is read from the frame's own content,
so an underexposed frame's lower diffuse white would be silently pulled up to 1.0, correcting
the very exposure the task requires preserving. That disqualification rests on the
**argument**, and the measurement adds an independent one: resolved through `DmaxSource::Auto`
they land at **9.1–9.5 EV** off and render every frame to **0/255**, because `Auto` takes its
percentile over the whole scan and the nearly-opaque film holder owns the top of it. That is
a defect in `Auto` rather than in the anchoring idea — it is now
[`algo/auto-anchor-interior-measurement`](../tasks/algo/auto-anchor-interior-measurement.md)
— so these two rows measure the bug, not the form. Candidate 7 remains valuable as the
zero-free-parameter check on the datasheet derivation, once `Auto` samples the picture area.

**Contingent — candidates 2 and 3.** Both pass the black gate, but both reference the
leader-measured Dmax, which three independent findings show is untrustworthy: same-stock rolls
differ by a full stop (0.295 density) while their bases agree to 0.0005; real content measures
*above* it (G3 1.3265 vs 1.2758; P3 1.5062 vs 1.3816); and grain sensitivity makes "fully
exposed" ill-posed, so a leader is a uniform field at an *uncontrolled* level. Their viability
depends on a `film-base` fix, not on anything decidable here. Candidate 2 additionally needs
a large +2.75 EV offset.

**Support — candidate 8** (Dmin-anchored, per-stock datasheet offset, `contrast = 0.745/Δ`).
The smallest residual offset (0.78 EV) of any black-passing shippable form, Dmax-free, and
content-free so it is default-eligible. Its residual is consistent with the **provisional**
datasheet constants: those rest on chart-read `D-min` values which PR #68 established are not
true Status M densities, and per-stock residuals against user preference were systematic
(Ektar ≈ +0.6, Portra 160 ≈ 0, Gold 200 ≈ −1.0), not random. The *form* is supported; the
constants need the proper spectral integration.

## The leader-uniformity check, redone per channel

The characterisation pass claimed per-channel percentiles but reduced each pixel to the mean
of its channels first, so a coloured fogging gradient with channels moving in *opposite*
directions would have cancelled and printed as uniform. That mattered because "the leader is
uniform, so its Dmax is an uncontrolled *level* rather than a gradient" is the finding
`film-base/dmax-anchor-reliability` rests on. Redone per channel (2026-08-03):

| roll | leader R / G / B tile median | largest \|L−R\| or \|T−B\| | B tile range vs R |
|---|---|---|---|
| Gold 200 | 1.2242 / 1.2340 / 1.3628 | 0.0076 | 0.0658 vs 0.0177 |
| Ektar | 1.2724 / 1.2865 / 1.3201 | 0.0089 | 0.0780 vs 0.0339 |
| Portra 160 | 1.4402 / 1.3297 / 1.3807 | **0.0478** (B, L−R) | 0.1396 vs 0.0400 |

**The conclusion survives — it was right but under-verified.** No channel-opposed gradient
exists anywhere in the set, so nothing was cancelling; every gradient is ≤0.009 except
Portra 160's blue. Two facts the scalar mean *was* hiding:

- **Blue is the odd channel in every leader** — its tile range runs 2–4× red's and its
  in-tile spread about 2×. Whatever a leader records, blue records it least uniformly.
- **Portra 160's leader is the least uniform of the three** (−0.048 blue left-to-right,
  ~3.5% of its Dmax) — and it is also the roll whose Dmax disagreed with its same-stock
  sibling. One roll is not a pattern, but it is the right place to look first.

## Reading the spread, and a gate I initially had backwards

`mid sd` measures how much a fixed anchor leaves varying between frames. **Low spread is not
a merit.** A reference-driven anchor applied to frames that genuinely differ in exposure
*should* leave spread; low spread means the anchor is *correcting* exposure — the frame-local
behaviour the default must not have. Compare only at comparable contrast, since low contrast
compresses between-frame differences too (candidate 1's small sd is that artifact).

## Limits, stated rather than implied

- **n = 10, randomly chosen, not selected for correct exposure.** Aggregate statistics
  (median headroom, per-stock residuals) are meaningful; per-frame conclusions are not.
- **Exposure labels are unreliable** — the user does not confidently recall which frames were
  correctly exposed, and a label inferred from "which render looks best" would be circular.
  Exposure-preservation is therefore reported as *consistent with*, never *verified*.
- **Per-frame exposure preference must not select an anchor.** Asking for a preferred EV frame
  by frame *is* frame optimisation. Only the central tendency is used: median +1.5 EV at
  contrast 2.0, i.e. white ≈0.45 density below Dmax, which independently agrees with the
  median measured diffuse-white gap of 0.417.
- **Δ rests on two frames.** Not enough to conclude from. The real fix is a **grey card in
  frame** (and a bracketed roll for exposure labels, and a calibrated transmission step wedge
  for absolute density) — all new-asset work under
  [`io/scanner-density-calibration`](../tasks/io/scanner-density-calibration.md).
- **Two frames are unresolvable by any single global curve.** G3 (sky best at +0, trees at +2)
  and P3 (window at +0, people at +1.5) exceed the SDR range. That is the HDR-output question,
  deliberately deferred until every candidate is renderable so it can be judged across all of
  them at once.

## What shipped (2026-08-03)

The task file mandates trying remedies in order — recalibrate defaults, then
reparameterize semantics, then change the equation. **The first two sufficed; the
equation is untouched.** §7.3's five lines are character-for-character what they were,
which is why no `pipeline_version` bump is owed here and why the two default curve
shapes remain comparable as the same family.

**Remedy 1 — recalibrated defaults.** `contrast 1.0 → 0.745/0.36 ≈ 2.0687` and
`shoulder 0.2 → 0.6`. Neither is a preference: the contrast is the manufacturers' own
mid-to-white aim delta expressed on the output axis (corroborated by film gamma 0.52
and system gamma 1.07), and the shoulder is the width whose bend begins at `D′ ≈ 0.70`,
essentially at mid-grey, where a print shoulder belongs. `toe` stayed at 0.2 — nothing
in the measurement asked it to move.

**Remedy 2 — reparameterized the anchor.** Recalibration alone could not fix the
defect, and this is the substantive change. `curve.dmax` used to be two things at once:
the roll's *reference* density and the density that renders to `1.0`. Splitting them
into `curve.dmax` (reference) + `curve.anchor` (which tone it places) is exactly the
"make the placement a declared control instead of an emergent value" option the task
listed, and it is what makes a photographic contrast usable — raising contrast pivots
the line about the pinned point, so pinning white necessarily darkens everything below
it. The default `{"mid-at-dmax-fraction": 0.5}` is candidate 3's form.

**Candidate 3 shipped as the default; candidate 8 did not, and could not.** 8 is the
better form — Dmin-anchored, so it does not inherit the leader's unreliability — but its
anchor needs per-stock datasheet offsets, i.e. the registry that
[`algo/film-stock-profiles`](../tasks/algo/film-stock-profiles.md) exists to build, and
those offsets currently rest on chart reads that are not true Status M densities. The
routing the user specified (declared stock → 8, no stock → 3, no roll Dmax → the fixed
fallback, per-frame always opt-in) therefore lands in two pieces: this task ships the
no-stock arm and the opt-in escape hatch, and the stock arm arrives with the registry.
`AnchorPlacement` is the seam it plugs into — a third variant, not a rewrite.

**What is deliberately still provisional.** `f = 0.5` is a measurement (α ≈ 0.48–0.57
across three stocks) whose numerator is one of those chart reads, and the fixed fallback
`NOMINAL_DMAX = 2.0` is not recalibrated: the measured rolls cluster near 1.36 once
Phoenix is excluded, but the user is adding samples and asked for that calculation to
wait. Mid-placement halves the cost of getting it wrong (`dA/dR = f`), which is why
leaving it is defensible rather than negligent. Both are parameter values, not forms —
a later change is a default change plus a conversion-version bump, which is the seam
`output/presets` and `conversion-versioning` already own.

**Retained as diagnostics, not deprecated.** `--sigmoid-white-at-d-max` is the old rule
kept reachable so the defect can be reproduced on demand, and candidate 7 (zero free
parameters, white at the *measured* diffuse white) remains the check on the datasheet
derivation. Both are content-driven or known-wrong for a default; neither is worth
deleting.
