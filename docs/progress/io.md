# Negative Converter — io Progress Log

Execution log for the `io` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `io`:

- **`decode(&Path) -> (LinearImage, DecodeInfo)`.** HDR vs HDRi is detected
  **structurally** (extra IFDs), not from metadata — `Silverfast:HDRScan="Yes"`
  appears on both. Real full-res scans have **three** IFDs (RGB, a
  reduced-resolution preview, then the IR plane), so the decoder scans all pages
  and skips reduced-resolution previews. Samples are 16-bit unsigned, normalized
  `/65535`, treated as linear.
- **IR provenance matters now.** `LinearImage::ir_verified` records whether the
  IR plane carried the `NewSubfileType=4` marker or was accepted by shape alone.
  `film-base/ir-holder-detection` consumes IR only when verified. Anything else
  that starts *acting* on IR must check this flag, not just `ir.is_some()`.
- **Input semantics are two independent axes** (`pipeline/input_semantics.rs`):
  `input.transfer` (linear / unknown) and `input.meaning` (scanner-device /
  colorimetric / unknown), resolved from the **SilverFast XMP packet** (TIFF tag
  700: `Company="LaserSoft Imaging"` + `HDRScan="Yes"`, with `Gamma` feeding
  transfer). Only `Linear` + `ScannerDevice` may convert (else exit 4);
  positive-mode scans (`Negative=No`) are rejected loudly. **The provenance rule
  is grounded on exactly one scanner/software combination** (Plustek OpticFilm
  8300i + SilverFast 9.2.x) — re-validate it before trusting it on other sources.
  Embedded ICC is recorded but never applied.
- **The encoder is where clamping happens, and it counts the loss.** Colour and
  algo stages pass values through unclamped; `encode` returns an `EncodeReport`
  (`clipped_low`/`clipped_high`/`non_finite`) that the orchestrator turns into
  report warnings. Never clamp earlier, and never silently.
- **f32 output is written verbatim** (values > 1.0 preserved); u16 clamps and
  rounds. Non-finite samples are counted at **both** depths, so a numerical fault
  upstream stays visible — don't launder `NaN` into a finite value in a stage.
- **Known gap:** artifacts are still written straight to their final paths
  (`io/transactional-output-writes`). *(The "peak memory is unbounded" half of this
  gap is resolved — see the next bullet.)*
- **Peak memory is now bounded and reported** (`io/memory-preflight`, done
  2026-07-27 — supersedes the "unbounded" gap above). Every command that decodes
  runs a **preflight before decode**, from a metadata-only `io::decode::probe`, and
  fails with **exit 6** (`NcError::Resource`) when the estimated peak exceeds the
  budget (fixed 6 GiB default; `--max-memory` to override — operational flag, not
  a recipe key). Exit 6 is `convert`/`inspect`/`estimate`; on **`roll`** the gate
  runs per frame and a rejection is that frame's error — recorded in its report
  entry, siblings still converted and written, roll exits **1** like any
  frames-failed batch. `pipeline::memory` is the one sizing model: **decode 18 ·
  film-base 16+12·s · render 32+12·s · encode 38+12·s bytes/px** for HDRi u16
  (`s` = sampled rectangle ÷ frame), so **encode** is the peak for `convert` and
  the **film-base** phase for `inspect`/`estimate`. The `12·s` sampling term rides
  into every *later* phase because freed pages stay resident — the same retention
  rule that sums the encode buffers. Two full images still overlap by design (the
  decoded image is held for `--export-ir`). `decode` is now `decode_within(&Path,
  budget_bytes)`. Anything that adds a full-frame buffer to a stage **must** update
  that model by hand — no test compares it against the code — or the gate silently
  under-approves.
- **`color::to_output` no longer copies** (`LinearImage` in, `(LinearImage,
  Vec<u8>)` out — it transforms the very buffers it was handed). It used to clone
  the whole image, IR plane included. Real peak on a 74.65 MP HDRi scan: 3.808 GB →
  3.146 GB, byte-identical output. Don't reintroduce a stage-local copy of a full
  image without counting it in the model.
- **Note:** the log entries for `color/scanner-profile-before-density-experiment`
  are stranded at the tail of the `input-data-semantics` section below — they lost
  their heading in the flat log before this epic split. Read them there.


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
- 2026-07-27: Epic-migration redirect — the silverfast-decode task path cited above
  is now [io/silverfast-decode](../tasks/io/silverfast-decode.md).
  The entry above is preserved verbatim.


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


