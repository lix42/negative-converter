# Negative Converter — Progress Log

How each task is actually being carried out — what was done and how, key
decisions, what works, what doesn't, and notes for dependent tasks. TASKS.md holds
the authoritative status (the checkboxes); this file is the narrative beside it.

One `##` section per task, named by the kebab task name. Read this before starting
a task; update your own section as you work. Append entries — don't rewrite them.

## project-foundation
**Status:** done
**Updated:** 2026-06-13

- Goal: Cargo project, dependency declarations, module skeleton, and shared core
  types (`LinearImage`, `FilmBase`, `OutDepth`, `NcError`, param structs).
- **Done.** `cargo init` binary crate `nc` (edition 2024, Rust 1.94). Deps added
  via `cargo add` so versions track current crates.io: `clap` 4 (`derive`),
  `serde` 1 (`derive`), `serde_json`, `tiff`, `image`, `palette`, `rayon`,
  `kamadak-exif`, `lcms2` 6 (pulls `lcms2-sys`, builds the C lib via `pkg-config`/
  vendored — builds clean on macOS).
- Module tree per design-spec §10: `main.rs` (thin: dispatch + exit-code map),
  `cli.rs`, `io/{decode,encode}.rs`, `pipeline/{film_base,color,stages}.rs`,
  `algo/{mod,simple,density}.rs`, `types.rs`. All non-`types` modules are stubs:
  fixed function/trait signatures returning `todo!()` so the tree compiles and
  downstream tasks have a stable shape to fill.
- **Decisions / notes for dependent tasks:**
  - `types.rs` is the neutral contract — **no crate-specific image/TIFF types in
    it**. Conversions to/from `image`/`tiff` belong in `io/*`.
  - `NcError` → exit code lives in **one** place: `NcError::exit_code()` (§11
    mapping: Other=1, Usage=2, Decode=3, Unsupported=4, Write=5). `NcError` impls
    `Display + Error`; `type Result<T> = std::result::Result<T, NcError>` is the
    crate-wide alias. `main` prints the error to stderr and returns the code.
  - Added two enums beyond the task sketch: `OutDepth {U16,F32}` and
    `BigTiff {Auto,On,Off}`, both `#[serde(rename_all="lowercase")]` so recipe
    JSON reads `"u16"`/`"auto"` etc. `OutputParams` carries them.
  - Param structs (`FilmBaseParams`, `DensityParams`, `SimpleParams`,
    `PrintParams`, `OutputParams`) use `#[serde(default)]` + a `Default` impl, so
    a **partial** recipe fills the rest from defaults (tested). Fields mirror the
    §9 flag names exactly (`density_scale`, `print_exposure`, `invert_white_balance`,
    …). Defaults are neutral/identity placeholders — algo tasks refine the numbers.
  - Stub signatures already chosen (change if a task needs to):
    `io::decode::decode(&Path) -> Result<LinearImage>`,
    `io::encode::encode(&LinearImage, &OutputParams, Option<&[u8]> /*icc*/, &Path) -> Result<()>`,
    `pipeline::film_base::estimate(&LinearImage, &FilmBaseParams) -> Result<FilmBase>`,
    `pipeline::color::to_output(&LinearImage, &OutputParams) -> Result<(LinearImage, Vec<u8>)>`
    (returns the converted image **and** the ICC blob to embed),
    `algo::Converter::convert(&self, &LinearImage, &FilmBase) -> Result<LinearImage>`,
    `cli::run() -> Result<()>`.
  - `main.rs` has a temporary crate-level `#![allow(dead_code)]` (the stubs aren't
    wired until `pipeline-orchestration`). **Remove it** when that task lands so
    genuinely-dead code surfaces again.
- **Verify:** `cargo build` clean, `cargo test` 4/4 pass (incl. `DensityParams`
  JSON round-trip + partial-recipe-defaults), `cargo clippy --all-targets` clean.
  `Cargo.lock` committed (binary crate); `/target` gitignored.
- **CI:** `.github/workflows/ci.yml` runs on every PR + push to `main`:
  `cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` → build →
  test (ubuntu-latest, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache`).
  **The gate is strict** — keep `fmt` clean and zero clippy warnings, or CI fails.

## silverfast-decode
**Status:** done
**Updated:** 2026-06-21

- Goal: read SilverFast HDR (48-bit RGB) and HDRi (64-bit RGB+IR) TIFFs into a
  linear `f32` `LinearImage`, preserving the IR plane.
- **Done.** `io/decode.rs` implemented; `decode(&Path) -> Result<(LinearImage,
  DecodeInfo)>`. Full CI gate clean (fmt/clippy `-D warnings`/build/test, 14 tests).

- **Key finding — the task spec's channel model was wrong, now corrected.** I
  inspected the user's real scans (`/Users/lix/src/nc-assets/{48,64}bit-{small,full}`,
  via `tiffdump`/`tiffinfo`). The IR channel is **not** a 4th interleaved sample.
  Layout, consistent across all 16 sample files:
  - **HDR (48-bit):** a single IFD — `SamplesPerPixel=3`, `BitsPerSample=16/16/16`,
    `Photometric=RGB`, `NewSubfileType=0`. No IR.
  - **HDRi (64-bit):** **two IFDs.** IFD0 is identical to the HDR image; **IFD1 is
    the IR plane** — `SamplesPerPixel=1`, `BitsPerSample=16`,
    `Photometric=BlackIsZero`, `NewSubfileType=4`, same W×H as IFD0.
  - Both: uncompressed, little-endian **ClassicTIFF** (full 66 MB files are still
    under the 4 GB classic limit — no BigTIFF seen), `PlanarConfiguration=1`
    (chunky), **no `SampleFormat` tag** ⇒ 16-bit **unsigned**, normalize `/65535`,
    treated as linear (no gamma).
  - **HDR vs HDRi is detected structurally** (`decoder.more_images()`), *not* from
    metadata: `Silverfast:HDRScan="Yes"` appears on **both** variants. Updated
    `design-spec.md` + `.html` §4 and this task's `tasks/silverfast-decode.md`
    accordingly.
- **Decisions / notes for dependent tasks (pipeline-orchestration, cli):**
  - **Signature changed** from the foundation stub: `decode` now returns
    `(LinearImage, DecodeInfo)`. `DecodeInfo` (in `io/decode.rs`, `Serialize`)
    carries `format` (`SilverFastFormat::{Hdr,Hdri}`), `width`/`height`,
    `channels`, `bits_per_sample`, `ir_present`, `make`/`model`/`software`
    (from TIFF tags 271/272/305), and `warnings`. Feed this straight into the
    `inspect`/report JSON — it's the "what was found" record PR #2 asked for.
  - Builds the image via `LinearImage::new(...)` (validated constructor), per the
    foundation note.
  - Failure mapping: unreadable/parse/IO → `NcError::Decode`; recognized-but-
    unhandled layout (non-16-bit, wrong channel count, planar-multi-sample,
    IR-dim mismatch, non-grayscale IR) → `NcError::Unsupported`. No panics.
  - **Planar guard:** the `tiff` crate's `read_image()` only returns the first
    sample plane under `PlanarConfiguration=2`; since RGB has 3 samples we reject
    planar with `Unsupported` rather than silently dropping G/B. All real scans
    are chunky, so this is a safety net.
- **Tests:** real-scan fixtures committed at `tests/fixtures/hdr-48bit.tif`
  (from `48bit-small/1.tif`) and `hdri-64bit.tif` (from `64bit-small/1.tif`) so
  the real-file tests also run in CI. Plus synthetic single-/two-IFD TIFFs built
  with the `tiff` encoder cover normalization, IR split, structural detection, and
  the `Unsupported`/`Decode` error paths.
- **Review pass (pre-ship):** added a `NewSubfileType` guard on IFD1 — the real IR
  plane is marked `NewSubfileType=4` (verified on the fixture); a matching-dimension
  16-bit grayscale second IFD without it is still accepted (layout is
  reverse-engineered; IR is only carried in Step 1) but now records a warning, so an
  incidental second page isn't reported as IR provenance with no trace. Added three
  tests: non-grayscale IR plane → `Unsupported`, the extra-IFD warning path, and a
  `Software`-tag round-trip pinning the "read metadata before `next_image()`"
  ordering. 11 decode tests, all green. The planar-config and `read_plane_u16`
  non-`U16` branches stay fixture-only (the `tiff` encoder can't synthesize those
  inputs); they fail loudly and are noted as known-untested-by-design.
- **PR-review pass (bot feedback on #8):** three further fixes.
  - **Decode limit:** the `tiff` crate's default `Limits` caps a single
    `read_image()` at 256 MiB; a full-size RGB16 IFD can exceed that. Raised
    `decoding_buffer_size`/`intermediate_buffer_size` to the 4 GiB classic-TIFF
    ceiling via `with_limits` — full archival scans decode in one read, while a
    corrupt oversized header still trips the cap and fails loudly (not OOM).
  - **Error contract:** `tiff_err` (was `decode_err`) now maps
    `TiffError::UnsupportedError` → `NcError::Unsupported` (exit 4) and everything
    else → `Decode` (exit 3), so readable-but-unsupported layouts (photometric/
    compression/etc.) are distinguishable from corrupt files per design-spec §11.
  - **WhiteIsZero IR:** `colortype()` returns `Gray(16)` for *both* BlackIsZero and
    WhiteIsZero, and the crate inverts WhiteIsZero on read — so a WhiteIsZero second
    page would be silently kept as an inverted IR plane. Now require
    `PhotometricInterpretation=1` (BlackIsZero, the verified layout) on IFD1, with a
    test. 12 decode tests, all green.
- **High-res preview IFD (2026-06-30, during film-base real-scan verification):**
  the full-resolution Nikon HDRi scans (5184×3600, 159 MB) have **three** IFDs —
  IFD0 RGB, **IFD1 a reduced-resolution RGB preview** (`NewSubfileType` bit 0,
  1470×1021), IFD2 the full-res IR plane (`NewSubfileType=4`). The old code assumed
  the *second* IFD was the IR plane and rejected these files as a mismatched-
  dimension IR (`Unsupported`). Fix: scan **all** remaining IFDs, **skip** any
  reduced-resolution preview (bit 0) without reading its strips, and validate the
  first non-preview page as the IR plane with the same strict checks as before
  (dims match, `Gray(16)`, `PhotometricInterpretation=1`, `NewSubfileType=4` else
  warn). All prior strict-rejection tests keep their semantics (a full-res non-gray
  / mismatched / WhiteIsZero page still errors); added
  `skips_reduced_resolution_preview_before_ir` mirroring the real 3-IFD layout.
  Verified: both `20260630-nikon-84{2,4}.tif` now decode as `Hdri 5184x3600
  ir=true` with **no warnings**. **14 decode tests, all green.** (Landed on the
  `film-base-estimation` branch since it blocked real-scan verification; logically
  a `silverfast-decode` follow-up.)
- **Ship review hardening:** the preview-skip now also requires *reduced
  dimensions*, not the `NewSubfileType` bit alone, so a full-res IR plane carrying a
  stray bit 0 (e.g. `5` = reduced|transparency-mask) still reaches IR validation
  instead of being silently dropped. `PlanarConfiguration` read errors now surface
  as `Decode` (a corrupt tag no longer silently defaults to chunky). Added tests:
  `preview_without_ir_decodes_as_hdr`, plus an accepted-by-shape warning assertion.

## tiff-encode
**Status:** done
**Updated:** 2026-06-28

- Goal: write u16/f32 TIFF with embedded ICC, BigTIFF auto-promote, IR export, and
  sidecar JSON.
- **Done.** `io/encode.rs` implements three public fns:
  - `encode(image, &OutputParams, Option<&[u8]> icc, &Path)` — kept the
    foundation stub signature instead of the task's `EncodeOptions`/`encode_tiff`
    sketch: `OutputParams` already carries `out_depth` + `bigtiff`, and `color`
    passes the ICC blob separately, so a second options struct would be redundant.
  - `export_ir(image, depth: OutDepth, &Path)` — added a `depth` param (the task's
    bare `export_ir(path, img)` gave no way to pick the IR file's bit depth; user
    confirmed taking the param). Errors `NcError::Unsupported` when `image.ir` is
    `None` — fail loudly rather than write a placeholder. The check runs *before*
    `File::create` (post-review) so a no-IR failure never truncates an existing
    target the user pointed `--export-ir` at.
  - `write_sidecar(output_path, recipe_json)` — writes `<output>.json` (e.g.
    `out.tiff` → `out.tiff.json`), matching design-spec wording. IO errors →
    `NcError::Write`.
- **`tiff` 0.11.3 capability check (verified via current docs, no gaps):**
  - f32 is native — `colortype::{RGB32Float, Gray32Float}` (SampleFormat::Float,
    32 bpp); u16 via `{RGB16, Gray16}`. No manual sample-format writing needed.
  - BigTIFF is a *constructor* choice: `TiffEncoder::new` (classic) vs `new_big`,
    which return **different `TiffKind` types** — so the policy can't be a runtime
    `bool` variable. Solved with a single generic `encode_planar<W, K: TiffKind,
    C: ColorType>` helper, dispatched by a `match (depth, big)` that picks the
    concrete `new`/`new_big` + colortype monomorphization. One body covers all
    u16/f32 × classic/big × RGB/Gray combos.
  - ICC: the crate has a first-class `Tag::IccProfile` (= 34675); written as a
    BYTE array via `image.encoder().write_tag(...)` before `write_data`. Read back
    in tests with `Decoder::get_tag_u8_vec(Tag::IccProfile)`.
- **Decisions / notes for dependent tasks:**
  - **Testable seam:** the `&Path` entry points wrap thin `*_to_writer<W: Write +
    Seek>` cores; tests encode into a `Cursor<Vec<u8>>` and decode the bytes back
    with `tiff::decoder` — no temp files, deterministic. `pipeline-orchestration`
    can reuse the path-based fns directly.
  - **u16 quantization:** `v.clamp(0.0, 1.0) * 65535.0` then `f32::round`
    (round-half-away-from-zero). Out-of-range clamps (no silent wrap); `NaN`
    forced to 0 via the `as` cast.
  - **f32 path:** samples written directly, **no clamp** — values > 1.0 preserved
    for HDR (round-trips exactly in test).
  - **Clipping/loss report (added 2026-06-28, post-review):** `encode` now returns
    `EncodeReport { total_samples, clipped_low, clipped_high, non_finite }`
    (`types.rs`, `#[must_use]`, `Serialize`). `color-management` deliberately does
    not clamp and may hand out-of-`[0,1]` or non-finite (`NaN`/`inf`) samples
    (density log/division math), so the encoder counts the trouble and surfaces it
    instead of silently blackening pixels — `any_loss()` / `loss_fraction()` for
    consumers. Model: `clipped_*` = finite out-of-`[0,1]` values clamped by the
    u16 path; `non_finite` = any `NaN`/`inf`, counted at **both** depths (u16
    forces to 0; f32 writes verbatim but is scanned via `scan_non_finite`), so a
    numerical fault surfaces regardless of output depth. `export_ir` discards the
    report behind a `debug_assert!(!any_loss())` because IR is decode-normalized to
    `[0,1]` and carried untouched (revisit when IR processing lands).
    **`pipeline-orchestration` must fold this into the JSON report and honor
    `--strict`** — the encoder only surfaces, doesn't decide.
  - **BigTIFF `Auto`:** promote when `w*h*channels*bytes + ICC bytes + 1 MiB
    margin` exceeds `u32::MAX` (~4 GiB classic 32-bit-offset limit). The embedded
    ICC is counted explicitly (post-review) so a large custom profile near the
    limit can't slip past the fixed margin. `resolve_bigtiff` uses saturating
    arithmetic so huge synthetic dims don't overflow the estimate.
  - `impl From<tiff::TiffError> for NcError` maps encoder errors to
    `NcError::Write` (exit 5).
  - **Explicit flush (added 2026-06-28, post-review):** the `tiff` encoder never
    flushes and `TiffEncoder` exposes no way to reclaim the moved writer, so the
    `&Path` entry points now *borrow* the `BufWriter` into the encoder (`&mut W`
    is `Write + Seek`) and call `flush_buf` after encoding. `BufWriter`'s implicit
    drop-flush discards errors (e.g. disk full on the last block) — flushing
    explicitly surfaces them as `NcError::Write` instead of silently truncating
    the file.
  - **Not yet wired:** `--export-ir` path and the resolved recipe-JSON for the
    sidecar still need a typed home in the CLI param surface (see `cli-framework`
    notes); orchestration calls `export_ir`/`write_sidecar` once those exist.
- **Verify:** `cargo test` (10 encode tests: u16/f32 round-trip incl. >1.0, BigTIFF
  policy header magic 42/43, Auto estimate threshold, ICC embed+read, IR
  single-channel + no-IR error, sidecar path, plus clipping-count and non-finite
  report assertions). Full suite 63/63 after the post-review additions; `fmt
  --check` clean, `clippy --all-targets -D warnings` clean.

## color-management
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

## film-base-estimation
**Status:** done
**Updated:** 2026-06-30

- Goal: estimate `Dmin` `FilmBase` from border/region with full CLI override.
- **Done.** `pipeline/film_base.rs` implements `estimate(&LinearImage,
  &FilmBaseParams) -> Result<FilmBase>` as a thin `match` over the selected
  `FilmBaseSource`, delegating to pure helpers (`sample_region`, `auto_estimate`,
  `percentile`).
- **Rebased onto the merged `cli-framework` model (was originally built on the
  flat `FilmBaseParams`).** The foundation-review question "flat fields vs enum"
  was answered by `cli-framework`, not here: `FilmBaseParams` is now `{ source:
  FilmBaseSource }` where `FilmBaseSource = Auto | Region([u32;4]) |
  Explicit([f32;3])`. Precedence (`explicit > region > auto`) is therefore
  **structural** and resolved in `cli.rs`'s flag→recipe merge — `estimate` just
  honors whichever variant it's handed. I dropped my earlier `FilmBaseEstimate`
  return type and its separate report-enum (name-collided with the input
  `FilmBaseSource` and the merged stub is `-> Result<FilmBase>`); reporting *how*
  the base was chosen is derivable by the orchestrator from `params.source`.
- **Decisions (unchanged by the rebase):**
  - **Estimation statistic:** per-channel **97th percentile** (nearest-rank,
    `SAMPLE_PERCENTILE`) over the sampled pixels — resists hot pixels/dust while
    landing on the bright base (task suggested 95th–99th). `percentile` sorts NaNs
    to the end so they can't poison the rank.
  - **Region sampling** validates the rect against image bounds with u64 math
    (no u32 wrap near the edge); out-of-bounds or empty region → `NcError::Usage`
    (exit 2). `cli.rs` already rejects a zero-area `Region` at the boundary, but
    the bounds/empty check stays here as defense-in-depth (the CLI can't see the
    image dimensions, so OOB can only be caught in the stage).
  - **Auto border detection (Step-1 heuristic):** sample the outer margin band
    (`AUTO_MARGIN_FRAC = 4%` of the shorter side on all four edges), take the p97
    per channel as the candidate base, and accept only if (a) the band is
    near-uniform — per-channel relative spread `(p97−p10)/p97 ≤ 0.15` — and (b) the
    base is brighter than the interior **median** (median, not p97, so a sampled
    interior that clips a wide rebate doesn't defeat the check). On low confidence
    it returns a clear, actionable `NcError::Other` telling the user to pass
    `--film-base`/`--base-region` (per user decision: **hard error, no silent
    fallback** to whole-image sampling).
- **Notes for dependent tasks:**
  - `pipeline-orchestration` / `nc estimate`: `estimate` returns just the resolved
    `FilmBase`. For the JSON report, take the *source* label from `cfg.film_base
    .source` (you already hold it) rather than expecting it back from `estimate`.
    If a report ever needs the auto path's *detected* region, `estimate` will have
    to be extended to return it — today it doesn't (the auto sample is a spread
    edge band, not a single reusable `--base-region` rect).
- **Verify:** 8 unit tests in `film_base.rs` (explicit verbatim, region samples the
  rect, auto detects a bright uniform border, p97 rejects hot pixels, OOB/empty
  region → Usage error, auto fails loudly on no-border and on a non-uniform
  gradient, non-finite samples never become the base). Full suite **76/76**,
  `clippy --all-targets -D warnings` clean, `fmt` clean.
- **Ship review pass (4 agents):** applied the accepted findings — `percentile` now
  ranks over finite values only via `f32::total_cmp` (a NaN/±inf can never be
  returned as the base; comment was previously unsound); fixed a contradictory
  "densest" comment and softened the auto doc's over-claim (it can mis-anchor on a
  uniform bright surround — deferred to `auto-base-redesign`); cast the auto index
  math to `usize` first. Declined (with reasons): changing the auto heuristic now
  (that's the `auto-base-redesign` task, which gained a "must not mis-anchor on a
  bright surround" requirement) and the auto-failure `NcError` variant (Other/exit-1
  catch-all is defensible per §11).
