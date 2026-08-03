# Negative Converter — algo Progress Log

Execution log for the `algo` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `algo`:

- **The shipped surface is `reconstruct(image, base, config) -> (FilmRgbImage,
  ReconstructionReport)` plus `finish_print`** — the old `Converter` trait and
  `AlgoParams` are gone. The recipe is one **tagged `reconstruction` object**
  (`schema_version: 1`) selecting `simple` or `density`, with density carrying a
  tagged `exponential` (default) or `sigmoid` curve. The legacy `algorithm` +
  top-level `density`/`sigmoid`/`simple` keys are **rejected with migration
  errors** — never re-add them as aliases.
- **`FilmRgbImage` is the typed boundary out of this epic.** Private fields,
  constructible only inside `algo`, so nothing can mint one that skipped
  reconstruction. `working_space::map_nc_film_rgb_v1` is its only intended
  consumer; its legacy alternative is `finish_print`, the stage-4 bridge that
  presets will displace.
- **Polarity: a *denser* negative renders *brighter*.** Stage 3 is
  `10^(+γ·(D′ − Dmax))`, not the `10^(−…)` in early spec sketches. A regression
  test pins this.
- **Density conversion and print rendering are separate sub-stages** — the core
  fidelity rule. Stage 2 (`to_density` → `regional_balance`) is shared by both
  curves; the curve is stage 3; `render_print` is the shared stage 4. A future
  curve injects itself as the stage-3 closure and reuses the rest.
- **Anchoring is not optional for sigmoid.** Both curves consume the resolved
  scalar `Dmax` from `film-base`; sigmoid *requires* a positive anchor and rejects
  `none`. `DmaxSource::None` is the bit-exact scene-referred escape hatch and is
  a density-only feature.
- **For sigmoid, `Dmax` is the *reference*, not the anchor** (since
  `reference-anchored-sigmoid`, 2026-08-03). `curve.anchor`
  (`AnchorPlacement`) says which tone the reference places: the default
  `MidAtDmaxFraction(0.5)` pins **mid-grey** at half the reference and lets white
  land above it, `WhiteAtDmax` is the old rule kept as a diagnostic. Consequences
  for other epics: the two coincide only under `WhiteAtDmax`, so the paper-black
  floor is `10^(−contrast·A)` and the reduce-to-exponential identity holds only
  there; a per-stock rule is a **third variant**, not a new field; and the default
  sigmoid contrast/shoulder are now `≈2.0687`/`0.6`, derived from manufacturer aim
  densities rather than chosen. Drift fingerprints did **not** move — the default
  recipe still selects `exponential`, so `output/presets` still owns that bump.
- **Mutually exclusive knobs are one enum, never parallel fields** — `WbSource`,
  `BalanceRange`, `DmaxSource`, the tagged `Reconstruction`/`DensityCurve`. This
  is what makes the flags-win merge sound and provenance representable.
- **Auto modes are two-pass and report their result for reuse.** Auto white
  balance renders a neutral analysis pass, estimates, then re-renders through the
  normal slot; the resolved gains ride back in the report and reproduce the output
  **bit-exactly** when fed back as explicit values. Same pattern for
  `balance_range`. The sidecar records the *mode*; the report carries the frozen
  values — freeze from the report.
- **A knob that would be silently ignored is a loud error, not a no-op.** An auto
  WB mode under an algorithm with no print WB stage, a customized `gamma` under
  sigmoid, sigmoid flags under exponential — all rejected or warned after merge.
- **Numerical discipline:** non-finite input propagates through every stage
  untouched so `io::encode`'s non-finite counter still sees it. Never launder
  `NaN` (`f32::max(NaN, 0.0)` returns `0.0` — a real trap here). Extreme-but-finite
  params that would posterize are bounded by explicit caps (`contrast ≤ 50`, knee
  widths ≤ 10) because such output trips *no* counter.
- **Golden fixtures live in `pipeline::stages::golden`** — curated **per-pixel**
  `f32::to_bits` vectors, plus a decoded-pixels hash. Never checksum a whole
  encoded TIFF or post-lcms2 pixels: the embedded ICC and colour transform differ
  by target, so such a gate is green locally and red on CI.


## interface
**Status:** done
**Updated:** 2026-06-16

- Goal: `Converter` trait + algorithm selection so converters are pluggable.
- **Done.** Everything lives in `src/algo/mod.rs`:
  - `Converter` trait kept **object-safe** — params live in the implementor, no
    associated `Params` type, `convert(&self, image, base) -> Result<LinearImage>`.
    The design-spec §7.2 sketch shows an associated-type variant; that can't form
    `Box<dyn Converter>`, which `build()` and the verification both need, so this
    task supersedes the sketch (noted in a doc comment on the trait).
  - `Algorithm { Simple, Density }` — `Copy`, `serde(rename_all="lowercase")` so it
    round-trips as `"simple"`/`"density"`, `#[default] Density` (the documented
    default algorithm).
  - `FromStr for Algorithm` with `type Err = NcError`; unknown names →
    `NcError::Usage` (exit 2), failing loudly instead of defaulting. CLI parses
    `--algorithm` through this.
  - `AlgoParams` enum: `Simple(SimpleParams)` and
    `Density { density: DensityParams, print: PrintParams }`. **Decision:** the
    `Density` variant (and the `Density` converter struct) carries **both**
    sub-stages' params now — density correction + the separate print render —
    rather than deferring `PrintParams` to `algo-density`. They stay distinct
    fields, preserving the density/print separation (core fidelity rule).
    `AlgoParams::algorithm()` reports which algorithm a param set selects.
  - `build(params: AlgoParams) -> Box<dyn Converter>` — **infallible**, takes the
    param set by value and moves it into the converter (no clone). The task sketch
    had `build(algo, params)` taking the algorithm separately, but the
    `AlgoParams` variant already *is* the algorithm selector (`AlgoParams::algorithm()`
    derives it totally), so a separate `Algorithm` argument carried zero info and
    only created a mismatch error that one argument makes unrepresentable
    ("make illegal states unrepresentable"). Any `--algorithm` vs flag
    contradiction is resolved/rejected in `cli-framework` where the flag context
    lives, and the CLI hands `build` one already-valid `AlgoParams`. (Decision from
    the ship code review — type-design agent.) The match is exhaustive over
    `AlgoParams`, so a future algorithm variant fails at compile time.
  - `AlgoParams::algorithm() -> Algorithm` kept (CLI uses it to derive the
    algorithm for the JSON report from the param set alone).
- **Touched `algo/density.rs`:** `Density` struct now has `density: DensityParams`
  + `print: PrintParams` (was `params: DensityParams`). `algo-density` fills the
    `convert` body and consumes both fields.
- **Notes for dependent tasks:**
  - `algo-simple` / `algo-density`: just implement `Converter::convert` on the
    existing `Simple` / `Density` structs; the field shapes are fixed (`Simple.params`,
    `Density.density` + `Density.print`). Don't widen the trait — push new tone
    controls into the param structs.
  - `cli-framework`: parse `--algorithm` via `Algorithm::from_str` (maps unknown →
    `Usage` for you); assemble an `AlgoParams` for the chosen algorithm and pass it
    to `algo::build`. `Algorithm` serializes lowercase for the JSON report/recipe.
- **Verify:** `cargo build`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` all clean; `cargo test` 13/13 (6 new: `from_str` ok + unknown
  → exit 2, default = density, lowercase serialize, object-safe boxed call, `build`
  for both algorithms, `build` mismatch → exit 2). Object-safety proven by a test
  `Identity` converter exercised through `Box<dyn Converter>`.


## simple
**Status:** done
**Updated:** 2026-07-12

- Goal: channel-inversion baseline converter (debug / B&W) with white balance and
  black/white points.
- **Done.** `src/algo/simple.rs` implements `Converter::convert` on `Simple`. It's
  the only file changed — `SimpleParams`' knobs (`invert_white_balance`,
  `clip_low`, `clip_high`) were already fully wired by `cli-framework` (recipe
  struct in `types.rs`, `SimpleOverrides` + merge arm + `validate` checks in
  `cli.rs`), so **no new knobs** were added and no four-spot wiring was needed.
- **Algorithm (pure, per channel, linear working space):**
  1. neutralize the film base — `normalized = value / base[c]` (removes the
     orange-mask multiplicative cast; an unexposed base pixel → 1.0);
  2. invert — `positive = 1 - normalized`;
  3. white balance — `* invert_white_balance[c]`;
  4. black/white points — linear remap `(x - clip_low) / (clip_high - clip_low)`.
  A neutral base `[1,1,1]` makes step 1 inert, giving the pure `1 - v` reference.
  No density-domain math (log/exp) — that's what distinguishes `density`.
- **Decisions:**
  - **Base neutralization is a divide, using the pipeline-provided `FilmBase`** —
    the task spec's step 1 ("optional normalize against base") and design-spec
    §7.1's "border neutralization". It reuses the existing film-base knobs
    (`--film-base`/`--base-region`/`--auto-base`); "optional" is expressed by a
    neutral base being inert, not by a new flag.
  - **No clamping** anywhere in the stage — output f32 may fall outside `[0,1]`
    (HDR/scene-referred); clamping is the u16 encoder's job (CLAUDE.md clamp
    boundary). Locked by `does_not_clamp_out_of_range_values`.
  - **rayon** `par_chunks_exact(3).flat_map_iter(..).collect()` — per-pixel
    independent, and rayon's ordered collect keeps it deterministic. `rgb.len()`
    is a multiple of 3 (a `LinearImage` invariant), so every chunk is one triple.
  - **IR plane carried through untouched** (`image.ir.clone()`), per Step-1 rule.
- **Review loop (pr-review-toolkit, 4 agents parallel + 1 confirmation round):**
  All four (code / silent-failure / tests / comments) converged on **one**
  important finding: the original `convert` doc claimed `cli::validate` guarantees
  a positive/finite `base` so the divide can't hit zero — **true only for
  `FilmBaseSource::Explicit`.** For `Region`/`Auto` the base is runtime-estimated
  by `film_base::estimate`, which has no positivity guarantee (a `--base-region`
  over the dark holder → `percentile` returns `0.0`), so `value / 0.0` would emit
  silent `inf`/`NaN` — a "quietly wrong image", violating fail-loudly.
  - **Fix (kept inside this task's file):** `convert` now guards the base up front
    — any channel that isn't finite-and-positive → `NcError::Other` (exit 1) with
    an actionable message (pass `--film-base` / point `--base-region` at the
    rebate). This stage is the first to divide by the base, so the guard is a
    *first* validation of a runtime-derived value, not a redundant re-check of a
    CLI-validated one (consistent with `film_base.rs`'s own defense-in-depth).
    Doc comment corrected to attribute each guarantee to the right layer.
  - Also added, per the test reviewer: `applies_base_then_invert_then_wb_then_clip_in_order`
    (all four ops active with distinct per-channel values — catches a step
    reorder that the one-op-at-a-time tests miss) and
    `parallel_path_preserves_sample_order` (large multi-chunk image, position-
    dependent samples — pins the rayon-collect ordering).
  - Confirmation re-review came back clean (no remaining/new important issues).
- **Notes for dependent tasks:**
  - **`pipeline-orchestration`:** `Simple::convert` can now return an error
    (degenerate base) as well as `LinearImage::new` failures — propagate its
    `Result`, don't `unwrap`. Exit 1 on a degenerate estimated base.
  - **`algo-density` (follow-up, not fixed here):** `density` will also divide by /
    take `log10` of the base (`D = -log10(scan/Dmin)`) and needs the **same base
    guard**; its `convert` is still a `todo!()` stub, so there's no live gap today.
  - **`film-base-estimation` (recommended follow-up, out of this task's scope):**
    the deeper fix is for `film_base::estimate` to reject a non-positive/non-finite
    estimated base loudly at the point it's born (beside its existing uniformity /
    brighter-than-interior gates), which would make the base valid for *every*
    consumer, not just `simple`. Left to that task rather than editing its
    completed file from here.
- **Verify:** `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, `cargo test` all clean. Full suite **87/87** (11 new
  `algo::simple` tests: inversion, base neutralization divides-before-invert, WB
  scaling, clip endpoint remap, combined-ordering, no-clamp passthrough, IR
  present/absent, dimension preservation, parallel order, degenerate-base error).
- **2026-07-12 — closed out.** Manual review approved; shipped via `/ship`
  (gates re-run green, CLAUDE.md gained the film-base guard gotcha, PR opened
  from branch `algo-simple`). The notes above for `pipeline-orchestration` /
  `algo-density` / `film-base-estimation` stand.


## density
**Status:** done
**Updated:** 2026-07-12

- Goal: density-domain converter (Cineon/negadoctor style) with separate density
  and print-render sub-stages; the default algorithm.
- **Done.** `src/algo/density.rs` implements the `density` converter as two pure,
  independently-testable sub-stage fns composed by `Converter::convert`:
  - `to_density(image, base, &DensityParams) -> DensityImage` — stages 1–2.
  - `render(&DensityImage, density_gamma, &PrintParams) -> LinearImage` — stages 3–4.
  - `DensityImage` is the algo-internal intermediate (corrected density + carried
    IR + dims), `pub(crate)`, no validated constructor (its length invariants hold
    by construction from a validated `LinearImage`).
- **Exact equations chosen (per channel `c`), for reproducibility:**
  1. transmission → density: `D_c = -log10(max(scan_c, EPS) / base_c)`, `EPS = 1e-6`.
  2. density correction: `D'_c = density_scale_c · D_c + density_offset_c`.
  3. density → positive: `lin_c = 10^(density_gamma · D'_c)`.
  4. print render: `lin_c = white_balance_c · 2^print_exposure · lin_c − black_point`,
     then per-channel highlight soft-clip.
  - **Highlight soft-clip:** identity for `x ≤ 1.0` (nominal display white) or
    `amount ≤ 0`; above white, `out = 1 + amount·(1 − e^(−(x−1)/amount))`, an
    exponential knee asymptoting to `1 + amount`. `amount = highlight_compress`.
    The `1.0` threshold is a documented anchor (definition of "highlight"), not a
    hidden knob — the exposed control is `highlight_compress`.
  - **Orange-mask compensation is structural:** dividing by the *per-channel* base
    lands an unexposed sample on `D = 0` in every channel, so a neutral patch stays
    neutral with default params; `density_offset`/`density_scale` trim the residual
    per-channel balance/contrast.
- **Key decision — polarity sign fix (deliberate deviation from the task-file /
  design-spec §7.2 sketch).** The sketch wrote stage 3 as `10^(−D'·gamma)`. With
  `D = -log10(scan/base)` (which is `≥ 0` and *grows* with the film's optical
  density: base = scene black at `D=0`, dense negative = scene highlight at large
  `D`), that formula yields `scan/base` — i.e. the original **negative** — not a
  positive. A true positive must brighten as `D` grows, so stage 3 uses
  `10^(+gamma·D')`. **Verified against darktable `negadoctor`'s source** (via
  WebFetch): its print output increases with film density (denser negative →
  brighter print), confirming the `+` sign. Guarded by
  `convert_is_positive_polarity_denser_is_brighter` so a regression to the `−` sign
  fails the build.
