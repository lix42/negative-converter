# Negative Converter — output Progress Log

Execution log for the `output` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `output`:

- **The HDR spike is closed and its numbers are binding.** ISO 22028-5:2026 and
  ISO 21496-1:2025; **203 cd/m² reference white**, **1000 cd/m² target peak**,
  4.926108 linear and 2.300448 log2 capacity of that display ratio (not
  per-pixel gain extrema — those come from the offset-adjusted formula). The
  renderers **may not change reference white, target peak, the common gain-map
  domain, or the RGB-map decision** without reopening `docs/hdr-output-spike.md`.
- **Containers:** JPEG + ISO 21496-1 gain map is the default HDR still; 10-bit
  4:4:4 BT.2020 AVIF is the explicit PQ/HLG path. HEIC is deferred (no portable
  encoder API for the final gain-map container, plus HEVC licensing risk).
  Before final-ISO conformance is available, the explicit `ultra-hdr-v1` JPEG
  path uses only the public Android/Adobe XMP + MPF/GContainer dialect and must
  not be labeled ISO-conformant.
- **Native dependency packaging:** the shipped Ultra HDR implementation keeps
  the audited libultrahdr/libjpeg-turbo snapshot in-tree for now. A deferred,
  non-blocking maintenance task may replace it only with an exact published
  Cargo release that preserves the required marker ordering, static linkage,
  and network-free native build.
- **The spike waived the licensed-normative-text review at spike level and
  re-homed it** as a pre-merge conformance gate on the encoder tasks. Don't treat
  it as already satisfied.
- **Ownership split — read this before touching a transform.**
  `output/display-p3-output` owns only the *destination encoding*: a synthesized
  Display P3 ICC (Little CMS writes D50 media white, the chromatic-adaptation tag,
  and Bradford-adapted D65 colorants automatically — verified against the ICC
  registry) plus the parametric sRGB TRC. `output/sdr-display-rendering` owns
  ACEScg → rendered-linear destination RGB: reference white, tone, chromatic
  adaptation, and gamut mapping. Renderers return **rendered-linear** pixels;
  transfer encoding happens afterward. Gain-map construction consumes the
  *pre-transfer* rendition so ratios are taken in a common linear domain.
- **The HDR renderer is implemented.** `pipeline::hdr::render_linear` returns
  finite, non-negative, reference-white-relative BT.2020 pixels. Gain-map work
  must transform that seam to common linear Display P3 before ratios;
  `encode_transfer` instead consumes it in place and returns opaque Rec.2100 PQ
  or HLG pixels plus the full-range CICP 9/16/9 or 9/18/9 contract.
  Reference white is 203 cd/m², peak is 1000 cd/m², and HLG pins the 1000-nit,
  zero-black reference OOTF with system gamma 1.2. Presets and containers remain
  downstream.
- **⚠ Display P3 is not yet a product path.** The SDR renderer now produces
  rendered-linear P3/sRGB and `encode_rendered_sdr` applies only the matching
  transfer/profile, but `output/presets` still owns CLI activation. Legacy
  `to_output` continues to source linear Rec.709, so selecting the profile knob
  directly still performs Rec.709→P3 plus the sRGB TRC.
- **Gain-map math is pinned:** per-channel `(HDR + offset_hdr) / (SDR +
  offset_sdr)` in common linear Display P3 normalized by 203 cd/m². Extrema come
  from actual per-pixel values over independently tone-mapped renditions. No
  arbitrary epsilon, no silent clamp, no `0/0` — those are fail-loud cases.
- **`output/presets` is the migration surface**, and it is atomic: presets reject
  legacy output-selection flags, the output suffix must match the resolved
  container and is never rewritten, and `film-master` rejects every non-default
  downstream control after merge. It depends on `core/conversion-versioning`
  because activating the new default owns a golden-tested `pipeline_version`
  boundary.
- **ICC bytes are platform-dependent**, so profile-inclusive byte hashes are not a
  valid cross-platform gate — profile determinism here is pinned per build via the
  dateTime-zeroing path.


## display-p3-output
**Status:** done
**Updated:** 2026-07-24