## memory-preflight

**Status:** done
**Updated:** 2026-07-27

- Goal: Make the pipeline's memory use **honest and bounded** without changing its whole-image architecture.
- 2026-07-27: **Done.** Both halves shipped: the no-copy color transform and the
  peak-memory preflight. Full CI gate clean (fmt / clippy `-D warnings` / build /
  472 tests).
- **Measured on `samples/largest.tif`** (10368x7200 = 74.65 MP HDRi, the new
  `perf-worst-case` asset — ~4x a standard roll frame), release binary,
  `/usr/bin/time -l` peak RSS on macOS/aarch64:

  | run | before | after |
  |---|---|---|
  | `convert` u16 | 3.808 GB | 3.146 GB |
  | `convert --output-hdr` | 3.892 GB | 2.698 GB |
  | `convert --export-ir` | 3.893 GB | 3.146 GB |
  | `inspect` / `estimate` | 1.502 GB | 1.503 GB (unchanged — no render) |

  A standard 18.66 MP frame went **~975 MB → 681 MB** (the same numbers in binary
  units: ~930 MiB → 650 MiB) — a 30% cut. Units in this section: measured peaks and
  model outputs in **decimal GB/MB** (what `time -l` is compared against), budgets
  and the allowance in **GiB/MiB**. Output is **byte-identical** before/after on all
  three real-scan paths (u16, hdr, export-ir) and on all six fixture paths (u16 /
  hdr / display-p3 x HDR / HDRi).
- **No-copy transform.** `color::to_output(LinearImage, &OutputParams) ->
  Result<(LinearImage, Vec<u8>)>` — was the same signature with an `image.clone()`
  inside. It now transforms the buffers it was handed and moves them back out
  (a `Vec` move is a handle move), so `stages::render` lets the positive *become*
  the output image. Dropped one full RGB buffer **and** one pointless full IR clone
  (16 B/px). The `rgb.len() % 3` guard stays. Consuming the image (rather than
  taking `&mut`) is deliberate: `profile_icc` can fail *after* the transform has
  run, and a by-reference signature would let that `Err` hand the caller a
  half-converted buffer.
- **The sizing model is per-phase, and the render is not the peak anymore.**
  `pipeline/memory.rs` accounts the simultaneously-live full-frame buffers:
  decode 18 B/px, film-base 16 + 12·s, render 32 + 12·s, encode 38 + 12·s
  (HDRi u16, `s` = sampled rectangle ÷ frame). With three images gone the
  **encode** phase is the peak for `convert` — decoded + rendered + the u16
  quantize buffer — because the decoded image is held for `--export-ir`. Anyone
  tempted to "just count the render" will under-estimate by 6 B/px.
- **Film-base sampling is not free** (caught in review — the first version of the
  model claimed `film_base` allocated no full-frame buffer, which is false).
  `film_base::region_channels` materializes its rectangle **unstrided** into three
  `Vec<f32>` = 12 B per sampled pixel, live alongside the decoded image, and the
  `auto` path's interior rectangle is ~69% of a 3:2 frame ⇒ ~24 B/px (a full-frame
  `--base-region` / `--d-max-region` rectangle ⇒ 28 B/px; `--grid` is charged one
  cell, ~1/16 of its rectangle). For `inspect`/`estimate` the **film-base phase is
  the peak**, and gating them at 18 B/px let them be admitted and then exceed their
  own estimate — measured **+26%** on the auto-interior rectangle and **+43%** on a
  full-frame one. The interior rule lives once, in `film_base::auto_interior_pixels`,
  which both the sampler and the model call.
- **Freed is not gone — and the first fix got this wrong.** Counting the film-base
  phase but letting it *compete* with the others still under-estimated a full-frame
  `--base-region` **`convert`** by **10.2%** (measured 3.743 GB vs 3.396 GB
  predicted). The sample is freed before the render, but peak RSS is a high-water
  mark and the allocator keeps the pages, so it is **retained**: added into the
  render and encode phases, not competed against them. That makes 50 B/px for that
  run, which reproduces the measurement to +0.3%. It is the same rule the encode
  buffers are summed under — one rule, stated once, instead of two ad-hoc choices.
  Its cost is the default budget: 4 GiB would now reject that (working) run, so the
  default is **6 GiB**.