- **No new knobs.** All params consumed (`density_scale/offset/gamma`,
  `print_exposure/black_point/white_balance/highlight_compress`) were already wired
  across the four coupled spots by `algo-interface` + `cli-framework`, so no
  `cli.rs`/`types.rs` param additions were needed — only a validation tightening
  (below).
- **`cli.rs` change (validation only):** `--highlight-compress` now must be `>= 0`
  (was finite-only). A negative value is silently a no-op in the soft-clip, so it
  now fails loudly at the CLI boundary (exit 2) per the "no silent no-op knob" rule.
- **Fail-loudly hardening (from review):**
  - `Density::convert` guards the film base via `check_base` (finite & `> 0` per
    channel, else `NcError::Other`/exit 1). The CLI validates an *explicit* base,
    but an **auto/region-estimated** base is never CLI-checked and could be `0`
    (e.g. a `--base-region` over a black holder) → division by zero → a silently
    black image. Guarded at the base's consumption point instead.
  - Non-finite scan input (`NaN`/`±inf`) propagates as `NaN` density (not laundered
    by the `EPS` floor), and the soft-clip passes non-finite through unchanged, so
    `io::encode`'s non-finite counter still surfaces corrupt/overflowed values. The
    `EPS` floor applies only to *finite* zero/negative/denormal transmission.
  - `render` builds its output via `LinearImage::new(...).expect(...)` (O(1) length
    checks) so a future invariant regression panics loudly instead of minting a
    malformed image.
