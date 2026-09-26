# Hanten — nf-look Progress Log

Execution log for the `nf-look` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status; this file is the
narrative beside it.

One `##` section per task in this epic, named by the bare task name. Read this
whole file before starting a task in this epic, and read other epics' `Epic
summary` sections when you depend on them. Append entries — don't rewrite earlier
ones.

## Epic summary

The creative stage the old chain never had: per-channel grade, path to white, contrast, look presets, and the stock data that survives `characteristic` leaving the decode.

The epic was created on 2026-09-19 with the new-flow plan (`docs/nf-migration.md`).
**`stage` closed 2026-09-23**: the stage, its empty `look` recipe section and its report
entry already existed from `nf-core`; what it settled is the spelling — **one key per
control under `look`**, each named and added by its own task with a field on
`LookParams`, a CLI flag (classified in `flow`, which must also refuse a new-flow-only
flag on the current chain), a `recipe::merge` arm with a merge test, and a value rule.
`contrast` and the grade overlap (equal pivoted exponents are contrast): contrast owns
neutral contrast, and the grade must not be able to move it. The section is `look::LookSection`, and
the stage's `LookParams` adds what the section cannot state (the decode's linearization, via
`Recipe::chain_params`). Two predicates, never a rule per knob: `is_empty` (moves no
pixel — `applied()`) and `asks_for_a_look` (neither the default nor empty — what
`film-master`'s refusal reads, in `nf-destinations/preset-set`); they differ because the
default look is not the identity, and an empty look is spared because it is one.

**`contrast` is done (2026-09-24): print contrast is `look.contrast`**, landed early by
`nf-reconstruction/gamma-split` — a power pivoted at mid-grey, applied first in the
stage. Highlight desaturation's band divides by the whole contrast (the decode's
linearization × this), so it is unchanged whichever stage carries a roll's contrast. It
stays **one knob** holding whatever makes the roll right: under
the rule `nf-calibration/anchor-comparison` chose (a bounded C) the solved per-roll value
*is* `look.contrast`, with the decode's anchor unchanged; taste is editing that number.
The default `2.0 / 1.8` is provisional; look presets must not set contrast; `--exposure`
is a level, not a contrast control (it moves the shadow slope only through reinhard's
mild curvature below mid). `anchor-comparison` has since chosen the per-roll rule
(2026-09-25; whole contrast 2.23–2.97 on nine rolls), which
`nf-calibration/roll-white-rule` implements. Measured
through the chain: shadow slope is the contrast (0.98–0.99× under reinhard); across
headrooms ≥ 2 stops it moves by under 0.003, and from 0 (fit range off) up by at most
~0.023 (≈2%, reinhard switching on). The grade runs after contrast and before
highlight desaturation, and its cast grows with contrast; its form (how it stays off
neutral contrast) is `per-channel-grade`'s.

**`per-channel-grade` is done (2026-09-25): `look.channel_grade` / `--channel-grade R,B`.**
A pivoted power on red and blue (green fixed at 1, since equal exponents would be a
hidden saturation knob), then the pixel's ACEScg luminance restored — so a neutral's
luminance slope is the contrast exactly and the grade is a colour operator. Runs between
contrast and highlight desaturation; exponent spread over `[r, 1, b]` under 1 keeps it
monotone; a pixel is graded whole or not at all — any channel ≤ 0 or non-finite (or a
luminance that is not finite and positive) passes the whole pixel through, since the
restore couples the channels. Desaturation's band is not adjusted for it. The balance flags now refuse under
`--new-flow` with it as the remedy.

**`path-to-white` is done (2026-09-24): highlight desaturation, on by default at 0.8.**
`look.highlight_desaturation` pulls near-neutral highlights to neutral, keyed on
brightness (one stop below diffuse white up to it) and on a band `0.015 → 0.025` over
`log10(max/min) / contrast` on ACEScg, luminance kept. It assumes the roll's white
balance (`hanten measure-roll`). On real frames the effect is modest and one-sided:
marked whites' chroma falls ~15% on average (visibly on bright clean-up cases), colours
hold.
**One spike is done and it settles what `path-to-white` ships**. `desaturation-spike`
([`docs/spike/highlight-desaturation.md`](../spike/highlight-desaturation.md)) chose a
**chroma pull** over a per-channel curve — not on appearance, which is equivalent
(ΔE ≈ 0.7), but because the curve moves luminance too and the fit range downstream eats
whatever compensates it. Its strength must key on **distance from the neutral axis as
well as brightness**, or a bright coloured surface is neutralised as hard as a bright
white one. It is a highlight operator and cannot reach cast below about L\* 70.

**`path-to-white` is built against a hand-set contrast (user decision 2026-09-22).**
Under the base-referenced anchor at contrast 2.0 the operator is **inert** — all three
measured rolls land 0.55–1.73 stops below white (09-11 corrected 2026-09-23) — so the
task is developed with a per-roll `--density-gamma` computed from that roll's base and
red p97 (candidate C/D in `docs/spike/white-placement.md`), and its band values are
provisional: `nf-calibration/anchor-comparison`
chose the rule on 2026-09-25, and the band wants re-fitting under it and a black point. Two tasks were split out of it to
run against today's binary, and both are done: `desaturation-band-fit` here (below) and
`nf-display-stages/gamut-map-share` (2026-09-23), which removes the double-up concern: at
the renders `path-to-white` is built under the gamut map moves no marked white. The
hand-set contrast must name `--display-tone reinhard`; left unstated it renders under
`shoulder`, where the map does real work. It now also waits on
`nf-scene-correction/roll-white-balance`.

**`stock-data-home` is done (2026-09-24): the stock data stays as evidence, the
inversion is split out to retire.** `src/film_stock/` holds the digitized tables and the
registry — the provenance of the fixed decode's constants, checked by tests that read the
tables forward — and becomes `#[cfg(test)]` once `nf-retire/characteristic` deletes
`src/algo/characteristic.rs`, the inversion. `--film-stock` leaves with that curve;
per-stock normalization is a planned, unscheduled look control that will bring its own
flag. The datasheets stay in the repo.

**`desaturation-band-fit` is done (2026-09-23)**
([`docs/spike/desaturation-band.md`](../spike/desaturation-band.md)): the band sits at
**`s0 = 0.025`, `s1 = 0.055` of `log10(max/min) / gamma` on film RGB** — the negative's
density spread, since linear RGB is rescaled by each roll's contrast. It is placeable
**only behind a roll-level white balance**: with none, Ektar's whites carry as much cast as
skin; a per-frame auto WB removes sunsets before the band sees them. The intent is to keep
the scene's light and remove only the roll-constant cast, measured from the roll's own top
percentile — never the base (it neutralises black) or the leader (1.5–3.5 stops over white,
gains the wrong way). Under that white balance the operator's extra cleaning was not
visible by eye; the band's value is keeping the pull off colour.