- 2026-07-24: Reviewed via the two-engine review-fix-loop (Codex + pr-review
  lenses: quality, tests, comments, silent-failure). Six findings, all
  doc/comment/test (no correctness change): reworded the "already-rendered
  linear-P3 / only transfer-encodes" framing to describe the shipped Rec.709→P3
  remap + sRGB TRC (marking linear-P3-in as the `sdr-display-rendering` target);
  added a production-path pixel assertion (saturated Rec.709 red golden) and a
  deep-shadow sRGB-toe sample; fixed the `srgb_trc` "shared by sRGB" comment; added
  `display-p3` to `--output-profile` help; relocated the ICC-registry note. Loop
  converged; gates green. Rebased onto origin/main (past #48 HDR + #49 telemetry);
  the merged design-spec is coherent (HDR spike confirms Display P3 as the
  gain-map SDR base and the display-p3-output ↔ sdr-display-rendering split).
  Shipped via /ship.

- 2026-07-21: Planned a deterministic synthesized Display P3 profile (D65/P3
  primaries with the piecewise sRGB TRC), avoiding dependence on or redistribution
  of the macOS system profile. This is the SDR rendition and gain-map base.
- 2026-07-21: Removed the false dependency on scanner/film characterization.
  Profile synthesis and ACEScg→P3 transforms can be verified with synthetic
  ACEScg samples; final product integration remains gated downstream.
- 2026-07-21: Narrowed ownership after review: this task supplies the standard
  Display P3 destination transform and ICC metadata. Reference white, SDR tone,
  and gamut rendering belong to `sdr-display-rendering`.
- 2026-07-21: Tightened ownership to encoding/profile only: SDR rendering owns
  ACEScg → rendered linear P3. The ICC v4 profile uses D50 PCS/media white,
  Bradford-adapted D65 P3 colorants and the adaptation tag; D65 remains the
  destination encoding white, not the ICC media white.

### Implementation (2026-07-23, uncommitted)

- **Approach.** Added `OutputSpace::DisplayP3` as a new `--output-profile` /
  `output.output_profile` keyword (`display-p3` / `displayp3`), reusing the
  existing string knob — no new CLI field, recipe field, or merge arm (the knob
  already merges at `cli::merge`; verified `display_p3_end_to_end_embeds_p3_icc`
  drives it through `to_output`). The profile is synthesized with Little CMS from
  the registered P3 encoding (D65 white 0.3127/0.3290; R 0.680/0.320,
  G 0.265/0.690, B 0.150/0.060) plus a **parametric** sRGB TRC.
- **Empirically verified (not assumed) lcms2 6.1.1 behavior** via a throwaway
  `#[ignore]` probe before writing code: `Profile::new_rgb(D65, P3, srgb_curve)`
  produces an **ICC v4.4** RGB **Display**-class profile that automatically writes
  **D50** media white (0.9642/1.0/0.8249), the **`chromaticAdaptationTag`**, and
  **Bradford D65→D50-adapted colorants** matching the ICC-registry / macOS
  `Display P3.icc` reference (rXYZ 0.51512/0.24119/-0.00105, etc.). So no manual
  chad/colorant/white handling is needed — lcms does it. No dependency on or
  redistribution of the macOS system profile.
- **TRC.** New `srgb_trc()` helper builds the Little CMS **parametric type-4**
  (IEC 61966-2.1) curve `[2.4, 1/1.055, 0.055/1.055, 1/12.92, 0.04045]`, not a
  gamma-2.2 power approximation. `synth` refactored to delegate to a shared
  `synth_curve(white, primaries, &curve)`; sRGB/ProPhoto/ACEScg paths unchanged.
- **Encoder semantics.** The "linear P3 → encoded P3" encoder is realized as the
  lcms working→output transform against the P3 profile. Proven by
  `linear_p3_samples_encode_with_srgb_trc_and_identity_primaries`: a *linear-P3
  source* profile → the P3 output profile applies only the sRGB TRC (linear 0.5 →
  0.735357) and keeps a pure P3 red on the red axis (G,B ≈ 0), i.e. **no gamut
  mapping and no ACEScg transform** here — those stay with `sdr-display-rendering`.
- **Determinism.** Reuses the existing `profile_icc` dateTime-zeroing path;
  `display_p3_icc_is_deterministic_with_zeroed_datetime` asserts byte-identical
  reruns. Range clamping still happens only at the u16 encode step (unchanged).
- **Tests added** (`src/pipeline/color.rs`): `display_p3_profile_is_rgb_display_class_with_d50_pcs`,
  `display_p3_colorants_match_icc_registry_reference`,
  `display_p3_trc_is_parametric_srgb_not_gamma`,
  `display_p3_decodes_to_registered_d65_encoding` (transforms encoded P3 → D50
  XYZ via lcms, un-adapts D50→D65 with the standard Bradford matrix, recovers the
  registered D65 primaries/white),
  `linear_p3_samples_encode_with_srgb_trc_and_identity_primaries`,
  `display_p3_icc_is_deterministic_with_zeroed_datetime`,
  `display_p3_end_to_end_embeds_p3_icc`; plus `display-p3` cases in
  `parse_keywords_and_path` and the builtins-validity loop.
- **Docs.** design-spec §5 output-color bullet and §9 `--output-profile` entry now
  list `display-p3`. (`docs/design-spec.html` does not exist in this worktree, so
  nothing to mirror.)
- **Notes / deferred to `sdr-display-rendering`.** This task does not activate a
  product path: the current `to_output` working space is still linear Rec.709, so
  selecting `display-p3` today would colorimetrically remap Rec.709→P3 (a valid
  conversion, but not the intended SDR render). `sdr-display-rendering` owns the
  ACEScg→rendered-linear-P3 transform, reference white, SDR tone, and gamut policy;
  once it produces linear-P3 working values, the same `to_output` transform becomes
  the pure-TRC P3 encoder these tests exercise. The full `display-p3` *preset*
  (container/tone/gamut, per `output-presets`) is also still future.
- **Not done here.** No real-scan visual check in macOS Preview/Photos or on an
  iPhone (task "How to Verify" item); that needs the activated SDR render and is
  deferred with the product-activation gate.

### Review-fix pass (2026-07-23, uncommitted)

Six verified doc/comment/test findings (no correctness change), all fixed:
- Reworded the "already-rendered linear-P3 / only transfer-encodes" framing in
  three places (`OutputSpace::DisplayP3` doc, design-spec §5, design-spec §9) to
  describe the *shipped* behavior — sources the linear Rec.709 working profile, so
  Little CMS does a lossless Rec.709→P3 remap (Rec.709 ⊂ P3, no gamut compression)
  **plus** the sRGB TRC — and marked the pure linear-P3-in transfer-encode as the
  future `sdr-display-rendering` state. Also fixed the isolation-test comment to
  say "here" = the encode step tested in isolation with a synthetic linear-P3
  source, which does NOT exercise the shipped `to_output` path.
- New test `to_output_display_p3_remaps_rec709_and_encodes`: drives the real
  `to_output` path and asserts a saturated Rec.709 red against the expected
  P3-encoded value derived from the standard Rec.709→P3 matrix + sRGB encode
  (≈ 0.9175/0.2004/0.1385), plus teeth that G/B lift off zero (contrasting the
  identity-primaries isolation test). Neutral-only was necessary-not-sufficient.
- Added a deep-shadow toe sample (lin 0.002 → 0.02584 = 12.92×0.002) to
  `linear_p3_samples_encode_with_srgb_trc_and_identity_primaries` — exercises the
  sRGB linear segment that distinguishes the parametric curve from a gamma power
  (~0.081 there).
- Fixed the `srgb_trc` comment (it is Display P3's only caller; sRGB output uses
  `Profile::new_srgb()`).
- Added `display-p3` to the `--output-profile` CLI help string (`cli.rs`).
- Moved the "verified against the ICC registry" note off the generic `synth_curve`
  doc onto the DisplayP3 build arm.

Gate after fixes: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo build`, `cargo test` all green (307 unit + 86 integration).


## hdr-output-spike
**Status:** done
**Updated:** 2026-07-24

- 2026-07-21: Added a decision gate for ISO HDR versus ISO 21496-1 gain-map HDR,
  HEIC/JPEG containers, encoder/licensing constraints, metadata, reference white,
  headroom, and cross-platform fallback before committing production code.
- 2026-07-23: Started the spike. The investigation will pin exact standards and
  container profiles, compare cross-platform encoder APIs and licensing, inspect
  metadata round trips with small reference files, and record numeric rendering
  policy plus a versioned platform/fallback matrix for downstream tasks.
- 2026-07-23: Wrote
  [`docs/hdr-output-spike.md`](../hdr-output-spike.md). The
  provisional implementation choice is JPEG for the default ISO 21496-1 gain-map
  output and 10-bit 4:4:4 AVIF for explicit PQ/HLG. HEIC is deferred because the
  portable encoder lacks the final gain-map container API and HEVC/x265 adds
  licensing and packaging risk.
- 2026-07-23: Pinned current standards and rendering inputs: ISO 22028-5:2026
  (which replaced the withdrawn 2023 technical specification), ISO 21496-1:2025,
  ISO/IEC 23008-12:2025/Amd 1:2025 for a future HEIF path, BT.2100-3,
  203 cd/m² reference white, 1000 cd/m² initial target peak, 4.926108 linear
  content headroom, and 2.300448 log2 capacity.
- 2026-07-23: Prototype PQ/HLG AVIF files carried the intended 10-bit BT.2020
  CICP values and decoded in macOS ImageIO with 4.92611 PQ headroom. Fixed
  single-thread encodes were byte-identical on the same build.
- 2026-07-23: Prototype `libultrahdr` 1.4.0 JPEG metadata decoded in
  libultrahdr/ExifTool/ImageMagick, but macOS ImageIO rejected the file. Its
  marker order was ISO APP2, MPF APP2, then JFIF APP0; upstream PR 394 fixes that
  ordering but is not released. Moving APP0 first and correcting the MPF offset
  locally still failed ImageIO, so final ISO serialization, repaired Apple
  decode, physical Android/iPhone/browser viewing, and legal review remain
  downstream pre-shipping gates; only licensed normative-text review remains a
  prerequisite for completing the spike itself.
- 2026-07-24: **Closed.** Decided to proceed *without* the licensed
  ISO 22028-5:2026 / ISO 21496-1:2025 text — completion gate 1 is waived at the
  spike level and re-homed to the encoder tasks as a pre-merge conformance gate
  (`gain-map-hdr-output` owns JPEG serialization/dual-dialect reconstruction,
  `hdr-avif-output` owns AVIF brands/limits/codec bounds, `display-output-acceptance`
  owns device evidence). Gates 2 and 3 satisfied: the container/profile/encoder,
  203-nit reference-white / 1000-nit peak, gain-map formula, and rendering
  contract (spike note §"Rendering contract") are final as written and give
  `sdr-display-rendering` / `hdr-display-rendering` everything they need. Those
  renderers may not change reference white, target peak, the common gain-map
  domain, or the RGB-map decision without reopening the note. Spike note status
  line + completion-gates section updated to match.


## hdr-display-rendering
**Status:** done
**Updated:** 2026-07-29

- 2026-07-23: The HDR spike pinned 203 cd/m² reference white, 1000 cd/m² target
  peak, PQ as the primary path, explicit HLG assumptions, hue-preserving gamut
  compression, and a 10-bit full-range BT.2020 4:4:4 AVIF encoder boundary.
- 2026-07-23: Rebased the renderer on intentional linear ACEScg film values from
  `film-master-render-pipeline`; physical scene recovery and optional correction
  profiles are not prerequisites.

- 2026-07-21: Planned a pure scene-linear ACEScg to BT.2020 PQ/HLG render stage.
  Rec.2100 is a display encoding, not nc's density or internal working space.
- 2026-07-21: Removed ambiguous ownership of the SDR base; this task now verifies
  PQ/HLG only, while `sdr-display-rendering` produces the independent SDR render.

- 2026-07-29: Started implementation from the completed shared display source
  and SDR branch. The HDR stage remains a pure renderer: one adjusted ACEScg
  source, fixed 203 cd/m² reference white and 1000 cd/m² target peak, BT.2020
  destination RGB, explicit PQ/HLG transfer assumptions, and reportable policy
  metadata. Preset activation and AVIF encoding remain downstream.
- 2026-07-29: Completed `pipeline::hdr`. The linear seam maps adjusted ACEScg/D60
  into BT.2020/D65, preserves adjusted `1.0` as 203-nit reference white, and
  applies a bounded C¹ Hermite shoulder to the 1000-nit / 4.926108-linear peak.
  Out-of-gamut color intersects the BT.2020 RGB cube radially at constant
  luminance with one common chroma scale; no per-channel terminal clip is used.
  The transfer seam mutates that buffer in place: PQ applies the ST 2084 inverse
  EOTF in absolute nits, while HLG applies the inverse reference OOTF (1000-nit
  peak, zero black, system gamma 1.2), a scene-linear radial signal-boundary
  intersection, and the reference OETF. Typed metadata fixes full-range CICP
  9/16/9 for PQ or 9/18/9 for HLG. The pre-transfer typed BT.2020 value remains
  borrowable by `output/gain-map-hdr-output`, which must convert it to common
  linear Display P3; the encoded pair is ready for `output/hdr-avif-output`.
- 2026-07-29: Verification covers current BT.2100 PQ and HLG vectors, neutral
  monotonic ramps, exact 203-nit reference-white and 1000-nit peak placement,
  shoulder continuity/monotonicity, constant-luminance radial gamut mapping,
  deterministic PQ/HLG goldens, explicit HLG assumptions, and fail-loud invalid
  inputs. Final gates passed: `cargo fmt --all --check`,
  `cargo clippy --all-targets --all-features --locked -- -D warnings`,
  `cargo build`, and `cargo test --no-fail-fast` (458 unit + 123 integration).
- 2026-07-29: Review tightened the encoded seam to an opaque nonlinear image
  type, made the BT.2020→common-linear-Display-P3 gain-map boundary explicit,
  documented the branch-specific HDR highlight knee and downstream preset memory
  dependency, and added direct linear-domain, matrix, and highlight-control
  tests. Review gates passed: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, and `cargo test`
  (461 unit + 123 integration).


## hdr-avif-output
**Status:** not started
**Updated:** 2026-07-23

- 2026-07-23: Added the missing owner for libavif/libaom FFI, static packaging,
  AVIF container and metadata conformance, determinism, licensing inputs, and
  codec-specific decoded-error thresholds. The initial contract is 10-bit
  full-range 4:4:4 AVIF v1.2 Advanced Profile, AV1 High Profile level ≤ 6.0,
  with `avif`/`mif1`/`miaf`/`MA1A` brands inside profile limits and explicit
  grid or general-brand-only behavior for oversized images.


## sdr-display-rendering
**Status:** done
**Updated:** 2026-07-28

- 2026-07-23: Rebased the renderer on intentional linear ACEScg film values and
  the shared post-ACEScg print controls from `film-master-render-pipeline`.

- 2026-07-21: Added the missing owner for scene-to-SDR rendering. It consumes
  characterized linear ACEScg and explicitly resolves print controls, reference
  white, tone mapping, destination gamut, and P3/sRGB transfer/profile output.
- 2026-07-21: Coordinated gain-map inputs: SDR reuses the shared linear
  WB/exposure/black adjustment stage, but owns its stronger SDR highlight/tone
  policy so that compression is not accidentally imposed on the HDR rendition.
- 2026-07-21: Made this renderer the sole owner of ACEScg → rendered linear
  destination RGB, including chromatic adaptation and gamut mapping; Display P3
  output only transfer-encodes and signals those already-rendered values.
- 2026-07-21: Corrected the implementation note: the shared linear adjustment
  stage is owned by `post-characterization-render-pipeline`, not characterization
  runtime.

- 2026-07-28: Started implementation from the merged `film-master-render-pipeline`
  split. The renderer will consume its typed, shared adjusted ACEScg source,
  produce rendered-linear Display P3 or sRGB plus explicit reference-white/tone/
  gamut metadata, and keep transfer encoding/profile signaling in the existing
  destination-output layer.
- 2026-07-28: Completed the pure SDR branch in `pipeline/sdr.rs`. It accepts only
  the typed shared adjusted ACEScg source, uses pinned AP1/D60 → P3-D65/sRGB-D65
  matrices, maps adjusted `1.0` to the binding 203 cd/m² reference white, applies
  a resolved Hermite highlight shoulder, and maps out-of-gamut colour radially
  toward the same-luminance neutral axis instead of clipping channels
  independently. The result is finite, non-negative rendered-linear RGB plus
  serialized policy metadata naming gamut, reference white, highlight control,
  shoulder, gamut policy, linear domain, and required transfer/profile.
- 2026-07-28: Added the destination seam `color::encode_rendered_sdr`: a
  rendered-linear P3 input receives only the sRGB transfer and Display P3
  profile, while rendered-linear sRGB receives the sRGB transfer/profile. It
  deliberately does not re-run the legacy Rec.709-working-space gamut transform.
  Refactored the existing transform body into a shared in-place helper without
  changing legacy behavior.
- 2026-07-28: Verification covers neutral/monotonic ramps, black and reference
  white, highlight shoulder behavior, finite radial mapping for synthetic
  out-of-gamut colors, golden P3 and sRGB vectors for the same ACEScg sample,
  deterministic public rendering/metadata, and destination transfer/profile
  signaling. `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, and `cargo test`
  are green (446 unit + 123 integration tests).
- 2026-07-28: Kept product activation out of scope: `display-p3` and
  `compatibility` remain planned preset names. `output/presets` owns exposing
  them after the HDR/gain-map/container dependencies land; `gain-map-hdr-output`
  can consume the renderer's pre-transfer pixels directly.
- 2026-07-28: Review/fix convergence hardened the finite-output invariant,
  replaced the independently-selectable encoder gamut with an opaque
  pixels-plus-metadata seam, bounded named-SDR `highlight_compress` to a mandatory
  0.75 baseline and 0.5 limiting knee, and removed terminal channel clamps from
  radial gamut mapping. Binary64 boundary intersection plus an exact limiting
  channel now constructs `[0,1]` output directly, while any non-finite or
  out-of-range postcondition fails with the pixel index. Added positive/negative
  overflow, control-direction, common-chroma-scale, and symmetric P3/sRGB
  transfer/profile regressions; documented the intentional frozen-legacy versus
  named-SDR semantic boundary. Final gates passed in order:
  `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, and `cargo test` (446 unit + 123 integration tests).


## gain-map-hdr-output
**Status:** in progress
**Updated:** 2026-07-29

- 2026-07-23: The spike changed the first container from HEIC to JPEG. The task
  now targets an 8-bit Display P3 base plus a half-resolution RGB map derived in
  linear Display P3, with final ISO 21496-1 and Android Ultra HDR v1 metadata.
  Stable libultrahdr 1.4.0 is gated on its JPEG marker-order fix and final-standard
  serialization; HEIC remains a future container.
- 2026-07-23: Separated 4.926108 linear display headroom from 2.300448 log2
  Ultra HDR XMP capacity, required actual per-pixel gain extrema to come from the
  canonical offset-adjusted formula over independently tone-mapped renderings,
  and added independent reconstruction plus semantic-agreement and
  ISO-preference tests for the two metadata dialects.
- 2026-07-23: Pinned the canonical per-channel gain form to
  `(HDR + offset_hdr) / (SDR + offset_sdr)` after selecting positive finite
  offsets. Added fail-loud domain rules and black/near-black/zero/invalid
  fixtures; no arbitrary epsilon, silent clamp, or `0/0` behavior is permitted.
- 2026-07-23: Pinned the formula's units to common linear Display P3 normalized
  by 203 cd/m² reference white: SDR/reference white and 203-nit HDR are `1.0`,
  while 1000-nit HDR enters as `4.926108...`; offsets use the same domain.
  Added equal-white gain-1, independently tone-mapped peak, and mixed-unit
  rejection fixtures so display headroom cannot be mistaken for pixel gain.
- 2026-07-21: Planned standards-neutral ISO 21496-1 output: Display P3 SDR base
  plus a gain map reconstructing the HDR rendition, initially targeting HEIC and
  requiring both Apple and non-Apple interoperability checks.
- 2026-07-21: Rewired the task to consume `sdr-display-rendering` rather than
  assuming profile synthesis alone produced an independently valid SDR base.
- 2026-07-21: Required both renditions to share the identical characterized and
  adjusted source, and pinned gain-ratio derivation to the standard-required
  common linear color domain rather than encoded P3/PQ/HLG channel division.
- 2026-07-29: Started implementation on `feat/gain-map-hdr-output`. The work
  begins at the typed pre-transfer SDR/HDR seams, keeps both renditions in common
  reference-white-relative linear Display P3 for canonical gain math, and leaves
  preset/CLI activation to `output/presets`. Current upstream encoder and
  final-standard metadata behavior will be verified before selecting the narrow
  container boundary.
- 2026-07-29: Implemented the first pure `pipeline::gain_map` seam: both branches
  are rendered from one `SharedDisplaySource`; HDR is transformed from linear
  BT.2020 into reference-white-relative linear Display P3 with a
  same-luminance radial compatibility mapping; the exact positive-offset formula
  produces one coupled SDR/HDR/gain result and actual per-channel extrema.
  Nine focused tests cover unit gain at reference white, real peak math,
  black/near-black, invalid offsets, mixed-unit rejection, the pinned matrix,
  gamut mapping, extrema, determinism, and dimensional coupling.
- 2026-07-29: Container work is blocked at the task's explicit conformance gate.
  The machine and current upstream `main` expose libultrahdr 1.4.0; the required
  segment-order correction remains open as google/libultrahdr PR #394 with
  changes requested, and no corrected release exists. Context7 is not connected
  in this environment. Completing final ISO 21496-1:2025 byte serialization
  requires the licensed standard or an approved equivalently authoritative
  source plus a reviewed/pinned corrected native source. The task stays in
  progress; review-fix-loop has not run because the implementation is not
  complete.
- 2026-07-30: Ran a partial-seam review/fix pass without attempting the blocked
  JPEG/ISO serialization or preset activation. Restored narrow pre-container
  dead-code allowances; added finite-positive per-channel gain gamma policy;
  pinned independently standards-derived BT.2020-primary → Display-P3 vectors;
  and separated common-linear HDR and gain ratios into opaque owned types with a
  consuming container seam. The focused 11-test gain-map suite and all four
  CI-equivalent gates (`fmt --check`, strict `clippy`, `build`, `test`) passed.
- 2026-07-30: Revalidated the container gate before resuming. Homebrew still
  provides libultrahdr 1.4.0; google/libultrahdr PR #394 remains open with
  changes requested after its latest patch; and current AOSP libultrahdr source
  still names the draft `urn:iso:std:iso:ts:21496:-1` namespace with ISO writing
  disabled by default. The ISO site confirms final ISO 21496-1:2025 is published
  but exposes only the abstract publicly. No further byte-layout, map
  quantization/downsampling, native FFI, memory calibration, or CLI activation
  is safe to implement until a permitted authoritative final-standard
  conformance checklist/oracle and a reviewed corrected encoder source are
  available.
- 2026-07-30: Correction to the preceding entry: its PR status came from a stale
  cached GitHub page. A live `gh pr view` query shows google/libultrahdr PR #394
  was approved and merged on 2026-07-27 as
  `11ac0c325bbf56ecf8be8704ff0f79fc9e1aac77`. The reviewed marker-order source is
  therefore available to pin even though Homebrew still packages 1.4.0. This
  removes the upstream-patch blocker; the separate final ISO 21496-1:2025
  conformance/oracle gate remains.
- 2026-07-30: With user approval, split delivery at the public format boundary.
  This task now owns a usable explicit `ultra-hdr-v1` JPEG with no ISO claim,
  while new downstream `output/iso-gain-map-metadata` owns final
  ISO 21496-1:2025 bytes, dual-dialect agreement, and the conformance oracle.
  `output/presets` still waits for that ISO extension before making the neutral
  dual-dialect `gain-map-hdr` output the default.


## presets
**Status:** not started
**Updated:** 2026-07-23

- 2026-07-23: Renamed `scene-master` to `film-master` and defined it as the
  unclamped linear ACEScg encoding of NC's intentional film rendering. Removed
  artifact/calibration assumptions; optional correction profiles do not affect
  preset availability.
- 2026-07-23: Added `conversion-versioning` as an explicit prerequisite because
  preset/default activation owns a golden-tested behavioral
  `pipeline_version` boundary.
- 2026-07-23: Added `hdr-avif-output` as a prerequisite so PQ/HLG presets cannot
  become reachable before AVIF encoding, profile/container conformance,
  packaging, determinism, and codec bounds are implemented.

- 2026-07-21: `gain-map-hdr` is the intended default. Separate presets make SDR
  Display P3, sRGB compatibility, linear ACEScg scene master, PQ, and HLG explicit;
  the ambiguous current `--output-hdr` name will not conflate float data with
  display HDR.
- 2026-07-21: Defined fail-loud CLI migration rules: the required output suffix
  must match the resolved container and is never rewritten; named presets are
  atomic and reject legacy output-selection flags; explicit combinations use
  `custom`; legacy flag-only calls retain their transitional TIFF behavior.
- 2026-07-21: Defined `scene-master` as a direct characterized-linear ACEScg
  branch that bypasses every print/display control. Removed cross-device checks
  from this task's definition of done; those remain exclusively in downstream
  `display-output-acceptance`. Preset mechanics remain independent of offline
  calibration; final color-accuracy acceptance waits for and exercises a real
  calibrated artifact as well as the explicit provisional fallback.
- 2026-07-21: Added the scene-master scale contract (no frame-local auto Dmax),
  distinguished current rendered `--output-hdr`, and made `roll-conversion` a
  real dependency. Preset migration now owns resolved-container suffixes,
  manifest/per-frame validation, shared/custom policy, and collision-free
  sidecar/report naming; the local stale base must reconcile before implementation.
- 2026-07-21: Tightened preset/roll semantics: scene-master rejects all effective
  non-default downstream controls after merge and reports the resolved defaults.
  Each batch image owns its path-derived sidecar, while one roll report retains
  stdout/`--report-file` routing and collision-checks against the entire batch.
- 2026-07-21: Added simple-control migration to the preset contract. Named
  presets characterize raw inversion first, then apply explicit
  `print.white_balance` and `print.linear_range`; legacy simple flags/keys warn
  and alias those fields, conflict with replacements, and are not emitted in new
  recipes/reports. Scene master rejects their non-default resolved values.
- 2026-07-21: Clarified that simple aliases preserve requested parameter values,
  not legacy pixels: WB generally does not commute with a channel-mixing
  characterization. Activating the new order emits a migration diagnostic and
  bumps `pipeline_version`; legacy no-preset TIFF retains current ordering during
  migration.
- 2026-07-21: Pinned linear-range alias merge semantics. Resolution starts from
  recipe/default; atomic `--linear-range` conflicts with legacy endpoint flags,
  otherwise `--clip-low`/`--clip-high` independently override their endpoints.
  Validation runs after merge, provenance is per endpoint, legacy use warns, and
  scene master rejects every final non-default range while allowing flags to
  reset recipe endpoints to `[0,1]`.


## iso-gain-map-metadata

**Status:** not started
**Updated:** 2026-07-30

- 2026-07-30: Split final ISO 21496-1:2025 serialization and dual-dialect
  conformance from the public Ultra HDR v1 JPEG implementation. This task will
  reuse exactly one SDR base, gain-map image, and canonical metadata model; it
  remains blocked on a permitted authoritative final-standard checklist or
  independent oracle, while `output/gain-map-hdr-output` can now complete and
  activate the explicitly named non-ISO `ultra-hdr-v1` path.


## gain-map-hdr-output (continued)

**Status:** done
**Updated:** 2026-07-30

- 2026-07-30: Completed the explicit, convert-only `ultra-hdr-v1` preset. It
  writes a quality-95, 4:4:4 Display P3 SDR primary plus a half-resolution
  grayscale luminance gain map using the public legacy Ultra HDR v1
  XMP/MPF/GContainer dialect. The canonical internal calculation remains RGB
  in common linear Display P3 for the downstream ISO serializer; this preset
  derives luminance because legacy XMP cannot signal a multichannel map. It
  writes no ISO or draft ISO marker and makes no ISO-conformance claim.
- 2026-07-30: Pinned and statically packaged google/libultrahdr at merged marker
  fix `11ac0c325bbf56ecf8be8704ff0f79fc9e1aac77` and libjpeg-turbo 3.1.0 at
  `20ade4dea9589515a69793e447a6c6220b464535`. Added complete distribution
  notices, recursive native-source build invalidation, Linux/macOS CI
  prerequisites, and deterministic snapshot verification covering 219
  libultrahdr files and 555 libjpeg-turbo files. Context7 was unavailable, so
  the narrow private FFI was verified against the pinned upstream headers,
  implementation, and executable decoder behavior.
- 2026-07-30: Verified the produced container independently with ExifTool and
  through libultrahdr reconstruction: legacy gain-map XMP, GContainer and MPF
  linkage, Display P3 ICC bytes, marker ordering, grayscale component count,
  odd dimensions, black/reference-white/peak/saturated vectors, and absence of
  ISO metadata are covered. macOS ImageIO's `sips` opens the corrected JPEG and
  reports its dimensions; physical Android verification remains conditional on
  an Android environment, with ordinary JPEG readers retaining the SDR primary.
- 2026-07-30: Calibrated the gain-map memory profile on an 18.7 MP HDRi real
  scan. The preflight estimated 1,851,158,528 bytes against a measured
  1,681,408,000-byte peak RSS, conservatively covering the overlapping render,
  gain-map, codec-input, and native-output buffers without changing the legacy
  profile.
- 2026-07-30: Completed the independent review/fix loop. Fixes covered
  center-aligned odd-size downsampling, display-render telemetry timing, native
  notices and reproducible pins, recursive build invalidation, ICC reassembly
  checks, and real pipeline-to-libultrahdr reconstruction. Both targeted
  re-reviews finished clean. Final gates passed:
  `scripts/check-vendored-native.py`, `cargo fmt --all --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo build --all-targets --all-features`, `cargo test --all-features`
  (479 unit + 126 integration tests), and `git diff --check`.


## ultrahdr-dependency-externalization

**Status:** not started
**Updated:** 2026-07-31

- 2026-07-31: Kept the reviewed local libultrahdr/libjpeg-turbo snapshot for the
  current gain-map change. Added this non-blocking follow-up to move dependency
  ownership back to Cargo after a published `ultrahdr-sys` release contains the
  required marker-order behavior and provides a fully pinned, network-free
  static native build. A Git dependency or system library is not the target
  because it would respectively retain repository-availability risk or make
  output depend on machine-installed native versions.


## gain-map-hdr-output (CI follow-up)

**Status:** done
**Updated:** 2026-07-31

- 2026-07-31: Fixed the clean-checkout native snapshot gate after the copied
  upstream `.gitignore` caused ordinary `git add` to omit 20 legitimate files
  that were still present and hashed locally. The files are force-tracked, and
  `scripts/check-vendored-native.py` now verifies that every hashed snapshot
  file is present in the Git index. This is an explicit local-vendoring
  workaround; `output/ultrahdr-dependency-externalization` removes the snapshot,
  tracking guard, and verifier together once a qualifying Cargo package exists.


## presets (continued)

**Status:** not started
**Updated:** 2026-07-30

- 2026-07-30: Added `algo/reference-anchored-sigmoid` as a required product
  prerequisite. The default recipe will use that sigmoid; output presets will
  not silently override an explicit reconstruction selection. Display defaults
  must preserve the reconstruction's black/midtone foundation and limit their
  differences from film-master to declared transfer, reference-white,
  highlight-headroom, and gamut adaptation. Exponential/simple remain advanced
  diagnostic paths pending a separate retirement decision.


## lossless-hdr-tiff

**Status:** not started
**Updated:** 2026-07-31

- 2026-07-30: Added distinct lossless HDR TIFF contracts for two use cases:
  bit-exact 32-bit float display-linear BT.2020 interchange, and losslessly
  stored 16-bit Rec.2100 PQ/HLG code values. Kept both separate from the linear
  ACEScg `film-master`, consumer HDR AVIF, and gain-map JPEG. The task requires
  standards-valid, independently inspected signaling and forbids private tags or
  unsupported viewer-compatibility claims.
- 2026-07-30: Made `color/colorimetry-source-of-truth` a prerequisite so the new
  BT.2020 profiles and encoder adapters cannot introduce another set of magic
  matrices or luma constants. `output/presets` now depends on this task for
  `hdr-linear-tiff`, `hdr-pq-tiff`, and `hdr-hlg-tiff` activation; its standalone
  `display-p3` and `compatibility` policies are explicitly 16-bit losslessly
  stored TIFFs.
- 2026-07-31: Review found that the task required failure-safe final paths while
  its graph did not require the existing transactional-write implementation.
  Added `io/transactional-output-writes` as a real prerequisite so the TIFF
  encoders reuse one atomic-write boundary instead of duplicating it or writing
  directly to final paths.