- **Two accounting subtleties that cost real accuracy.** (1) At decode the u16
  read buffer is freed before the IR plane is read, so those are *alternatives*
  (`max`), and the common base is the RGB f32 buffer, **not** the whole decoded
  image — getting this wrong first gave 22 B/px instead of 18 and a +29% error on
  `inspect`. (2) At encode the IR export buffer *is* freed before the main
  quantize buffer, and counting them **additively** is a deliberate over-count in
  line with the err-toward-rejecting policy — not something measurement requires:
  `convert` and `convert --export-ir` measure the *same* peak RSS, i.e. the IR
  export buffer never shows up in the high-water mark at all.
- **Allowance = 15% + 128 MiB** on top of the accounted buffers, for allocator
  slack, the binary, lcms2, and the tiff writer. Worst measured overhead was
  **12.9%** (74.65 MP `--output-hdr`; the u16 convert is +10.9% and `inspect`
  +11.9%, and both 18.66 MP runs measure *below* their accounted total), so 15%
  keeps a margin — 2.1 percentage points — instead of tracking one machine's
  allocator exactly. Every calibration point over-estimates (+7% to +13% at
  full size; looser on small frames, where the fixed term dominates). A preflight
  that under-estimates is worse than useless — it approves the run that OOMs. The
  fixed term is also an unavoidable **floor**: nothing can be admitted under a
  budget of 128 MiB or less, and the rejection message says so instead of
  suggesting a smaller frame.
- **Budget: fixed 6 GiB default + `--max-memory`, warn tier tracks RAM.** The
  hard default is a constant so the pass/fail decision is machine-independent;
  the *warning* compares against detected physical RAM (70%), which is the one
  deliberately environment-dependent piece (and, via `--strict`, the one way the
  exit code can differ between machines — documented in §11 and the module doc).
  RAM detection is fail-soft: `sysctlbyname("hw.memsize")` on Darwin (new direct
  `libc` dep — already in `Cargo.lock` transitively), `min(/proc/meminfo MemTotal,
  cgroup memory limit)` on Linux (v2 `memory.max`, else v1
  `memory/memory.limit_in_bytes` — `MemTotal` alone reports the *host's* RAM and
  would keep the warn tier silent inside a container capped well below it, which is
  where an OOM kill is likeliest), else `None` ⇒ no warning. The Linux body is a
  plain function, not inline `#[cfg]` code, so it type-checks on macOS too; its
  parsers are unit-tested on every target.
- **`--max-memory` is operational, not a recipe key** (like `--report` /
  `--strict` / telemetry): CLI-only, absent from the sidecar, and proven not to
  perturb output bytes; a recipe carrying `max_memory` is rejected by
  `deny_unknown_fields`.
- **The gate needs a metadata-only probe.** `io::decode::probe(&Path) ->
  ImageShape` reads IFD0 dimensions/colortype plus an IFD walk for the IR plane
  and **never calls `read_image`** — `decode` only returns dimensions *after*
  allocating, which is far too late. Verified: rejecting `largest.tif` under
  `--max-memory 2GiB` exits 6 in 0.29 s at **3.5 MB** peak RSS, with no output or
  sidecar written. The probe deliberately mirrors `decode`'s IR rule (skip
  reduced-res previews, take a full-res 1-channel page); tests pin probe and
  decode against each other on the fixtures and the three synthetic layouts,
  because a missed IR plane silently under-estimates by a third of the RGB
  footprint.
- **`decode` is now `decode_within(&Path, budget_bytes)`** — one entry point,
  with the `tiff` read buffers capped at `min(4 GiB, budget)`. That is what
  reconciles the old standalone 4 GiB input limit with the peak budget: the
  preflight is the authority, this cap is the defense-in-depth for a header whose
  strip sizes don't match the shape the probe read. The old budgetless `decode`
  survives only as a test-module helper.
- **Per-command profiles matter.** `inspect`/`estimate` gate on
  `RunProfile::DecodeOnly` (decode ~18 B/px, film-base ~24–28); gating them on the
  convert peak would reject scans they diagnose comfortably. An e2e test picks a
  budget *between* the two estimates and asserts `inspect` passes while `convert`
  exits 6. The film-base phase is sized from a `SamplePlan` the orchestrator derives
  from the resolved base source plus `estimate`'s `--grid` / `--d-max-region`, so a
  command's actual sampling — not a guess — reaches the model.
