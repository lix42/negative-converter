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

| # | Candidate | ship | mid median | \|EV\| to 0.18 | shadow median | clip |
|---|---|---|---|---|---|---|
| 1 | white@Dmax, c=1.0 — **shipped** | yes | 0.1634 | **0.14** | **72/255** | 0.00% |
| 2 | white@Dmax, c=2.0 | yes | 0.0267 | 2.75 | 12/255 | 0.00% |
| 3 | mid@0.5·Dmax, c=2.0 | yes | 0.0944 | 0.93 | 30/255 | 0.00% |
| 4 | white@measured | **no** | 0.3175 | 0.82 | 54/255 | 0.00% |
| 5 | black@0.00061, c=2.0 | yes | 0.0067 | 4.75 | 3/255 | 0.00% |
| 7 | white@measured, c=0.745/Δ | **no** | 0.3055 | 0.76 | 50/255 | 0.00% |
| 8 | mid@(Dmin+datasheet), c=0.745/Δ | yes | 0.1049 | **0.78** | 27/255 | 0.00% |

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

**Reject — candidate 5 (black-pinned at a fixed contrast).** Needs **+4.75 EV**. Pinning
black alone leaves white and mid unplaced, so at any fixed contrast everything else lands
arbitrarily. Making it work requires pinning a *second* point, which forces per-roll adaptive
contrast — already rejected, since contrast is film character worth preserving.

**Diagnostic only — candidates 4 and 7.** Their anchor is read from the frame's own content,
so an underexposed frame's lower diffuse white would be silently pulled up to 1.0, correcting
the very exposure the task requires preserving. That disqualification rests on the
**argument**, not on this data: both resolve on **2 frames only** (they need a valid white),
so their spread figures are uninformative and are not offered as evidence. Candidate 7
remains valuable as the zero-free-parameter check on the datasheet derivation.

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