- **Real-scan verification (throwaway `#[ignore]` probes, decoded via `io::decode`;
  probes not committed):**
  - Decoding works on every real scan tried: `../nc-assets/{48,64}bit-full/*`
    (3456×2396) and the full-res `~/Pictures/scan/20260630-nikon-84{2,4}.tif`
    (5184×3600 HDRi, after the decode preview-IFD fix above). Region/explicit
    `estimate` paths return sensible per-channel values on all of them.
  - **Real scans have a `holder → thin rebate → picture` structure, NOT a bright
    outer margin.** Marching a 1px strip inward from each edge: the outermost band
    is the near-black film **holder** (~0.01), then a **thin, bright, uniform
    orange film-base rebate** sits *behind* it, then the picture. The rebate only
    appears on some edges and can be a few px wide. Measured rebate is consistent
    per film stock (e.g. `48bit-full/1` bottom and `/2` left both ≈`[0.53, 0.26,
    0.16]`), confirming Dmin is a stock/develop/scanner property, not per-frame.
  - **The current outer-4%-margin auto heuristic can't isolate that rebate** — it
    averages holder+rebate+picture into one high-spread blob and **fails loudly**
    (correct fail-safe, exercised on real data), but the auto *happy path* does not
    work on real scans. A proper fix (scan strips inward, pick the brightest
    low-spread band past the holder) is **deferred** — see decision below.
  - **Decision (with user): focus on the explicit-reference workflow, not auto.**
    Because Dmin is constant across a roll scanned with fixed settings, the
    accurate path is: scan one **unexposed reference** frame once, measure its base
    with `--base-region`, and reuse it as `--film-base` across the batch (design's
    reusable-recipe idea). Verified end-to-end: the unexposed reference
    `20260630-nikon-844.tif` (same film/develop/scanner as the `842` scan) yields a
    large uniform base of **`[0.553, 0.271, 0.159]`** from a center region; `842`'s
    own left-edge rebate reads `[0.475, 0.236, 0.136]` and its picture center
    `[0.387, 0.189, 0.090]` (darker, as expected). Note the reference-vs-edge-rebate
    gap (~14%): the large clean unexposed area is the more reliable anchor than a
    narrow edge strip (edge falloff/fog) — another reason to prefer a dedicated
    reference frame.
- **Follow-up tasks noted (not in this branch):**
  - **Auto redesign:** inward-strip "brightest uniform band past the holder"
    detector so `--auto-base` works on real `holder→rebate→picture` scans. Deferred
    per the Step-1 "don't over-engineer auto" guidance now that the explicit path
    covers real work.
  - **White holder support:** some film holders are white, not black — auto/border
    logic assumes a dark surround. Add a CLI flag (e.g. `--holder white|black`) to
    tell the detector which. Follow-up.

## algo-interface
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

## cli-framework
**Status:** done
**Updated:** 2026-06-18

- Goal: clap subcommands, recipe load/merge (flags override), JSON report,
  `params` subcommand, exit-code mapping.
- Note (from project-foundation review): param-struct ranges are doc-only so far.
  Add validation **at the parse/merge boundary** (not in the pure stages): reject
  NaN, `clip_low > clip_high`, non-positive gamma/gains, etc., mapping failures to
  `NcError::Usage` (exit 2) so bad recipes fail loudly. The pure stages then trust
  their inputs.