- **New exit code 6** (`NcError::Resource`), distinct from `Unsupported` (4): the
  input is fine, it is *this run on this budget* that can't proceed, so an agent
  can retry with `--max-memory` instead of giving up on the file. Exit 6 is
  `convert`/`inspect`/`estimate`; on `roll` a rejected frame is recorded per frame
  (with its `memory` block, which lives on the frame entry rather than on the "ok"
  payload for exactly that reason), siblings are still written, and the roll exits
  **1** — roll's ordinary frames-failed code, unchanged by this task.
- **Reproducing the measurements** (not in `harness.sh`, whose `resource` row is
  per-roll; `largest.tif` is a standalone sample). Release binary, and the film base
  is arbitrary because peak RSS doesn't depend on it — this is a memory
  measurement, not a color baseline:

  ```bash
  BIG=../nc-assets/samples/largest.tif
  /usr/bin/time -l ./target/release/nc convert --film-base 0.9,0.55,0.42 \
      -o /tmp/big.tiff "$BIG" | jq .memory      # estimate vs measured RSS
  /usr/bin/time -l ./target/release/nc convert --film-base 0.9,0.55,0.42 \
      --max-memory 2GiB -o /tmp/nope.tiff "$BIG"   # exit 6, ~3.5 MB peak
  ```

  The report's `memory.estimated_peak_bytes` is directly comparable to
  `time -l`'s `maximum resident set size`; the pinned pairs live in
  `pipeline::memory`'s `estimate_stays_conservative_against_the_measured_peaks`.
- Not done here (deliberately): getting below **two** overlapping images. The
  decoded image outlives the render because `--export-ir` reads it, so the model
  keeps counting both. Bounding to a small working set is `io/streaming-tiled-io`.
- 2026-07-28: **Landed after two review rounds** (six independent reviewers across
  the two). What the review caught is worth recording, because all of it was in the
  *model*, not the plumbing:
  - The first model claimed `film_base` allocated no full-frame buffer. It does —
    `region_channels` materializes its rectangle unstrided into three `Vec<f32>`.
    Measured consequence: `estimate --base-region` on the 74.65 MP frame was
    admitted at a predicted 1.679 GB and actually peaked at **2.119 GB (+26%)**.
  - The first *fix* (count the phase, let it compete via `max`) still
    under-estimated a full-frame-region `convert` by **10.2%**. Freed pages stay
    resident, so the sample had to be **retained** into the later phases. That
    reproduces the measurement to +0.3% — and forced the default budget from 4 GiB
    to **6 GiB**, since 4 GiB would have rejected a run that measures 3.743 GB and
    completes fine.
  - Ship-time review added two more: an out-of-bounds `--base-region` was charged
    its raw `w*h` and rejected as a 12852 GiB *resource* overrun (exit 6) instead
    of the usage error (exit 2) — `sampled_pixels` now clamps to the frame, leaving
    `region_channels` the authority; and Linux cgroup detection read only the root
    limit files, so nested (Kubernetes/systemd) cgroups were missed and the warn
    tier stayed silent in exactly the containers it exists for.
  - Verified at the end: every one of 11 model-vs-measured pairs is conservative
    (+4.6% to +76%), and all nine real-scan and fixture outputs are byte-identical
    to the pre-change baselines.
  - **For `io/streaming-tiled-io`:** the auto film-base path currently *refuses* on
    every real asset (no uniform rebate band), so its 8.3 B/px interior sample is a
    future cost, not a present one — but a user-sized `--base-region` already
    reaches 12 B/px today, and that is the term the model exists to catch.
