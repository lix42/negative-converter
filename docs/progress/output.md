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
- **`ultra-hdr-v1` is not HDR on Apple platforms — measured, not inferred
  (2026-08-06).** Apple ignores Google's legacy Ultra HDR v1 XMP entirely, so
  that preset's file opens as an ordinary SDR JPEG on macOS/iOS (ImageIO reports
  no gain map of either kind, headroom 1.0). Only the ISO 21496-1 dialect is read
  there. Matters to `analysis/display-output-acceptance`, whose cross-device pass
  must expect SDR from the legacy preset rather than treat it as a failure, and
  it is why the future `gain-map-hdr` default is dual-dialect. The ISO dialect
  (`Dialects::LegacyPlusIso`) is implemented and Apple-verified but still has
  **no CLI path** — `output/gain-map-dialect-activation`.
- **Verify gain-map output with `scripts/iso-decoder-oracle/`** (Apple ImageIO,
  macOS-only, not in CI). exiftool and libultrahdr both accept a file no decoder
  parses — that is exactly how a placement defect shipped. Two traps when using
  it: the sample set needs `NC_ISO_SAMPLE_EV=3.0` or the gain map is inert
  (`GainMapMax` ≈ 1.003x at defaults, because the exponential curve leaves
  content below the SDR shoulder), and the reported `headroom 4.9261084` is nc's
  own declared `1000/203` echoed back rather than a measurement — it reads the
  same on a flat map, so the pass condition is `PRESENT` **plus** a `GainMapMax`
  above 0.
- **Native dependency packaging:** the shipped Ultra HDR implementation keeps
  the audited libultrahdr/libjpeg-turbo snapshot in-tree. **The plan changed on
  2026-08-05** (`output/ultrahdr-dependency-externalization`, id kept, scope now
  *removal*): the exit is nc writing the Ultra HDR v1 XMP and MPF container in
  Rust so the C/C++ dependency leaves the tree, **not** swapping in a published
  crate. The published `ultrahdr-sys` cannot qualify at any version — it obtains
  libjpeg-turbo by build-time clone at a mutable tag, or from a machine-installed
  library. Two facts for anyone touching this: our snapshot's
  `libultrahdr/CMakeLists.txt` is the **one file modified** from upstream
  `11ac0c3` (both libjpeg-turbo fetch blocks replaced by `DOWNLOAD_COMMAND ""`),
  and only **6** native calls are on the shipping path — the rest of the `uhdr::`
  surface is the test-only decode oracle, to be replaced by captured goldens
  rather than kept as a dev-dependency, since `cargo test` would still drag the
  native toolchain into CI.
