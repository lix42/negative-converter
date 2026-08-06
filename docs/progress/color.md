# Negative Converter — color Progress Log

Execution log for the `color` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `color`:

- **The working space is linear Rec.709/sRGB primaries, D65, linear TRC.** That
  is a pinned assumption, not a measurement — decode gives scanner RGB with no
  input ICC, and `io/input-data-semantics` now gates conversion to
  scanner-device + linear inputs so the assumption is at least honest.
- **NC film RGB v1 is the versioned boundary out of the film rendering.**
  `working_space::map_nc_film_rgb_v1(FilmRgbImage) -> AcesCgImage` applies one
  pinned 3×3 matrix (`AP1⁻¹ · Bradford(D65→ACES) · NPM_Rec709`) in binary64,
  stored `f32`, **unclamped**, IR carried through. The same mapper serves every
  reconstruction path — no per-curve fitted matrices.
- **This is film-rendering *intent*, not physical scene recovery.** The film,
  lens, development, and scanner rendering is deliberately preserved. Any measured
  neutralization is an explicitly selected correction
  (`color/optional-color-correction-profiles`), never a default or a prerequisite
  for P3, HDR, presets, or acceptance. Two earlier characterization tasks were
  closed as superseded for exactly this reason.
- **`AcesCgImage` is compiler-enforced.** Private fields, module-private
  constructor, and its only input is a `FilmRgbImage` — so a named colour output
  cannot be handed a value that skipped the mapper.
- **The mapper is total: it returns `AcesCgImage`, not `Result`.** Non-finite
  values pass through and are counted at encode.
- **The mapper is wired into the render path for the `film-master` branch only.**
  `stages::render` dispatches on `output.preset`: `film-master` runs
  `reconstruct → map_nc_film_rgb_v1 → render_split::film_master` and skips
  `color::to_output` entirely, while the **legacy no-preset path still runs
  `reconstruct → finish_print → color::to_output` and its pixels are unchanged**
  (the golden fixtures are byte-for-byte green). The display presets that route the
  rest through the mapper are `output/{sdr,hdr}-display-rendering` / `output/presets`.
  The report stamps `working_mapping = "nc-film-rgb-v1"` on every path — provenance
  only, deliberately not a knob.
- **`to_output` does not clamp** and may hand the encoder out-of-`[0,1]` or
  non-finite values; clamping and loss counting belong to `io/encode`.
- **`film-master` is the shipped unclamped-ACEScg branch** (`--output-preset
  film-master` / `output.preset`, `pipeline::render_split::film_master`) that
  bypasses every print and display control. It rejects frame-local auto Dmax and
  every non-default downstream control **loudly**, never silently — see
  `film-base`'s summary for why the anchor must be roll-fixed. `--output-hdr`
  remains a *rendered* float TIFF and is never an alias for it.
- **The preset's rejections are checked on the *resolved value*, with exactly one
  presence exception.** A knob is rejected identically whether it came from a recipe or
  a flag, and a flag that resets a value back to its documented default is accepted
  (that is how a graded roll recipe is re-exported as a master). The exception is
  `--output-sdr`, rejected by flag **presence**: it *forces* 16-bit integer output the
  master cannot produce, and it has no recipe spelling, so there is no second
  provenance to keep in step. Don't generalize the exception, and don't remove it.
- **`nc roll` treats `output.preset` as roll-fixed**, alongside `film_base` and
  `reconstruction.curve.dmax`: a per-frame manifest override is applied but raises a
  loud, `--strict`-promotable roll warning, because it gives that frame a different
  *image class*, not just a different rendering.
- **The shared print controls exist but have no CLI-reachable consumer yet.**
  `render_split::display_source` resolves `WB → exposure → black point →
  linear_range` once and hands both branches the same borrowed buffer; because no
  display preset is accepted, a non-default `print.linear_range` is a loud usage
  error rather than a silently-ignored knob. `output/{sdr,hdr}-display-rendering`
  are its consumers.
- **`pipeline/colorimetry/` is the single source of truth for every
  standards-based matrix and luma vector.** No stage may define its own: import
  from `colorimetry::pinned`. The runtime **never derives** — the binary64
  derivation and the audit harness are `#[cfg(test)]`, so rendering stays
  independent of an installed ICC/CMM and of any per-run computation. Product
  policy (reference white, peak nits, shoulder, gain-map offsets) still belongs
  to the stage that owns it, but must refer to a *named* space rather than
  restate its colorimetry. Changing anything there follows
  [`docs/colorimetry-maintenance.md`](../colorimetry-maintenance.md);
  `NC_COLORIMETRY_REGEN=1 cargo test colorimetry::audit` regenerates the audit
  artifact and **only** that — it never rewrites the runtime literals, so it
  cannot silently move pixels. Four things downstream epics will trip on:
  **(a)** two Bradford conventions coexist on purpose — the frozen
  `nc-film-rgb-v1` mapping needs Lindbloom's published inverse and new artifacts
  must use the exact one; **(b)** the three luma vectors have three different
  provenances and three different verification rules — `BT2020_LUMA` is a
  normative table (deliberately *not* matching a derivation), `DISPLAY_P3_LUMA`
  is an exact derivation, and `SRGB_LUMA` is the derivation rounded to six
  decimals (43 ulps out, with its own allowance); **(c)** the pinned-vs-derived
  check tolerance is ±1 `f32` ulp, measured against the fact that the
  chromaticities' own three-decimal rounding moves entries ~3,500 ulps;
  **(d) `pinned.rs` is not the only runtime consumer of a definition** —
  `pipeline::color` feeds `REC709`, `DISPLAY_P3`, `ACESCG`, and `PROPHOTO`
  straight into Little CMS, so editing one of those four is a pixel change even
  with `pinned.rs` untouched and every audit ulp at 0, and nothing automated
  catches it (the drift gate stops before lcms2; the audit only compares pinned
  artifacts). `output/lossless-hdr-tiff` depends on this epic so BT.2020 TIFF
  profiles reuse these definitions instead of adding a third generation of
  duplicated coefficients — and **(d) applies directly to it**, since a BT.2020
  output profile would add a fifth lcms2-consumed space.
- **Note:** the log entries for `scanner-profile-before-density-experiment` are
  stranded at the tail of the `input-data-semantics` section in
  [`io.md`](io.md) — they lost their heading in the flat log before this epic
  split. Read them there.


## management
**Status:** done
**Updated:** 2026-06-21

- Goal: working→output ICC transforms with depth-aware default profile (sRGB for
  u16, wide-gamut for f32); provide the ICC blob to embed.
- **Done.** `pipeline/color.rs` implemented over `lcms2` 6.1.1 (API verified via
  Context7 + crate source, not memory). Public surface: `OutputSpace` enum
  (`SRgb`/`ProPhoto`/`AcesCg`/`Custom(PathBuf)`) with `OutputSpace::parse`,
  `resolve_output_space(explicit, depth)`, `icc_profile(space) -> Vec<u8>`, and
  the foundation-established `to_output(&LinearImage, &OutputParams) ->
  (LinearImage, Vec<u8>)` (kept verbatim — orchestration depends on it).