- 2026-07-27 (review pass, same day): the six-reviewer round on this change found
  one real modelling bug and a set of doc/derivation errors. Carried forward:
  - The **film-base phase was missing** from the model (see the dedicated bullet
    above). It only mattered for `inspect`/`estimate`, which could be admitted at 18
    B/px and then peak at ~24 — the gate approving a run that exceeds its own
    prediction. `convert` was never wrong, but for the wrong reason.
  - **Every measured row in the calibration table used an explicit `--film-base`**,
    so no measurement here exercised the interior sample. The auto path's ~8.3 B/px
    is *derived* (74.65 MP `inspect --auto-base`: 1.811 GB accounted, 2.22 GB
    estimated) and still needs a `time -l` run to confirm — the one open calibration
    item left by this task.
  - Derived numbers corrected in place: worst measured overhead is **12.9%**, not
    11% (so the allowance margin is 2.1pp, not 4); the 74.65 MP `convert` estimate is
    **3.40 GB = 3.16 GiB**, not "~3.25 GB"; the 18.66 MP before/after is
    **975 → 681 MB** (30%), not the unit-mixed "~930 MiB → 681 MB" (27%).
  - Nothing enforces the model against the code: the per-phase test compares it
    against the numbers in the module doc, so a new full-frame buffer is invisible
    until someone updates both. Stated plainly in the module doc now, since this
    round is proof it can happen.


## streaming-tiled-io

**Status:** not started
**Updated:** —

- Goal: Bound peak memory to a small working set (a few strips/tiles) instead of several whole-image buffers, by moving decode and encode toward **strip/tile streaming**: strip/tile decode, quantize-and-write encode strip-by-strip, avoiding materializing the full u16 and f32 images at once.


## transactional-output-writes

**Status:** not started
**Updated:** —

- Goal: Ensure a failed or interrupted `nc convert` never leaves a **partial or inconsistent artifact set** on disk.


## scanner-density-calibration

**Status:** not started
**Updated:** 2026-08-02

- Goal: Establish what a scanner's numbers mean in **absolute** density, so
  manufacturer-published densities are directly usable by reconstruction.
  `input-data-semantics` resolved transfer and meaning but not absolute
  normalisation — this closes that gap.
- 2026-08-02 (filed during `algo/reference-anchored-sigmoid` planning): two tiers,
  split by what they ask of the user. **Tier 1** uses only an unexposed frame, which
  the workflow already requires for `Dmin`: compute `−log10(scan)` per channel
  *without* dividing by `Dmin` and compare to the stock's published `D-min` (Ektar
  100: R ≈0.20 / G ≈0.56 / B ≈0.77). **Tier 2** adds a grey card or step wedge for a
  full offset+slope profile, and must stay strictly optional.
- **Known limit of tier 1, to be stated in its report rather than glossed:** one
  known density fixes a zero point, not a slope — and nc already anchors at the base
  by construction (`D = −log10(scan/Dmin)`), so a base measurement adds no new zero
  point. Its real value is supplying *three* known densities at once (one per
  channel, spanning ≈0.57 on Ektar), so a compressed spread is a slope signal. The
  irreducible ambiguity: a mismatch may be a wrong scale **or** a scanner filter
  whose spectral response differs from Status M. Report the ambiguity; don't pick.
- **A mismatch is not fatal.** Datasheets give the *relationship* between landmarks
  (mid-grey → diffuse white, Δ ≈ 0.36); a locally measured Δ gives the scale. Off
  scale just means deriving contrast from the measured Δ — a different number, same
  method. So the profile is a correction to apply, never a gate on conversion.
- Distinct from `color/scanner-profile-before-density-experiment`, which is about a
  *colour* transform before density conversion; this is about the *density scale*.
  Don't conflate them.
- 2026-08-02 (PR #68 Codex review, three findings accepted): **Tier 1 is a
  non-calibrating diagnostic, not a calibration.** `io::decode::normalize_u16` divides
  16-bit samples by 65535, so a scan value is a code-value ratio against *full scale*,
  not `I/I₀`. Scanner exposure and per-channel gains put an arbitrary offset between
  `−log10(scan)` and published `D-min`, so a perfectly linear scan can disagree with the
  datasheet for reasons that have nothing to do with scale. Absolute density needs a
  **same-settings open-gate reference measurement**.
- **The cross-channel-spread-as-slope argument was wrong and is removed.** The three
  channel readings are one point on three *different* response curves, each with its own
  gain and spectral sensitivity — not three points on one curve. A compressed spread can
  come from channel gains alone, and no channel has a second point from which a slope is
  identifiable; deriving a correction from it would corrupt colour. Slope needs a second
  known density *per channel*.
- **Tier 2 needs a calibrated transmission step wedge, not a photographed grey card.** A
  photographed card's developed density depends on illumination, exposure, processing and
  the characteristic curve, so it is not a known density and cannot pin offset+slope.