**`look-presets` is done (2026-09-25): there are no look presets.** Nothing in the look
is coupled the way the old bundles were, and a named look is a `--params` layer
(`core/recipe-composition`), so `--preset` retired with the `characteristic` curve
(`nf-retire/characteristic`) rather than being rebuilt. A new look control needs no
preset row; do not reuse the name.

## stage

**Status:** done
**Updated:** 2026-09-23

- 2026-09-19: created with the new-flow plan. Goal: the look stage.
- 2026-09-23: **closed as a decision task — most of it had already shipped.**
  `nf-core/stage-skeleton` built the stage (`pipeline::look`, `SceneReferredImage ->
  GradedImage`, order enforced by the types), `nf-core/recipe-schema` gave it an empty
  `look` section that refuses any key, and every new-flow report already lists
  `{"stage": "look", "applied": "identity"}` in `new_flow.stages`. Decisions, with the
  user:
  - **One key per control under `look`** (names left to each
    control's task), not one CDL-style object. CDL's slope restates white
    balance and its offset the flare subtraction, both scene correction's;
    path-to-white is not CDL-shaped; and one object holding knobs of different
    lifetimes is the trap `reconstruction.curve` already shipped. The grade's inner
    form stays `per-channel-grade`'s question.
  - **The empty look reports `"identity"`**, the vocabulary every stage uses.
  - **The `film-master` refusal is not built here**: the new flow has one destination
    and `LookParams` has no field, so the rule would be unreachable dead code. Its
    shape is recorded — one predicate on `LookParams` ("non-empty"), one rule keyed on
    whether the destination runs a look — and its verification moved to
    `nf-destinations/preset-set`. The first control to land adds the predicate.
  Code: `look.rs` rustdoc only. The identity guarantee is
  `chain::tests::the_first_three_stages_are_a_bit_exact_identity`; note that its
  input is built through `simple` → the NC film RGB v1 3×3, which mixes channels,
  so an awkward value sharing a pixel with a NaN or infinity reaches the look as NaN.
  The first look control should place its test values *after* the matrix.
- 2026-09-23: review follow-ups. A stage-local identity test was dropped as a
  duplicate of the chain's, sharing its construction flaw. The `film-master`
  obligation is now written into all three control task files (and conditionally
  into `preset-set`, which may land first); the grade/contrast overlap — equal
  pivoted exponents *are* contrast — is an open question on both tasks.

## per-channel-grade

**Status:** done
**Updated:** 2026-09-25

- 2026-09-19: created with the new-flow plan. Goal: a per-channel grade with a mid-grey pivot.
- 2026-09-24: decisions from `nf-look/contrast` recorded in the task file: the grade runs
  after contrast and before highlight desaturation, its cast grows with contrast, and its
  form (how it stays off neutral contrast, which contrast owns) is still this task's.
  Also opened: whether desaturation's band should account for the grade.
- 2026-09-24: **started; form decided (user, on a plan).** Pivoted per-channel power
  then a luminance restore, spelled `look.channel_grade = [r, b]` with green at 1; a
  channel ≤ 0 or non-finite passes the power, the restore is skipped on a non-positive
  luminance; exponents positive with spread under 1 (monotone); desaturation's band
  unchanged. The measurement that ruled out the restore-free form (neutral slope 0.95–1.05
  at a 0.4 exponent spread) and the reasons are in the task file's Decisions.
- 2026-09-24: **built.** `look.channel_grade` / `--channel-grade R,B` (key named
  `channel_grade` because `pipeline::chain` already calls the whole look "grading").
  Plumbing as for contrast: `LookOverrides`, `recipe::merge` arm, `validate_look` rule
  (flag-and-key and key-only wording), `flow`'s new-flow-only row, `applied` joining the
  controls that ran. Golden: three pixels, window enumerated over both `powf` results
  (they enter one restore, so a one-argument `reachable_window` does not cover it).
  **Synthetic check through the binary** (`nctool metrics image --space display-p3`): a
  neutral wedge with red/blue scale off +8%/−6% about the anchor's mid (D′ = 0.62), so
  the cast is a pure crossover — mean chroma 4.55 → 1.47 at `0.95,1.05`, worst band
  7.07 → 2.51; `0.92,1.07` 3.71 and `0.9,1.1` 4.75 bracket it. A first fixture with the
  error applied from the base carried a level cast at mid too, which no pivoted grade can
  remove — that is white balance's, the same division of labour the task file states.
  **Gotchas:** at unit exponents the restore's ratio is exactly 1 (both luminances come
  from the same arithmetic), so the identity holds even without the skip; the balance
  refusals were `NotYet` naming this task and are now `Never` with `--channel-grade` as
  the remedy — two integration tests used `--shadow-balance` as their "not yet" example
  and now use `--preset`.
- 2026-09-24: **review round.** Both review engines found a gamut-edge defect, latent
  today (the fixed decode emits `10^x > 0` and the v1 3×3 and scene-correction gains are
  positive, but the planned flare subtraction will produce negatives): passing a
  non-positive channel through the power while it still entered the restore's
  luminance let the powered luminance approach zero — `[0.05, −0.011346323, 0]` at
  `[1.45, 0.55]` came out near `[3.6e5, −1.5e5, 0]` — flip the output's luminance sign,
  or jump across a luminance of zero; a finite pixel whose power overflowed came out
  infinite. **Guard changed to whole-pixel pass-through** (a pixel is graded only when
  all three channels and both luminances are finite and positive, and the result
  finite; otherwise it is untouched, bit for bit). On an all-positive pixel the restore
  is a weighted mean of per-channel ratios, so it is bounded, and since exposure never
  flips a channel's sign a pixel is graded along its whole exposure ray or not at all —
  monotone for every pixel. This departs from contrast's per-channel pass-through on
  purpose. Tests: every repro passes through bitwise; a sign/magnitude grid near zero
  stays finite, keeps luminance on graded pixels and bounds each channel by `Y / w_c`;
  mutation-checked against the old guard. The golden's pixels are all positive, so it
  did not move. Doc fixes: the balance refusal now speaks of the regional balance as a
  whole (it is printed for the range flags too); design-spec §9 gains `channel_grade`
  in the v2 example and the `look` list; `docs/using-nc.md`'s refusal example and
  refused-knobs table name `--channel-grade`; design-update notes the as-built form and
  marks the look's spelling settled; the task file's decisions are prose.
- 2026-09-24: **review round 2.** The guard's result-finite branch had no test; a pixel
  near `f32::MAX` (`[5e37, 3.3e38, 1]` at `[0.5, 1.0]`: finite luminances, ratio ≈ 1.06,
  green overflows) now covers it, mutation-checked. The first candidate,
  `[3e38, 3e38, 3e38]`, never reaches the branch: `3e38 / MID_GREY` overflows, so the
  powered luminance is already infinite and the luminance check catches it. The guard's statement in the
  field doc and design-spec §9 now includes "the result is finite". Recorded the
  whole-pixel guard's trade-off: continuous along exposure, not across colour — a
  channel at +ε is graded and its neighbour at −ε is not, so noisy deep shadows holding
  negatives would read as salt and pepper (at −6 stops a red exponent of 1.1 moves red
  about 34%). Latent until `nf-scene-correction/flare-removal`, the first producer of
  negatives, which must revisit it; its task file now says so.
