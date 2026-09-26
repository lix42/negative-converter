# Hanten — nf-display-stages Progress Log

Execution log for the `nf-display-stages` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status; this file is the
narrative beside it.

One `##` section per task in this epic, named by the bare task name. Read this
whole file before starting a task in this epic, and read other epics' `Epic
summary` sections when you depend on them. Append entries — don't rewrite earlier
ones.

## Epic summary

Fit range and fit gamut as real stages, shared by both display branches, plus the operator question the shadow end raises.

The epic was created on 2026-09-19 as part of the new-flow migration plan
(`docs/nf-migration.md`).

**Fit range has landed** (`fit-range`, 2026-09-23): one reinhard with the display's
peak as its argument, `Y′ = r(Y)·(1 + (P − 1)·s(Y))` on ACEScg luminance. Every peak
agrees bit for bit below diffuse white (`algo::fixed::DIFFUSE_WHITE = 1.0`); above `W`
the output exceeds the peak on every branch and the encoder counts it; an HDR
destination gets no hard ceiling there (decided 2026-09-24, `branch-contract`). Knob `fit_range.headroom_stops` (`--display-tone-headroom`);
the peak is the destination's. Non-finite samples are refused.

**Fit gamut has landed** (`fit-gamut`, 2026-09-24): `fit_gamut::radial_to_boundary` is
the one gamut map. The legacy renderers call it with their own ceilings, byte-identically;
the new flow maps against `max(peak, Y)`, with the peak on `RangeFittedImage`. Above the
peak, content renders neutral at its luminance and clips at the encode. At the default
headroom the new flow clips nothing on 92 real frames.

**The branch contract has landed** (`branch-contract`, 2026-09-24): SDR and HDR split
after the look (`chain::render_pair` grades once and copies the `GradedImage`), and the
headroom is shared above the split, so the renditions differ only in the peak. Below
white they are bit-identical except where the SDR cube binds, which
`chain::contract::check` re-derives exactly: 0 violations on 92 real frames at every
headroom measured. `pipeline::gain_ratio` is the pair's per-channel gain, ratioed
against the base as stored. No hard HDR ceiling above `W`: the encoder clamps and
counts what passes the peak — a gain-map destination must do that itself
(`nf-destinations/gain-map-destination`), since `gain_ratio` does not.

**The gamut map's share of highlight desaturation is near zero where it matters**
(`gamut-map-share`, done 2026-09-23; `docs/reports/gamut-map-share.md`). It moves no
marked white under `sigmoid-knees`, the desaturation spike's control, or
`path-to-white`'s hand-set contrast, and every limit it hit was the cube's top, never
P3's primaries. It acts heavily only under the `shoulder` display tone, whose plateau
leaves no chroma room at the cube's top.

So under the `none` and `reinhard` tones in SDR, `fit-gamut` need not shrink the map to
make room for the look's operator. Re-check that if fit range's operator plateaus near
display white, and for HDR, which was not measured.

The map gets no diagnostic off switch. Should one ever be wanted, "off" means an unmapped
float destination, never a per-channel clip.

## fit-range

**Status:** done
**Updated:** 2026-09-23

- 2026-09-19: created with the new-flow plan. Goal: fit range as one stage.
- 2026-09-23: **done.** `pipeline::fit_range` is one operator with the display's peak
  as its argument: `Y′ = r(Y)·(1 + (P − 1)·s(Y))` on ACEScg luminance, channels scaled
  by `Y′/Y`. `r` is the mid-grey-preserving extended reinhard at `W = 2^headroom_stops`
  (bit-identical to `display_tone::extended_reinhard`, pinned while both exist); `s` is
  a smoothstep in stops from diffuse white to `W`. Decisions (user, 2026-09-23):
  - **Both peaks in one function now**, though no HDR destination reaches it yet.
    Chosen over legacy's HDR form (asymptotic base + lift): `P = 1` is exactly reinhard
    and transcendental-free, and **every peak agrees bit for bit below diffuse white**
    (legacy: within 0.03%). The price: `r`'s `v/W²` tail survives, so content above
    `W` exceeds `P` on HDR as it exceeds `1.0` on SDR — `f(W) = P·r(W)`, 1.006·P at six
    stops — clamped and counted at the encode. Legacy dropped the tail to hold HDR
    strictly under its 1000-nit peak (measured: an unbounded base peaked 5.3–17.0
    against 4.93 on seven frames). Revisit in `branch-contract` if an HDR destination
    needs a hard ceiling.
  - **Diffuse white is `algo::fixed::DIFFUSE_WHITE = 1.0`**, the value the decode
    renders its anchor to (≈0.08 stop from the datasheets' diffuse white). The lift
    starts there; `nf-look/path-to-white` reads the same constant.
  - **Knob: `fit_range.headroom_stops`** (default 6, `0`–`24`, `0` the identity) via the
    existing `--display-tone-headroom`, now kept under `--new-flow`. The peak is the
    destination's (`chain_params(peak, gamut)`), never a recipe key. No selector:
    `--display-tone` is refused at every value (`Never`), and `--sigmoid-shoulder`
    became `Never`, pointing at the headroom.
  - **Non-finite samples are refused**, naming the lowest pixel, at every headroom;
    luminance ≤ 0 passes through untouched. Luminance uses a new pinned
    `ACESCG_LUMA` (exact derivation, audited).
  - Report: `new_flow.fit_range` = `{operator, headroom_stops, white_point,
    display_peak}`; the stage list reads `reinhard-peak-lifted-v1`, or `identity` when
    the white point is 1.
  - Goldens (`chain_golden`): SDR bit-exact; HDR windowed over `log2` and asserting
    bit-equality with SDR below white; threaded vectors recaptured at the shipped
    headroom and moved to the seven finite pixels (the NaN pixel's refusal is its own
    test). Mutation-checked: `MID_GREY` 0.1801 reds SDR, HDR and threaded; a linear
    ramp in place of the smoothstep reds HDR.
  - On `hdr-48bit.tif` at the guide's base: 0.12% of samples clipped at the default,
    32.23% at zero headroom — byte-identical to the pre-task binary. The guide's quoted
    9.88% for that command was already stale before this change.
  - Not done here: fit gamut does not yet read the peak (`fit-gamut` adds it to
    `RangeFittedImage` when its ceiling needs it); display black / a toe
    (`parametric-operator`).
- 2026-09-24: review round (`/code-review high`), all fixed:
  - **Luminance ≤ 0 is no longer passed through unscaled.** `f(Y)/Y → gain` as
    `Y → 0⁺` (≈1.22 at six stops), so scale 1 there stepped every channel ~22% where a
    saturated colour's luminance crosses zero. Those pixels now take the limit, the
    mid-grey gain (user decision) — continuous, still unclamped. No golden moved (the
    fixtures hold no such pixel); a unit test pins the continuity.
  - **The headroom rule is `types::headroom_fault`**, one predicate returning which
    rule failed; the current chain's `check_headroom_stops` and the new chain's
    `validate_fit_range` word their own messages from it, and the stage re-checks it.
    `FitRangeParams::check` and the placeholder peak it needed are gone.
  - **"Bit-reproducible across targets" was too strong**: `W = 2^stops` is an `exp2`
    call, exact only at whole stops. The module doc now says so.
  - `--display-tone-headroom`'s help names both chains' recipe keys; the stale "a
    non-finite sample reaches the encoder" comments in `scene_correction`, `fit_gamut`
    and `working_image` now say fit range refuses it; the goldens and tests state the
    HDR peak themselves instead of importing `hdr::LINEAR_HEADROOM`, which retires.
  - Declined: a v2 recipe saved before this renders differently (unversioned new-flow
    output is `nf-verification/fingerprints`' gap); overflow of the scale at huge
    luminance (finite for any f32 at the default); the reinhard duplicate of
    `display_tone` (the migration rule; a test pins them bit-identical).
- 2026-09-24: rebased onto `nf-retire/sigmoid-and-simple` (#151), which removed the
  `--sigmoid-*` flags and `--reconstruction` outright. So the `--sigmoid-toe` /
  `--sigmoid-shoulder` availability rows this task had reworded are gone with them;
  instead the removed-flag message for both knees now names `--display-tone-headroom`
  as the `--new-flow` remedy (it said "not yet available under `--new-flow`"), and its
  test checks the new flow accepts that flag.
- 2026-09-24: rebased onto `nf-scene-correction/roll-white-balance` (#152), which
  retired the new chain's per-frame auto white balance and dropped `chain::render`'s
  measurement region. The threaded auto-white-balance golden went with it; the fit-range
  goldens, the finite-pixel input and the NaN-refusal test carry over unchanged.

## fit-gamut

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: one gamut-mapping implementation.
- 2026-09-24: **done.** Decisions (user, 2026-09-24): switch the legacy call sites to
  the new primitive; report id `…-v2`.
  - **One primitive**, `fit_gamut::radial_to_boundary`, written fresh. The three copies
    did identical arithmetic, and there were **four** call sites, not three: `hdr` also
    maps HLG's scene-linear signal against `1.0`. Switched and deleted. Legacy output
    measured **byte-identical** on both fixtures × nine display presets × `shoulder` /
    `reinhard` (36 of 36), with every legacy golden untouched.
  - **The new stage's ceiling is `max(peak, Y)`**, with the peak added to
    `RangeFittedImage` (fit range resolves it). Fit range overshoots the peak above `W`,
    which is the legacy SDR situation, so the same rule applies to both peaks. Above
    the peak the cube holds only the neutral, so that content renders neutral at its
    luminance. Luminance comes from the destination's luma vector after the matrix.
    `Y ≤ 0` renders black.
  - **Mutation-checked**: ceiling pinned at the peak, peak ignored, ACEScg luma in
    place of P3, `Y ≤ 0` branch dropped, exact-boundary assignment dropped, and gating
    the map at the peak (the ring). Each reds at least one test. Pinning is *not* the
    ring, because the scale goes to 0 continuously as `Y` crosses the peak; gating is.
    The exact-boundary assignment needed a sweep test: hand-picked vectors land on the
    boundary without it.
  - Goldens: `FIT_GAMUT_P3` and `THREADED` recaptured. The fit-gamut vector now covers
    in gamut, a negative channel, a channel just over 1 and a pixel above the peak.
  - **Real frames**, `HEAD` vs this change, new flow at defaults, 92 frames on
    Ektar100 0909, Portra400 0911 / 0920 and Gold200 0918: clipped samples
    **336,106 → 0**, and frames clipping 36 → 0. So at the default headroom every
    clipped sample had been an out-of-gamut colour. Changed pixels ≤ 0.9% per frame
    (Portra 0920 / 1903), mean ≤ 0.05% per roll. **None of the 41 marked whites moved.**
    On `hdr-48bit.tif`: 0.12% → 0 at the default, and 32.23% → 31.92% at zero headroom.
  - The SDR and HDR ceilings disagree below diffuse white for saturated colour; noted
    on `branch-contract`, which owns it.
- 2026-09-24: review round (`/code-review`). Fixed:
  - `two_ceilings_disagree_only_where_one_of_them_binds` was close to a tautology.
    Every fixture had a negative channel, so black limited them all and the HDR ceiling
    never bound. It is now four explicit groups (inside, black-limited, over SDR only,
    over both), mutation-checked by ignoring the ceiling.
  - `radial_to_boundary` debug-asserts its precondition `0 ≤ Y ≤ ceiling`, which
    holds across the whole suite, current-chain callers included. Its "bit for bit"
    claim was narrowed: a channel far below the luminance can round to 0, and `-0.0`
    comes back as `+0.0`.
  - Report id renamed to `…neutral-axis-radial-boundary-v2` to line up with the legacy
    metadata's `…-boundary-v1`. The suffix names the caller's ceiling rule, since the
    arithmetic is shared.
  - Stale prose: `recipe.rs`'s `FitGamut`, `TASKS.md`'s dependency rationale, the
    golden's `FILM_RGB` note, and design-spec §9, which now describes the stage. The
    guide now says a colour at P3 luminance ≤ 0 is written black.
  - Declined: a ceiling of `max(DIFFUSE_WHITE, Y)` below white to restore SDR/HDR
    agreement. It would desaturate HDR for nothing, and the disagreement is what a gain
    map carries; `branch-contract` owns it. Also declined: an in-gamut fast path, which
    is an unmeasured gain and could move current-chain bits on the edge samples above.
- 2026-09-24: second review round (`/code-review high`). Fixed:
  - **A finite input could reach the encoder as NaN.** At zero headroom a large finite
    sample overflows the 3×3 (`1.379 × 3.3e38`), and the NaN or infinite luminance got
    past the `Y ≤ 0` test. The stage now refuses it and names the pixel. The test is
    mutation-checked.
  - `Y ≤ 0 → black` moved into `radial_to_boundary`, so a new caller cannot forget it.
    `Y ≥ peak` is written directly as `[Y; 3]`.
  - **The current chain now renders through a new-flow module**, and no fingerprint
    reaches that far. So the primitive is pinned bit for bit at each caller's ceiling
    (`1`, `1000/203`, `max(1, Y)`). That includes a colour whose intersection misses the
    boundary without the limiting channel's assignment, which the first vectors did not
    catch.
  - The id-suffix doc no longer says a suffix names a ceiling rule (the legacy ids
    contradict it). Stale prose fixed: `using-nc.md`'s and `recipe.rs`'s "fit gamut gets
    a knob later", and `chain::render`'s list of which stages can fail.
  - The task's first criterion is recorded as met by the byte-identical current-chain
    renders, not by a render-level test.
  - Re-verified: the current chain 36 of 36 byte-identical, and the new flow on
    `hdr-48bit.tif` identical to the first round at headroom 6 and 0.
  - Declined: re-freezing the current chain away from the shared primitive (the task's
    decision; the golden above guards it), and de-duplicating the tests' restated
    matrix (independent restatement is what makes them goldens).
- 2026-09-24: third review round (`/code-review high`). Fixed:
  - **The "follow the pixel" rule has one spelling now.** `radial_to_boundary` raises
    its ceiling to the luminance itself. That replaces SDR's `.max(1.0)`, the `Y ≥
    peak` branch in `apply`, and the debug assert, so no caller can pass a luminance
    the cube cannot hold. The primitive's golden merged its SDR and follows-the-pixel
    arms. Removing the `max` fails five tests in three modules.
  - **Caller coverage, checked by mutation rather than assumed.** A wrong ceiling at
    SDR's or the gain map's call site already failed their own tests. Neither HDR call
    site was covered: the display render (headroom → `max(1, Y)`) and HLG (`1` →
    headroom) both survived. `hdr::tests::each_call_site_maps_against_its_own_ceiling`
    now fails on either.
  - The docs now say fit gamut's losses are not counted: a negative channel moves onto
    `0` with the colour, and `Y ≤ 0` is written black, neither in the encoder's clip
    count. The chain test also asserts the mapped pixel keeps its luminance, so a
    black-out fails it (mutation-checked).
  - Prose: the task file's Goal and Open sections, `design-update.md`'s legacy ids
    (`bt2020-` / `display-p3-` prefixes), and the `radial_to_boundary` doc shortened.
  - Re-verified: the current chain 36 of 36 byte-identical to round two; the new flow
    on `hdr-48bit.tif` identical at headroom 6 and 0.
- 2026-09-24: rebased onto `nf-look/path-to-white` (#156), `nf-retire/display-tones`
  (#157), the CLAUDE.md cut (#158) and `nf-retire/dmax-machinery` (#159). The legacy
  renderers were rewritten around `display_tone::Headroom`, so I took main's `sdr`/`hdr`/
  `gain_map` and re-applied the swap to them. The HDR call-site test now uses zero
  headroom where it used `--display-tone none`. Re-verified: 46 renders byte-identical
  to an `origin/main` build and none differ (2 fixtures × 9 presets × headroom 6/2/0).
  The other 8 are the SDR presets at zero headroom, which both builds refuse on this
  content. The new flow runs the look before fit gamut now, but the guide's numbers are
  unchanged: no clipping at the default, 31.92% at zero headroom.

## parametric-operator

**Status:** not started
**Updated:** 2026-09-25

- 2026-09-19: created with the new-flow plan. Goal: a parametric operator with a toe.
- 2026-09-25: **now also places black** (user, from [`nf-calibration/anchor-comparison`](../tasks/nf-calibration/anchor-comparison.md)). The new chain has no black
  point; every white rule reviewed there looked pale without one and improved with a probe
  that moved where the film base renders (L\* 12–15 at the cap round's contrast ≈ 1.8; 2.6–9.2 under
  the chosen rule, lower at higher contrast) to L\* ≈ 2. The base is the reference:
  every roll's darkest pixels bottom out there, and it is already measured. The probe was a
  subtraction, so it is the bar to match, not the mechanism. Folded here rather than filed
  apart, because where black lands and how the curve reaches it cannot be judged separately.
  `nf-calibration/roll-white-rule` now depends on this task.

## branch-contract

**Status:** done
**Updated:** 2026-09-24

- 2026-09-19: created with the new-flow plan. Goal: the sdr/hdr branch contract.
- 2026-09-24: implemented; the HDR-ceiling question is measured and awaits a decision.
  Decisions (user, 2026-09-24):
  - **The gamut map may split the pair below white** (option A). Where the SDR cube's
    top binds a saturated colour, SDR maps it and HDR keeps it, and the gain map carries
    the difference per channel, with gains below 1 as well as above. The alternative,
    mapping HDR into the SDR cube below white, discards colour HDR can show.
  - **A small fresh gain function**, `pipeline::gain_ratio`: per-channel ratio against
    the base *as stored* (clamped to `[0, 1]`), a full-precision round trip
    (`apply_to`), and `GainRange::flat`, so a flat map is a stated fact. Downsampling,
    quantization and the container stay with `nf-destinations`.
  - **Measure before deciding the HDR ceiling.**
  - **Shape.** `chain::render` is `grade` (scene correction, look) then `display` (fit
    range, fit gamut); `render_pair` grades once, clones the `GradedImage`, and runs
    `display` per peak. `ChainParams` is `SharedParams` (scene correction, look, **and
    the headroom**, which shapes the midtones) plus the destination's `DisplayTarget`
    (peak, gamut). A pair takes one gamut. `render` is bit-identical to the matching
    branch of `render_pair` (tested), so a single destination is not a second path.
    No output moved: every chain golden passes unchanged.
  - **The check** (`chain::contract::check`): below white read exactly as fit range
    reads it (ACEScg luminance of its input ≤ `DIFFUSE_WHITE`), each pixel is
    bit-identical, or differs with the SDR rendition at its cube's top (a channel
    ≥ 1), or is a violation. **Gotcha:** the SDR ceiling is `max(1, Y)` in the
    destination's luminance, and a saturated blue at ACEScg `≤ 1` reads `1.012` in
    Display P3 — so SDR renders it neutral at 1.012, not with a channel at exactly 1.
    The first version of the check (a channel `== 1.0`) called that a violation;
    it showed up only at zero headroom, where fit range leaves such pixels near white.
    The unit test now runs at both headrooms.
  - **Falsifiable:** the check reports violations for a per-branch headroom and for
    the look applied to one branch only (both tests); a lift starting half a stop
    under white (mutation by hand) fails the pair test and the flat-map test.
  - **Real frames** (`pipeline::branch_probe`, `#[ignore]`; job, bases and output in
    `../temp/branch-contract/`): 92 frames on Ektar100 0909, Portra400 0911 / 0920 and
    Gold200 0918, stride 2, default recipe with each roll's base, HDR peak 1000/203.
    **0 violations at headroom 6, 3, 2 and 0.** Below-white pixels the SDR cube splits:
    0.011% at 6, 0.2% at 0. The gain map rebuilds HDR to ≤1.2e-7 relative, and is
    flat on 16 frames (nothing above white, nothing bound).
  - **HDR above the peak** (fit range keeps reinhard's tail): 0 samples at 6 stops;
    57 samples on 4 frames (max 1.21·P) at 3; 0.0014% of samples on 21 frames (max
    2.01·P) at 2; 393 on 9 frames (max 3.05·P) at 0. The SDR rendition exceeds 1 on no
    frame at the default either. Recommendation: no hard ceiling — nothing reaches it
    at the default, and what lower headrooms push past is clamped and counted.
- 2026-09-24: review round (`/code-review high`). Fixed:
  - **The check was too loose where differences are allowed**: any SDR pixel with a
    channel ≥ 1 passed, whatever the HDR pixel held. Below white, fit range's output is
    the same for both peaks, so an HDR pixel its own cube leaves alone *is* fit gamut's
    pre-map value, and the SDR pixel must be exactly `radial_to_boundary(hdr, Y, 1)`.
    The check now asserts that bit for bit, and keeps the loose rule only for a pixel
    both cubes bind (`both_bound`). Grid and 92 real frames at headroom 6, 3, 2 and 0:
    **every difference re-derives exactly, `both_bound` 0**, 0 violations.
  - **`gain_ratio::between` refused legitimate pairs**: where two channels tie on the
    black face, the one fit gamut does not assign lands about an ulp below zero (≈5% of
    exact ties, emulated). It now takes each sample as an encoder stores it (SDR to
    `[0, 1]`, HDR to `≥ 0`) and refuses only non-finite ones.
  - `GradedImage` is no longer `Clone`; `split` (pipeline-only) is the branch point's
    one copy, and the minting test counts it. `memory::RunProfile::NewFlowSdrTiff` says
    it covers one branch and a pair needs its own arm.
  - The probe's rolls and bases are in the code (`ROLLS`), with the parameters from
    `NC_PROBE_*` variables, so the numbers above reproduce from the repository; it
    fails when no frame is measured. `contract::check` no longer copies the buffers.
  - `fit_range`'s module doc points at the open ceiling question instead of quoting a
    count; `Recipe::chain_params`' doc no longer says "in chain order".
  - Declined: decoding each probe frame once instead of twice (the probe runs in about
    13 s, and grading through `render_pair` itself keeps it measuring the shipped path);
    sharing the asset helpers with `shadow_metrics` / `curve_probe` (pre-existing
    duplication, not this task's).
- 2026-09-24: **decided (user): no hard ceiling above `W` for an HDR destination.** The
  recommendation above stands: nothing reaches the peak at the default headroom, and
  the encoder clamps and counts what lower headrooms push past. Nothing is open.
- 2026-09-24: review round. "The encoder clamps and counts" holds for a
  single-rendition encoder; for a gain map, `gain_ratio::between` clamps HDR only to
  `≥ 0`, so an above-peak sample would become a larger gain, uncounted. Recorded as an
  obligation of the gain-map destination in `nf-destinations/preset-set` (with
  `render_pair` and `gain_ratio` as its inputs); `gain_ratio`'s module doc says the
  caller clamps and counts. `WorkingBuffer` is no longer `Clone`: its one full-frame
  copy is `WorkingBuffer::copy`, called by `GradedImage::split`, and that copy includes
  the IR plane (noted in `nf-destinations/memory-profiles`). The real-frame probe now
  also asserts `both_bound` is 0.
- 2026-09-24: **done.** Landed: `chain::render` is `grade` (scene correction, look,
  shared headroom) then `display` (fit range, fit gamut per `DisplayTarget`);
  `render_pair` grades once and copies once (`WorkingBuffer::copy`, not `Clone`), and
  `render` is bit-identical to the matching branch. Verified: every CI gate green;
  `chain::contract::check` re-derives every below-white difference bit for bit, 0
  violations and 0 `both_bound` on 92 real frames (`branch_probe`, re-run after the
  review loop); the gain map rebuilds HDR to ≤1.19e-7; no chain golden moved. For
  `nf-destinations/preset-set`: the gain-map destination consumes `render_pair` and
  `gain_ratio`, ratios against the base as stored, reports a flat map, and must clamp
  HDR to its peak and count it (`gain_ratio::between` clamps only to ≥ 0). For
  `nf-destinations/memory-profiles`: a pair holds two full-frame buffers, the IR
  plane included.
- 2026-09-24: rebased onto `nf-reconstruction/gamma-split` (#164), which gives the look
  a print contrast by default (`look::DEFAULT_CONTRAST`). Only the test fixture's
  `LookParams` moved (`linearization` replaces `decode_contrast`); nothing of this
  change was dropped. **The grid now has `both_bound` pixels** (33 of 471 below
  white, against 187 re-derived exactly): contrast pushes its most saturated blues
  onto a face of the HDR cube too — the black face, or the peak itself at ACEScg blue
  ≈ 6.7 with a luminance under white. Permitted, so the grid test bounds them
  (`both_bound < sdr_bound`) instead of pinning 0. Real frames re-measured, 92 frames
  at headroom 6 / 3 / 2 / 0: **0 violations and 0 `both_bound` at every headroom**;
  SDR-bound 0.010% / 0.011% / 0.012% / 0.197% of below-white pixels; HDR above the
  peak 0 / 57 (max 1.21·P) / 0.0014% (max 2.00·P) / 381 on 9 frames (max 3.03·P).
  The no-ceiling decision stands.

## gamut-map-share

**Status:** done
**Updated:** 2026-09-23

- 2026-09-22: filed, split out of `nf-look/path-to-white`. Goal: how much of the
  highlight chroma convergence nc already produces is the **gamut map** rather than a
  tone or reconstruction curve. Two things rest on it: `path-to-white` must not double up
  with the map, and the map is one of only two surviving candidates for the knee'd
  sigmoid's clean whites after design-update Appendix F eliminated the other three
  (`sdr.rs:249-266`, radial near luminance 1.0 with a ceiling that follows the rendered
  luminance — which is how the display tone reaches it indirectly although no nc tone can
  converge channels itself). Nothing turns the map off by flag, which is exactly why
  `nf-look/desaturation-spike` could not separate them; `--display-tone shoulder` failed
  as a separator because it drives top-end chroma to 0.0 with 17-29 degrees of rotation,
  i.e. it flattens rather than shapes. Runs against today's binary. Two routes: a
  throwaway patch disabling the map (measures the share) or counting top-end samples out
  of gamut before mapping (bounds it, no patch).
- 2026-09-23: **done.** Report: `docs/reports/gamut-map-share.md`; scripts, job file,
  raw results and the throwaway patch (`probe.diff`) in `../temp/gamut-map-share/`.
  - **Route: a throwaway in-crate probe, not a binary patch.** An env-var patch skipping
    the map would have handed the overshoot to the u16 encode, whose per-channel clip is
    a gamut policy of its own, so it would have measured map-vs-clip. The probe split
    `render_destination_pixel` into tone-scale and map, read both sides in float, and
    asserted its mapped output bit-identical to `sdr::render` (403 of 403 accepted
    renders). The task's "counting needs no patch" was wrong: nothing reports the
    pre-map values, so counting needs the same patch.
  - **Scope:** 92 frames on Gold200 0918, Ektar100 0909, Portra400 0911 and 0920; 41
    marked whites (17 on 0920, marked for this task). Five neutral-patch marks were lost
    to frames since pruned from the assets (Ektar 1615/1620/1623, Portra 1643/1652).
  - **Knees: 0.00% of top-end pixels touched on every roll**, so the knee'd whites are the
    shoulder's. The 2x2 with `--sigmoid-shoulder 0` is not a share: that render is
    refused on every frame, and in float the map greys what overshoots, so the map-first
    ordering credits it 93-130%.
  - **Spike control / C / D (reinhard): mean C\* removed 0-2.6 per roll**, from a few
    bright frames; no marked patch moved (largest 0.001).
  - **`path-to-white`'s hand-set spelling named no display tone**, so it meant `shoulder`,
    under which the map removes 5.3-13.8 on average (worst 52). Corrected there to
    `--display-tone reinhard`.
  - Off switch: none, on either chain (user decision 2026-09-23).