- **The AVIF path does *not* use libavif** (decided 2026-08-05 in
  `hdr-avif-output`, amending the spike note's encoder paragraph): no published
  crate ships libavif ≥ 1.4.2 and `avif-serialize` cannot emit the required
  `MA1A` brand, so it is published `libaom-sys` for the codestream plus an
  nc-owned Rust MIAF/AVIF container writer. Consequence for anyone touching it:
  `av1C` is filled by **parsing the encoded sequence-header OBU**, never from the
  encoder config. Windows static builds are deferred (no Windows CI runner) →
  `output/hdr-avif-windows-packaging`.
- **`hdr-pq` and `hdr-hlg` are live**, as explicit `convert`-only presets
  requiring an `.avif` path — and **`hdr-linear-tiff`** since 2026-08-05, requiring
  `.tif`/`.tiff`, so **six** preset names were accepted at that point (eight once
  chunk B's `hdr-pq-tiff`/`hdr-hlg-tiff` landed — see below). Two things
  downstream tasks inherit: the suffix and convert-only rules are driven by one
  `cli::required_extensions` table (extend *that*, don't add a parallel check), and
  `stages::render_gain_map_source` is now **`render_display_source`** returning
  `DisplaySource`, because every display preset shares it. Note what the table's
  coupling actually means: pinning a suffix *is* what makes a preset
  `convert`-only, because roll derives frame names itself and nothing yet makes that
  derivation honour a required extension. `film-master` pins no row and stays
  roll-capable — so `output/presets` must add roll-aware naming before any of the
  suffix-pinning presets (four at this point, six now) can run in a roll, and
  `hdr-linear-tiff` writes a TIFF
  yet is still refused there.
- **`hdr-linear-tiff` is the display-linear HDR interchange master**, and its three
  non-identities are the point: it is not `film-master` (linear ACEScg *before*
  display rendering), not `hdr-pq`/`hdr-hlg` (no transfer applied), and not
  `--output-hdr` (print-rendered float in the selected output space). It writes
  `pipeline::hdr::render_linear`'s pre-transfer BT.2020/D65 samples verbatim as
  unclamped f32 — bit-exact, values running to ≈4.926108. Three things other epics
  inherit: **(a)** `io::encode::encode_hdr_linear` takes the opaque
  `LinearBt2020Hdr` **by value** (via a new `into_parts`), so a future consumer must
  not reintroduce a borrow-and-copy; **(b)** the **report block, not the ICC
  profile, is authoritative** for reference white / peak / headroom — the ICC PCS
  stops at the media white, so no v4 profile can carry them, and the profile
  deliberately has no `cicpTag` because the full-range flag would over-state a
  range these samples exceed; **(c)** its peak memory phase is the **render**, not
  the encode (no quantization buffer, and `tiff` streams strips instead of
  assembling a container), so adding lossless TIFF compression later reintroduces a
  staging term. It is **not** the only such profile — `HdrCodedTiff` and
  `UltraHdrV1` peak at render too. Which phase peaks is per-profile and measured:
  read it off `pipeline::memory`'s
  `which_phase_peaks_is_per_profile_and_measured_not_assumed`, never off a
  category, because prose about it has been wrong twice.
- **`definitions::BT2020` is now fed to Little CMS** by
  `color::hdr_linear_bt2020_icc`, making **five** lcms2-consumed colour spaces
  (`REC709`, `DISPLAY_P3`, `ACESCG`, `PROPHOTO`, `BT2020`). Editing any of the five
  changes embedded ICC bytes and lcms2-transformed pixels *even with `pinned.rs`
  untouched and every audit ulp at 0*, and nothing automated catches it. The
  definitions module note used to say `BT2020` had no runtime consumer; that is
  fixed.
- **`hdr-pq-tiff` and `hdr-hlg-tiff` are live**; with the SDR pair
  (`display-p3` / `compatibility`, 2026-08-09) **ten** preset names are accepted
  today, enumerated once in `OutputPreset::ALL`, which the parse diagnostics are
  generated from. The coded TIFFs store the *same rendition* the AVIF presets code, as full-range
  16-bit TIFF codes. Five things downstream tasks inherit:
  **(a)** for an **RGB** data space ICC.1:2022 §10.3 *requires*
  `MatrixCoefficients = 0`, so the `9` in
  `HdrRenderMetadata::cicp_matrix_coefficients` (correct for AVIF, which stores
  Y'CbCr) must never be copied into an RGB profile — the report writes 0 for that
  reason; **(b)** `hdr::transfer_for` now answers for **four** presets, so it can
  never be used to pick a container — `convert_frame` matches the preset
  exhaustively, and reintroducing an `if let Some(transfer)` chain there would hand
  the TIFF presets to the AVIF encoder; **(c)** the PQ profile is an
  **extended-range A2B** (PCS `Y = L/203`, unclipped) built through `lcms2-sys`,
  because the safe crate cannot insert pipeline stages and a matrix-shaper TRC
  cannot exceed `[0, 1]`; **(d)** the HLG profile is **scene-referred** since HLG's
  OOTF is not per-channel separable — a display-referred one needs a 3D CLUT;
  **(e)** they are documented as **limited-interoperability interchange, never
  display-ready**, since TIFF has no CICP tag of its own. macOS ColorSync *parses*
  them (`sips` names the profile), and the 2026-08-06 viewer gate confirmed they
  render correctly — but that gate was **not discriminating** for HDR presentation
  (diffuse-highlight scene, and the still-default exponential curve rather than the
  sigmoid), so presentation stays unclaimed.
- **⚠ The coded-HDR profiles are valid *source* profiles, not conformant
  Display-class ones, and `output/presets` owns closing that.** Verified against
  ICC.1:2022: §8.4.2 requires `BToA0Tag` (only `AToB0Tag` is written, so a strict CMM
  cannot use them as a transform *destination*) and §8.2 requires
  `chromaticAdaptationTag` (missing, so the D65 encoding white is unrecoverable).
  Deferred there on 2026-08-06 because closing them needs two more pinned colorimetry
  artifacts and **changes the profile bytes**. Anyone reading `synth_coded_hdr`'s
  conformance notes should treat the `cicp` tag — not the profile's tag completeness —
  as the authoritative signal. A conformant `BToA0` is also inherently capped at
  ≈406 cd/m² by the `u1Fixed15` PCS, so it can never carry the `AToB0`'s range.
- **ICC PCSXYZ in a LUT tag is `u1Fixed15Number`** (`1.0` → `0x8000`), so any future
  A2B matrix must be pre-divided by `32768/65535` or every luminance comes out 2×.
  And Little CMS serializes `mAB ` only for a recognized stage pattern — M curves →
  Matrix → B curves is the compact one, and the identity B curves are mandatory.
- **`pinned::BT2020_TO_XYZ_D50` exists because nc now authors a profile itself.**
  Every other nc profile lets Little CMS derive colorants from pinned primaries;
  an A2B pipeline cannot, so the colorant matrix became a pinned artifact with its
  own audit entry and an independent anchor (the colorants lcms itself computed,
  read back with `exiftool`).
- **ISO 22028-5:2026 was never a blocker for the TIFF work**, correcting
  `iso-gain-map-metadata`'s 2026-08-04 note that grouped `lossless-hdr-tiff` with it.
  The reference-white and peak numbers come from the closed spike; TIFF 6.0,
  ICC.1:2022, H.273 and BT.2100-3 are all obtainable.
- **The preset/`RunProfile` ownership rule, from the `ultra-hdr-v1` and now AVIF
  precedents:** whichever task ships an explicit `convert`-only preset also adds
  and calibrates that preset's `memory::RunProfile`. `output/presets` verifies
  profile *selection* and owns the default, the rest of the suffix table, `custom`,
  and roll integration — it does not re-derive an already-calibrated model. Recorded
  in both task files.
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
**Status:** done
**Updated:** 2026-08-05

- 2026-07-23: Added the missing owner for libavif/libaom FFI, static packaging,
  AVIF container and metadata conformance, determinism, licensing inputs, and
  codec-specific decoded-error thresholds. The initial contract is 10-bit
  full-range 4:4:4 AVIF v1.2 Advanced Profile, AV1 High Profile level ≤ 6.0,
  with `avif`/`mif1`/`miaf`/`MA1A` brands inside profile limits and explicit
  grid or general-brand-only behavior for oversized images.
- 2026-08-05: Started with a STEP 0 packaging/feasibility spike, because the
  task's written design ("wrap `libavif` 1.4.2 or newer") has no supply chain:
  **no published crate ships libavif ≥ 1.4.2.** `libavif-sys` 0.17 is libavif
  **1.0.4** + libaom 3.11.0 — below the task's floor and predating Advanced
  Profile / `MA1A` brand writing. Vendoring upstream was measured at ~1,445
  files / 45 MB for libaom alone (libavif itself is only 31 files / 1.3 MB), and
  would double down on exactly the in-repo-snapshot pattern that
  `output/ultrahdr-dependency-externalization` exists to undo.
- 2026-08-05: **Decision (with user approval): published `libaom-sys` for the AV1
  codestream + an nc-owned Rust MIAF/AVIF container writer.** This supersedes the
  spike note's "narrow Rust FFI around libavif" for this task only; the spike's
  binding *numbers* (203-nit reference white, 1000-nit peak, gain-map domain, RGB
  map) are untouched. Rationale: `libaom-sys` 0.17.2 vendors libaom 3.11.0 inside
  the crate and builds it statically via cmake with **no network and no in-repo
  snapshot** (measured: 29 s clean build on macOS/aarch64), which is precisely the
  dependency shape `ultrahdr-dependency-externalization` names as the target. The
  container is ours because **`avif-serialize` 0.8.9 hardcodes
  `compatible_brands: [mif1, miaf]` with no setter** — it cannot emit the required
  `avif`/`MA1A` brands, and has no grid support for the oversized path. Writing the
  container also turns the task's "independently inspect ... rather than assuming
  encoder defaults establish conformance" clause from an audit into an authored
  guarantee.
- 2026-08-05: Confirmed the target bytes are achievable before committing. Local
  libavif 1.4.2 / aom 3.14.1 `avifenc -d 10 -y 444 -r full --cicp 9/16/9` writes
  major brand `avif` + compatible `avif mif1 miaf MA1A`, supports `--clli`, and
  three repeated `-j 1` encodes were byte-identical. That reference file's full box
  layout was decoded and used as the writer's target: `hdlr` 33, `pitm` 14, `iloc`
  30 (v0, 4/4 offset/length sizes, absolute offset), `iinf` 40 / `infe` 26
  (v2, `av01`, item name `"Color"`), `ipco` 87 (`ispe`,`pixi`,`av1C`,`colr`,`clli`),
  `ipma` 24 — with **only `av1C` carrying the essential bit** (`0x83`), and `av1C`
  `configOBUs` deliberately empty.
- 2026-08-05: STEP 0 probe result (scratchpad, not committed): libaom encodes
  10-bit 4:4:4 full-range via `AOM_USAGE_ALL_INTRA` + `AOM_IMG_FMT_I44416`,
  `g_profile = 1`, `g_threads = 1`, `g_limit = 1` and
  `full_still_picture_hdr = 0`. The hand-written container's `meta` box came out
  **byte-identical to libavif 1.4.2's except one byte** — the `iloc` extent length,
  which differs only because our codestream is 49 B vs its 70 B. `avifdec` (dav1d,
  independent of libaom) decodes it as 64x64, 10-bit, YUV444, Full range, CICP
  9/16/9, CLLI 1000,203; ExifTool agrees on brands and CICP; the y4m round trip
  reports `C444p10` / `XCOLORRANGE=FULL` with chroma preserved exactly and luma
  max error 6/1023 (RMS 1.62) at `cq_level` 20 — the first datapoint for the
  codec-bounds chunk.
- 2026-08-05: **Two gotchas worth keeping.** (1) libaom's packet list is *per
  `aom_codec_encode` call*: draining `aom_codec_get_cx_data` only after the flush
  silently yields a **0-byte codestream**, because the frame is emitted during the
  first call (`lag_in_frames` is 0 for all-intra). Drain after every call. (2)
  `AV1E_GET_SEQ_LEVEL_IDX` reports the *target* level and returned **31**
  (unset) — writing it into `av1C` would have signalled a bogus level where
  libavif writes 0. So **every `av1C` field must be parsed back out of the
  codestream's own sequence-header OBU**, not read from the encoder config. The
  probe's reduced-still-picture-header parser confirms `seq_profile` 1,
  `still_picture` 1, `reduced_still_picture_header` 1, `seq_level_idx_0` 0,
  CICP 9/16/9, full range, 4:4:4, 10-bit — and is the seed of the conformance
  inspector the task's verification section requires.
- 2026-08-05: Windows static builds **deferred** with the gap recorded (user
  decision): CI is `[ubuntu-latest, macos-15]` with no Windows runner, so the
  task's three-platform clause has no coverage. Delivery gates macOS + Linux;
  a Windows follow-up is filed when this task closes. Linux/macOS already install
  `cmake clang libclang-dev nasm` from the gain-map work, which is what libaom's
  build needs — so no new CI prerequisite is expected, but the x86_64 Linux build
  is unproven locally and CI is the first place it compiles.
- 2026-08-05: **Confirmed the Advanced Profile limits against the published AVIF
  v1.2 text** rather than from memory, which also discharges the brand/limit half
  of the spike's re-homed normative-text gate — unlike ISO 21496-1, the AVIF and
  AV1 specifications are public. Verbatim: Advanced Profile requires "the High
  Profile and the level shall be 6.0 or lower", and its coded image items "may not
  have a number of pixels greater than 35651584, a width greater than 16384 or a
  height greater than 8704", with brands `avif, mif1, miaf, MA1A`. Those four
  numbers are now named constants in `io::avif` with the quote attached. Note the
  level bound is `seq_level_idx <= 16`, *not* "an index that looks like a 6":
  the index is `(major - 2) * 4 + minor`, so 17/18/19 are levels 6.1/6.2/6.3 and
  are over the ceiling.
- 2026-08-05: Added `pinned::BT2020_NCL_RGB_TO_YCBCR` through the colorimetry
  maintenance workflow before writing any encoder code, because AVIF's
  `matrix_coefficients = 9` means the file stores Y'CbCr while the renderer
  produces R'G'B' — and a standards matrix may not be inlined in a stage. Details
  and its three verification anchors are in `docs/progress/color.md`; the
  headline for this epic is that it audits at `ulps = 0`, moves no existing
  artifact, and is therefore **not** a pixel change to any shipped path.
- 2026-08-05: Implemented `src/io/avif.rs`: quantization, the libaom FFI, the
  container writer, the sequence-header inspector, and error translation.
  `encode(RenderedHdr, &Path) -> (Staged, EncodeOutcome, AvifSummary)` mirrors
  `io::ultra_hdr::encode`, so the whole file is built in memory and committed
  through `io::staged` — a failure anywhere leaves nothing at the destination
  (a test proves the uncommitted path). Native handles are RAII guards
  (`Encoder` boxes the context because libaom stores interior pointers to it;
  `Image` owns the `aom_img_alloc` frame), every `unsafe` block carries a SAFETY
  comment, and libaom status codes become `NcError::Write` /
  `NcError::Other` / `NcError::Resource` with `aom_codec_error_detail` text.
- 2026-08-05: The encoder does not trust itself. After encoding,
  `parse_sequence_header` reads the codestream back and `verify_codestream`
  refuses to package a file whose coded seq_profile, still_picture, subsampling,
  bit depth, CICP, colour range or frame size disagrees with the renderer's
  declared contract; `resolve_profile` then classifies the *parsed* level and the
  real dimensions, so an encoder that picked a higher level than expected
  downgrades the brand instead of being mis-advertised. `MA1A` is written only
  when every published limit holds, and `AvifProfile::GeneralOnly` carries the
  reason back for the report.
- 2026-08-05: Two deliberate policy calls to revisit at calibration. `CQ_LEVEL`
  is **provisional** — it round-trips a neutral ramp within 6/1023 but is not yet
  a reviewed quality decision. And `clli` is written for **PQ only**: PQ carries
  absolute luminance so `MaxCLL`/`MaxPALL` report the pinned 1000-nit peak and
  203-nit reference white, whereas HLG is display-referred, so inventing absolute
  values there would be a false claim. `MaxPALL` is policy, not a per-image
  measurement; measuring it per image is deferred with the codec bounds.
- 2026-08-05: Clipping in quantization is **reachable, not defensive**. BT.2100-2
  Table 9's full-range chroma row puts a fully saturated primary at
  `±0.5 · 1023 + 512`, i.e. half a code outside the range at each end, so those
  samples are counted into `EncodeReport` rather than silently clamped. A
  non-finite sample falls back to *its own* neutral level — 0 for luma but 512 for
  chroma — so a numerical fault cannot turn into a saturated colour. `OutputStats`
  means are taken on the R'G'B' signal, not the written Y'CbCr codes, because the
  type is defined per R/G/B channel and reporting chroma under `mean[1]` would
  mislead.
- 2026-08-05: Verified nc's *own* output with independent tools, not just unit
  tests. `avifdec` (dav1d) reports both files as 64x1, 10-bit, YUV444, Full range,
  CICP 9/16/9 (PQ, CLLI 1000,203) and 9/18/9 (HLG, no CLLI), with no ICC/EXIF/XMP;
  ExifTool agrees on brands `avif, mif1, miaf, MA1A`; macOS `sips` opens both at
  10 bits. In-repo, a libaom round trip decodes the container's own `iloc` extent
  and checks neutral pixels stay achromatic and the ramp stays monotonic, and
  three repeat encodes are byte-identical.
- 2026-08-05: Packaging shape confirmed: `libaom-sys` is an ordinary
  `[dependencies]` entry with `default-features = false, features =
  ["av1_encoder"]`, and the decoder is a `[dev-dependencies]` feature so tests can
  round-trip. Verified rather than assumed — `aom_codec_av1_dx` is **absent from
  the release binary**, so resolver v3 does keep the dev-dependency feature out of
  `cargo build`. No in-repo native snapshot exists;
  `scripts/check-vendored-native.py` still reports only the libultrahdr /
  libjpeg-turbo files. Caveat: the libaom round-trip test shares an implementation
  with the encoder, so it proves self-consistency, not conformance — the
  independent-decoder bounds the task requires remain the codec-bounds step's job.
- 2026-08-05: All four CI-equivalent gates green in order — `cargo fmt --all
  --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, and
  `cargo test` (555 unit + 130 integration, up from 539 + 130), plus
  `scripts/check-vendored-native.py`. **Still open in this task:** CLI/preset
  activation of `hdr-pq`/`hdr-hlg` (which removes `io::avif`'s module-level
  `dead_code` allow), the `RunProfile` memory model and its calibration, the
  oversized-image grid path, codec error bounds via an independent decoder,
  report wiring for `AvifSummary`, and the licence/patent-review record.
- 2026-08-05: **Recorded the `output/presets` boundary** in both task files, because
  each claimed the memory-calibration gate and a literal reading meant either
  duplicated work or a mutual gap. The rule, from the `ultra-hdr-v1` precedent:
  whichever task ships an explicit `convert`-only preset also adds and calibrates
  that preset's `RunProfile`; `output/presets` verifies profile *selection* and owns
  the default, the rest of the suffix table, `custom`, and roll integration. That
  ordering is also forced — `output/presets` additionally depends on
  `output/iso-gain-map-metadata`, which is hard-blocked on the paywalled
  ISO 21496-1:2025 text, so deferring activation would have left a complete, tested
  AVIF encoder unreachable behind an unrelated standard.
- 2026-08-05: Activated `hdr-pq` and `hdr-hlg` as explicit `convert`-only presets.
  `OutputPreset` gained the two names (moved out of the "not accepted yet" list — the
  `hdr-*-tiff` presets are *different* presets and stay planned) plus
  `hdr_transfer()` as the single place a preset becomes an `HdrTransfer`. Both
  resolve `OutDepth::U16` for the optional IR TIFF only; the primary is fixed 10-bit
  AVIF. The suffix and roll gates were **generalized rather than special-cased**: one
  `required_extensions` table now drives both the `.avif`/`.jpg` requirement and the
  "convert-only" refusal, so a future container cannot acquire one rule and miss the
  other.
- 2026-08-05: Renamed `stages::render_gain_map_source` → `render_display_source` and
  `GainMapSource` → `DisplaySource` (one call site). The function was never
  gain-map-specific — it is the shared reconstruction + print-controls source, and
  both display presets now consume it, so a gain-map-shaped name would have been
  actively misleading about what `hdr-pq` shares with `ultra-hdr-v1`.
- 2026-08-05: Added `RunProfile::HdrAvif` and **calibrated it on two real scans**,
  which is the part worth repeating. Solving `measured = px·(28 + X) + fixed` across
  an 18.66 MP and a 74.65 MP frame gave 78.47 B/px with only ~7.9 MB fixed — clean
  linear scaling — so the true AVIF staging is 50.47 B/px. Pinned
  `AVIF_STAGING_BYTES_PER_PX = 48`, leaving `accounted` 3.4–3.8% *under* measured for
  the 15% allowance to cover. **A first pass at 64 B/px was wrong in the expensive
  direction**: padding the enumerated buffers double-counts the allowance and put the
  18.66 MP estimate at 1.43x measured, which rejects runs the machine could serve.
  HLG measured 1.503 GB against the same estimate, so one profile covers both.
- 2026-08-05: Pinned `CQ_LEVEL = 8` after measuring the quality/size curve on a real
  scan plus a four-class test field (`cq` 0 / 8 / 12 / 20 → 20.38 / 0.99 / 0.35 /
  0.07 MiB at max code error 0 / 10 / 14 / 20 of 1023). It is a fixed part of the
  preset like `ultra_hdr::JPEG_QUALITY`, not a new knob. Two findings recorded in the
  constant's doc: **`cq_level = 0` is mathematically lossless**, so AV1 could carry a
  bit-exact HDR still at ~20x the size if a preset ever wants one; and AVIF is nc's
  *delivery* container, so the archival paths remain `film-master` and the planned
  lossless HDR TIFFs.
- 2026-08-05: Codec bounds are pinned by **equality, not tolerance**, because AV1
  reconstruction is normatively specified and bit-exact. Measured with
  `avifdec`/dav1d at `cq_level` 8 on the four-class field (max, RMS per plane):
  PQ `(9, 0.702) (10, 0.849) (9, 0.591)`, HLG `(8, 0.645) (8, 0.782) (7, 0.615)`.
  The committed test decodes with libaom and **reproduces those dav1d numbers
  exactly**, which is what makes a CI-runnable in-repo decode a legitimate stand-in
  for the independent one; a neutral ramp comes back with chroma at exactly the
  achromatic level.
- 2026-08-05: **Oversized-image policy: general-brand-only, no grid.** The AVIF v1.2
  text permits either, and implementing a conforming grid would mean pinning tile
  ordering and edge-tile behaviour for a case nc can already serve correctly. Proven
  on a real 74.65 MP scan: the file is a valid AVIF, `MA1A` is omitted, and the
  report plus a `--strict`-promotable warning name the limit. That run also surfaced
  a reporting bug — libaom emits `seq_level_idx = 31`, AV1's **"maximum parameters"
  sentinel**, which my first version formatted as "level 9.3", a level the
  specification does not define. `level_name` now renders 31 and the 24..=30 reserved
  range as names. The brand *decision* was right throughout; only the label was wrong.
- 2026-08-05: Wired the report: a new `avif` block carries the profile (and, when
  general-brand-only, the reason), bit depth, the AV1 profile/level **parsed from the
  codestream**, the CICP triple, range and coded size — evidence about the artifact
  rather than an echo of the request. Recorded libaom's licence and the Alliance for
  Open Media Patent License 1.0 in `THIRD_PARTY_NOTICES.md`, stating plainly that the
  summary is not a completed legal review: the *standards* half of the spike's
  re-homed gate is discharged (AVIF/AV1 are public and were checked against their
  normative text), while counsel review of the patent grant stays with release.
- 2026-08-05: End-to-end on the 18.66 MP Phoenix scan, both presets: 5184x3600,
  10-bit, YUV444, Full range, CICP 9/16/9 (PQ, CLLI 1000,203) and 9/18/9 (HLG, no
  CLLI), brands `avif mif1 miaf MA1A`, level 6.0 — the ceiling, legitimately, since
  18.66 MP exceeds level 5.x's 8,912,896-pixel limit. PQ 1.03 MB, HLG 3.29 MB. Under
  `--strict` both exit 1 on the documented IR-plane warning, as any HDRi scan does.
  All four gates green: 559 unit + 133 integration.
- 2026-08-05: **Review round: `clli` is now measured, and the per-axis limit was
  wrong.** (1) The earlier "MaxPALL is policy, measurement deferred" call was not
  defensible: CTA-861.3 defines both fields as properties of *this content* and
  displays tone-map from them, so writing the 1000/203 constants made a nearly
  black frame claim a 1000-nit peak. `pipeline::hdr::render_linear` now measures
  per-pixel luminance (`dot(rgb, BT2020_LUMA) · 203`, where its values are still
  display-linear and reference-white-relative), MaxCLL = peak and MaxFALL = mean,
  and carries them as `HdrRenderMetadata::content_light` for `io::avif` to write.
  On the `hdr-48bit` fixture the box now reads 114/41 instead of 1000/203, and the
  same frame four stops darker reads 7/3 — confirmed on the written files by
  `avifdec --info` (libavif/dav1d, independent of nc's parser), and pinned in-repo
  as the dark-versus-bright regression at both the unit and CLI levels. Render
  metadata is not a recipe key, so
  `PIPELINE_FINGERPRINTS` and `params_hash` are unmoved (verified, not assumed),
  and the pinned codec bounds still match exactly — no pixel moved. HLG still
  omits the box. (2) The dimension gate used `aom_img_alloc`'s documented `2^27`,
  which bounds the *allocator*; the **encoder** refuses anything over 65,536 per
  axis (`av1_cx_iface.c:646-647`, `RANGE_CHECK(cfg, g_w, 1, 65536); // 16 bits
  available` — a format limit, since `frame_width_bits` is `f(4)`). An axis in
  `65_537..=2^27` therefore paid for a full quantization pass and three plane
  allocations before failing as a generic exit-1; it is now exit 4 before any
  allocation. (3) Three smaller review fixes: the module-wide
  `#![allow(dead_code)]` in `io::avif` is gone now that the presets are wired (no
  item needed a replacement allow); `OutputPreset::hdr_transfer` became
  `pipeline::hdr::transfer_for`, since `types` is the shared-types leaf and must
  not depend on a pipeline module; and `write_container`'s 32-bit guard now counts
  the 8-byte `mdat` header, because a codestream in `u32::MAX - 7 ..= u32::MAX`
  would have wrapped the box size to 1..8 and written a malformed file with no
  error — `bx` now converts the size with a checked cast instead of `as u32`.
- 2026-08-05 (ship review): Codex caught the `RunProfile::HdrAvif` **render** phase
  under-counting by 4 B/px on every IR input. The shared display source is
  `image`-shaped, not RGB-only — reconstruction carries the IR plane through
  `AcesCgImage` into `AdjustedAcesCgImage` — so render holds decoded RGB+IR *and*
  shared RGB+IR *and* the rendition. `UltraHdrV1` already modelled this correctly
  with `mul(image, 2)`; the new profile did not. Now
  `sum(mul(image, 2)?, rendition)`. The gate decision is unaffected (encode is
  still the peak, and the estimate stays byte-identical to the calibrated
  1,765,311,488 on the 18.66 MP scan) — what was wrong was the reported per-phase
  breakdown, which CLAUDE.md requires to track the code. Lesson for the next
  profile: check whether a stage's buffer is `image`-shaped (carries IR) before
  modelling it as a flat RGB buffer.

- 2026-08-05: **Two residual risks, neither resolvable here.** (1) Only
  `aarch64-apple-darwin` is installed on this machine, so the **x86_64 Linux build of
  libaom is unproven until CI runs** — CI already installs `cmake clang libclang-dev
  nasm` from the gain-map work, which is what libaom needs, and `io::avif` contains
  no platform-gated code, but CI is genuinely the first place it compiles. (2)
  Windows is deferred by decision with no CI runner; filed as
  `output/hdr-avif-windows-packaging`.


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


## lossless-hdr-tiff (chunk A — hdr-linear-tiff)

**Status:** in progress
**Updated:** 2026-08-05

- 2026-08-05: Started after `output/hdr-avif-output` merged (`3d62db7`) made this
  task executable. Split delivery in two chunks with user approval — A
  `hdr-linear-tiff`, B `hdr-pq-tiff`/`hdr-hlg-tiff` — because the task file itself
  requires the PQ/HLG *signaling contract* to be pinned before that variant is
  implemented, and the linear half has no signaling ambiguity to resolve. Presets
  are activated in-task per the `hdr-pq`/`hdr-hlg` precedent and the epic summary's
  rule, not deferred to `output/presets`.
- 2026-08-05: **Corrected a recorded blocker: this task is not gated on a paywalled
  standard.** `iso-gain-map-metadata`'s 2026-08-04 entry flagged ISO 22028-5:2026
  for purchase "since `hdr-avif-output` and `lossless-hdr-tiff` hit the same gate".
  They do not: the 203-nit reference white and 1000-nit peak were pinned by the
  *closed* HDR spike and this task only records them, while the signaling contract
  comes from ICC.1:2022, ITU-T H.273, BT.2100-3 and TIFF 6.0 — all obtainable. AVIF
  shipped on the same footing.
- 2026-08-05: **Read ICC.1:2022 §9.2.17/§10.3 rather than working from memory**, and
  it settles Chunk B's contract in advance. The `cicpTag` is 12 bytes (`'cicp'`,
  four reserved zero bytes, then ColourPrimaries/TransferCharacteristics/
  MatrixCoefficients/VideoFullRangeFlag as `uInt8` per ITU-T H.273), permitted only
  for an RGB/YCbCr/XYZ data space in an Input or Display profile. The spec's own
  examples name our code points verbatim — `9-16-0-1` = "PQ R'G'B' full range
  representation specified in Recommendation ITU-R BT.2100-2, Table 9", `9-18-0-1`
  = the HLG equivalent. **The trap worth carrying forward: MatrixCoefficients must
  be 0 for an RGB data space** (§10.3 requires it), whereas the AVIF path writes 9
  because AVIF stores Y'CbCr — so copying `HdrRenderMetadata::
  cicp_matrix_coefficients` into a TIFF profile would be non-conformant. The tag
  *supplements* rather than replaces the transform tags, so a real TRC is still
  needed beside it. `lcms2` 6.1.1 can write it in **safe Rust**
  (`Profile::write_tag(TagSignature::CicpTag, Tag::VideoSignal(…))`, Little CMS
  2.19) — no `lcms2-sys` FFI, unlike the global error handler.
- 2026-08-05: Implemented Chunk A. `hdr::LinearBt2020Hdr::into_parts` (mirroring
  `RenderedHdr::into_parts`) hands the buffer to the new
  `io::encode::encode_hdr_linear`, which takes the render **by value** — encoding
  from `image()`'s borrow would have put a second full-frame f32 image on the heap
  that the memory model does not account for. The encoder is a *domain-typed* entry
  point rather than a flag on `encode`: it accepts the opaque BT.2020 type so those
  samples cannot be confused with the Rec.709 working-space images `encode` handles,
  while reusing the same `encode_planar` writer, `resolve_bigtiff`,
  `scan_non_finite` and `channel_means_f32`. The linear-BT.2020 profile is
  `color::hdr_linear_bt2020_icc`, built from `definitions::BT2020` at gamma 1.0
  through the existing dateTime-zeroing path; the orchestrator resolves it and
  passes it in, so the embedded blob is provably the one it chose.
- 2026-08-05: **`definitions::BT2020` is now the fifth lcms2-consumed colour space**,
  and the module note that said it had no runtime consumer was stale — it claimed
  `BT2020` was reached "only from the `#[cfg(test)]` derivation and audit harness".
  Fixed, and the note now carries the enumerated hazard list (`REC709`,
  `DISPLAY_P3`, `ACESCG`, `PROPHOTO`, `BT2020`): editing any of the five changes
  embedded ICC bytes and every lcms2-transformed pixel *even with `pinned.rs`
  untouched and every audit ulp at 0*, and nothing automated catches it. The
  `color` epic summary had already predicted this task would do exactly that.
- 2026-08-05: **Deliberately no `cicpTag` on the linear profile.** ICC would permit
  one (RGB + Display class, verified by `exiftool`), and Chunk B's PQ/HLG profiles
  will carry one — but H.273's `VideoFullRangeFlag` describes a *bounded* code range
  while these samples deliberately run past 1.0 to ≈4.926108, so the claim would
  over-state the encoding while adding nothing the colorants and linear TRC already
  say. Recorded on the builder so it is not "fixed" later.
- 2026-08-05: The **report block is authoritative for the luminance semantics, by
  necessity.** The ICC PCS stops at the media white, so no v4 profile can state that
  `1.0` is 203 cd/m² and highlights reach 1000 cd/m². `report.hdr_linear_tiff`
  carries reference white / peak / headroom / shoulder / tone / gamut ids plus the
  frame's *measured* MaxCLL/MaxFALL, and an `interoperability` string that says
  plainly the profile does not convey them. The task required that the profile never
  be claimed to communicate all HDR semantics; this is that requirement in the
  artifact rather than only in documentation.
- 2026-08-05: **This is the only display profile whose peak phase is the render, not
  the encode**, and that is a property of the model rather than an oversight:
  f32 needs no quantization buffer (like `Convert`'s `OutDepth::F32` arm) and the
  `tiff` writer streams strips straight into the staged `BufWriter` under the
  default `Predictor::None`, so nothing assembles a container in memory the way
  AVIF and the gain-map JPEGs do. Verified by reading `tiff` 0.11.3's
  `write_strip`/`write_data` rather than assuming. **A lossless-compression option
  would reintroduce a staging term** — noted at the model.
- 2026-08-05: `RunProfile::HdrLinearTiff` calibrated on the same two real scans as
  `HdrAvif`. 18.66 MP: accounted 820,917,504 against a measured 906,526,720-byte
  peak RSS (9.5% under, allowance covers it; estimate 1,078,272,857). 74.65 MP:
  accounted 3,284,582,400 against measured 3,578,101,760 (8.2% under; estimate
  3,911,487,488). Clean linear scaling, and **no free constant to tune** — every
  term is an enumerated buffer, so the 8–9.5% gap is unmodelled allocator/writer
  overhead and inventing a term to close it would be fabrication. The render phase
  reproduces by hand: `2·image + 12·px` = 2·298,515,456 + 223,886,592 =
  820,917,504 exactly. Its `--export-ir` term is **4** B/px, not the 2 B/px the AVIF
  and gain-map profiles stage, because this preset resolves `OutDepth::F32`.
- 2026-08-05: Verified on real scans with independent tools. `exiftool`:
  `BitsPerSample 32 32 32`, `SampleFormat Float; Float; Float`,
  `PhotometricInterpretation RGB`, `Compression Uncompressed`,
  `ProfileClass Display Device Profile`, `ColorSpaceData RGB`, D50 media white.
  Its reported colorants match an **independent** Bradford D65→D50 adaptation of the
  BT.2020 primaries to 2.2e-4 — consistent with Little CMS's own adaptation
  rounding and well inside the ±2e-3 band `display_p3_colorants_match_icc_registry_
  reference` already accepts. `sips` opens both files at 32 bits.
- 2026-08-05: **The strongest real-scan evidence**: decoding the produced 18.66 MP
  file back gives `min 0.09329889 max 4.9261084` with **7.92%** of samples above
  reference white and zero non-finite — the maximum is *exactly*
  `hdr::LINEAR_HEADROOM`, so the 1000-nit peak survives the round trip bit-for-bit.
  That is the task's "values between reference white and peak, and values near the
  supported maximum" clause discharged on real data rather than a fixture.
- 2026-08-05: Tests: 12 new unit + 2 integration (603 + 135, from 591 + 133). The
  round-trip test was **mutation-checked** — inserting a `clamp(0.0, 1.0)` in the
  encoder fails exactly `hdr_linear_tiff_round_trips_every_sample_bit_exactly` and
  `hdr_linear_tiff_counts_non_finite_without_laundering_it`, so both are
  falsifiable rather than incidentally green. The `--strict` integration run uses
  the **IR-free** `hdr-48bit.tif`, so "no promotable warning" is a real assertion.
  The drift gate was **checked, not assumed**: `PIPELINE_FINGERPRINTS` and
  `params_hash` are unmoved, since the default preset stays `legacy`.
- 2026-08-05: Two stale things fixed while passing through, both pre-existing:
  the `--output-preset` help still listed `hdr-pq`/`hdr-hlg` as "not accepted yet"
  after `hdr-avif-output` activated them, and design-spec §8's `encoding` identifier
  list omitted the shipped JPEG and AVIF names. Also reworded
  `reject_roll_unsupported`, whose message blamed "non-TIFF containers" — false for
  a TIFF preset. The real rule is the **suffix contract**: a preset that pins a
  required extension is `convert`-only because roll derives frame names itself and
  nothing makes that derivation honour one. `film-master` pins no row, which is
  exactly why it stays roll-capable.
- 2026-08-05: **Observation, deliberately not acted on:** every nc-synthesized
  profile carries Little CMS's default `ProfileDescription: "RGB built-in"`,
  including this one — unhelpful in an application's profile list. Setting a real
  description only here would be inconsistent, and setting it in the shared `synth`
  helper would change the embedded ICC bytes of already-shipped outputs. Left for a
  deliberate decision (a natural `output/presets` or release-readiness item) rather
  than changed silently as a side effect of this task.
- 2026-08-05: Chunk A gates green in CI order: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`.
  **Still open for Chunk B:** the fallback-TRC probe with viewer evidence, u16
  quantization with reported max/RMS error, the `cicpTag` profiles, and the
  truthful-naming decision about 16-bit not being one of BT.2100's specified depths.


## lossless-hdr-tiff (chunk B — hdr-pq-tiff / hdr-hlg-tiff)

**Status:** done
**Updated:** 2026-08-06

- 2026-08-06: **STEP 0 overturned the plan's own default, which is why it existed.**
  Chunk A left the fallback TRC as "probe, and default to the exact PQ inverse
  (÷10,000 nits) if inconclusive". The probe was *not* inconclusive: it found that
  real-world practice does a third thing neither candidate covered, and that the
  planned default is the worst of the three. Measured through Little CMS, Adobe's
  reference `9-16-0-1 BT2100-PQ-Display-Full.icc` maps **203 nits → PCS Y ≈ 1.0 and
  does not clip**, carrying extended range to ~49.6 — i.e. reference-white-relative
  luminance, exactly nc's own linear-domain semantics. Went back to the user rather
  than applying a default whose premise had changed.
- 2026-08-06: Facts about that reference profile worth keeping, since it is the only
  known prior art for HDR TIFF in Photoshop/macOS. It is **Adobe-authored, ICC
  v4.2** (not 4.4 — a `cicpTag` does not require 4.4), Display-class, and
  **LUT-based**: `AToB0`/`AToB1`/`BToA0`/`BToA1` with **no** `redTRC`/matrix-column
  tags at all. Its `mAB` is A curves (`curv`, 512 entries) → CLUT → M curves
  (`para` type 1) → matrix → B curves (`curv`, count 0 = identity), and its matrix
  is the BT.2020 colorants × **0.5** — which is how the PCS encoding factor below
  was found. Its dark end is more accurate than nc's because the 16-bit
  quantization happens in the *perceptual* A-curve domain and the range expansion
  is a continuous parametric M curve; nc's simpler 3-stage form quantizes the linear
  output instead. Recorded as a known, accepted difference, not a defect.
- 2026-08-06: **Decision (user-approved): extended-range A2B, Adobe-compatible.**
  A matrix-shaper profile cannot express it — an ICC `curveType` output is confined
  to `[0, 1]`, so a shaper could only clip at reference white or normalize to
  10,000 nits and render everything at 2% — and the extended-range form is
  simultaneously the *honest* one: a pure scaling of absolute luminance satisfies
  ICC.1:2022 §9.2.17's "equivalent to the data colour space encoding" without
  clipping or inventing anything.
- 2026-08-06: **The safe `lcms2` crate cannot build an A2B profile**, established
  before committing to the approach: it exposes no way to insert a stage into a
  `Pipeline` (only `cat`), and `Profile::handle` is `pub(crate)` so the raw handle
  is unreachable. So `color::synth_coded_hdr` builds the whole profile through
  `lcms2-sys`. The unsafe region is confined to profile *construction* — no pixel
  passes through it, the result is plain ICC bytes the safe API reads back, and
  every handle has an RAII guard so the early-return `fail!` paths cannot leak.
- 2026-08-06: **Two things the probe taught that no amount of reading would have.**
  (1) Little CMS refuses to serialize a 2-stage pipeline: "LUT is not suitable to be
  saved as LutAToB". Its `mAB` writer accepts only recognized patterns, and the
  compact one that fits is **M curves → Matrix → B curves**, so identity B curves
  must be present even though they do nothing. (2) Every luminance came out exactly
  **2× too large** until the matrix was pre-divided by `32768/65535`: ICC PCSXYZ in
  a LUT tag is `s1Fixed15Number`, where `1.0` encodes as `0x8000`. Adobe's matrix
  carries the same halving, which confirmed the reading rather than leaving it a
  guessed fudge factor.
- 2026-08-06: Curve resolution settled by measurement: **1024 entries**, because
  1024 and 4096 give *identical* accuracy (the limit is the 16-bit quantization of
  each stored value, not the table length) and 4096 would cost 18 KB of profile for
  nothing. Measured accuracy of the whole round trip through Little CMS: ≤0.1% above
  20 nits, ≤0.8% above 5 nits, degrading below ~1 nit where the 16-bit step
  (0.153 nits) dominates — an absolute error under 0.08 nits, below any display's
  black level. It affects only how a colour-managed viewer interprets the file; the
  stored code values are untouched.
- 2026-08-06: **HLG's profile is scene-referred, and that is forced, not a
  shortcut.** HLG's OOTF is `R_D = α · Y_S^(γ-1) · R_S` — each channel scaled by a
  function of the *pixel's* scene luminance — so it is not per-channel separable and
  no 1D curve set can represent it. Applying it as a per-channel power anyway is a
  common and wrong shortcut. Adobe ships exactly this split: their HLG **Scene**
  profiles are 1D-plus-matrix like nc's (7.2 KB) while their HLG **Display**
  profiles are ~66 KB because they need a 3D CLUT. nc's PCS is anchored on
  `hdr::hlg_reference_white_signal()` — the signal the renderer actually produces
  for 203 nits (≈0.7499, BT.2100's nominal diffuse white), *computed* rather than
  asserted, so the profile's anchor and the renderer's output cannot drift apart.
- 2026-08-06: `pinned::BT2020_TO_XYZ_D50` added through the documented colorimetry
  workflow, with a new `derive::rgb_to_xyz_adapted` and audit `Source` variant. It is
  the first artifact nc needs *because* it authors a profile itself: every other nc
  profile lets Little CMS derive colorants from pinned primaries, which a
  matrix-shaper can do and an A2B cannot. Held in `f64` (its consumer is an
  `s15Fixed16` matrix stage, not an `f32` pixel loop) and derived with the canonical
  `BRADFORD`. Its independent anchor is the colorant matrix **Little CMS itself
  computed**, read out of chunk A's profile with `exiftool` — a different
  implementation, quantized through ICC and printed by a third tool, agreeing to
  2.2e-4. A second test pins the structural invariant that the columns sum to the
  D50 adopted white, which a colorant check alone would not catch.
- 2026-08-06: Replaced `convert_frame`'s render dispatch if-chain with an
  **exhaustive match on the preset**. This was a real latent bug, not tidying:
  `hdr::transfer_for` now legitimately answers for four presets (PQ and HLG each
  have an AVIF *and* a TIFF preset rendering an identical rendition), so the old
  `else if let Some(transfer) = transfer_for(..)` would have silently handed the new
  TIFF presets to the AVIF encoder. The transfer and the container are independent
  choices; the compiler now enumerates the containers.
- 2026-08-06: Quantization is one pinned `round` (half away from zero) to full-range
  16-bit, and out-of-domain samples are **rejected, not clipped** — the opposite of
  the legacy `encode` path, where clipping is an expected outcome of an unclamped
  render and is *counted*. Here the transfer stage guarantees finite `[0, 1]`, so an
  out-of-domain sample means that stage is broken; the error names the pixel index.
  On a real 18.66 MP scan the measured RMS error is **0.286 codes against the 0.2887
  (`1/√12`) a uniform rounding residual predicts**, and on the 74.65 MP scan it is
  **0.2875** — converging on the theoretical value as the sample count grows, which
  is a strong independent sign the quantizer behaves as theory says. Max is 0.5, its
  structural ceiling. The 74.65 MP file is 448 MB (exactly `px · 3 · 2`,
  uncompressed) and stays ClassicTIFF, as the 4 GiB threshold implies.
- 2026-08-06: **The strongest verification is cross-artifact.** Converting one real
  scan to both `hdr-linear-tiff` and `hdr-pq-tiff` and decoding the PQ codes with an
  *independent* ST 2084 EOTF recovers the linear TIFF's samples to **0.0149% worst
  case over all 55,971,648 samples** (above ~4 nits), with the above-reference-white
  count matching exactly (4,433,118 = 7.92%). That simultaneously confirms the
  quantization, the transfer, and that the two presets really do share one rendition.
- 2026-08-06: Independent metadata verification with `exiftool`: 16-bit unsigned RGB,
  uncompressed, ICC v4.4 Display class, and the `cicp` fields decoded as
  `ColorPrimaries: BT.2020, BT.2100` with `TransferCharacteristics: SMPTE ST 2084,
  ITU BT.2100 PQ` / `BT.2100 HLG, ARIB STD-B67`, `MatrixCoefficients: Identity
  matrix`, `VideoFullRangeFlag: Full`. **macOS `sips` reports both profiles by name**
  ("Rec.ITU-R BT.2100 PQ Full Range (nc)"), so ColorSync parses and accepts an
  extended-range A2B profile nc authored. That is evidence of *parsing*, and it is
  deliberately not written up as evidence of HDR presentation — the visual
  Photoshop/Preview check stays an external manual gate, and the documented
  compatibility is not broadened past it.
- 2026-08-06: Named the new profiles (user-approved scope): the PQ, HLG **and**
  chunk A's linear-BT.2020 profiles carry real `profileDescriptionTag` values instead
  of Little CMS's default `"RGB built-in"`. Deliberately *not* retrofitted to the
  older sRGB/P3/ACEScg/ProPhoto profiles, which would change the embedded bytes of
  already-shipped outputs; that stays a separate decision.
- 2026-08-06: `RunProfile::HdrCodedTiff` calibrated on the same 18.66 MP scan:
  accounted 820,917,504 against a measured 906,346,496-byte peak RSS (PQ) and
  906,330,112 (HLG). Render is still the peak, so the number matches
  `HdrLinearTiff` exactly — the +6 B/px quantize buffer lands in the encode phase,
  which stays below render. Every display profile now shares one render term
  (`2·image + 12·px`), which the tests assert directly so a future divergence is
  visible.
- 2026-08-06: **Truthful naming, in the artifact rather than only the docs.** The
  report's `hdr_coded_tiff.interoperability` string states that 16 bits is TIFF's
  quantization and not one of BT.2100's specified depths (10 and 12), that the
  signalling lives in the ICC `cicpTag` because TIFF has none of its own, that only
  a CICP-aware reader honours it, and that these are limited-interoperability
  interchange rather than display-ready — pointing at the AVIF and gain-map presets
  for delivery. The report's `cicp` triple deliberately writes **0** for
  MatrixCoefficients rather than echoing `HdrRenderMetadata`'s 9, because it
  describes the artifact (an RGB ICC profile) and not the renderer's AVIF contract.
- 2026-08-06: **The manual viewer gate ran, and the result is "valid and correct,
  but not discriminating."** A review set was generated from a real Portra 400 roll
  (`portra400-2026-08-04`, frames 1244/1249) with the *same* recipe across five
  formats — `hdr-pq-tiff`, `hdr-hlg-tiff`, the equivalent `hdr-pq` AVIF as a control
  (macOS presents AVIF PQ as HDR natively), `hdr-linear-tiff`, and the legacy sRGB
  TIFF as an SDR baseline. User verdict: **every TIFF and AVIF renders correctly and
  all of them look good, with little visible difference between them.**
  - What that **does** establish: the files are well-formed, ColorSync accepts the
    hand-authored extended-range A2B profiles, and nothing renders garish, inverted,
    or crushed — the failure modes a broken profile or a wrong PCS scale would show.
  - What it **does not** establish: visible HDR presentation. "No difference from the
    SDR baseline" is equally consistent with the viewer tone-mapping to SDR, so the
    documented compatibility stays exactly where it was — **limited-interoperability
    interchange, not display-ready**. Do not upgrade that claim on this evidence.
  - Two reasons the test was under-powered, both worth fixing before a retest.
    **(a) The scene.** 6.96% of samples sit above reference white with the max at the
    1000-nit peak, so there *is* HDR content — but it is diffuse (sky/highlights),
    not the small specular glints that make eDR obvious. **(b) The rendering.** The
    set was converted with the **default reconstruction, which is still the
    exponential curve** (`DensityCurve::default()`), every print control neutral —
    not the reference-anchored sigmoid. So it exercised the container and profile,
    which was the point, but not the intended product look.
  - A discriminating retest needs a specular-highlight frame *and* an explicit
    sigmoid reconstruction, and should compare **A against C** (PQ TIFF vs PQ AVIF,
    the same rendition in two containers) rather than against the legacy SDR
    baseline, which renders through a different path entirely.
- 2026-08-06: Dmax was **not measurable on that roll**, which is itself a datapoint
  for `film-base/dmax-anchor-reliability`: frame 1229 (the fully-exposed reference)
  is clipped to zero transmission in *all three* channels, so it is denser than the
  scan captured and `estimate --d-max-region` correctly refuses it. The review used
  `--d-max 1.35`, the median of previously measured rolls. Also worth recording
  against that task: the film **base does not transfer between capture sessions** as
  cleanly as the earlier "0.0005 agreement" note implies — this roll's own measured
  base (`0.5122/0.2270/0.1417`, from 1230 with `--grid`) differs from the other
  Portra 400 roll's by **13% on green**, enough to blow highlights when borrowed.
- 2026-08-06: **Code review (two engines) — eight findings, seven fixed, one
  deliberately deferred.** Fixed: the `HdrLinearTiff` memory profile charged 4 B/px
  for an f32 IR-export buffer that is **never allocated** (`export_ir_to_writer`'s
  f32 arms pass the existing slice straight to the writer, which is exactly why
  `Convert`'s f32 arm charges 0) — it over-stated the peak and could reject runs that
  fit, and a test had pinned the wrong number; two `_` catch-all arms inside the
  dispatch whose own comment called it "exhaustive" (the ICC choice now keys off
  `render.metadata().transfer`, the value that actually produced the codes); the
  measured MaxCLL/MaxFALL being dropped from the PQ TIFF report while the AVIF path
  writes them into `clli`; missing RSS calibration rows for both new profiles; and
  four stale doc/count errors. **Deferred with the reason recorded:** two real ICC
  conformance gaps — §8.4.2 requires `BToA0Tag` and §8.2 requires
  `chromaticAdaptationTag` — both verified against the normative text and now
  documented on `synth_coded_hdr` instead of being papered over. Closing them needs
  two more pinned artifacts and **changes the profile bytes**, which would invalidate
  the review set above, so it is a reviewed decision rather than a silent edit.
- 2026-08-06: A claim about peak phase was wrong **twice**, and the second time a
  test caught it. The first version said `hdr-linear-tiff` was the *only*
  render-peaking profile (its coded sibling is too); the correction said every
  non-TIFF profile peaks at encode, which `ultra-hdr-v1` immediately falsified —
  its four simultaneous display buffers make render 72 B/px against encode's 68.
  There is no category: `which_phase_peaks_is_per_profile_and_measured_not_assumed`
  now pins each profile's peak phase so the next author reads it off a test rather
  than a sentence.
- 2026-08-06: **Decision: the two ICC conformance gaps are deferred to
  `output/presets`** (user call). The full closing recipe went into that task file
  rather than being left as a comment — the two pinned artifacts it needs
  (`XYZ_D50_TO_BT2020` and the Bradford D65→D50 matrix), the mirrored `mBA ` stage
  order, and the warning that Little CMS accepts only recognized stage patterns. Two
  reasons it belongs there: closing them **changes the profile bytes**, so it wants
  one re-review, which that task already performs for preset activation; and the
  existing `output/lossless-hdr-tiff --> output/presets` edge already carries it, so
  **no dependency-graph change was needed** (verified in both the diagram and the
  canonical list). `color::synth_coded_hdr` now names `output/presets` as the owner
  instead of describing the work as an undecided question.
- 2026-08-06: **Second review round (Codex + the nc reviewer), eleven findings, all
  addressed.** The one that mattered was a genuine requirement gap neither the first
  round nor I had caught:
  - **P1 — the sidecar did not carry the HDR contract at all.** The task makes the
    *sidecar* authoritative for semantics the ICC provably cannot express, but the
    reference-white / peak / headroom / tone / quantization values were added only to
    `Report`. Any run that discards stdout — `--report none`, which is exactly how a
    batch script calls it, and how this task's own review set was generated — wrote a
    file whose luminance semantics existed nowhere. Fixed by extending the sidecar's
    `meta`. It could **not** be a third sibling key: `SidecarEnvelopeIn` is
    `deny_unknown_fields`, so `{meta, params, output}` would make every new sidecar
    fail to reload through `--params`. `meta` is safe because the read side keeps it
    as an ignored raw `Value`, and the blocks are the *same types* the report
    serializes, so the two cannot drift. Pinned by
    `hdr_tiff_sidecars_carry_the_luminance_contract_and_still_reload`, which also
    asserts the envelope shape is unmoved and that a sidecar replays byte-identically.
  - **The extended-range claim was over-stated, and the correction is worth keeping.**
    The `AToB0`'s *own* output encoding is the same `u1Fixed15` PCS, so an
    **integer** ICC pipeline (including lcms's own `cmsDoTransform` with 16-bit
    formats) clamps at ≈1.99997 — about 406 cd/m² — and flattens every highlight. The
    ≈49.26 the tests measure survives only because lcms evaluates in float *and* the
    identity B curves are parametric. The cap was previously attributed to `BToA0`
    alone; it applies in the shipped direction too, for any 16-bit consumer.
  - The ICC type was misnamed `s1Fixed15Number`; ICC.1:2022 §4.8 defines
    **`u1Fixed15Number`** — unsigned. Read as signed, the maximum would be ≈0.99997
    and the matrix scale would be mis-derived by 2x, so the name mattered.
  - **`--output-preset --help` said `hdr-pq-tiff`/`hdr-hlg-tiff` were not accepted**
    while `parse` accepted them — the primary discovery surface contradicting the
    parser. `OutputPreset`'s own rustdoc still said "Only three variants are accepted
    today" (stale since #78 and extended by this change). Both now say eight, with a
    note that these three places have to move together.
  - Also fixed: the `media_white` literal was `0.82491` where ICC.1:2022 §7 states
    **0.8249** — a small profile-bytes change, made because no reading justifies the
    old value; the design-spec copy of the "only render-peaking preset" claim; the
    `required_extensions` row list omitting the coded presets; a stale
    `#[allow(dead_code)]` on `RenderedHdr::metadata()` that production now calls; the
    IR-export depth comment omitting five shipped presets; and a **vacuous
    assertion** — `message.contains('1')` always passed because the error text
    carries a `1` via "`[0, 1]`", so a missing index would not have failed it.
  - **Deferred to `output/presets`, folded into the `chad` work:**
    `pinned::BT2020_TO_XYZ_D50` adapts to `definitions::D50.to_xyz()`
    (`[0.96429568, 1, 0.82510460]`, D50 from *rounded chromaticities*) rather than to
    the spec's PCS white, so a neutral lands ≈2.4e-4 off the declared media white.
    The matrix must adapt to the spec value *and* share it with the new `chad` tag,
    so re-deriving it belongs in that single profile-bytes change. Recorded there,
    including that the existing lcms-observed anchor test tolerates 2.5e-4 and so does
    not currently catch it.
- 2026-08-06: Tests: chunk B plus both review rounds bring this to **617 unit + 137
  integration** (from 591 + 133 before this task began, and 603 + 135 after
  chunk A).
- 2026-08-06: **Third and fourth review rounds (the `/review-fix-loop` two-engine
  pass, then `/ship`'s reviewers). One substantive correctness defect, found only on
  the fourth pass.** `quantize_coded_u16` scaled in `f32`, which rounds **twice** —
  into the product, then in `round()`. A sample whose exact product sits just under
  a half-code boundary was pushed onto it and rounded away from the nearest code:
  `0.996_498_05_f32 · 65535` evaluates to exactly `65305.5_f32` and stored 65306
  where the exact product is 65305.4995957 and the nearest code is 65305. The `f32`
  residual then *reported* 0.5 while the stored code was really 0.5004 away, so both
  the "at most half a code" claim in the report block and the assertion in our own
  test were false for those inputs. **271 of the 167,772 `f32` values in
  `[0.99, 1.0)` disagree on the nearest code** — concentrated exactly where PQ puts
  highlights. Now scaled and measured in binary64. This **changed stored codes** for
  `hdr-pq-tiff`/`hdr-hlg-tiff` by up to 1 code on ~0.16% of samples;
  `hdr-linear-tiff` is untouched (verbatim f32, no quantization).
- 2026-08-06: **Two test oracles computed in `f32` and would have confirmed the
  defect rather than caught it** — the same blind spot as an lcms-round-trip test
  that shares an implementation with what it checks. Both now compute in binary64.
  The regression test pins the stored code *and* that the reported error does not
  understate the true one, and asserts the `f32` path still reproduces the defect so
  the witness value cannot silently go stale. Verified cross-target-safe: every
  operation involved (f32 multiply, f64 multiply, `round`) is exactly rounded per
  IEEE-754, not transcendental, so the assertions are bit-stable on x86_64 Linux too.
- 2026-08-06: **The "only render-peaking profile" claim needed a third correction**,
  which is worth recording as a pattern rather than an incident. Rounds 2 and 3 fixed
  the module doc, the design-spec, and the test; the fourth found it still live on the
  `HdrLinearTiff` *variant rustdoc* and, worse, inside the test comment meant to be
  the authority ("Every other profile peaks at encode" — false, `UltraHdrV1` peaks at
  render at 72 vs 68 B/px). The lesson encoded in the comments now: what is unique to
  `HdrLinearTiff` is the **absent staging term**, not the peak phase, and the peak
  phase is per-profile and must be read off
  `which_phase_peaks_is_per_profile_and_measured_not_assumed`.
- 2026-08-06: Five smaller round-four fixes: the eagerly-allocated pipeline stages
  mean a null alloc leaks its *siblings*, not just "a failed insert" as the SAFETY
  note claimed; `describe` discarded `MLU::set_text`'s `bool`, so a failure would
  have produced a description-less profile while returning `Ok`; "four TIFF-HDR rows"
  where five were added; `validate_output_preset`'s rule-3 doc still naming only
  `ultra-hdr-v1` for `print.linear_range`; and the `io::encode` module header
  documenting only one of the two HDR entry points. Final: **619 unit + 137
  integration**, all four gates green. All four gates green in CI
  order plus `colorimetry::audit` in check mode and
  `scripts/check-vendored-native.py`. The drift gate is unmoved — the default preset
  is still `legacy`, verified rather than assumed.
- 2026-08-06: **Correction to the second-review entry above: the vacuous assertion
  was *not* fixed then.** That entry lists "a **vacuous assertion** —
  `message.contains('1')`" among the round's fixes. The claim was false: the line
  was still `message.contains('1')` in
  `io::encode`'s `coded_hdr_tiff_rejects_an_out_of_domain_sample_instead_of_clipping`,
  so the property it advertised — that the error names the *offending sample index*,
  the whole reason that path refuses instead of clipping — remained unguarded. A
  third review round caught it; it is asserted on the rendered index now
  (`message.contains("sample 1 is outside")`) and **verified falsifiable**: removing
  `{index}` from `quantize_coded_u16`'s message makes the test fail, which was
  confirmed by doing it and then restoring the message. Logged as a new entry
  because this file's dated history is append-only — the earlier entry stands as
  written, wrong.

  Two things worth carrying forward. First, a fix listed in a progress entry is not
  evidence the fix landed; only the code is. Second, an assertion on an error
  *message* is worth a moment's thought about what else the message contains — the
  `1` this one matched came from the "finite values in [0, 1]" tail, which no amount
  of re-reading the assertion in isolation would reveal.
- 2026-08-06: **Third review round (Codex + the nc reviewer), fifteen findings.**
  Beyond the vacuous assertion above, three were substantive and the rest were
  doc/comment accuracy:
  - **The `describe` helper tagged the linear profile's `desc` record with the null
    locale** while `synth_coded_hdr` wrote `en`/`US` through `cmsMLUsetASCII` — so
    the *same profile* carried `lang=\x00\x00 ctry=\x00\x00` on `desc` and `enUS` on
    the `cprt` Little CMS fills in by default. ICC.1:2022 §10.15 wants an ISO 639-1
    language and ISO 3166-1 country, and a reader that requests a locale without
    falling back to record 0 showed *no* description — the exact defect naming the
    profile was meant to remove. Now `Locale::new("en_US")`, pinned by
    `every_named_profile_tags_its_description_en_us`, which parses the language and
    country bytes **out of the tag table** rather than asking `Profile::info` with a
    locale: querying through lcms with the same null locale round-trips trivially
    and proves nothing. Verified externally on written bytes before and after.
    Profile length is unchanged (only the two locale fields move), so no size or
    `icc_bytes` expectation shifted.
  - **`pq_decode_nits`'s comment misattributed its own guard.** It said `.max(0.0)`
    existed because a code above 1.0 would take a negative base to a fractional
    power; false — above 1.0 the numerator is positive and the clamp does nothing.
    It is ST 2084's own `max(0, …)` protecting the **low** end: `power` drops below
    `C1` for codes under ≈7.31e-7. Above 1.0 the function is simply out of contract,
    and worth stating why: the *denominator* `C2 − C3·power` crosses zero at code
    ≈1.99206, past which it returns NaN, and well before that the values are
    nonsense (code 1.5 → ≈3.1e6 cd/m²). No live bug — the only caller is the ICC
    table builder over `[0, 1]` — but the fn is `pub`, so the comment was actively
    misleading a future caller.
  - **`hdr::transfer_for(HdrLinearTiff) == None` was unpinned**, though it is the
    subtle member of that answer: it *is* an HDR rendition and answers `None` only
    because it applies no transfer, where `Legacy`/`FilmMaster`/`UltraHdrV1` answer
    `None` because they are not HDR renditions at all. Added to the array, so the
    interesting case is no longer the one nothing guards. In the same pass the two
    new atomicity tests gained the **`output.bigtiff`** case they were missing
    (`--bigtiff on` rejected, `--bigtiff auto` accepted as the falsifiable control),
    driven through `merge` so the *flag* spelling is what is pinned.
  - Doc/comment accuracy, all verified against the code: the Epic summary's "only
    render-peaking display profile" claim (`HdrCodedTiff` and `UltraHdrV1` peak at
    render too — read it off
    `which_phase_peaks_is_per_profile_and_measured_not_assumed`, never off a
    category); both suffix-pinning preset counts, which were off by one (**four** at
    chunk A, **six** now); the remaining `s1Fixed15Number` → `u1Fixed15Number` spots,
    each of which paired the wrong name with the *unsigned* ≈1.99997 maximum and so
    was internally contradictory; `color.rs`'s "the one place nc reaches past the
    safe `lcms2` wrapper", which ignored `cli`'s process-global
    `cmsSetLogErrorHandler` — the wider-reaching of the two; design-spec §5's
    identity-only description of the sidecar `meta` (it now carries
    `hdr_linear_tiff` / `hdr_coded_tiff`, and §5 now says *why* they cannot be a
    third sibling key); §9's `--export-ir` depth entry, which listed three cases out
    of eight and said "sidecar" where it meant the IR TIFF; and the missing name for
    the `hdr_coded_tiff` report block.
  - Recorded in `output/presets`, not fixed here: an **open option** for the deferred
    `BToA0` gap — declaring the coded profiles **Input** class would close it with no
    inverse at all (ICC.1:2022 §8.3.2 requires only `AToB0Tag` for an N-component
    LUT-based Input profile, and §9.2.17 permits `cicpTag` for Input as well as
    Display), leaving `chad` as the only requirement. Deliberately logged as
    *unresolved*: every ColorSync acceptance observation was made with a
    Display-class profile, and a class change may be a coarser break than the byte
    change already planned. Also recorded there: `colorimetry/tests.rs`'s
    `bt2020_to_xyz_d50_maps_white_to_the_d50_adopted_white` pins the column sums to
    **1e-12**, so correcting the adaptation target will fail it loudly and it must
    move in that same change — the task file previously noted only the looser
    2.5e-4 lcms anchor, which would not catch it.
- 2026-08-06: Tests after the third round: **618 unit + 137 integration** (the one
  addition is `every_named_profile_tags_its_description_en_us`; the round's other
  work tightened existing assertions rather than adding cases). All four gates green
  in CI order, plus `colorimetry::audit` in check mode and
  `scripts/check-vendored-native.py`. Nothing in the round moved a pinned
  colorimetry artifact, so `derived-artifacts.txt` and the drift gate are unmoved.


## iso-gain-map-metadata (continued)

**Status:** in progress
**Updated:** 2026-08-04

- 2026-08-04: Re-checked the conformance gate and found the blocker is harder
  than the 2026-07-30 entry recorded. **No accessible implementation implements
  the final standard.** The vendored snapshot, upstream `main`, *and* the new
  `v1.5.1` (released 2026-07-30) all still write `urn:iso:std:iso:ts:21496:-1`
  (`lib/src/jpegr.cpp:63`). Worse, `lib/src/gainmapmetadata.cpp:129` carries an
  upstream `TODO` saying "the draft says that this specifies the count of
  channels … Should this be revised?" — an open question about one of the exact
  fields this task must pin. Android's "ISO 21496-1 support" is that same
  implementation, so the spike's platform matrix overstates it as independent
  evidence. Conclusion: the "equivalently authoritative final-standard
  implementation" route has no qualifying candidate.
- 2026-08-04: **Decided to buy the licensed ISO 21496-1:2025 text** (user
  decision, after briefly selecting the Apple-ImageIO-as-pin alternative and
  reversing). Rationale: an implementation oracle can establish the byte layout
  and dual-aware precedence empirically, but cannot establish mandatory-vs-
  optional fields, legal ranges, or whether dual-dialect coexistence is
  permitted at all — two of the four things this task's Design section must pin.
  Apple ImageIO is retained as the independent decoder *oracle* for the
  verification step, used **after** the text rather than instead of it. ISO
  22028-5:2026 flagged for the same purchase since `hdr-avif-output` and
  `lossless-hdr-tiff` hit the same gate. Licence discipline: the repo carries our
  own field table and tests, never quoted normative text and no checked-in PDF —
  the same pattern `pipeline/colorimetry/definitions.rs` uses for standards data.
- 2026-08-04: Resolved an inconsistency inside the task file itself. Its Goal
  sentence permitted "an authoritative final-standard checklist **or**
  independent oracle", while its Design section demanded a checklist or an
  authoritative *implementation*. Those are different bars, and a reader could
  have concluded the task was startable on an oracle alone. The Design section
  now names the licensed text as the pinning source and records why the
  implementation route does not qualify.
- 2026-08-04: Implemented the **standard-independent half** in the new child
  module `src/pipeline/gain_map/iso.rs` — a child so it may destructure
  `GainMapRender`'s private fields, which `gain_map.rs`'s module note already
  reserved for exactly that. It contains **no byte serializer and no JPEG
  placement**, so there is deliberately nothing here that could emit an ISO
  segment before the text lands; that absence *is* the conformance gate, rather
  than a flag someone could flip. Shipped: `Rational`/`UnsignedRational` with a
  continued-fraction approximation following the reference implementation's
  convention (so fields land where existing decoders expect) but with `f64`
  internals and a loud error instead of a silent best-effort result; `project`,
  which takes `log2` where the dialect stores logarithmic units; and
  `encode_iso_gain_map`, which keeps the **three RGB channels** the ISO dialect
  can signal, consuming the canonical ratios directly rather than re-deriving
  them. Eleven tests. No production path touched — the render is byte-identical
  and `pipeline_version` is unchanged.
- 2026-08-04: Field semantics transcribed from the reference implementation, and
  therefore **provisional pending the licensed text**: `gainMapMin`/`Max` store
  `log2` of the content boost as *signed* rationals while our canonical model
  holds *linear* ratios; `gamma` and both headroom fields are *unsigned*; offsets
  are *signed*; `baseHdrHeadroom = log2(hdr_capacity_min)` and
  `alternateHdrHeadroom = log2(hdr_capacity_max)`. The SDR base is at reference
  white by construction, so its headroom is `1.0` linear / `0` log2.
- 2026-08-04: Two findings worth carrying forward. **(a)** Per-channel
  normalization means each channel's own min→0 and max→1, so *equal sample bytes
  can represent different gains* — the chroma lives in the per-channel extrema,
  not the bytes. A first test asserting raw-byte inequality failed for exactly
  this reason; it now reconstructs the way a decoder does. Do not "fix" a future
  byte-equality surprise by collapsing the windows. **(b)** Deinterleave the
  planes once before downsampling: rebuilding one per output pixel makes the
  resample quadratic in frame size (caught pre-review).
- 2026-08-04: Recorded a determinism scope in the module note. Field values pass
  through `log2`, and a 1-ulp transcendental difference can move a
  continued-fraction expansion to a wildly different numerator/denominator pair.
  ISO metadata bytes are therefore per-build/architecture only — consistent with
  the spike's "Determinism and acceptance" scope, and **not** something to pin
  with a checked-in cross-platform hash.
- 2026-08-04: Gates green in CI order: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`
  (555 unit incl. 3 ignored + 130 integration). Remaining before merge: the
  licensed field table, the APP2/MPF placement work below, the Apple-oracle
  reconstruction check, and the deliberately-conflicting dual-dialect precedence
  fixture.
- 2026-08-04: Flagged the real engineering risk for the container half.
  libultrahdr owns assembly (`io::ultra_hdr::package`), so injecting an ISO APP2
  segment shifts **every MPF offset** and perturbs the marker order the shipped
  `ultra-hdr-v1` path already verified. Expect that rewrite, not the field table,
  to be the hard part — the spike already failed once at this exact seam. Owning
  the segment ourselves is still preferred over enabling libultrahdr's draft-URN
  ISO writer, which would put a conformance patch in vendored source and break
  this task's "a correction changes only the ISO serializer" isolation rule.


## iso-gain-map-metadata (licensed text in hand)

**Status:** in progress
**Updated:** 2026-08-04

- 2026-08-04: **Correction to the two entries above, and to the 2026-07-30
  entry.** The user supplied the licensed ISO 21496-1:2025 text the same day.
  Reading it overturns the central claim: **the `ts:` URN is not draft-era.** C.3
  and the C.4.6 segment-layout table both specify
  `urn:iso:std:iso:ts:21496:-1` for the *published first edition* — 27 characters
  plus a null, which is exactly the table's 28-byte length. So libultrahdr's
  identifier is correct, and "still names the draft namespace" (2026-07-30) and
  "no accessible implementation implements the final standard" (earlier today)
  were both wrong on that evidence. Anyone re-deriving this from an
  implementation's URN alone will reach the same wrong conclusion; the length
  arithmetic in `segment_label_matches_the_published_length_and_identifier` is
  the guard.
- 2026-08-04: **The purchase still paid for itself immediately, for a different
  reason: it found a real conformance defect in the reference implementation.**
  The normative structure (C.2.2) has **no common-denominator compact form and no
  `backwardDirection` field** — bits 5..0 of the flags byte are `reserved`, and
  every value is an explicit numerator/denominator pair. `libultrahdr` sets flag
  bit 3 and emits a shortened layout whenever all denominators match, and writes a
  direction bit with no home in the structure. nc's uniform `1/64` offsets and
  `gamma = 1` make all denominators match, so that compact path is the *common*
  case for us — reusing the reference serializer would have written a
  non-conformant payload on nearly every file. This is the concrete
  justification for owning the serializer, and it was not knowable from the
  implementation.
- 2026-08-04: C.2.3 also resolves the upstream `channelCount` TODO that this task
  flagged as an open semantic question: `is_multichannel` describes the
  **per-channel metadata** count, and the standard states outright that it may
  differ from the gain map's actual channel count (5.2.5.1 likewise). No
  ambiguity remained to carry forward.
- 2026-08-04: **Two of the four things the task set out to pin are not in this
  standard at all**, and the task file previously assumed they would be. ISO
  21496-1 is silent on Google's XMP dialect, so the "legal mapping between ISO and
  Ultra HDR v1 metadata" cannot be derived from it — that mapping is ours to
  define. And "a dual-aware decoder must prefer ISO metadata" traces to Android
  guidance, not normative text, so it is decoder behaviour to measure, never an
  ISO conformance claim. Both recorded in the task file.
- 2026-08-04: Implemented the payload serializer against the text.
  `serialize_metadata` writes C.2.2 big-endian with the reserved bits clear and
  no compact form; `serialize_version` writes the 4-byte `GainMapVersion` that
  C.4.3 requires in the *baseline* image; `app2_segment` wraps either payload per
  the C.4.6 table, whose length counts itself and excludes the marker.
  `validate_fields` enforces the standard's stated constraints —
  `writer_version >= minimum_version`, `H_alternate != H_baseline` (5.2.7,
  compared as *values* since `0/1` and `0/2` denote one headroom),
  `max(G) >= min(G)` (5.2.5.3), non-zero denominators, and a non-zero gamma
  numerator. 24 tests in the module, 561 unit + 130 integration overall, all four
  gates green.
- 2026-08-04: The strongest new test is
  `gain_matches_the_standards_application_formula_round_trip`: it recovers the HDR
  rendition from the SDR base and the canonical gain via Clause 6.3's
  `Alternate = (Baseline + k_base) * 2^(W*G) - k_alt`. That independently confirms
  nc's linear-ratio canonical model and the standard's log2 `G` are the same
  thing, which is the agreement the dual-dialect requirement actually rests on.
  It also confirms the spike's reference-white-relative common domain is the
  standard's "gain map application space" (3.4, B.2) — scaled so reference white
  is 1.0, exactly as pinned.
- 2026-08-04: Still open before merge: APP2 insertion into both images plus the
  MPF offset repair, the C.4.3 Exif-vs-JFIF baseline question, the co-sited
  resampling decision, the Apple-oracle reconstruction check, and the
  deliberately-conflicting precedence fixture. All recorded in the task file.


## iso-gain-map-metadata (container half)

**Status:** in progress
**Updated:** 2026-08-04

- 2026-08-04: **The MPF repair I flagged as "the hard part" is much smaller than
  predicted, and a throwaway probe is what established that.** Probing
  `package()` with APP2 segments pre-inserted into both input JPEGs showed
  libultrahdr **rewrites the baseline image's marker segments** — dropping our
  unknown APP2 entirely, and emitting SOI · APP0 JFIF · APP1 XMP · APP2 ICC ·
  APP2 MPF — while **appending the gain-map image verbatim**, so a segment
  inserted there survives untouched. Do not reason about this from the source;
  the probe is cheap and the behaviour is asymmetric in a way that is easy to get
  backwards (I had it backwards).
- 2026-08-04: Consequence, and the key placement decision: MPF individual-image
  offsets are measured from the byte **after** the `MPF\0` label (verified:
  gain map at 2310 = TIFF start 2190 + stored offset 120). So inserting the
  baseline's segment **immediately before the MPF segment** moves the reference
  point and the appended gain map by the same amount and leaves *every stored
  offset correct* — only the first image's recorded size grows. That is one `u32`
  to patch, not an MPF rewrite. Inserting *after* MPF would invalidate every
  offset; `insert_baseline_iso_segment` documents this and
  `baseline_insertion_keeps_every_mpf_offset_resolvable` fails if the placement
  ever moves.
- 2026-08-04: Implemented `Dialects::{LegacyUltraHdrV1, LegacyPlusIso}` and
  `encode_with`. The gain map's full `GainMapMetadata` segment goes in at encode
  time via `jpeg_encoder::add_app_segment`; the baseline's 4-byte
  `GainMapVersion` segment (C.4.3 requires version-only there, not the full
  structure) is spliced in after packaging. `encode` keeps its signature and
  delegates, so the shipped `ultra-hdr-v1` path is untouched.
- 2026-08-04: **A dual-dialect file necessarily shares the achromatic luminance
  gain map**, not the RGB one. Legacy XMP cannot signal a multichannel map, and
  the task forbids generating a second map, so the shared image is the legacy
  form and the ISO fields are projected from *that map's own encoded metadata*.
  That is what makes the two dialects agree by construction rather than by
  coincidence. `encode_iso_gain_map` (RGB) therefore has no caller in this path;
  it is kept because 4.3 states the component count *should* match the baseline
  for maximum accuracy, so an ISO-only output is the standard-preferred form and
  the grayscale map is the legacy compromise.
- 2026-08-04: `is_multichannel` stays `true` (3 metadata channels) even for the
  achromatic map. C.2.3 explicitly permits the metadata count to differ from the
  map's, and always writing 3 keeps the payload size independent of image
  content. Deriving it from whether the channels happen to be identical would
  make the byte length data-dependent for no benefit.
- 2026-08-04: **No CLI surface added, deliberately.** `output/presets` owns the
  neutral `gain-map-hdr` name and default activation, and `ultra-hdr-v1` is
  contractually ISO-free — `tests/pipeline.rs` asserts its bytes contain no
  "21496". Inventing a preset name here would hand `output/presets` a migration
  instead of a capability, so `LegacyPlusIso` carries a documented dead-code
  allowance naming that task as the consumer.
- 2026-08-04: Six container tests: both segments present in the correct images
  with the correct (differing) payloads; MPF offsets unmoved with the baseline
  size grown by exactly the inserted bytes and the gain map still resolving to a
  real SOI whose recorded size reaches the file end; libultrahdr still probes the
  dual-dialect package; JFIF stays first and the ISO segment precedes MPF (the
  ordering that already cost this epic an ImageIO decode once); and malformed or
  MPF-less input fails loudly rather than corrupting a file. All four gates green:
  566 unit + 130 integration.
- 2026-08-04: **Code complete for this task, with one item blocked on an
  unavailable standard** (below). Closed out in this pass:
  - `encode_with` covered end-to-end on a real render through the production
    stages, asserting two ISO segments in the dual file, **zero** in the legacy
    one, the legacy XMP and Display P3 ICC in both, and the size difference.
  - **Resampling phase decided, not inherited: staying centre-aligned.** 6.2.2
    NOTE 1 prefers co-sited (H.265 ChromaLoc type 2), but the NOTE is informative,
    and switching would change the already-shipped `ultra-hdr-v1` bytes for no
    measured gain. Recorded on `resample_axis` with the condition for revisiting;
    both dialects share that function, so they cannot diverge.
  - **Conflicting-dialect fixture built** (`conflicting_dialect_fixture_really_
    disagrees`): legacy XMP says `log2(4) = 2`, the ISO payload says `log2(8) = 3`,
    both asserted present and in conflict. Precedence *selection* is deliberately
    not asserted — libultrahdr reads only the legacy dialect and the standard is
    silent on coexistence, so selection is external-decoder behaviour. The value
    here is proving the fixture is not vacuous.
  - **Exif tripwire** (`baseline_carries_no_exif_colorspace_claim`): C.4.4 branches
    on Exif ColorSpace, and a value of 1 *forces* an sRGB reading that would
    misidentify our Display P3 base. With no Exif, branch two applies and the ICC
    governs. The test fails if Exif ever appears, so whoever adds it must choose
    Uncalibrated.
  - **`iso_sample_for_external_decoder`** (`#[ignore]`, honours
    `NC_ISO_SAMPLE_DIR`) emits a dual-dialect file for the manual oracle gate,
    since there is deliberately no CLI path to produce one.
- 2026-08-04: **Independent verification with exiftool 13.55 and macOS `sips`.**
  exiftool resolved the MPF index and *extracted* MPImage2's 1186 bytes:
  `Number Of Images 2`, MPImage1 `Baseline MP Primary Image` length 2350 start 0,
  MPImage2 length 1186 start 2350, and 2350 + 1186 = 3536 = the file size exactly.
  That is a third-party reader confirming the patched baseline size and the
  untouched relative offsets agree. JFIF 1.02, `hdrgm:Version 1.0`,
  GContainer `Primary, GainMap`, and the Display P3 ICC (rXYZ
  0.51512/0.2412/-0.00105, matching the registry values `display-p3-output`
  recorded) all present; `sips` opens it. The single `[minor] XMP is missing
  xpacket wrapper` warning is **byte-identical on the shipped legacy preset**, so
  it is pre-existing libultrahdr behaviour, not introduced here.
- 2026-08-04: **Blocked, and not worked around: C.4.3's CIPA DC-007 baseline
  requirement.** C.4.3 requires a DC-007-compliant baseline image and its NOTE
  explains that means Exif-compliant; we write JFIF and no Exif. What ISO 21496-1
  alone settles is that our *colour space* signalling is unambiguous (C.4.4 branch
  two: no Exif + ICC present ⇒ the ICC governs). What it cannot settle is DC-007's
  own baseline requirements. DC-007 and DC-008 are **free** from CIPA
  (`cipa.jp/e/std/std-sec.html`, DC-007-Translation-2025 and
  DC-008-Translation-2026) but sit behind a JavaScript/POST disclaimer gate that
  resisted scripted download — trivial to fetch in a browser. I deliberately did
  **not** synthesise an Exif block against an unavailable standard: a partial one
  claiming compliance we cannot verify is worse than a documented gap, and if it
  used `ColorSpace = 1` it would actively mis-signal the P3 base.
- 2026-08-04: Also still external, by nature: the Apple/Android decoder oracle
  (an ISO-aware decoder reconstructing the HDR rendition, and observing which
  dialect a dual-aware decoder selects). `iso_sample_for_external_decoder`
  produces the file that gate needs. Final gates green: 568 unit (1 new ignored)
  + 130 integration.
## ultrahdr-dependency-externalization (continued)

**Status:** not started
**Updated:** 2026-08-04

- 2026-08-04: Checked this task's trigger while working the ISO gate; it is
  **not yet met**, though half of it now is. Upstream libultrahdr released
  `v1.5.0` and `v1.5.1` on 2026-07-30, and `v1.5.0` **does contain** our pinned
  marker fix (`git compare` against `11ac0c32…`: 0 behind, 4 ahead). But this
  task's trigger is an exact published **Cargo** release, and crates.io
  `ultrahdr-sys` is still at **0.1.5 (2026-04-29)** — predating both the fix and
  those releases. So the snapshot, its force-tracked files, and
  `scripts/check-vendored-native.py` all stay until `ultrahdr-sys` publishes a
  version wrapping ≥ `v1.5.0` with a network-free static build. Re-check
  crates.io rather than upstream tags when revisiting.


## ultrahdr-dependency-externalization (re-scoped)

**Status:** not started
**Updated:** 2026-08-05

- 2026-08-05: **Superseding the preceding entry's trigger.** "Wait for a published
  crate wrapping ≥ v1.5.0" is not a sufficient condition and never was; inspecting
  the published archive (which the task's own How-to-Verify asked for) found a
  second, structural blocker no version bump can fix. Task **re-scoped** from
  "externalize the snapshot to a published crate" to **"remove the native
  dependency from the tree entirely."** The task **id is deliberately unchanged**
  — eight references depend on it, including `scripts/check-vendored-native.py:39`
  and this log's own append-only headings — so only the human-readable title moved.
- 2026-08-05: Evidence against the published crate. It is a third-party wrapper
  (`Enter-tainer/libultrahdr-rs`), **not Google's** — correcting an impression an
  earlier entry left. Still 0.1.5 (2026-04-29). Its bundled `jpegr.cpp` has no
  APP0 extraction (`grep -c "Extract APP0"` → 0), so adopting it would reintroduce
  the exact ordering that made ImageIO reject our files. The structural problem:
  libultrahdr's CMake takes libjpeg-turbo from
  `ExternalProject_Add(GIT_REPOSITORY … GIT_TAG 3.1.0)`. With the crate's
  `vendored` feature that is a build-time clone at a **mutable tag**; without it,
  `cargo:rustc-link-lib=jpeg` links a **machine-installed** library. First breaks
  pinning, second breaks the self-contained binary and makes output vary per user
  machine. The `GIT_TAG` sits inside the crate's own bundled CMake and
  `ExternalProject_Add` has no cache-variable override for it (unlike
  `FetchContent`'s `FETCHCONTENT_SOURCE_DIR_*`), so it cannot be pinned without
  forking — i.e. a local copy again.
- 2026-08-05: **What our snapshot actually is**, verified rather than assumed —
  worth recording because the obvious guess is wrong in both directions.
  `libultrahdr/lib/src/jpegr.cpp` is **verbatim upstream `11ac0c3`** (empty diff);
  I suspected local patches from its comment style and was wrong. But
  `libultrahdr/CMakeLists.txt` **is** modified: both libjpeg-turbo
  `GIT_REPOSITORY`/`GIT_TAG 3.1.0` blocks became `DOWNLOAD_COMMAND ""` so the build
  consumes the in-tree `third_party/turbojpeg` (pinned `20ade4de`). That two-line
  edit *is* the offline build, and it is exactly what no published crate provides.
  `patches/libultrahdr-no-threads.patch` is applied at build time on top.
- 2026-08-05: **The size motive does not survive measurement.** Whole-repo pack is
  **14.36 MiB**; vendor is 782 tracked files / 18 MB working tree. "Reduce
  repository size" was chasing a non-problem, which caps what the task should be
  willing to pay. The genuine cost is the maintenance apparatus: the
  force-tracking guard (needed because the copied upstream `.gitignore` hides
  legitimate files) and `check-vendored-native.py`.
- 2026-08-05: **Which readiness conditions are load-bearing**, after challenging
  both. *Static linkage: keep.* Linking a system libjpeg would lose the
  self-contained binary (a design-spec choice, and the same reason the HDR spike
  rejected HEIC/x265) and would make output vary by **user machine** — a bigger
  determinism hole than a build-time fetch, and one no test can see. Checking in
  prebuilt `.a`/binaries is worse on every axis: one artifact per target, larger
  than the source it replaces, unauditable, and only `aarch64-apple-darwin` is
  installed here so the Linux artifacts could be neither built nor verified
  locally. *No-network: too strict as written.* The property worth protecting is
  **pinning to an immutable revision**, not bundling; a SHA-pinned fetch would be
  fine. Relaxing it still does not unblock this crate, for the reason above.
- 2026-08-05: **The chosen route, and why the oracle is not kept as a
  dev-dependency.** Only **6** native calls are on the shipping path
  (`uhdr_create_encoder`, `uhdr_enc_set_compressed_image`,
  `uhdr_enc_set_gainmap_image`, `uhdr_encode`, `uhdr_get_encoded_stream`,
  `uhdr_release_encoder`) and they only write XMP + MPF around two JPEGs nc
  already encodes in pure Rust. **29 of the module's 46 `uhdr::` references are
  tests** — the decode-and-verify oracle. Keeping that as a dev-dependency was
  the first proposal and it was wrong: `cargo test` builds dev-dependencies, so
  CI would still need cmake/clang/nasm and the libjpeg fetch, and the dependency
  would have *moved rather than gone*. Its value is also narrower than it appears
  — libultrahdr reads only the **legacy** dialect, so it was never an ISO oracle,
  and the manual Apple/Android gate answers the same question with real consumer
  decoders. Replaced by captured goldens recorded **while the dependency is still
  present**, plus exiftool structural validation and the documented external gate.
- 2026-08-05: Two consequences to plan for. Assembling the container ourselves
  **retires `insert_baseline_iso_segment`** — with placement under our control both
  ISO segments go in directly instead of being spliced in after packaging — and it
  removes the marker-order bug class, since ordering becomes ours to state rather
  than inherited from libultrahdr's APP0-extraction fix. It also **changes the
  shipped `ultra-hdr-v1` bytes**, because our XMP will not serialize
  byte-identically. That preset is non-default and the gain map is not in
  `version::PIPELINE_FINGERPRINTS`, so no `pipeline_version` boundary is involved,
  but its determinism/golden assertions must be **re-captured deliberately, never
  adjusted until they pass**.
- 2026-08-05: The published-crate route stays recorded but unpursued, with real
  trigger conditions (contains `11ac0c3` **and** obtains libjpeg-turbo without a
  mutable-tag fetch or system library). Watching crates.io for a version bump is
  explicitly **not** the trigger. Our delta is small enough to upstream if anyone
  wants to try, but merge and release cadence would not be ours.
## hdr-avif-windows-packaging

**Status:** not started
**Updated:** 2026-08-05

- 2026-08-05: Filed by `output/hdr-avif-output`, which shipped the AVIF encoder
  gated on macOS and Linux only. CI's matrix is `[ubuntu-latest, macos-15]` with no
  Windows runner, so the task's three-platform clause had no coverage and claiming
  it would have been false. This task adds a `windows-latest` job and proves the
  static libaom build under MSVC; no encoding behaviour changes. Note the contract
  it must *not* over-claim: byte identity is scoped per build/architecture
  (design-spec §8), so the Windows binary is not expected to reproduce the
  macOS/Linux bytes — only the semantic metadata and the pinned decoded-pixel
  bounds. If MSVC cannot build the vendored libaom source unpatched, prefer
  documenting Windows as unsupported over carrying a local patch; the repo already
  has one regretted native snapshot.
## iso-gain-map-metadata (decoder oracle — a real defect)

**Status:** in progress
**Updated:** 2026-08-06

- 2026-08-06: **The external decoder oracle ran, and it found a shipping bug the
  entire in-repo suite could not.** The oracle is Apple ImageIO on macOS 26.5
  (`kCGImageAuxiliaryDataTypeISOGainMap`, available since macOS 15.0) — an
  independent ISO 21496-1 implementation, so no device was needed after all.
  Harness: `scripts/`-free, a ~100-line Swift program in the scratchpad plus the
  new `iso_oracle_samples` ignored test.
- 2026-08-06: **The finding: nc's baseline ISO segment was placed where no JPEG
  reader looks.** `insert_baseline_iso_segment` inserted the C.4.3 version-only
  segment "immediately before the MPF segment" — which satisfies every MPF
  invariant and is exactly what the 2026-08-04 entry above reasoned out. But
  libultrahdr emits MPF **after `SOF0` and the tables**, and an `APPn` scan stops
  at the frame header. The bytes were well-formed, correctly sized, MPF-safe —
  and never parsed. ImageIO reported *no gain map at all* and decoded plain SDR.
  The marker order it was written into:
  `SOI · APP0 JFIF · APP1 XMP · APP2 ICC · **SOF0** · APP2 ISO · APP2 MPF · SOS`.
- 2026-08-06: **Isolated by bisection on the bytes, not by reading source.** Three
  hypotheses were tested by patching a produced file and re-running the oracle:
  clearing `use_base_colour_space`, collapsing 3 metadata channels to 1 (with the
  MPF second-image size repaired), and both — **all three still ABSENT**. Moving
  the *unmodified* segment into the header block made it PRESENT immediately. Four
  positions then all worked (after SOI, after JFIF, after XMP, after ICC), which
  pins the boundary as `SOF0` and nothing else.
- 2026-08-06: **`is_multichannel = true` is vindicated, and that was the surprise.**
  The 2026-08-04 decision to always write 3 metadata channels over an achromatic
  map (C.2.3 permits the counts to differ) was the leading suspect. ImageIO parses
  all three and reports them individually. Do not "fix" it.
- 2026-08-06: Fix is `leading_app_segment_end(packaged)?.min(mpf_start)` — the end
  of the leading `APPn` run, clamped to the MPF start in case a future libultrahdr
  emits MPF earlier. That satisfies **both** constraints at once and keeps JFIF
  first. Pinned by `baseline_iso_segment_precedes_the_frame_header`, which was
  confirmed falsifiable (restoring the old insertion point fails it with the marker
  sequence printed). `marker_sequence` gained an `SOF` arm, since ordering against
  MPF alone provably cannot catch this — both markers sat on the wrong side of
  `SOF0` together.
- 2026-08-06: **Verification results on a real 4715x3297 Ektar frame** (dual-dialect
  file, post-fix). ImageIO's independent parse against what nc wrote:

  | field | nc wrote | ImageIO read |
  |---|---|---|
  | GainMapMax (log2) | 9187889/8388608 = 1.09528 | 1.095282 |
  | GainMapMin (log2) | -15127609/268435456 = -0.0563547 | -0.056355 |
  | Gamma | 1/1 | 1.000000 |
  | Base/AlternateOffset | 1/64 | 0.015625 |
  | AlternateHeadroom | log2(1000/203) | 2.300448 |
  | channels | 3 | 3, reported separately |

  HDR reconstruction headroom **4.9261084** = 1000/203, from both
  `CGImageSourceCreateImageAtIndex(kCGImageSourceDecodeToHDR)` and
  `CIImage(.expandToHDR)`. The gain map resolves at 2358x1649 — the ceil-halved
  2x downsample. This closes the "both dialects express the same semantics"
  bullet with an *external* reader rather than by construction.
- 2026-08-06: **Dual-aware precedence is now observed, not assumed: ISO wins.** On
  the conflicting file (legacy XMP `GainMapMax` 1.09528, ISO 2.095282 — one stop
  apart by construction) ImageIO reports **2.095282**. Recorded as *observed Apple
  decoder behaviour*; ISO 21496-1 is silent on coexistence, so this still must
  never be stated as a conformance property.
- 2026-08-06: **Product finding, and it raises the stakes on `output/presets`: the
  shipped `ultra-hdr-v1` preset is not HDR on Apple platforms.** The legacy-only
  file is ABSENT for *both* `kCGImageAuxiliaryDataTypeISOGainMap` and
  `kCGImageAuxiliaryDataTypeHDRGainMap`, and decodes at headroom 1.0. Apple
  ignores Google's Ultra HDR v1 XMP entirely; only the ISO dialect makes the file
  HDR there. The ISO work is therefore not a conformance nicety — it is the only
  thing that makes nc's gain-map output function on macOS/iOS.
- 2026-08-06: Oracle validated against a **known-good control** before any
  conclusion was drawn — `ultrahdr_app` v1.4.0 (homebrew) encoding a synthetic
  rgba1010102 gradient. ImageIO reads that file's ISO metadata and reconstructs at
  4.926 headroom, so ABSENT on nc's files was nc's problem, not the harness's.
  Conversely libultrahdr **accepts all three nc files** (exit 0) where a plain JPEG
  control fails with "does not contain gainmap image" — the legacy dialect and the
  container were always fine. Note the control's payload is 61 bytes (1 channel)
  against nc's 141 (3 channels); both decode to the same layout formula
  `4 + 1 + 16 + 40·channels`, which is independent evidence nc's C.2.2 field order
  is right.
- 2026-08-06: `iso_oracle_samples` (ignored) emits the three-file set the gate
  needs — legacy-only, dual, and conflicting — from one render, so any difference
  is attributable to metadata alone. It renders a **real scan** when
  `NC_ISO_SAMPLE_INPUT`/`_BASE`/`_DMAX`/`_EV` are set. That is not optional
  polish: the toy fixture *and* the default exponential render both produce a flat
  gain map (`GainMapMax` 0.0039 log2 = 1.003x), which cannot discriminate an HDR
  reconstruction. See the separate finding below.
- 2026-08-06: **Separate, larger finding — nc's default render produces no HDR.**
  On a real Ektar frame at defaults the gain map is inert: `GainMapMax` 0.00392
  log2 = **1.0027x**, while the metadata advertises `HDRCapacityMax` 2.30045. The
  file claims 2.3 stops and delivers 0.004. `--highlight-compress` does not move it
  (identical to 6 significant figures at 0, 1 and 4 — the flag *does* resolve, so
  this is content, not plumbing): the exponential curve anchors display white at
  `Dmax` and real content lands far below the SDR shoulder knee, `clipped_high: 0`
  with mean 0.258. Only `--print-exposure +3` pushes content over the knee
  (`GainMapMax` 1.095 log2 = 2.14x), which is what the oracle files use. This is
  the same non-discriminating condition `lossless-hdr-tiff`'s 2026-08-06 viewer
  gate hit and attributed to "diffuse-highlight scene, exponential default curve" —
  it is now measured, and it is the *default*, not the scene. **Belongs to
  `output/presets` / the sigmoid default work, not here**; filed as a note rather
  than fixed, because changing what the default render puts in the gain map is a
  pixel decision this task has no mandate to make.
- 2026-08-06: Shipped `ultra-hdr-v1` output is **byte-identical** across the fix
  (sha256 `67911f22…5540` before and after on the real Ektar frame) — the changed
  path runs only when ISO fields are present. All four gates green: 620 unit + 137
  integration.
- 2026-08-06: **Still open**, unchanged by this pass: C.4.3's CIPA DC-007 baseline
  requirement (the document is still unfetched), and CLI activation, which
  `output/presets` owns. The Android half of the "Android 15+ and target Apple
  software" bullet is also still unrun — Apple is now covered.

## iso-gain-map-metadata (CIPA DC-007 read — verdict)

**Status:** in progress
**Updated:** 2026-08-06

- 2026-08-06: **The CIPA documents are no longer blocked.** The "JavaScript/POST
  disclaimer gate that resisted scripted download" is just an undocumented POST
  contract: `std/js/dll.js` copies the page's query string into a hidden
  `dlltarget` field and posts it to `std/documents/dll.cgi`. So
  `curl -X POST .../dll.cgi --data-urlencode dlltarget=CIPA_DC-007-2025_E` returns
  the PDF directly; no browser needed. Same for `CIPA_DC-008-2026-E`. Both are
  free downloads gated only on accepting a no-warranty disclaimer. **Do not commit
  either PDF** — they were read from the scratchpad and only restated here.
- 2026-08-06: **First correction: DC-007 is Multi-Picture Format, not Exif.**
  DC-008 is Exif. Earlier entries here said "DC-007 ⇒ Exif-compliant", which is
  right only transitively — DC-007 §4.2.1 says a Baseline MP File *uses the
  compressed image file format of the Exif standard*, and §5.1 places the MP
  Extensions APP2 "immediately after the Exif Attributes in the APP1 marker
  segment". So the Exif requirement reaches us through MPF, and both documents are
  in scope.
- 2026-08-06: **DC-007-2025 explicitly anticipates us.** §4.2.1 names "gain map
  images specified by ISO 21496-1" as a recordable Dependent Image, and Table 4
  assigns them **MP Type Code `050000`**, with details to "follow the provisions of
  Annex C of ISO 21496-1". This is the interop contract nc is actually writing
  against, and it is far more on-point than the C.4.3 NOTE suggested.
- 2026-08-06: **Concrete non-conformance found, and it is small: nc's gain map is
  typed `Undefined`.** Measured on the dual-dialect file — MPImage1 is `030000`
  (Baseline MP Primary Image, correct) but MPImage2 is `000000`, which Table 4
  marks **× = "shall not be used"** in a Baseline MP file (.JPG). The correct value
  is `050000`. **This comes from libultrahdr, not from nc's code** — its own
  reference output (`ultrahdr_app` v1.4.0) writes `000000` too, unsurprisingly,
  since the gain-map type code postdates it. nc ships the file, so it is nc's gap.
  The repair is a 4-byte field in the MPEntry array, in the same post-packaging
  patch that already fixes the first image's recorded size. Note it would change
  the shipped `ultra-hdr-v1` bytes, which is why it is **not** being folded into
  the oracle fix.
- 2026-08-06: **Second gap, larger: the baseline is JFIF with no Exif APP1.** nc's
  header is `APP0 JFIF · APP1 XMP · APP2 ICC · APP2 ISO · APP2 MPF`, so the MP
  Extensions do not follow Exif Attributes as §5.1 specifies. Weighing how hard
  this binds: §7's tag-level requirement is "**should** be followed" for
  non-thumbnail Individual Images, and its tables are pinned to Exif 2.32 / DCF 2.0
  — so the *tag* obligations are recommendations. The structural statements in
  §4.2.1/§5.1 are the stronger ones. Adding Exif also changes the baseline's marker
  layout, which is precisely the class of change that just cost this task a
  silently inert feature, so it must be re-run against the ImageIO oracle rather
  than trusted to `cargo test`. Whether libultrahdr's `package()` preserves,
  rewrites or drops an APP1 Exif is **unknown and must be established by probe**
  (it has an `-x` Exif-insertion flag, so the native API may be the right route);
  the 2026-08-04 asymmetry lesson applies — do not reason it out from source.
- 2026-08-06: **Verdict: both items move to a follow-up task**
  (`output/mp-container-conformance`). Neither is required for the ISO metadata to
  function — Apple ImageIO reconstructs HDR from nc's file today with the type code
  `Undefined` and no Exif present — so they are conformance-claim work, not
  functional work, and they change shipped container bytes. Keeping them here would
  hold `output/presets` behind a container change that has nothing to do with the
  metadata this task owns. `baseline_carries_no_exif_colorspace_claim` stays as the
  tripwire; whoever adds Exif must still choose `Uncalibrated`, never `1`.

## mp-container-conformance

**Status:** not started
**Updated:** 2026-08-06

- Goal: make the gain-map JPEG a conformant CIPA DC-007 Baseline MP File — type
  the gain map `050000` instead of `Undefined`, and settle the Exif-baseline
  requirement (or narrow the claim, with the reason cited).
- Filed 2026-08-06 out of `iso-gain-map-metadata`'s DC-007 read; the findings and
  the reasoning behind the split are in that task's
  `## iso-gain-map-metadata (CIPA DC-007 read — verdict)` section above, and the
  approach is in [the task file](../tasks/output/mp-container-conformance.md).
- The one thing to carry forward before touching anything: a marker-layout change
  is exactly what silently disabled the ISO metadata (see the oracle section
  above), so **re-run the ImageIO decoder oracle** — the Rust suite provably
  cannot catch a placement regression on its own.
- 2026-08-06 (review pass on the oracle branch): that instruction is now
  followable — the Swift harness moved out of the scratchpad into
  `scripts/iso-decoder-oracle/` (macOS-only, not in CI, with a README covering
  build, sample generation, why `_EV` is required, and how to read `PRESENT` +
  headroom). Two supporting changes: `io::ultra_hdr` gained a container seam
  (`compress_images` + `package_images`) so the oracle's three files and the
  product go through **one** assembly path — a future Exif/MPEntry change cannot
  leave the oracle measuring a container nc no longer ships — and the three files
  now come from one render rather than three (`encode_with`'s output is
  byte-identical across the refactor, checked by sha256). Also recorded here
  because review surfaced it: in the **gain-map** image libultrahdr prepends its
  XMP, so `APP1` precedes `APP0 JFIF` — JFIF is not first in the dependent image.
  Pre-existing, harmless so far, filed as a third item on
  `output/mp-container-conformance`.
- 2026-08-06 (second review round): the seam went one level deeper —
  `io::ultra_hdr::assemble` now returns the container bytes and `package_images`
  is a thin `stage_bytes` wrapper, because the `dual_dialect_package` **test
  fixture** was still a second assembly path, and it is the fixture behind all
  four marker-order tests. Left as it was, an Exif APP1 added to the product
  would have moved the shipped layout while those tests stayed green — the same
  shape of hole this branch exists to close. The fixture now calls `assemble`;
  its bytes are unchanged (checked by a temporary equality test against the old
  inline construction), and
  `baseline_iso_segment_precedes_the_frame_header` was re-confirmed falsifiable
  through the new path by restoring the defective insertion point. Also retired
  `iso_sample_for_external_decoder`: `iso_oracle_samples` produces the same
  dual-dialect file (sha256 `8039f2ad…9216`, identical) plus the other two.
  Shipped `ultra-hdr-v1` still `67911f22…5540` on the Ektar frame.
- 2026-08-06 (third review round): **correction to how the oracle's result was
  reported above — the 4.926 headroom figure is not evidence of reconstruction.**
  ImageIO's `HDR decode: headroom 4.9261084` is `2^AlternateHeadroom`, i.e. nc's
  own declared `1000/203` policy constant parsed back out of the metadata.
  Measured both ways on 2026-08-06: the toy fixture with no `_EV`, whose gain map
  is flat (`GainMapMax = 0.000000` on all three channels), reports the *same*
  4.9261084 as the real frame at `+3 EV` (`GainMapMax = 1.095282`). So a pass
  condition of "PRESENT + headroom above 1.0" cannot fail on any file nc
  produces. What the oracle did establish is unchanged and still load-bearing:
  the **ABSENT → PRESENT flip** when the segment moved before `SOF0`, and
  ImageIO's field-by-field parse agreeing with what nc wrote. The discriminating
  number is `GainMapMax`; the harness README now states the criterion that way
  and names the echo explicitly, as do the task file, `TASKS.md`, and
  `insert_baseline_iso_segment`'s rustdoc.

## iso-gain-map-metadata (closed)

**Status:** done
**Updated:** 2026-08-07

- 2026-08-07: Closed after PR #81 merged. Landed: nc-serialized ISO 21496-1
  C.2.2 metadata in both images (C.4.3 version-only in the baseline, C.4.6 full
  structure in the gain map), placed in the **header block** — the correction the
  decoder oracle forced, see the two sections above. Verified by Apple ImageIO
  reading every field back as written and by libultrahdr still decoding the
  legacy dialect from the same file.
- 2026-08-07: **Shipped without two of its own verification bullets, deliberately
  and with the user's call.** Android 15+ was never exercised, and there is still
  no CLI path to a dual-dialect file (`Dialects::LegacyPlusIso` keeps its
  `#[allow(dead_code)]`). Both moved to `output/gain-map-dialect-activation`
  rather than being dropped. The reason to close anyway: the Apple oracle is a
  genuine independent ISO implementation and it agrees field-for-field, so the
  serializer is evidenced; holding the task open past that only kept
  `output/presets` — the plan's biggest hub — blocked behind a device test.
- 2026-08-07: **Do not repeat the headroom mistake.** The oracle's
  `HDR decode: headroom 4.9261084` is `2^AlternateHeadroom`, nc's own declared
  `1000/203` echoed back; it reads identically on a completely flat gain map.
  Evidence of a working reconstruction is `PRESENT` **plus** a `GainMapMax`
  materially above 0. Three documents briefly carried the wrong framing before
  the ship review caught it.
- 2026-08-07: Also filed out of this task's DC-007 read:
  `output/mp-container-conformance` (MP Type `000000` where Table 4 assigns
  `050000`; JFIF-not-first in the dependent image; the missing Exif baseline).
  Conformance only — none of it functional, all of it changing shipped container
  bytes.

## gain-map-dialect-activation

**Status:** not started
**Updated:** 2026-08-07

- Goal: verify the dual-dialect file on Android 15+, and give the ISO dialect a
  CLI path so a user can produce one.
- Filed 2026-08-07 out of `iso-gain-map-metadata`'s close-out; rationale and the
  `output/presets` boundary are in
  [the task file](../tasks/output/gain-map-dialect-activation.md).
- Two things to carry in: the sample set needs `NC_ISO_SAMPLE_EV=3.0` or the gain
  map is inert and the test discriminates nothing; and whichever of this task and
  `output/presets` ships a CLI surface first owns the `gain-map-hdr` name — the
  shipped `ultra-hdr-v1` is contractually ISO-free and must not be re-pointed.

## sdr-preset-followups

**Status:** not started
**Updated:** 2026-08-09

- Goal: hold the three open questions from the SDR presets — the default flip,
  Adobe RGB, and confirming the inherited memory profile — so the space stays
  visible rather than remembered. See
  [the task file](../tasks/output/sdr-preset-followups.md).
- Filed 2026-08-09 alongside `display-p3` / `compatibility`. Deliberately *not*
  answered: the user's steer was to track the work rather than lock the details.

## presets (SDR half: display-p3 + compatibility)

**Status:** in progress
**Updated:** 2026-08-09

- 2026-08-09: **`display-p3` and `compatibility` are live**, `convert`-only,
  `.tif`/`.tiff`. Both are 16-bit integer TIFF (lossless) through the modern
  display stage — NC film RGB v1 → linear ACEScg → shared print controls →
  `pipeline::sdr` with its shoulder and gamut mapping — differing **only** in
  destination gamut. This is the SDR half of `output/presets`; the default flip,
  roll integration and `gain-map-hdr` remain that task's.
- 2026-08-09: They reuse `FrameRender::Tiff` and the existing 16-bit encode path
  rather than introducing a container. `stages::render_sdr_preset` is the pure
  addition: display source → one `sdr::render` → `color::encode_rendered_sdr`,
  returning the same `Rendered` the legacy branch does. An SDR preset's product
  *is* a rendered image plus a profile, so there was nothing new to encode.
- 2026-08-09: **The suffix table is now complete, and completing it broke `nc
  roll` until I fixed the coupling — worth knowing before touching either.**
  `legacy` and `film-master` previously pinned no suffix, so
  `nc convert -o out.jpg` wrote a TIFF named `.jpg`, exit 0, no warning. Giving
  them `.tif`/`.tiff` was right, but `reject_roll_unsupported` *derived*
  "convert-only" from "pins a suffix" — so every preset became roll-refused and
  `nc roll` had nothing to run. Roll capability is now an **explicit list**
  (`legacy` + `film-master`). The two concepts had merely coincided; CLAUDE.md's
  "one table drives both" note was true when written and is not any more.
- 2026-08-09: `RunProfile::SdrTiff` **inherits** `HdrCodedTiff`'s arithmetic
  rather than being calibrated: the buffers genuinely match (one f32 rendition +
  a 3x2 B quantize buffer, streamed strips, the output transform mutating in
  place). That is a structural argument, not a measurement, and it is flagged as
  such on the variant and filed in `output/sdr-preset-followups`.
- 2026-08-09: Three questions deliberately left open rather than guessed —
  which preset becomes the default (a pixel change needing its own version bump
  and report, and what finally lets `legacy` be deleted), Adobe RGB as a
  first-class gamut (a real addition: the modern renderer gamut-*maps* rather
  than tagging, so it needs a colorimetry definition with provenance), and
  confirming the memory profile. All in `output/sdr-preset-followups`.

- 2026-08-09 (review-fix round): **the memory profile is measured, superseding
  the "inherits" entry above.** Peak RSS 0.850 GB at 15.55 MP and 3.594 GB at
  74.65 MP against estimates of 0.921 / 3.911 GB (1.08x / 1.09x over), with the
  enumerated buffers alone at 0.80x / 0.91x of measured. Two frame sizes, as the
  calibration rule requires. `RunProfile::SdrTiff` is calibrated in its own right
  now; the follow-up item asking for it is closed.
- 2026-08-09 (review-fix round): **extensionless output paths are now rejected,
  as a decision rather than a side effect.** Completing the suffix table made
  `nc convert -o positive` exit 2, which previously exited 0 — the code comment
  and the spec edit had justified only the *mismatched*-suffix case. Keeping the
  strictness was the user's call (a file with no extension misleads exactly as a
  wrongly-named one does, and nc is unreleased); it is written into design-spec §5
  and pinned by tests at both the CLI and integration level. The diagnostic no
  longer blames `--output-preset legacy` when no preset was passed — under the
  default path the message is about the output path, and only a *named* preset is
  named.
- 2026-08-09 (review-fix round): `--output-sdr`'s refusal is now reason-specific.
  The presence-based rejection stays (the documented asymmetry), but for
  `display-p3`/`compatibility` it says the flag is **redundant** — those presets
  resolve exactly 16-bit integer TIFF, so calling it a contradiction told the user
  something false about what they were getting.
- 2026-08-09 (review-fix round): the two "accepted: …" lists in
  `OutputPreset::parse` are **generated from `OutputPreset::ALL`**. Both had gone
  stale the moment a preset shipped, so `--output-preset displayp3` listed eight
  names and hid the one the user wanted; a test loops over `ALL` and asserts every
  name appears in both messages, which closes the class rather than the instance.
- 2026-08-09 (review-fix round): `output/sdr-preset-followups` now holds **the SDR
  report block** in place of the closed memory item — `stages::render_sdr_preset`
  drops `SdrRenderMetadata` as `_metadata` while the HDR TIFF presets surface
  their equivalent blocks, so the SDR contract reaches the report only as prose.
  `RenderedSdr::metadata()`'s `#[allow(dead_code)]` is the marker for it.

## output-path-suffix

**Status:** not started
**Updated:** 2026-08-09

- Goal: `-o` names the output, the resolved preset supplies the container. An
  explicit suffix is still validated, and honoured verbatim when it matches.
- Origin: raised 2026-08-09 while updating `docs/using-nc.md` for the completed
  suffix table. Completing the table was right — `nc convert -o out.jpg` used to
  write a TIFF named `.jpg` — but it made the user responsible for knowing each
  preset's container, which is what the preset is for.
- The one thing to settle before writing code: `output/presets` states "the output
  path remains required and is never silently renamed" and owns container-aware
  roll naming. Completing an absent suffix is arguably not renaming, but that
  wording is the governing statement and presets is `[~]` in progress — agree the
  boundary with it rather than around it.
