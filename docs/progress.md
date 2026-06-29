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
    not clamp and may hand out-of-`[0,1]` or `NaN` samples (density log/division
    math), so the encoder counts the information lost and surfaces it instead of
    silently blackening pixels — `any_loss()` / `loss_fraction()` for consumers.
    f32 encodes never quantize and report all-zero (`total_samples == 0`).
    `export_ir` discards the report behind a `debug_assert!(!any_loss())` because
    IR is decode-normalized to `[0,1]` and carried untouched (revisit when IR
    processing lands). **`pipeline-orchestration` must fold this into the JSON
    report and honor `--strict`** — the encoder only surfaces, doesn't decide.
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
**Status:** not started
**Updated:** —

- Goal: estimate `Dmin` `FilmBase` from border/region with full CLI override.
- Note (from project-foundation review): `FilmBaseParams` keeps three flat fields
  (`film_base`, `base_region`, `auto_base`) which can express contradictory combos.
  **Enforce and unit-test the precedence** here: explicit `film_base` overrides
  `base_region` overrides `auto_base`. Use `FilmBase::from([f32;3])` for the
  `film_base` override (conversion lives in `types.rs`). If the flat shape proves
  awkward, consider collapsing to an enum `FilmBaseSource { Auto, Region(..),
  Explicit(..) }` — deferred from foundation, decide here.

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
**Status:** not started
**Updated:** —

- Goal: channel-inversion baseline converter (debug / B&W) with white balance and
  black/white points.

## algo-density
**Status:** not started
**Updated:** —

- Goal: density-domain converter (Cineon/negadoctor style) with separate density
  and print-render sub-stages; the default algorithm.

## pipeline-orchestration
**Status:** not started
**Updated:** —

- Goal: wire `convert`/`inspect`/`estimate` end to end, producing a positive TIFF
  and JSON reports from a real scan.