- **Decisions (these are the task's open questions, now resolved):**
  - **Working space = linear Rec.709/sRGB primaries, D65, linear TRC.** Decode
    gives "linear scanner RGB" with no input ICC in Step 1, so the source
    colorimetry had to be pinned to build any transform. Synthesized as the
    transform's source profile. The `--input-profile`/`--assume-linear` knobs
    (`InputColor`, added by cli-framework) are parsed into config but not yet
    applied; any input→working conversion will live upstream in decode/
    orchestration, so this fixed working space still holds.
  - **f32 wide-gamut default = `AcesCg`** (AP1 primaries, ~D60 white, **linear**
    TRC — scene-referred, avoids clipping HDR range). u16 default = `SRgb`.
    (User confirmed ACEScg over ProPhoto/Rec.2020.)
  - **TRC is a property of the space, not the output depth** — every embedded
    profile self-describes its data. `SRgb`→sRGB curve (display), `ProPhoto`→
    ROMM/D50 gamma 1.8 (display), `AcesCg`→linear (scene). So an explicit
    `--output-profile prophoto` is always a valid encoded profile regardless of
    `--out-depth`.
  - **This stage does not clamp.** A gamut remap can push values outside
    `[0, 1]`; range clamping + clipping warnings are the encoder's job
    (`tiff-encode`), per "fail loudly". Note left for that task.
  - Intent: `RelativeColorimetric`. Transform runs on the interleaved `f32` RGB
    buffer in `[f32;3]` chunks via `transform_in_place` (no extra copy beyond the
    one `image.clone()`); IR plane carried through untouched.
  - `Custom` profile load/parse failures map to `NcError::Usage` (exit 2);
    transform/serialize failures to `NcError::Other`.
- **Notes for dependent tasks:**
  - `tiff-encode`: `to_output` returns the ICC blob to embed and may hand you
    out-of-`[0,1]` values — clamp at encode for u16 and surface clipping as a
    report warning. f32 output (AcesCg) is **linear/scene-referred**; sRGB output
    is **display-referred** (already tone-curved).
  - `cli-framework`: `--output-profile` string → `OutputSpace::parse` (keywords
    `srgb`/`prophoto`/`acescg` case-insensitive, else treated as an ICC path).
- **Verify:** `cargo test` 13 color tests pass (whole suite 40/40): resolve
  defaults + explicit override, keyword/path parse + misspelled-keyword rejected,
  linear 0.5 → sRGB ~0.7353, sRGB round-trip within 0.005, ICC bytes
  valid+re-openable for all built-ins, custom-from-disk load+transform,
  missing-path and garbage-ICC → exit 2, IR pass-through preserved, f32/AcesCg
  transform runs, wide-gamut saturated-red primaries remap. `cargo fmt --check`,
  `clippy --all-targets -D warnings` clean.
- **Review fixes (ship, 2026-06-21):** multi-agent review applied —
  (1) `OutputSpace::parse` is now fallible: a misspelled bare-word keyword
  (`prophooto`) is a loud `Usage` error instead of a deferred "cannot read ICC"
  path error; (2) the chunk-remainder guard is a real runtime check (was
  `debug_assert!`, which compiled out in release → risked a quietly-wrong tail);
  (3) `Custom` profiles are validated as RGB color space (else `Usage`), so a
  CMYK/Lab/gray profile fails clearly rather than with an opaque transform error;
  (4) `icc_profile` and `to_output` share a `profile_icc(&Profile)` helper — no
  duplicated `.icc()` string and, per PR #7 review, no rebuilding/re-reading the
  output profile it already holds.
- **Deferred follow-up for `pipeline-orchestration`/`main`:** lcms2
  `transform_in_place` can't return an error — Little CMS reports runtime
  transform failures (OOM-class) only through the process-global
  `cmsSetLogErrorHandler`. A pure stage can't own a process-global handler, so
  **`main`/`cli` must install one at startup** (lcms2 `ThreadContext::
  set_error_logging_function`) to turn those into loud errors. Tracked here so
  orchestration wires it.


## post-reconstruction-color-characterization
**Status:** closed—superseded
**Updated:** 2026-07-23

- 2026-07-23: Closed the artifact-based characterization runtime without
  implementation. Physical scene recovery is not NC's goal: the rendering
  contributed by the film stock, lens, development, and scanner is intentional
  by default. Measured neutralization, if added later, must be an explicitly
  selected correction rather than a prerequisite for P3, HDR, presets, or
  display acceptance. Replacement film-preserving reconstruction,
  working-space, and render-pipeline tasks will be defined separately.

- 2026-07-21: Added the missing production boundary from reconstructed
  scanner/film RGB to defined linear ACEScg. It corrects channel mixing and
  nonlinear color error beyond white balance; assigning an output ICC alone is
  explicitly not characterization.
- 2026-07-21: Clarified the stage boundary after review: reconstruction stops at
  unclamped positive scanner/film RGB, characterization maps that into linear
  ACEScg, and only then do white balance, exposure, black point, highlight
  compression, and output rendering run. The task includes splitting the current
  combined algorithm/print-render boundary.
- 2026-07-21: Split this mega-task. It now owns only runtime types, artifact
  loading/versioning, explicit provisional fallback, stage ordering, and the
  direct scene-master branch. Offline target fitting and measured model selection
  moved to `color-characterization-calibration`.
- 2026-07-21: Made the display-branch boundary explicit: the runtime exposes one
  shared linear adjustment stage for WB/exposure/black placement. SDR and HDR use
  identical resolved adjustments, then diverge for their own highlight/tone/gamut
  policy; `scene-master` bypasses both.
- 2026-07-21: Narrowed runtime ownership further and made color semantics
  fail-loud: the named-output fallback is a versioned assumed linear Rec.709/D65
  → ACEScg/D60 transform, while identity device RGB is only an untagged custom
  diagnostic. Pinned artifact operation/schema/hash validation and a canonical
  reconstruction-domain compatibility contract covering all sensitive params.
  Render-stage refactoring moved to `post-characterization-render-pipeline`.
- 2026-07-21: Corrected that compatibility contract after a second review. A
  reusable scanner/film/development artifact binds coordinate-defining algorithms,
  operation policies, and settings—not incidental measured Dmin/Dmax values,
  regions, or confidence. Those stay reported runtime provenance unless an
  artifact explicitly declares narrower roll scope. Artifact/contract digests
  now omit their own field and use RFC 8785 canonical JSON plus SHA-256.
- 2026-07-21: Final coordinate audit found density Dmax could not simply be
  omitted while it remained ahead of nonlinear artifact curves. The target now
  characterizes Dmax-neutral `10^(gamma*D')`, then applies
  `10^(-gamma*Dmax)` as a scalar ACEScg placement. Sigmoid v1 instead scopes the
  artifact to exact fixed Dmax because its curve shape changes; simple pins its
  unclamped affine inversion and has no Dmax.
- 2026-07-21: Preserved `f32` image buffers while requiring `f64`/equivalent
  extended-range scalar evaluation across the unanchored density artifact path;
  this avoids undoing the current anchored exponent's overflow protection before
  the downstream Dmax gain can cancel scale.
- 2026-07-21: Corrected Dmax semantics for nonlinear characterization. The
  characterization runtime now owns one fused, private extended-range
  `U → artifact → scalar placement` operation and returns placed `f32` ACEScg.
  Post-artifact Dmax is deterministic roll exposure placement, not a guaranteed
  white-at-1 anchor; display reference white belongs to the SDR/HDR renderer.
- 2026-07-21: Corrected the simple canonical boundary: target characterization
  now receives raw unclamped `1 - scan/Dmin`, not the shipped renderer's later
  inversion-WB and clip-affine result. Those adjustments are downstream render
  controls and no longer affect simple artifact identity.


## film-rgb-working-space
**Status:** implemented (uncommitted in worktree; not shipped)
**Updated:** 2026-07-24

- 2026-07-24 (implementation): **Shipped the NC film RGB v1 mapper** in a new
  pure module `src/pipeline/working_space.rs`:
  `map_nc_film_rgb_v1(FilmRgbImage) -> AcesCgImage`. The mapping is a single
  pinned 3×3 matrix `M = NPM_AP1⁻¹ · Bradford(D65→ACES) · NPM_Rec709` applied per
  pixel in **binary64**, stored `f32`, unclamped, IR carried through. The **same**
  mapper serves simple, density/exponential, and density/sigmoid (no fitted
  curves/matrices — film/lens/development/scanner/curve differences are
  preserved).
  - **Pinned matrix** (const `NC_FILM_RGB_V1_TO_ACESCG`, f64), from Rec.709
    primaries + D65 `(0.3127,0.3290)` → AP1 primaries + ACES white
    `(0.32168,0.33767)`, **Bradford** CAT (Lindbloom cone matrix), operation order
    `AP1⁻¹ · CAT · Rec709`. Rows:
    `[0.6130974, 0.3395231, 0.0473795]`,
    `[0.0701937, 0.9163540, 0.0134523]`,
    `[0.0206156, 0.1095697, 0.8698146]`. Row sums = 1 (neutral→neutral, so the
    white point is correctly adapted); coincides with the published
    sRGB-linear→ACEScg Bradford matrix (colour-science/OCIO) as an external check.
    Derived and cross-checked with a standalone f64 program before pinning.
  - **Typed boundary:** `AcesCgImage` has private fields and a **module-private**
    constructor `new`, so `map_nc_film_rgb_v1` (same module) is the *only*
    producer — nothing outside can mint one, and a named output that accepts an
    `AcesCgImage` therefore can't be handed a value that skipped the mapper. Its
    only input is a `FilmRgbImage` (itself only mintable by `algo::reconstruct`),
    so "cannot attach a named profile directly to `FilmRgbImage`" is
    compiler-enforced. Following the `algo/mod.rs` precedent, no `trybuild`
    dev-dep was added; the privacy annotations are the guarantee (documented in a
    test comment). Read side: `width/height/rgb/ir` accessors + `pub(crate)
    into_linear` for the future named-output/film-master encode.
  - **Report identity:** added top-level report field `working_mapping`
    (`Option<&'static str>`), stamped `"nc-film-rgb-v1"` on every convert
    (`working_space::WORKING_MAPPING_ID`), matching design-spec §8. It is a fixed
    constant, **not** a tunable knob, so it is deliberately *not* a CLI flag /
    recipe key (§9 assigns it no recipe home) — provenance only. A future mapping
    is a new identifier under `conversion-versioning`.
  - **Legacy path untouched:** the mapper is defined + tested but **not yet wired**
    into `stages::render` — the legacy no-preset path still goes
    `reconstruct → finish_print → color::to_output` and its pixels are unchanged
    (all `pipeline::stages::golden` fixtures still green). Wiring happens in
    `film-master-render-pipeline` / `output-presets` when named presets move stage
    4 after the ACEScg boundary. The stamped `working_mapping` is still accurate:
    the "interpret film RGB as linear Rec.709/D65" rule *is* what every path
    already applies; the typed mapper is its realization for the preset consumers.
  - **Tests** (7 unit in `working_space` + 2 integration assertions):
    `matrix_matches_independent_bradford_derivation` (const == in-test
    from-primaries f64 derivation, <1e-12); `neutral_maps_to_neutral_rows_sum_to_one`
    (external ground truth for the adaptation); `pinned_vectors_match_binary64
    _reference_within_2e_minus_6` (primary/neutral/saturated/negative/above-one vs
    independent f64 `matvec`, ≤2e-6, no named renderer/profile invoked);
    `every_reconstruction_path_uses_the_same_mapper...`;
    `mapping_is_deterministic_across_repeat_runs_and_configs` (per-pixel bits, no
    full-frame/post-lcms2 checksum per the cross-platform caveat);
    `nonfinite_and_out_of_range_pass_through_unclamped`; `ir_absent_stays_absent`.
    Integration: `convert_simple_...` and `convert_sigmoid_...` pin
    `report["working_mapping"] == "nc-film-rgb-v1"` on both paths.
  - **API-signature note for dependents:** `map_nc_film_rgb_v1` returns
    `AcesCgImage` directly, **not** `Result` — the mapping is total (pure matrix
    multiply; non-finite passes through and is counted at encode, never errored
    here). Updated the design-spec §7 sketch (was `Result<AcesCgImage>`,
    "target") to match the shipped total signature.
  - Gate green in the worktree: `cargo fmt --all --check`, `clippy --all-targets
    -D warnings`, `cargo test` = **332 unit + 86 integration, 0 failed**. Left
    uncommitted per instructions; `TASKS.md` checkbox not flipped.

- 2026-07-24 (review fixes): Addressed the `film-rgb-working-space` review round.
  (1) **Doc ship-state reconciled** — the type + mapper are implemented but
  uncommitted and **not yet wired into the render path** (no non-test callers of
  `map_nc_film_rgb_v1`; only the `working_mapping` report field is wired, via the
  `WORKING_MAPPING_ID` constant). Reworded the design-spec §7 interface sketch
  comment away from "Shipped", made the §4 table row (was "planned working-space
  mapper") and the §7 prose (was "future working-space mapper") consistent, and
  changed the §9 note "`working_mapping` lands with…" to present tense. Reworded
  `working_space.rs` "the only error…is the final f32 rounding" → "only
  *significant* error", noting the <1e-12 pinned-vs-derived delta.
  (2) **External cross-check now test-enforced** — added
  `matrix_matches_published_srgb_to_acescg`: a hardcoded published sRGB-linear →
  ACEScg Bradford matrix (colour-science / OCIO ACES, 4-dp) asserted against the
  pinned const within 1e-4 (max observed diff 5.2e-5). A genuinely independent
  oracle (the existing derivation re-uses the const author's primaries/white).
  (3) **Multi-pixel value assertion** — added
  `multi_pixel_values_match_binary64_reference_within_2e_minus_6` (3 distinct
  pixels vs the binary64 `derived_matrix`/`matvec`, ≤2e-6) to guard chunk-boundary
  regressions; prior value fixtures were 1×1 only. Out of scope (accepted
  decisions, untouched): `working_mapping` stays report-only, mapper stays
  `-> AcesCgImage`. Gate green in the worktree: `cargo fmt --all --check`,
  `clippy --all-targets -D warnings`, `cargo build`, `cargo test` = **334 unit +
  86 integration, 0 failed**. Left uncommitted.

- 2026-07-23: Retired `docs/design-spec.html` as a maintained companion. The
  Markdown design spec is now the sole source; HTML may be regenerated after the
  feature roadmap stabilizes.
- 2026-07-23: Defined NC film RGB v1 as the existing intentional linear
  Rec.709/D65 interpretation followed by the pinned transform/adaptation to
  ACEScg/D60. This is NC's film-rendering intent, not a provisional physical-
  scene claim.
- 2026-07-23: Added planned private-field `FilmRgbImage` → `AcesCgImage`
  boundaries, recipe/report mapping identity, unclamped vector fixtures, and a
  fail-loud prohibition on direct film RGB tagging as a named color output.
- 2026-07-23: Kept working-space verification local to direct pinned
  Rec.709/D65 → ACEScg/D60 matrix/adaptation vectors. Cross-encoding and
  independent display decode-back belong exclusively to display acceptance.


## film-master-render-pipeline
**Status:** done
**Updated:** 2026-07-28

- 2026-07-23: Replaced `scene-master` with `film-master`: unclamped linear
  ACEScg containing the intentional film/lens/development/scanner rendering, not
  physical scene-linear recovery. Reconstruction, density curve, and supported
  Dmax placement remain in the master; later print/display controls are bypassed.
- 2026-07-23: Pinned fail-loud rejection of frame-local auto Dmax and non-default
  downstream controls. Named display outputs share WB → exposure → black/range
  placement after ACEScg before branching into SDR/HDR renderers. Legacy
  no-preset TIFF ordering remains unchanged until preset migration.
- 2026-07-23: Clarified that the optional correction task may insert an
  explicitly selected correction immediately before the split. A corrected
  unclamped ACEScg master remains `film-master` but must identify the correction;
  the default master has no profile and this task does not depend on one.

- 2026-07-27 (implementation, uncommitted in worktree): **Shipped the named-output
  split.** The mapper from `film-rgb-working-space` is now wired into the render
  path for the first time — the Epic summary above was rewritten in this same change
  to say so, replacing its earlier "produced but not yet wired" caveat. Only the
  `film-master` branch crosses the mapper; the *legacy* path still does not, and its
  pixels are untouched.
  - **New module `src/pipeline/render_split.rs`** — the whole split, pure functions
    only:
    - `film_master(AcesCgImage) -> LinearImage` is a **pure unwrap**. That is the
      definition of the master, so any operation added there would be a bug; it does
      not even *take* a `PrintParams`, so a control cannot leak in by accident.
    - `resolve_shared_controls(&AcesCgImage, &PrintParams) -> ResolvedPrintControls`
      resolves the shared controls **once** (an `auto` WB becomes concrete gains
      there), `apply_shared_controls` applies them in the pinned order
      `WB → exposure → black point → linear_range`, and `display_source` composes the
      two into one `SharedDisplaySource`. Both branches then **borrow** that buffer
      via `branch_source(&shared, DisplayBranch)` — so "SDR and HDR receive the
      identical adjusted source" is structural (pointer equality in the test), not a
      convention two renderers must remember.
    - `AdjustedAcesCgImage` has private fields + a module-private constructor, so
      `apply_shared_controls` is its only producer. A display renderer that accepts
      one cannot be handed a buffer that skipped the shared stage — and cannot be
      handed the master either, which is a plain `LinearImage` and never this type.
    - `highlight_compress` is deliberately **not** shared: highlight roll-off is
      branch-specific SDR tone policy. Pinned by a test (a hot sample stays hot).
  - **`AcesCgImage`-only boundary.** Every function in the split takes an
    `AcesCgImage`, whose constructor is private to `working_space` and whose only
    input is a `FilmRgbImage` — so film RGB and raw device RGB *cannot* reach a named
    output. That is compiler-enforced (documented in a test comment; no `trybuild`
    dev-dep, per the `algo/mod.rs` precedent). The split stays **producer-agnostic**
    as the task requires, but every fixture here constructs its input through the
    direct uncorrected `reconstruct → map_nc_film_rgb_v1` path. **No** correction
    profile selection/stage/provenance is implemented — that stays
    `optional-color-correction-profiles`.
  - **`stages::render` now dispatches on the resolved preset.** `legacy` keeps the
    frozen `reconstruct → finish_print → color::to_output` ordering (all 11 golden
    fixtures still green, byte-for-byte); `film-master` runs
    `reconstruct → map_nc_film_rgb_v1 → film_master` and **skips `color::to_output`
    entirely**, fetching only the ACEScg ICC blob. That skip is load-bearing:
    `to_output` would re-apply the Rec.709→ACEScg matrix to values that already
    crossed it. `ConvertReport::white_balance` stays `None` on the master by
    construction — reporting gains for a branch that applied none would be a false
    provenance claim — while the reconstruction's own `dmax`/`balance_range` *are*
    reported, because they are part of what the master contains.
  - **New knobs (both fully four-spot wired, with merge tests):**
    - `--output-preset <legacy|film-master>` / `output.preset` (§9 *Output / encode*)
      — one `OutputPreset` enum field, never parallel bools. `legacy` **is** the
      no-preset state (byte-identical to passing nothing, and still compatible with
      the legacy selectors); `film-master` is *named* and therefore atomic.
      `OutputParams::depth()` returns `F32` for the master by definition, not via
      `output.hdr`.
    - `--linear-range LOW,HIGH` / `print.linear_range` (§9 *Print / tone render*),
      default `[0,1]` = the exact identity. Atomic pair; validated finite,
      `low < high`, and **representable span** (two finite endpoints can still
      overflow their difference and silently collapse every sample — the same trap
      `--balance-range` has).
  - **Fail-loud, never-silent rejections** (all on the *resolved* config, so source
    doesn't matter; all exit 2):
    - `film-master` + frame-local `auto` Dmax → rejected for either curve, with the
      roll-fixed alternatives named. Supported anchors are pinned **by curve type**:
      exponential `fixed`/explicit/`none`, sigmoid `fixed`/explicit, simple none.
    - `film-master` + any non-default print control → rejected, naming the offender.
      The sweep **destructures** `PrintParams` rather than field-accessing it, so a
      future control makes `validate_output_preset` fail to compile and forces an
      explicit decision — a field-access sweep would silently omit it and reintroduce
      exactly the silent-ignore this rule exists to prevent.
    - Named preset + legacy selector → rejected **twice on purpose**: by flag
      *presence* (so `--output-sdr`, whose value equals the default, still errors) and
      by resolved *value* (which is what catches a recipe-sourced one). clap
      `conflicts_with` can't express this, because `--output-preset legacy` is
      legitimately compatible with all four.
    - A flag that resets a recipe control back to its documented default **is**
      accepted — that is how a graded roll recipe gets re-exported as a master
      without editing it. Pinned by a test.
    - `scene-master` → rejected as an unreleased-schema break naming the rename, with
      an explicit "there is no alias". Planned names (`gain-map-hdr`, `display-p3`,
      `compatibility`, `hdr-pq`, `hdr-hlg`, `custom`) get their own "not accepted
      yet" message so an agent can tell *not yet* from *typo*. Flag and recipe key
      share one parser (`OutputPreset::parse`, a custom `Deserialize` delegating to
      it), so a name is diagnosed identically wherever it appears.
    - Non-default `print.linear_range` → rejected **on both branches**. Only the
      shared display stage applies it, no display preset is accepted yet, the legacy
      ordering is frozen, and the master bypasses print controls — so every path
      would silently ignore it. See the scope note below.
  - **New report block `output_render`** (§8): `preset`, `print_controls`,
    `display_render`, `encoding`
    (`rendered-u16-tiff` | `transitional-rendered-float-tiff` |
    `unclamped-linear-acescg-float-tiff`), `content`, `working_mapping`,
    `reconstruction_schema_version`. `print_controls` means "the stage ran at all",
    so it is `false` for the master *and* for legacy `simple`. The master's `content`
    states the intentional film rendering and explicitly disclaims physical scene
    recovery. `pipeline_version` is **deliberately absent** rather than guessed —
    `core/conversion-versioning` is still `[ ]` and owns stamping it.
  - **Determinism / cross-platform care:** no test checksums a whole encoded TIFF, a
    full frame, or post-lcms2 pixels. Bit-identity is pinned only with small curated
    per-pixel `to_bits()` vectors (the `pipeline::stages::golden` style); the e2e
    float-TIFF test asserts sample format + value magnitudes, and the two byte-equal
    file comparisons it makes are *same-build A-vs-B* reruns, not checked-in hashes.

- 2026-07-27 (scope call — read this before `output/{sdr,hdr}-display-rendering`):
  **The removed simple controls stay rejections, not warned aliases.**
  Design-spec §7.1/§9 describe `--invert-white-balance` / `--clip-low` /
  `--clip-high` becoming warned aliases at "preset migration", and the task's
  How-to-Verify allows "old-key **rejection or** migration diagnostics". Chose
  rejection, with the message upgraded to name the concrete replacement that now
  exists (`--white-balance R,G,B` / `--linear-range LOW,HIGH`) and to state that the
  *value* carries over but the *pixels* do not. Why: the alias's target,
  `print.linear_range`, has **no consumer in this build** — the legacy ordering is
  frozen and `film-master` bypasses print controls — so an alias could only ever emit
  a migration warning and then hard-error on the same invocation. A single precise
  rejection is better than warn-then-fail, and §7.1's own wording ties alias
  activation to *named display presets applying them after NC film RGB mapping*,
  which is the SDR/HDR tasks, not this one. **What the display tasks inherit:** relax
  the `linear_range` rule in `cli::validate::validate_output_preset` (rule 2) for
  their presets, then implement the alias contract §7.1 specifies — atomic flag
  conflicts with either legacy flag, `--clip-low`/`--clip-high` independently
  override their endpoint, warn, and report endpoint provenance. Note `merge` has no
  warnings sink today, so the warning channel needs threading (or the alias
  resolution needs to move into `convert_frame`).
  The **value/pixel boundary is already pinned**:
  `render_split::wb_gains_do_not_commute_with_the_working_space_matrix` shows
  non-uniform gains give different pixels before vs after the matrix (and that a
  *uniform* gain does commute, so the difference is genuinely the ordering). Never
  promise bit-identical migration.

- 2026-07-27 (notes for dependent tasks):
  - **`output/sdr-display-rendering` / `output/hdr-display-rendering`:** your entry
    point is `render_split::display_source(AcesCgImage, &PrintParams)` →
    `SharedDisplaySource`; take your buffer with `branch_source(&shared,
    DisplayBranch::{Sdr,Hdr})`. Do **not** re-resolve the controls or re-estimate an
    auto WB — that is the invariant. `shared.controls` tells you what already ran.
    Reference white, highlight/tone behaviour, destination gamut mapping, and
    transfer encoding are yours alone; the shared stage clamps nothing and
    compresses no highlights. Adding a preset also means teaching
    `stages::render` a new arm and relaxing the `linear_range` rule.
  - **Auto-WB domain shift:** the legacy estimator runs on pre-matrix film RGB;
    `resolve_shared_controls` runs the *same* estimators on mapped ACEScg, so an
    `auto` mode resolves to **different numbers** there. That is the documented
    consequence of moving the controls after the boundary, not a bug — but it means
    a display preset's auto WB is not comparable to the legacy report's gains, and
    `conversion-versioning` owns the resulting `pipeline_version` bump when a preset
    changes default pixels.
  - **`color/optional-color-correction-profiles`:** the split is producer-agnostic
    over `AcesCgImage` exactly so you can insert a correction between the mapper and
    `film_master`/`display_source` without touching either. Nothing in
    `render_split` inspects provenance.
  - **`output/presets`:** `OutputPreset::parse` is the single place to add a name,
    and it already emits a pinned "not accepted yet" message for each of yours.
    Output-path *suffix* validation against a preset's resolved container is still
    unimplemented (`film-master` is a TIFF, so the existing `.tif`/`.tiff` rule
    covers it, including `nc roll`'s `<stem>_positive.tiff`). A `film-master` roll
    recipe works today.
  - **Gates green in the worktree** (`cargo fmt --all --check`, `clippy
    --all-targets -D warnings`, `cargo build`, `cargo test`): **382 unit + 91
    integration, 0 failed**. Left uncommitted; `TASKS.md` set to `[~]`, not `[x]`.

- 2026-07-27 (review fixes, still uncommitted in the same worktree): six independent
  review engines ran over the uncommitted diff; the verified findings are fixed here.
  Three were behavioural.
  - **Preset atomicity unified on resolved-value semantics; the flag-presence rule is
    deleted.** `reject_preset_flag_conflicts` is gone. It checked flag *presence* and
    early-returned when the preset came from a recipe, while `validate_output_preset`
    checked the *resolved value* — so a selector whose resolved value equalled its
    default fell through both, and the *same user intent got opposite outcomes
    depending only on provenance*. Measured before the fix (`hdri-64bit.tif`,
    `--film-base 0.9,0.55,0.42`): a recipe `{"output":{"preset":"film-master"}}` plus
    `--output-sdr` exited **0** and wrote 2 783 902 bytes (4 bytes/sample f32) — a user
    asking for a 16-bit SDR TIFF silently received unclamped float ACEScg — while the
    *flag* preset plus `--output-sdr` exited 2. `--bigtiff auto` and an explicit
    `"hdr": false` were likewise silently ignored.
    **Why value semantics and not a mirrored presence rule:** the task spec's own
    wording is "rejects every **non-default** downstream control"; `PrintParams`
    already works exactly this way and is documented and tested
    (`film_master_accepts_a_recipe_whose_controls_a_flag_resets_to_default`), so the
    output side was the odd one out; §5's documented escape hatch ("a roll recipe
    carrying print controls can still be re-exported as a master") *requires* that a
    flag resetting a value to its default be accepted, and for `output.hdr` the reset
    flag is `--output-sdr` — which the presence rule rejected, making that a dead end;
    and mirroring presence for recipe keys would mean probing raw JSON per key (the
    `LoadedRecipe::curve_dmax_present` machinery) purely to reproduce a rule that only
    the resolved value can state. **Consequence, deliberately accepted:** under the
    preset, `--output-sdr` / `--bigtiff auto` / `"hdr": false` are accepted and still
    produce the f32 master — they resolve the documented defaults, so they ask the
    preset for nothing it does not already do. All four previously-incoherent cases now
    behave identically (exit 0, 2 783 902 bytes), and `hdr: true` is exit 2 from either
    provenance.
    Mechanically: the sweep moved to `OutputParams::non_default_legacy_selector`
    (types.rs), which **destructures** — a new output selector fails to compile there,
    the same guard the print sweep already had — and returns the offender so the error
    blames one selector instead of listing all three (the old message listed all three
    unconditionally, which made "did we blame the right one?" tests vacuous). The rule
    is now gated on `OutputPreset::is_named()`, not on `FilmMaster`, so the next named
    preset inherits recipe-side protection; messages take the name from the new
    `OutputPreset::name()` instead of hardcoding `"film-master"`.
    **What the display/preset tasks inherit:** there is one atomicity rule, by value,
    for every named preset. Do not re-add a presence check.
  - **Telemetry recorded the wrong depth for every master, and could not name the
    branch.** `output_hdr` read `cfg.output.hdr`, which the preset pins at `false`
    while `depth()` returns `F32` — verified: `"output_hdr": false` next to
    `"output_bytes": 2783902`. It now derives from `cfg.output.depth() == F32`, the
    single place a recipe becomes a depth (its own doc claimed to be that single
    place; telemetry was a fourth consumer bypassing it). Added
    `conversion.preset`, without which a master is indistinguishable from a legacy
    u16 run except by file size. **`SCHEMA_VERSION` 2 → 3**; design-spec §9's record
    shape and the pinned wire-shape snapshot both updated. There was zero telemetry
    coverage of the preset; there is now a unit snapshot plus an e2e test that
    cross-checks `output_hdr` against the bytes actually written.
  - **The master accepted one frame-local measurement while rejecting its sibling.**
    Rule 1a's rationale for rejecting `DmaxSource::Auto` ("measures the anchor per
    frame … breaks the cross-frame consistency the master exists to preserve") applies
    verbatim to `reconstruction.density.balance_range`, which defaults to
    `BalanceRange::Auto` and is measured from *this* frame's 0.5/99.5 corrected-density
    percentiles. `--output-preset film-master --shadow-balance 0.1,0,0` exited 0 with
    no warning, so two frames of a roll got different ramp anchors. Now rejected — but
    **only when the range is actually consulted**, because `regional_balance`
    short-circuits before measuring whenever the two balances are equal (neutral, or
    equal-but-non-neutral, which collapses to a tone-independent offset). That
    predicate is `algo::density::consults_balance_range`, deliberately placed beside
    the short-circuits it mirrors, with a unit test asserting it agrees with
    `regional_balance`'s observable `Some(range)`/`None` — the default `Auto` range
    stays accepted, which every default master depends on.
  - **`ResolvedPrintControls` now has a checked constructor.** It was all-public
    fields feeding an infallible divide by `high - low` behind a comment claiming CLI
    validation — untrue even of this module's own tests, which pass `[0.5,1.5]` /
    `[0.0,0.5]` that `cli::validate` rejects. Fields are private, `new` is the sole
    constructor (`resolve_shared_controls` already returned `Result` and did not use
    it), and it validates finite positive WB gains, a finite non-zero
    `exposure_gain`, a finite `black_point`, and finite `low < high` with a positive
    representable span. This also closes a real reachable overflow: `--print-exposure
    200` is `finite()`-valid but `2^200` is `inf`. Accessors (`white_balance()` …)
    replace the public fields — **`output/{sdr,hdr}-display-rendering` read them
    through the accessors now**.
  - **`branch_source` deleted.** Its whole body was `let _ = branch; &shared.source`,
    and the test "proving" SDR/HDR see the same buffer compared one reference against
    itself (`ptr::eq(sdr, hdr)` where both came from that pass-through) — it could
    never fail. Branch-independence is **structural**: `SharedDisplaySource` owns
    exactly one `AdjustedAcesCgImage` whose constructor is module-private, so there is
    no per-branch buffer to diverge; that is now stated in a comment instead of
    asserted, and the test pins what *is* falsifiable — the single buffer really
    carries the resolved controls, recomputed independently.
    **Display tasks: take `&SharedDisplaySource` (or `&shared.source`) directly**;
    `DisplayBranch` remains as the seam you match on to pick a renderer.
  - **Legacy-branch pixel freeze pinned at the boundary this change introduced.**
    `stages::golden` calls `reconstruct_and_print` **directly**, so it never crosses
    the new `match output_params.preset`, and the e2e legacy test compares legacy to
    legacy — *swapping the two match arms left every one of them green*. New in-process
    test `legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence` asserts
    `render(…, &OutputParams::default())` equals
    `color::to_output(reconstruct_and_print(…))` bit-for-bit (pixels, IR, ICC, report)
    across all three reconstructions and three output configs. In-process only, so it
    is not the cross-target ICC/post-lcms2 trap.
  - **Report no longer claims a Dmax placement that need not exist.** `output_render`'s
    `content` was a static string ending "and the resolved roll-fixed Dmax placement",
    but validation deliberately accepts exponential `dmax = none` and `simple` has no
    anchor. It is now conditional on `master_places_dmax`, with unit + e2e coverage
    (the old e2e only ran `--d-max 0.2`).
  - **Tests that could not fail were removed or repaired**, beyond the two above:
    `film_master_render_is_f32_regardless_of_the_output_hdr_switch` never called
    `render` (it asserted `!params.hdr` on a literal it had just built) and now flips
    the switch through `render` and demands bit-identical output; the `pipeline_version`
    absence assertion (a field the struct does not declare) became an exact key-set
    assertion; `legacy_hot > master * 1.5` now first asserts `master > 0.0`, since
    post-matrix ACEScg legitimately goes negative; two fixture-arithmetic-against-
    literals asserts and a single-process `assert_eq!(run(), run())` determinism test
    are gone (the GrayWorld coverage the latter carried moved into the auto-WB test,
    which now loops both estimators); the `linear_range` and auto-Dmax rejection tests
    now assert phrases distinctive to the rule they name — `contains("auto")` also
    matched `balance_range: "auto"`, a film-base `"auto"`, and `--auto-wb`, and both
    `linear_range` rules mention `linear_range`, so each test stayed green with its own
    rule deleted. `film_master_rejects_frame_local_auto_dmax…` also now pins sigmoid +
    `dmax: none` as rejected (validation order previously left that unpinned).
  - **Coverage added:** a master run whose written TIFF contains a **negative** sample
    (the old test proved "unclamped" only upward; note the `clipped_*` counters are
    structurally 0 on the f32 path, so they were never the proof the comment claimed);
    an end-to-end check of the master's embedded **ICC tag**, compared against what the
    *same binary* embeds for `acescg` and shown to differ from `srgb` — never a
    checked-in ICC hash; and the `--export-ir` sidecar under the preset, asserting its
    `SampleFormat`/`BitsPerSample` flip to f32 while the IR samples requantize back to
    the legacy u16 plane (carried, not consumed). That depth coupling is now documented
    in design-spec §9's `--export-ir` entry.
  - **Documentation corrections** (each verified wrong, not stylistic): `render_split`'s
    module header and closing comment claimed a `FilmRgbImage` "cannot reach a named
    output … there is no signature that accepts one" — false, `io::encode(&LinearImage,
    &OutputParams, …)` accepts exactly that pairing; only *construction* is
    compiler-enforced (`AcesCgImage`'s private fields + module-private `fn new`), which
    is what both now say, as does `working_space`'s matching overclaim. "Every function
    here takes an `AcesCgImage`" was false for `branch_source` (now deleted). The
    "guard it loudly like `color.rs` / `working_space.rs` do" comment named the wrong
    precedents: `working_space` does `debug_assert!`, `color.rs` returns `Err` and has
    no assert, and `debug_assert!` is compiled out in release. Four sites said
    `stages::golden` pins the legacy *branch*'s pixels; it pins the
    **pre-colour-transform** pixels. `cli.rs` said the removed simple controls stay
    rejections "**never** warned aliases" while §7.1/§9 specify warned aliases under
    preset migration — resolved in the spec's direction (rejections *in this build*,
    aliases activate with the display presets), with §7.1's stale "current legacy
    controls … currently run before the output transform" corrected to "removed /
    rejected". §8 no longer says target migration "later adds `print.linear_range`" (it
    shipped). `CLAUDE.md`'s `stages::render` bullet said "stages 3–4"; `stages.rs` said
    "2–5a/5b" and has no 5b arm — both corrected. Two **new** broken intra-doc links
    (`unresolved link to 'golden'`, from `#[cfg(test)] mod golden`) are gone; the four
    remaining unresolved links and three redundant-target warnings match the pre-feature
    baseline exactly. `color.rs::icc_profile` lost its stale `#[allow(dead_code)]`
    ("used by the tests here" — `stages` calls it in production).
  - **Deferred, recorded for `output/{sdr,hdr}-display-rendering`** (not implemented
    here): WB gains / `black_point` / `density.scale` are `positive()`/`finite()`-
    validated with **no upper bound**, while `sigmoid.contrast`/`toe`/`shoulder` and
    `linear_range`'s span *are* bounded — worth closing that asymmetry when a display
    renderer consumes them (the `exposure_gain` half is closed above). If
    `ResolvedPrintControls` ever gains `Serialize`, note `serde_json` renders
    `f32::INFINITY` as `null`, so an `inf` gain would appear as `null` in the report —
    the checked constructor now prevents producing one. `density::estimate_wb_gains`
    hard-errors on channel levels ≤ 0, but post-matrix ACEScg legitimately contains
    negatives and `wb_channel_samples` filters only non-finite: that is a real domain
    change for the first display renderer that calls auto WB after the ACEScg boundary.
    Print controls under legacy `simple` are still accepted-and-silently-dropped (now
    at least *reported* as `print_controls: false`); pre-existing, needs a
    reject-or-warn decision. The planned preset names return exit 2 like a typo, while
    §11 reserves exit 4 for "unsupported variant"; if that changes,
    `scene_master_is_rejected_as_an_unreleased_schema_break` asserts `code == 2` for
    `gain-map-hdr` and must move with it.
  - **`pipeline_version` handoff:** still deliberately absent from the master report.
    `core/conversion-versioning` is landing it in parallel and its own notes record
    that the master must eventually name a behavioural version; the carve-out stays,
    and the vacuous test that "pinned" it is replaced by the key-set assertion above.
  - **Gates green in the worktree** (`cargo fmt --all --check`, `clippy --all-targets
    -D warnings`, `cargo build`, `cargo test`): **384 unit + 96 integration, 0
    failed**. Still uncommitted; `TASKS.md` still `[~]`.

- 2026-07-27 (review round 2, still uncommitted): Codex's delta re-review came back
  clean and an empirical delta pass re-confirmed B2/B3/the checked constructor
  (including all five master rejections through the `roll` per-frame manifest path,
  which is separate code). Four items remained.
  - **⚠ CORRECTION to the round-1 entry above: `--output-sdr` next to a named preset is
    now rejected by flag *presence* (exit 2).** Round 1 unified *everything* on
    resolved-value semantics and recorded `--output-sdr` as "correctly a non-issue". That
    conclusion was wrong for this one flag — do not re-derive it from the entry above.
    Everything else in that entry stands: the three selectors (`hdr`,
    `output_profile`, `bigtiff`) are still value-checked through the destructured
    `non_default_legacy_selector`, and that part was re-confirmed as closing the
    future-knob hole.
    **Why this flag is genuinely different.** (a) *Its documented meaning is
    contradicted, not merely subsumed.* design-spec §9 defines `--output-sdr` as
    "**force** the default 16-bit integer output"; a named preset does not write 16-bit
    integer output, so honouring the preset silently discards an explicit container
    request. By contrast `--bigtiff auto` means "decide for me" and a recipe
    `"hdr": false` is the `serde` default asserting nothing — those two genuinely ask
    for nothing the preset does not already do, and stay accepted. (b) *It has no recipe
    spelling*, so the cost argument that killed a general presence rule does not reach
    it: the recipe carries only `hdr: bool`, whose `false` is indistinguishable from
    omission, so there is no recipe form left behaving differently and detecting the
    flag costs one field read — no `curve_dmax_present`-style raw-JSON probing. (c) The
    "reset use" round 1 set out to protect does not survive inspection: recipe
    `hdr: true` + preset + `--output-sdr` resolved `hdr=false` and wrote the float
    master, i.e. the user asked for 16-bit *twice* and silently got f32.
    Implemented as `cli::reject_output_sdr_with_named_preset`, called after `merge` and
    before `validate` (it needs the resolved preset *and* the raw flags, so it cannot
    live inside `validate`, which `roll` also calls with config only — and `roll` has no
    output flags, so it misses nothing). Hard error, not a warning: an explicit request
    for a container the preset cannot produce is what "fail loudly" covers.
    `--output-sdr` keeps its entire legacy job when no named preset is in play,
    including resetting a recipe `hdr: true`. Pinned by
    `output_sdr_is_rejected_by_presence_next_to_a_named_preset` (both preset
    provenances, the double-request case, and four accepted legacy shapes) and by the
    e2e block in `film_master_never_silently_ignores_a_requested_adjustment`.
    **What the display/preset tasks inherit:** value semantics for the three selectors,
    plus exactly one presence exception, for exactly one flag, for a stated reason. Do
    not generalize the exception; do not remove it.
  - **`roll` now warns on a per-frame `output.preset` override.** It was the only
    roll-fixed choice of three that warned about nothing: `film_base` and
    `reconstruction.curve.dmax` each emit a "breaks color consistency" roll warning,
    while a manifest frame overriding `output.preset` silently emitted one 1.4 MB u16
    frame among 2.7 MB f32 masters (verified: rc=0, `warnings: None`). Added
    `sets_output_preset` (a raw-JSON probe like `sets_curve_dmax`) and a third warning
    beside the other two — same shape, `--strict`-promotable, applied-not-rejected. It
    warns even for an override that *restates* the shared preset, because
    `FrameStatus::Ok` carries no `output_render` block (convert-only), leaving
    `frames[].overrides` as the only other trace. This is the coarsest of the three
    breaks: not a different Dmin or anchor but a different **image class**. Pinned by
    `roll_frame_override_of_output_preset_warns_and_is_strict_promotable` (asserts the
    warning text in the roll report *and* on stderr, that the two frames really are
    32-bit vs 16-bit, and that `--strict` exits non-zero) plus a unit test on the probe
    and a no-override control assertion in `roll_accepts_a_film_master_recipe`.
    design-spec §9's roll-invariant list gained it as item (5).
  - **`exposure_gain` guard tightened to `is_normal()`.** `!is_finite() || == 0.0`
    accepted **subnormals**: `2f32.powf(-140.0)` is `7.17e-43` — finite, non-zero, and
    subnormal (f32 subnormals reach `2^-149`) — so it passed while contradicting the
    message's own "roughly −126..127" bound. A subnormal gain underflows every
    `px · wb · gain` product toward zero, producing an all-black image that trips
    neither the clip counter nor the non-finite counter: the same silent-destruction
    class the sigmoid contrast/knee caps close. `is_normal()` rejects zero, subnormal,
    infinite and NaN in one predicate. Added as a ninth case to
    `resolved_controls_reject_the_values_the_arithmetic_cannot_survive`. Note this guard
    is not CLI-reachable yet (nothing calls the shared display stage); it is armed for
    `output/{sdr,hdr}-display-rendering`. Deliberately still unguarded, as inherent:
    `px · wb · gain` can overflow to `inf` for a *validated* finite gain and a large
    pixel — that cannot be bounded without knowing the pixels, and `io::encode`'s
    non-finite counter catches it.
  - **Telemetry skill doc synced to schema v3.**
    `.agents/skills/perf-telemetry/SKILL.md` still showed `"schema_version": 2` with no
    `preset`, and `CLAUDE.md` names that skill as the telemetry how-to — so it
    contradicted the design-spec §9 update from round 1. Sample record updated, plus
    prose for `conversion.preset` and for why `output_hdr` is derived from
    `OutputParams::depth()`. (`.claude/skills/` is a symlink into `.agents/skills/`, so
    one edit covers both.)
  - **Measured outcomes** (`hdri-64bit.tif`, `--film-base 0.9,0.55,0.42`):
    `--output-sdr` + flag preset → rc 2; `--output-sdr` + recipe preset → rc 2;
    `--bigtiff auto` + preset → rc 0, 2 783 902 B; recipe `hdr:false` + preset → rc 0,
    2 783 902 B; recipe `hdr:true` + preset → rc 2; `--output-sdr` with no preset → rc 0,
    1 392 370 B (16-bit, unchanged); roll per-frame preset override → rc 0 with the
    warning, `--strict` → rc 1.
  - **Explicitly out of scope, still open:** the planned preset names
    (`gain-map-hdr` etc.) return `NcError::Usage`/rc 2 where design-spec §11 arguably
    reserves exit 4 for "unsupported variant". Re-confirmed as unaddressed and
    deliberately left; `tests/pipeline.rs::scene_master_is_rejected_as_an_unreleased_schema_break`
    asserts `code == 2` for `gain-map-hdr` and must move if that ever changes.
  - **Gates green in the worktree** (`cargo fmt --all --check`, `clippy --all-targets
    -D warnings`, `cargo build`, `cargo test`): **386 unit + 97 integration, 0
    failed**. Still uncommitted; `TASKS.md` still `[~]`.

- 2026-07-27 (review round 3, pre-PR polish, still uncommitted): both ship reviewers
  returned "ship — no Critical, no High"; Codex clean. One Medium plus five doc/API
  polish items.
  - **Fixed an unfalsifiable `--strict` assertion (the Medium).** The new roll test used
    `hdri-64bit.tif`, which **carries an IR plane** — so every frame raises a per-frame
    "IR preserved but not used" warning, `strict_failure` is already true via
    `frames.iter().any(|f| !f.warnings.is_empty())` (`cli.rs`), and a *no-override* roll
    on that fixture exits 1 under `--strict` by itself. Measured both ways: no-override
    `--strict` → rc **1** on `hdri-64bit.tif` vs rc **0** on `hdr-48bit.tif`, both with
    zero roll-level warnings. So `assert_ne!(code, 0)` passed for the wrong reason and
    stayed green with `sets_output_preset` gutted to `|_| false`. Switched the test to
    the IR-free `hdr-48bit.tif` and **added the control run** that makes the promotion
    falsifiable: same recipe, same fixture, same `--strict`, no override ⇒ rc 0 and an
    empty/absent roll `warnings`. The warning-*emission* half was already sound (top-level
    `warnings` is `null` on a no-override run, so the `.expect()` fires if the warning
    disappears) and was left alone. **General lesson for this repo's roll tests: any
    `--strict` promotion assertion must use an IR-free fixture, or it proves nothing.**
  - **Two guard docs made true rather than trimmed.** The `is_normal()` comment claimed
    it closed the class where "every `px · wb · gain` product underflows", but the guard
    inspected `exposure_gain` alone. Verified the hole: `--white-balance
    1e-30,1e-30,1e-30 --print-exposure -100` passes both existing checks (gains
    positive-finite; `2^-100` = `7.9e-31` is normal) yet the product is `7.9e-61`, which
    in f32 is **exactly `0.0`** — every sample zeroed, no counter firing. Added a
    per-channel `(wb[c] * exposure_gain).is_normal()` check, so the claim now holds.
    Three cases added to the guard test (both-directions product collapse, one-channel
    collapse so the loop is exercised, and product overflow).
    Also corrected two overstatements I could measure: the message blamed **both** ends
    of the exposure range for silent destruction, but the overflow end yields `inf`/`NaN`
    and **does** trip the non-finite counter — only the subnormal end is silent; and
    "underflow to `0.0`" is really `3.59e-43` for `2^-140 · 0.5`, i.e. crushed to a
    quantizes-to-black value, not literally zero. Both now say what is true.
  - **The span comment's claim was wrong, not the behaviour.** A subnormal-but-positive
    span (`linear_range: [0.0, 1e-40]`) passes both `ResolvedPrintControls::new` and
    `cli::validate`, and `0.5 / 1e-40` is `inf` — so "cannot produce inf/NaN from the
    span itself" was false. Left the behaviour alone (deliberately: it is **loud** via
    the non-finite counter, unlike the gain underflow) and rewrote the comment to say
    exactly which failure modes are excluded and which are delegated to the counter.
    The constructor doc now carries a "what this deliberately does not prevent" note
    covering both this and `px · gain` overflow for a validated gain.
  - **`validate` no longer hides convert's second gate.** `validate` is `pub` and reads
    as *the* CLI-boundary validator, but for `convert` it is incomplete —
    `reject_output_sdr_with_named_preset` is private and was enforced at one call site,
    so a future orchestrator doing `merge` + `validate` would reinstate the round-2 bug.
    Added `pub fn validate_convert(cfg, args)` composing both; `run_convert` calls it, and
    `validate`'s doc now says it is *not* the whole convert gate and that `roll`
    legitimately calls it directly (no output flags to miss). **`output/presets`: call
    `validate_convert`.**
  - **A generic check no longer carries a film-master-specific claim.**
    `reject_output_sdr_with_named_preset` is generic over named presets and interpolates
    `preset.name()`, then asserted "film-master is an unclamped 32-bit float linear
    ACEScg TIFF" — which would describe `hdr-pq` as an ACEScg float TIFF, exactly what
    `OutputPreset::name()` exists to prevent. Reworded to "(film-master, **for example**,
    is …)", the same illustrative construction rule 1 uses.
  - **"Resolves the container" softened to match the code.** `--bigtiff auto` is
    accepted next to a named preset (deliberately), so the size-based classic-vs-BigTIFF
    **promotion decision stays delegated** to `resolve_bigtiff` — a master over ~4 GiB
    legitimately comes out BigTIFF. Both messages now say "container *format*, bit depth,
    and colour profile", and rule 1 carries a comment stating that only `--bigtiff
    on|off` (which would override the policy) is the atomicity violation.
  - **Stale illustrative `params_hash` marked as such** in both design-spec §9 and
    `.agents/skills/perf-telemetry/SKILL.md`. Adding `print.linear_range` and
    `output.preset` changed the sidecar bytes, so `92a827ffd2d0aebd` is stale — and the
    sample's `"dmax": 1.6195` does not correspond to any default-anchor invocation, so
    there is no run to re-derive it from. Rather than invent a number, both sites now say
    the hash is illustrative, covers the whole recipe, and must not be asserted. (Nothing
    does.)
  - **Recorded, not fixed — handed to `algo/density-safety-bounds`:** a reachable,
    fully silent all-black output on the *legacy* path. `render_print`'s
    `2f32.powf(print.print_exposure)` (`algo/density.rs:478`) and
    `px[c] * wb[c] * exposure_gain` (`:486`) are guarded only by `finite()`/`positive()`,
    so on `hdr-48bit.tif` `--print-exposure=-200` writes **100 % zero samples at rc 0
    with every `loss` counter at 0, no warning, and `--strict` also 0**;
    `--white-balance=1e-45,1,1` kills exactly one channel the same way (so a
    whole-image collapse test would miss it); `--print-exposure 300` is already loud
    (`clipped_low: 695772`). This is a *different site* from the stage-3 tone map that
    task's original context block describes, so I appended a second `Context` block to
    `docs/tasks/algo/density-safety-bounds.md` with the measurement table, the
    reproduction command, and two implementation notes — plus a dated pointer in
    `docs/progress/algo.md`. Not fixed here on purpose: a naive `is_normal()` on
    user-supplied gains would reject legitimate extreme-push recipes, and that task owns
    the real-scan false-positive validation.
  - **Left alone as instructed:** `DisplayBranch`'s unused-but-`#[allow]`ed status
    (`output/sdr-display-rendering` introduces the match site), `OutputPreset::parse`'s
    trim/lowercase leniency (`OutputSpace::parse` precedent), the ICC byte-equality test
    (same-process both sides; `roll_two_frame_output_is_byte_identical_on_rerun` already
    depends on stable profile serialization), and the `gain-map-hdr` exit-2-vs-exit-4
    question.
  - **Gates green in the worktree** (`cargo fmt --all --check`, `clippy --all-targets
    -D warnings`, `cargo build`, `cargo test`): **386 unit + 97 integration, 0
    failed**. Still uncommitted; `TASKS.md` still `[~]`.

- 2026-07-28: **Task closed — shipped.** Landed after three review rounds
  (Codex + five pr-review lenses in round 1, Codex + silent-failure in round 2, two
  ship lenses in round 3; final Codex pass clean, `ship-code` verdict "no Critical,
  no High"). Final gates: **386 unit + 97 integration, 0 failed**; `cargo doc` at the
  4 pre-existing unresolved links.
  - **What landed:** `pipeline/render_split.rs` — `film_master` (a pure unwrap of the
    ACEScg buffer) plus the shared print controls resolved once in the pinned order
    `WB → exposure → black point → linear_range`. Two knobs: `--output-preset` /
    `output.preset` and `--linear-range` / `print.linear_range`. `simple`'s
    `clip_low`/`clip_high` are removed and rejected with a migration diagnostic.
  - **For `output/{sdr,hdr}-display-rendering` (read this first):** the display half
    is built and unit-tested but has **no CLI consumer**, so a non-default
    `print.linear_range` is a loud usage error rather than a silently-ignored knob —
    flip that when you wire a renderer. Take `&SharedDisplaySource` / `&shared.source`;
    `branch_source` was deleted as a pure pass-through, and `DisplayBranch` is left
    for you to introduce a match site. `ResolvedPrintControls::new` is the sole
    (fallible, private-field) constructor and validates finite-positive WB gains, a
    normal `exposure_gain`, a finite `black_point`, and a finite positive span —
    including the per-channel `wb[c] * exposure_gain` product, since two
    individually-valid factors can collapse to `0.0`.
  - **Atomicity rule, do not "tidy":** resolved-value for `output.hdr` /
    `output_profile` / `bigtiff`, deliberately **flag-presence** for `--output-sdr`
    alone. Round 2 reversed a round-1 decision here — the ⚠ CORRECTION entry above is
    the authority, and CLAUDE.md now carries the rule.
  - **Deferred out, with reproductions recorded:** the frame-local auto `Dmin`
    wording overclaim; unbounded WB/black-point/density-scale inputs; the
    `Serialize`→`inf`-as-`null` hazard; `estimate_wb_gains` vs post-matrix ACEScg
    negatives; print controls silently dropped under legacy `simple`; exit-2-vs-4 for
    planned preset names. Separately, a **reachable, fully silent all-black output**
    on the legacy print path (`--print-exposure=-200` → 100% zero samples, all loss
    counters 0, no warning, `--strict` rc 0) is handed to
    `algo/density-safety-bounds` with its reproduction — it is a *different site*
    (stage-4 `render_print`) from that task's existing stage-3 context.
  - **Known carve-out:** `pipeline_version` is deliberately absent from the master
    report, its absence pinned by test; `core/conversion-versioning` owns adding it.


## optional-color-correction-profiles
**Status:** optional / deferred
**Updated:** 2026-07-23

- 2026-07-23: Reframed measured scanner/film/development/lens neutralization as
  an opt-in CCR-like profile after the defined working-space boundary. Profiles
  must state what they correct and whether a lens is included.
- 2026-07-23: Kept capture, fitting, curves/matrices, and Delta E validation out
  of the default pipeline. This task depends on `film-rgb-working-space` and
  `film-master-render-pipeline` so it owns insertion before the split and
  corrected-master semantics; it has no downstream dependency edges.
- 2026-07-23: Pinned selection to `--correction-profile PATH` /
  `correction.profile.file` (default `null`), with correction immediately after
  NC film RGB v1 and before the film-master/display split. The optional task owns
  runtime integration, fail-loud artifact validation, hash/scope provenance, and
  corrected-master reporting.


## scanner-profile-before-density-experiment

**Status:** not started
**Updated:** —

- Goal: Determine empirically whether applying the same conventional scanner ICC transform to both image pixels and Dmin before component-wise density conversion improves negative reconstruction.
- **This task's two dated entries are stranded in [`io.md`](io.md)**, at the tail
  of its `## input-data-semantics` section: they lost their heading in the flat
  log before the epic split, so it carried them there verbatim. Read them before
  starting.


## colorimetry-source-of-truth

**Status:** done
**Updated:** 2026-07-31

- 2026-07-30: Added as accepted technical debt after the gain-map implementation
  introduced more standards-based matrices and luma coefficients. The task will
  add a central colorimetry module, migrate existing transforms without changing
  pixels, and establish a deterministic, CI-checked workflow for future
  color-space updates. It depends on `output/gain-map-hdr-output` so the
  gain-map surface is stable before consolidation, and it blocks no output task.
- 2026-07-30: **Dependency update:** it still does not block the in-progress
  gain-map implementation, but `output/lossless-hdr-tiff` now deliberately
  depends on it so future BT.2020 TIFF profiles and encoder adapters reuse the
  audited definitions instead of deepening the same debt.
- 2026-07-31: **Coefficient inventory.** Six runtime artifacts to centralize:
  `working_space::NC_FILM_RGB_V1_TO_ACESCG` (f64), `sdr::ACESCG_TO_SRGB`,
  `sdr::ACESCG_TO_DISPLAY_P3`, `hdr::ACESCG_TO_BT2020`,
  `gain_map::BT2020_TO_DISPLAY_P3` (all f32 3×3), plus `hdr::BT2020_LUMA` and
  `gain_map::DISPLAY_P3_LUMA`. Separately, `color.rs` repeats the Display P3
  primaries + D65 white twice (lines ~184 and ~299) in the lcms2 profile
  builders. The `PIPELINE_FINGERPRINTS` drift gate covers `film_base::estimate`,
  `reconstruct_and_print` on the golden vectors, and the default recipe JSON —
  none of which this refactor touches, so the fingerprint must not move.
- 2026-07-31: **The two luma vectors are different *kinds* of number and must not
  be centralized alike.** `BT2020_LUMA = [0.2627, 0.6780, 0.0593]` is the
  normative tabulated vector from the standard; deriving it from the BT.2020
  primaries instead gives `[0.262700212, 0.677998072, 0.059301716]`, agreeing only
  to ~2e-6. `DISPLAY_P3_LUMA` is the opposite: a full-precision *derived* value
  that reproduces the P3 NPM Y row exactly. So BT.2020's belongs in the
  standard-definitions category and P3's in the derived-artifacts category, and
  the check mode must apply the matching rule to each.
- 2026-07-31: **The tree contains two different Bradford conventions** — the
  first thing the re-derivation surfaced. `NC_FILM_RGB_V1_TO_ACESCG` reproduces
  to **1.1e-16** using Lindbloom's *published 7-decimal* inverse cone matrix and
  is off by **9.1e-8** using the exact inverse; the four display matrices are the
  reverse, matching the **exact-inverse** derivation. A single centralized
  `bradford()` therefore cannot reproduce all of them, so both conventions must be
  named explicitly, with the Lindbloom-published one documented as retained
  *solely* so the frozen v1 mapping re-derives exactly.
- 2026-07-31: **Phase 0 (can the pinned f32 literals be reproduced exactly?) —
  answered: no, and the check mode must be tolerance-based.** Swept the
  derivation space in binary64 (adjugate vs Gauss-Jordan inverse, left vs right
  composition association, four 3-term summation orders, Bradford vs CAT02):
  **33 of 36 entries reproduce bit-exactly and every variant gives the same 33**.
  The three residuals are `ACESCG_TO_SRGB[2][1]`, `ACESCG_TO_DISPLAY_P3[2][0]`,
  and `BT2020_TO_DISPLAY_P3[0][2]`, each exactly **−1 f32 ulp**.
  - Accumulation order is *not* the cause. The sweep moves the f64 result by only
    ~5 f64 ulp (~1e-17), while reaching the f32 rounding boundary needs 3.7e-10
    absolute / ~3e-9 relative — seven orders of magnitude more. The residual is a
    source-data difference, most consistent with the original values having been
    composed from intermediate matrices rounded to ~9–10 significant digits. No
    derivation script or note was committed with #61/#62/#63, so the exact
    historical route is unrecoverable from the repo.
  - **This is not a coefficient correction and does not warrant a behavioral
    task.** The published chromaticities are specified to three decimals, so their
    own rounding (±5e-4) moves matrix entries by up to **4.2e-4 — 3,544× one f32
    ulp**. A 1-ulp disagreement sits three orders of magnitude below the precision
    the standards themselves define; neither value is "more correct", and re-pinning
    the three entries would *itself* be the unreviewed pixel change this task
    forbids. Record it as a measured, bounded deviation instead.
  - **Decision:** pinned literals move verbatim, and check mode asserts agreement
    with the canonical f64 derivation within **±1 f32 ulp**, with the three
    boundary entries named individually and this measurement cited as the
    justification the task's How-to-Verify asks for.
- 2026-07-31: **Shipped.** `src/pipeline/colorimetry/` is now the single source
  of truth, split by the four categories the task asked for:
  `definitions.rs` (standard source data with provenance),
  `pinned.rs` (the reviewed literals the runtime multiplies by),
  `derive.rs` (binary64 derivation), `audit.rs` (the check/regen harness), and
  `tests.rs` (tolerances + independent anchors). `derive`/`audit`/`tests` are
  **`#[cfg(test)]`**, which is what structurally guarantees "the runtime never
  derives" — stronger than a comment, and it avoids a `dead_code` allow on the
  math.
- 2026-07-31: **Migrated consumers, verbatim.** `working_space`, `sdr`, `hdr`,
  `gain_map` now import from `colorimetry::pinned`; no stage keeps a private
  copy. `color.rs` gained `lcms_inputs(ColorSpace)` so all five lcms2 profile
  builders reference a named definition — that removed the file's duplicated
  Display P3 primaries + D65 white (two copies, nothing keeping them in step).
  `PROPHOTO` was added to the definitions for the `--output-profile prophoto`
  builder; it has no pinned matrix because Little CMS does that colorimetry.
  PQ/HLG constants moved into `definitions::transfer` — safe because every PQ
  constant is a ratio of small integers and so is exactly representable at both
  widths.
- 2026-07-31: **Bit-identity verified two ways, same machine.** A temporary
  `#[cfg(test)]` probe dumped raw `to_bits()` for 21 stage renditions
  (working-space mapping; SDR sRGB/P3 × three highlight-compress values; HDR
  linear × three, plus PQ and HLG; gain-map SDR/HDR-P3/ratio × three) before and
  after — byte-identical. End-to-end, five outputs were checksummed before and
  after: legacy TIFF, film-master, `ultra-hdr-v1` on both the 48-bit and 64-bit
  fixtures, and an explicit `--output-profile display-p3` TIFF — all identical.
  The probe was deleted afterwards rather than committed: those paths run
  `powf`, so a checked-in bit-exact gate would be red on the other CI target
  (CLAUDE.md's cross-platform caveat). `PIPELINE_FINGERPRINTS` is untouched, as
  expected — the gate covers `film_base::estimate`, `reconstruct_and_print`, and
  the default recipe JSON, none of which this refactor reaches.
- 2026-07-31: **The maintenance command is a test-harness with a regen mode**
  (`NC_COLORIMETRY_REGEN=1 cargo test colorimetry::audit`), so CI exercises
  check mode through the existing `cargo test` gate — no new binary (the crate
  stays single-bin), no new CI step, no Python. **Regeneration rewrites only
  `derived-artifacts.txt`, never `pinned.rs`.** That asymmetry is deliberate: a
  generator that edits runtime coefficients could silently change pixels,
  whereas this one can only produce a reviewable diff. Because the artifact
  records shipped values too, editing a literal without regenerating also fails
  the check, so staleness is caught in both directions. The derivation is pure
  IEEE-754 `+ - * /` with no transcendentals, so the artifact is cross-platform
  stable and safe as a CI gate.
- 2026-07-31: **Fixed a test that could not fail.**
  `gain_map::bt2020_to_p3_matrix_matches_independent_primary_vectors` claimed
  "independently calculated" reference vectors that were in fact the matrix's own
  columns to the same digits — it could only ever restate the literal. It now
  re-derives from the named definitions, and genuine independence lives in
  `colorimetry::tests`: an externally published matrix, plus chromaticities
  recovered from the transformed primaries and checked against the standards'
  published values. `hdr`'s colored-vector test was left alone — its expected
  values are a real independent derivation, not the matrix columns.
- 2026-07-31: Wrote `docs/colorimetry-maintenance.md` (the 7-step workflow, the
  representation-only vs pixel-change decision, and the two coexisting Bradford
  conventions). All four gates green.
- 2026-07-31: **Review round: `ulps_f32` overflowed on straddle-zero
  comparisons.** It subtracted raw IEEE-754 bit patterns, which are
  sign-magnitude and therefore not ordered across the sign. Any pair spanning
  zero exceeded `i32` — `f32::MIN_POSITIVE` against its negation needs
  2_147_483_648, one past `i32::MAX` — so it panicked in a debug build (how
  `cargo test` runs) and wrapped to nonsense in release. Reachable, not
  theoretical: `BT2020_TO_DISPLAY_P3[2][0]`, `ACESCG_TO_DISPLAY_P3[2][0]`, and
  `ACESCG_TO_BT2020[1][0]` all sit near zero, so a standards revision flipping
  one across zero would make the audit *panic* instead of reporting the
  difference it exists to report. Fixed with a monotonic ordered key returning
  `i64` (`-0.0` and `+0.0` key alike; `MAX_ULPS` widened to match).
  **Side effect worth recording:** raw-bit subtraction was also sign-*inverted*
  for two negative values, so the three known deviations were being reported as
  −1 when the derivation is genuinely one ulp *above* the shipped literal. They
  now read **+1**. Magnitudes, every runtime literal in `pinned.rs`, and all
  source definitions are unchanged; the regenerated `derived-artifacts.txt` diff
  is exactly those three signs. Output stayed bit-identical (legacy TIFF and
  ultra-hdr-v1 checksums both unmoved).
- 2026-07-31: **Review round 2: an "independent" oracle that was not, and two
  transfer constants still outside the source of truth.**
  `colorimetry::tests::transformed_primaries_recover_the_standards_chromaticities`
  compared the recovered chromaticities against `definitions::BT2020.primaries`
  — the same const its own derivation chain runs on — while its comment claimed
  the expected numbers were "read straight off the standards". It did catch a
  typo in `definitions.rs` left *un*-accompanied by a re-pin, but the documented
  maintenance flow is "edit the definition, then re-pin the matrix", and in that
  flow the definition and the pinned matrix move together, so a shared typo
  validated itself. The expected values are now BT.2020-2 chromaticities
  re-typed as literals in the test, with a comment saying plainly that pointing
  them back at the const would destroy the independence the task requires.
  **Demonstrated rather than asserted:** perturbing the BT.2020 red primary to
  `x = 0.718`, re-pinning `BT2020_TO_DISPLAY_P3` to the new derivation, and
  regenerating `derived-artifacts.txt` — so definition, pinned literal, and
  audit all agreed with each other — leaves the new test failing
  (`recovered (0.718000, 0.292000), standard says (0.708, 0.292)`), while the
  previous `BT2020.primaries` form *passes* the identical perturbation. All
  three files were restored afterwards and re-checksummed.
  Separately, `hdr.rs`'s HLG OETF constant `a` and `color.rs`'s sRGB type-4
  parametric TRC parameters were the last standards-based transfer constants
  living in a stage. They moved to `definitions::transfer::hlg::OETF_A` and
  `definitions::transfer::srgb::{G,A,B,C,D}` as `f64` with provenance. The HLG
  narrowing is safe by bit pattern (`0.178_832_77` as `f32` and
  `0.178_832_77_f64 as f32` are both `3e371ff0`), and the sRGB parameters are
  kept as the standard's quotients (`1.0 / 1.055`, `0.055 / 1.055`,
  `1.0 / 12.92`) rather than pre-evaluated decimals, so the `f64` values handed
  to Little CMS are unchanged. `b` and `c` stay derived from `a` inside
  `hlg_oetf` — that is the standard's own formulation. The test-only
  `srgb_encode` helper in `color.rs` was deliberately **not** pointed at the new
  definitions and now carries a comment saying so: it is the independent oracle
  for the very curve those parameters build, and it states the standard's encode
  direction, which is not the form type 4 stores. `derived-artifacts.txt` is
  unchanged (transfer constants are not in the audit catalog), all four gates
  are green (500 + 126 tests), `cargo test hlg` passes its 4 tests, and the
  legacy TIFF and `ultra-hdr-v1` checksums are both unmoved.
- 2026-07-31: **Round 3 review — Codex's remaining P2 partially rejected, with
  evidence.** It reported ACEScg→Display P3 as "vulnerable to self-validating
  definition errors" because this module has no direct independent vector for it.
  The gap in *knowledge* was real; the vulnerability was not. Tested by tampering
  `DISPLAY_P3` and driving the full self-validating scenario — regenerate the
  audit artifact *and* re-pin every affected matrix so definition, derivation and
  pin all agree:
  - A **realistic** single-digit typo (`0.680 → 0.690`) is caught, by
    `color::display_p3_colorants_match_icc_registry_reference` and
    `display_p3_decodes_to_registered_d65_encoding` — genuinely external
    ICC-registry anchors that live in `pipeline::color`, not in
    `colorimetry::tests`, which is why the review missed them.
  - A **sub-rounding** perturbation (`0.680 → 0.6805`, finer than the three
    decimals the standard specifies) slips through everything except two
    behaviour goldens. That is not a defect: the standard does not define the
    value to that precision.
  - The same experiment exposed something the round-2 fix does *not* cover:
    `transformed_primaries_recover_the_standards_chromaticities` anchors the
    **source** space only. The destination NPM appears in both the pinned matrix
    and the recovery step and cancels
    (`NPM_dst · NPM_dst⁻¹ · NPM_src == NPM_src`), so a mistyped *destination*
    primary is invisible to it.
  Fix applied was documentation, not machinery: `colorimetry/tests.rs` now
  carries a per-space table of where each external anchor lives, states the
  source/destination asymmetry, and warns that deleting `color`'s two ICC-registry
  tests would remove Display P3's only real anchor. Adding a redundant vector
  would have implied coverage that the cancellation makes impossible to get from
  a recovery test.
- 2026-07-31: **Ship-time review round — two more real findings, both from
  Codex, both fixed.**
  - **`sdr.rs` still hard-coded two luma vectors** at `destination_rgb`, and one
    of them (`[0.228_974_57, 0.691_738_55, 0.079_286_91]`) was a byte-for-byte
    duplicate of `pinned::DISPLAY_P3_LUMA` — precisely the duplication this task
    exists to remove, in a module the task names as migrated. Two inventory
    greps missed it because they searched for named `const` declarations and
    these were inline array literals inside a `match`; the same grep-shaped blind
    spot hid the HLG OETF coefficient earlier. **Lesson: inventory a source-of-
    truth migration by reading the consuming functions, not by grepping for
    `const`.**
  - The sRGB vector `[0.212_639, 0.715_169, 0.072_192]` turned out to be a
    **third provenance kind**: not the normative BT.709 table (`0.2126, 0.7152,
    0.0722`) and not the exact derivation, but the derivation **rounded to six
    decimals** — 0 / −6 / **43** ulps out. It is now `pinned::SRGB_LUMA`, moved
    verbatim, with its own `SRGB_LUMA_MAX_ULPS = 43` allowance rather than
    relaxing the shared ±1 bound, and a test that pins the 6-dp relationship
    itself so the gap cannot silently drift. Re-pinning to the exact derivation
    would be a pixel change: the sRGB SDR branch multiplies by it.
  - **The maintenance doc's "representation-only" rule was unsound.** It let a
    reader conclude "ulps all 0 ⇒ no pixel change", but `pipeline::color` feeds
    `definitions::{REC709, DISPLAY_P3, ACESCG, PROPHOTO}` straight into Little
    CMS, so changing one of those four moves ICC bytes and lcms2-transformed
    pixels with `pinned.rs` untouched and every ulp at 0. Nothing automated
    catches it — the drift gate stops before lcms2 and the audit only compares
    pinned artifacts. Step 4 now carries an explicit warning that those four
    spaces are always a pixel change regardless of the ulp column, and step 5
    requires the before/after comparison for them too.
  - Pixels re-verified after both fixes: legacy TIFF and `ultra-hdr-v1` on both
    fixtures still byte-identical to a pristine `b8ce1d7` build.

- 2026-08-05 (added by `output/hdr-avif-output`, per step 7 of
  `docs/colorimetry-maintenance.md`): Added one new artifact,
  `pinned::BT2020_NCL_RGB_TO_YCBCR` — the BT.2020 non-constant-luminance
  R'G'B' → Y'CbCr matrix that AVIF signals as `matrix_coefficients = 9`. New
  `derive::ycbcr_from_luma` closes the standard's `Kr`/`Kb` formulas
  (BT.2020-2 § 3.4, BT.2100-2 Table 6) into a matrix; new
  `Source::YCbCrFromLuma` registers it in the audit catalog.
  **No existing artifact moved and every new entry audits at `ulps = 0`**, so
  this is an addition, not a pixel change: nothing shipped consumes it yet
  (`io::avif` is not CLI-reachable), no `pipeline_version` decision is owed, and
  none of the four Little-CMS-consumed spaces was touched.
- 2026-08-05: Two things about that artifact worth knowing before editing it.
  (1) It is the **first nonlinear-domain artifact** in the module — it multiplies
  transfer-encoded PQ/HLG code values, not linear light, which is exactly what
  "non-constant luminance" means. The file's other matrices are all linear-light
  transforms, so the usual "this is a colour transform" intuition does not carry
  over. (2) It is derived from the **tabulated** `BT2020_LUMA`, not from the
  BT.2020 primaries, and that is load-bearing rather than incidental: decoders
  invert the rounded tabulated form, so deriving from primaries would put nc's
  forward transform ~2e-6 away from every decoder's inverse. A test asserts row 0
  *is* the same pinned literal as `BT2020_LUMA`, so the two cannot desynchronize.
- 2026-08-05: Verification anchors follow the module's "oracle must not share a
  source" rule: the published four-decimal coefficients (as carried by ffmpeg /
  libavif / dav1d colour tables) at ±5e-5; exact `0.5` at full blue's Cb and full
  red's Cr, which is *why* the chroma rows carry the `2(1-Kb)` / `2(1-Kr)`
  scaling; a round trip through the matrix's own inverse; and an achromatic sweep
  over the **exact 10-bit code ladder**. That last bound is a measured maximum,
  not a round number — worst chroma residual 2^-25 at code 546, worst luma
  residual 2^-23. A first attempt swept 33 evenly-spaced values instead and
  understated the peak by 8x, because the residual is a rounding artifact that
  peaks near 0.5 rather than growing with the input.
