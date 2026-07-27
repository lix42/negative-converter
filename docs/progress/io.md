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
- **Known gaps:** artifacts are still written straight to their final paths
  (`io/transactional-output-writes`), and peak memory is unbounded and honestly
  larger than the documented 4 GiB input limit implies — measured **~930 MiB at
  18.66 MP** on real scans (`io/memory-preflight`, then the evaluate-first
  `io/streaming-tiled-io`).
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

**Status:** not started
**Updated:** —

- Goal: Make the pipeline's memory use **honest and bounded** without changing its whole-image architecture.


## streaming-tiled-io

**Status:** not started
**Updated:** —

- Goal: Bound peak memory to a small working set (a few strips/tiles) instead of several whole-image buffers, by moving decode and encode toward **strip/tile streaming**: strip/tile decode, quantize-and-write encode strip-by-strip, avoiding materializing the full u16 and f32 images at once.


## transactional-output-writes

**Status:** not started
**Updated:** —

- Goal: Ensure a failed or interrupted `nc convert` never leaves a **partial or inconsistent artifact set** on disk.
