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
- **⚠ The mapper is defined and tested but NOT yet wired into the render path.**
  The legacy no-preset path still runs `reconstruct → finish_print →
  color::to_output`, and its pixels are unchanged. `color/film-master-render-pipeline`
  and `output/presets` are what activate it. The report already stamps
  `working_mapping = "nc-film-rgb-v1"` — provenance only, deliberately not a knob.
- **`to_output` does not clamp** and may hand the encoder out-of-`[0,1]` or
  non-finite values; clamping and loss counting belong to `io/encode`.
- **`film-master` is the planned unclamped-ACEScg branch** that bypasses print and
  display controls. It rejects frame-local auto Dmax by design — see
  `film-base`'s summary for why the anchor must be roll-fixed.
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
**Status:** not started
**Updated:** 2026-07-23

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