- **Output is scene-referred / HDR.** With neutral defaults the base maps to `1.0`
  and exposed detail sits above it; nothing is clamped here (per the project rule —
  clamping is the u16 encode's job, which counts/report clips). Fit to a display
  range with a negative `--print-exposure` and/or `--black-point`, or keep the HDR
  range via `--out-depth f32`.
- **Notes for `pipeline-orchestration`:** call `algo::build(AlgoParams::Density{..})`
  and `Converter::convert` as usual; `convert` can now return `NcError::Other` when
  the resolved/estimated film base is invalid — surface it as a normal pipeline
  error. The density-domain default is intentionally exposure-hot (base → 1.0);
  when wiring `inspect`/reports, remember output may exceed `[0,1]` (expected, HDR).
- **Verify:** `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, build,
  and `cargo test` all clean — full suite **95/95** (21 new density tests + a cli
  validate case). Density tests cover: `-log10` ratio, per-channel/orange-mask base,
  scale-then-offset order, epsilon floor on finite zero/negative, non-finite scan
  propagation, IR carry-through (both sub-stages + convert), the `10^` curve, gamma
  exponent, wb→exposure→black order, soft-clip (disabled/below-white/rolloff/bounded/
  non-finite pass-through), soft-clip routed through `render`, composition
  (`convert == render∘to_density`), positive polarity (denser → brighter), neutral
  patch stays neutral, default output finite/no-blow-up, and the base guard
  (zero/negative/NaN/inf → error).
- **Review:** ran `pr-review-toolkit:review-pr` (code-reviewer, silent-failure-hunter,
  type-design-analyzer, pr-test-analyzer) — 2 rounds.
  - Round 1 findings fixed: negative `--highlight-compress` no-op → CLI reject;
    NaN/inf scan laundering → propagate NaN; zero-base silent-black → `check_base`;
    `pub` → `pub(crate)` + validated-constructor in `render`; test gaps (non-finite
    input, non-tautological soft-clip-in-render, no-blow-up) → added.
  - Round 2: code-reviewer clean; silent-failure-hunter flagged `soft_clip` still
    masking `+inf` → `1+amount` under compression → fixed with the `!x.is_finite()`
    guard + test. Re-ran gates: clean.
  - Minor/dismissed: `check_base` uses exit-1 (`Other`) rather than exit-4
    (`Unsupported`) for a bad *estimated* base — a defensible judgment call, kept
    (explicit bad base is already exit-2 at the CLI).
- **2026-07-12 — closed out.** Manual review approved; shipped via `/ship` (gates
  re-run green, PR opened from branch `algo-density`). **Follow-up for the spec:**
  design-spec §7.2's stage-3 sketch (`10^(−D'·gamma)`) has the polarity bug
  described above — correct it (and design-spec.html together) to `10^(+gamma·D')`.
- **2026-07-13 — PR-review follow-ups.** From bot review on the PR: `render` now
  consumes its `DensityImage` (in-place transform, IR moved not cloned); film-base
  transmissions are bounded to `(0, 1]` at both the CLI (`--film-base`, exit 2) and
  `check_base` (estimated/recipe base, exit 1) — a `90`-for-`0.90` typo previously
  blew out silently. **Deferred design finding (for `pipeline-orchestration` /
  spec):** with default params the render maps scene black (base) to `1.0` and all
  detail *above* it, so the default u16 encode clips the whole image (loudly, via
  the clip report, but still unusable). Needs a display-range anchor — e.g. a
  Dmax-style white anchor or different default `print_exposure`/`black_point` —
  decided at the spec level (§7.2/§9 defaults) alongside the polarity correction.


## dmax-white-anchor
**Status:** done
**Updated:** 2026-07-13

- Goal: anchor scene white (Dmax) in the density render so default u16 output
  fills the display range instead of clipping (PR #12 review finding; NLP
  comparison priority 1). Includes the design-spec §7.2 polarity correction.
- **Done.** The `render` sub-stage (`src/algo/density.rs`) now renders density
  relative to a display-white anchor `Dmax`; `to_density` is untouched and the two
  sub-stages stay separate. Full CI gate clean; suite **122/122**.
- **Exact formula + chosen form (for reproducibility):**
  - Stage 3 is now `lin_c = 10^(density_gamma · (D'_c − Dmax))`.
  - **Gain form (chosen):** this factors as `10^(γ·D') · 10^(−γ·Dmax)`, so the
    constant `anchor_gain = 10^(−γ·Dmax)` is **folded into the stage-4 exposure
    gain**: `exposure_gain = anchor_gain · 2^print_exposure`. Picked over
    subtracting `Dmax` inside the exponent because the anchor and `print_exposure`
    are both multiplicative scalars — folding makes the bit-exactness guarantee
    trivial (see below) and keeps the per-pixel hot loop one multiply.
  - **Auto percentile:** `AUTO_DMAX_PERCENTILE = 0.995` (99.5th) of the *finite*
    corrected densities, **scalar/pooled across all channels** (a per-channel
    anchor would double as color correction — deferred to `auto-neutral-wb`).
    Nearest-rank via `select_nth_unstable_by(round((n−1)·p), f32::total_cmp)`
    (O(n); the order-statistic value is tie-order-independent ⇒ deterministic).
    Non-finite densities are filtered out first; empty/all-non-finite ⇒ `0.0`
    (neutral gain 1.0, not a panic). 0.995 catches genuine scene white while
    ignoring the top ~0.5% (specular sparkle / dust / hot pixels).
- **Knob shape (one enum, per §9 conventions):** `DmaxSource { Auto (default) |
  Explicit(f32) | None }` in `types.rs`, recipe key **`density.dmax`** (sits beside
  `density_gamma` in `DensityParams`, and like `density_gamma` is applied in the
  render sub-stage — that's why it lives under `density.*`, not `print.*`).
  Serializes `"auto"` / `{"explicit":<d>}` / `"none"`, mirroring `FilmBaseSource`.
  CLI: mutually-exclusive `--d-max <d>` / `--auto-d-max` / `--no-d-max` (clap
  `conflicts_with_all`, dedicated `DmaxOverrides` group like `FilmBaseOverrides`).
  Four coupled spots all wired: `DmaxOverrides` field + merge arm + `validate`
  (explicit d-max must be finite & `> 0`) + recipe field, each with a test.
- **Bit-exact `None` guarantee (HDR f32 workflows depend on it):** `DmaxSource::
  None` ⇒ `resolve_dmax` returns `None` ⇒ `anchor_gain` returns the literal `1.0`
  ⇒ `exposure_gain = 1.0 · 2^print_exposure`, which is `2^print_exposure`
  bit-for-bit in IEEE-754, and the per-pixel arithmetic is otherwise unchanged.
  Pinned by `none_anchor_is_bit_exact_with_pre_anchor_render`, which recomputes the
  pre-anchor expression and asserts `assert_eq!` on f32 (not an epsilon).
- **Default is now `Auto`** — this deliberately changes the default `density`
  output from scene-referred (base → 1.0, everything above) to display-range-
  filling (scene white → ≈1.0). That is the whole point of the task (closes PR #12's
  "default u16 clips the whole image"). Verified on the real-scan fixture
  (`tests/fixtures/hdr-48bit.tif`) via a throwaway `#[ignore]` probe (removed):
  default `Auto` u16 clipped fraction **0.49%** (spot highlights only) vs
  **99.9996%** with `--no-d-max`; resolved Dmax ≈ 1.087.
- **Resolved anchor rides back for the report:** the `Converter` trait gained a
  **defaulted** `convert_reported(&self, image, base) -> Result<(LinearImage,
  ConvertReport)>` (`algo/mod.rs`); `ConvertReport { dmax: Option<f32> }`. `Density`
  implements the real work in `convert_reported` and has `convert` delegate to it
  (`.0`); `simple` inherits the default (no diagnostics). This is a *diagnostics
  output* channel (analogous to `EncodeReport`), not a control knob, so it doesn't
  reopen the "don't widen the trait for controls / associated-Params breaks
  object-safety" decision — `Box<dyn Converter>` still works.
- **Spec updated (md + html together):** §7.2 stage-3 corrected to `10^(+γ·D')`
  (was the ambiguous "exponential back-transform"; polarity bug per the
  `algo-density` note), plus new polarity + Dmax-anchor prose; §9 density-stage
  gained the `--d-max`/`--auto-d-max`/`--no-d-max` keys under `density.dmax`.
- **Review (pr-review-toolkit, 5 agents parallel):** code-reviewer, silent-failure,
  type-design, tests, comments.
  - code-reviewer: **no findings at threshold** — confirmed bit-exactness,
    determinism, four-spot wiring, fail-loud all sound.
  - silent-failure-hunter, 2 MEDIUM — both analyzed and **dismissed with rationale
    (not code-changed):** (1) "Auto anchor can be non-positive → brightens" — this
    is *correct* display-fill behavior for a dim frame (bring near-white content up
    to 1.0); the explicit-path positivity guard exists for *typo* protection on user
    input, whereas Auto is a trusted deterministic measurement, so the asymmetry is
    intentional. (2) "pathological `--density-gamma`×`--d-max` underflows gain to 0 ⇒
    all-black finite image the encoder backstop can't see" — reachable only with
    absurd inputs, and in most such cases `10^(γ·D')` overflows to `+inf` first ⇒
    `inf·0 = NaN` ⇒ *is* caught by the encoder's non-finite counter; the narrow
    all-black-finite edge is best surfaced as an orchestration warning (see note
    below), not speculative clamping in the pure stage.
  - type-design: clean (DmaxSource is a textbook "one enum, not parallel fields",
    defaulted `convert_reported` is a sound object-safe diagnostics channel).
  - tests: added 5 (nearest-rank precision on distinct values, Auto→render
    end-to-end scene-white→1.0, anchor×print_exposure composition at a known value,
    scalar-pooled-across-channels guard, nested `density.dmax` recipe parse).
  - comments: accurate; reworded the `Auto` doc ("no `--d-max` flag" → "none of the
    three dmax flags").
- **Notes for `algo-sigmoid`:** reuse this anchor — the S-curve tone map wants the
  same "scene white → display white" reference. The resolved `Dmax` (frame-local
  scene-white density) is the natural shoulder anchor; consume it via the same
  `DmaxSource`/`convert_reported` path rather than re-measuring, and keep the
  `None`-is-bit-exact escape hatch for HDR.
- **Notes for `pipeline-orchestration`:** call `Converter::convert_reported` (not
  `convert`) so `ConvertReport.dmax` reaches the JSON report — add it beside the
  film base. **Nothing consumes `convert_reported` yet** (only tests), so wire it
  or the reporting channel stays a no-op. Also consider a report warning when the
  resolved anchor gain is degenerate (underflow → ~all-black, or overflow) since the
  encoder's clip/non-finite counters can't see an all-zero-but-finite image
  (silent-failure Finding 2). `convert`/`convert_reported` can still return
  `NcError::Other` on a bad estimated base (unchanged from `algo-density`).

- 2026-07-14 — **PR #17 review fixes.** (1) The anchor is now applied in the
  exponent (`10^(γ·(D'−Dmax))`) instead of a folded `10^(−γ·Dmax)` gain — the
  factored form overflowed f32 when `γ·D'` alone exceeded the pow10 range (e.g.
  γ=5 with EPS-clamped D'≈8 rendered scene white as inf); regression test added.
  `None` stays bit-exact (`d − 0.0 == d`). (2) The Auto anchor now measures a
  deterministic strided sample capped at 2^20 values (~4 MB transient) instead
  of copying the full density buffer — stride derived from length only, bumped
  off multiples of 3 so interleaved RGB isn't single-channel biased; small
  images are unaffected (stride 1). Spec §7.2 sentence updated to match.
- 2026-07-14 — **closed out.** Manual review approved; shipped via `/ship`
  (gates re-run green: 122 tests; branch rebased onto post-docs main). Unblocks
  `algo-sigmoid`. Merge-time follow-up with `pipeline-orchestration` stands:
  wire `convert_reported`'s `ConvertReport.dmax` into the JSON report.


## sigmoid
**Status:** done
**Updated:** 2026-07-14

- Goal: third converter — S-curve (H&D / paper response) tone mapping in density
  space with toe/shoulder control (design-spec §12 roadmap; NLP comparison
  priority 2).
- **Done.** New `Converter` impl in `src/algo/sigmoid.rs`, selected via
  `--algorithm sigmoid`. Reuses `to_density` (stages 1–2), the resolved `Dmax`
  anchor (`resolve_dmax`), and the film-base guard (`check_base`) from
  `density`; stage 4 was factored out of `density::render` into a shared
  `render_print` and is reused unchanged. Full CI gate clean (see the final gate
  run at the end of this section for the current suite total).
- **Exact formula (the concrete, documented curve — spec §7.3):** per channel,
  in log₁₀-output space, with `A = Dmax` (resolved anchor) and `c = contrast`:
  ```text
  t = c·(D' − A)                                 the density algorithm's straight line
  F = −c·A                                       paper-black floor (the line's value at D' = 0)
  p = F + toe·log10(1 + 10^((t−F)/toe))          toe  FIRST: soft-max with F   (skipped if toe = 0)
  v = p − shoulder·log10(1 + 10^(p/shoulder))    shoulder LAST: soft-min with 0 (skipped if shoulder = 0)
  lin = 10^v
  ```
  Chosen over a closed-form logistic because the task requires **reduction to
  the straight line as toe/shoulder → 0** — with both `0` the knee branches are
  skipped and the expression is *bit-identical* to density's stage 3
  (`10^(c·(D'−A))`), pinned by an `assert_eq!` end-to-end test. Properties (all
  test-pinned): strictly monotonic; **white asymptote `1.0` reached from strictly
  below with the guarantee `lin ≤ 1.0` for every finite density when
  `shoulder > 0`** (so the default u16 encode cannot clip highlights — verified on
  the real-scan fixture: density default clips 3 429 samples / 0.49 %, sigmoid
  clips **0**, same resolved anchor 1.6281); black asymptote `≈ 10^(−c·A)` (exact
  when `shoulder = 0`). `shoulder = 0` gives no highlight roll-off — highlights
  follow the toe-shaped line and can exceed `1.0` like `density`.
- **Knee order is load-bearing (PR-review fix, 2026-07-14).** Two independent
  reviews (Codex P2 + pr-review comment-analyzer) caught that the original order —
  shoulder first, **toe last** — let the toe soft-max lift the white asymptote to
  `(1 + 10^(−c·A/toe))^toe > 1`, which *overshoots and clips* for a small anchor
  (e.g. `--d-max 0.1`, default `toe 0.2`, `c 1` → ≈ `1.056`), defeating the headline
  "shoulder ⇒ no highlight clip" guarantee. **Fix: reorder to toe-first,
  shoulder-last**, so the soft-min-with-white is the final op and nothing can lift
  it. This trades a raised white asymptote for an *imperceptibly* lowered black
  floor (the shoulder now nudges the floor a hair below `10^(−c·A)` — negligible).
  The shoulder is written in the **manifestly-bounded** form
  `−shoulder·log10(1 + 10^(−p/shoulder))` (algebraically equal to
  `p − shoulder·log10(1 + 10^(p/shoulder))` but a negative × non-negative, so
  `v ≤ 0` in *f32 by construction* — the subtraction form rounded a hair above 0,
  `10^v = 1.0000006`, which would clip). Regression tests: a curve-level sweep over
  small-anchor / low-contrast / toe≫shoulder param sets asserting `lin ≤ 1.0`, and
  an e2e `--d-max 0.1` asserting `clipped_high == 0`. Bit-exact `toe=shoulder=0`
  reduction preserved (both branches still skipped).
- **Numerical gotchas (recorded for future density-domain curves):**
  - `log10(1 + 10^y)` must be the stable `max(y,0) + log10(1 + 10^(−|y|))` —
    the naive form overflows `10^y` at `y ≳ 38` (e.g. any tiny-but-nonzero knee
    width) and would send the knee to `−inf` instead of its asymptote.
  - Rust's `f32::max(NaN, 0.0)` returns `0.0` (NaN-launder trap!) — the stable
    form still propagates NaN via its second term; pinned by a test. NaN
    density → NaN output for `io::encode`'s non-finite counter, per the
    `SCAN_EPSILON` convention in `density.rs`.
- **Refactor first (pure, bit-exact):** `density::render` used to fuse stage 3
  (`10^(γ·(D'−Dmax))`) with stage 4 (WB → `2^exposure` → black point →
  soft-clip). Stage 4 is now `render_print(density, tone, print)` with the
  stage-3 curve injected as a per-sample closure — same arithmetic order, so
  the existing value-pinning render tests (incl.
  `none_anchor_is_bit_exact_with_pre_anchor_render`) double as the bit-exact
  regression suite; all pass unchanged. The two sub-stages stay separately
  parameterized (core fidelity rule).
- **Param/knob shape (four coupled spots wired, each with a test):**
  - `SigmoidParams { contrast (>0, default 1.0), toe (≥0, default 0.2),
    shoulder (≥0, default 0.2) }` in `types.rs`; recipe section `sigmoid.*`
    (`sigmoid.contrast` / `sigmoid.toe` / `sigmoid.shoulder`).
  - CLI flags `--sigmoid-contrast` / `--sigmoid-toe` / `--sigmoid-shoulder`
    (`SigmoidOverrides` in `cli.rs`) — prefixed for namespacing; recipe keys
    drop the prefix (like `--d-max` ⇒ `density.dmax`).
  - `merge` arms + merge test; `validate`: contrast finite `>0`, knee widths
    finite `≥0` (a negative width would silently read as "knee off").
  - `ResolvedConfig` gained the `sigmoid` section; `AlgoParams::Sigmoid
    { density, sigmoid, print }`; `stages::algo_params` takes `&SigmoidParams`.
- **Anchor decision:** the S-curve is anchored on `[0, Dmax]` (white knee and
  black floor both derive from it), so it **requires** an anchor — reused via
  the same `DmaxSource`/`resolve_dmax`/`convert_reported` path as `density`
  (one measurement, reported as `report.dmax` identically). `sigmoid` +
  `dmax = none` is rejected: `validate` (Usage, exit 2) for the CLI/recipe
  path, plus a fail-loud backstop inside `convert_reported` (exit 1) for
  programmatic construction. The `None`-is-bit-exact HDR escape hatch stays a
  `density`-algorithm feature (documented in §9).
- **`density_gamma` is ignored under sigmoid** (it parameterizes the straight
  line the S-curve replaces; `sigmoid.contrast` is the analogue). Because the
  rest of the `density.*` section *is* consumed (scale/offset/dmax), a
  customized-but-ignored gamma is the silent-no-op trap — `run_convert` emits a
  report warning (which `--strict` promotes) when `algorithm = sigmoid` and
  `density_gamma != 1.0`. Fully inert sections (e.g. `simple.*` under density)
  stay silent as before — the warning is only for the partial-consumption case.
- **`--highlight-compress` interaction (documented, not disabled):** the
  shoulder compresses in density space before exposure/WB; the print soft-clip
  compresses in linear space after them. They compose; with the shoulder on and
  neutral print params nothing exceeds `1.0`, so the (default-off) soft-clip
  simply never engages.
- **Real-scan spot check** (committed fixture, throwaway `#[ignore]` probe,
  removed): contrast sweep 0.7 / 1.0 / 1.5 → p50 0.373 / 0.245 / 0.121 and
  mid-separation (p75−p25) 0.235 / 0.227 / 0.176 — midtone contrast visibly
  adjustable; max sample 0.926 / 0.944 / 0.965 — highlights roll off smoothly,
  never reaching 1.0 (no hard clip); shadow separation (p05−p01) stays positive
  at every contrast.
- **Docs:** design-spec **md + html together** — new §7.3 (curve, anchors,
  reduction, anchor requirement, gamma/soft-clip interactions), §6 diagram and
  §2/§12 algorithm lists, §8 recipe-section list, §9: `--algorithm` gains
  `sigmoid`, density-stage header notes the sharing, `--no-d-max` marked
  density-only, new "Sigmoid stage" section with the three knobs.
- **Notes for dependents:** `render_print` is the shared stage-4 entry point
  for any future density-domain curve (power-law roadmap item) — inject the
  curve as the `tone` closure, keep `resolve_dmax` as the single anchor source.
  `auto-neutral-wb` / `regional-color-balance` operate on `density.*`/`print.*`
  and therefore apply to `sigmoid` runs unchanged.
- **Review (pr-review-toolkit, parallel panel):** code-reviewer, comment,
  test-coverage, type-design, silent-failure. Two findings fixed:
  - **(type-design/silent-failure, correctness):** the `Auto`-resolved anchor was
    only checked `Some(_)`, not positive. `auto_dmax` can return `0.0`
    (empty/all-non-finite) or a *negative* percentile when a wrong film base
    pushes most corrected densities below zero; with `anchor ≤ 0` the toe floor
    `10^(−contrast·anchor) ≥ 1`, so every sample renders above display white — a
    quietly-wrong all-white image. Fixed: `convert_reported` now guards
    `resolved.filter(|a| a.is_finite() && *a > 0.0)` and errors loudly (exit 1),
    covering the `none` programmatic path *and* the degenerate-`Auto` case (the
    CLAUDE.md film-base gotcha, mirroring `simple.rs`). Tests added
    (`convert_rejects_a_non_positive_auto_anchor`: scan>base → negative percentile,
    plus a smuggled negative `Explicit`).
  - **(test-coverage, sev-6):** the `density_gamma`-ignored-under-sigmoid warning
    had no coverage. Added an e2e (`sigmoid_warns_when_density_gamma_is_ignored`)
    asserting the warning fires for sigmoid+custom gamma, is absent for
    sigmoid+default and density+custom, and `--strict` promotes it to exit 1.
  - Re-ran code-review after the fixes: **clean, no findings** (bit-exact refactor,
    four-spot wiring, exit codes, docs md+html sync all confirmed). Gates green:
    fmt clean, clippy clean, build clean, **152 unit + 21 e2e** tests pass.
- **Rebased onto `origin/main` 3c7f5bd** (post-#20/#21/#22). Conflicts resolved:
  - `src/types.rs`, `src/cli.rs`: #20 renamed the output knob `--out-depth
    u16|f32` → `--output-hdr` bool (`OutputParams.hdr`; `OutDepth` is now internal,
    dropped from the cli import). Adjusted my sigmoid test in `pipeline/stages.rs`
    (`out_depth: OutDepth::F32` → `hdr: true`) — the only code touch the rebase
    needed. Kept `output_hdr_bool_drives_depth` (upstream) alongside my
    `SigmoidParams` / `algorithm_serializes_sigmoid_lowercase` tests; dropped the
    now-obsolete `out_depth_serializes_lowercase`.
  - `docs/TASKS.md`: kept upstream's new `dmax-reference` task line and marked
    `algo-sigmoid` `[x]`.
  - `docs/design-spec.md`+`.html` §9/§12: combined upstream's `--output-hdr`
    wording and the `bw-support` roadmap graduation with my §7.3/sigmoid-stage
    additions.
  - Confirmed no sibling-agent content leaked (initial bare `stash pop` grabbed a
    sibling's stash off the **shared** worktree stash stack; recovered by
    `reset --hard origin/main` then re-applying my own stash by immutable SHA).
- **New-design review:** the new (unstarted) `dmax-reference` task will change the
  *default acquisition* of `Dmax` (per-frame auto → roll-fixed reference) and
  demote `--auto-d-max`, but explicitly **keeps the anchor a positive scalar in
  density units and keeps the render machinery** — so the sigmoid anchor contract
  (positive scalar via `DmaxSource`, `--no-d-max` rejected, degenerate-Auto guard)
  is unaffected. No sigmoid change needed now; when `dmax-reference` lands the
  sigmoid default path simply consumes the fixed reference anchor (still positive).
- Post-rebase gates: fmt/clippy/build clean; **155 unit + 21 e2e** tests pass
  (unit count rose from the new base's added tests).
- **Second review round (2026-07-14, Codex + pr-review 5-agent).** Primary
  correctness fix = the knee-order/white-overshoot bug (documented above). LOW
  items folded in:
  1. **Contrast upper bound** — `SIGMOID_CONTRAST_MAX = 50.0` (in `sigmoid.rs`),
     enforced in `validate`. An extreme slope collapses the S-curve into a hard
     threshold whose knees launder the blow-out into a finite two-level image that
     trips *neither* the clip nor the non-finite counter (density surfaces `+inf`);
     the cap closes that silent-destruction hole. Test + §9 docs (md+html) updated.
  2. **`debug_assert!`** at the top of `s_curve` (`contrast > 0`, `toe/shoulder ≥ 0`)
     — defense for the pure stage that otherwise trusts CLI-validated inputs.
  3. **Contrast-backstop comment** in `convert_reported` explaining the asymmetry
     (the anchor has a runtime guard; `contrast` is config-only, fully
     CLI-validated, so no runtime re-check — the debug assert covers programmatic
     callers).
  4. **Anchor error now names the true cause** (`anchor_error` helper): `none` →
     disabled-anchor message; `Some(≤0)` with no finite densities → corrupt/
     non-finite input (not the base); `Some(≤0)` with finite densities → wrong
     base. Test `anchor_error_distinguishes_corrupt_input_from_bad_base`.
  5. **Sigmoid recipe round-trip e2e** with non-default toe/shoulder
     (`sigmoid_sidecar_recipe_round_trips_through_recipe_in`) — guards the
     four-spot serialization/merge for the sigmoid section.
  Deferred (optional nice-to-haves): shoulder↔`--highlight-compress` composition
  test and a sigmoid e2e determinism assertion — the shared `render_print`/anchor
  paths are already determinism- and composition-tested via the density suite and
  the existing sigmoid round-trip; judged low marginal value. Final gates green
  (see the ship report).
- **Third review round (2026-07-14, Codex + pr-review 5-agent).** Both reviewers
  converged on one theme: the manifestly-bounded shoulder that fixed the white
  overshoot also *silently launders extreme upstream inputs* into a clean in-range
  sample, contradicting the fail-loud / non-finite-counter discipline. Two
  complementary MUST-FIXes:
  1. **Non-finite propagation in `s_curve`.** A non-finite corrected density
     (`NaN`/`±inf`, e.g. an accepted-but-huge `--density-scale`/`--density-offset`
     overflowing `to_density`) was mapped by the bounded knees to `10^v = 1.0`,
     hiding the fault (`density` surfaces it as `+inf`). Fixed: `s_curve` now
     returns the input `d` verbatim when `!d.is_finite()` **before** the knees, and
     also surfaces a finite-`d`→non-finite-`p` knee-math overflow (capped contrast
     × huge offset). So `10^v ≤ 1.0` is guaranteed only for *finite* stage-3
     output; a non-finite sample rides through to `io::encode`'s counter. Bit-exact
     `toe=shoulder=0` reduction preserved (finite path untouched). Tests:
     `s_curve_propagates_non_finite` (NaN/±inf/overflow, knees on & off) and
     `convert_propagates_non_finite_scan_to_output` (a non-finite scan rides
     through the full converter). NB: a *CLI-driven* overflow e2e isn't
     constructible on the committed fixture — its corrected densities are too small
     to overflow f32 within validated param ranges (scale alone can't; a uniform
     offset overflows *all* pixels → the anchor-guard's corrupt-input branch, exit
     1) — so the converter-level test pins the path instead.
  2. **Knee-width cap.** A huge *finite* `--sigmoid-toe`/`--sigmoid-shoulder`
     (verified: `shoulder 10000` → all-black, `toe 10000` → all-white) flattens the
     image with finite in-range samples that trip no counter — the same
     silent-destruction class the contrast cap closed. Added
     `SIGMOID_KNEE_MAX = 10.0` (shared for both; ~11× the ~0.05–0.9 photographic
     range and ~5× a scan's full density range, so it rejects only degenerate
     widths), enforced in `validate` with an actionable message; §9 docs (md+html)
     updated; boundary tested (accept at cap, reject cap+1 / 10000 / +inf).
  SHOULD/LOW also done: hardened the white-ceiling test with an FP-stressful corner
  (`contrast 50, shoulder 0.001`) plus `s_curve_manifest_form_beats_the_naive_subtraction_form`
  (asserts the naive subtraction form overshoots >1.0 where `s_curve` stays ≤1.0 —
  guards against a revert); `convert_requires_a_dmax_anchor` now asserts the
  `None`-specific "scene-referred" token; scoped the "clipping impossible" doc claim
  to *stage-3 output under neutral print params* (the print stage can lift samples
  back above 1.0); refreshed the stale headline test count; `anchor_error` now
  distinguishes a programmatic non-positive `Explicit` anchor from the wrong-base
  case; added a `shoulder = 0` complement test (highlights may exceed 1.0 like
  density). Deferred: shoulder↔`--highlight-compress` composition e2e (low value;
  both knobs' math is unit-tested and they compose additively in log/linear
  space). Gates green: **159 unit + 23 e2e**.
- **Final pass (2026-07-14).** Round-3 review converged (a Codex "won't compile"
  P0 was a verified false positive — destructuring `self.sigmoid` copies the Copy
  f32 fields; the crate builds). The one round-3 MEDIUM (within-cap extreme params
  posterize with no warning) is an **accepted, documented tradeoff**: the caps
  reject nonsense/degenerate-asymptote values, not aggression — no warning band, no
  tighter caps (documented at the consts in `sigmoid.rs` and in §9, md+html). Also
  added: a knees-off finite-overflow case to `s_curve_propagates_non_finite`; a
  `debug_assert!(matches!(source, DmaxSource::None))` in `anchor_error`'s `None`
  arm (pins `resolve_dmax` `None` ⟺ source `None`); a near-cap toe
  (`SIGMOID_KNEE_MAX`) case in the white-ceiling sweep; and scoped the §7.3
  "cannot clip" claim to stage-3-under-neutral-print (the print stage can lift
  samples above 1.0). Gates green: **159 unit + 23 e2e**.
- **Deferred (shared / general-robustness, NOT sigmoid-specific — do not fix under
  this task):**
  - A *tiny-positive* `Auto`/`Explicit` `Dmax` anchor passes the `> 0` guard yet is
    degenerate (renders near-black or extreme). Pre-existing and shared with the
    `density` render's anchor path (`dmax-white-anchor`); a general anchor-sanity
    follow-up, not a regression here.
  - Verifying a non-finite sample still reaches `io::encode`'s non-finite counter
    *across the lcms2 color transform* (`pipeline::color::to_output`) — a gap
    shared with `density` (both feed the same color→encode path); belongs to a
    color/encode robustness pass, not this task.


## auto-neutral-wb
**Status:** done
**Updated:** 2026-07-14

- Goal: deterministic auto white-balance estimation (gray-world / neutral-
  percentile) feeding `print.white_balance`, reported for roll reuse (NLP
  comparison priority 3a).
- **Done.** Two per-frame estimators behind the existing stage-4 slot; full CI
  gate clean (fmt / clippy `-D warnings` / build / test), suite **216 tests**
  (191 unit + 25 E2E). Rebased onto post-#27 main (the `--out-depth` → boolean
  `--output-hdr` rename #20, bw-support docs #21, roll/versioning follow-ups
  #22, auto-base inward-scan redesign #23, sigmoid tone algorithm #27). The
  auto-WB E2E test uses `--output-hdr` (the removed `--out-depth f32`).
- **Rebased onto the sigmoid refactor (#27): stage 4 is now the shared
  `render_print(density, tone, white_balance, print)`** — sigmoid fuses its
  S-curve as the `tone` map. Reconciliation: my WB change made `render_print`
  take the **resolved** `white_balance: [f32;3]` (it no longer reads
  `print.white_balance`, now a `WbSource`); the density `render` wrapper is kept
  (resolved args → `render_print`) for density + its tests. **Auto-WB now works
  for `sigmoid` too**, not just `density`: both share `render_print` and the
  print WB stage, so `estimate_wb_gains` is `pub(crate)` and `Sigmoid::
  convert_reported` runs the same two-pass (neutral analysis render → estimate →
  re-render through the slot) and reports the gains. The `validate` guard now
  whitelists `density | sigmoid` (rejects only `simple`, which has no print WB
  stage) — supporting sigmoid was *less* special-casing than restricting it.
  Also reconciled: `stages::render` now takes a resolved `&FilmBase`
  (auto-base #23 moved estimation to the orchestrator) and `stages::algo_params`
  takes 5 args (sigmoid) — both auto-merged; my WB wiring sits on top unchanged.
- Design checked against the new `roll-conversion` (auto-WB is a frame-local
  `--auto-*` mode; reported gains are the value to freeze into a roll recipe's
  `print.white_balance = {"explicit": […]}`) and `dmax-reference` (Dmax stays a
  scalar and the render machinery is unchanged, so resolving the anchor once and
  sharing it across the analysis + final passes still holds) — no code change
  needed.
- **Knob shape (the task's core decision): `print.white_balance` is now one
  source enum, `WbSource { Explicit([f32;3]) | GrayWorld | Percentile }`**
  (`types.rs`), default `Explicit([1,1,1])` (= neutral, auto off). This is a
  deliberate **recipe wire-format change**: the key serializes as
  `{ "explicit": [r,g,b] }` / `"gray-world"` / `"percentile"` (kebab-case,
  mirroring `FilmBaseSource`/`DmaxSource`), no longer a bare `[r,g,b]` array.
  Rationale: explicit-beats-auto **by source** falls out of the type — after the
  merge the variant records provenance, so `--white-balance 1,1,1` over a recipe
  auto mode means "neutral gains", never re-estimation (a value-based or
  parallel-field encoding cannot express that). Pre-release, so old sidecars
  weren't grandfathered; §9 (md + html) updated. CLI: `--white-balance R,G,B`
  vs `--auto-wb gray-world|percentile` (clap `conflicts_with`; `AutoWb`
  ValueEnum in `cli.rs`). All four coupled spots wired with tests: override
  fields, merge arm (source-precedence test included), `validate` (explicit
  gains positive; auto modes carry no value), recipe nesting test.
- **An auto mode without `--algorithm density` is a loud usage error (exit 2),
  not a silent no-op** (review finding, fail-loudly rule): only `density` reads
  `print.white_balance`, so an auto mode elsewhere would drop the requested
  estimation silently. `validate` **whitelists `density`** (`!= Density`
  errors), not blacklists `simple`, so a future third algorithm that also
  ignores the print stage fails loudly by default — the "forgotten coupled
  spot" trap (silent-failure review, MEDIUM). §9 (md + html) documents it; test
  `validate_rejects_auto_wb_with_the_simple_algorithm`. Explicit
  `print.white_balance` under `simple` stays allowed (inert, not an action
  dropped — `simple` has its own `invert_white_balance`).
- **CLI-flag coverage guard:** `every_auto_wb_source_has_a_cli_flag`
  (`cli.rs`) uses an exhaustive `match` so a future `WbSource` auto mode fails
  to compile until it is given an `--auto-wb` value — closes the type-design
  review's "recipe-only mode could ship silently" drift risk.
- **Estimators (`algo/density.rs::estimate_wb_gains`), deterministic statistics
  only:** samples come from a strided pixel walk (`AUTO_WB_MAX_PIXELS = 2^20`,
  whole-pixel stride so no channel bias), non-finite samples dropped per sample,
  each channel fully sorted (`total_cmp`) so every statistic is order-defined.
  - `GrayWorld` (≈ NLP Auto-AVG): per-channel mean of the central 98%
    (`AUTO_WB_TRIM = 0.01` per end) — the trim is frame-relative, so clipped
    speculars/dead pixels are excluded in both display-anchored and
    scene-referred (`--no-d-max`) renders. Documented weakness: a dominant
    scene color biases it (test pins this vs percentile).
  - `Percentile` (≈ NLP Auto-Neutral): per-channel nearest-rank 95th percentile
    (`AUTO_WB_PERCENTILE = 0.95`) — equalizes near-white, robust to dominant
    colors; the top 5% never enters the statistic.
  - Gains are **green-anchored** (`g = 1.0` exactly): WB corrects color, not
    exposure. Degenerate channels (all non-finite / non-positive level /
    non-finite gain) **fail loudly** (`NcError::Other`, exit 1) — never
    silently-neutral gains.
- **Estimation reads, application re-renders (the task's hard requirement).**
  `Density::convert_reported` resolves the Dmax anchor **once**, renders an
  analysis positive from a *clone* of the density buffer with a fully neutral
  print (unit gains, 0 EV, no black point, no soft-clip — so the statistics
  measure exactly the quantity the WB slot multiplies; the user's exposure
  would cancel in the ratios, black/soft-clip would distort them), estimates,
  then runs the real `render` with the resolved gains through the standard
  stage-4 slot. `render`'s signature changed to take the **resolved** anchor
  (`Option<f32>`) and **resolved** gains (`[f32;3]`) instead of the source
  enums — both passes must share one anchor without re-measuring; it returns
  just the image now (resolution moved to the caller). Explicit gains skip the
  analysis pass entirely, so the default path's per-pixel arithmetic (and
  output) is unchanged.
- **Reuse contract pinned bit-exactly:** unit test
  `auto_wb_output_is_bit_exact_with_explicit_rerun_of_reported_gains` plus E2E
  `auto_wb_reports_gains_that_reproduce_the_output_when_reused` (report gains →
  `--white-balance` → byte-identical f32 TIFF; JSON's shortest-round-trip f64
  parses back to the identical f32). Determinism test (same input ⇒ same gains
  and rgb) included.
- **Report:** `ConvertReport` and the convert JSON `Report` gained
  `white_balance: Option<[f32;3]>` — the *resolved* gains (auto-estimated or
  explicit; `None` for `simple`). Per the task decision, `nc estimate` was NOT
  extended (its contract is Dmin-only; it can't render the positive these
  statistics need — `estimate-reuse-output` territory). Note: the **sidecar**
  recipe records the auto *mode* (the run's parameters — rerunning it
  re-estimates); the frozen gains live in the *report*, by design.
- **Real-scan spot check** (committed fixtures, CLI runs, derived numbers only):
  with the guessed base `0.9,0.55,0.42` — gray-world `[1.458, 1.0, 0.542]`
  (hdr-48bit) / `[1.347, 1.0, 0.621]` (hdri-64bit); percentile
  `[1.583, 1.0, 0.494]` / `[1.543, 1.0, 0.521]`. I.e. the typical blue-heavy
  post-inversion cast is pulled down toward neutral; dmax unchanged (≈1.63 /
  ≈1.62), 0% clipping at u16.
- **Notes for dependents:**
  - `regional-color-balance`: the global gains here are a single multiplier per
    channel — they cannot fix shadow/highlight crossover; that task's
    density-weighted offsets slot into stage 2. Reuse the sampling helpers
    (`wb_channel_samples` / `trimmed_mean` / `nearest_rank`) if useful, and keep
    its knob a single source enum like `WbSource`.
  - Rebate/border pixels are *not* excluded from the statistics (no crop knob
    exists yet). They render neutral by construction (base → `D=0` in all
    channels), so they dilute gains toward 1 rather than casting them —
    deterministic and mild; revisit if a crop/region knob lands.
  - `estimate-reuse-output`: if `estimate` ever grows a WB story, the report's
    `white_balance` array is the value to make drop-in reusable.
- **Review (pr-review-toolkit, 5 dimensions):** code-reviewer clean (all four
  hard requirements verified); comments clean; tests → the auto-wb+simple
  no-op + the `--no-d-max` robustness gap (both fixed, above); type-design →
  the CLI-flag exhaustiveness guard (fixed, above) plus a *recommended*
  extraction of `render`'s three read `PrintParams` scalars out of the
  `&PrintParams` arg; silent-failure → the whitelist-vs-blacklist polarity
  (fixed) plus a LOW note that explicit `--white-balance` under `simple` is
  silently inert.
  - **Deliberately not changed (reported with reasoning):** (1) `render` keeps
    `print: &PrintParams` with `white_balance` documented-as-ignored rather than
    expanding to a 7-argument signature across ~13 call sites — one `pub(crate)`
    caller, the ignored field is documented at the signature, and the
    bit-exact-reuse contract is test-pinned; the code-reviewer did not flag it.
    (2) explicit `--white-balance` under `simple` staying inert is pre-existing,
    documented cross-algorithm-knob behavior (a *value* left unused, not a
    *computation* dropped), not a regression from this task.


## regional-color-balance
**Status:** done
**Updated:** 2026-07-17

- Goal: shadow/highlight per-channel balance (density-weighted offsets in stage
  2) to correct color crossover a global gain can't fix (NLP comparison
  priority 3b).
- 2026-07-14 — **implemented.** New pure sub-stage `regional_balance`
  (`algo/density.rs`) completing stage 2 between `to_density` and `render`:
  `D'_c = B_c + shadow_balance_c·w_lo(D̄) + highlight_balance_c·w_hi(D̄)` with
  `w_hi = smoothstep((D̄ − lo)/(hi − lo))`, `w_lo = 1 − w_hi` (complementary, so
  equal balances degenerate to a uniform `density_offset`), and `D̄` the
  per-pixel **scalar** tone = mean of the *finite* pre-regional corrected
  channels (per-channel weighting would misfire on exactly the crossover pixels;
  a NaN channel is excluded from the tone but stays NaN itself, so the encode
  non-finite counter still sees it).
- **Decisions:**
  - *Naming convention (§9):* "shadow"/"highlight" are the **positive's** tone
    regions — low corrected density (near base) = shadow, high = highlight — and
    with the positive polarity a **positive balance value brightens that channel
    in its region**. Documented in §7.2/§9.
  - *Range anchors:* new enum `BalanceRange` (`types.rs`), `Auto` (default) |
    `Explicit([lo, hi])` — one enum field, not parallel knobs. `Auto` measures
    nearest-rank percentiles **0.5 % / 99.5 %** of the per-pixel tone `D̄` over a
    deterministic strided pixel sample (cap 2^20 pixels, mirrors the `auto_dmax`
    approach; strides whole RGB triples so no channel-bias bump is needed). The
    measurement uses the same `D̄` domain the ramps consume, so non-default
    `density_scale`/`offset` can't make anchors and inputs drift. It deliberately
    does **not** anchor on the Auto `Dmax` (measured *after* stage 2 — circular).
  - *Ordering:* regional balance runs **before** `render`, so an `Auto` `Dmax`
    is resolved from the *post-balance* densities (display-white anchor stays
    consistent with what is rendered), and before print WB (stage 2 fixes the
    tone-dependent crossover; print WB the residual global cast).
  - *Neutral default is bit-exact:* `[0,0,0]` balances return before touching
    the buffer (even `+0.0` would flip `-0.0`) and skip the measuring pass;
    pinned by a bit-level test.
  - *Fail loudly:* a requested balance with an unmeasurable `Auto` range
    (uniform / all-non-finite frame) is an `NcError::Other` naming
    `--balance-range` as the recovery — never a silently skipped correction.
    Explicit ranges are CLI-validated (finite, `lo < hi`; exit 2).
  - *CLI:* `--shadow-balance R,G,B`, `--highlight-balance R,G,B` (both with
    `allow_hyphen_values` — negative offsets are the common case),
    `--balance-range LO,HI` ⊕ `--auto-balance-range` (clap-conflicting pair).
    All four coupled spots wired (overrides, `DensityParams` fields, merge arms,
    validate) + merge/recipe-nesting/conflict tests.
  - *Report:* `ConvertReport.balance_range` → report key `balance_range`
    (`[lo, hi]`, omitted when `None`) so a roll can reuse one frame's measured
    range via `--balance-range` — same reuse pattern as `dmax`.
- **Notes for dependents:** `auto-neutral-wb` — regional balance composes with
  (and precedes) print WB; if auto-WB ever wants tone context, reuse the
  measured `balance_range` from the report rather than re-measuring inside
  stage 2. `algo-sigmoid` — the sub-stage boundary is unchanged: sigmoid replaces
  the `render` tone map, not stage 2, so regional balance carries over as-is.
- 2026-07-17 — **rebased onto `algo-sigmoid` (#27) + `auto-base-redesign` +
  #24/#25/#26** (commit-WIP method). algo-sigmoid refactored `density::render`
  into a shared `render_print(density, tone, print)` and added a `sigmoid`
  converter that reuses stages 1–2 (`to_density`) and stage 4 (`render_print`).
  My `render(density, gamma, dmax, print)` wrapper kept its signature (now
  delegates to `render_print`), so `density::convert_reported` was unaffected.
  **Decision — regional balance now applies under `sigmoid` too:** the
  `shadow_balance`/`highlight_balance`/`balance_range` knobs live in the shared
  `DensityParams` and regional balance is a stage-2 op, which sigmoid shares —
  so `sigmoid::convert_reported` now calls `regional_balance` after `to_density`
  (before its anchor resolve, same post-balance-`Dmax` ordering as `density`) and
  surfaces `balance_range` in its `ConvertReport`. Without this, `--shadow-balance`
  would have been a silent no-op under `--algorithm sigmoid` (violating the
  fail-loud / no-silent-no-op rule). Pinned by three sigmoid tests
  (applies-not-noop, reports the range, and bit-exact match to `density` with
  knees off + a balance). `ConvertReport` gained `balance_range`, so sigmoid's
  `ConvertReport { dmax }` construction was updated to include it. §7.2/§9
  (both .md and .html) reconciled: the "sigmoid shares this whole section" note
  now explicitly includes the regional balance.


## negative-reconstruction-density-curves
**Status:** done
**Updated:** 2026-07-24

- 2026-07-24: Reviewed via the two-engine review-fix-loop (Codex + 5 pr-review
  lenses: quality, tests, types, silent-failure, comments). Bit-identity was
  independently proven — a reviewer re-ran the pre-refactor code at HEAD and
  matched all 9 golden configs + 4 whole-TIFF hashes bit-for-bit. Five findings
  fixed: `FilmRgbImage::from_linear` `pub(super)`→`pub(in crate::algo)` (the
  boundary invariant was crate-wide, not module-private) + corrected the
  overclaiming doc-comments; `merge_json` now handles internally-tagged
  `reconstruction`/`curve` type switches in roll per-frame overrides
  (`internally_tagged_switch`), carrying the shared roll-fixed `dmax` — matching
  the CLI `merge()` semantics; +3 golden fixtures (auto-WB × regional balance,
  auto-WB × sigmoid, auto balance-range); fixed a golden cross-ref comment; and
  corrected the stale CLAUDE.md §9 legacy-schema carve-out. Loop converged, the
  merge_json delta got a targeted re-review (sound). Rebased onto origin/main
  (past #48 HDR, #49 telemetry, #50 display-p3); clean auto-merge, gates green
  (325 unit incl. #50's tests + 86 integration). Shipped via /ship.
- 2026-07-24: CI (Linux) surfaced a non-portable golden: `tiff_hash` hashed the
  whole encoded TIFF including the embedded ICC, whose header carries
  platform-dependent bytes (Little CMS), so the macOS-captured hash failed on CI
  even though every per-pixel `f32::to_bits` golden passed there (pixels are
  bit-identical cross-platform). Retargeted it to `tiff_pixels_hash`: decode the
  written TIFF back and hash only the pixel samples + dimensions, excluding the
  ICC/container. This matches nc's actual determinism contract (byte-identity is
  per build/architecture, design-spec §8) while still pinning the encode
  quantization/layout. Test renamed to
  `golden_no_preset_encoded_pixels_are_unchanged`.

- 2026-07-23: Defined tagged `simple` and `density` reconstruction. Density owns
  its parameters and a tagged `exponential { gamma }` or
  `sigmoid { contrast, toe, shoulder }` curve; exponential is the default. The
  unreleased `--algorithm` and old recipe schema are rejected cleanly.
- 2026-07-23: Separated corrected density `D′` from the curve, preserved current
  exponential pixels and the exact sigmoid equation, moved Dmax ownership to
  the curve, and made every path return typed `FilmRgbImage`. Simple WB/range
  moves downstream for named presets while legacy no-preset TIFF ordering stays
  unchanged through migration.
- 2026-07-23: Pinned the target recipe to one nested tagged
  `reconstruction` object: density correction lives under `.density`, while
  exponential/sigmoid parameters and Dmax live under `.curve`. Pinned every CLI
  key mapping and made cross-curve fields—including customized gamma with
  sigmoid—fail after merge instead of being ignored.
- 2026-07-23: Separated `reconstruction.schema_version = 1` from behavioral
  `pipeline_version`. Partial input may omit the curve and resolve to tagged
  exponential defaults, while normalized recipes/reports always emit the curve.
  The bit-identical refactor/no-preset compatibility does not claim a behavioral
  bump; `conversion-versioning` owns the prospective bump when named-preset
  activation and simple reordering change default pixels.
- 2026-07-23 (implementation): **Golden-first refactor.** Before touching any
  code, captured pre-refactor outputs as bit-level fixtures: per-pixel
  `f32::to_bits` for nine converter configurations (density exponential
  default/custom/none/auto-dmax, sigmoid default/custom, simple default, and
  both auto-WB modes) over a 5-pixel shadow/mid/highlight/out-of-range/base
  vector, plus FNV-1a byte hashes of four whole encoded TIFFs (density/simple/
  sigmoid u16 + density f32) from a synthetic 16×16 negative. These live as
  `pipeline::stages::golden` tests (`golden_*`, incl.
  `golden_no_preset_tiff_bytes_are_unchanged`) and all pass against the split
  pipeline — the bit-identical default-exponential / numerically-exact-sigmoid
  acceptance gate, and the proof this task claims no `pipeline_version` bump.
- 2026-07-23: **Structure shipped.** `types.rs` gained the tagged
  `Reconstruction` (custom serde: always emits `schema_version: 1` + `type`;
  wire structs give named cross-variant-key errors and reject unknown fields at
  every level; omitted curve normalizes to tagged exponential defaults) with
  `DensityParams {scale, offset, shadow_balance, highlight_balance,
  balance_range}`, `ExponentialParams {gamma, dmax}`, `SigmoidParams
  {contrast, toe, shoulder, dmax}` under `DensityCurve`. `algo::` replaced the
  `Converter` trait / `AlgoParams` with pure `reconstruct(image, base, config)
  -> (FilmRgbImage, ReconstructionReport)` — `FilmRgbImage` has private fields
  and a `pub(super)` constructor, so the reconstruction module is its only
  producer — plus `finish_print` (the legacy stage-4 bridge; simple passes
  through untouched). The old fused density render was split into `apply_curve`
  (stage 3, mints the typed boundary) + `render_print` (stage 4); auto-WB now
  strides the film positive instead of toning a strided density sample
  (bit-identical: a per-sample map commutes with striding — pinned by
  `golden_auto_wb_estimation_is_bit_identical`).
- 2026-07-23: **Simple WB/clip removed from reconstruction.** Interpreting
  "downstream, named-presets only": simple reconstruction ends at the unclamped
  `1 − scan/Dmin` (bit-identical to the old default since the removed controls'
  defaults were the exact identity), and `--invert-white-balance` /
  `--clip-low` / `--clip-high` plus the `simple.*` recipe keys are **rejected
  with migration errors** pointing at the future `print.white_balance` /
  `print.linear_range` homes — an unreleased tool must not keep a control whose
  placement is about to change. Customized values are inexpressible until
  preset migration (loud, never silently different pixels).
- 2026-07-23: **CLI/recipe surface.** `--reconstruction simple|density` +
  `--density-curve exponential|sigmoid`; every existing flag remapped exactly
  per the spec (`--density-scale/-offset` ⇒ `reconstruction.density.scale/
  .offset`, regional-balance flags ⇒ same-named density fields,
  `--density-gamma` ⇒ `curve.gamma`, sigmoid flags ⇒ `curve.{contrast,toe,
  shoulder}`, the four Dmax flags ⇒ `curve.dmax`). `merge` became fallible:
  invalid tagged combinations (density/curve/Dmax flags with simple, sigmoid
  flags under exponential, `--density-gamma` under sigmoid — flag presence, not
  value) are post-merge usage errors naming the offending flag; a curve switch
  via `--density-curve` carries the roll-fixed `dmax` across variants.
  `--algorithm` and the legacy `algorithm`/`density`/`sigmoid`/`simple` recipe
  keys are rejected with migration errors (`reject_removed_flags` /
  `reject_legacy_recipe_keys`, shared with roll per-frame overrides).
- 2026-07-23: **Report & telemetry.** The convert report gained `recipe` (the
  effective config — so `recipe.reconstruction` is the exact tagged schema) and
  `reconstruction_result` (`{"type":"simple"}` or density with `curve.dmax =
  {policy, value, provenance}`; policy `fixed|explicit|auto|none`, provenance
  `default|recipe|cli|auto-frame` — `auto` always reports `auto-frame`, the
  master-incompatibility marker). Recipe-vs-default provenance is witnessed
  from the raw JSON at load (`LoadedRecipe.curve_dmax_present`), since a recipe
  that wrote `"fixed"` is indistinguishable post-defaulting. Telemetry
  `SCHEMA_VERSION` bumped to 2: `conversion.algorithm` → `conversion.
  reconstruction` + optional `conversion.curve` (skill + design-spec §9 record
  examples updated). `estimate`'s `d_max_recipe` fragment keeps its
  `{"dmax":{"explicit":…}}` shape but now documents/tests the
  `reconstruction.curve` destination.
- 2026-07-23: **Docs.** design-spec §2/§4/§6/§7/§8/§9/§10/§12 flipped from
  "current legacy vs target" to shipped-tagged-schema framing (examples, the
  interface sketch — `reconstruct`/`finish_print` shipped, `map_nc_film_rgb_v1`
  still target — and the §9 reconstruction-select section; the "Current shipped
  keys" callout is gone). NOTE for the main-tree merge: `CLAUDE.md`'s
  architecture section still describes the `Converter` trait and
  `algo/{simple,density}` two-algorithm framing — update it there (kept
  untouched here since the main tree owns it).
- 2026-07-23: **For `film-rgb-working-space`:** the mapper's input contract is
  ready — `algo::FilmRgbImage` (private fields; read via `width/height/rgb/ir`,
  consume via `pub(crate) into_linear`; construction only inside `algo`), and
  `algo::finish_print` is the seam to displace: the mapper slots between
  `reconstruct` and the print controls once presets move stage 4 after ACEScg.
  The report's `working_mapping` field (design-spec §8 example) was deliberately
  left to that task.
- 2026-07-23 (review fixes): (1) `FilmRgbImage::from_linear` visibility bug —
  `pub(super)` on a top-level module is crate-wide; now `pub(in crate::algo)`
  (real construction restriction) with the overclaiming doc-comments corrected.
  (2) `merge_json` gained `internally_tagged_switch`: a per-frame roll override
  that changes `reconstruction.type` or `curve.type` now replaces the tagged
  object instead of deep-merging a rejected union, carrying the base's `dmax`
  when the overlay doesn't set it — the same roll-fixed-anchor semantics the
  CLI `merge` gives `--density-curve` (tests:
  `merge_json_switches_internally_tagged_type_and_carries_dmax`,
  `per_frame_override_switches_variants_and_keeps_the_roll_fixed_dmax`).
  (3) Three golden gaps closed (captured from the proven pipeline): auto-WB ×
  regional balance, auto-WB × sigmoid curve, and `BalanceRange::Auto` with
  non-zero balances (`golden_auto_wb_with_regional_balance_is_bit_identical`,
  `golden_auto_wb_with_sigmoid_curve_is_bit_identical`,
  `golden_auto_measured_balance_range_is_bit_identical`). (4) `algo/mod.rs`
  module doc now points at `pipeline::stages` `mod golden` for the fixtures.
  (5) CLAUDE.md's §9 recipe carve-out corrected in place: the tagged schema is
  shipped and the legacy forms are rejected (flagged for the user's manual
  review — it edits project instructions).


## bw-support

**Status:** not started
**Updated:** —

- Goal: Convert B&W negatives to clean mono positives through the existing `density` algorithm.


## density-safety-bounds

**Status:** not started
**Updated:** —

- Goal: Close the gap where a validation-passing density recipe can silently produce a degenerate (e.g. finite all-black) image, via bounded `density_scale`/`density_offset`/`density_gamma` ranges at the CLI `validate` boundary plus a post-render degenerate-output warning.
- 2026-07-27 (from the `color/film-master-render-pipeline` review; **no code changed
  here**): a **second** silent-underflow site was confirmed and reproduced — the
  **stage-4 print render**, not the stage-3 tone map this task's original context block
  describes. `render_print`'s `2f32.powf(print.print_exposure)` (`algo/density.rs:478`)
  and `px[c] * wb[c] * exposure_gain` (`:486`) are guarded only by `finite()` /
  `positive()`, so `--print-exposure=-200` writes 100 % zero samples at rc 0 with every
  `loss` counter at 0, no warning, and `--strict` also 0; `--white-balance=1e-45,1,1`
  kills exactly one channel the same way (so a whole-image collapse test would miss it).
  The overflow direction (`--print-exposure 300`) is already loud via `clipped_low`.
  The measurement table, the exact reproduction command, and two implementation notes
  (why a naive `is_normal()` on user-supplied gains is the wrong fix here, and where a
  reference predicate already exists in `pipeline::render_split`) are in the task
  file's second `Context` block — start there rather than rediscovering it.


## reference-anchored-sigmoid

**Status:** not started
**Updated:** 2026-07-30

- 2026-07-30: Product direction decided: Dmin remains the film-base/density
  origin, while the reconstruction sigmoid owns shadow-floor/toe placement in a
  roll-fixed Dmax-normalized coordinate. Film-master and display outputs must
  therefore share the same tonal foundation; display rendering must not repair a
  raised floor with a second large grade. The default is reference-based and
  preserves under/overexposure. Sigmoid is the candidate sole product
  reconstruction; exponential/simple remain explicit diagnostic paths until a
  later evidence-backed retirement decision.
- 2026-07-31: Review clarified the unshipped work: §7.3 already provides the
  Dmin-origin, Dmax-normalized monotone sigmoid. The remaining defect is
  empirical—frozen real-roll conversions crowd correctly exposed photographic
  shadows into a narrow raised interval and look pale. The task now requires a
  pinned fixture/reference/recipe baseline and quantitative film-master/SDR/HDR
  shadow-spread metrics before deciding whether defaults, parameter semantics,
  or the equation itself must change. `output/presets` remains the activation
  boundary; content-aware fitting remains excluded from the default.


## content-aware-sigmoid-toe

**Status:** not started
**Updated:** 2026-07-30

- 2026-07-30: Parked content-derived toe placement as an optional, explicit
  follow-up rather than part of the product default. The task distinguishes
  per-frame and roll-frozen acquisition, requires complete provenance, and
  forbids frame-local fitting from film-master/normal product presets so nc does
  not silently auto-correct exposure.


## reference-anchored-sigmoid (continued)

**Status:** not started
**Updated:** 2026-07-31

- 2026-07-31 (PR review terminology correction): The shipped sigmoid is
  **Dmax-anchored**, not Dmax-normalized: its coordinate is
  `t = contrast * (D' - Dmax)` and does not divide by Dmax. The earlier
  2026-07-30 and 2026-07-31 entries above used “Dmax-normalized” imprecisely;
  this entry supersedes that wording while preserving the append-only history.


## content-aware-sigmoid-toe (continued)

**Status:** not started
**Updated:** 2026-07-31

- 2026-07-31 (PR review): Added `output/presets` as a prerequisite. This task
  promises named-preset rejection and byte-identity verification, so the preset
  surface must exist before those contracts can be implemented or tested.


## film-stock-profiles

**Status:** not started
**Updated:** 2026-08-02

- Goal: A selectable registry of known film stocks carrying the per-stock reference
  densities reconstruction needs, sourced from manufacturer datasheets with
  provenance, with a generic C-41 fallback so naming a stock stays a refinement
  rather than a requirement.
- 2026-08-02 (filed during `reference-anchored-sigmoid` planning): the seed data
  already exists — the Kodak *Judging Negative Exposures* aim tables plus the
  Spectral-Dye-Density charts give, per stock, the grey-card and diffuse-white aim
  densities (Status M, red, absolute) and per-channel `D-min`. Measured: Ektar 100
  0.82 / 1.18, Δ 0.36, `D-min` red ≈0.20; Portra 160 0.84 / 1.20, Δ 0.36, ≈0.17;
  Gold 200 0.95 / 1.35, Δ 0.40, ≈0.22 (E-4046 / E-4051 / E-7022).
- Two decisions recorded up front: the professional C-41 aims cluster tightly enough
  that a **generic profile is viable**, so stock selection must never be a
  precondition; and the data's shape should follow `pipeline/colorimetry/` (source
  data with provenance / pinned literals / `#[cfg(test)]` audit) rather than
  inventing a second convention for reference data.
- Deliberately **not** made a dependency of `film-base/dense-base-dmax-plausibility`,
  which wants the C-41-calibrated plausibility floor made stock-relative: that task
  can loosen its floor without a registry, and a false edge would kill real
  parallelism. The two must still be coordinated so stock-awareness isn't solved
  twice.
- 2026-08-02 (PR #68 Codex review, two findings accepted): **measured roll `film_base`
  stays authoritative; a published `D-min` is nominal only.** The repo already defines
  `Dmin` as stock + development + scanner settings
  (`film-base/estimate-reuse-output`), and base fog shifts with processing, storage and
  the individual roll — so letting a stock selection substitute a nominal
  standard-process base would misplace tones on a real roll.
- **The chart-read `D-min` values are provisional, not Status M densities.** Status M is
  a prescribed broadband response: a Status M channel density requires converting the
  spectral-density curve to transmittance, integrating against that channel's response,
  then taking the log. Single-wavelength sampling can be materially wrong where dye
  spectra overlap — the Portra 160 midscale read of 0.73 against a tabulated 0.79–0.89
  is likely this effect. The manufacturer-*tabulated* aims (and their difference Δ) are
  the authoritative half; chart reads must not become ground truth for the registry or
  for `io/scanner-density-calibration` until properly integrated or tabulated.
## reference-anchored-sigmoid (Phase 0)

**Status:** in progress
**Updated:** 2026-08-02

- 2026-08-02: **Phase 0 complete — fixture Dmin/Dmax frozen for all three fixture rolls**
  via `harness.sh freeze`, which now reads its roll triples from the asset manifest:

  | Roll | Dmin (r,g,b) | Dmax | note |
  |---|---|---|---|
  | `2026-07-24-Gold200` | 0.6001831, 0.27512017, 0.14776836 | 1.2758015 | new; no estimator warning |
  | `Ektar` | 0.51679254, 0.2768597, 0.18973067 | 1.2933096 | reproduced bit-identically |
  | `Portra160-2026-07-22` | 0.49988556, 0.24776074, 0.14920272 | **1.3816013** | re-frozen; see below |

- **The Portra160 re-freeze was necessary and material.** The committed `Portra160.json`
  named Dmin/Dmax frames `20260720-nikon-1059` / `1058`, neither of which is in the
  current `Portra160-2026-07-22` roll (manifest: unexposed 1097 / leader 1096) — the
  recipes predate an asset reorganisation. Re-freezing from the manifest's frames gives
  Dmax **1.3816** against the stale **1.3352**, a 0.046 shift. Reusing the old value
  would have anchored the entire baseline comparison on a different piece of film.
- `Ektar`, `Portra400-leica-flaw` and `phoenix` reproduced bit-identically, which both
  validates harness determinism and confirms the defect was specific to Portra160.
- Gold200 raised **no** plausibility warning (Dmax 1.2758 is above the C-41 `≳1.0`
  floor), so the `film-base/dense-base-dmax-plausibility` risk did not materialise here.
- **Stale artifacts left in place, flagged not fixed:** `Portra160.json` and
  `Portra400.json` name rolls that no longer exist under those names. `Portra160.json`
  now sits beside `Portra160-2026-07-22.json` with a *different* Dmax, which is a trap
  for the next reader — recommend deleting both stale files, but that removes another
  task's committed artifacts so it is the user's call, not a side effect of this task.
- Three `*.hdr.json` files show as modified with **no value change** — the harness now
  emits the `output` block after `reconstruction` instead of before. Committed so a
  future re-freeze shows a clean diff.
- α recomputed against the frozen anchors: Ektar 0.479, Portra160 0.485, Gold200 0.572
  (mean ≈ 0.51). Config 3's sweep covers 0.5/0.6/0.65, so it still spans the range —
  and per the PR #68 review the numerator is a provisional chart read, so this must not
  be used to narrow the sweep.
- 2026-08-02 (**Evidence D upgraded from suggestive to measured**): the user restored the
  `Portra160` and `Portra400` roll folders, so both same-stock pairs now exist. **Correction
  to the Phase 0 entry above: those recipes were never "stale" — they were *orphaned* by the
  folders' removal.** Re-freezing reproduced their recorded Dmax exactly (1.3352162 and
  1.7382799), so they were correct for their rolls all along. The `Portra160-2026-07-22`
  freeze was still necessary: that is a *different* roll of the same stock, with no recipe
  of its own.
- The controlled comparison — same stock, same scanner, contrasting the **base** (a genuine
  film + development property) against the leader-derived Dmax:

  | Stock | pair | base Δ (r / g / b) | leader Dmax Δ |
  |---|---|---|---|
  | Portra 160 | `Portra160` vs `-2026-07-22` | +0.029 / +0.027 / +0.021 | +0.046 (0.15 stops) |
  | Portra 400 | `Portra400` vs `-leica-flaw` | **−0.0005** / +0.023 / +0.021 | **−0.295** (0.98 stops) |

  The Portra 400 row is decisive: its **red base agrees to 0.0005 density** — same stock,
  same instrument — while the leader-derived Dmax differs by a **full stop**. Both
  quantities cannot be film properties.
- **Framing sharpened:** "accidental" was too strong. Portra 160's leaders agree to 0.046,
  within base-level variation. The leader is not reliably wrong, it is **uncontrolled** —
  sometimes it lands, sometimes it is a stop out, and a single measurement cannot tell you
  which. That is worse for an anchor than a consistent bias.
- By-products: **±0.03 density is the cross-roll reproducibility floor** for a
  Dmin-referenced quantity (good for config 8 — ~0.07 decades at contrast 2.2); and in
  *both* pairs the later-dated roll carries ~+0.02 more green/blue base while red does not
  move consistently — n = 2, so a hypothesis, but a systematic per-session per-channel shift
  would bound how far any one-time scanner profile can be trusted
  (`io/scanner-density-calibration`).
- Gold200's stock confirmed by the user as **Kodak Gold 200** (E-7022), retroactively
  validating the use of that datasheet's aims (0.95 / 1.35, Δ 0.40) — previously inferred
  from the folder name.
- 2026-08-02 (**Phase 2 harness landed; Phase 1 proposal run**): added
  `src/pipeline/shadow_metrics.rs`, declared `#[cfg(test)] pub mod` in
  `pipeline/mod.rs`. Two `#[ignore]`d entry points — `propose_patches` and
  `characterise_reference_frames` — plus 4 always-on unit tests for the geometry and
  statistics. Skips with a message when `../nc-assets` is absent, so the full suite
  (126 tests) stays green with no assets and CI needs none.
- **Harness bug caught by its own first run:** it globbed the roll directory and so
  proposed "shadow" and "diffuse white" patches on the *leader* and *unexposed* frames,
  where both are meaningless. Now reads `role` from the manifest and proposes only over
  `real` frames; leader/unexposed get their own characterisation pass. Also note the two
  `#[ignore]`d tests interleave stdout when run together — use `--test-threads=1` or the
  roll headers are misattributed.
- **Leaders are uniform — no fogging gradient.** Interior tile `D′` range across the
  leader: Gold200 0.024, Ektar 0.039, Portra160 0.067; L−R / T−B gradients ≤ 0.024.
  Their median `D′` is 99.9 / 100.1 / 100.3 % of the frozen Dmax, confirming the anchor
  is that frame's own level. **This refutes a speculation in the plan** — non-uniformity
  is *not* additional evidence for the leader problem, because there is none. The case
  rests entirely on the cross-roll comparison: a uniform field at an *uncontrolled level*.
- Unexposed frames sit at `D′` 0.016–0.026 over the interior (the base was frozen from a
  centre 40% region, so the wider interior reads marginally denser) with in-tile spread
  0.024–0.040. That spread is the **measurement noise floor** — any patch spread below
  ~0.04 is grain, not texture. Real-frame candidates ran 0.15–0.98, comfortably above it.
- **The decisive measurement: diffuse white lands at 41–93 % of the leader Dmax (median
  ~66 %), never near 100 %.** Density headroom above the brightest textured diffuse
  candidate is 0.09–0.81 (median ≈ 0.43). At contrast 1.0 that is ~0.43 decades of range
  reserved for densities no photograph in the set contains — the saturation-as-white
  hypothesis, measured on real frames rather than inferred from a datasheet.
- Mid-tone sits at 11–58 % of Dmax across frames: the genuine exposure spread the task
  must preserve, and a usable signal for the exposure-spacing metric.
- **Check A is not evaluable from auto-proposed patches, as expected.** The implied
  mid→white Δ scatters 0.085–0.850 against the datasheet's 0.36, because the proposal's
  "mid-tone" is the frame's *median tile* (not a mid-grey surface) and its "diffuse
  white" is the brightest textured tile (not necessarily a diffuse reflector). Suggestive
  detail: the three frames whose Δ lands nearest 0.36 (0.303, 0.347, 0.435) are the ones
  whose mid-tone sits at 37–46 % of Dmax, i.e. the normally-exposed-looking ones. Δ is
  printed labelled "orientation only, NOT Check A".
- 2026-08-02 (Phase 1 review aid): added `scripts/sigmoid-baseline/patch-review.sh` +
  `build_patch_review.py`, which turn the `propose_patches` output into a reviewable HTML
  page — each `real` frame rendered as a positive with the candidate rectangles drawn on
  it, a magnified crop per box, and per-frame questions keyed to a stable mark (G1–G3,
  E1–E3, P1–P4) so a later discussion can name one box. Crops are pure CSS
  `background-position` off the single per-frame JPEG, so no crop files are generated.
- Deliberate choices there: previews render through **`--density-curve sigmoid`**, both
  because it is the curve under investigation and because the frozen *exponential* recipe
  clips ~10.3% of samples — blown highlights would defeat the "is this a diffuse white?"
  judgement the page asks for. Output goes to `../temp/patch-review` (throwaway), never
  into `../nc-assets` or the repo, and it is **not** published as an Artifact: these are
  the user's personal photographs and publishing would upload them to an external host.
- 2026-08-02 (review-page bug, fixed): the magnified crops rendered as solid black. Cause
  was HTML, not CSS geometry — the crop's inline style used `url("X.jpg")` with **double
  quotes inside a double-quoted `style` attribute**, so the attribute terminated at
  `url(` and the remainder was parsed as junk attributes. Fixed to single quotes, with a
  comment at the site since the failure mode (black box) does not point at quoting.
  Verified in Chrome by inverting the CSS background math to recover the displayed source
  region: all 30 crops resolve to their declared rectangle within ~1 px, aspect 0.959
  (= 328/342), `background-size` 1580.49 % (= 5184/328), no crop left without a
  background. Verification used JS introspection only — no screenshots — so no sample
  pixels entered an agent context.
- 2026-08-02 (exposure question reworked, at the user's request): "correct / under / over"
  proved genuinely hard to answer, and the reason is diagnostic — **at the shipped contrast
  the raised black floor leaves no black reference, so a frame reads as neither under nor
  over.** The question is now "which EV variant reads as correctly exposed?", answered from
  a row of five real renders per frame.
- Implemented as **real `--print-exposure` renders, not a CSS `filter: brightness()`**. CSS
  filters act on *encoded* sRGB, so `brightness(2)` is not one stop; it is a non-photometric
  curve and a variant chosen that way would not map back to any pipeline value. The real
  knob is a true `2^EV` linear gain, so the chosen variant converts directly into an EV
  offset — and it is the *relative* answers across frames that classify exposure.
- The row renders at `--sigmoid-contrast 2.0` (the datasheet-derived ≈2.07) while the
  full frame above stays at the shipped 1.0, so the page also shows the contrast comparison
  directly. Sweep runs downward (−2 … 0) because at contrast 2.0, EV 0 clips **nothing** on
  these frames while EV +1 clips ~14 %. A brightness/contrast slider is included for
  free-form looking, labelled non-photometric so it is not mistaken for a candidate setting.
- WB deliberately left neutral despite the visible blue cast: auto-WB is frame-local, and
  injecting a per-frame correction into a comparison whose purpose is reading per-frame
  differences would confound it.
- 2026-08-03 (Phase 1 round 1 reviewed; tool improved per user feedback):
  - **Patches were too coarse.** The 12 × 8 grid gave 328 × 342 patches (6.3 % × 9.5 % of
    frame) and the user's answers showed they straddled objects — "dark branch *and*
    distant forest", "2/3 shadow *and* background forest", and for P1 all three boxes were
    "a mix of dark forest and bright sky". A patch whose semantics cannot be stated is
    useless for the Δ calibration. Grid is now 32 × 22 → ~123 × 124 (a quarter the area),
    overridable via `NC_TILES=<x>x<y>`, plus **non-maximum suppression**
    (`MIN_SEPARATION_TILES = 3`) so the reported top-3 are spatially distinct rather than
    three adjacent cells of one surface.
  - **Consequence, flagged to the user:** the boxes moved, so round-1 answers no longer
    describe them (E1's white went from the lake at 2262,1115 to 2713,555). Round 1 is not
    wasted — its *general* observations stand (P1 is all forest/sky mixture; P2 has no
    large shadow area; P4's white is a specular tractor highlight) — but the per-box
    yes/no answers must be re-collected against the new geometry.
  - **My sweep was one-sided, and that was a measurement error.** All ten frames picked
    EV 0, the boundary of a −2…0 range, which means the optimum sat at or beyond it. Now
    two-sided (−2 … +1.5), and `EVS` is overridable.
  - **Upward EV clips heavily and that is itself a finding:** at contrast 2.0, EV 0 clips
    *nothing* on E1 while +0.5 clips 11.6 % and P3 reaches 20.1 %. Raising exposure buys
    brighter midtones only by blowing 7–26 % of highlights, because the shoulder has
    already packed content against white. **Exposure is the wrong knob for a raised
    floor** — an argument for changing the curve's shape (configs 3/4/8) over recalibrating
    defaults.
  - **Why the boundary preference happens at all:** with white pinned at Dmax, raising
    contrast pivots the line *around white*, pushing everything below it down. So more
    contrast darkens midtones and needs +EV to compensate — the two knobs fight. A
    mid-anchored or diffuse-white-anchored form would not have that interaction.
  - **The same-illumination constraint on Δ** (missed until the user's descriptions
    exposed it): the datasheet says the grey card and the paper grey scale each *"receiv[e]
    same illumination as subject"*, so Δ = 0.36 is defined for white and mid under the
    **same light**. The proposal ranks on density alone and has no notion of illumination,
    so it will pair a sunlit white with a shadowed mid and put the lighting difference
    straight into Δ — very likely much of the 0.085–0.850 scatter. P3 is the clearest
    casualty: its white (window ledge in sunshine) is the best in the set, but its mid
    (sofa in shadow) makes the pair invalid.
  - Patch-quality triage from round 1: genuine diffuse whites on only **G1** (painted
    garage door), **G2** (white flower) and **P3** (window ledge); bad mids on G1 (blue
    sky) and P4 (parking lot in shade); **P1 unusable for patch metrics entirely.** So
    Check A may rest on one or two frames — far too thin, which independently confirms the
    user's own point that a grey card in frame (not merely more frames) is what is needed.
  - User asks the tool be kept and reused for config comparison beyond this task; `EVS`
    and `NC_TILES` are the first steps toward that.
- 2026-08-03 (review-page tweaks, user request): removed the CSS brightness/contrast
  sliders (unused — and they were non-photometric anyway, so nothing is lost), and added a
  **hover zoom** on the EV variants: hovering a thumbnail shows the same file full-size in
  a fixed overlay labelled with its mark and EV. Same `src`, so it is served from cache
  rather than downloaded twice. Variant renders bumped 1000 → 1600 px so the zoom actually
  resolves detail (~45 MB total in `../temp`, throwaway).
- 2026-08-03 (variant comparison reworked; hover replaced by a click lightbox): hover was
  unusable for two reasons the user hit immediately — a centred thumbnail is covered by its
  own popover, so you cannot move to the next one, and the gaps between thumbnails make the
  overlay flicker as the pointer crosses them. Replaced with **one shared lightbox**: click a
  variant to open, ‹ / › buttons or arrow keys to step through that frame's row (wrapping at
  both ends), Esc or a click anywhere in the overlay except the buttons to dismiss. One
  modal rather than 80 overlays is what makes prev/next possible at all.
- **Verification limit, stated rather than glossed:** the interactive behaviour could *not*
  be confirmed in the Chrome-automation sandbox. Inline scripts appear to be blocked there —
  a capture listener saw zero clicks from a synthetic dispatch, and the page's own keydown
  handler did not respond to a bubbling Escape, while the script text is demonstrably present
  in the document. Structure, CSS and the handler's logic were verified (row length 8, index
  lookup, opening resolves to `display: grid`); the event wiring is unverified here and needs
  a human check in a normal browser. This is an artifact of the automation environment, not
  of a local `file://` page, where inline scripts run normally.
- User asks for a **separate task** to improve this tool properly rather than continuing to
  patch it inline. Not filed yet: `docs/TASKS.md` is currently modified on the open PR #68
  branch, so adding another task now would conflict. File it once #68 merges.
- 2026-08-03 (Phase 1 round 2 answers; sweep extended a **second** time):
  - **My sweep was bounded too low twice.** A −2…0 range had all ten frames pick 0; a
    −2…+1.5 range had six of ten pick +1.5. A near-unanimous boundary choice means the
    optimum lies *outside* the range. Now −1…+3.5, and the generator reads the same `EVS`
    list the render script uses so the two cannot drift into referencing unrendered images.
  - **The deficit is quantified and it is large.** At contrast 2.0 with white pinned at
    Dmax, a measured mid-tone lands at `10^(2·(D′−Dmax))`: E1's 0.5418 → 0.031 linear
    (50/255), needing **+2.52 EV** to reach 0.18; E2 needs **+3.57 EV**. So the shipped
    anchor places midtones 2.5–3.6 stops too dark once contrast is photographic — which is
    exactly why every frame wanted more exposure than I offered.
  - **The datasheet chain closes on itself.** With white pinned at *diffuse* white and
    `contrast = 0.745/Δ = 2.07`, a mid-grey sitting Δ below white lands at
    `10^(2.07·−0.36) = 0.18` — mid-grey, exactly, by construction. So the entire observed
    EV deficit is attributable to anchoring on Dmax rather than diffuse white, and configs
    4/7/8 predict **zero** exposure compensation. Sharp and falsifiable.
  - **Content exceeds the leader Dmax.** G3's auto white measures `D′` 1.3265 against
    Dmax 1.2758, and P3's 1.5062 against 1.3816 — real photographic content sits *above*
    the anchor. The leader-derived Dmax does not even bound the frame, which is an
    independent blow to it beyond the same-stock inconsistency already recorded.
  - Testing "chosen EV == the diffuse-white pin" gave mean |diff| 1.50 EV — **not** a clean
    confirmation, but the residual is structured, not random: it is ≤0.5 EV on the frames
    whose white is genuinely diffuse and below Dmax (E1 +0.27, E3 +0.50, G2 +0.41) and
    breaks down precisely where the patch is invalid — G3 −1.84 and P3 −2.33, the two
    super-Dmax speculars. The test is also censored, since six answers sat at my +1.5 cap.
    Re-run once the extended sweep is answered.
  - **Confirmed patch semantics (round 2).** Genuine diffuse whites on only **G2** (white
    lily) and **P4** (white painted sign); P3's window ledge is white but sunlit and
    super-Dmax. Speculars: E2 and E3 ("sunshine reflected on leaves"), P4's earlier tractor
    highlight. Sky: G3, P1. Fog: E1, P2. **E1's white is contaminated by a scanning dust
    speck** — dust blocks light, so it is dense in the negative and renders as a false
    highlight; IR-based dust removal is a roadmap item, and that patch must move.
  - **User pushback on G1's mid being blue sky, partially accepted:** sky luminance can sit
    near mid-grey, so it is defensible as an *exposure* reference. It is still poor for Δ,
    which needs a spectrally *neutral* surface — strongly blue sky has very unequal
    per-channel densities, and sky luminance varies with angle to the sun and haze.
  - P1's blue cast: agreed out of scope. The frozen recipe uses neutral WB deliberately.
- 2026-08-03 (**review path corrected — previews now come from the measured renderer**):
  the user asked whether the review JPEGs carry HDR. They do not (8-bit, sRGB, no gain map)
  — but the question exposed a worse problem: previews came from the **legacy** path
  (`reconstruct → finish_print → color::to_output`) while the acceptance bounds are measured
  on `pipeline::sdr::render`, which is different code (Hermite shoulder + radial gamut
  mapping vs legacy's linear-space soft clip). Reviewing one renderer while measuring
  another is not a fair test. Previews now render with `--output-preset ultra-hdr-v1`,
  whose JPEG **base is** the `pipeline::sdr` rendition, so the page shows what gets
  measured. Display P3 survives the `sips` downscale (verified: red matrix column 0.51512 /
  0.2412 / −0.00105); the gain map is dropped, which is correct for SDR thumbnails.
  - **Correction to a previously reported finding.** "Exposure is the wrong knob because it
    costs 7–26 % blown highlights" was measured on the **legacy** path only. On the display
    path, contrast 2.0 at EV +1.5 reports `clipped_high: 0`. The clipping argument does not
    transfer. What survives is the *exposure deficit* itself (midtones 2.5–3.6 stops too
    dark), which is density arithmetic and path-independent.
  - HDR review deferred by user decision: "we are just at the first one at our all
    comparison, and this white-pinned one is even not the one with highest expectation."
    Full-size gain-map files for the mixed-range frames (G3, P3) come once every candidate
    config is renderable, so the HDR question can be judged across all of them at once.
    Constraint to remember: `sips` cannot downscale a gain-map JPEG without destroying the
    gain map, so HDR review needs full-size (~6.5 MB) files.
- 2026-08-03 (**first uncensored exposure comparison — reference-driven anchor beats
  content-driven**). Round-3 EV preferences on the corrected display path, extended range
  −1…+3.5 with no answer at the boundary: E1 +1.5, E2 +1, E3 +1.5, G1 +2.5, G2 +1.5,
  G3 mixed (sky +0 / tree +2), P1 +2.5, P2 +2.5, P3 +1.5 (people) / +0 (window), P4 +2.5.
- Two candidate anchors were scored against those preferences, expressing each as the
  equivalent `--print-exposure` at contrast 2.0 (`EV = c·(Dmax − W)/log10 2`):

  | | mean \|diff\| | median |
  |---|---|---|
  | **A** content-driven — pin the *measured* brightest diffuse patch | 0.96 EV | 0.59 |
  | **B** reference-driven — pin the *datasheet* diffuse-white-above-base | **0.63 EV** | 0.58 |

- **B wins, and its residuals are per-stock systematic rather than random:** Ektar ≈ +0.6
  throughout, Portra 160 ≈ 0 (P1/P2/P4 all within **0.16 EV** — one constant predicting
  three different scenes to a sixth of a stop), Gold 200 ≈ −1.0. A constant per-stock
  offset is exactly the signature expected if each stock's derived white is off by a fixed
  amount — which is anticipated, since they rest on the **provisional chart-read `D-min`**
  values PR #68 flagged as not true Status M densities. The *form* is supported; the
  constants are the uncertain part. (Correcting each stock by its mean residual would fit
  within ~0.3 EV everywhere, but that is fitting, not prediction, so it is not validation.)
- **This favours the shippable candidate over the diagnostic one.** A is config 4/7 —
  frame-local content adaptation, forbidden for the default — while B is config 8, which is
  content-free and default-eligible. A's failures are also explained by that: it stretches
  whatever the brightest diffuse patch happens to be up to 1.0, so a frame containing no
  true white (P2 fog, P4 sign, E2 specular leaves) is forced too bright. Frame-local
  fitting misbehaving exactly as the plan predicted.
- **G3 and P3 are unresolvable by any single global curve** (sky +0 vs trees +2; window +0
  vs people +1.5) — their scene range exceeds SDR. That is the HDR question, deferred.
- 2026-08-03 (**scope reduced by the user: filter methods, do not tune parameters**).
  - **Bias identified in my measurement design:** asking for a preferred EV frame by frame
    *is* per-frame optimisation, which contradicts being honest to the film. Quantified —
    the user's preferences have stdev 0.57 EV with a **within-stock spread of 0.5–1.0 EV**,
    and that within-stock part is what a fixed anchor cannot follow and must not chase:
    some is real exposure variation in the negatives (which the task requires *preserving*)
    and the rest is judgement noise. For comparison, candidate A's own frame-to-frame swing
    is 1.43 EV across 8 distinct values — it adapts *more* than the human — while B's is
    0.54 EV across 3 values, one per stock.
  - **Correct use of the data is the central tendency, not the per-frame values.** Median
    preference is +1.5 EV at contrast 2.0 = white pinned **0.452 density below Dmax**, close
    to the median *measured* diffuse-white gap of 0.417 — two independent routes to
    ~0.42–0.45. Candidates will no longer be scored frame-by-frame against preference. Note
    an offset stated relative to Dmax inherits Dmax's unreliability; config 8's Dmin
    reference does not.
  - **Revised goal:** (1) do not seek the optimal parameter — contrast ≈2 ≫ 1.0 suffices,
    2.21 vs 2.22 is out of scope; (2) filter which anchoring forms deserve support, with
    **no requirement to pick a single winner** — closer to the task file's "choose the least
    invasive remedy" than a parameter hunt; (3) parameter tuning moves to a follow-up task
    once higher-quality, deliberately correctly-exposed samples exist (the current ten were
    picked at random).
  - **Consequences:** acceptance bounds become **qualitative gates** (reaches a plausible
    black; needs no per-frame correction; preserves exposure spacing; finite/continuous/
    monotone; no clipping) rather than numeric thresholds — defensible at n = 10, which
    thresholds never were. And the deliverable becomes a filtered candidate set plus a
    *provisional* parameter; since `output/presets` activates whatever default this task
    lands, it would ship a provisional value. Acceptable pre-release, and the seam is clean
    (a later parameter change is a default change + conversion-version bump, not a change of
    form) — recorded so it is a conscious decision rather than a surprise at activation.
- 2026-08-03 (**Phase 3 measured — candidate set filtered**). Froze
  `scripts/sigmoid-baseline/fixtures.json` (schema 1) with each patch's rectangle *and*
  user-confirmed semantics plus validity flags, so invalid patches are skipped rather than
  averaged in: 2/10 valid diffuse whites (G2 white lily, P4 painted sign), 7/10 valid mids,
  9/10 valid shadows, **2 frames usable for the datasheet Δ**. Added
  `shadow_metrics::measure_candidates`, which exploits the fact that **every anchoring form
  reduces to one number** — the sigmoid anchor `A` (`curve.dmax`) plus a contrast — so no new
  curve code was needed. Report: `docs/reports/sigmoid-reference-baseline.md` (+ raw output).
- **The defect, precisely: the shipped default gets midtones nearly right and blacks badly
  wrong.** Candidate 1 needs only 0.14 EV to place a mid-grey yet its darkest *confirmed*
  shadow patch sits at 72/255. That is why the complaint is "pale" and not "dark", and it is
  now reproduced on confirmed patches rather than inferred.
- Filtering outcome: **reject 1** (black gate, 72/255); **reject 5** (black-pinned needs
  +4.75 EV — pinning black alone leaves white and mid unplaced, and fixing that requires a
  second pin ⇒ adaptive contrast, already rejected); **4 and 7 diagnostic-only** on the
  frame-local argument, explicitly *not* on this data since both resolve on **2 frames only**;
  **2 and 3 contingent** on a `film-base` Dmax fix; **support 8** — smallest residual (0.78 EV)
  of any black-passing shippable form, Dmax-free and content-free.
- **A gate I had backwards, now corrected in the harness output.** "Lower mid spread = more
  reference-driven" is wrong. A reference-driven anchor applied to frames that genuinely differ
  in exposure *should* leave spread; **low** spread means the anchor is *correcting* exposure —
  the frame-local behaviour the default must not have. Also only comparable at equal contrast,
  since low contrast compresses between-frame differences (candidate 1's small sd is that
  artifact, not a merit).
- 2026-08-03 (**two of my rejections were wrong; user caught both**):
  - **Candidate 5 (black-pinned) was rejected on my parameter, not its form.** Pinning black
    at NLP's 0.00061 with c=2.0 implies an anchor of `−log10(0.00061)/2 = 1.607` — *above*
    every roll's Dmax (1.28–1.38) — so nothing reached white and the frame rendered dark. My
    stated reason ("pinning black alone leaves everything unplaced at any fixed contrast") was
    simply false: fixed contrast is exactly what candidate 2 does. Retested at targets
    consistent with the contrast — 0.002 → anchor 1.349, 0.005 → anchor 1.151. Black-pinning
    is as legitimate as white- or mid-pinning: another rule for the same single anchor, and
    **Dmax-free**. Results: 5a needs +3.04 EV (shadow 9/255), 5b +1.71 EV (shadow 20/255).
  - **Gating the content-driven candidates on a *semantically valid* white was incoherent.**
    A content-driven mode has no knowledge of what a real white is — it measures the brightest
    content and adapts. Requiring validity also meant they resolved on 2 frames only, making
    their statistics worthless. They now use the shipped `DmaxSource::Auto` (99.5th percentile
    of corrected densities), so they resolve on all ten and test *shipped* behaviour. Verdict
    changes from "diagnostic only" to **"explicit-mode only"** — a legitimate opt-in mode
    (`algo/content-aware-sigmoid-toe`), just never the default.
  - **And that immediately found a real defect: `Auto` is dominated by the film holder.** It
    resolves to **2.23–2.37** on every frame against a roll Dmax of 1.28–1.38, because the
    opaque holder has near-zero transmission and therefore enormous corrected density, so it
    owns the 99.5th percentile of a full-frame scan. Candidates 4 and 7 render everything to
    0/255 — that is holder contamination, not content adaptation, so it is **not** a verdict on
    the form. A content-driven mode must measure the *interior*, which is what `film-base`'s
    rebate detection exists for. This also explains why `--auto-d-max` was demoted to opt-in.
- Added `scripts/sigmoid-baseline/candidate-review.sh` + `build_candidate_review.py`: renders
  all 8 candidate forms × 10 frames (80 images) through the display path with **no exposure
  applied**, and builds a comparison page with the same click lightbox. The anchor rules live
  in one place so renders and page cannot disagree.
- 2026-08-03 (**user verdicts on the candidate forms, and a gap in my sweep**):
  - Ranking, best to worst: **3 and 8** (both GO), then **5b** (most likely GO), **5a**
    (unsure), **1** (maybe not go), **2** (not go). Plus two per-frame notes: on **G3, 8 > 3**;
    on **E2, 2 > 1**.
  - The user notes the ranking is partly exposure-driven, since exposure is the most salient
    cue to the eye. Recorded as an honest property of the data, not a contamination — the
    verdicts are per *form* and aggregated, which is what the reduced scope asks for.
  - **P3 exposes a clean physical trade, and the arithmetic reproduces the user's eye
    exactly.** Output gap between `D′` 1.40 and 1.50 (where the curtain detail lives):
    c1 0.067, c2 0.083, c5a 0.047 (detail kept) · c5b **0.00057** ("lost some") · c3 0.00008,
    c8 0.00003 ("lost all"). Mechanism: lowering the anchor to lift midtones and blacks puts
    more content *above* white, where the shoulder must compress it — and at width 0.2 that
    compression saturates and differentiation collapses.
  - **Answer to the user's question — yes, recoverable, via the shoulder, which I never
    varied.** All eight configs used the shipped `shoulder = 0.2`; that is a real gap. Swept on
    c8 (A=1.03, c=2.069): shoulder 0.2 → gap 0.00003, mid 0.1799; 0.6 → 0.0164 / 0.1740;
    **1.0 → 0.0502 / 0.1525**; 1.5 → 0.0699 / 0.1188; 2.0 → 0.0684 / 0.0887. So **0.6–1.0
    recovers most highlight differentiation** (1.0 is comparable to c1's 0.067) for ~0.24 EV of
    midtone cost; beyond 1.5 midtones darken for little gain.
  - **Why the shipped default is too narrow:** 0.2 was calibrated for a regime where content
    essentially never exceeded white (anchor at Dmax, nothing reaches it). Moving the anchor to
    diffuse white makes the shoulder **load-bearing for the first time**, so a width chosen for
    a decorative roll-off is inadequate. Testing 0.2 vs ~1.0 is therefore a *form-viability*
    question, not the 2.21-vs-2.22 tuning that was deferred.
  - Mechanism summary that now ties the three datapoints together: **anchor height governs
    highlights** (G3: 8's 1.13 anchor beats 3's 1.01 on a sky-heavy frame; P3 likewise),
    **contrast governs shadows** (E2: 2 > 1 on a dark forest frame where the pale floor is most
    objectionable), and **the shoulder is what relaxes the conflict between them**.
- 2026-08-03 (**shoulder verdict: ≈0.6, with a mechanism, not a preference**). User read:
  0.2 sharper in the regular range but obvious highlight loss; 0.6 and 1.0 both avoid most of
  the loss; 1.0 only beats 0.6 on P3; 0.6 sharper than 1.0. Verdict **0.6 > 1.0 > 0.2**.
  Quantified — local contrast per 0.05 density on c8 (A=1.03, c=2.069):

  | `D′` | region | sh 0.2 | sh 0.6 | sh 1.0 |
  |---|---|---|---|---|
  | 0.67 | mid-grey | 0.0430 | 0.0393 | 0.0308 |
  | 0.85 | upper mid | **0.0995** | 0.0716 | 0.0498 |
  | 1.20 | highlight | 0.0043 | 0.0428 | **0.0507** |
  | 1.40 | curtain | **0.0000** | 0.0117 | **0.0298** |

- **The decisive figure is where each shoulder begins eating local contrast:** `D′` 0.95
  (sh 0.2), **0.70** (sh 0.6), **0.45** (sh 1.0). Mid-grey is at 0.67 — so **0.6 begins bending
  right at mid-grey**, where a print shoulder belongs, while **1.0 begins well below it** and is
  therefore no longer a highlight shoulder but a flattening of the entire upper half. That is
  the mechanism behind "0.6 is sharper than 1.0", and it makes 0.6 principled rather than
  merely preferred.
- **On the user's "clamp to 1.0 when part of the image is too light":** their own instinct that
  it belongs to a different story is correct, and the reason is precise — selecting the shoulder
  from how much content is too light is **content-adaptive**, so two frames of one roll would
  get different curves and their highlight relationships would stop being comparable. Same
  category as content-driven anchoring; belongs in the explicit mode, not the default.
- **Better resolution:** the *only* frame where 1.0 beats 0.6 is **P3** — already identified as
  one of the two frames whose scene range exceeds SDR. So the frames that would trigger the
  clamp are exactly the frames that should get **HDR output** instead. Do not adapt the shoulder
  to force a high-DR scene into SDR; give it the range it needs.
- **A legitimate reference-driven version does exist**, and should be recorded rather than lost:
  a **per-stock** shoulder taken from datasheet curve shape (not per-frame content) would be as
  defensible as the per-stock anchor. No datasheet shoulder data exists yet, so it is follow-up
  work under the parameter-tuning task — but it is the honest way to get what the clamp was
  reaching for.

### 2026-08-03 — Phase 4: the remedy, and why it stopped at step 2

- **Remedies 1 and 2 sufficed; §7.3's equation is character-for-character unchanged.**
  Recorded because the task mandated that order and the outcome is the answer to "why did
  the less invasive options suffice": the defect was never in the curve, it was in *which
  tone the curve pins*. Recalibrating alone could not fix it — at contrast 1.0 the floor is
  72/255, and raising contrast with white pinned drags midtones down, because steepening a
  line pivots it about the pinned point. Two coupled changes were needed, not one.
- **Defaults recalibrated:** `contrast 1.0 → 0.745/0.36 ≈ 2.0687`, `shoulder 0.2 → 0.6`,
  `toe` unchanged. Both derived, with the derivation in the doc comments so a future reader
  can re-check rather than trust: the contrast from the manufacturers' own mid-to-white aim
  delta (film gamma 0.52 / system gamma 1.07 as independent corroboration), the shoulder
  from where its bend begins (`D′ ≈ 0.70`, essentially mid-grey).
- **Anchor reparameterized** — the substantive change. `curve.dmax` was overloaded: the
  roll's *reference* density **and** the density rendering to `1.0`. Now `curve.dmax` is the
  reference and `curve.anchor` (`AnchorPlacement`) says which tone it places, defaulting to
  `{"mid-at-dmax-fraction": 0.5}` — candidate 3's form, `A = f·R + 0.745/contrast`.
- **`AnchorPlacement` is an enum, not a bool + f32**, for the same reason `DmaxSource` and
  `FilmBaseSource` are: independent fields can encode illegal combinations that a flags-win
  merge then silently mis-resolves. The two CLI flags conflict at the clap layer and each
  replaces the whole variant.
- **Golden impact, verified rather than predicted.** The two sigmoid goldens moved and were
  recaptured with the reasoning at the site — the default vector's base pixel 0.0115 →
  0.00177 (≈28/255 → ≈6/255, an actual black), its dense highlight 0.448 → 0.946. The
  **auto-WB golden gain dropped 2.2304 → 1.0574**, which is worth keeping: the estimator
  samples the rendered positive, so WB had been partly compensating for the broken curve.
  Both drift fingerprints are **unmoved**, as predicted — the default recipe still selects
  `exponential`, so `output/presets` still owns the bump when it flips the default curve.
- **Candidate 8 could not ship here, and that is a dependency fact rather than a
  reversal.** Its per-stock offsets *are* `algo/film-stock-profiles`, and they currently
  rest on chart reads PR #68 established are not true Status M densities. So the user's
  routing lands in two pieces: this task ships the no-stock arm (3) plus the opt-in escape
  hatch, and the stock arm is a **third `AnchorPlacement` variant** when the registry
  exists. Recording the seam explicitly so the next task does not re-litigate the design.
- **`NOMINAL_DMAX` deliberately left at 2.0.** The measured rolls cluster near 1.36 with
  Phoenix excluded, but the user is adding samples and asked for that calculation to wait.
  Safe to defer *because* of the mid placement: `dA/dR = f`, so the fallback's error is now
  halved (fixed default gives `A = 1.36` against a measured ≈1.01, where the old rule gave
  2.0 against 1.3).
- **Two doc claims were conditional on the old rule and are now qualified, not left
  quietly false:** the paper-black floor is `10^(−contrast·A)`, and "zero knees reduce to
  the exponential curve bit-for-bit" holds only under `white-at-dmax` — under the default
  placement it is the same line *offset*. The existing reduction test already pinned that
  variant explicitly, which is why it stayed green and the staleness was invisible to CI.
- **Gate:** `fmt` clean, `clippy --all-targets -D warnings` clean, 507 binary + 126
  integration tests green, drift gate 4/4. End-to-end on `tests/fixtures/hdr-48bit.tif`:
  both flags reach the resolved report, each changes the image, and the emitted recipe fed
  back through `--params` reproduces the render bit-identically.

## auto-anchor-interior-measurement

**Status:** not started
**Updated:** 2026-08-03

- Goal: make `DmaxSource::Auto` measure the picture area, not the whole scan.
- 2026-08-03 (found by `algo/reference-anchored-sigmoid`): `Auto` takes the 99.5th percentile
  of corrected densities over the **whole frame**, and the nearly-opaque film holder sits at the
  `SCAN_EPSILON` floor, so its corrected density is enormous and it owns the top percentile. On
  the three fixture rolls `Auto` resolved to **2.23–2.37** against roll Dmax 1.28–1.38, and every
  frame rendered to 0/255. The fix is a sampling *region*, not a new statistic — `film_base`
  already locates the rebate by marching inward, so plumb a resolved interior in via the
  orchestrator (as the film base is). An implausible `Auto` result must fail loudly.


## sigmoid-parameter-calibration

**Status:** not started
**Updated:** 2026-08-03

- Goal: turn `reference-anchored-sigmoid`'s **provisional** parameters into calibrated ones.
- Provisional values and their firmness: contrast ≈2.07 (firmest — derived `0.745/Δ` from
  *tabulated* datasheet Δ); shoulder ≈0.6 (bends at `D′` 0.70 ≈ mid-grey 0.67, judged on ten
  frames); per-stock anchor offsets (**weakest** — from chart-read `D-min` values that are not
  true Status M densities, with systematic per-stock residuals of Ektar ≈+0.6 EV, Portra 160 ≈0,
  Gold 200 ≈−1.0); and the `NOMINAL_DMAX = 2.0` fallback (measured rolls 0.90–1.74, ≈1.35 better).
- **More frames will not fix this.** Per-frame exposure preference *is* frame optimisation, so
  only central tendency is usable. It needs a **bracketed roll** (exposure labels true by
  construction, which makes exposure-preservation verifiable rather than "consistent with"), a
  **grey card in frame** (a real 18% reference under the same illumination as a diffuse white —
  only 2 of 10 existing frames could even approximate the datasheet Δ), and ideally the
  calibrated transmission step wedge.