- Note (from PR #2 review): **reject unknown recipe keys.** With `#[serde(default)]`
  alone a typo like `density_gama` silently deserializes to the default → a quietly
  wrong image, which the "fail loudly" rule forbids. Add `#[serde(deny_unknown_fields)]`
  (or equivalent) on the recipe-facing structs — placement depends on the recipe
  layout you choose here (per-struct sub-objects vs one flat object); deny only
  works cleanly with the former.
- Note (from PR #2 review): `--export-ir <path>` (design §9) has no typed home yet —
  `OutputParams` only carries depth/profile/bigtiff. Add the path here (or in a
  dedicated output config) as you assemble the full param surface, so orchestration
  can drive the IR exporter. Likewise the encoder needs the resolved recipe JSON to
  write the `out.tiff.json` sidecar — pass it the Report/Recipe value once defined.

- **Done.** `cli.rs` holds the full agent-facing surface; `cli::run()` parses and
  dispatches. `main`'s exit-code mapping is unchanged.
- **Decisions for dependent tasks (esp. `pipeline-orchestration`):**
  - **Recipe = nested per-stage objects** (user decision), not a flat bag:
    `{ "algorithm": "...", "input": {…}, "film_base": {…}, "density": {…},
    "print": {…}, "simple": {…}, "output": {…} }`. This is the *only* layout that
    lets `#[serde(deny_unknown_fields)]` reject typos at every level
    (`serde(flatten)` would silently defeat it). The single struct that *is* this
    shape is `cli::ResolvedConfig` — it doubles as the recipe (partial, serde
    defaults fill gaps), the `--dump-params` output, and `nc params` output, so the
    three can't drift. **Updated design-spec §8/§9 + HTML** to document the nesting.
  - **Merge model:** clap arg structs use `Option<T>` per knob (+ presence-flag
    `bool`s for `--auto-base`/`--assume-linear`). `merge(cfg, &ConvertArgs)` is a
    pure fn applying `defaults ← recipe ← CLI` (flags win); a `false` presence flag
    never clobbers a recipe `true`. Orchestration consumes the returned
    `ResolvedConfig` — it should not re-read CLI args.
  - **Validation at the boundary:** `validate(&ResolvedConfig)` rejects NaN/inf,
    `clip_low > clip_high`, non-positive gamma/gains/film-base, zero `base-region`
    w/h → all `NcError::Usage` (exit 2). **Pure stages can trust their inputs** —
    don't re-validate ranges downstream.
  - **New types added to `types.rs`:** `Algorithm {Simple,Density}` (default
    `Density`, serde-lowercase + `clap::ValueEnum`); `InputParams`
    (`assume_linear`, `input_profile`) for the §9 input flags; `export_ir:
    Option<String>` added to `OutputParams`. `OutDepth`/`BigTiff` gained
    `clap::ValueEnum` (their lowercase ValueEnum names already match serde).
    `deny_unknown_fields` added to all recipe-facing param structs.
  - **stdout is report-only;** logs/warnings/errors go to stderr (agents pipe
    stdout). `--report json|none`, `--report-file`, `-v`, `--quiet`, `--strict`
    are parsed and carried; the `Report` struct + `emit_report()` exist but are
    populated by orchestration (kept minimal here).
  - **clap error handling:** `Cli::parse()` lets clap exit directly — `--help`/
    `--version` exit 0, usage/value-parser errors exit 2 — so those don't route
    through `NcError`. Everything else flows through the `NcError` exit-code map.
  - **Stubs:** `convert`/`inspect`/`estimate` resolve+validate config (and write
    `--dump-params`) then return `NcError::Other("… not yet wired
    (pipeline-orchestration)")` (exit 1). The pipeline replaces those returns.
    `main.rs`'s `#![allow(dead_code)]` still needed (Report/emit_report unused
    until wired) — remove it in `pipeline-orchestration`.
- **Verify:** `cargo fmt --check`, `clippy --all-targets -D warnings`, build all
  clean; `cargo test` 14/14 (6 new cli tests: parser `debug_assert`, comma-list
  parsers, merge precedence, dump→reload round-trip, unknown-key rejection,
  validation). Manual: `nc --help`/`convert --help` list every §9 flag; `nc params`
  emits the full default JSON; dump→`--params` reload round-trips byte-identical;
  forced usage/validation/bad-recipe/bad-value paths all exit 2.
- **2026-06-18 (ship review):** multi-agent review before merge. Fixes:
  - **Bug:** `PrintParams::print_exposure` default was `1.0`; spec §9 neutral is
    `0.0` (exposure is in **stops/EV**, not a linear multiplier — every other print
    default is identity). Corrected to `0.0` and documented the unit in `types.rs`.
  - **`--strict` made an explicit deferral:** it's parsed but only acted on by
    `pipeline-orchestration` (promote warnings→errors); marked so in `run_convert`
    rather than looking silently dropped. **For pipeline-orchestration: wire
    `args.strict` into the warnings path.**
  - **Tests +3 → 25 total:** boolean presence-flag merge (`assume_linear`/
    `auto_base` — a `false` flag never clobbers a recipe `true`), `load_recipe`
    error mapping (missing/malformed/unknown-key file → `NcError::Usage`), and
    recipe-smuggled bad values caught by `validate` (zero film-base transmission,
    zero-area `base_region`) — recipes bypass clap value-parsers, so `validate` is
    their only guard.
  - **Deferred (noted, not done):** profile/`export_ir` as `PathBuf`/enum vs
    `String`; range bounds on print knobs and `film_base ≤ 1.0`; a `ValidatedConfig`
    newtype to make "unvalidated config reaches a stage" unrepresentable; a
    `--no-assume-linear` counterpart. These belong to the algorithm / film-base /
    pipeline-orchestration tasks that own those semantics.
- **2026-06-18 (PR #5 bot review):** addressed automated review (claude-review /
  Codex / Gemini). Fixes (26 tests):
  - **`export_ir` moved `OutputParams` → `InputParams`** (recipe key
    `output.export_ir` → `input.export_ir`). Spec §9 lists `--export-ir` under
    Input/decode; with `deny_unknown_fields` the old home rejected the
    documented recipe shape. Code now matches the spec.
  - **`--seed <n>` now parsed** (reserved `Option<u64>` on `ConvertArgs`, carried
    like `--strict`). Spec §documents it; clap previously rejected it as unknown,
    so the documented interface wasn't actually accepted.
  - **Equal clip endpoints rejected:** `validate` now requires `clip_low <
    clip_high` (was `<=`) — equal bounds are a zero-width interval the simple
    remap can't normalize without dividing by zero.
  - **Declined (with reasons):** let-chain "unstable" claim is false here (edition
    2024, CI green proves it compiles); rejecting flags for the unselected
    algorithm is deliberate — inert params are retained so recipes round-trip
    across `--algorithm` switches.
- **2026-06-18 (#5/#6 enum rework, user-directed):** the two deferred merge gaps
  were fixed by modeling mutually-exclusive choices as enums (illegal states
  unrepresentable), not patching the booleans. **Recipe shape changed** — spec
  §9 (md+html) updated to match:
  - **`FilmBaseSource { Auto, Region([u32;4]), Explicit([f32;3]) }`** replaces the
    `film_base`/`base_region`/`auto_base` trio. `FilmBaseParams` is now
    `{ source }`. Recipe: `"film_base": { "source": "auto" | {"region":[…]} |
    {"explicit":[…]} }`. Higher specificity always wins with no fallback, so it
    was always one choice, not three knobs.
  - **`InputColor { Auto, Linear, Profile(String) }`** replaces
    `assume_linear`/`input_profile`. `InputParams` is now `{ color, export_ir }`.
    Recipe: `"input": { "color": "auto" | "linear" | {"profile":"<icc>"} }`.
    `"auto"` (the no-flag default) = the file's embedded/default profile, which is
    **not** linear — that's why `assume_linear` can't be inferred from "no
    profile". **For color-management/decode: define what `Auto` resolves to.**
  - **CLI:** the source flags within each group are now a clap mutual-exclusion
    group (`conflicts_with`/`conflicts_with_all`) — passing two is a usage error.
    `merge` maps whichever single flag is present to the enum, replacing the
    recipe's choice; so `--input-profile` over a recipe `linear` now wins cleanly
    (the #6 bug) and `--base-region` over a recipe explicit base wins (the #5 bug).
  - **Verified:** fmt/clippy/build clean, **27 tests**; manual `nc params` shows
    the new shapes; recipe load→`--dump-params` round-trips the nested variants;
    `--assume-linear` over a `{"profile":…}` recipe resolves to `"linear"`.

## algo-simple
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

## algo-density
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

## pipeline-orchestration
**Status:** done
**Updated:** 2026-07-14

- Goal: wire `convert`/`inspect`/`estimate` (+ `params`) end to end, producing a
  positive TIFF and JSON reports from a real scan. Final Step-1 MVP task.
- **Done.** The four subcommands run end to end. Full CI gate clean
  (`fmt --check`, `clippy --all-targets -D warnings`, `build`, `test`); suite is
  **124 tests** (110 unit + 14 new end-to-end in `tests/pipeline.rs`).

### What was built
- **`pipeline/stages.rs` (the pure in-memory core).** `render(image,
  &FilmBaseParams, AlgoParams, &OutputParams) -> Result<Rendered>` threads stages
  2–4 (film-base estimate → algorithm → output color transform) and returns
  `Rendered { image, icc, film_base }`. `algo_params(algorithm, &simple,
  &density, &print) -> AlgoParams` assembles the selected algorithm's param set.
  Both are **pure** (no I/O); decode (stage 1) and encode (stage 5) are I/O and
  stay in the `cli` orchestrator, honoring "stages stay pure; main/cli
  orchestrate."
- **`cli.rs` (orchestration).** `run_convert` does decode → IR-export fast-fail
  guard → render → lcms error check → BigTIFF-auto notice → optional IR export →
  encode → effective-recipe sidecar → report emit → `--strict` gate. `run_inspect`
  decodes and reports `DecodeInfo` + a best-effort auto `Dmin` (a failed auto is a
  report *warning*, not fatal — real scans need `--base-region`/`--film-base`).
  `run_estimate` runs only film-base estimation from the selected source and emits
  the `FilmBase`. `run_params` unchanged.
- **Report shape.** One `Report` struct serves all commands; per-command
  irrelevant fields are `None`/empty and omitted via `skip_serializing_if`, so
  stdout is a clean per-command JSON object. Serialize-only (embeds the
  serialize-only `DecodeInfo`/`EncodeReport`). `film_base_source` is the
  **structured** `FilmBaseSource` (`"auto"` / `{"region":[…]}` / `{"explicit":[…]}`),
  not a display string, so an agent gets the sampled rect without string-parsing.
- **`io/encode.rs`.** Added `plans_bigtiff(&OutputParams, &LinearImage, icc_len)`
  reusing the internal `resolve_bigtiff`, so orchestration can report an `auto`
  BigTIFF promotion without duplicating the threshold logic.
- **Removed `#![allow(dead_code)]` from `main.rs`.** Revealed three deliberately
  unused-by-Step-1 items now behind narrow, documented `#[allow(dead_code)]`:
  `algo::Algorithm` + `AlgoParams::algorithm()` (the CLI/recipe standardized on
  the identical `types::Algorithm`) and `color::icc_profile` (orchestration gets
  the ICC from `to_output`). See follow-up below.

### Key decisions / notes for dependents
- **lcms2 gotcha handled (CLAUDE.md).** `color.rs` builds profiles/transforms on
  lcms2's **global** context, and `transform_in_place` is infallible; Little CMS's
  default handler is a **no-op that silently swallows errors** (verified in the
  vendored source). The safe `lcms2` wrapper only exposes the handler per
  `ThreadContext`, which would *not* cover the global-context transforms — so
  `cli` installs the process-global handler directly via the **`lcms2-sys` FFI**
  (`cmsSetLogErrorHandler`), added as a direct dep (`cargo add lcms2-sys`, 4.0.7,
  already transitively present). The handler sets an `AtomicBool` + logs to
  stderr; `run_convert` clears the flag right before `render` and checks it right
  after, turning a runtime CMS fault into a loud `NcError::Other`.
- **Exit codes (§11):** Usage=2 (bad params/recipe, bad `--film-base`), Decode=3
  (unreadable/non-TIFF input), Unsupported=4 (`--export-ir` on an HDR scan with no
  IR plane — fails *before* any output is written), Write=5 (unwritable output
  path), Other=1 (degenerate estimated base; `--strict` warning promotion; lcms
  fault). All exercised end-to-end.
- **`--strict`** promotes any accumulated warning to exit 1 *after* emitting the
  report (the machine-readable record still lands). Output/sidecar are written
  before the strict gate because clip counts are only known post-encode — a
  `--strict` failure therefore leaves the (honestly-reported-as-clipped) files on
  disk; the loud exit code is the signal. Documented in the flag's behavior.
- **IR handling:** the "IR present but not consumed in Step 1" notice is a report
  warning **only when `--export-ir` is absent** — exporting *is* the user handling
  it, so `--strict --export-ir` is a usable workflow on the primary HDRi format
  (otherwise every HDRi `--strict` run would fail on that notice).
- **`estimate` gained the film-base flags** (`EstimateArgs` flattens
  `FilmBaseOverrides`) so the design-spec §8 calibrate-once-from-a-reference
  workflow (`nc estimate ref.tif --base-region …`) works; `inspect` keeps the
  bare `IoArgs`. Explicit-base range validation is shared with `convert` via
  `validate_explicit_film_base`.
- **Verbosity:** `Log { verbose, quiet }` — `-v` enables stderr progress lines,
  `--quiet` silences them; warnings always land in the report regardless, and a
  **non-finite (NaN/inf) fault is echoed to stderr even under `--quiet`** (it's a
  numerical fault, not routine clipping, so `--quiet --report none` can't fully
  hide it). stdout stays report-only.

### Real-scan verification (committed fixtures — the large `../nc-assets/*`
  and `~/Pictures/scan/*` dirs are absent in this environment, so verification
  used the committed real SilverFast scans `tests/fixtures/{hdr-48bit,hdri-64bit}.tif`,
  502×462, from the small asset set):
- `inspect hdri-64bit.tif` → `format=hdri 502x462 ir=true`,
  make/model/software `Plustek / OpticFilm 8300i / SilverFast 9.2.8`; auto Dmin
  reported unavailable (non-uniform border, relative spread 0.83) as a warning —
  the documented real-scan behavior.
- `estimate hdr-48bit.tif --base-region 10,10,50,50` → structured
  `film_base_source = {"region":[10,10,50,50]}`, a finite per-channel base.
- `convert … --algorithm density` (default **u16**) → exit 0, report warns
  **100% clipped_high (695772/695772)** — the known no-white-anchor issue below.
- `convert … --algorithm density --out-depth f32 --film-base 0.9,0.55,0.42` →
  exit 0, `loss = {clipped_low:0, clipped_high:0, non_finite:0}` (clean HDR).
- `convert … --export-ir ir.tiff` on HDRi → writes a 1-channel IR TIFF; on HDR →
  exit 4, no output written. `--strict --export-ir` on HDRi → exit 0.
- Determinism confirmed: two identical `convert` runs produce **byte-identical**
  TIFF + sidecar; a sidecar reloaded via `--params` reproduces the output.

### Known issue explicitly NOT fixed here (parallel task `dmax-white-anchor`)
- With the **current** default density params the render maps scene black (the
  base) to `1.0` with all detail above it, so the default **u16** encode clips
  heavily (≈100% on these fixtures) — surfaced loudly via the clip report and
  `--strict`-promotable. This is **temporary**: the parallel `dmax-white-anchor`
  branch adds a Dmax white anchor that drops the default clip fraction to ~0.5%
  (per that agent), so nothing here treats the heavy clipping as permanent. Test
  wording/assertions were kept anchor-independent — `u16_clipping_is_reported_and_
  strict_promotes_it` forces clipping with a large `--print-exposure` rather than
  relying on the default, so it stays valid after the anchor lands. Verify HDR
  output end-to-end with `--out-depth f32` (clean) meanwhile.
- **Report is extensible for the incoming `dmax`.** `dmax-white-anchor` adds a
  defaulted `Converter::convert_reported -> ConvertReport { dmax }`; at merge the
  resolved anchor rides into the JSON report by adding one optional field to
  `Report` and carrying a `ConvertReport` on `stages::Rendered` (which already
  bundles the algorithm's outputs) — the flat `skip_serializing_if` report shape
  takes a new optional field without reshaping the JSON. Not integrated here per
  the coordination note (its API does not exist in this branch); cli.rs edits were
  kept tight so its `DmaxOverrides` merge/validate/flatten conflict stays small.

### Follow-ups / deferred (with reasons)
- **Unify the two `Algorithm` enums.** `types::Algorithm` (CLI/recipe) and
  `algo::Algorithm` are identical; two reviewers suggested collapsing them via
  `pub use crate::types::Algorithm;` in `algo`. Deferred to keep this task's diff
  off the completed `algo-interface` module's type identity during parallel work;
  left behind a documented `#[allow(dead_code)]`. Cheap, low-risk cleanup.
- **Per-command `Report` enum.** A tagged `enum ReportBody { Convert/Inspect/
  Estimate }` would make "field set for the wrong command" unrepresentable
  (type-design review). Deferred as beyond Step-1 MVP; the flat all-`Option`
  shape with `skip_serializing_if` is tested and produces correct per-command JSON.
- **lcms handler** latches on *any* lcms log (can't see severity), so a benign
  recoverable ICC-parse warning during a custom `--output-profile` could fail an
  otherwise-good run — a *loud* false-positive (not a silent-wrong image), kept as
  the fail-safe posture; refine by inspecting error codes if it ever bites.

### Review
- Ran `pr-review-toolkit:review-pr` (code / silent-failure / tests / type-design /
  comments) — 1 full round + 1 confirmation round.
  - **Fixed:** stale `cli.rs` module doc; `--quiet --report none` could hide a
    non-finite fault (now always stderr-echoed); IR-present warning made
    `--strict --export-ir` unusable (now gated); lossy `film_base_source` string
    → structured enum; duplicated explicit-base validation → shared
    `validate_explicit_film_base`; IR export reordered before main encode; lcms
    flag cleared before render; dangling `Reporter::warn` doc link. Added tests:
    determinism, sidecar recipe round-trip via `--params`, exact exit codes
    (1/3/5), `-v` stdout-cleanliness, `--report-file`.
  - **Deferred (above):** Algorithm-enum unification, per-command Report enum,
    lcms severity discrimination. Confirmation re-review came back clean of
    important issues.

- 2026-07-14 — **PR #16 review fixes (data-loss guards).** (1) Every write
  target (`--output`, the sidecar, `--dump-params`, `--report-file`,
  `--export-ir`) is now checked against the input scan and against each other
  before anything is decoded or written — previously `-o <input>` destroyed the
  negative and `--report-file <output>` truncated the just-written TIFF, both
  with exit 0. `encode::sidecar_path` extracted so the CLI can include the
  sidecar in the check. (2) `--input-profile` / recipe `input.color.profile`
  was a silent no-op (parsed, never applied); `convert` now rejects it with
  exit 4 until input-side color management lands (§9 note added). Four new E2E
  tests pin all of it.
- 2026-07-14 — **rebased onto merged #16 and wired the report.** The rebase had
  one trivial conflict (cli.rs import list). The merge-time follow-up landed in
  this branch since orchestration is now underneath it: `stages::render` calls
  `Converter::convert_reported`, `Rendered` carries the `ConvertReport`, and the
  convert JSON report gains an optional `dmax` field (auto/explicit value;
  absent for `--no-d-max` and `simple`). E2E test pins both presence and
  absence.
- 2026-07-14 — **closed out.** Manual review approved; shipped via `/ship`
  (gates re-run green: 110 unit + 13 integration tests; CLAUDE.md refreshed —
  module map, dead-code note, and the lcms2 global-handler mechanism now match
  the implementation; branch rebased onto post-docs main). **Step-1 MVP is
  complete** — Phase 4 closes. Merge-time follow-up recorded for
  `dmax-white-anchor` integration: switch `stages::render` to
  `Converter::convert_reported`, carry `ConvertReport` on `Rendered`, add one
  Option field to `Report`.

## auto-base-redesign
**Status:** done
**Updated:** 2026-07-15

- Goal: robust `--auto-base` film-base detection on real scan layouts (dark
  holder → rebate → picture), replacing the best-effort Step-1 heuristic.
- **Scope note (2026-07-15):** the content-based source (`--base-content` /
  `film_base.source = "content"`) was **reassigned out of this task** to the new
  `film-base-content-fallback` task (see the authoritative "Scope change" note
  below). I had implemented it here; it has since been **removed** from this
  worktree — enum variant, flag + wiring, content-estimate logic, report shape,
  and its tests are gone. What remains of content mode here is a **one-line
  cross-reference**: the auto-refusal error message *suggests* `--base-content`
  (naming the owning task) and never silently falls back to it.
- **Done (kept scope).** `pipeline/film_base.rs` rewritten around an inward-scan
  detector, plus the two same-family items retained by the scope change: the
  `--base-region` uniformity warning, and `nc inspect` candidate regions. Whole
  task is pure functions; `cli`/`stages` only ferry warnings.
- **Detector shape** (`rebate_candidates` + `select_auto_base`; `auto_estimate`
  is their composition):
  - Per edge, march 1-px strips inward, up to `REBATE_SCAN_FRAC = 10%` of the
    short side (min 3 px). Strips are **trimmed by the scan depth at both
    ends**, otherwise the perpendicular edges' holder margins contaminate every
    strip (the reason the probe strips in the original verification looked
    dirty at the corners).
  - Per strip: per-channel p97 (`SAMPLE_PERCENTILE`) + worst-channel relative
    spread `(p97−p10)/p97`. Classes: **holder** (all channels p97 <
    `HOLDER_MAX_TRANSMISSION = 0.05`; real holder ≈ 0.01, dimmest real rebate
    channel ≈ 0.14, so 0.05 splits with margin), **uniform** (all-channel
    spread ≤ 0.15 — kept the strict all-channel gate), else **other**.
  - A candidate band is the **first** run of ≥ `MIN_BAND_STRIPS = 2` uniform,
    value-continuous strips (adjacent-strip step ≤ 10% per channel — this is
    what stops the band merging into an adjacent flat picture region) sitting
    **immediately behind a contiguous holder run**; the whole band is then
    re-measured as one region and must pass the spread gate again (catches slow
    drift). Bands at depth 0 (no holder outside) are rejected.
  - Selection: candidates must beat the frame-interior **median** on **every**
    channel by ≥5% (`INTERIOR_BRIGHTNESS_MARGIN`, replacing the old lenient
    any-channel/2% gate); the **brightest** survivor wins.
- **Key decisions and why:**
  - **The corroborating anti-bright-surround signal is holder-backing, not
    mandatory cross-edge agreement.** A bright surround bleeding to the frame
    edge has no dark holder outside it → no candidate → refusal. Mandatory
    cross-edge agreement was rejected because a real rebate legitimately appears
    on a single edge (verified: `48bit-full/2` left-only). Cross-edge
    *disagreement* between surviving candidates is a report **warning**
    (`--strict`-promotable), not an error.
  - **"Brightest candidate wins" is physically grounded:** the rebate is Dmin =
    per-channel max transmission; no genuine picture area can out-bright clean
    base. This is also why a uniform dark band behind the holder can never
    out-rank a real rebate (unit-tested).
  - `estimate` now returns **`BaseEstimate { base, warnings }`**; the region
    path's uniformity check emits a warning (never alters the value), rides
    `Rendered.film_base_warnings` through `stages::render` into the report, and
    `--strict` refuses it (e2e-tested).
  - `percentile` switched from full sort to `retain(finite)` +
    `select_nth_unstable_by` (O(n), still deterministic — an order statistic is
    tie-order independent).
  - `inspect` now reports `base_candidates` (edge, `--base-region`-ready rect,
    value, spread) even when selection refuses, so users confirm a rectangle
    instead of measuring one.
- **Verification:** unit tests in `film_base.rs` (single/two-edge rebate,
  bright-surround refusal naming the recovery flags, dark-band out-ranking,
  disagreement warning + within-tolerance no-warning, mixed-region warning with
  unchanged value, degenerate-region-base rejection, candidate serde-shape +
  rect round-trips through `Region` incl. the mirrored bottom-edge arithmetic,
  plus the retained explicit/region/percentile suite); e2e test (mixed
  `--base-region` warning + `--strict` refusal on both `convert` and `estimate`).
  Full gate clean on the rebased base: fmt / clippy `-D warnings` / build /
  **145 unit + 19 e2e**.
- **Post-review pass (2026-07-14, pr-review-toolkit 5-agent review of the
  working-tree diff; findings fixed):**
  - **Degenerate base rejected at birth** (type-design + silent-failure, HIGH):
    `estimate` now runs `guard_base` over every source, erroring loudly on any
    zero / negative / non-finite channel. Previously a region on the holder could
    return a poison base with exit 0 and no warning — `nc estimate` has no
    downstream algo to catch it. Closes the "reject degenerate bases at birth"
    follow-up noted in the CLAUDE.md film-base gotcha (per-algo guards stay as
    defense-in-depth); CLAUDE.md updated to match.
  - **Estimation moved out of `stages::render`** (silent-failure, MEDIUM):
    `run_convert` now resolves the base and pushes its warnings *before* the
    fallible render, so a downstream render error can't swallow the "non-uniform
    region" line explaining a bad base. `render` takes a resolved `&FilmBase`;
    `Rendered` lost its `film_base`/`film_base_warnings` fields (the orchestrator
    owns them). This also tightens the stage split (estimation = stage 2,
    render = stages 3–4).
  - **`nc estimate --strict`** (silent-failure, MEDIUM): the base-producing
    command now promotes its warnings to a failing (non-zero) exit, so a script
    baking a Dmin into a recipe short-circuits on a plausible-looking-but-bad
    region. The report (including the warnings) still emits *before* the gate —
    matching `convert` — so the signal is the non-zero exit code, not a suppressed
    value; a consumer must gate on the exit code. Makes the `BaseEstimate`
    "`--strict` promotes" doc true on every warning-producing path.
  - **Minor:** unified auto-refusal recovery wording into one `RECOVERY_ADVICE`
    const (all refusals, incl. the too-small case, *suggest* `--base-content` as
    the owned-elsewhere fallback); doc fixes (candidates are pre-brightness-gate;
    `percentile` is rounded-rank not nearest-rank; `select_auto_base` names the
    5% margin + same-image contract). Declined: `base_candidates: Some(vec![])`
    for "ran, found nothing" — the adjacent "unavailable" warning already
    disambiguates; not worth the shape change. Review came back clean after the
    fixes.
- **Rebased onto origin/main `3c7f5bd` (2026-07-15)** to pick up #20's
  `--out-depth`→`--output-hdr` rename, the merged bw-support docs (#19/#21), and
  the #22 scope-change note. Conflicts resolved in `design-spec.{md,html}` and
  `stages.rs` (the render test now uses `hdr: true`, not `OutDepth::F32`); then
  the content-source removal above was applied on the new base.
- **Real-scan status:** the full-size scans (`../nc-assets`, `~/Pictures/scan`)
  are **not present in this environment** — only the committed 502×462 fixture
  crops, which are picture-interior crops (probe: all strips high-spread, no
  holder). On them the detector correctly refuses and the region-warning behaves
  as designed. Thresholds are set from the numbers recorded in the
  `film-base-estimation` real-scan verification (holder ≈0.01, rebate
  ≈[0.53,0.26,0.16], picture spread ≫0.15); **running the detector's happy path
  on the full-size scans still needs doing — fold it into
  `real-scan-verification`** (its task already covers default-output checks).
  Note the follow-ups `ir-holder-detection` and `auto-base-neutral-stock` layer
  on this: the detector deliberately uses **no** orange/colored-base assumption
  (holder-backing + flatness + brightness are color-independent), so a
  near-neutral base (Harman Phoenix, R/B ≈ 0.84) does not break the confidence
  gates.
- **Notes for dependents:**
  - `white-holder-support`: the polarity assumption lives in exactly two spots —
    `StripClass::Holder` classification (`HOLDER_MAX_TRANSMISSION`) and the
    doc'd "holder-backing" rationale. A `film_base.holder = white` knob should
    flip the holder test to "very bright on all channels" (and then the
    "brightest survivor" rule needs care: a white holder is brighter than the
    rebate, but it sits *outside* the band, so selection logic is unchanged —
    only classification flips). `Edge`/`RebateCandidate` are already public.
  - `estimate-reuse-output`: `BaseEstimate.warnings` and
    `Report.base_candidates` are the hooks for the reuse-ready output; the
    candidate `region` is already `--base-region`-shaped.

### Scope change — content-based source reassigned (2026-07-15)

A design pass (Phoenix/Ektar real-scan verification + workflow discussion) moved
work out of this task. The task file couldn't be edited during the pass (agents
active on it), so this note is the authoritative redirect for whoever picks it up.

**Remove from scope — the Content-based source bullet.** The "Also in this task's
scope" section lists a **Content-based source (ladder Tier 3)** item — the
`film_base.source = "content"` variant, the `--base-content` flag, per-channel
high-percentile of exposed content, its report wiring, and its tests. **That is
now owned solely by the new `film-base-content-fallback` task**
(`docs/tasks/film-base-content-fallback.md`). Drop it from this task's
implementation *and its verification* so the two tasks don't both build the same
enum/flag/report/tests.

**Keep in scope (unchanged):** the inward-scan detector, the **uniformity warning
on `--base-region`**, and **`nc inspect` reporting candidate rebate regions**. The
only remaining content-mode responsibility here is a **one-line cross-reference**:
when auto-detection refuses, the failure message should *suggest* `--base-content`
(never implement it or silently fall back).

**Two follow-ups now layer on top of this task (no action needed here, but read
them so you don't bake in assumptions they'll have to unwind):**

- `ir-holder-detection` — uses the IR plane to mask the holder (0–4 edges)
  content-independently, feeding the RGB rebate search; may replace the RGB-only
  holder-classification step where IR is present. Largely sidesteps
  `white-holder-support` (opacity, not color, is the IR signal).
- `auto-base-neutral-stock` — hardens detection for near-neutral bases (Harman
  Phoenix, R/B ≈ 0.84) where base color isn't a usable discriminator. Real-scan
  verification found opposite bases across stocks (Ektar orange R/B 2.73, Phoenix
  neutral 0.85), so any confidence gate that assumes a colored/orange base needs a
  color-independent corroborator (flatness / geometry / cross-frame value
  agreement). **Don't hard-code an orange-mask assumption.**

Both are tracked in `TASKS.md` as dependents of this task.

## white-holder-support
**Status:** not started
**Updated:** —

- Goal: support scans made in light/white film holders, where the current
  darker-than-interior assumptions of base estimation don't hold.

## estimate-reuse-output
**Status:** done
**Updated:** 2026-07-14

- Goal: `nc estimate` output shaped for direct reuse (drop-in recipe fragment /
  flag values), closing the measure-once-reuse-for-the-roll loop.
- **Done.** Full CI gate clean (`fmt --check`, `clippy --all-targets -D
  warnings`, `build`, `test`); suite is **208 tests** (182 unit + 26 E2E; the
  counts include tests other tasks added under the same base).
- **Rebased onto `origin/main` 94fdc12 (2026-07-16).** Post-merge of #23
  (`auto-base-redesign`: inward-scan rebate detector — `estimate` now returns
  `BaseEstimate { base, warnings }`, plus `RebateCandidate`/`Edge`/`guard_base`)
  and #27 (`algo-sigmoid`). Conflicts in film_base.rs, cli.rs, tests/pipeline.rs,
  and both design-spec files, all reconciled keeping BOTH sides:
  - **film_base.rs:** my `estimate_grid`/`GridEstimate`/`GridCell` now sit on the
    new detector. Grid cells switched from the old `sample_region` (which now
    returns `BaseEstimate`) to `sample_region_at(_, SAMPLE_PERCENTILE)` (still
    `FilmBase`). Dropped the two obsolete `auto_fails_*` unit tests that #23
    deleted; kept all grid tests.
  - **cli.rs:** `run_estimate`'s non-grid branch adopts the `BaseEstimate` API
    (`est.base` + folds `est.warnings`); grid branch and reuse-ready output
    unchanged. `Report` keeps both #23's `base_candidates` and my
    `film_base_flag`/`film_base_recipe`/`grid`. Deduped a doubly-added
    `EstimateArgs.strict` (both #23 and I added `--strict` to estimate) into one
    field with a merged doc.
  - **tests:** kept #23's `mixed_base_region_warns_and_strict_refuses_it` plus my
    two estimate tests. Dropped the `--strict` from the reuse round-trip test:
    the new inward-scan uniformity gate warns on any `--base-region` of this
    real-photo fixture (no region-uniform patch exists), so a clean-strict-exit-0
    assertion isn't expressible here — `estimate --strict` is covered by #23's
    mixed-region test instead.
  - **docs:** subcommand table keeps #23's richer `inspect` row + my `estimate`
    row; the §8 ladder Tier-1 keeps #23's inward-scan detail with my
    `nc estimate --grid` reference.
- **Rebased onto `origin/main` 3c7f5bd (2026-07-15).** Post-merge of #20
  (`--out-depth u16|f32` → the boolean `--output-hdr`), #21 (bw-support docs),
  and #22 (roll-workflow/versioning follow-up tasks + acquisition ladder). Only
  code change from the rebase: the two `convert` round-trip invocations in the
  reuse E2E test switched `--out-depth f32` → `--output-hdr` (both still set
  `output.hdr = true`, so the byte-identical A/B comparison holds). My
  estimate/grid/reuse surface does not touch `OutDepth`, so cli.rs merged
  cleanly. Design check against the new tasks: my Tier-1 `estimate`
  reader + `film_base_flag`/`film_base_recipe` are exactly what
  `base-acquisition-planner` (Tier 1) consumes and `roll-conversion` applies —
  complementary, no overlap; I use only `Explicit`/`Region`/`Auto` sources, not
  the `--base-content`/`FilmBaseSource::Content` owned by
  `film-base-content-fallback`; and I touch no `Dmax` surface (owned by
  `dmax-reference`). §8/§9 grid + reuse wording sits inside the existing
  acquisition ladder without duplicating the new task specs.

### What was built
- **Reuse-ready report fields (`cli.rs`).** The `estimate` report now carries,
  beside the raw `film_base` measurement:
  - `film_base_flag` — a paste-ready `"--film-base R,G,B"` string. Values are
    formatted with `f32`'s `Display` (shortest round-tripping decimal), so
    parsing them back yields the **bit-identical** measured `f32`s.
  - `film_base_recipe` — the same measurement as a `FilmBaseParams` fragment,
    serializing to the documented `{"source":{"explicit":[r,g,b]}}` shape; it
    parses back both standalone and as a recipe's `film_base` section
    (unit-tested against `deny_unknown_fields`).
  - Both are attached **only when the measurement passes the same
    explicit-base validation `convert` applies** (each channel in `(0, 1]`).
    A degenerate measurement (e.g. a region on the dark holder sampling ~0)
    is still reported as `film_base`, but a warning explains why no
    reuse-ready output was emitted — "reuse-ready" therefore implies "will be
    accepted by convert".
- **Grid calibration mode (`estimate --grid`, `pipeline/film_base.rs`).**
  `estimate_grid(image, rect)` samples a fixed 5-cell grid (top-left,
  top-right, bottom-left, bottom-right, center; each cell 25% × 25% of the
  rectangle, ≥1 px, same 97th percentile as single-region sampling) over the
  full frame or `--base-region`. Returns `GridEstimate { base, cells, spread,
  tolerance, agreement }`:
  - combined `base` = per-channel **median** across cells (robust to one bad
    cell, deterministic);
  - `spread` = per-channel `(max − min) / max` across cells, judged against
    the documented `GRID_MAX_RELATIVE_SPREAD = 0.05`;
  - disagreement is **diagnostic, not fatal**: the CLI pushes a report warning
    naming the spread/tolerance and pointing at the per-cell evidence in
    `grid.cells` (never averaged away silently).
- **`--strict` on `estimate`.** Promotes any estimate warning (grid
  disagreement, decode notes, unusable-base) to exit 1 *after* the report is
  emitted — same contract as `convert`.

### Key decisions / notes for dependents
- **`--grid` is an estimate-only CLI mode, not a recipe key.** It configures a
  *measurement/diagnostic* of the `estimate` command (like `--report`), not a
  conversion knob: `convert` never grid-samples — the workflow is measure once
  with `estimate`, then freeze the explicit value via the emitted flag/fragment.
  So the four-spot knob wiring (Overrides/Params/merge/validate) deliberately
  does not apply; the recipe surface is unchanged. clap-conflicts with
  `--film-base` (nothing to sample) and `--auto-base` (grid replaces border
  detection); compatible with `--base-region` (grid within the rectangle).
- **Report `film_base_source` under `--grid`** is the overall rectangle sampled
  (`{"region":[x,y,w,h]}`, full frame when no `--base-region`); the new `grid`
  report object documents the per-cell method. No new `FilmBaseSource` variant —
  grid never enters the convert/recipe surface.
- `GridEstimate`/`GridCell` live in `pipeline/film_base.rs` (Serialize-only,
  embedded in `Report` like `DecodeInfo`), keeping report-shape types beside
  the stage that produces them.
- Verified on the committed real fixtures: `estimate --base-region 0,0,60,60`
  emits flag + fragment; full-frame `--grid` on a real (non-blank) frame
  disagrees as expected (spread ≈ 0.42–0.56 ≫ 0.05) → warning, exit 0, and
  exit 1 under `--strict`. E2E round-trip test feeds both the flag string and
  the fragment (as `--params` recipe) back into `convert` and asserts the same
  base and **byte-identical outputs** between the two reuse forms.
- Docs: design-spec §8 (estimate example now shows the real report shape and
  the grid mode), §9 ladder tier 1 + Global `--strict`, roadmap item 10 marked
  shipped — `.md` and `.html` edited together.

### Review
- Ran `pr-review-toolkit:review-pr` (code / tests / silent-failure / type-design
  / comments) — 1 full round + 1 confirmation round; confirmation came back
  clean.
  - **Fixed from round 1:** extracted the reuse-output computation into the
    pure `reuse_ready(rgb) -> Option<(String, FilmBaseParams)>` so the
    degenerate-suppression branch is unit-testable (and tested);
    `GridEstimate.cells` tightened from `Vec<GridCell>` to `[GridCell; 5]`
    (expresses the fixed layout at compile time, identical JSON, removes a
    latent wrong-median hazard if the count ever changed); the grid warning
    now distinguishes a *degenerate sample* (combined base channel ≤ 0) from
    genuine *disagreement* (light leak/falloff/dust) so the diagnostic names
    the actual problem; doc qualifiers (reuse fields are conditional; derived
    grid fields; spread-sentinel ambiguity); added tests — all-black-frame
    spread sentinel (guards against `0/0 = NaN` → `null` in the report),
    clean `estimate --strict` exits 0, grid runs emit the reuse fields.
  - **Deliberate, not fixed:** reuse fields are still emitted when a grid
    *disagrees* but the combined base is valid — the median is designed to
    resist one bad cell, the disagreement warning is loud, and `--strict`
    fails the run; scripted consumers that want hard safety use `--strict`.
  - **Disputed/deferred:** a plausibility floor for tiny-but-positive bases
    (e.g. `0.002` from a region on the dark holder) was suggested (silent-
    failure review, MEDIUM). Deferred: any threshold is arbitrary (a dense
    orange mask legitimately scans B ≈ 0.03 on real fixtures), the emitted
    value *is* accepted and correctly processed by `convert`, and design-spec
    roadmap item 8 (`auto-base-redesign`) already owns `--base-region`
    plausibility/uniformity warnings — recorded there rather than inventing a
    magic constant here.

### Post-merge review fix (2026-07-17) — grid degenerate base now hard-errors
Six-reviewer pass (Codex + 5 lenses) on the reuse-output change found one
warranted correctness gap: the `--grid` path only *warned* on a degenerate
combined base (any channel non-finite or ≤ 0 — e.g. `--grid --base-region` on the
dark holder) and exited 0 unless `--strict`, while the single-measurement path
hard-errors on the identical condition at birth via `film_base::estimate`'s
finite-and-positive guard. That asymmetry violated the CLAUDE.md film-base gotcha
and design-spec §11 fail-loudly.
- **Fix:** `cli::run_estimate` now, in the `--grid` branch, hard-errors after
  `emit_report(...)` when the combined base has any non-finite or ≤ 0 channel —
  **regardless of `--strict`** — using the same `NcError::Other` (exit 1) the
  single-path guard returns, so both estimate paths map a degenerate base to one
  exit code. The per-cell "grid measured non-positive transmission…" warning
  still rides the emitted report as diagnostics; the diagnostic report (with
  `grid.cells`) lands on stdout *before* the gate, same emit-before-gate contract
  as the `--strict` check.
- **Docs:** design-spec §8 (grid behavior) and §11 (exit code) updated in both
  `.md` and `.html` to state `--grid` errors (exit 1) on a degenerate combined
  base regardless of `--strict`.
- **Tests added:** `film_base.rs` — odd, non-square grid rect (cell sizing /
  center-origin arithmetic + in-bounds), single-channel disagreement drives the
  agreement verdict, and a note-pinning test that `estimate_grid` itself reports
  (not errors) a degenerate base (the hard error is the caller's job).
  `tests/pipeline.rs` — e2e `estimate --grid` on an all-black frame exits 1
  without `--strict`, matching the single-path degenerate exit, with the report
  emitted first. New committed fixture `tests/fixtures/black-48bit.tif` (64×64
  all-zero 16-bit RGB) supplies the all-black input (generated once via a
  throwaway in-crate generator, since integration tests can't reach the `tiff`
  crate).

### Post-merge follow-ups (2026-07-17) — type-design + doc from the same review
Three further items the user decided after the review:
- **(doc, no behavior change) Reuse output survives grid disagreement.** Made
  explicit that a cells-disagree *warning* does NOT suppress the reuse-ready
  output — the combined median resists a single bad cell, so the base is still
  offered; consumers check `warnings` or run `--strict`. Documented in
  design-spec §8 (md+html) and at the `reuse_ready` emission site in
  `run_estimate`. Only a *degenerate* base withholds the reuse forms (and it is a
  hard error).
- **(Type #1, done) Collapsed the parallel reuse `Option`s.** `Report`'s
  `film_base_flag: Option<String>` + `film_base_recipe: Option<FilmBaseParams>`
  (illegal flag-without-recipe was representable) became one
  `reuse: Option<ReuseReady>` where `struct ReuseReady { flag, recipe }` — both-or-
  neither is now unrepresentable (the CLAUDE.md parallel-`Option` anti-pattern).
  Wire shape is **byte-identical**: `#[serde(flatten)]` on the `Option` plus
  `#[serde(rename = "film_base_flag" / "film_base_recipe")]` on the struct fields
  keeps the two flat top-level keys (present together / both absent); the `reuse`
  wrapper name never appears on the wire. Safe because `Report` is serialize-only
  (no `Deserialize`/`deny_unknown_fields`, so no flatten conflict). Added a
  snapshot unit test (`report_reuse_flattens_to_flat_keys_or_nothing`) locking the
  shape; existing round-trip/e2e tests pass unchanged.
- **(Type #2, deferred to a task) `GridEstimate.agreement: bool` limitation.**
  Documented on the type that `agreement=false` conflates *disagree* vs
  *degenerate* and that the `spread` `1.0` value is an overloaded sentinel, so the
  CLI re-derives the case from the base. Kept as-is for now; created
  `docs/tasks/grid-verdict-enum.md` (replace the bool + sentinel with a
  `GridVerdict { Uniform | Disagree | Degenerate }` enum; deps
  `estimate-reuse-output`, `film-base-estimation`; post-MVP) and wired it into
  `TASKS.md` (Mermaid graph, dependency list, Phase 5 checklist as `[ ]`).

### Follow-ups / deferred (with reasons)
- Grid tolerance (0.05) is a documented constant, not a flag — make it a knob
  only if real blank-frame scans show legitimate falloff above 5%
  (`real-scan-verification` can inform this).
- `inspect`'s suggested-Dmin output does not carry the reuse fields (kept
  estimate-only per the task scope; trivial to add later if wanted).
- `--base-region` plausibility (dark-holder detection / uniformity warning)
  stays with `auto-base-redesign` (roadmap item 8), which owns that diagnostic.
- If grid sampling ever becomes a `convert`-usable source, it must join
  `FilmBaseSource` as a variant (one enum per the conventions), not a bool
  beside it.

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

## algo-sigmoid
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

## real-scan-verification
**Status:** not started
**Updated:** 2026-07-21

- Goal: run the verification matrix (inspect/estimate/convert/IR/determinism/
  resources) against the full-size real scans once the user prepares the assets;
  record results here, file follow-up tasks for defects.
- 2026-07-21: Narrowed this to the current TIFF pipeline so full-size resource
  measurements can run before the HDR/display roadmap and can inform the
  `streaming-tiled-io` go/no-go. Final preset and cross-device checks moved to
  `display-output-acceptance`.

## perf-instrumentation
**Status:** parked (superseded by `perf-telemetry`)
**Updated:** 2026-07-15

- Original goal: per-stage timings in the JSON report, tracing spans to stderr
  behind `-v`, and criterion benches for the hot kernels — local-only,
  report-side (byte-identical output untouched). Pre-release performance
  visibility.
- **Parked, not merged.** On review (2026-07-15) we decided the LAB
  micro-benchmark framing answers the wrong question: we don't primarily want to
  bench kernels on a synthetic image in a controlled setting — we want to know how
  `nc` behaves **in the real world** on the user's actual scans, emit that as
  machine-readable metadata, and eventually ship it to a server. That is now
  `perf-telemetry` (below).
- The prototype is preserved on branch `prototype/perf-bench-instrumentation`
  (see its `docs/prototypes/perf-bench-instrumentation.md`). Reusable parts (the
  per-stage `Instant`-pair timing in `stages::render` + orchestrator) were lifted
  into `perf-telemetry`; the criterion benches, the lib/bin split, and the
  `tracing` spans were **not** brought over.

## perf-telemetry
**Status:** done
**Updated:** 2026-07-15

- Goal: embedded, opt-in performance + context telemetry for `nc convert` — after
  a real conversion, collect a **full** metadata record (image + per-stage timing
  + run context + outcome) and emit it as JSON to a persistent local JSONL log
  and/or a one-off file. No new subcommand/entrypoint. Groundwork for a future
  background uploader (`telemetry-upload`).
- **Why real-world, not lab:** decided with the user (see the parked
  `perf-instrumentation` note). Telemetry is embedded in the normal run, opt-in
  via a flag, and describes actual conversions — not a separate benchmark tool.
- **User-approved decisions honored:** sink = **BOTH** (a persistent append-only
  JSONL log AND an optional per-run file); record scope = **FULL**.
- **Flag surface (operational, NOT recipe keys):** `--telemetry` (append to the
  JSONL log) and `--telemetry-file <path>` (`-` = stdout; overwrites a one-off
  file); may be combined. Collected iff at least one is present. These are on
  `ConvertArgs` directly — **not** in `ResolvedConfig`/`*Params`/`merge`/`validate`
  (they're operational like `--report`, and must never touch the recipe/sidecar).
- **Default log path (dependency-free):** `NC_TELEMETRY_LOG` overrides, else
  `$XDG_DATA_HOME/nc/telemetry.jsonl`, else `$HOME/.local/share/nc/telemetry.jsonl`
  (Unix) / `%APPDATA%\nc\telemetry.jsonl` (Windows). Chose a hand-rolled XDG
  resolver over the `directories` crate to honor the house minimal-deps rule (the
  task explicitly called this acceptable). No new crate was added.
- **Record schema (`schema_version` 1, serialize-only):** `timestamp_ms`
  (epoch ms via `SystemTime`; unit in the name, no date crate), run context
  (`nc_version`, `target`, `cpu_count`), `image` (format/dims/megapixels/bit_depth/
  channels/ir_present/input_bytes/output_bytes), `timing_ms` (total + decode/
  film_base/algorithm/color/encode, and ir_export only when it ran), `conversion`
  (algorithm, `params_hash` = FNV-1a over the effective recipe JSON, film_base_source,
  dmax when applied, output_hdr), `outcome` (warnings/clipped/non_finite — no
  `success` flag; see the round-2 note below). See design-spec §9 for the shape;
  both `.md` and `.html` updated.
- **`target` triple:** added a dependency-free `build.rs` that re-exports Cargo's
  build-script `TARGET` as `NC_TARGET`, read at runtime via `env!("NC_TARGET")`.
- **Determinism boundary (verified):** telemetry is emitted last (after the output
  TIFF + sidecar are written) and only reads their facts. Per-stage timings ride a
  report-only channel (`stages::StageTimings` on `Rendered` + orchestrator
  `Instant` pairs), never serialized into the sidecar. The e2e
  `telemetry_does_not_perturb_output_or_sidecar` test asserts byte-identical TIFF
  **and** sidecar with telemetry on vs off.
- **Fail-soft (documented deviation from fail-loudly):** a telemetry *write*
  failure is warned on stderr and never fails the run; kept out of
  `report.warnings` so `--strict` can't promote it (the image already succeeded).
  A `--telemetry-file` path *collision* with a real artifact is the exception — a
  config error, caught up front by the existing collision guard (exit 2).
- **`-`/stdout caveat:** `--telemetry-file -` writes the compact line to stdout;
  since the report is on stdout by default, pair it with `--report none`/
  `--report-file` when a parser consumes stdout (documented on the flag + in §9).
- **Tests:** unit (record-builder fields, missing-IR omits `ir_export`, stable
  `params_hash`, JSONL append vs one-off overwrite) in `src/telemetry.rs`; e2e
  (full record on a fixture, ir_export timing, one-line-per-run, both sinks, the
  determinism invariant, fail-soft under `--strict`, collision usage error) in
  `tests/pipeline.rs`. Real-scan spot check ran the release binary on the
  committed real-scan fixture (full-size assets aren't in this environment):
  502×462, 0.2319 MP, per-stage ms populated, dmax 1.6195, clipped count matched
  the report warning.
- **Notes for `telemetry-upload`:** the JSONL log is the queue to drain (one
  object per line, crash-safe append). Upload must stay off the conversion
  critical path, honor an `NC_TELEMETRY=0`-style off switch (design-spec §12), and
  key ingestion off `schema_version`. Records already carry no pixels and no file
  *paths* — keep that invariant.

### Round-2 review fixes (2026-07-17, uncommitted — Codex + 5 pr-review agents)
- **[Codex P2] Atomic JSONL append.** `append_jsonl` now builds the record line +
  its `\n` into one buffer and emits it with a single `write_all` to an `O_APPEND`
  handle. On POSIX an append write below `PIPE_BUF` (4 KiB; a record is far
  smaller) is atomic, so two concurrent `--telemetry` runs sharing a log can't
  interleave a body with another's newline. `writeln!` (two writes) forfeited that.
  Added `append_jsonl_is_atomic_under_concurrency` (8 threads × 200 appends, every
  line must parse; count exact).
- **[tests] `outcome` wiring pinned end-to-end.** New e2e tests:
  `telemetry_outcome_reports_clipping_and_warnings` (+12-stop exposure ⇒
  `outcome.clipped > 0` and `warnings >= 1`) and
  `telemetry_outcome_counts_ir_ignored_warning` (HDRi w/o `--export-ir`, f32 out ⇒
  `clipped == 0`, `warnings >= 1`) — proving `report.warnings.len()` /
  `clipped_total()` actually flow into the record, not just type-check.
- **[tests] Flags are operational, not recipe keys.** New
  `telemetry_key_in_recipe_is_rejected`: a `--params` recipe with a `telemetry` key
  is rejected exit 2 by `deny_unknown_fields`, no artifact written.
- **[type-design] `build_record` is now fully pure.** `timestamp_ms` and
  `cpu_count` are injected via `RecordInputs`; the orchestrator does the ambient
  reads (`telemetry::now_unix_millis` / `telemetry::cpu_count`), mirroring
  `default_log_path`→`resolve_log_path`. Only compile-time constants
  (`CARGO_PKG_VERSION`, `NC_TARGET`) remain in the builder.
- **[type-design] Dropped `OutcomeInfo.success`.** It was a hardcoded `true` that
  carried no information and could contradict `non_finite > 0`. A `success`/`status`
  field returns with the failure-path record in `telemetry-strategy`/
  `telemetry-upload`, where it actually varies. **`schema_version` stays 1** — the
  feature is unreleased, so no record with the old shape exists in the wild and
  there's no ingestion compat to preserve. SKILL + design-spec (md+html) + record
  example + task schema all updated to match.
- **[silent-failure] `Log::warn_always`.** Added a helper that prints
  `nc: warning:` regardless of `--quiet`; `emit_telemetry`'s `warn` closure now
  delegates to it, removing the duplicated format string and the fragile coupling
  to `Log::warn`'s internal quiet-gating.
- **[comments/docs] `--dump-params` is a file writer, not stdout.** Corrected the
  stdout-writer list (accurate set: `emit_report` + `nc params`) in design-spec
  (md+html), `TASKS.md`, and `stdout-broken-pipe-safety.md`.
- **[docs LOW] Log-path precedence + Option contract.** Fixed the task file's
  default-log-path order (APPDATA before the HOME fallback, matching
  `resolve_log_path`); fixed the SKILL's jq fallback to honor `XDG_DATA_HOME`
  first; documented the omitted-vs-null Option contract on `TelemetryRecord`.
- **DEFER (not done here):** the default stdout **report** (`emit_report`'s
  `println!`) panicking on a broken pipe is the pre-existing
  `stdout-broken-pipe-safety` task, out of scope for perf-telemetry.

### Rebase onto origin/main + algo-sigmoid interaction (2026-07-17, uncommitted)
- **Rebased** the branch onto `origin/main` (now carrying `algo-sigmoid` #27 and
  `auto-base-redesign` #23). Conflicts in `src/pipeline/stages.rs` and `src/cli.rs`.
- **Reconciliation:** `auto-base-redesign` moved film-base estimation OUT of
  `stages::render` (the orchestrator now resolves the base and passes `&FilmBase`,
  so estimate warnings surface before the fallible render). So the film-base
  **timing** moved with it: `StageTimings` now carries only `algorithm_ms` +
  `color_ms` (the two stages `render` still runs); the orchestrator measures
  `film_base_ms` around its own `film_base::estimate` call (like `decode_ms` /
  `encode_ms`) and folds it into the telemetry `TimingInfo`. Kept `algo-sigmoid`'s
  `--algorithm sigmoid` warning (density-gamma no-op) alongside the telemetry
  decode-timing line in `run_convert`.
- **Sigmoid telemetry check (verified):** a `--algorithm sigmoid` conversion
  produces a sane record — `conversion.algorithm` = "sigmoid", a resolved `dmax`
  (sigmoid shares the density anchor), all per-stage timings populated, and
  `params_hash` covers the `sigmoid.*` recipe keys (changing `--sigmoid-contrast`
  changes the hash, since the hash is over the effective recipe JSON). Added the
  e2e test `telemetry_records_sigmoid_algorithm_and_params_hash`.

### Round-3 review fixes (2026-07-17, uncommitted — Codex + 5 pr-review lenses)
- **[Codex P2] Case-only telemetry-file/output collision.** `-o out.tiff
  --telemetry-file OUT.TIFF` on a case-insensitive FS (macOS/Windows default) was
  NOT rejected: with neither file pre-existing, `collision_key` can't canonicalize
  to a shared casing and `ensure_write_targets_distinct` compared the keys with a
  case-sensitive `==`, so the guard passed and the telemetry write clobbered the
  just-written TIFF (exit 0). Fix: new `keys_collide(a, b)` helper comparing exactly
  OR ignoring ASCII case (`to_string_lossy().eq_ignore_ascii_case`), used for both
  the input-key and seen-set checks. Conservative over-reject (can't cheaply detect
  per-volume case sensitivity; false-rejecting a case-only pair in one invocation is
  a harmless annoyance vs. silently clobbering the output). Doc comments on
  `collision_key`/`ensure_write_targets_distinct` updated. Unit tests:
  `keys_collide_is_case_insensitivity_aware` and
  `write_targets_reject_case_only_collision_before_creation`.
- **[tests] Strengthened `append_jsonl_is_atomic_under_concurrency`:** payloads now
  padded past a 4 KiB page (was ~30 bytes) so an interleaved pre-fix two-write would
  actually corrupt a line, per-thread pad char distinct so a splice shows up as a
  JSON parse failure.
- **[type-design] Wire-shape snapshot test `record_wire_shape_is_pinned`:** pins the
  exact serialized JSON for a fully-populated and a minimal record (fixed
  `nc_version`/`target` literals), so any field/order/foreign-enum drift that should
  bump `SCHEMA_VERSION` fails a test.
- **[comment] `append_jsonl` atomicity rationale corrected:** the guarantee is
  `O_APPEND` offset-then-write atomicity on a local FS, not the `PIPE_BUF` bound
  (which governs *pipe* writes) — reworded, with the distinction called out.
- **[docs] Stale/inaccurate references fixed:** `telemetry-strategy.md` no longer
  cites the dropped `outcome.success` field (a record's existence implies success);
  design-spec §12 item 17 emit_report consumer list now reads "convert/inspect/
  estimate" (md+html); design-spec §9 collision parenthetical now reads
  "(`NC_TELEMETRY_LOG` or the default path)" (md+html).

## External review triage — 7 findings → 7 tasks (2026-07-18, docs-only, uncommitted)

An external code review of the Step-1 codebase produced seven findings. Each was
**verified against the actual code** before acting (several claims were checked
with `tiffinfo`/`exiftool` on the real `../nc-assets` scans, `cargo build`, and
direct source reads); all seven held up. Per the user's direction the pass stayed
**docs-only** — every finding was turned into a tracked task rather than fixed in
place, since the working tree already held documentation edits and code changes
were to be scheduled, not mixed in. Result: `docs/TASKS.md` updated (Mermaid graph
+ dependency list + phase checklists) and seven new `docs/tasks/*.md` files. No
source, `Cargo.toml`, or `Cargo.lock` touched.

The tasks (all deps `[x]` ⇒ executable now, except where noted):

- `input-color-management` (Phase 6) — **input ICC → working space.** `InputColor::Auto`
  promises embedded/default-profile decoding but `Auto` ≡ `Linear` today (decode
  normalizes integers, every stage assumes linear Rec.709/D65; only `Profile` is
  rejected). Investigated with `exiftool`/`tiffinfo`: **all 26 real scans carry no
  embedded ICC profile and no colorimetry tags** (raw `Gamma=1` Plustek/SilverFast),
  while our own `converted/` outputs embed "sRGB built-in" — so this is a
  forward-looking fidelity feature (enabled once the user makes an IT8 scanner
  profile), not a fix for current output. One profile per scanner (device
  characterization), **not** per film roll; stock differences stay the density
  stage's job. Task uses lcms2 to build a source→working transform applied after
  decode; lifts the `--input-profile` rejection. Deliberately skipped the cheaper
  "honest default / fail-loud on embedded profile" option (pre-release).
- `density-safety-bounds` (Phase 6) — physical bounds on
  `density_scale`/`offset`/`gamma` (the sigmoid-bounds analogue density lacks;
  `validate` checks only finiteness/positivity) + a degenerate-output
  (histogram/dynamic-range collapse) **warning** catching the finite-all-black
  underflow the loss counters miss (`10^(γ·D')`: huge-negative density → finite
  `+0.0`, uncounted — acknowledged at `algo/density.rs:221-226`). Offset stays
  negative-capable (mask compensation) ⇒ magnitude cap. Warning needs a
  false-positive guard validated on real (legitimately dark) scans.
- `transactional-output-writes` (Phase 8) — artifacts written straight to final
  paths via `File::create`, sequentially; reproduced sidecar-fails-after-primary
  leaving an orphaned TIFF. Temp-write → fsync → rename. Framed as **honest
  "no partial artifacts + minimized window," not literal multi-file atomicity**
  (POSIX rename is per-file). Records the existing IR-before-primary mitigation.
- `memory-preflight` (Phase 8, Phase A of the memory review) — the 4 GiB decode
  limit guards only the u16 read buffer while derived peak is a multiple
  (u16+f32 decode, full-image clone in `to_output` incl. the never-transformed IR,
  quantize buffer; three full images can overlap — decoded `image` + algo
  `positive` + `to_output` clone ⇒ ~24 GiB, ~6× the 4 GiB input ceiling, and still
  ~16 GiB / two images after the in-place fix) unchecked. Adds a peak-memory preflight
  (one shared sizing model, operational `--max-memory`-style knob, fail-loud) and
  drops the `to_output` clone (transform in place, skip IR).
- `streaming-tiled-io` (Phase 8, Phase B, **evaluate-first**, gated on
  `memory-preflight` + `real-scan-verification`) — strip/tile decode + streaming
  encode. Opens with a **STEP 0 gate**: evaluate from measured peak whether it's
  needed at all; if data is insufficient, collect it first; proceed only if real
  scans exceed the budget. Default expectation: not needed yet (~18 MP ⇒ ~600 MB).
  Pushed back on committing to a full streaming architecture unmeasured.
- `dependency-hygiene` (Phase 8) — drop three unused crates (`image`,
  `kamadak-exif`, `palette`; **verified `cargo build --all-targets` succeeds
  without them** — `image` pulls a large codec tree) and unify the two `Algorithm`
  enums onto `types::Algorithm`, removing the dead `algo::mod::Algorithm` copy and
  its `#[allow(dead_code)]`. Pure cleanup. (Noted: `cargo` doesn't warn on unused
  *deps* by default, which is why CI missed them.)
- `release-readiness` (Phase 8) — (1) **doc-accuracy corrections** (do-first,
  independent): README still says "pre-implementation / coding hasn't started" +
  "Planned usage" (false); `TASKS.md` says "two algorithms" omitting `sigmoid`
  (three exist); obsolete `--out-depth f32` → `--output-hdr` in **two** task files
  (`real-scan-verification.md:32`, `pipeline-orchestration.md:49`); the research
  report's `citeturn…` tokens are **PUA-wrapped** (plain grep finds 0) and need
  delimiter-aware cleanup. (2) **productization**: license (**user decision** —
  none present), Cargo release metadata (all fields absent), supported platforms
  (lcms2-sys C-FFI cross-compile constraint), binary packaging (sequence after
  `real-scan-verification`).

**Deferred / not created:** the cheaper Option-1 honest-default for input color
(pre-release makes it moot — folded into `input-color-management` lifting the
rejection). **Open:** pick a first task — the doc-accuracy half of
`release-readiness` is the quickest, most user-visible win.

## roll-conversion
**Status:** done (implementation; user ships the checkbox)
**Updated:** 2026-07-19

- Goal: add a **roll workflow** — convert a batch of frames from ONE shared,
  frozen recipe so the whole roll is color-consistent and reproducible. This is
  the **batch-apply** half of plan→recipe→apply: it replays a *provided* frozen
  recipe over N frames, independent of how the recipe was produced. The
  auto-cascade that *generates* the recipe is the separate, dependent
  `base-acquisition-planner` task (NOT built here). Extends design-spec §12 item 6.

### What was built
- **New `nc roll` subcommand** (not a mode of `convert`) — see the decision below.
  `RollArgs` in `cli.rs`: positional `inputs` (files / directories / shell globs)
  **or** `--frames <manifest.json>`; required `-o/--out-dir <DIR>`; `--params`
  (the shared frozen recipe); `--strict`; the shared `ReportArgs`. Adds **no new
  recipe keys** — it reuses the existing `ResolvedConfig`/recipe shape; its flags
  are operational (like `--report`/telemetry).
- **Shared per-frame core `convert_frame`.** Extracted the decode → film-base →
  render → optional IR export → encode + sidecar block out of `run_convert` into
  a pure-of-orchestration `convert_frame(command, input, output, &cfg, &log) ->
  ConvertedFrame`. Both `run_convert` (single frame) and `run_roll` (per frame)
  call it, so **a roll frame's output is byte-identical to a single `convert`**
  with the same effective recipe (verified by a test that diffs the bytes).
  `run_convert`'s report emission, `--strict` gate, and telemetry stay in the
  orchestrator (telemetry is `convert`-only). Extracted
  `reject_unsupported_input_color` (the `input.color` profile guard) so both
  commands fail identically on a profile-bearing recipe.
- **Roll report** (`RollReport` on stdout / `--report-file`): `command:"roll"`, the
  shared frozen `recipe` **once** (its roll-fixed `film_base` / `density.dmax` live
  there, not repeated per frame), a `frames[]` list (`FrameReport`: input/output,
  `status` ok|failed, per-frame `film_base`/`dmax`/`white_balance`/`balance_range`/
  `loss`/`warnings`, the applied `overrides`, and `error` on failure), and a
  `summary { total, succeeded, failed }`. Emitted via a new generic `emit_json`
  helper (`emit_report` now wraps it).
- **Per-frame overrides** via the `--frames` manifest: each entry is
  `{ input, output?, params? }` where `params` is a *partial* recipe deep-merged
  (`merge_json`) onto the shared recipe's JSON, then deserialized back with
  `deny_unknown_fields` (so a typo'd override key is a loud error). This is the
  frame-local knob (e.g. per-frame `print.print_exposure`) and the shape
  `base-acquisition-planner` will emit.
- **Naming scheme:** default per-frame output is `<out-dir>/<input-stem>_positive.tiff`
  (sidecar `<...>.tiff.json` as usual); a manifest may set an explicit `output`
  (relative → joined onto out-dir, absolute → verbatim).
- **Determinism & safety:** positional inputs are sorted+deduped and directories
  expand to their sorted `.tif`/`.tiff` files, so frame order is deterministic. All
  per-frame outputs + sidecars (and `--report-file`) are collision-checked against
  every input and against one another up front (`ensure_roll_targets_distinct`,
  the multi-input analogue of `ensure_write_targets_distinct`) — a same-stem
  collision fails loudly (exit 2) before anything is written. `input.export_ir` is
  rejected in roll mode (one path, N frames would overwrite it).

### Key decisions / notes for dependents
- **Subcommand vs mode → new `nc roll` subcommand.** `convert` takes a single
  `input` positional + a single `-o <file>`; batch needs multiple inputs and an
  output *directory* with a naming scheme plus a differently-shaped roll report.
  Overloading `convert` would muddy its contract and risk its byte-identical
  guarantee. A separate subcommand keeps `convert` untouched and lets `roll` own
  its small operational surface while sharing the recipe machinery and the
  `convert_frame` core. **Single-frame `convert` output is unchanged** (all
  pre-existing convert/telemetry integration tests pass verbatim).
- **Config errors vs runtime errors.** A bad *shared* recipe or a bad per-frame
  *override* (bad merge, unsupported knob) fails loudly **up front** (exit 2/4)
  before any frame is converted. A per-frame **runtime** error (unreadable input,
  degenerate base) is **recorded** (`status:"failed"` + `error`) and the roll
  **continues**; the process then exits **1** with a summary on stderr — the report
  (emitted first) carries the per-frame detail. `--strict` promotes any frame's
  warnings to a failing exit after the report is emitted (convert's contract,
  aggregated).
- **Sequential, not parallel.** Frames are converted sequentially for simple,
  deterministic report ordering and logging. Per-frame output is independent, so
  `rayon`-parallelizing the loop is a safe future optimization (output bytes are
  unaffected); left out to keep the scaffold lean.

### Coordination notes
- **`dmax-reference` reconcile (trivial).** That parallel task changes
  `density.dmax` semantics + the default render. Roll treats `density.dmax` exactly
  as it exists on `main` today (it only carries the shared recipe's value through
  to `convert_frame`). When `dmax-reference` merges, the roll frozen-recipe handling
  of `density.dmax` needs no code change — the shared recipe simply carries whatever
  the new semantics define; only the `ROLL_RECIPE` test fixture's explicit
  `{"dmax":{"explicit":1.6}}` may want a value refresh if defaults shift.
- **`base-acquisition-planner` (the dependent).** It owns the **plan** step:
  detect the roll-fixed film base / `Dmax` once and *emit* the frozen recipe (and,
  for per-frame differences, a `--frames` manifest) that this `nc roll` then
  applies. The manifest shape (`{ frames: [{ input, output?, params? }] }`, partial
  `params` deep-merged per frame) is the intended hand-off contract.

### Verification
- CI gate clean in the worktree: `cargo fmt --all --check`, `cargo clippy
  --all-targets -- -D warnings`, `cargo build`, `cargo test` all pass. Suite is
  **305 tests** (252 unit + 53 end-to-end), +13 for this task.
- New end-to-end tests (`tests/pipeline.rs`, driving the compiled binary against
  the committed `tests/fixtures/{hdr-48bit,hdri-64bit}.tif`): batch from a
  hand-authored frozen recipe → per-frame outputs + sidecars + a roll report with
  the shared Dmin/Dmax once; **re-run is byte-identical**; a `--frames` per-frame
  `print_exposure` override applies to **just that frame** (each roll output diffed
  byte-for-byte against the equivalent single `convert`); a missing-input frame is
  recorded `failed` while the good frame still converts and the roll exits 1; a
  same-stem output collision is rejected (exit 2). Unit tests cover `merge_json`,
  the per-frame override merge keeping roll-fixed params, manifest
  `deny_unknown_fields`, output naming, the export-ir rejection, the target
  collision guard, and the roll-report shape.

### Review-fix loop (2026-07-19)
Two-engine review (Codex + 5 pr-review lenses) over the uncommitted changes; all
findings verified and applied (still uncommitted):
- **Roll-fixed film-base invariant now enforced.** (a) When the shared recipe's
  resolved `film_base.source` is not `explicit`, `run_roll` emits a loud
  **roll-level warning** (new `RollReport.warnings`, echoed to stderr, promoted to
  a non-zero exit by `--strict`) explaining the roll is not frozen/color-consistent
  and how to calibrate once — a warning, not a hard error, so best-effort batches
  stay usable. (b) A per-frame manifest override whose `params` sets `film_base`
  is **applied** (the frame converts with its overridden base) with the same loud,
  `--strict`-promotable roll-level warning (`resolve_frames` pushes it into
  `roll_warnings`) — a warn-and-continue, not a reject, per the user's course
  correction that roll-fixed-invariant violations warn rather than fail.
  `density.dmax` overrides stay allowed pending `dmax-reference`.
- **`FrameReport` de-stringified.** The `status:&str` + all-`Option` payload +
  `error` layout became a data-carrying `FrameStatus` enum (`Ok { … }` / `Failed
  { error }`), internally tagged (`#[serde(tag="status")]`) and `#[serde(flatten)]`ed
  so the JSON wire shape is unchanged (`warnings`/`overrides` are common fields).
- **Failed frames keep their warnings.** `convert_frame` now accumulates warnings
  into a caller-owned buffer (`push_warning_buf`); on the `Err` path `run_roll`
  attaches them to the failed frame's report (previously hardcoded empty), so a
  frame that warns then fails still surfaces the warning even under `--quiet`.
- **Manifest subdirectory outputs are created** (`create_dir_all(output.parent())`
  per frame) before encode. Per-frame command label passed as `"roll"`.
- **Docs corrected** (design-spec.md + .html together): the shared recipe *config*
  appears once; each frame additionally echoes its *resolved* base/Dmax (the old
  "not repeated per frame" wording was wrong). Clarified positional inputs
  (directories expand; globs are shell-expanded, not by nc). Added a `merge_json`
  doc note on multi-variant enum overrides being rejected loudly by `from_value`.
- **Tests added** (now driving 21 roll-related tests): directory expansion (sorted,
  non-TIFFs ignored), empty-batch errors on both paths, the not-frozen warning +
  its `--strict` promotion (report still emits, `failed==0`), a two-frame
  determinism diff, per-frame sidecar records the merged recipe, manifest subdir
  output, per-frame `film_base` override warns and `--strict`-promotes (frame still
  converts), a warn-then-fail frame keeping its warning, and a `frame_report_err`
  unit test.

### Review-fix loop — P2 pass (2026-07-21)
Rebased onto `origin/main` (now carries the merged #36 "7 tracked tasks" and #37);
three verified Codex P2 findings applied (all `src/cli.rs`):
- **`merge_json` replaces an externally-tagged enum variant switch instead of
  unioning tags.** A per-frame override that flips a one-key enum map to a
  different variant (real case: shared `film_base.source={"region":…}` + override
  `{"film_base":{"source":{"explicit":…}}}`) was deep-merged key-by-key into a
  two-tag `{"region":…,"explicit":…}` object that no enum deserializes, so a valid
  override became a confusing `from_value` rejection. New `is_variant_switch` guard
  (both sides single-key objects with *different* keys) replaces wholesale; same-tag
  objects still deep-merge (a partial sub-field override keeps its siblings). Unit
  test for the merge both ways + a `resolve_frames` test proving the region→explicit
  override applies **and** still raises the roll-fixed-base warning.
- **The `--frames` manifest is protected from roll write targets.** `run_roll` now
  adds `args.frames` (the manifest path) to the read-input set passed to
  `ensure_roll_targets_distinct`, so `--report-file` (or any output) equal to the
  manifest is rejected (exit 2) before any write. Guard test added.
- **Directory expansion fails loudly on an unreadable entry.** `expand_input`'s
  `read_dir` iteration dropped per-entry `Err`s via `filter_map(Result::ok)` →
  a silently short batch; it now propagates each entry error as a usage error
  (exit 2, same class as failing to open the directory). Happy-path expansion test
  added (a per-entry `read_dir` error is not portably reproducible in a unit test).
## dmax-reference

Made the display-white anchor `Dmax` a **roll-fixed calibration** (like `Dmin`)
instead of a per-frame measurement, and changed the default render accordingly.
Uncommitted (the user runs the review loop, then reviews manually).

### What changed

- **`DmaxSource` enum extended** (`types.rs`) from `{ Auto(default), Explicit,
  None }` to `{ Fixed(default), Explicit, Auto, None }` — one enum, not parallel
  fields (the flags-win merge stays sound). Wire forms: `"fixed"` /
  `{ "explicit": <d> }` / `"auto"` / `"none"`. `DensityParams::default().dmax` is
  now `Fixed`.
  - **`Fixed`** = the roll-fixed **nominal** anchor `algo::density::NOMINAL_DMAX =
    2.0` (a scene-independent placement **in corrected-density units**, not base
    transmission + range). The default when nothing is calibrated.
  - **`Explicit(d)`** now documents the roll-fixed **calibrated** value (measured
    reference / per-stock constant), reused across the roll like an explicit
    `--film-base`. This is the form a roll recipe freezes.
  - **`Auto`** kept but **demoted** to an opt-in (`--auto-d-max`), documented as
    per-frame exposure normalization (the 99.5th-percentile measurement is
    unchanged — only its priority moved).
  - **`None`** unchanged (bit-exact scene-referred escape hatch preserved).
- **Resolution order for the default** (design §7.2): measured reference →
  per-stock constant → nominal. Realized as: a measured/stock value rides in
  `Explicit`; `Fixed` is the no-calibration nominal fallback. `resolve_dmax`
  (`density.rs`) gained the `Fixed => Some(NOMINAL_DMAX)` arm; `Fixed` ignores the
  buffer, so every frame gets the same anchor (the roll-fixed property).
- **Plan-phase reference measurement** (`estimate --d-max-region X,Y,W,H`), the
  mirror of `--base-region` for `Dmax`. Point it at a fully-exposed (near-opaque)
  reference frame — the light-struck leader — with the roll's `Dmin` as
  `--film-base`; it samples the region's **median** transmission
  (`film_base::sample_region_at`, now `pub(crate)`; median is robust to dust on a
  near-opaque frame without a uniformity gate — relative spread on near-zero
  transmissions is noise-dominated and would false-alarm), reduces the per-channel
  corrected density to **one scalar** via a gray/mean reduction
  (`algo::density::reference_dmax`), and reports it. A per-channel `Dmax` would
  smuggle in white balance (three different gains in the exponent), so the anchor
  stays a scalar by construction; the unit test
  `reference_derived_dmax_introduces_no_per_channel_correction` proves the applied
  gain is identical across channels.
- **Freeze = scalar, provenance = report.** `estimate --d-max-region` emits
  reuse-ready forms — `d_max_flag` (`--d-max <d>`) and `d_max_recipe`
  (`{ "dmax": { "explicit": <d> } }`, drops into a recipe's `density` section) —
  and records the sampled region as **`dmax_region` provenance only**. There is
  **no** `{ "reference": … }` recipe form: the frozen recipe carries the scalar so
  the apply phase re-reads nothing (the deterministic-apply / same-recipe-hash
  contract stays intact). The e2e test
  `estimate_measures_roll_fixed_dmax_from_a_reference_region_and_it_round_trips`
  drives estimate → freeze (flag and recipe) → convert and asserts byte-identical
  output.
- **Four-spot wiring** for the new `--fixed-d-max` flag and reworked group
  (`cli.rs`): CLI `DmaxOverrides` (added `--fixed-d-max`, all four mutually
  exclusive), recipe `DensityParams.dmax`, the `merge` arm, and `validate`
  (`Fixed`/`Auto`/`None` need no value check; `Explicit` stays positive-finite).
  `--fixed-d-max` exists so a recipe's explicit/auto is CLI-overridable back to the
  default (an absent presence flag never clobbers a recipe value). Merge test
  `merge_dmax_flags_map_to_the_source_enum` extended; conflict + validate tests
  updated.
- **Sigmoid** unchanged in behavior: it still requires a positive anchor, and the
  new default `Fixed` (2.0 > 0) satisfies it, so `--algorithm sigmoid` works by
  default now (previously the default `Auto` also satisfied it). `anchor_error`
  matches on the resolved `Option<f32>`, not the source enum, so adding `Fixed`
  needed no change there; the CLI usage message was reworded to mention the default
  fixed anchor.

### pipeline_version bump — reconcile note (needs orchestrator attention)

This **changes the default render** (frame-local `auto` 99.5th percentile → fixed
nominal `Dmax = 2.0`). The task said to bump `pipeline_version` by hand. **There is
no `pipeline_version` code constant to bump yet** — `conversion-versioning` has not
shipped (grep: `pipeline_version` appears only in `docs/`, and
`docs/reports/v0-baseline.md` records "pipeline_version 0" as prose). I did **not**
fabricate a constant: adding an unconsumed `const` trips clippy `-D warnings`
(dead_code), and wiring it into the report/telemetry record is `conversion-versioning`'s
design surface (and would collide with the parallel `perf-tel-fix` work). So:
- Documented the change as **superseding v0** in design-spec §12 (item 14 now
  "Implemented", and notes the default-render change must be `pipeline_version` ≥ 1).
  (§7.2 documents the roll-fixed anchor itself but does not mention v0 /
  `pipeline_version` — only §12 does.)
- `v0-baseline.md` already anticipates this exact rework as the future **`v1`**
  ("a later `v1` (auto white-balance, tone curve, Dmax rework)"), so its recorded
  numbers stand as the v0 reference and need no edit.
- **Action for `conversion-versioning`:** when it lands, this default must be
  labeled `pipeline_version 1` (not `v0`), and its golden-output gate should treat
  this commit as the v0→v1 boundary for the density default.

### Reconcile with parallel tasks

- **`roll-conversion`** (parallel worktree) consumes `density.dmax` from a frozen
  recipe. The frozen form this task produces is exactly
  `density.dmax = { "explicit": <scalar> }` (from `estimate --d-max-region`'s
  `d_max_recipe`, or a hand-set `--d-max`), which is what a roll recipe carries —
  no re-read directive, deterministic apply. No code dependency either way; the
  enum default change (`Fixed`) is transparent to a roll recipe that sets `dmax`
  explicitly.
- **`conversion-versioning`** — see the pipeline_version note above.

### Verification

- `cargo test`: 250 unit + 50 e2e = **300 passed, 0 failed**. New tests:
  `fixed_anchor_resolves_to_the_nominal_constant`,
  `reference_dmax_is_the_gray_mean_of_base_relative_density`,
  `reference_dmax_rejects_a_non_opaque_region`,
  `reference_derived_dmax_introduces_no_per_channel_correction` (density.rs);
  `dmax_reuse_fragment_round_trips_as_a_recipe`, `estimate_parses_d_max_region`
  (cli.rs); `estimate_measures_roll_fixed_dmax_from_a_reference_region_and_it_round_trips`,
  `convert_default_uses_the_fixed_roll_anchor_not_per_frame_auto` (pipeline.rs).
  The all-black fixture stands in for the near-opaque leader (no real leader frame
  committed; **real-leader verification, Ektar 1009 / Phoenix 1010, deferred to the
  user** per the task).
- CI gate (`fmt` / `clippy -D warnings` / `build` / `test`): clean.

### Review-fix loop (2026-07-19)

A two-engine review (Codex + 5 pr-review lenses) ran over the uncommitted changes;
the verified findings were applied (still uncommitted). What changed:

- **`reference_dmax` hardened against silent wrong-calibration** (`density.rs`).
  Previously it only rejected a non-positive *averaged* gray density. Now, on
  **every** channel, before the gray reduction: (a) a transmission that is
  non-finite or at/below `SCAN_EPSILON` is a **hard error** — a floored channel
  (dead sensor / clipped black / dark holder) must not be laundered into `D ≈ 6`
  and freeze a black-rendering anchor (the `Dmin` "zero channel errors loudly"
  gotcha, applied to `Dmax`); (b) each channel's base-relative density must be
  `> 0` (a colored/wrong region can average positive while one channel
  out-transmits the base). All hard errors are `NcError::Other` (exit 1),
  consistent with the mirrored `Dmin` guard and the existing test — **not** the
  `Usage`/exit-2 that finding 1(a) literally named (that contradicted its own
  "mirror the Dmin guard" instruction and the algo-layer convention).
- **Plausibility warning** (`density.rs` `MIN_PLAUSIBLE_REFERENCE_DMAX = 1.0`,
  warned in `cli::run_estimate`). A positive-but-implausibly-low reference density
  (a mid-tone region, not a leader) is **not** rejected (thin stock varies) but
  emits a loud, `--strict`-promotable warning. Threshold `1.0` is conservative — a
  full density decade below the base, well under a real leader's `≈ 2–3`.
- **Domain guard (finding 3 — fallback approach taken).** The real domain fix
  (thread `density_scale`/`offset` into the estimate path) is genuinely infeasible
  in scope: `estimate` resolves only a film base, has no density-correction params,
  and does *not* build a `ResolvedConfig` (the finding's premise was inaccurate).
  Instead, `cli::run_convert` now emits a `--strict`-promotable warning when an
  explicit `--d-max` is combined with non-default `density_scale`/`density_offset`
  on a density-domain algorithm — the anchor (raw `D`) would otherwise be in a
  different domain than the render's corrected `D′`. `reference_dmax`'s doc now
  documents both the scale/offset caveat and the spatial regional-balance caveat
  (a non-neutral balance can't fold into any scalar anchor).
- **Reuse gating consistency (finding 4).** The reuse-ready `--d-max` / `density.dmax`
  forms are now gated on the same `(0, 1]` base-usability check the film-base reuse
  uses (`validate_explicit_film_base`), not merely `finite && > 0`. A base in
  `(1, ∞)` still emits the diagnostic `dmax`/`dmax_region` but no longer advertises
  a reuse-ready `--d-max` measured against a base that isn't a valid `--film-base`.
- **Docs.** §7.3 MD sigmoid-anchor sentence aligned with the HTML (the default
  `fixed` nominal, not auto-by-default). §12 item 14 `pipeline_version` reworded as
  a **deferred** obligation (no constant exists yet) in md+html, and the stale
  "§12 item 12" cross-ref corrected to item 16. §8 example terminology fixed:
  raw `-log10(t/base)` is **base-relative density D** (= corrected density under
  default scale/offset), not "corrected density" (§4). Progress note above
  corrected: only §12 (not §7.2) mentions v0/`pipeline_version`.
- **Tests.** The reference-region e2e round-trip now synthesizes a **near-opaque
  non-zero** leader (~2% transmission) via a new `tiff` dev-dependency + in-test
  TIFF generator, instead of the all-black fixture (now a guarded error). Added:
  degenerate all-black region → hard error; per-channel out-transmitting region →
  hard error; floored/zero channel → hard error (`density.rs`); implausibly-low
  reference → strict-promotable warning (e2e); degenerate grid base → `--d-max-region`
  skipped, still hard-errors (e2e); `sample_region_at` median (`p = 0.5`) on a
  non-uniform region differs from the high/low percentiles (`film_base.rs`).

### Review-fix loop (2026-07-21)

A second review pass (Codex P2 findings) over the still-uncommitted changes. Two
verified findings applied:

- **B1 — regional balance folded into the Dmax domain guard** (`cli.rs`). The
  explicit-`Dmax` domain-mismatch warning previously fired only on non-default
  `density_scale`/`density_offset`. But the render subtracts `Dmax` from
  `D′ = B + shadow_balance·w_lo(D̄) + highlight_balance·w_hi(D̄)`, so a **non-neutral
  regional balance** also shifts that domain — a reference/explicit `--d-max` reused
  with a non-neutral shadow/highlight balance silently mis-anchored with no warning.
  This supersedes the prior "spatial balance can't fold into any scalar anchor,
  documentation-only" stance (2026-07-19 note): it can't be *folded into* the anchor,
  but a non-neutral balance still shifts `D′`, so the fixed anchor still mis-anchors —
  hence it now belongs in the guard. The guard decision was extracted into a pure,
  testable `explicit_dmax_domain_warning(&ResolvedConfig) -> Option<String>`; the
  message now names regional balance alongside scale/offset. Test:
  `explicit_dmax_domain_warning_fires_on_nonneutral_regional_balance`.
- **B2 — per-channel reference plausibility** (`density.rs` + `cli.rs`). The
  implausibly-low warning checked only the averaged scalar `dmax`, so a colored region
  with one dense channel and others barely above base cleared it (base `[1,1,1]`,
  transmissions ≈ `[0.001, 0.99, 0.99]` → per-channel densities ≈ `[3.0, 0.004, 0.004]`,
  avg ≈ 1.0 > threshold; the `d ≤ 0` hard error doesn't catch positives). `reference_dmax`
  now returns a `ReferenceDmax { scalar, per_channel }` (the scalar value is unchanged —
  still the gray mean); the `d ≤ 0` per-channel hard error is kept. The estimate-side
  plausibility decision was extracted into a pure
  `reference_dmax_plausibility_warning(&ReferenceDmax) -> Option<String>` with two
  distinct, mutually-exclusive shapes: (a) sub-floor gray mean → the existing frame-thin
  warning; (b) plausible mean but weakest channel below `MIN_PLAUSIBLE_REFERENCE_DMAX` →
  a new colored/wrong-region warning. Tests: `reference_dmax_exposes_a_weak_channel_a_plausible_scalar_hides`
  (`density.rs`, the data) and `reference_dmax_plausibility_warns_on_a_weak_channel_a_plausible_scalar_hides`
  (`cli.rs`, the wiring, covering all three branches).
- Determinism, `None` bit-exactness, four-flag mutual exclusivity, and the
  four-coupled-spots invariant are untouched — both fixes only add/route report
  warnings (no image-output change). CI gate (`fmt` / `clippy -D warnings` / `build` /
  `test`): clean.

## input-data-semantics
**Status:** done (`[x]`)
**Updated:** 2026-07-22

- 2026-07-22: Shipped via `/review-fix-loop` + `/ship`. Two-axis input semantics
  (`input.transfer`/`input.meaning`) with provenance keyed on the SilverFast XMP
  packet (tag 700: `Company=LaserSoft Imaging` + `HDRScan=Yes`; `Gamma` feeds the
  transfer axis via `GammaFact`, with malformed/locale gamma → ambiguous, not
  linear). Generic/processed/colorimetric RGB16 → `Unknown` → `convert` exit 4
  with an assert escape-hatch; positive-mode (`Negative=No`) rejected loudly.
  Reviewed by 6 engines + an adversarial pass + a 2-engine delta re-review, all
  converged; gates green (299 unit + 86 integration). Deferred follow-ups noted
  below still to be filed as tracked tasks: (a) provenance re-validation against a
  broader sample set, (b) positive-mode + embedded-ICC support.

- 2026-07-21: Replaced the planned automatic input-ICC transform after reviewing
  SilverFast HDR/HDRi gamma-1 samples and the role of Dmin. The normal path must
  first establish whether pixels are linear scanner measurements or color-encoded
  data; an embedded scanner profile is reported but is not sufficient reason to
  mix channels before component-wise density conversion. This supersedes
  `input-color-management` and restores the previously skipped fail-loud input
  contract as the higher-priority work.
- 2026-07-21: Review found that a single combined `ScannerLinear` option would still combine two facts. The
  task now resolves transfer encoding separately from measurement meaning:
  Gamma 1 proves only linear transfer, while supported SilverFast raw-mode
  evidence must independently establish scanner-device values.
- 2026-07-21: Replaced the legacy combined assertion in the target contract with
  independent transfer/meaning CLI and recipe axes, deterministic evidence
  precedence, override provenance, and an explicit allowed-combination table.
  An override cannot make an unsupported colorimetric/encoded negative valid.
- 2026-07-21: **Implemented** (left uncommitted for review). New pure, table-tested
  resolver module `src/pipeline/input_semantics.rs`:
  - **Schema / types.** Two independent recipe/CLI axes in `types.rs`:
    `TransferAssertion { auto, linear }` ⇒ `input.transfer`, and
    `MeaningAssertion { auto, scanner-device, colorimetric }` ⇒ `input.meaning`
    (both `clap::ValueEnum` + serde, like `Algorithm`). `InputParams` now holds
    `transfer` / `meaning` / `export_ir` — the old `InputColor` enum and
    `input.color` key are **gone**. Resolver output types (in the module):
    `TransferDescription { linear, unknown }`, `MeasurementMeaning { scanner-device,
    colorimetric{reference}, unknown }`, `ColorReference`, and evidence records
    `InputEvidence { axis, kind, detail, provenance?, displaced? }` with
    `EvidenceKind { user-assertion > structural > descriptive > embedded-icc >
    default }`. `ContainerColorFacts { raw_mode, gamma, embedded_icc }` is the raw
    decode→resolver hand-off; `InputColorMetadata` is the resolved bundle.
  - **Resolver.** `resolve(facts, &InputAssertions) -> Result<InputColorMetadata>`
    is pure/total for `auto` (never errors — ambiguity ⇒ `Unknown`), erroring only
    on an explicit assertion that contradicts authoritative structure
    (`--input-meaning colorimetric` on raw-mode scanner data ⇒ usage/exit 2).
    `require_convertible` is the convert gate: only `Linear` + `ScannerDevice`
    passes; else exit 4. SilverFast raw-mode structure proves **both** linear
    transfer and scanner-device meaning; gamma proves **only** transfer; a
    non-linear gamma tag contradicting raw-mode linear ⇒ transfer `Unknown`
    (convert rejects, inspect explains) unless `--input-transfer linear` overrides
    (records the displaced gamma). Embedded ICC is recorded as informational
    device-characterization evidence, never applied, and does not establish an axis.
  - **Wiring.** `io::decode` extracts embedded ICC via TIFF tag 34675
    (`Tag::IccProfile`) into `DecodeInfo.embedded_icc` (`#[serde(skip)]` — never a
    byte dump). `convert_frame` resolves + gates after decode (before film-base),
    attaches an `InputColorReport` (both axes + per-axis evidence + safe ICC
    summary via lcms2 [class/space/PCS/version/description] + `transfer_decoded`,
    always `false` in Step 1). `inspect` resolves `auto`/`auto` and reports without
    gating. CLI-vs-recipe provenance is threaded via a small `InputFromCli` flag
    into `convert_frame` (roll passes `none()`; its axes are recipe-only).
  - **Migration.** `--assume-linear` ⇒ loud usage error (points at the two flags);
    `--input-profile` ⇒ loud unsupported (exit 4, reserved for
    `scanner-profile-before-density-experiment`); recipe `input.color` ⇒ pinned
    migration error at load (`reject_legacy_input_color`, ahead of the opaque
    `deny_unknown_fields` message).
  - **Tests.** Resolver table tests cover every transfer×meaning combination,
    evidence precedence, contradictions, override provenance, ICC summary safety,
    and `transfer_decoded`. Integration tests (`tests/pipeline.rs`): real-scan
    convert/inspect report resolved axes + structural evidence; colorimetric-on-
    scanner rejected (exit 2); legacy `input.color` recipe rejected (exit 2);
    `--input-profile` rejected (exit 4). CI green (fmt/clippy/build/test).
  - **Decisions / tradeoffs.** (a) `ContainerColorFacts.gamma` is always `None`
    from real decode — SilverFast carries no gamma tag and establishes linear
    transfer *structurally*; the field + gamma logic exist for the resolver's
    synthetic table tests and future encoded/DNG inputs (the "small pure helper,
    table-tested with synthetic metadata" the task asks for). (b) Provenance is
    recorded at CLI-flag-vs-recipe granularity (via `InputFromCli`), not deeper.
    (c) An explicit `--input-meaning scanner-device` on a non-raw file is honored
    (recorded, displaced "no structural evidence") — decode only accepts SilverFast
    (always raw-mode) today, so this override path is exercised only by table tests.
  - **For dependents.** `post-reconstruction-color-characterization` and
    `scanner-profile-before-density-experiment`: the resolved `InputColorMetadata`
    (retained `embedded_icc` + evidence) is the hook for a future
    scanner→working characterization; the working-space assumption in
    `pipeline::color` is unchanged (still linear Rec.709/D65) but now gated to
    scanner-device+linear inputs only. `ColorReference` and `RawMode` are the
    extension points for colorimetric spaces and non-SilverFast raw modes.
- 2026-07-22: **Review fixes** applied (still uncommitted). Six review engines'
  unanimous headline was that raw-mode provenance was hardcoded, making the whole
  Unknown/ambiguity framework dead in production. User-approved resolution and the
  rest of the confirmed findings:
  - **P1 (real provenance).** Grounded the heuristic in the user's actual scans
    (throwaway `#[ignore]` dump, since removed): every genuine HDR (48-bit) and
    HDRi (64-bit) sample carries `Software = "SilverFast …"`; HDRi also carries a
    validated IR plane; none carry an embedded ICC. New
    `DecodeInfo::looks_like_silverfast()` = `Software` contains "silverfast"
    (case-insensitive) **OR** a validated IR plane is present.
    `container_color_facts` now sets `raw_mode = looks_like_silverfast().then_some(…)`
    instead of hardcoding `Some`. A generic/colorimetric RGB16 TIFF → `raw_mode:
    None` → meaning `Unknown` → `convert` exits 4 with an actionable message
    naming the `--input-transfer linear --input-meaning scanner-device` escape
    hatch. Verified end-to-end (generic RGB16 rejected + inspect diagnoses; escape
    hatch converts; real SilverFast still converts).
  - **P2 (roll report).** `FrameStatus::Ok` now carries `input_color`, so `nc roll`
    frame reports include the resolved axes/evidence/ICC summary like single
    `convert`. Roll test added.
  - **M1 (roll fail-fast).** `reject_roll_unsupported_input` runs on the shared
    recipe (and each resolved per-frame override) BEFORE the frame loop, rejecting
    the unconditionally-unsupported `input.meaning = colorimetric` up front (exit
    4) rather than after a 100+ MB decode. (Only colorimetric is decidable
    pre-decode; the other axes stay per-frame gated since they need structural
    facts.)
  - **L1.** `has_legacy_input_color` now also runs on per-frame `--frames`
    manifest override JSON, so a per-frame `input.color` gets the same pinned
    migration message as the shared recipe.
  - **S-Low1.** `io::decode` ICC extraction now distinguishes tag ABSENCE
    (`Ok(None)`, silent) from a genuine READ ERROR / non-byte type (surfaced as a
    non-fatal decode warning) instead of swallowing everything via `.ok()`.
  - **L2-precedence-doc.** Reworded the `EvidenceKind` + module docs: the resolver
    is contradiction-aware, not a blind "higher precedence wins" pick (no `Ord`).
  - **L3-serde.** `TransferAssertion` → `kebab-case` (matches its mirrors).
  - **L2-code (serde shape).** `MeasurementMeaning` now serializes as a flat
    kebab-case **string** (custom `Serialize`); the colorimetric reference moved to
    a sibling `InputColorReport::meaning_reference` field, so `meaning` is a
    homogeneous string on the wire.
  - **Test gaps.** Added: generic-RGB16 reject + escape-hatch (P1/M4), CLI-vs-recipe
    provenance end-to-end (M2), `--assume-linear` through the binary (M3), IR
    byte-identity across input resolution (H1), roll input_color (P2), roll
    colorimetric pre-flight (M1), agreeing-gamma-on-raw branch, flat-`meaning`
    serialize shape. Renamed `contradictory_gamma_on_raw_is_ambiguous_and_*` to
    `…_not_convertible` (it exercises `require_convertible`, not the full command).
  - **Skipped:** optional T-M3 (typed `EvidenceRelation`) — would touch every
    evidence construction + the report shape for a nice-to-have; deferred to keep
    scope contained (contradiction is still tested via `detail` substring +
    per-axis `TransferDescription::Unknown`).
  - IR bit-identity across input resolution holds because resolution never touches
    the image buffers (facts are read from `DecodeInfo`, not the pixels).
- 2026-07-22: **Adversarial-review fix — XMP-based provenance gate** (still
  uncommitted). Codex flagged the `looks_like_silverfast` heuristic (Software
  substring OR IR-plane) as a [high]: it misclassified (a) processed SilverFast
  exports that keep the `Software` tag and (b) a generic RGB16 + matching Gray16
  multipage (IR-alone branch). User-approved replacement, keyed on SilverFast's
  XMP mode metadata.
  - **Grounded first** (throwaway `#[ignore]` dump of TIFF tag 700, removed): every
    genuine scan carries an XMP packet with `Silverfast:` RDF attributes
    (namespace URI `LSI/`) — `Company="LaserSoft Imaging"`, `HDRScan="Yes"`,
    `Gamma="1"`; negatives `Negative="Yes"`, the positive samples `Negative="No"`.
  - **Dep added:** `roxmltree = "0.21.1"` (read-only, deterministic XML tree; 1
    locked package). `Cargo.lock` updated + committed.
  - **decode:** extract tag 700 (`Tag::Unknown(700)`) → UTF-8 → `parse_silverfast_xmp`
    (roxmltree, reads the `Silverfast:` namespaced attributes) → typed
    `SilverfastXmp { company, hdr_scan, gamma, negative }` on `DecodeInfo`
    (serialized in the `decode` report; skipped when absent). Same loud-vs-silent
    contract as the ICC tag: absence silent, read-error / non-UTF-8 → non-fatal
    decode warning.
  - **provenance rewire:** removed `looks_like_silverfast`. `DecodeInfo::is_silverfast_raw_mode()`
    = `Company=="LaserSoft Imaging" && HDRScan==Some(true)`; `container_color_facts`
    now sets `raw_mode` from that and feeds `ContainerColorFacts.gamma` from the XMP
    `Gamma` (finally making the descriptive-gamma path LIVE — a processed export with
    `HDRScan=Yes` but non-linear `Gamma` now hits structural-linear-vs-descriptive-
    nonlinear → transfer `Unknown` → rejected; verified end-to-end). Software string
    and IR-presence are no longer provenance (IR validation still decodes the plane).
  - **positive-mode:** `DecodeInfo::is_silverfast_positive_mode()` (`Negative==No`);
    `reject_positive_mode` in `convert_frame` (after the transfer/meaning gate)
    fails loudly (exit 4, distinct message) rather than misconverting a positive as
    a negative. `inspect` still reports (doesn't gate).
  - **Tests:** decode unit (parser fields; provenance from XMP not Software/IR;
    positive vs negative), integration (RGB16+Gray16-no-XMP rejected; Software-only
    rejected; synthetic negative converts; non-linear-gamma rejected + inspect shows
    transfer unknown; positive-mode rejected). Throwaway bash loop over
    `../nc-assets` confirmed all real negatives (48/64 full+small, samples
    embedded/non-embedded) resolve scanner-device/linear and both positive samples
    hit the positive-mode error; committed fixtures already carry the XMP so the
    existing suite still converts them.
  - **Deferred follow-ups to file formally:** (a) **positive-mode + embedded-ICC
    support** — use the `Negative` flag + the retained embedded ICC to convert
    positive-mode / ICC-embedded SilverFast scans; (b) **re-validate input
    provenance/metadata detection once we have a wider sample set** — other
    scanning software, other scanners, cameras, and different SilverFast
    configurations. The current XMP-Silverfast gate (`Company=LaserSoft Imaging` +
    `HDRScan=Yes` + `Gamma`) is grounded on a single scanner/software combination
    (Plustek OpticFilm 8300i, SilverFast 9.2.x); the detection rules should be
    re-examined against broader real samples when available, so genuine scans from
    other sources aren't wrongly rejected and the mode/gamma markers still hold
    (user's explicit request).
- 2026-07-22: **Delta re-review fixes — three XMP silent-signal-drops** (still
  uncommitted). Both engines confirmed the gate fails closed; these close the
  remaining silent drops in the new XMP path.
  - **F1 (had a wrong-image path): malformed/locale gamma no longer resolves
    linear.** Introduced `types::GammaFact { Absent, Value(f64), Malformed(String) }`
    (shared by `io::decode` and the resolver — lives in `types` to avoid an
    io→pipeline dep) so a *present-but-uninterpretable* gamma is distinct from
    *absent*. `parse_silverfast_xmp` now yields `Malformed(raw)` for an unparseable
    `Silverfast:Gamma` (e.g. German-locale `"2,2"`); decode pushes a warning naming
    the value; `resolve_transfer` treats `Malformed` (even with raw-mode structure)
    as ambiguous → transfer `Unknown` → convert exit 4. An explicit
    `--input-transfer linear` still overrides it (records the uninterpretable tag as
    displaced). nc does **not** guess comma-decimals.
  - **F2: unrecognized/malformed tag-700 now warns.** When tag 700 is present and
    valid UTF-8 but `parse_silverfast_xmp` returns `None` (malformed XML, or a
    namespace/layout that isn't the `LSI/` shape), decode pushes "XMP packet present
    but no recognizable SilverFast metadata …" instead of silently losing
    provenance — important for the broader-sample follow-up (a future scanner's
    namespace diff would otherwise be silent).
  - **F3: `yes_no` no longer conflates "not yes" with "explicit No".** Returns
    `Some(true)` for yes, `Some(false)` for no, `None` for anything else — so an
    unrecognized `Negative` value (`"y"/"1"/…`) is `None`, not a masquerading No,
    and a genuine negative isn't failed as positive-mode (`is_silverfast_positive_mode`
    only fires on an explicit `Negative=No`); likewise an unrecognized `HDRScan`
    → not raw-mode → rejected as unknown (correct).
  - **Tests:** resolver unit (malformed gamma → Unknown; explicit-linear override
    records displaced); decode unit (Malformed gamma fact + warning; unrecognized
    XMP warning; unrecognized yes/no → None); integration (malformed gamma → convert
    exit 4 + inspect transfer unknown with breadcrumb; unrecognized `Negative` on a
    genuine negative still converts).
**Status:** not started
**Updated:** 2026-07-21

- 2026-07-21: Split scanner-profile placement into a deferred controlled
  experiment. Compare density-first, ICC-first in a defined linear colorimetric
  space, and joint scanner+film characterization using target-patch error; do not
  lift `--input-profile` into the normal workflow without evidence.
- 2026-07-21: Narrowed after design review: this task now compares only raw
  density-first versus applying the same conventional scanner ICC to image and
  Dmin before density. Post-reconstruction characterization is an independent
  production track and is not blocked on this deferred experiment.

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

## color-characterization-calibration
**Status:** superseded
**Updated:** 2026-07-23

- 2026-07-23: Superseded by `optional-color-correction-profiles`. Measured
  neutralization is now an explicitly selected, non-blocking correction feature;
  it is not part of the default film-preserving pipeline and no display task
  depends on it.

- 2026-07-21: Added the offline calibration half split from the runtime task. It
  fits matrix/curves against controlled target data, validates held-out Delta E,
  justifies model complexity, and emits a reproducible versioned artifact with
  scanner/film/development provenance.
- 2026-07-21: Added explicit target reference coordinates/illuminant and declared
  adaptation into ACEScg D60. Calibration normalization may not bake creative WB;
  artifacts also carry the exact reconstruction-domain compatibility contract.
- 2026-07-21: Calibration inputs now follow per-algorithm canonical domains:
  density artifacts fit the Dmax-neutral positive and reuse across scalar Dmax
  placement; sigmoid v1 fits one exact fixed Dmax; simple fits its pinned affine
  inversion settings.
- 2026-07-21: Superseded the prior simple affine wording. Simple calibration fits
  raw unclamped `1 - scan/Dmin`; inversion WB and black/white placement are
  excluded from calibration and artifact compatibility.

## post-characterization-render-pipeline
**Status:** superseded
**Updated:** 2026-07-23

- 2026-07-23: Superseded by `film-master-render-pipeline`. The replacement
  consumes typed NC film RGB v1 mapped ACEScg, renames `scene-master` to
  `film-master`, and explicitly preserves intentional film rendering rather than
  claiming physical scene recovery.

- 2026-07-21: Split pipeline/routing work from characterization runtime. This task
  moves WB/exposure/black/highlight controls after characterization, provides the
  common SDR/HDR source API, and defines a true scene master. The master rejects
  frame-local auto Dmax, accepts supported `none` or fixed/roll Dmax, and preserves
  exposure; current `--output-hdr` remains a rendered transitional float TIFF.
- 2026-07-21: Made the master bypass fail-loud: any non-default downstream render
  control remaining after CLI/recipe merge is a usage error, never ignored.
  Added flags-win reset, conflict, and resolved-report provenance requirements.
- 2026-07-21: Inserted algorithm-specific placement before the output split.
  Density Dmax is now a scalar gain after characterization; sigmoid/simple arrive
  already placed under their artifact contracts. Scene-master includes placement
  but still bypasses every later print/display control.
- 2026-07-21: Moved ownership of density artifact evaluation and Dmax placement
  wholly into the characterization runtime. This task now accepts only ordinary
  placed `f32` ACEScg, cannot observe the private extended-range intermediate,
  and records the fixed/none placement already applied to a scene master.
- 2026-07-21: Moved shipped simple inversion-WB and clip-low/high remapping after
  characterization. Target presets use `print.white_balance` plus new
  `print.linear_range`; old simple controls are warned conflicting aliases, while
  legacy no-preset TIFF retains current ordering during migration. Scene master
  rejects any non-default resolved adjustment.

## negative-reconstruction-density-curves
**Status:** not started
**Updated:** 2026-07-23

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

## film-rgb-working-space
**Status:** not started
**Updated:** 2026-07-23

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

## display-p3-output
**Status:** not started
**Updated:** 2026-07-21

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

## hdr-output-spike
**Status:** not started
**Updated:** 2026-07-21

- 2026-07-21: Added a decision gate for ISO HDR versus ISO 21496-1 gain-map HDR,
  HEIC/JPEG containers, encoder/licensing constraints, metadata, reference white,
  headroom, and cross-platform fallback before committing production code.

## hdr-display-rendering
**Status:** not started
**Updated:** 2026-07-23

- 2026-07-23: Rebased the renderer on intentional linear ACEScg film values from
  `film-master-render-pipeline`; physical scene recovery and optional correction
  profiles are not prerequisites.

- 2026-07-21: Planned a pure scene-linear ACEScg to BT.2020 PQ/HLG render stage.
  Rec.2100 is a display encoding, not nc's density or internal working space.
- 2026-07-21: Removed ambiguous ownership of the SDR base; this task now verifies
  PQ/HLG only, while `sdr-display-rendering` produces the independent SDR render.

## sdr-display-rendering
**Status:** not started
**Updated:** 2026-07-23

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

## gain-map-hdr-output
**Status:** not started
**Updated:** 2026-07-21

- 2026-07-21: Planned standards-neutral ISO 21496-1 output: Display P3 SDR base
  plus a gain map reconstructing the HDR rendition, initially targeting HEIC and
  requiring both Apple and non-Apple interoperability checks.
- 2026-07-21: Rewired the task to consume `sdr-display-rendering` rather than
  assuming profile synthesis alone produced an independently valid SDR base.
- 2026-07-21: Required both renditions to share the identical characterized and
  adjusted source, and pinned gain-ratio derivation to the standard-required
  common linear color domain rather than encoded P3/PQ/HLG channel division.

## output-presets
**Status:** not started
**Updated:** 2026-07-23

- 2026-07-23: Renamed `scene-master` to `film-master` and defined it as the
  unclamped linear ACEScg encoding of NC's intentional film rendering. Removed
  artifact/calibration assumptions; optional correction profiles do not affect
  preset availability.
- 2026-07-23: Added `conversion-versioning` as an explicit prerequisite because
  preset/default activation owns a golden-tested behavioral
  `pipeline_version` boundary.

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

## display-output-acceptance
**Status:** not started
**Updated:** 2026-07-23

- 2026-07-23: Removed calibration from the dependency and acceptance matrix.
  Acceptance now verifies faithful preservation of NC's intended film rendering,
  cross-encoding consistency, tone/gamut behavior, metadata, determinism, and
  viewer interoperability rather than agreement with a physical scene.
- 2026-07-23: Made acceptance reproducible with a versioned golden manifest,
  canonical pre-encode buffers, independent decode-back oracles, quantitative
  bounds for float/SDR/PQ/HLG/gain-map outputs, normalized metadata comparison,
  and a separate binary manual-viewer interoperability rubric.
- 2026-07-23: Refined PQ/HLG acceptance to a bit-depth/transfer-derived
  independent quantization oracle (half-code lossless or spike-approved one-code
  codec allowance) over observable stored codes; pre-quantization arithmetic is
  not asserted by the black-box acceptance harness. Pinned
  cross-encoding exposure/reference-white normalization, D65 CIELAB,
  Sharma–Wu–Dalal CIEDE2000 parameters, and CIE 1976 u'v' formulas.

- 2026-07-21: Split final display/HDR acceptance from core real-scan verification.
  This task waits for output presets and reuses the verified full-size assets to
  check the gain-map default, explicit presets, metadata, deterministic encoder
  contracts, and Apple/non-Apple aware plus SDR-fallback readers.
- 2026-07-21: Added calibrated-characterization acceptance as a real dependency.
  The matrix exercises both a matching measured artifact and the explicitly
  warned/reported provisional fallback; output preset implementation itself stays
  independent of offline calibration.
- 2026-07-21: Acceptance now distinguishes a compatible measured artifact, the
  internally valid but provisional assumed-source fallback, and the untagged
  identity-device diagnostic rejected by named presets. Scene-master acceptance
  also checks fixed-Dmax cross-frame exposure preservation.

## color-management planning — main reconciliation
**Status:** documentation reconciled
**Updated:** 2026-07-21

- Rebased onto `origin/main` after `roll-conversion` (`3b93ae5`) and
  `dmax-reference` (`06b75fb`) merged. Preserved both append-only implementation
  histories and marked both tasks complete in the canonical index.
- Replaced the stale output-preset reconciliation note with the shipped `nc roll`
  contract: `<stem>_positive.tiff` automatic names today, explicit manifest
  outputs and per-frame partial recipes, path-derived per-image sidecars, one
  stdout/`--report-file` roll report, and pre-write collision checks. The future
  preset task extends those guarantees only where container-specific suffixes and
  per-frame preset selection require it.
- Reconciled shipped Dmax behavior with the planned characterized runtime. Today
  `density.dmax` defaults to roll-fixed `fixed`, `--fixed-d-max` resets a recipe,
  `nc estimate --d-max-region` emits the reusable explicit scalar, roll mode warns
  on auto/per-frame Dmax, and the default-render change remains the deferred
  `pipeline_version 1` boundary. Future characterization keeps the scalar's
  roll-fixed acquisition but treats density Dmax as post-artifact exposure
  placement rather than promising display white; sigmoid still scopes the exact
  numeric Dmax.
- Review correction: `sdr-display-rendering` returns rendered-linear destination
  pixels and resolved metadata, never transfer-encoded pixels. Display P3 or the
  corresponding destination-output stage applies transfer encoding afterward;
  gain-map construction consumes the pre-transfer rendition for common-linear
  ratio derivation. This removes the prior double-encoding ambiguity.

### Real-scan core verification — executed 2026-07-22 (task: real-scan-verification)

Full matrix run against the user's five real rolls (Ektar, phoenix, Portra160,
Portra400, Portra400-leica-flaw) on the compiled release binary. Derived numbers
only — no sample pixels read into context. Full write-up + numbers:
[`docs/reports/real-scan-verification.md`](reports/real-scan-verification.md);
rerunnable harness + frozen recipes under `scripts/real-scan-verify/` (see its `README.md`).

- **All assets are HDRi** (`ir_present: true`; IR carried in the transparency-mask
  IFD), standard frame 5184×3599 ≈ 18.66 MP. Scanner Plustek 8300i / SilverFast 9.2.9.
- Per-roll `Dmin` (unexposed frame) + `Dmax` (fully-exposed leader) measured from a
  holder-free center-40% region, frozen to recipes. 4/5 rolls clean.
- Matrix: inspect ✅ · estimate ✅ (`--auto-base` fails loudly on every frame per the
  holder layout — correct) · convert 16-bit+float ✅ (float byte-lossless; u16 clips
  4.8–10.3% high) · IR export + `--strict` ✅ · determinism byte-identical ✅ ·
  resource ✅.
- **Resource / streaming STEP 0 input:** measured peak **~930 MiB @ 18.66 MP
  (~50 MiB/MP)**, ~1.6 s wall — ~1.5× the design's ~600 MB model (model omits the
  carried IR plane + `to_output` clone). Target = 8 GB M3 MacBook Air (2024),
  ~4–5 GB usable. Assumed 4× worst case ⇒ ~3.7 GiB (4× MP) to ~15 GiB (4× per side)
  ⇒ **`memory-preflight` gate required; streaming a conditional GO** pending
  post-preflight re-measure and the true input envelope.
- **Follow-ups:** (1) default 16-bit highlight clipping → display-output roadmap;
  (2) Harman Phoenix dense base trips the `Dmax ≳1.0` floor + base-uniformity check
  → candidate new task (per-stock/dense-base Dmax handling); (3) widen
  `memory-preflight` sizing model to count IR + clone. No hard defects.
