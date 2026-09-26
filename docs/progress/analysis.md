# Hanten — analysis Progress Log

Execution log for the `analysis` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

> **Consolidated 2026-09-13** (user-authorised; see CLAUDE.md's exception to the
> append-only rule). Sections of *done* tasks were rewritten as summaries keeping the
> decisions, gotchas and every measurement an open task cites; the full history is in
> git before that date. Sections of open and parked tasks are unchanged except: a
> 2026-09-14 entry under `nlp-comparison` records a supersession.

## Epic summary

What other epics need to know about `analysis`:

- **This epic verifies the pipeline; it is not part of it.** Everything lives in
  `scripts/`, and the hard invariant is that **only derived numbers and
  downscaled thumbnails leave the tools** — never sample pixels into context.
  Metadata comes from `nc inspect`; bytes are streamed only to hash.
- **`nctool metrics` reads output pixels (2026-09-02), and is the toolkit's only
  command that does.** Every other number here comes from `nc`'s own JSON report
  and therefore exists only for nc outputs; `metrics` measures any producer's
  image — NLP, SmartConvert, a hand-edited export — on the same footing. It is
  also the only command that is not stdlib-only (`numpy`, `tifffile`, via
  `scripts/analysis/requirements.txt`, plus `Pillow` for JPEG; CI installs them into
  a venv and sets `NCTOOL_REQUIRE_DEPS=1`). It still emits derived numbers only. `metrics image`
  measures one file; `metrics roll` measures a converted roll and rolls the scalars
  up into a spread table; `metrics table` re-renders that as Markdown. Four facts
  other epics may want: an input's colour space must be **declared**, never
  inferred (except from a run's frozen recipe, which is provenance); a full-frame
  measurement of an uncropped scan measures the **film holder** as much as the
  picture (it renders to white — excluding it moved one frame's median by 3.1
  stops); `cast_by_tone_band` is the **crossover** detector, the one colour number
  a negative conversion turns on; and a roll's spread is **not attributable** to
  the calibration, because scene content is mixed into it.
- **Comparing renders by eye is one command (2026-09-12).** `nctool review generate
  <matrix.json>` renders every (frame x config) cell a matrix names, writes each rendition's
  `nctool metrics` record beside it, and emits the `review.json` that `tools/review-app`
  reads; `scripts/preset-review/presets.matrix.json` is the worked example. Two things other
  epics will care about: the matrix is **data**, so comparing a new configuration is an edit
  to a JSON file rather than to any script, and a review set now carries its **measurements**,
  which the app draws as tone and cast charts under each picture. It needs `../nc-assets` and
  the metrics venv, so it is not in CI, and its output goes to a throwaway directory outside
  the repo — the frames are the user's own photographs. **Every cell is an `nc convert`**, so
  a study comparing nc against an *outside* reference (NLP's own TIFFs) cannot be expressed
  as a matrix today; `film-base/dmax-per-channel-reduction` (parked 2026-09-13) asks for a
  reference-cell kind rather than a bespoke script.
- **A review set can compare two *builds* since 2026-09-22.** A matrix declares
  `builds` (an id, a label and a path to a **pre-built** binary each); the generator
  renders every config under every build and flattens the product into cells named
  `<config>@<build>`, so the app keeps its one toggle and its one grid cell per frame.
  What other epics need to know: the cell's provenance is **derived, never declared** —
  each config's `producer` block in `review.json` carries the identity that binary
  reported about itself, and a build that reports two identities inside one run aborts
  it. `nf-verification/reference-snapshot` is the intended first consumer, and the
  pre-flight accepts the **pre-rename `nc` banner** precisely so a git-tagged reference
  binary is usable. `--nc` is refused beside a `builds` block; `--build <id>=<path>`
  repoints one arm for the rebuild loop.
- **The tone bands are cut in CIELAB lightness, and the record is `schema_version`
  2 (2026-09-10).** Edges every 15 L\* to 75, then diffuse white (L\* 100), then an
  overflow band above it — `deep_shadow, shadow, low_mid, mid, high_mid,
  highlight, above_diffuse_white`. They were even in *stops* through schema 1
  (-4 / -2 / +2 / diffuse white), which put a median 83% of a real frame in `mid`
  alone. **Anyone quoting a band share from before 2026-09-10 is quoting the old
  definition**; `metrics table` refuses a schema-1 record rather than rendering it
  under the new labels. The cut is stated inside every record (`record.bands`), so
  an artifact carries its own definition. `cast_by_tone_band` entries now carry
  `pixels` and a `sparse` flag (under 0.1% of the region), and the rollup withholds
  `crossover_*` when either contributing band is sparse.
- **`tone.histogram` is the only list-valued field in any of this toolkit's
  records.** Four series — luminance and each of R/G/B — binned one count per L\*
  unit over **L\* 0..200**, i.e. to twice diffuse white, with `above_range` past
  that and separate counters for samples with no lightness. Diffuse white sits on
  the bin-100 boundary and the record names `mid_grey_bin` / `diffuse_white_bin`.
  The `luminance` series uses the **declared space's own luma weighting**, the same
  one `tone.percentiles_stops` is built from, so the two cannot disagree — and it
  is emitted rather than derived at draw time because luma is a weighted sum of
  linear channel values and is not recoverable from three per-channel histograms.
  It is there for `analysis/metrics-visualization` to draw; the band edges fall
  exactly on bin edges, so bands and bars share one axis. It streams, so it costs
  ~0.6 s at 18.7 MP and no measurable memory; a record is ~8 KB.
- **For anyone consuming nc's ProPhoto output:** `color::build_profile` writes a
  **pure 1.8** power law, omitting the ROMM linear toe the standard specifies. A
  decoder applying the toe disagrees with nc's own pixels below encoded 0.03125 —
  1.3 stops out at 0.01, in exactly the samples deep-shadow statistics are made of.
  `metrics` therefore carries two ProPhoto spaces and maps nc's output to the pure
  one.
- **Real-scan core verification is done (2026-07-22/23)** across five rolls; the
  write-up is [`docs/reports/real-scan-verification.md`](../reports/real-scan-verification.md)
  and the rerunnable harness plus frozen recipes are under
  `scripts/real-scan-verify/`. The execution record itself is in
  [`_unassigned.md`](_unassigned.md) — in the flat log it was nested under the
  `color-management planning` heading, so the epic split carried it there
  verbatim. Read it there.
- **The numbers other epics are waiting on:** all assets are HDRi with IR;
  standard frame 5184×3599 ≈ 18.66 MP; measured **peak ~930 MiB @ 18.66 MP
  (~50 MiB/MP)**, ~1.6 s wall — about 1.5× the design's model, which omits the
  carried IR plane and the `to_output` clone. That is the STEP 0 input for
  `io/streaming-tiled-io`: `io/memory-preflight` is required, streaming is a
  conditional GO pending a post-preflight re-measure. (`io/memory-preflight` has
  since shipped and re-measured: 975 → 681 MB at 18.66 MP, 3.808 → 3.146 GB on the
  74.65 MP `largest.tif` — see `docs/progress/io.md`; STEP 0 is still unwritten.)
- **Also found:** `--auto-base` fails loudly on every real frame (correct, given
  the holder layout — use the measured-reference workflow); u16 output clips
  4.8–10.3% high by default, routed to the display-output roadmap; float output is
  byte-lossless; determinism is byte-identical.
- **Assets live in a shared Google Drive folder**, reached through a
  **machine-local, uncommitted symlink** `../nc-assets`. The tracked inventory is
  `manifest.json` **at the assets root**, with paths relative to its own directory
  (no `asset_root` field), so it is machine-portable. sha256 is recorded for
  irreplaceable data and omitted for regenerable nc outputs.
- **`python -m nctool manifest {generate,validate,roles}`** is the entry point
  (stdlib only; needs `scripts/analysis` on `PYTHONPATH`; `--asset-root` defaults
  to `$NC_ASSET_ROOT`, else `../nc-assets`).
  `scripts/analysis/generate_manifest.py` is now a thin shim.
  `generate` is idempotent — a re-run must stay byte-identical. `validate`
  **reports only, never deletes**, and exits 0 clean / 1 discrepancies / 2
  operational. The harness's roll list comes from `manifest roles`, not a
  hard-coded array.
- **`python -m nctool compare {run,diff}`** was added by
  `core/conversion-versioning` (logged in `docs/progress/core.md`): it converts the
  fixed benchmark set in `scripts/analysis/benchmark.json` under one `nc` build and
  diffs two builds keyed on `pipeline_version` + commit. It **reuses this epic's
  inventory** — a benchmark case names a roll + frame *stem* and resolves its path
  and `sha256` through `manifest.json`, so there is still exactly one asset
  inventory — and reads only derived numbers (the report's `output_stats` / `loss`
  and the telemetry record's timings), never pixels. It records the digest of the
  bytes it actually converted (`input_sha256` + `checksums: verified|computed|skipped`)
  so a comparison's input identity is provable from the artifact, not just from the
  exit code.
  **`compare`'s exit codes deliberately differ from `manifest`'s above:** `0` = the
  comparison ran and its verdict (identical or differing) is the report — a
  discrepancy between two *different* builds is the normal answer here, not a fault;
  `1` = the comparison failed or proved a broken invariant (a case would not convert,
  input checksum drift, cases disagreeing about the build, or one build producing two
  different results); `2` = operational/usage. The determinism claim in particular is
  **precondition-guarded** — it fires only once every *other* explanation for the
  difference is ruled out (clean pinned source, same frame set, same input digests with
  no skipped checksum, same output depth, same per-frame `params_hash`); a failed
  precondition is rc 0 plus a `determinism_check_blocked` note, never an accusation.
  Documented in `compare.py`'s module docstring and `determinism_blockers`.
- **Reference comparison (NLP and hand-edited targets) pairs by manifest
  `source_frame` identity, never by registration.** Global metrics carry it; a
  pixel-wise section is an opt-in gated on exact dimension equality (2026-09-02).
  The reference set is **two export regimes with different declared spaces** —
  11 float files, linear sRGB/709 (`nlp/2026-07-23`, `2026-07-24`, `2026-08-04`)
  and a 32-file 16-bit Adobe RGB batch (`nlp/2026-09-09`) that is **pixel-aligned
  with its sources** and is the set any number that has to hold up should use
  (2026-09-11). A `nlp/2026-09-11` Portra400 batch (32 files) has since been added
  and registered but not surveyed.
- **Open question for the user:** the committed `recipes/*.hdr.json` key order
  lags the current harness `jq` (values identical); a `freeze` re-run will
  reorder them.
- **The analysis stdlib suite is CI-gated on Linux and macOS (2026-08-11).** It
  includes a hermetic real-binary `freeze` → `convert` harness test plus a fake
  successful-wrong-container regression. The Drive-backed verification matrix
  remains manual; CI protects its CLI/recipe/container plumbing.


## real-scan-verification
**Status:** done (2026-07-23)
**Updated:** 2026-09-13 (consolidated)

- Goal: run the verification matrix (inspect/estimate/convert/IR/determinism/
  resources) against the full-size real scans; record results, file follow-up
  tasks for defects. Narrowed 2026-07-21 to the current TIFF pipeline so full-size
  resource measurements could run before the HDR/display roadmap; final preset and
  cross-device checks moved to `display-output-acceptance`.
- **Execution record:** [`_unassigned.md`](_unassigned.md) (`### Real-scan core
  verification — executed 2026-07-22`), carried there verbatim by the epic split.
  Write-up: [`docs/reports/real-scan-verification.md`](../reports/real-scan-verification.md).
  Harness + frozen per-roll recipes with provenance: `scripts/real-scan-verify/`.
- **What downstream tasks take from it:** every matrix row passed on five rolls;
  per-roll `Dmin`/`Dmax` frozen from a holder-free centre-40% region (4/5 rolls
  clean — Harman Phoenix trips the C41-calibrated `Dmax ≳ 1.0` floor and the
  base-uniformity check, filed as `film-base/dense-base-dmax-plausibility`);
  measured peak **~930 MiB @ 18.66 MP** (the `io/streaming-tiled-io` STEP 0
  input); u16 clips 4.8–10.3% high (display-output roadmap); float byte-lossless;
  determinism byte-identical. `display-output-acceptance` reuses the same asset
  classes and frozen recipes.


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


## conversion-analysis-tooling
**Status:** done (spike, 2026-07-23)
**Updated:** 2026-09-13 (consolidated)

- Spike outcome, decided with the user: a **Python package** `scripts/analysis/nctool/`
  as the single entry point (`python -m nctool …`); a **JSON manifest** of rolls +
  converted outputs with human-seeded roles (`unexposed|leader|real`) and
  `nc inspect`-derived facts, replacing the harness's hard-coded `ROLLS` array,
  with `validate` as the cleanup-surfacing mechanism (reports, never deletes);
  **NLP comparison by manifest `source_frame` identity, no registration.** Split
  into `asset-manifest` → `conversion-metrics` → `nlp-comparison`, plus
  `drive-asset-migration`. Details in the task file's "Spike outcome".
- One decision did **not** survive: the spike said `harness.sh` would retire or
  become a shim. It did not — it is the real-scan verification driver, now
  fixture-tested in CI (`harness-regression-tests`, 2026-08-11).
- **2026-07-24 — the assets moved to Google Drive** (`…/My Drive/temp/nc-assets`,
  12 GB), reorganised into `rolls/ samples/ converted/{nc,nlp}/` (experiment
  fixtures dropped — repo tests use `tests/fixtures/`; `converted/nc/V0` kept as the
  v0-baseline set), with `samples/largest.tif` (10368×7200, **74.6 MP**, HDRi) as
  the perf worst-case. The manifest was rethought to live **at the assets root**
  with paths relative to its own directory, so it is machine-portable; sha256 for
  irreplaceable data only. The repo's `../nc-assets` convention is bridged by a
  **machine-local, uncommitted symlink** `~/src/nc/nc-assets → <Drive>/temp/nc-assets`,
  so CLAUDE.md, the harness (`A=../nc-assets`) and the reports work unchanged
  across worktrees. `drive-asset-migration` owns what remains (materialisation
  guard, sync hygiene).


## asset-manifest
**Status:** done (2026-07-24)
**Updated:** 2026-09-13 (consolidated)

- Shipped `scripts/analysis/nctool/manifest.py` — `generate` / `validate` /
  `roles` over shared directory walkers, so generate's structured build and
  validate's on-disk set cannot diverge — and the `python -m nctool` dispatcher
  (`--asset-root`, default `$NC_ASSET_ROOT` → `../nc-assets`).
  `generate_manifest.py` is a backward-compat shim; the `asset-manifest` skill
  documents when and how to regenerate. `harness.sh` fills its roll list from
  `manifest roles` (only rolls with exactly one unexposed + one leader are emitted)
  and fails loud (exit 2) if the manifest or `roles` fails.
- **Contracts other tools rely on** (cited by `nctool compare` and `roll`):
  `generate` is idempotent and byte-identical on re-run, preserves human fields
  (role/stock/kind/note, bucket `regenerable`/`nc_version`/`recipe_dir`), and
  recomputes sha256 by default (`--reuse-hash` opts into size-based reuse).
  `validate` reports **drift** / **missing** / **orphans** (full-tree `.tif`/`.tiff`
  scan, so a stray at any depth is flagged) and treats a non-regenerable entry with
  no sha256, or an entry carrying an `error`, as a PROBLEM; exit **0 clean / 1
  discrepancies / 2 operational**; never deletes; needs no `nc`. A wholesale
  nc-absent `generate` exits 2 unless `--allow-exiftool-fallback`; an `nc inspect`
  that returns unparseable JSON is a per-file `error`, not a silent exiftool
  downgrade. Writes are `mkstemp` + `fsync` + `os.replace`.
- **Roles.** `roles` folds an unrecognised role into `real` with a loud warning
  (it once silently bucketed a typo into a phantom key and dropped the frame).
  Relevant to `calibration-frame-capture`, whose bracketed target frames need a
  fourth role plus exposure offset and lighting — the existing three describe a
  roll's frames only.
- **Known inference to fix with real provenance** (`manifest.py`, the `bits == 16`
  arm): every 16-bit nc output is labelled `u16-srgb`, which is wrong for a
  `display-p3` result; tracked under `output/sdr-preset-followups`.
- Verified at the time: `generate` reproduced the live manifest byte-identical
  (6 rolls, 5 samples, ~12 GB hashed in ~10 s); manifest-driven `freeze` produced
  recipes byte-identical to the hard-coded array. **Open:** the committed
  `recipes/*.hdr.json` key order lags the harness `jq` (values identical), so a
  `freeze` re-run reorders those keys — decide whether to refresh them.
- Tests: `scripts/analysis/nctool/test_manifest.py`, hermetic, `nc` stubbed.


## conversion-metrics

**Status:** done (2026-09-03)
**Updated:** 2026-09-13 (consolidated; the 2026-09-10 band re-cut is kept in full)

- Goal: a deterministic, pixel-derived metric artifact for *any* output image,
  regardless of producer, describing tone, colour, range use and endpoint
  behaviour well enough that two renderings can be compared numerically. It
  describes, it does not rank. 2026-08-12: the briefly separate
  `photographic-result-analysis` follow-up was folded in.
- **Decisions (2026-09-02), all still standing:** `numpy` + `tifffile` (+ `Pillow`)
  in a venv — `nctool` stopped being stdlib-only, the import is lazy, CI installs
  them and `NCTOOL_REQUIRE_DEPS=1` turns a forgotten install into a failure rather
  than 29 skips under a green `ok`; **every input's colour space is declared,
  never guessed** (measuring one file as `linear-srgb` instead of `srgb` moved the
  key 2.22 stops with no error either way — and the manifest's `encoding` names
  depth, not transfer); measure in linear light in one common space; tone in log2
  stops relative to 0.18; regions are fractions (`--inset` / `--region`) and are
  recorded in the artifact. Colorimetry is transcribed from `definitions.rs` and the
  tests re-read it, so a one-sided edit fails on both sides — which is also why
  **`ADOBE_RGB` was defined in the Rust first** (2026-09-02) although nc renders to
  no such space. Its `--help` space list is built from `metrics.SPACES` and pinned
  by a test, after it went stale by hand once.
- **Shipped as `nctool metrics {image,roll,table}`** (tone slice and colour slice
  2026-09-02, roll rollup + Markdown table + JPEG input 2026-09-03, band re-cut and
  histogram 2026-09-10). Verified against nc's own `loss.*` counters — the one
  independent source of truth: `clipped_high` 0.103282844 reported, 0.103283
  measured. Tests: `test_metrics.py`.
- **The film holder *is* the highlight distribution on an uncropped frame, and it
  is measurable** (2026-09-02). It blocks all light, so it is maximum density and
  renders to white — the top code. Ektar 971 (`display-p3`, default sigmoid): top-code
  share 0.0950 → 0.0134 → 0.0018 → 0.0000 for insets 0 / 0.05 / 0.10 / 0.15, with
  `shoulder_span_stops` recovering 0.000 → 0.417 and `above_diffuse_white` 0.0541 →
  0. On Portra160 1102 the white border moved the *median* by **3.1 stops** between
  the full frame and a centre-76% region (−2.30 → −5.42). So a full-frame
  measurement of an uncropped scan is not a measurement of the picture; the region
  parameter is not a convenience. A 5% inset does not clear a real holder (it can
  occupy 10–15% of one edge); excluding dark pixels as a holder proxy would bias the
  shadow statistics being measured — region only. (Relevant to
  `algo/auto-anchor-interior-measurement`'s no-IR inset question.) The `legacy`
  render on that frame does genuinely clip (10.3%) and its top-code population
  survives the holder's removal (2.85% at inset 0.15).
- **Colour** (`color_stats`): per-channel balance in stops, mean cast and chroma,
  chroma percentiles, neutral share, six hue sectors, and **the cast of each tone
  band separately** — the crossover detector, because the characteristic
  negative-conversion fault moves shadows one way and highlights the other and
  averages to nothing. On the Portra160 1102 pair nc goes `b* = −0.7` (deep
  shadow) → `−32.2` (mid) where NLP goes `−0.1` → `−3.8`, with nc's blue 0.715 stops
  hot against green. CIELAB's reference white is **derived from this module's own
  D65**, not the tabulated triple, so an RGB-neutral frame reads `a* = b* = 0`
  exactly; `display-output-acceptance` pins the tabulated white for its absolute
  cross-encoding oracle and the two coexist deliberately (a test pins the
  difference). Colour streams in row blocks; every colour fraction is over the
  region, with `measured_fraction` stating how much had measurable luminance.
- **Memory, measured:** 1.18 GB peak at 18.66 MP after the tone slice (~63 B/px),
  1.43 GB with colour (~77 B/px) — **~5.7 GB extrapolated to 10368×7200**, inside
  nc's 6 GiB default but not by much. Nothing gates it.
- **Roll rollup (2026-09-03):** `metrics roll <roll> <run>` writes `metrics.json`
  beside the tag (per-frame records + a spread table over thirteen axes plus two
  crossover terms); `metrics table` re-renders it. **The spread, not the mean, is
  what a roll gets — and it does not measure the calibration**: one frozen recipe
  served every frame, so variation combines scene content with calibration fit
  (Ektar under `display-p3`: key spread 1.12 stops, `b_over_g` 0.62 across three
  frames — as easily three scenes as a bad fit). No outlier rule, no verdict; the
  extreme frames are named. **The colour space is resolved from the run's frozen
  recipe**, established by conversion + exiftool: `display-p3` and `film-master`
  carry the *same* ICC description ("RGB built-in"), so only the preset can tell
  them apart; refused with reasons are the AVIF presets, the PQ/HLG TIFFs, an
  `--output-profile` path, and an f32 `legacy` TIFF whose transfer is unverified.
- **JPEG input (2026-09-03):** `--jpeg-image sdr|hdr` (default `sdr`) selects which
  rendition of a gain-map JPEG to measure. `sdr` reads the base (Pillow opens an nc
  gain-map JPEG as a single frame, so the appended gain map is never touched); `hdr`
  is refused in two tiers — "no gain map" before "reconstruction is not
  implemented", deliberately, since applying the metadata slightly wrong yields
  plausible wrong numbers and `hdr-linear-tiff` is the HDR signal with no
  container. Gain-map detection is a **marker walk**, not a count of `FFD8FF`
  across the file (an EXIF APP1 embeds a whole thumbnail JPEG, so every camera and
  Lightroom export had reported a gain map). Records carry `image.container`,
  `bits_per_sample`, `decoder`, `gain_map_present`, `jpeg_image`. Verified against
  the TIFF path: 16-bit TIFF, 8-bit TIFF and JPEG q100/q85 give identical keys and
  p95, diverging only at p0.1 — mostly bit depth, not codec. **This moved
  `gain-map-hdr` and `ultra-hdr-v1` from refused to measurable as `display-p3`**,
  which falsified the earlier "nc's default is among the refused" claim and
  `nlp-comparison`'s "the metric reader will not open it" — so a default roll and
  the NLP JPEGs are comparable without a re-render, but `gain_map_present` has to
  reach any comparison output or a P3 SDR base gets compared as the whole rendition.
- **Durable gotchas from the review rounds** (all fixed, all regression-tested):
  a planar TIFF must be detected from the file's own `planarconfig`, not the
  decoded array's shape; the three channels' geometric means must share one pixel
  support or `b_over_g` reads neutral on a frame with half its blue crushed (ratios
  are withheld and `color.balance_support` says so); `crossover_a/b` difference the
  **shadow and mid** bands (the `highlight` band is often empty), and the prose says
  so; `np.histogram(range=…)` discards out-of-range values, so saturated samples
  are clipped into the top bin and `max_chroma` reported exactly; nc's ProPhoto is
  a **pure 1.8** power law, so two ProPhoto spaces exist (`prophoto` for ISO
  22028-2 exports, `prophoto-gamma1.8` for nc's); percentages live only in the
  Markdown report, the JSON keeps fractions; the report header names the build and
  marks `git_dirty`.

- 2026-09-10: **Re-cut the tone bands in CIELAB lightness, and added the per-channel
  histogram.** `schema_version` 1 -> 2.
  The old cut was even in stops — `-inf / -4 / -2 / +2 / diffuse white`, after Zones
  III and VII — and even steps of exposure are uneven steps of anything a viewer
  sees: `shadow` 2.00 stops wide, `mid` 4.00, `highlight` 0.47, an 8.5:1
  discontinuity. Measured through the shipped code path on 33 real renders (frames
  G1/G2/G3/E1/E2/P4 through all five `nc convert --preset` bundles, plus the three
  Negative Lab Pro references for the Gold200 roll, `display-p3` / `srgb`, 5% inset):
  `mid` held a **median 82.6% of the frame and 95.0% at worst**, `highlight` read
  under 0.1% on 15 of 33, and `above_diffuse_white` on all 33. Worse than
  uninformative: `cast_by_tone_band` still emitted a `highlight` entry off whatever
  pixels happened to be there, and nothing in the entry said how many. On
  `G2-chr-aim` that band's cast — `a* = -19.5`, the largest colour excursion in the
  record — was the colour of **one pixel** out of 15.1 million, printed beside a
  `mid` cast resting on 91.7% of the frame.
  **Seven cuts were scored on the same 33 renders before choosing**, every one
  digitized off the same `stops` array `tone_stats` uses, so the comparison is
  exact rather than re-binned. Largest band as a share of the frame (median over
  the 33, then the worst single render), and how many of the `33 x bands` band
  shares came out under 0.1%:

  | cut | bands | median | worst | <0.1% |
  |---|---|---|---|---|
  | A old: stops -4 / -2 / +2 / white | 5 | 82.6% | 95.0% | 68/165 |
  | B Adobe: quartiles of the encoded axis | 5 | 64.5% | 79.3% | 43/165 |
  | C Zone: 1-stop bins | 10 | 37.4% | 53.9% | 96/330 |
  | D Zone framing, `mid` subdivided | 6 | 58.7% | 72.4% | 68/198 |
  | E L\* 20 / 40 / 60 / 80 | 6 | 53.2% | 66.2% | 45/198 |
  | **F L\* 15 / 30 / 45 / 60 / 75 (chosen)** | 7 | **46.3%** | **56.2%** | 44/231 |
  | G L\* every 12.5 | 9 | 39.7% | 52.7% | 72/297 |

  (Every cut's `above_diffuse_white` band accounts for 33 of its own `<0.1%`
  count: all 33 renders are SDR. Net of it, A is 35/132 and F is 11/198.)
  **B**, Adobe's parametric-curve splits converted to stops (-1.82 / +0.25 /
  +1.54), named the *shape* of the answer — narrowing smoothly, because equal
  steps in an encoded domain compress in stops — but Adobe's regions are
  overlapping weighting regions for editing, not disjoint measurement bins. **C**,
  the literal Zone system, is the wrong shape at both ends on a display-referred
  render — three whole bands whose *median* is under 0.1%. **D** shows the framing
  was the problem, not the band count.
  **Chosen: equal steps of CIELAB L\***, every 15 to L\* 75, then diffuse white
  (L\* 100), then an overflow band. Why L\* rather than the encoded axis: it is the
  **same perceptual space the colour stage already measures cast in**, and splitting
  sRGB's curve would have made nc's own output encoding the authority for measuring
  everyone else's. The two agree closely anyway, which is itself the argument that
  the family is right and the choice within it is not delicate.
  **Measured before -> after, same code path, both cuts:** largest band median
  **82.6% -> 46.3%**, worst **95.0% -> 56.2%**; `highlight` median **0.85% -> 3.84%**
  and frames under 0.1% **15/33 -> 6/33**; `deep_shadow` frames under 0.1% **20/33
  -> 5/33**; cast entries resting on under 0.1% of the region **18 of 120 -> 12 of
  200**. The headline is discrimination: across the five presets of frame G2 the
  old band vector spread **2.1 percentage points** (`mid` 91.70 / 93.78 / 91.81 /
  93.26 / 92.84), the new one spreads **24.0** (`low_mid` 51.34 / 27.32 / 49.71 /
  30.02 / 28.97).
  **Why 15 and not 20, and why stop there.** L\* 20/40/60/80 (**E**) was rejected
  on measurement: its largest band still takes 66.2% of one real frame against
  56.2%. Going finer stops paying — L\* every 12.5 (**G**) buys 3.5 points of worst
  case for two more bands and takes the sparse-entry rate from 5.6% to 14.8%.
  **`above_diffuse_white` stays, and is a deliberate exception to "no band empty on
  a normal frame".** It is an overflow bin: an SDR rendition cannot populate it,
  but on `film-master` or `hdr-linear-tiff` it is the only place in the tone stage
  where headroom above display white appears.
  **The population rule is a caveat, not a filter.** Every `cast_by_tone_band`
  entry now carries `pixels` and `sparse` (under `BAND_SPARSE_FRACTION`, 0.1% of
  the region). Sparse entries are **kept** — a band set that varies frame to frame
  cannot be diffed — but the rollup's `crossover_a`/`crossover_b` are withheld when
  either contributing band is sparse. `highlight` is now a tracked rollup axis.
  **`tone.histogram`**: the record's first list-valued field. Four series
  (`luminance`, `r`, `g`, `b`), one count per L\* unit, plus per-series counters
  for samples above the range and for those with no lightness — they partition the
  region, and a test pins that. L\* and not stored code values (which describe the
  encoding), and not stops (which give black an unbounded tail); and because the
  bands are cut on the same axis **every band edge falls exactly on a bin edge**, so
  one chart can shade bands over bars without interpolating (a test breaks if
  either side leaves integer L\*). The channel series apply the same L\* curve to
  one channel, which is a level and not a colorimetric lightness — stated in the
  record. Streams in row blocks: **+0.55 s** at 18.7 MP and no measurable memory.
  **The record states its own band cut** (`record.bands`: domain, names, L\* edges,
  stops edges, sparse threshold), because the edges have now moved once. `metrics
  table` **refuses** a record whose `schema_version` is not the current one: every
  column label still fits a schema-1 record, so rendering it would silently compare
  two definitions of shadow. `nctool roll` and `nctool compare` carry their own
  schema constants and never read a metrics record; `scripts/real-scan-verify/`
  does not use `metrics` at all.
  Verified: 226 analysis tests, each new invariant confirmed to fail when broken.

- 2026-09-10 (follow-up): **Histogram range extended past diffuse white.**
  **The range now runs L\* 0..200 in 200 bins**, not 0..100 in 100. A float or HDR
  rendition genuinely carries samples above display white and a scalar overflow
  counter **cannot be drawn** — L\* 200 is 6.46x diffuse white (+5.17 stops),
  covering nc's own 1000/203 HDR ceiling (L\* 181.4) with margin. And putting white
  at the *edge* of the axis hid the commoner SDR question: how far short of diffuse
  white the highlights stop. Measured on the five preset renders of G2, the last
  non-empty luminance bin sits at L\* **88 / 92 / 88 / 92 / 98** — `sig-knees` the
  only one that nearly reaches it (input to `algo/contrast-latitude-spike`).
  Diffuse white is the bin-100 boundary; the per-series overflow counter is
  `above_range`. Cost unchanged: +0.60 s at 18.7 MP, a record 6.8 -> 8.2 KB. The
  record names `mid_grey_bin` (49) and `diffuse_white_bin` (100).
  **A test pins that the `luminance` series and `tone.percentiles_stops` describe
  the same quantity** — and the obvious break does not work: monkeypatching
  `luminance_weights` moves both together. Breaking only the histogram's weighting
  is the real check, and it needed a strongly channel-separated fixture and all
  eleven percentiles asserted before a Rec.709-on-P3 swap failed it.
  **Survey of how other tools bin tonal regions:** no surveyed tool defines
  disjoint bins for *measurement* — Adobe's parametric curve, darktable's `color
  balance rgb` and RawTherapee's shadows/highlights are all overlapping *editing*
  weights. The one disjoint binning found is darktable's tone equalizer (nine
  zones, 1 EV apart, −8 to 0 EV) — candidate **C** above, which this task rejected
  on sparsity, not principle: in an editing tool an empty zone is a harmless
  slider, in a measurement record it is a reported number with nothing behind it.
  RawTherapee draws its L-curve histogram in CIELAB L\*, the domain chosen here.

## drive-asset-migration

**Status:** not started
**Updated:** —

- Goal: Make working from the Google Drive-hosted asset folder robust across machines, now that the assets — inputs *and* conversion outputs — physically live there (moved and reorganized 2026-07-24, with a self-relative `manifest.json` at the root). The move and reorg are done; this task covers the remaining robustness/tooling and the repo path-convention decision.


## nlp-comparison

**Status:** not started
**Updated:** 2026-09-11

- Goal: Ingest Negative Lab Pro (NLP) conversion outputs (the user adds them to `nc-assets`) and compare them against nc's outputs: global per-image metrics side by side, plus side-by-side downscaled thumbnails.
- 2026-09-02: Task rewritten and widened from "NLP vs nc" to reference comparison,
  after the user pointed out that NLP is not ground truth — they edit its results and
  can contribute those edits as assets. References therefore carry a role: `reference`
  (another tool's output as it came) versus `target` (an image edited to the wanted
  result). That yields three deltas per axis, and makes `|nc − target| < |NLP − target|`
  the acceptance question; the NLP→target spread also supplies the scale for what counts
  as a meaningful difference, instead of a picked tolerance. Re-verified the asset facts
  the no-registration design rests on: `nlp/2026-07-23` is 4406×2930 against a 5184×3600
  source and its **aspect ratio differs** (1.504 vs 1.44), so the crop cannot be undone
  arithmetically — but `nlp/2026-08-04` is full-frame 5184×3600, so an opt-in pixel-wise
  section gated on exact dimension equality will genuinely engage on some sets.
  `converted/SmartConvert/TIFF` is present but carries neither a `source_frame` nor an
  ICC profile, so it is unpaired until both are declared by hand. Noted that nc's default
  gain-map JPEG is unreadable by the planned metric reader, so comparison runs go through
  a TIFF preset.
- 2026-09-10: **First measured nc-versus-NLP numbers, recorded as a starting point
  rather than acted on.** They fell out of the `analysis/conversion-metrics` band
  re-cut, which needed a non-nc producer to score candidate cuts against. Nothing
  here changed a render: the preset brightness target was approved by the user on
  2026-09-09 (see `docs/progress/algo.md`) and re-calibrating it is not this
  evidence's call.
  Three Gold200 frames have an NLP reference for the same source
  (`converted/nlp/2026-07-24/` 1137 / 1144 / 1151 = G1 / G2 / G3). Measured with
  `nctool metrics image --inset 0.05`, nc through `--preset chr-generic` and
  `sig-flat` at `--output-preset gain-map-hdr` (`display-p3`), NLP read as `srgb`.
  **`metrics` was not built to compare across producers at this precision**: the
  NLP files are cropped to 4897x3265 against a 5184x3600 source, so a 5% inset of
  each is not the same picture content, and there is no registration.
  **The colour space of the NLP file is unresolved, and the choice moves every
  number.** Its ICC description is `sRGB IEC61966-2.1 (Linear RGB Profile)`, which
  is self-contradictory, and `exiftool` reports `ColorSpace: Uncalibrated`. Samples
  are 32-bit float and **not** bounded to [0, 1]: full-frame min -0.022096, max
  1.284324, with 0.008% of samples below 0 and 0.096% above 1 — an unclamped
  export, which is consistent with either reading. The distribution argues for the
  gamma reading without proving it: full-frame median 0.5646 decodes to +0.63 stops
  over mid grey if sRGB-encoded and to +1.65 if linear, and a normal photograph's
  median does not sit 1.65 stops over mid grey. **That is plausibility, not evidence
  from the file**, so both readings are reported below and the conservative (gamma)
  one is used for the comparison. Resolving it needs either the NLP/Lightroom export
  setting from the user, or a known-value target pushed through the same NLP path —
  do that before any acceptance number is derived from these files.
  Median luminance, in stops over mid grey — nc `chr-generic` / `sig-flat`, then NLP
  read both ways:

  | frame | nc chr-generic | nc sig-flat | NLP (gamma) | NLP (linear) |
  |---|---|---|---|---|
  | G1 | -0.71 | -0.78 | -0.19 | +1.22 |
  | G2 | -0.05 | -0.10 | +0.77 | +1.70 |
  | G3 | -0.99 | -1.11 | -3.86 | -0.71 |

  **The "nc is darker" reading is not established, and which way it goes on one
  frame depends on the unresolved colour space.** Under the conservative gamma
  reading it holds on two frames and **reverses on the third**: NLP's median sits
  0.52-0.59 stops above nc's on G1 and 0.82-0.87 above on G2, but **2.75-2.87
  stops below** on G3, where 56.2% of the frame lands in `deep_shadow` against nc's
  0.20-0.29%. Under the linear reading NLP is brighter on all three (+1.93-2.00,
  +1.75-1.80, +0.28-0.40). So the reversal is a property of the *gamma* reading,
  not a fact about the two tools, and the direction cannot be stated at all until
  the colour space is resolved. Three frames could not settle it in any case, and
  G3 is the frame `sigmoid-baseline`'s fixtures already flag as exceeding SDR range
  (sky best at +0 EV, trees at +2) — exactly where a per-frame auto-adjustment and
  a frozen recipe should disagree most.
  **The difference that is consistent across all three frames is contrast, not
  brightness.** nc's `contrast.p95_minus_p5` is 3.55-4.51 stops on every frame and
  both presets; NLP's is **6.98 (G2) / 10.92 (G3) / 11.52 (G1)** under the gamma
  reading and **3.83 / 7.35 / 8.14** under the linear one. Under gamma NLP is wider
  on all three, by +3.28 to +7.60 stops. Under linear it is wider on G1 (+4.09 to
  +4.22) and G3 (+2.84 to +3.03) but **essentially tied on G2** (+0.13 to +0.28),
  so the gap survives both readings on two frames and collapses on one — still a
  stronger signal than the median, which survives neither cleanly. What holds
  unconditionally is the *stability*: nc's figure moves 0.96 stops across the three
  frames where NLP's moves 4.54 (gamma) or 4.31 (linear), the signature of one
  frozen recipe against a per-frame adjustment. That, not the median, looks like
  the thing worth investigating first.
  **Highlight occupancy on G2 is the one comparison that survives everything so
  far**: NLP 19.4% of the frame in `highlight` against nc's 1.4-1.8% under the
  gamma reading, 68.0% under the linear one — same direction, larger under the
  reading that is not being used. It does not generalize, though: on G3 nc holds
  **more** (15.9% against NLP's 14.0%).
  All of these are single-frame measurements of differently cropped images with an
  unresolved reference colour space. They are a starting point for this task, not a
  finding about either tool.
- 2026-09-11: **The colour space is resolved: the NLP files are linear, sRGB/709 primaries,
  32-bit float — so the "gamma reading" used above is the wrong one and every number
  derived from it should be read as superseded.** Resolved from the files themselves, not
  from the user: the embedded ICC profile (520 bytes, all 11 files in
  `converted/nlp/*/`) carries `rTRC`/`gTRC`/`bTRC` of type `curv` with `count=1,
  gamma=1.00000`, and `rXYZ = (0.436035, 0.222488, 0.013916)`, which is sRGB/Rec.709
  adapted to D50 (AdobeRGB's red colorant would be ~0.6098). **The lesson is where the
  authority lies:** `exiftool`'s `ProfileDescription` says
  `sRGB IEC61966-2.1 (Linear RGB Profile)` — self-contradictory, which is what made this
  look unresolvable — while the TRC and colorant tags are the actual definition and are
  unambiguous. Parse the profile, never the description. (The user recalled the export as
  16-bit AdobeRGB; the files disagree, so that recollection is of a different export.)
  Consequences for the entry above, all of which used the gamma reading:
  **`--space linear-srgb` is correct.** NLP's median is above nc's on **all three** frames
  (+1.22 / +1.70 / −0.71 against nc's −0.71..−1.11), so there is **no G3 reversal** — that
  was a decode artefact, and "nc renders darker than NLP" holds on this roll after all.
  Contrast: NLP 8.14 / 3.83 / 7.35 against nc 3.55–4.51 — wider on G1 and G3, **tied on
  G2**. The G2 gap splits 43% shadow / 57% highlight, so it is not shadow-led. The one
  finding that never depended on the reading stands: nc's `p95 − p5` moves **0.96** stops
  across the three frames where NLP's moves **4.31**.
  Unchanged caveats on those three frames: one roll, and NLP cropped to a different
  aspect ratio with no registration.
- 2026-09-11: **The NLP reference set is two export regimes, and the larger one is far
  better evidence than the frames measured above.** Surveyed every file by parsing its
  embedded profile (43 TIFFs; `**/*.tif` — a `*/*.tif` glob misses `2026-09-09`, which
  nests a subdirectory):

  | files | depth | primaries | TRC | directories |
  |---|---|---|---|---|
  | 11 | 32-bit float | sRGB/709 | linear (gamma 1.0) | `2026-07-23`, `2026-07-24`, `2026-08-04` |
  | 32 | 16-bit | Adobe RGB (1998) | gamma 2.1992 | `2026-09-09/2026-09-09-Ektar` |

  So **a declared space is per directory, never per set** — measuring the whole reference
  folder with one `--space` would be wrong for one regime or the other. The 16-bit batch is
  also unambiguous, its `desc`, colorant primaries and TRC all agreeing, where the
  float batch's description contradicts itself.
  **And the 32-frame batch is pixel-aligned with its sources** — every sampled pair has
  identical dimensions (e.g. 4945x3350, 4936x3352), sources present under
  `rolls/2026-09-09-Ektar/` and named in the manifest. That is the condition this task's
  design reserved the opt-in pixel-wise section for, so it now genuinely engages: 32 frames
  of one stock, one calibration, no registration problem and no colour-space ambiguity.
  Prefer it over the three Gold200 frames for any number that has to hold up.
  Follow-up is `algo/contrast-latitude-spike`.
- 2026-09-13 (note, no work): a further batch `converted/nlp/2026-09-11/2026-09-11-Portra400`
  (32 files) is registered in the manifest but was not part of the survey above; its
  declared space must be established the same way before it is measured.
- 2026-09-14: the 2026-09-02 note above that the default gain-map JPEG is unreadable by
  the metric reader was superseded on 2026-09-03 — `metrics --jpeg-image sdr` reads the
  base, so a default roll is comparable as its SDR rendition (see `conversion-metrics`).

## display-output-acceptance (continued)

**Status:** not started
**Updated:** 2026-07-30

- 2026-07-30: Added a quantitative master/display tonal-delta gate. The
  reference-anchored reconstruction sigmoid owns the toe; normalized display
  outputs may differ for declared transfer/reference-white/highlight/gamut
  reasons but fail if they introduce a second shadow-floor lift or broad
  midtone re-grade. Numeric bounds must be established from the frozen real-scan
  baseline before default activation rather than replaced by a visual-only
  judgment.


## comparison-review-tooling

**Status:** done (2026-09-12)
**Updated:** 2026-09-13 (consolidated)

- Goal: promote the ad-hoc review pages built during `algo/reference-anchored-sigmoid` into a
  maintained tool for comparing rendering configurations by eye. Requested explicitly by the
  user rather than continuing to patch the scripts inline.
- **Lessons paid for before the tool existed, and still binding on it:** render through the
  path being measured (the previews once used the *legacy* path while the metrics measured
  `pipeline::sdr::render`); click, not hover; one shared lightbox; never publish these pages
  (rendered personal photographs — throwaway dir only, never `../nc-assets` or the repo);
  and **`sips` destroys a gain map when downscaling**, so HDR review needs full-size files.
- **Viewer shipped 2026-09-02 as `tools/review-app/`** (user decision to halve the scope to
  a viewer plus the data format). `review.json` (`tools/review-app/SCHEMA.md`) declares
  `configs` and `images`, `snake_case`, paths resolved against the review file so a set is
  one movable directory; config order sets button order and the number-key mapping; an
  unknown config id in `renditions` is a loud error, a *missing* rendition renders as a
  visible gap. **Toggling in place is structural**: every rendition of a frame occupies one
  CSS grid cell, so switching config cannot move the picture by a pixel — side-by-side
  hides exactly the highlight differences this exists to show. `fullsize` has pan controls
  and a mini-map. Own CI job (`pnpm check` / `test` / `build`), deliberately not joined to
  the Rust matrix.
- **Fullstack since 2026-09-10** (TanStack Start): the server reads the set off disk —
  `pnpm dev <path to review.json>` (a directory means the `review.json` in it) or
  `REVIEW_SET` — and a bare `pnpm dev` renders the committed synthetic example, which is why
  that example is in the repo. Dev-server-only by decision (no `pnpm start`). Images are
  served from an **allowlist by opaque id**, never a path from a URL, which is also what
  lets a set name files outside its own directory; the id carries the file's mtime, so a
  re-render is a different URL (responses cache `immutable`). **It watches the set**: a
  re-run of `nc` updates the page in place, keeping selected config (held by **id**, not
  index) and scroll. Live refresh after a *server restart* rests on a plain `fetch` poll of
  `/alive` (boot id), because `router.invalidate()` issues no request once the module
  graph is replaced — the `EventSource` stream is the fast path only; a restart reloads the
  page. Test live refresh with the **file** form of the path: the directory form never hit
  the cache and hid a stale-URL bug for a whole review round. Every silent trap found on the
  way (`shellComponent` is server-only, `server.handlers` is stripped from the client, a
  scroll handler writing a signal, `requestAnimationFrame` in a hidden tab) is in the app's
  `README.md`.
- **Styling is Panda CSS with `strictTokens` + `strictPropertyValues` (2026-09-10)**, so
  `panda.config.ts`'s theme is the app's design system; `presets` is
  `['@pandacss/preset-base']` alone (machinery, no token ladders — keeping `preset-panda`
  would put 422 meaningless entries behind every autocomplete). Values are **px, not rem**,
  on purpose: a pixel-inspection tool's chrome should not rescale with the reader's font
  size while the images do not. The `@layer` line in `src/index.css` naming all five layers
  is Panda's injection point — drop one and it emits nothing at exit 0. `pnpm-workspace.yaml`
  needs `allowBuilds: {esbuild: true}` under pnpm 11. Verified as a pure value substitution:
  106 emitted rules before and after, 104 identical, the two differences deliberate.
  **Surfaced, not fixed:** the app has two reading measures (78ch under the title, 80ch in
  the standalone panels), kept as separate tokens because unifying them moves the layout —
  someone should decide which.
- **Generator shipped 2026-09-12 as `nctool review generate <matrix.json>`**, which closes
  the task. The matrix is data: `scripts/preset-review/presets.matrix.json` replaces the
  Python list `generate.py` carried, and the script is gone. A config states its own
  `args`; per-roll values arrive through placeholders — `{dmin}` from
  `scripts/sigmoid-baseline/fixtures.json` (the declaration the metrics already read) and
  `{film_stock}` from the matrix's `rolls` block — validated when the matrix loads, not 35
  renders in. **The suffix comes from the matrix's preset, but the colour space each cell
  is measured in comes from the recipe `nc` reports it resolved**: `legacy` and `custom`
  accept `--output-profile`, so reading the space off the preset name would have measured
  a ProPhoto render as sRGB with every number looking reasonable. A matrix restating a flag
  the generator owns (`--output-preset`, `-o`, `--report`) is refused. Each rendition gets
  its `nctool metrics` record and its `width`/`height` written beside it; re-measuring is
  keyed to the file's **checksum**, declared space and JPEG decoder, not its mtime (a 74 MP
  measurement is minutes). Failure is per cell. It **refuses an output directory inside the
  repository**, and checks arguments before the environment. Colliding cell names are
  refused case-folded (the default macOS volume treats `A-c.jpg` and `a-c.jpg` as one
  file); ids are checked filename-safe. The matrix is read with `deny_unknown_fields`
  discipline: `"arg"` for `"args"` would render the default under five labels at exit 0.
  Verified end to end on P3, G2, E1 across five presets: 15 renditions + 15 records; the
  matrix's `metrics.inset` 0.18 clears the holder on all three (a holder in the region reads
  as a hard spike at the bottom of the histogram); a second run re-rendered byte-identically
  and re-measured none. Tests: `test_review.py` (55).
- **Deferred with reasons (in the task file):** HDR review — nothing downscales, so an
  HDR-capable browser already shows the HDR rendition; a deliberate HDR review (headroom
  readout, SDR/HDR toggle) is a new task. Build-vs-build — identifying two builds in a
  page is a provenance problem, not a flag; worth doing when a default actually moves.
  **Not covered and wanted:** a **reference-cell kind** (an existing TIFF/JPEG from another
  producer, brought to a common SDR sRGB JPEG) — every cell today is an `nc convert`, which
  is what stopped `film-base/dmax-per-channel-reduction`'s NLP-comparison review set from
  shipping (2026-09-13).

## metrics-chart-design

**Status:** done (accepted as v1, 2026-09-12)
**Updated:** 2026-09-13 (consolidated)

- Goal: settle the chart encodings, the rendering technology and the component split,
  independently of the review app. Split out of `metrics-visualization` 2026-09-10 at the
  user's request as the harder, app-independent half; the 2026-09-03 chart ranking stays
  recorded under `metrics-visualization` below.
- **Findings from a design canvas drawn against real records** (frame G2 through the five
  `--preset` bundles): a colour vertex must carry its band's population — the `highlight`
  point rested on under 1 px in 18.7 M on some presets and manufactured a crossover out of
  rounding (this is why `pixels`/`sparse` exist in the record); the presets are
  brightness-matched, so curves **fan** rather than shift (0.02 st apart at p50, 0.27 at the
  toe, 0.34 at the shoulder — that fan is contrast, and no scalar in the record locates it);
  and colour alone stops separating past **three** overlaid configs (the `dataviz` dark
  steps pass all-pairs CVD at 2 and 3 series, fail at 5), so compare mode must become small
  multiples beyond three.
- **Two modes, separated at the user's request:** compare (n variants) and inspect (one).
  A chart takes n variants only if it still has a free series dimension — per-channel
  histograms spend it on RGB, the hue polar on angle. Compare-mode charts draw every config
  and the toggle *emphasises* one; inspect-mode charts bind to the active config and swap
  in place like the picture. An n-variant chart at n=1 is its own design.
- **v1 locked 2026-09-11: three charts** — luminance histogram, per-channel histogram, and
  cast-over-tone as two axis-coloured curves. The **a\*/b\* path was rejected by the user as
  unreadable**; what replaced it was the user's own design — two lines over the bands, y in
  CIELAB with 0 as neutral, each line **coloured by its own value** (a\* green-to-red, b\*
  blue-to-yellow), colour there to teach the axis rather than carry data. Three facts that
  cost a round each: the ramp is **fixed, never scaled to the data** (a fixed
  `RAMP_REFERENCE` of 20 CIELAB units, widened only when a frame exceeds it, symmetric and
  clamped — the first implementation normalised by the plotted range, so `b* = +2` painted
  identically to `+20`); its ends sit at the **measured sRGB gamut limit per direction**
  (at L\* 65 green clips at 42.8 and blue at 54.3 — ±41 for a\*, ±52 for b\*); and a mark
  and the line beneath it share one mapping. **Once colour carries hue it cannot also carry
  config identity**, so this chart is inspect-mode by construction.
- **Built 2026-09-11** in `tools/review-app/src/charts/`, geometry in pure `.ts` with
  tests, a `/charts` demo route from a committed synthetic record. Four defects sat inside
  components and passed a green type-check and 83 tests (a reference line whose condition
  could never be true, a required prop nothing read, a `bounds()` frozen at the first
  record, an x axis equating bin index with L\*) — the reason arithmetic stays out of
  `.tsx` in that app.
- **Accepted as v1 2026-09-12** ("enough for the current work"). **Still open for v2**, in
  the task file: whether chart 3's x axis moves to true L\* centres (which would let bands,
  histogram and cast share one axis); what a compare-mode cast chart looks like; degenerate
  cases beyond `sparse`; and how far `sparse` should demote the **curve** rather than only
  the mark (today the polyline runs through a sparse band at full weight, so a one-pixel
  outlier can still read as a crossover). Also deferred there: a `tone.bands` stacked bar
  (its rejection no longer holds after the L\* re-cut — a strong v2 candidate and the
  natural roll view) and a conditional `endpoints` strip.

## metrics-visualization

**Status:** done (2026-09-12)
**Updated:** 2026-09-13 (consolidated)

- Goal: plot the `nctool metrics` output inside `tools/review-app`, so numeric review
  sits beside visual review rather than in a separate tool.
- 2026-09-03: Filed after the user reviewed the metrics record field by field and asked
  for a visualization next, naming `bands`, `cast_by_tone_band` and `hue_sectors` as
  candidates. The task file records a different ranking and the reason for it: the
  **percentile curve** comes first because two overlaid decompose a difference into
  exposure (vertical gap), contrast (relative slope) and curve shape (where they
  diverge), which is the question the app exists to answer — `bands` is a coarser view
  of the same data and cannot say *where* the change is. `cast_by_tone_band` is second
  but must be drawn as a **path on the a\*/b\* plane**, not as bars: the shape of the
  path is the crossover. `hue_sectors` ranks last because six sectors is coarse and two
  polar charts compare poorly. `endpoints` was added to the list although the user did
  not name it — a per-channel bar is what made a 22% top-code population visibly
  *blue-only* on a real frame. (Both leading picks were later overturned against real
  records — see `metrics-chart-design`: the histogram displaced the percentile curve and
  the a\*/b\* path was rejected as unreadable.)
- The constraint that ranks them: **every chart must overlay two configs**, because the
  app's premise is toggling configs in place.
- **2026-09-12 — wired.** A rendition may name a record via an optional `metrics` key
  relative to `review.json` (a **sibling file**, not inlined: ~20 kB of histogram counts,
  written by a different tool at a different time, and a separate file is what lets a
  re-measurement update the page without rewriting the review document). It is read and
  parsed **server-side**, so the wire carries the charted subset (27 kB for five records,
  5.4 kB each rather than 20); `review.json`'s `schema_version` stays **1** (additive,
  optional). **It joins the watch targets** — the model holds the *parsed* record, so a
  record nothing stamps would sit invisibly behind the previous numbers; records are
  deliberately not in the asset map. All three v1 charts spend colour on what they encode,
  so they **swap with the config** below the picture (the stage is the widest thing on the
  page). A rendition with no record renders its picture and says so; an unreadable record
  costs only its own charts. The panel states **what was measured** ("the central 41% of
  the frame, 7.6 Mpx") and **which rendition** — a gain-map JPEG is one file carrying two,
  the browser shows the HDR one on an HDR display, and `nctool metrics` reads the SDR base.
  The `/charts` demo renders the same `MetricsPanel` the app mounts. App suite 128 tests.
- **Known and unowned:** nothing checks that a record describes the rendition it is
  attached to — the generator keys reuse to the image's checksum, but a hand-assembled
  set could pair them wrongly and nothing would notice.

## harness-regression-tests

**Status:** done (2026-08-11)
**Updated:** 2026-09-13 (consolidated)

- Filed 2026-08-09 out of the `output/presets` review round: the default flip to
  `gain-map-hdr` broke `scripts/real-scan-verify/harness.sh` in three places with all
  four CI gates green — `stage_freeze`'s `jq` still wrote the removed `output.hdr`
  key; the `convert` stages passed `.tiff` paths and hit exit 2; and `stage_convert`
  failed **without an error at all** (`nc roll` had become container-aware, wrote
  `_positive.jpg`, the `*_positive.tiff` rename glob matched nothing, and the stage
  printed its usual success line). The silent one is the reason the task exists.
- **Shipped 2026-08-11.** `harness.sh` uses fail-fast shell semantics, accepts an
  isolated `REC`, renders u16/f32 into a fresh per-run staging tree, requires exactly
  one TIFF+sidecar pair per frame per mode (TIFF magic; sidecars with object-valued
  `meta` and `params`), rejects directory-shaped publication targets, and publishes
  only after the complete set validates and revalidates it before the success line.
  Saved roll reports have `frames[].output` rewritten to the durable published paths.
  The intentional strict probe accepts only exit 1 carrying both the IR-ignored and
  strict-promotion diagnostics. `nctool.test_harness` drives the real debug binary
  through `freeze` → `convert` on the committed fixtures (no assets, no `exiftool`)
  and reproduces the successful-wrong-container failure with a fake `nc`.
- **The full `scripts/analysis` unittest suite runs in CI on Linux and macOS** since
  this task (it had run under no gate before). The Drive-backed image-quality,
  interoperability, IR, determinism and resource checks remain manual. A real
  Drive-backed `freeze` regenerated 21 files across seven rolls, semantically
  identical to the committed recipes after normalising JSON key order.


## calibration-frame-capture

**Status:** not started
**Updated:** 2026-09-12

- Goal: shoot, develop, scan and register the ColorChecker bracket rolls three other tasks
  name as a precondition, and take a first neutrality measurement against them.
- 2026-09-12 (filed): three tasks each named these frames in their own words and none owned
  producing them — `io/scanner-density-calibration` (the 3×3 + offset fit),
  `algo/sigmoid-parameter-calibration` (bracketed roll + grey card), and
  `algo/split-default-migration` (its release gate names a known-neutral reference, which is
  evidence rather than a task and so was invisible to the graph). The effect was that the
  graph reported work as executable when the thing blocking it was a roll of film that did
  not exist. The protocol agreed with the user 2026-09-08 moved here from
  `io/scanner-density-calibration`, which now points at it rather than restating it.
- Mostly photographic work. The code half is the manifest role for a bracketed target frame
  (exposure offset + lighting recorded alongside it) and whether one measurement command
  serves all three consumers or each wants its own read.

## review-reference-cells

**Status:** not started
**Updated:** 2026-09-13

- Goal: an outside producer's image (NLP export, SmartConvert, hand-tweaked target) as
  a grid cell beside nc's renders of the same frame, brought to a common SDR sRGB JPEG,
  paired by manifest `source_frame`. Asked for by three tasks; filed 2026-09-13.

## review-build-axis

**Status:** done (2026-09-22)
**Updated:** 2026-09-22

- Goal: the same frame and config across two builds as toggling cells, labelled from
  the sidecar's `identity` block. Deferred by `comparison-review-tooling` until a
  default moves; two default moves are now filed.

### 2026-09-22 — shipped

- **No nc change was needed, and the task file understated what already existed.**
  Every `convert` writes `<output>.json` = `{meta, params}`, and `meta` carries the
  identity block **flattened** (not nested under `meta.identity`). But the *report* on
  stdout carries the same value — `SidecarMeta.identity` is the very `&Identity` the
  report serializes — and `_render` already parsed that report and threw it away. So
  provenance cost one line, not a file read. Probed on a committed fixture: report
  `identity` and sidecar `meta` are byte-identical. The sidecar stays as a fallback
  in `cell_identity`, but **no binary is known to write one without the other** — it
  is kept because it costs a line, not because a version needing it was established.
  (An earlier draft of this entry and of that docstring both asserted the history;
  the docstring was corrected in review and this bullet with it.)
- **Answers to the task's three open questions.** (1) *Neither* a third matrix
  dimension nor paired matrices: `builds` is declared top-level and the generator
  expands builds x configs into the flat `configs` the app already renders. The app's
  premise is one grid cell per (frame, config) so toggling cannot move the picture by a
  pixel; a real second axis means a second toggle. A config may state its own `builds`
  subset for the asymmetric pair. (2) **Pre-built binaries**, plus `--build <id>=<path>`
  to repoint one arm without editing a committed matrix — no cargo, no scratch target
  dir, no dirty-tree question. (3) **Both**: the matrix gives a short name, which the
  generator composes into `label` as `<config> · <build>`; the *identity* is derived and
  rides in a new optional `configs[].producer` block.
- **The join is `@`, and that is load-bearing.** `SAFE_ID` admits hyphens inside an
  author's id, so a hyphenated join would reproduce the `<frame>-<config>` ambiguity
  `colliding_stems` exists to refuse. `@` is outside `SAFE_ID` altogether, so an author
  id can never contain one and a composed id splits exactly one way — the join is
  injective, which *removes* a collision class. `charts/domId.ts` already hex-escapes
  everything outside `[A-Za-z0-9]`, so SVG gradient ids were safe with no change.
- **`producer` is tagged by `kind`, for `analysis/review-reference-cells`.** `hanten`
  carries the identity fields (all optional — a tarball build stamps no commit);
  `external` carries `label` + `note` for an outside producer's cell. Both answer the
  same question — which cell did nc-as-configured not render — so doing them as two
  blocks would have been the mistake. Additive and optional, so `review.json` stays
  `schema_version` 1 (the `metrics` key set that precedent), and a matrix with no
  `builds` produces the document it always did, byte for byte.
- **The honesty checks, and why two of them are notes rather than errors.**
  *Error, aborts the run:* a build that reports one identity and then another — the
  binary changed underneath, so every cell already rendered under that name is suspect.
  Since the matrix claims no identity, "a cell that disagrees with its claimed build"
  can only mean disagreeing with the rest of its own build. **The abort also deletes an
  earlier run's `review.json` from the output directory when this run overwrote a cell
  that file names** — writing none is not enough, because those cells would still be
  attributed to the previous build by the set sitting there, and the app *watches* the
  set, so a page already open refreshes straight onto the new pixels under the old
  label. The deletion is conditional on the *destinations this run actually wrote*,
  which the loop now tracks: a run over different frames or different configs
  overwrote nothing the stale set indexes, told it no lie, and gets a message saying
  the set was left alone. Deleting there would destroy a set that is still true, and
  the first version's fixed message claimed an overwrite that had not happened.
  **The compare is the hard part, and its errors are one-sided.** A miss lands on
  the branch that *spares* the file, which then asserts the set still describes its
  own pixels — so the predicate must recognise every spelling of a destination the
  schema permits, not only the ones this generator writes. Three got through the
  first conditional version: `review.json`'s **string** rendition shorthand (the
  form `SCHEMA.md` calls the common case) was invisible; `src` is a path *relative
  to the set*, not a filename, so `renders/../F1-a.jpg` never matched; and the
  compare was case-sensitive although `F1-Dflt.jpg` and `F1-dflt.jpg` are one file
  on the default macOS volume — reachable from two ordinary runs in the very
  rebuild-into-the-same-directory loop `--build` invites, with no hand-authored set
  involved. `colliding_stems` had casefolded for exactly this reason since before
  the axis existed and cannot help here: it only ever sees one run's matrix, so it
  refuses `Dflt`/`dflt` *within* a matrix and is blind across runs. Now
  `indexed_sources` reads both shapes, resolves each `src` against the output
  directory and compares case-folded paths (`dest_key`) — and returns a second
  value saying whether it read the whole document. That second value is what
  separates **three** outcomes from two: deleted, spared-and-vouched-for, and
  spared-but-unchecked. "I recognised nothing" is not "there was nothing", so an
  unparseable set, a shape the schema does not have or a path that will not resolve
  leaves the file (the app refuses a set it cannot parse, loudly) and says the
  question is open instead of answering it. The two messages deliberately share no
  phrase, so a test asserting one is absent can tell them apart. **A fourth axis is
  not about spelling at all: a render that *fails* can still have written its
  output.** `--strict` gates after encoding, so nc writes the image and its sidecar
  and *then* exits 1 — tested against the release binary on `hdri-64bit.tif`, where
  the IR warning makes it fail, and `--strict` is not an owned flag so an ordinary
  config may pass it. **Record the assumption as disproved**: "a failed convert
  leaves nothing on disk to account for" is plausible, was asserted in review, and
  is false; do not re-derive it. Destinations are therefore booked *before* the
  render, not after it succeeds, so a render that failed before writing anything
  over-matches — the same trade `dest_key`'s casefold makes, and the reason is the
  same one in both places: over-matching costs a stale index that was arguably
  still true, a miss leaves a lie in place and vouches for it. All of it caught in
  review, none of it by the first version of the test. *Note:* two builds that are the **same file** — refusing that would refuse the
  task's own acceptance probe, which is that the same binary twice yields byte-identical
  cells (verified: `cmp` clean). *Note:* two **different** files reporting the same
  identity — the realistic shape of a patched-versus-shipped spike, and it cannot be
  called an error because the binaries really do differ; what it can be is said out loud.
  The binary's sha256 is the only fact here that separates those last two, which is why
  the pre-flight computes it.
- **The pre-flight accepts the pre-rename `nc` banner**, via `manifest.is_nc`. Not
  tidiness: the reference arm of a before/after is a git-tagged binary
  (`nf-verification/reference-snapshot`), and a tag old enough to be worth comparing
  prints `nc`. A check that demanded `hanten` would have rejected exactly the binary the
  axis exists for. `manifest.sha256` was reused the same way — both stdlib.
- **Two behaviour changes to the existing path**, both because a matrix with no `builds`
  is modelled as one *unnamed* build rather than as a second code path. (1) The
  pre-flight now runs `is_nc` on the `--nc` binary too, not just on a declared build's.
  A `--nc` pointing at something that is not this CLI used to fail at the first render;
  it now fails before anything is written — same verdict, twenty minutes earlier.
  (2) **Identity drift aborts a no-axis run too**: if the `--nc` binary changes commit
  between two cells, the run exits 1, writes no `review.json` and removes an earlier
  one that names a cell it overwrote. That is the path most runs take, so it is not a
  build-axis footnote; the abort message names the binary by path, since there is no
  build id to print. Covered end to end (`test_a_run_with_no_build_axis_aborts_on_drift_too`)
  and stated in the skill's step 4 as a rule that is not build-axis-only.
- **Known limit:** the cells are a product and only ten have a number key, so two builds
  over five configs already spends the whole keyboard. A build comparison wants a short
  config list. That follows from the one-toggle decision and is the price of it.
- **Also deliberate:** a matrix with **no** `builds` gets no `producer` at all, even
  though its single binary is known. Keeping the no-axis document byte-identical was
  worth more than recording provenance nobody asked for; the per-cell `<image>.json`
  sidecars carry the commit in that case anyway.
- Tests: `test_review.py` 55 → 143, including eighteen end-to-end cases driven through
  `cmd_generate` against a **fake `hanten`** (no assets, no venv, no real binary) that
  changes the commit it reports between calls. Those thirteen carry the ordering: a rule
  called directly never proves it runs before the coarser gate that used to hide it.
  Falsifiability checked: neutering the drift check reds two of them, and reverting
  each of the three compare axes in turn reds a distinct unit-plus-end-to-end pair,
  and booking a destination only on success reds the fail-after-write case (whose
  fake `hanten` writes its output and *then* exits nonzero, the shape no other
  failure case in the suite has).
  `indexed_sources` also has direct tests, which it needed most: it is the one
  function here that reads a document this toolkit did not write, so a test built
  only from `build_review`'s output exercises half the schema. App suite 265,
  with `producer.ts` holding the formatting
  (nothing above the pure `.ts` layer in that app is testable). Verified in a real
  browser — the button tooltip and the line under the picture both read the identity,
  and the line swaps with the config.

## probe-fixture-roll-names

**Status:** done (2026-09-24)
**Updated:** 2026-09-24

- 2026-09-24: filed while restructuring CLAUDE.md. The `#[ignore]`d asset probes'
  `FIXTURES` look rolls up by names `manifest.json` no longer uses, so they panic
  before measuring; the task file lists the stale keys.
- 2026-09-24: done. Mapped every stale key by re-measuring each dated roll's `base.tif`
  and `leader.tif` over the frozen recipe's regions: `Ektar` → `2026-07-15-Ektar100`,
  `Portra160-2026-07-22` → `2026-07-23-Portra160` and `WHOLE_ROLLS`' `2026-09-09-Ektar` →
  `2026-09-09-Ektar100` reproduce `Dmin` and `Dmax` exactly, so the recipe stems stay.
  `Portra160`, `Portra400` and `Portra400-leica-flaw` are gone from `../nc-assets`
  (neither manifest nor disk) and were dropped; `curve_probe`'s corpus is now three rolls
  and 10 frames. A roll missing from the manifest skips with a `SKIP roll …` line instead
  of panicking. `cargo test --release -- --ignored`: 13 passed.
  Found on the re-run: `whole_roll_scale` printed the pre-2026-09-16 default
  (`0.90 / 0.86`) as "shipped default" — now `0.84 / 0.73` — and `WHOLE_ROLLS` was
  described as ~32 frames a roll; the pruned rolls hold 12 and 11.
  Not done: `nctool/manifest.py`'s `SEED_ROLES` has no seeds under the dated roll names,
  so a from-scratch manifest generation would lose those rolls' roles.
- 2026-09-24: review follow-up. A skipped roll now prints `SKIP roll …` on **stdout**,
  before any header, and `sigmoid_scale` counts only rolls that contributed frames. A
  roll present but malformed still panics. Rows labelled "shipped" now read
  `algo::fixed::DENSITY_SCALE`, and `[1, 1, 1]` is labelled "identity". Figures quoted
  from the six-roll corpus carry a note. The `SEED_ROLES` gap is filed as
  `analysis/manifest-seed-roles`.

## manifest-seed-roles

**Status:** not started
**Updated:** 2026-09-24

- 2026-09-24: filed from `probe-fixture-roll-names`. No `SEED_ROLES` entry matches a
  date-named roll, so a from-scratch generation would mark reference frames `real`.

## review-test-local-binary

**Status:** done (2026-09-25)
**Updated:** 2026-09-25

- 2026-09-25: filed while shipping `nf-calibration/anchor-comparison`. Goal: the `nctool`
  default-binary test stops depending on whether a release binary is built.
- 2026-09-25: done. The test now runs from a temporary working directory holding a fake
  `hanten` at `DEFAULT_NC`, and asserts `resolve_binaries` resolves to it — a positive
  check that the fallback is *used*, where the old one only matched the path in a
  missing-binary error. `review.py` is unchanged: `DEFAULT_NC` stays relative to the
  working directory, which is the behaviour under test. The missing-binary refusal the
  old test covered incidentally has its own test now, through an `--nc` path that does
  not exist. Removed the "move the binary aside" advice from `scripts/analysis/CLAUDE.md`
  and `nc-fixer.md`.