- 2026-09-25: **done.** Landed as `look.channel_grade` / `--channel-grade R,B`: a pivoted
  power on red and blue with green at 1, then the ACEScg luminance restore, graded whole
  or not at all (every channel, both luminances and the result finite and positive).
  Verified by unit tests, a two-`powf`-window golden, binary tests and the synthetic
  crossover read back with `nctool metrics`; reviewed by two engines plus the ship
  review (Codex and `ship:diff-reviewer` clean on the final tree). For dependents:
  `nf-retire/regional-balance` can now remove the balances from the current chain — the
  new flow already refuses them naming the grade; `nf-look/look-presets` decides whether a
  preset sets the grade; `nf-scene-correction/
  flare-removal` must revisit the whole-pixel guard once it produces negative channels.

## desaturation-spike

**Status:** done
**Updated:** 2026-09-21

- 2026-09-21: filed once the three-way measurements had settled the structural half —
  per-channel compression against a common ceiling is what converges channels, no nc
  display tone can do it, and the operator belongs pre-branch at diffuse white. What is
  left is the **form**: a per-channel curve (what film, paper and all three converters
  do — desaturates *and* shifts hue) against a hue-preserving chroma pull (what the
  design's prose describes). Nothing has compared them, and they differ most on exactly
  the content the user cares about, saturated highlights.
- 2026-09-21: **rendered and measured; both families work and the difference between
  them is smaller than the difference between strengths.** Set in `../temp/desat-spike/`
  (8 frames x 8 configs, gain maps stripped so every cell is plain SDR). Frames chosen by
  measurement, not by eye: highlight chroma above L\* 90, median across the three
  outside producers, split into **saturated** (1820, 1799, 1793, 1796 — C\* 11.8–15.2)
  and **neutral** (1789, 1811, 1810 — C\* 1.8–2.2).

  White is pinned to **each frame's own** p97 for this set. Under the base-referenced
  anchor nothing reaches the operator's range at all (`nf-reconstruction/anchor-spike`),
  and a per-frame anchor is the wrong rule but the right control here — the question is
  the operator's form, so the anchor is held in a state where both families can act.

  Matched on how much neutral-highlight chroma each removes:

  | config | neutral C\* | removed | saturated C\* | removed | hue shift |
  |---|---|---|---|---|---|
  | control | 14.8 | 0% | 19.9 | 0% | 0.0° |
  | perch-30 | 6.9 | 53% | 11.7 | **41%** | 5.0° |
  | perch-60 | 7.0 | 53% | 10.8 | 46% | 4.6° |
  | hue-35 | 7.4 | 50% | 10.7 | 46% | **4.2°** |
  | hue-06 | 3.0 | 80% | 5.4 | 73% | 5.6° |

  **(1) At matched strength the two families are close.** `perch-30` against `hue-35`:
  the per-channel curve keeps 5 points more saturated chroma for the same neutral
  cleanup, the chroma pull rotates hue 0.8° less. Both differences are small beside what
  a strength change does, so **strength is the parameter that matters and family is a
  second-order choice** — the opposite of what the task was filed expecting.

  **(2) The per-channel curve is the more selective of the two**, which was not the
  expectation. It removes 53% of neutral-highlight chroma against 41% of saturated,
  where the chroma pull is near-uniform (50% / 46%). If that holds up it argues for the
  per-channel family on the sunset question, not against it.

  **(3) The "hue-preserving" half is only approximately so.** Lerping toward `(Y, Y, Y)`
  in linear ACEScg holds luminance exactly (measured max |ΔY| = 3e-3, quantisation) but
  is **not** a constant-hue path in CIELAB — it still rotates 4.2°. A genuinely
  hue-constant operator needs a perceptual construction, which is a finding about the
  implementation rather than about the families.

  **(4) `--display-tone shoulder` drives top-end chroma to exactly 0.0** on every frame,
  with 17–29° of hue rotation. That is flattening, not shaping — it plateaus above its
  knee and the gamut map then has no room for chroma at all. Independent support for the
  design retiring it, and the reason it failed as a gamut-map separator: it does not
  isolate the gamut map's share, it destroys the highlights.

  **Not settled: the gamut map's share.** Nothing reachable by flag turns the gamut map
  off, so `perch` against `control` measures shoulder-plus-gamut-map jointly. Separating
  them needs either a throwaway patch or a measurement of how many top-end pixels are
  out of P3 before mapping. Carried forward.

- 2026-09-21: **the user's verdict closed it, and corrected the experiment on the way.**
  Comparing #2 (per-channel, shoulder 0.3) against #5 (chroma pull 0.35): *"I can tell the
  bright change, but I cannot tell the color change."*

  That was a flaw in the set and a finding at once. The two arms were matched on **chroma
  removal** and left unmatched on **brightness**, because the per-channel curve compresses
  luminance as well as converging chroma while the pull holds luminance by construction:

  | config | L\* top | vs control | C\* top | removed |
  |---|---|---|---|---|
  | control | 80.6 | +0.0 | 17.0 | 0% |
  | per-channel 0.3 | 76.6 | **−4.0** | 9.0 | 47% |
  | chroma pull 0.35 | 80.6 | **+0.0** | 8.8 | 48% |

  **Matching the brightness needed `--print-exposure 0.38` and two attempts** — the first
  at 0.19 recovered only half, because the exposure is applied *before* the fit range and
  reinhard re-compresses the lift. With it matched:

  | | L\* top | C\* top | hue shift |
  |---|---|---|---|
  | per-channel, matched | 80.4 | 9.5 | 5.1° |
  | chroma pull | 80.6 | 8.8 | 4.3° |

  **ΔL\* 0.16, ΔC\* 0.66, Δhue 0.7° — a total ΔE of about 0.7, below visibility.** At
  matched brightness the two families produce the same picture, which is what the eye
  reported before the measurement caught up.

  **So the form question is closed, and not on appearance.** What separates the families
  is that the per-channel curve **entangles tone with chroma** and the pull does not —
  and the entanglement cannot be undone by a scalar, since whatever exposure compensates
  it is partly eaten by the fit range downstream. In a staged chain where fit range
  already owns luminance, an operator that also moves luminance double-compresses and
  then needs a correction that does not fully land.

  **Recommendation for [path to white](../tasks/nf-look/path-to-white.md): the chroma
  pull**, chosen for **separability, not for looks**. Its remaining open question is no
  longer "which family" but what "approaches white" is measured on, and at what strength.
- 2026-09-21: **the user marked 12 patches on 7 frames, and they found the case the
  aggregate hid.** Measured per patch rather than over the top 3%
  (`../temp/desat-spike/patch-measurements.json`).

  **(1) The operator's reach is exactly its threshold, and two marked surfaces are
  outside it.** The chroma pull starts at linear luminance 0.5, about L\* 76, and how
  much a patch moves scales with how far above that it sits — 96% at L\* 86.9, 44% at
  80.5, 25% at 74.9, and **0% at 67.9 and 63.9**. The user's "fog" (L\* 63.9, C\* 10.4)
  and one "cloud" (L\* 67.9, C\* 5.9) are untouched by **every** configuration.

  That is not a defect — it is the boundary of what a *highlight* operator can do. Cast
  on a surface at L\* 64 belongs to [the per-channel
  grade](../tasks/nf-look/per-channel-grade.md) or to the decode's `scale`, and this
  operator will never reach it. It also means **judging "are the whites clean" on a frame
  whose white sits at L\* 64 measures the decode, not the operator** — the same
  measure-the-right-thing trap as the white-patch bias in Part 3.

  **(2) The families diverge on exactly one patch, and it is the sunset case.** Matched
  on average cleanup, 11 of 12 patches agree within C\* 1.4. The twelfth is **1820 "sand
  beach"** — the brightest and most saturated of them, L\* 86.9, control C\* 27.5:

  | | C\* kept |
  |---|---|
  | per-channel, brightness-matched | **7.6** |
  | chroma pull | **1.1** |

  The pull neutralises a bright warm surface almost completely; the per-channel curve
  keeps two-thirds more of it. **This corrects the earlier "indistinguishable" reading**,
  which averaged over the top 3% where such surfaces are rare — one marked patch was
  worth more than eight frames of aggregate.

  The mechanism is that the pull's strength keys on **brightness alone**, so a bright
  *coloured* surface is neutralised as hard as a bright *white* one. That is the sunset
  problem, reproduced on a real patch, and it sharpens the open question: keying on
  distance from the neutral axis as well as on luminance would protect the sand beach
  while still cleaning the cloth. **What "approaches white" is measured on is now the
  live design question, not the family.**
- 2026-09-21: **the saturation guard works, and is strictly better than either family
  on this evidence.** Added a third and fourth parameter to the throwaway operator: a
  band over linear-RGB saturation `(max−min)/max`, full pull below `s0`, off above `s1`,
  linear between. Placed at **0.30 → 0.45** from the patches themselves — the
  whites-with-cast run 0.137–0.279 and the sand beach sits at **0.463**, 1.7× the next
  highest, so a band rather than a single knee is what separates them.

  | | whites with cast | genuinely coloured |
  |---|---|---|
  | control | 12.9 | 27.5 |
  | per-channel, brightness-matched | 5.7 | 7.6 |
  | chroma pull, unguarded | **6.1** | **1.1** |
  | **chroma pull + guard** | **6.1** | **22.3** |

  Identical cleanup on the whites (6.1 either way, and every other patch unchanged to
  0.1), and the sand beach keeps **22.3 against 1.1**. The per-channel curve's 7.6 was
  the best of the two families; the guard beats it threefold without giving up any
  cleaning.

  **So the answer to "what does approaching white get measured on" is: not luminance
  alone.** Keying strength on brightness *and* on how far the pixel already is from the
  neutral axis is what lets one operator clean a cast without flattening a sunset —
  which was the objection that made "off must stay available" feel mandatory. It may
  still be wanted, but for less.

  **Caveat, and it is not small.** The band was placed from 12 patches on one roll, and
  the sand beach is the only one above it. The *mechanism* is demonstrated; the
  *parameters* are fitted to a single example and should not be carried into
  `path-to-white` as values. What carries is the shape: two thresholds, on saturation,
  multiplying the brightness term.
- 2026-09-21: written up as [`docs/spike/highlight-desaturation.md`](../spike/highlight-desaturation.md); the entries above are the execution trail, the report is the result.
- 2026-09-21: **confirmed by eye — "I can see the diff between 3 and 4. 4 keeps the
  color."** The guard is visible, not just measurable, on 1820's sand beach at the
  strength that cleans the whites. Spike **done**; the throwaway operator is reverted and
  never merged.

  **Verdict for [path to white](../tasks/nf-look/path-to-white.md):**

  1. **A chroma pull, not a per-channel curve** — chosen for **separability**, since the
     per-channel curve moves luminance too and the fit range downstream eats whatever
     compensates it. The two are perceptually equivalent otherwise (ΔE ≈ 0.7 matched).
  2. **Strength keys on brightness *and* on distance from the neutral axis.** Brightness
     alone neutralises a bright coloured surface as hard as a bright white one. This is
     the spike's main result and it was invisible to every aggregate — one marked patch
     found it.
  3. **It is a highlight operator and cannot be more.** Its reach is its threshold:
     surfaces at L\* 64–68 are untouched at any setting, so midtone cast belongs to the
     per-channel grade or the decode. The user confirmed those read fine, consistent with
     the eye being less sensitive to cast in shade.
  4. **"Off" is still wanted but for less** than the design assumed — the guard removes
     the flatten-a-sunset objection rather than the hide-a-cast one.

## path-to-white

**Status:** done
**Updated:** 2026-09-25

- 2026-09-19: created with the new-flow plan. Goal: highlight desaturation.
- 2026-09-19: the gating spike moved to `nf-calibration/scale-ladder` and was
  redefined. The evidence behind this task's premise — that the knee'd sigmoid's
  clean whites come from its per-channel shoulder — does not separate the shoulder
  from the anchor (0.28 vs 0.5 mid-fraction) or the display tone (`none` vs
  reinhard), which the two presets also differ in. Whether any `density.scale`
  reaches those whites now decides if this task is load-bearing or an optional look.
- 2026-09-22: **decided how this task gets built, before it can start.** The anchor
  spike's number changes the premise: under the base-referenced `mid-at-base-offset`
  anchor at contrast 2.0, all three measured rolls land **0.55–1.28 stops below white**
  ([`docs/spike/white-placement.md`](../spike/white-placement.md)), so this operator would
  be inert in a shipped render whatever form it takes. The lift comes from letting the
  roll's own content drive **contrast** rather than moving a level — candidates **C**
  (pin mid and solve `gamma = MID_GREY_OUTPUT_DECADES / (W - d)`; 4.15 / 2.57 / 3.11 on
  the three rolls) and **D** (C with a gamma ceiling, so the noise budget is explicit
  rather than accidental).

  That rule belongs to `nf-reconstruction/anchor-rule`, which is expected to land
  **after** this task. **User decision (option a):** build and verify this task against a
  **hand-set per-roll contrast** — flags only, no new code — and treat the band's values
  as provisional, re-fitted once the anchor rule is chosen. The control is
  `--density-curve exponential --anchor-mid-offset <d> --density-gamma <g>` with `g`
  computed per roll from that roll's measured base and its red p97.

  Two things this changes about the tuning, both recorded on the task file:

  **(1) Do not carry the spike's band values over.** The spike pinned white per frame at
  p97, which is a *level* move; a contrast move steepens everything below white too, so a
  different population of pixels lands in the operator's range. Re-placing the band under
  the wrong white is the same work twice.

  **(2) The threshold must become scene-referred.** The throwaway operator started at
  *rendered* linear luminance 0.5 (~L\* 76), i.e. after fit range, while the design
  requires the trigger at diffuse white, pre-branch. Once the decode pins mid — and on a
  straight line a mid anchor and a white anchor are one rule — diffuse white is a known
  value at the decode's output, so the threshold is expressed against that. The
  operator's anchor *is* the decode's anchor.

  Also noted: `nf-calibration/anchor-comparison` depends on this task while this task
  needs an anchor that reaches white. The task graph stays acyclic and nothing records
  that second direction as an edge — the coupling is the hand-set contrast.
- 2026-09-22: two pieces split out so they can run now, against today's binary, instead
  of waiting three tasks for the look stage: `nf-look/desaturation-band-fit` (the band's
  numbers, fitted to one patch on one roll) and `nf-display-stages/gamut-map-share` (how
  much of the convergence is the gamut map already). Both are now dependencies.
- 2026-09-23: **two inputs changed.** (1) The 2026-09-22 entry's "0.55–1.28 stops below
  white" and 09-11's gamma 3.11 carried a calibration frame; corrected they are
  **0.55–1.73** and **6.61** — see the correction in
  [`white-placement.md`](../spike/white-placement.md). (2) `desaturation-band-fit` is done:
  band `s0`/`s1` = 0.025/0.055 on `log10(max/min)/gamma`, placeable only behind a roll-level
  white balance, now filed as `nf-scene-correction/roll-white-balance` and added to this
  task's dependencies. Its band edges miss by a little on both halves of the pair check
  (placed from patch medians, applied per pixel) — added to the task's open questions.
- 2026-09-24: **started, on the new chain now that fit range (#153) renders a viewable
  picture.** Decisions (user): the knob is `look.highlight_desaturation`; the saturation
  measure is taken on **ACEScg**, not film RGB (no inverse matrix needed — on the band
  fit's 29 patches it separates as well, 3 misclassified against 4, at ~0.72x the scale);
  whether it ships on by default is decided after rendering it.

  **Placement, per pixel** (`../temp/ptw/scripts/place.py`, the band fit's cached ACEScg
  pixels at candidate-C contrast times the roll gains, the operator simulated on every
  pixel of every in-range patch). The ambiguous zone is narrow — `s` ≈ 0.024–0.026 holds
  two whites (1774 cloud, 1800 truck) and two colours (1815 sunset cloud, 1727 pastel
  cloth) — so the band's position is a choice, and the user's intent (keep the sunset,
  `s0` low) makes it: **`0.015 → 0.025`**, where colours keep 92–100% of their chroma and
  whites at or below 0.016 lose 70–100%. `0.020 → 0.030` cleaned more but left the
  sunset cloud at 57%. A smoothstep band ramp differed from a linear one by 0.001, so it
  stays linear. Start stays one stop below diffuse white.

  **Built:** the operator in `pipeline::look` (`rgb ← rgb + strength · b · w · (Y − rgb)`,
  luminance kept, band tested on max/min ratios so only a pixel inside a ramp calls a
  libm), `LookSection` as the recipe section with the `is_empty` predicate the first
  control owed, `LookParams` adding the decode's contrast via `Recipe::chain_params`,
  flags `--highlight-desaturation{,-start,-band}` (new-flow only, refused on the current
  chain), `new_flow.look` in the report. Golden: one pixel per path (full pull —
  bit-exact; band ramp — `log10` window; brightness ramp — `log2` window; colour —
  untouched), mutation-checked on both ramps.

  **Pair check through the binary** (`../temp/ptw/`: per roll a recipe with candidate-C
  contrast and `measure-roll` gains, fit range at its default; 21 frames × strength
  0/0.5/0.8/1.0). Mean patch C\* — whites 7.08 → 6.50 → 6.01 → 5.83, colours 17.31 →
  17.19 → 17.11 → 17.10; luminance within 0.2 L\*. Colours hold (the sunset cloud 11.0 →
  9.6 is the largest move); the best-cleaned whites are the 1810 cloud 9.1 → 5.4 and the
  birds ~8 → ~5.5. Smaller than the simulation because most marked whites render a little
  below diffuse white, where the brightness ramp is only partly in.

  The sigmoid's retirement (#151) removes the knee'd render the Design asked to re-check
  the band under, so that caution is closed rather than open.
- 2026-09-24: **done — on by default at strength 0.8** (user decision after the review
  set: "I cannot see the difference on most of the photos, but I do the diff at 1810 and
  1735, and I like when it's on"). With a default that is not the identity, "the look is
  empty" and "the look is the default" became two questions: `LookSection::is_empty`
  (what `applied()` reports) and `is_default` (what a no-look destination reads to refuse
  a look the user set — keying `film-master`'s refusal on emptiness would refuse every
  default recipe). `nf-destinations/preset-set`, `contrast` and `per-channel-grade` now
  say so. Still open for later: exact hue preservation, anything per branch above
  diffuse white, and whether the minimal "direct" preset keeps it (`look-presets`).
- 2026-09-24: **review correction — the refusal predicate is `asks_for_a_look`, not
  `is_default`.** Keying `film-master`'s refusal on "not the default" would refuse
  `--highlight-desaturation 0`, an identity that renders exactly what `film-master` does,
  and with it the flags-win reset. The acceptance set is the default plus every empty
  look, so `LookSection::is_default` became `asks_for_a_look` (`!is_empty() && != default`);
  the three task files above and `nf-look/stage`'s verify line now say so.
- 2026-09-25: cross-reference from [`nf-calibration/anchor-comparison`](../tasks/nf-calibration/anchor-comparison.md), which chose the white rule this task's
  band was provisional against (roll white = brightest frame under a +2.0 cap, floor
  +1.5; whole contrast 2.23–2.97), and found the chain needs a black point. **The band
  wants re-fitting under that rule and a black point.** Its verification measured marked
  whites' C\* rising in proportion to the contrast (09-18: 3.8 at gamma 2.0, 5.5 under the
  rule, 7.8 at 4.15), because contrast is a per-channel power that multiplies residual
  cast, and the operator at `0.015 → 0.025` does not take it back.

## contrast

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: the print-contrast knob.
- 2026-09-24: **the knob landed early, with `nf-reconstruction/gamma-split`.**
  `look.contrast` / `--contrast` exists: a per-channel power on ACEScg pivoted at
  mid-grey, default `2.0 / 1.8`, running before highlight desaturation (whose band now
  divides by linearization × contrast). The task file is rewritten to what remains —
  the default and the overlap with the grade. Contrast landed first, so it owns neutral
  contrast. Details in `docs/progress/nf-reconstruction.md`, `gamma-split`.
- 2026-09-24: **the default question is settled as a set of decisions (user).**
  `look.contrast` stays **one knob** holding whatever makes the roll right — no
  `k_roll × k_taste` split, since `linearization × contrast` is already one product a
  reader must carry. Under `anchor-comparison`'s candidate C (or D below its cap) the
  solved per-roll value *is* `look.contrast`, and the decode's anchor does not move:
  the decode still sends `d` to 0.18 and the look's power about 0.18 lifts the roll's
  white to 1.0 (the spike's "anchor 0.800" for C is the whole chain's white in the
  pre-split framing, not a decode setting). Taste is editing that number per roll.
  `--exposure` is **not** the taste substitute: it is a level move, whose effect on the
  shadow slope comes only from reinhard's mild curvature below mid (a few percent,
  growing as exposure lifts shadows toward mid); contrast sets the slope 1:1. The
  default stays `2.0 / 1.8` as a provisional value this task does not move. Look
  presets must not set contrast (recorded in `look-presets`). `W`'s percentile, and whether it is a
  constant or a recipe value, is out of scope here; `anchor-comparison` owns it.
  Remaining: the shadow-separation measurement and the grade overlap.
- 2026-09-24: **done.** Shadow separation measured on a synthetic neutral ramp through
  `chain::render` (`chain::tests::contrast_not_fit_range_decides_shadow_separation`):
  mid-grey renders at 0.18 in every cell, so lightness is matched for free. The log-log
  slope from 0.005 to 0.05 is the contrast exactly with fit range off, and 0.98–0.99×
  it at headrooms 2/3/6 (0.977 / 1.091 / 1.487 at contrast 1 / 1.11 / 1.5, six stops);
  across headrooms ≥ 2 stops it moves by under 0.003 (from 0 up, at most ~0.023 at
  contrast 1: reinhard switching on). Reinhard being on at all costs 1–3% of slope, as a
  near-constant shadow gain. Real frames skipped: both operators are per-pixel, so a
  frame's neutrals show the same function. Grade overlap (user): the grade's form is
  `per-channel-grade`'s; it runs after contrast and before highlight desaturation, and
  its cast grows with contrast (a decode crossover is an exponent mismatch the contrast
  multiplies too) — recorded in that task file. `chain.rs`'s two doc lines that still
  described the look as highlight desaturation alone now name contrast.

## look-presets

**Status:** done
**Updated:** 2026-09-25

- 2026-09-19: created with the new-flow plan. Goal: re-express the `--preset` bundles.
- 2026-09-24: recorded in the task file from `nf-look/contrast`: a preset does not set
  `look.contrast`.
- 2026-09-25: **done — retired, not rebuilt** (user decision). A look-only preset would
  bundle one or two independent knobs (`look.contrast` is the roll's and a preset may not
  set it; `look.channel_grade` corrects the roll's crossover), and layered `--params`
  already names a look without code. `nf-retire/characteristic` removes the flag with the
  last three names: a hidden migration error at every value on both chains, no
  `conversion_preset` report block, precedence `defaults < params < flags`.

## stock-data-home

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: a home for the film-stock data.
- 2026-09-24: **decided per piece and split for the retirement** (with the user).
  - **Tables, `curves.json`, the digitizer and the registry (`FilmStock`, `curves_for`)
    stay**, moved to `src/film_stock/`. Consumer today: the `characteristic` curve.
    Consumer after it: the evidence for the fixed decode's constants —
    `the_fixed_decode_mid_is_the_generic_aim` (`fixed::MID_ABOVE_BASE`),
    `generic_sits_inside_the_measured_spread`, `blue_is_steeper_than_red_on_every_stock`,
    `aim_table_agrees_with_the_curve` — so the module becomes `#[cfg(test)]` then, until
    per-stock normalization gives it a runtime reader again. `FilmStock` moved beside the
    data; `types` re-exports it while it is recipe vocabulary.
  - **The inversion is the retiring part**, split whole into `src/algo/characteristic.rs`
    (`invert`, `apply_curve`, `OutOfTable`, `check_tables`, `aim_red_scale` and their
    tests), so `nf-retire/characteristic` deletes one file. Two evidence tests read the
    tables through `invert`; they now bracket the aim on the forward curve instead (the
    aim lies between the densities at `0.18 ± tol`), which is the same condition because
    the curve is increasing. Mutation-checked: moving an aim, the generic's mid or
    `MID_ABOVE_BASE` reds the matching test. A new `every_table_rises_strictly_from_the_base`
    states the two table invariants without `check_tables`, which retires.
  - **`--film-stock` leaves with the curve** rather than lingering as provenance —
    `--film-type` already records chemistry, and a key that gates nothing is the dead API
    the waiver rule is about. Per-stock normalization, when it is built, brings its own
    flag. The `--new-flow` refusal no longer names this task as the one bringing it.
  - **The datasheet PDFs stay in the repo** (7.8 MB): the README's rule is that the file,
    not a URL, is the provenance, and the assets folder is neither reachable from CI nor
    meant for third-party documents.
  - No pixel moved: 21 tests before and after (20 moved, 1 new), the generated
    `curves.rs` is byte-identical to the emitter's output from the moved `curves.json`.
- 2026-09-24: review follow-ups after rebasing onto #157/#158. The forward-bracket
  rewrite of the mid-grey test was **not** the same condition at a table's edge:
  `density_at` extrapolates, so it now also asserts the bracket lies inside the table,
  which the old test's `in_table` flag did. The table-invariant test gained
  `check_tables`' 8-point floor so that survives the retirement too. Stale pointers fixed
  in `design-spec.md`'s module tree, `stages.rs` and `display_tone.rs`; the module doc
  now quotes design-update's red gamma (0.53–0.61). Not changed: the `--film-stock` row
  stays `NotYet` — the capability is planned, only the flag goes.

## scene-range-mapping

**Status:** not started
**Updated:** 2026-09-25

- 2026-09-19: created with the new-flow plan. Goal: spike: opt-in bounded scene-range mapping.
- 2026-09-25: from [`nf-calibration/anchor-comparison`](../tasks/nf-calibration/anchor-comparison.md): **a reviewed noise budget for a lone dark frame.** Treating
  single underexposed frames as a roll of one, the user preferred the white floored at
  +1.5 scene stops above mid-grey (whole contrast ≤ 2.97) over +1.0 (4.45, worst) and +2.0.
  Delivered noise there was 1.75× the scan's floor on 09-11, against 5.4× at the contrast an
  unbounded solve asks for. That is the number "bounded" needs, measured rather than
  picked.

## desaturation-band-refit

**Status:** not started
**Updated:** 2026-09-25

- 2026-09-25: filed from `nf-calibration/anchor-comparison`'s review, where nothing owned
  `path-to-white`'s re-fit. Goal: re-place the band under the chosen white rule and a black
  point.

## desaturation-band-fit

**Status:** done
**Updated:** 2026-09-23

- 2026-09-22: filed. Goal: place the saturation band from a distribution of marked
  patches rather than from the single "sand beach" example on `2026-09-18-Gold200`. The
  spike itself says the shape carries and the parameters do not, and names more marked
  saturated patches on more rolls as the cheapest way to advance it. Runs against today's
  binary — throwaway operator, anchor set by flag, patches marked in the review app — so
  it does not wait for the look stage. Measure under the hand-set candidate C/D contrast
  `path-to-white` will use, not the spike's per-frame p97 white.
- 2026-09-23: started. Workspace `../temp/band-fit/` (uncommitted — the user's photographs).
  **Four rolls, leave-one-roll-out** rather than one fixed hold-out: fit on three, test on
  the fourth, rotate — so every roll is tested once, and holding out a Portra400 (two
  rolls of one stock) tests roll-to-roll variation while holding out Gold200 or Ektar tests
  a different stock. Brightness start held at **1 stop below diffuse white** throughout
  the fit (it decides which patches are in range); revisit afterwards.

  Control is candidate C by flags: `--density-curve exponential --anchor-mid-offset 0.62
  --density-gamma g`, `W` = p90 over frames of per-frame red p97 (the anchor spike's
  definition, reproduced to three decimals on its three rolls), `g = 0.7447 / (W − 0.62)`:

  | roll | base (`estimate --grid`) | W | gamma |
  |---|---|---|---|
  | 2026-09-18-Gold200 | 0.4710, 0.2324, 0.1080 | 0.800 | 4.15 |
  | 2026-09-14-Ektar100 | 0.3529, 0.1948, 0.1266 | 0.910 | 2.57 |
  | 2026-09-11-Portra400 | 0.3469, 0.1626, 0.0934 | 0.860 | 3.11 |
  | 2026-09-20-Portra400 | 0.4210, 0.1993, 0.1097 | 0.941 | **2.32** (new) |

  The binary confirms `anchor_value` = W (0.7996 on Gold200). **The per-roll gamma spans
  1.8x, which bears on the choice of measure**: a fixed cast of `Δd` density renders at
  linear-RGB saturation `1 − 10^(−γ·Δd)`, so the same slightly warm white reads more
  saturated on Gold200 than on 09-20. Alongside linear-RGB and a perceptual measure, the
  fit therefore tries the log ratio divided by gamma — the negative's own chroma.

  The fit itself needs no operator: `s0`/`s1` are placed from each patch's colour at the
  operator's *input*, which a `film-master` render under the same flags gives directly.
  The throwaway operator is needed only for the pair check, and is kept as a `.patch` in
  the workspace this time (the spike's was reverted and is in no branch).

- 2026-09-23: **the anchor spike's 09-11 numbers include a non-picture frame.** Its
  Portra400 roll was measured over 12 frames, but the manifest gives that roll 11 of role
  `real` — the twelfth is `calibration.tif`, whose red p97 is **1.524** against 0.45–0.87
  for every picture. Dropping it moves `W` from **0.860 to 0.733**, and candidate C's
  gamma from 3.11 to **6.61** — twice what the outside converters measured on Gold200.
  So the 09-11 row of `docs/spike/white-placement.md` (`W`, "−0.88 stops short", the
  `d` = 0.488 in the fixed-`d` spread) is contaminated; the other two rolls carry no
  non-picture frame besides base and leader, which were excluded, so they stand. It also
  makes 09-11 the first real case of C's known weakness (it cannot tell a flat or
  underexposed roll from a wrong `d`, and pays in contrast). 09-11 is held out of marking
  until its control is decided; the review set holds 101 picture frames of the other three.
- 2026-09-23: **first measurement: 39 patches (27 new on three rolls, plus the spike's 12),
  read at the operator's input** — a `film-master` render under the candidate-C flags
  (linear ACEScg, diffuse white = 1.0), median per patch (`../temp/band-fit/
  input-measurements.json`, `separation.json`). Only 17 of the 39 sit at or above the
  brightness start (1 stop below white) — 11 W, 6 C, of which **1 C on Ektar and 2 on
  Portra, both from one frame**. The rest are outside the operator's reach, so they do
  not constrain the band. Findings, provisional on that count:

  **(1) The spike's 0.30 → 0.45 does not carry, as the task predicted.** Under candidate
  C, in-range whites read linear-RGB saturation 0.20–0.52 at the input (Gold200 up to
  0.52): gamma multiplies the decode's cast in log space, and Gold200 gets 4.15.

  **(2) The gamma-normalised log ratio is the steadiest measure; linear RGB the least.**
  With no white balance, `s_log` misclassifies 2 of 17 at its best threshold (~0.10) and
  separates Gold200 on its own (W ≤ 0.098, C ≥ 0.108); linear RGB overlaps by 0.27 across
  rolls because each roll's gamma rescales it.

  **(3) Per-frame auto white balance defeats the band before it acts.** With
  `--auto-wb percentile` on 1815 the two sunset clouds drop to `s_log` 0.008 — the
  estimator reads the sunset as the cast and removes it, so the operator then sees a
  white and pulls it. Gray-world is the same. Both auto modes misclassify 3–4 of 17. This
  is a scene-correction ↔ path-to-white interaction, not a band parameter.

  **(4) An idealised per-roll WB (leave-one-patch-out over the roll's W patches) separates
  Gold200 widely on every measure** (`s_log` W ≤ 0.034, C ≥ 0.056), but the same two
  patches stay misclassified in every state: 1727 Ektar "cloth" (a pastel — C\* 23.6 at
  the input, less than the Ektar whites) and 1880 Portra "cloud". Too few to say whether
  they are the measure's failure or the marking's.
- 2026-09-23: **second round: 52 patches, 29 in range (12 W, 17 C — 12 of the C on
  Ektar).** 1727's C cloth was re-marked with a W beside it; 1880's cloud is `?` (shot at
  5–6 PM, may be warm) and reported, not fitted. Still **no in-range Portra W**. Added a
  realistic fifth input state, **auto-roll**: one gain per roll, the per-channel median of
  per-frame `--auto-wb percentile` gains over every picture frame.

  **The result is that the band measures distance from the roll's white, so it is only as
  portable as the white is neutral.** Best single `s_log` threshold, misclassified of 29:

  | input state | misclassified | why |
  |---|---|---|
  | none (the new chain's default) | 6 | Ektar whites carry C\* 16–38 of cast (`s_log` 0.065–0.097), as much as skin, sand and a leaf (0.067–0.080) |
  | per-frame auto WB | 9–10 | removes sunsets on 1815 before the band sees them |
  | auto-roll | 5 | separates **within** each roll, at different places: Gold200 ≤ 0.029 vs ≥ 0.039, Ektar ≤ 0.180 vs ≥ 0.160 — Ektar's roll gain (`[1.43, 1, 0.79]`) overshoots and lifts everything |
  | ideal-roll (leave-one-out over W patches) | 3 | one band nearly fits both rolls: whites ≤ ~0.05, colours ≥ ~0.06 |

  The three that ideal-roll still misses are the informative ones: 1738 "cloth" W at 0.077
  (−0.87 stops, likely lit by coloured light), 1727 "cloth" C at 0.038 (a pastel) and 1730
  "leaf" C at 0.025 (green, which the Ektar gain partly cancels). So even at best the
  band's gap is ~0.01 of `s_log` wide, and it holds across rolls **only** if scene
  correction has neutralised the roll first; under the default neutral WB it cannot be
  placed for Ektar at all without flattening skin.

  **Measure: `s_log` (log ratio / gamma) wins in every state**; linear RGB never separates
  across rolls, because gamma rescales it per roll (Gold200 whites 0.33–0.52, Ektar
  0.20–0.34 with no WB).
- 2026-09-23: **which white the white balance should be measured on — a roll-level top
  percentile, not the film.** The user's framing, adopted: there is no correct WB, only an
  intent, and one global gain cannot keep a sunset on the sky while removing it from a
  cloth — so the preference is to **keep the scene's light** (sunsets stay warm) and remove
  only what is constant across the roll: film, process and scanner cast. That is what a
  roll-level statistic estimates, since one sunset frame barely moves it; it also makes
  1880's `?` cloud moot. The corollary for this task: `s0` should sit **low**, so the
  operator only cleans the faint residual a roll WB leaves — a higher `s0` would start
  neutralising sunset-lit whites, which is per-object colour removal by another route.

  **The film cannot supply the white.** The base is already the decode's per-channel
  divisor, so it neutralises black, where a multiplicative gain does nothing. The leader,
  rendered under the control, sits **+1.5 to +3.5 stops above diffuse white** and asks for
  gains the wrong way round (red 0.53–0.67, blue 1.8–2.6, against the W-patch gains' red
  1.11–1.27, blue 1.18–1.65) — past the straight line, at an uncontrolled exposure, which is
  why `film-base` had already disqualified it as a per-channel source.

  **Estimators tried** (Python on cached `film-master` pixels, 5% inset; its 95th-percentile
  gains match the binary's `--auto-wb percentile` to 0.01, median 0.0008, over 101 frames):
  - Median of per-frame percentile gains: **p95 → p99 fixes Ektar's blue** (0.79 → 1.16–1.21,
    ideal 1.28) — at p95 its top 5% is sky, not white. 5/29 → 4/29 misclassified.
  - Pooled roll top percentile, excluding pixels within `m` density of the leader on any
    channel (the user's guard against a fully exposed frame mixed into the roll): 2–6/29
    for every sensible variant, thresholds 0.03–0.05. **No variant clearly wins at 29
    patches.** The per-channel form is steadier across `q` than a median of the brightest
    pixels. The leader guard removes 0–5% of pixels at `m = 0.1` and changes little here —
    none of these rolls holds a blown frame, so its protective value is untested; at
    `m = 0.2` it eats 16% of Ektar, whose white sits only 0.15 below its leader.
  - Portra disagrees with its "ideal" on red under every estimator (0.86–1.03 vs 1.12), but
    that ideal rests on two dim W patches and no in-range one; the user found no bright
    whites on those frames, so **Portra's W side is a known gap** for now.

  Working choice for the pair check: **pooled per-channel p99, leader margin 0.1**. The
  estimator itself belongs to scene correction; its exact form is not this task's to settle.
- 2026-09-23: **pair check rendered — the band does its job structurally.** Throwaway
  operator saved as `../temp/band-fit/operator.patch` (binary `bin/hanten-ptw`, env
  `NC_SPIKE_PTW="start,k,s0,s1,gamma"`; reverted from `src/`, never merged): a pull toward
  `(Y, Y, Y)` after the shared controls, brightness ramp (smoothstep) from 1 stop below
  diffuse white to white, max pull `k = 0.8`, band over `s_log` computed on film RGB (the
  inverse of the v1 matrix) of the white-balanced pixel. Per-roll WB = pooled per-channel
  p99, leader margin 0.1 (`roll-wb.json`). 22 frames, every in-range patch, SDR base:

  | mean C\* | raw | + roll WB | + pull, no band | + band 0.025→0.055 | + band 0.020→0.035 |
  |---|---|---|---|---|---|
  | W (12) | 24.4 | 7.2 | 3.2 | 4.4 | 5.7 |
  | C (17) | 30.6 | 17.0 | **7.2** | **16.1** | 16.8 |

  **(1) White balance does most of the cleaning** (24.4 → 7.2); the operator's share on
  top is 7.2 → 4.4 with the band. **(2) The band protects every C above `s1` exactly**
  (100% of chroma kept at `s_log` ≥ 0.077, 96–99% at 0.061–0.070 where some pixels fall in
  the ramp), where the unbanded pull flattens them to 7.2. **(3) The cost sits in the
  ramp:** 1815's sunset clouds (0.034 / 0.041) keep only 50% / 64% under the main band and
  82% / 99% under the narrow one, which in turn cleans whites less (5.7). Luminance held:
  |ΔL\*| ≤ 0.17 between WB and band. Set: `review-pair.json`, for the user's eye.
- 2026-09-23: **user's verdict on the pair check, and the task closes.** Band vs narrow
  band on 1815/1810: "very small … I like 4 a little better" — so the wider **0.025 →
  0.055** stands. WB vs WB + banded pull on the whites: "I cannot tell real difference" —
  the operator's cleaning on top of a roll WB is invisible at strength 0.8, one stop start.
  Pull vs band: little difference on skin, **observable on 1739's rock, band better**.

  Held-out check, Gold200 vs Ektar (Portra has no in-range W): fitted on either alone the
  band lands in the same zone; on the other, no `c` patch is fully pulled and one `w` is
  treated as colour (Ektar 1738 cloth at 0.067, likely lit by coloured light).

  Written up as [`docs/spike/desaturation-band.md`](../spike/desaturation-band.md);
  `path-to-white` carries the values, the measure, the white-balance precondition, and a
  new open question on whether the operator earns a default-on place. Open, not this
  task's: Portra's white side; the roll-WB estimator (scene correction's); the 09-11
  correction to `white-placement.md`.
- 2026-09-23: follow-ups done after `/code-review`: the report now states that the whites
  below `s0` do **not** clean identically (four of seven keep 0.7–1.6 C\* more with the
  band, because a patch's pixels scatter across `s0`), and fixes three counts (52 patches
  in all; 29 in range over three rolls; Ektar p99 blue 1.16–1.21 vs the pooled 1.25). Filed
  `nf-scene-correction/roll-white-balance`, which `path-to-white` now depends on, and added
  the 09-11 correction to `white-placement.md` and every task file quoting it.
- 2026-09-23: second `/code-review` pass, all fixed. The pair check is only partly met on
  **both** halves, not one — colours just above `s1` keep 96–99%, not all — and the task
  stays **done by user decision**, with the band-edge placement carried to `path-to-white`
  as an open question. Also: the chart legend's scale (12 characters per 0.02), the
  1738 cloth white above `s1` stated in the placement, "upper bound" → "reference" for the
  W-patch white balance (a pooled variant scored better at its own threshold), `s` stated
  as computed on film RGB, and the 09-11 correction's two spread statistics told apart.
