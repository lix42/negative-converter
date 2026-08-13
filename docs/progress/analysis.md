# Negative Converter — analysis Progress Log

Execution log for the `analysis` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `analysis`:

- **This epic verifies the pipeline; it is not part of it.** Everything lives in
  `scripts/`, and the hard invariant is that **only derived numbers and
  downscaled thumbnails leave the tools** — never sample pixels into context.
  Metadata comes from `nc inspect`; bytes are streamed only to hash.
- **Real-scan core verification is done (2026-07-22/23)** across five rolls; the
  write-up is [`docs/reports/real-scan-verification.md`](../reports/real-scan-verification.md)
  and the rerunnable harness plus frozen recipes are under
  `scripts/real-scan-verify/`. (The task section below still mirrors an old
  `not started` status — `TASKS.md` is authoritative and marks it `[x]`.)
- **Note:** the execution record for `real-scan-verification` is in
  [`_unassigned.md`](_unassigned.md) — in the flat log it was nested under the
  `color-management planning` heading, so the epic split carried it there
  verbatim. Read it there.
- **The numbers other epics are waiting on:** all assets are HDRi with IR;
  standard frame 5184×3599 ≈ 18.66 MP; measured **peak ~930 MiB @ 18.66 MP
  (~50 MiB/MP)**, ~1.6 s wall — about 1.5× the design's model, which omits the
  carried IR plane and the `to_output` clone. That is the STEP 0 input for
  `io/streaming-tiled-io`: `io/memory-preflight` is required, streaming is a
  conditional GO pending a post-preflight re-measure.
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
  (stdlib only; needs `scripts/analysis` on `PYTHONPATH`).
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
- **NLP comparison is global-metrics + side-by-side thumbnails, no registration**
  — NLP outputs are cropped and differently sized, aligned only by manifest
  `source_frame` identity.
- **Open question for the user:** the committed `recipes/*.hdr.json` key order
  lags the current harness `jq` (values identical); a `freeze` re-run will
  reorder them.
- **The analysis stdlib suite is CI-gated on Linux and macOS (2026-08-11).** It
  includes a hermetic real-binary `freeze` → `convert` harness test plus a fake
  successful-wrong-container regression. The Drive-backed verification matrix
  remains manual; CI protects its CLI/recipe/container plumbing.


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
- 2026-07-27: Epic migration — the actual execution record for this task is in
  [`_unassigned.md`](_unassigned.md) (`### Real-scan core verification — executed
  2026-07-22`); it was nested under another heading in the flat log, so the split
  carried it there verbatim rather than into this section.


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
**Status:** done (spike)
**Updated:** 2026-07-23

- Goal: decide scope/structure to grow `real-scan-verify` into a reusable
  conversion-analysis toolkit (asset manifest, image-library analysis, NLP-vs-nc
  comparison). Spike deliverable = design note + concretely-scoped child tasks.
- **Research.** Confirmed the current state: `harness.sh` drives `nc` with a
  hard-coded `ROLLS` array; **all quantitative numbers today come from nc's own
  JSON reports** (clip %, Dmin/Dmax) — the numpy/tifffile + ImageMagick analysis
  the task mentions was ad-hoc and **not committed anywhere**, so the
  image-library layer is net-new. Assets (`../nc-assets`, ~11 GB rolls + 6.2 GB
  converted) match the user's three categories: experiments (`48/64bit-*`,
  `samples/`), rolls (5, each unexposed+leader+real), converted (`V0` = the
  v0-baseline set; `2026-07-22` = harness output). **numpy/tifffile/Pillow are not
  installed** (system Python 3.14) → toolkit needs its own venv.
- **Decisions (with the user, via interview):**
  - Tooling → **Python package** `scripts/analysis/nctool/`; single entry point;
    subsumes `real-scan-verify` (`harness.sh` retires/shims).
  - Asset root → **configurable, local for now**; relative paths + portable
    checksums so a later Drive switch is one line. Drive = deferred task.
  - Manifest → **JSON, rolls + converted, experiments excluded**; roles
    (`unexposed|leader|real`) human-seeded, derived facts generated from
    `nc inspect`; replaces `ROLLS`. `manifest validate` (orphans/missing/drift) is
    the cleanup surfacing mechanism (reports, never deletes).
  - NLP → **global metrics + side-by-side thumbnails, no registration**; align by
    manifest `source_frame` identity.
- **Invariant preserved:** only derived numbers (JSON) + downscaled thumbnails
  leave the tools; full-res pixels read one-at-a-time, never surfaced.
- **Split into 4 child tasks:** `asset-manifest` → `conversion-metrics` →
  `nlp-comparison`; `asset-manifest` → `drive-asset-migration` (deferred). Graph
  wired in TASKS.md.
- **Cleanup done:** removed 4 stray `.DS_Store` from `../nc-assets`.
  `converted/V0/` kept (v0-baseline artifacts for `conversion-versioning`).
- **Next:** `asset-manifest` is the unblocked starting task.

---

**Update 2026-07-24 — assets moved to Google Drive + reorg + first manifest.**

- User relocated nc-assets from local `../nc-assets` to
  `…/GoogleDrive-devlix42@gmail.com/My Drive/temp/nc-assets` (12 GB) for
  multi-machine work, and added: `samples/largest.tif` (10368×7200 = **74.6 MP**,
  HDRi w/ IR — the ~4× perf worst-case the memory report lacked) and an NLP set
  (`NLP converted/`) for a new roll `Portra160-7-22` (renamed `Portra160-2026-07-22`
  in the reorg below).
- **Reorg (full category regroup):** `rolls/{Ektar,phoenix,Portra160,
  Portra160-2026-07-22,Portra400,Portra400-leica-flaw}`, `samples/`
  (`largest.tif` + `icc/`), `converted/{nc/{2026-07-22,V0},nlp/2026-07-23}`.
  Dropped the 48/64bit experiment fixtures (repo tests use committed
  `tests/fixtures/`, not these); kept `converted/nc/V0` (v0-baseline). Cleaned
  `.DS_Store`.
- **Manifest rethink:** lives **at the assets root** (`manifest.json`) with paths
  **relative to its own dir** → no `asset_root` field, machine-portable; scope
  broadened to a **full inventory** (rolls+roles, samples, converted nc+nlp).
  sha256 for irreplaceable data (rolls/samples/NLP/V0); omitted for regenerable
  nc/2026-07-22 outputs. `source_frame` links + `coverage_gaps`.
- **Generated** `manifest.json` (throwaway `nc inspect`-driven Python script;
  formalized later by `asset-manifest`'s `manifest generate`). `nc inspect`
  corrected exiftool: all frames incl. largest.tif are **hdri w/ IR**. NLP outputs
  are **4406×2930 32-bit float, cropped** (validates no-registration); frames
  1096/1097 have no NLP output (coverage gap).
- Task docs synced (`asset-manifest`, `drive-asset-migration` [now in-progress,
  not deferred], spike outcome, `nlp-comparison`). **Open:** repo `../nc-assets`
  convention (recommend machine-local symlink → Drive) — CLAUDE.md/harness still
  say `../nc-assets`.

**Update 2026-07-24 (cont.) — symlink bridge + committed manifest tooling + skill.**

- Created machine-local symlink `~/src/nc/nc-assets → <Drive>/temp/nc-assets` so
  the repo's `../nc-assets` convention (CLAUDE.md, harness `A=../nc-assets`,
  reports) keeps working unchanged across worktrees; not committed (machine-local).
- `scripts/analysis/generate_manifest.py` — reusable, update-aware, stdlib
  generator (locates `nc`/exiftool; preserves human fields role/stock/kind/note +
  bucket regenerable/nc_version/recipe_dir; recomputes sha256 every run by
  default for source-of-truth integrity, opt-in size-based reuse via
  `--reuse-hash`). **Idempotent** (byte-identical re-run). Reproduces the live
  manifest and adds `megapixels` on converted outputs.
- `scripts/analysis/manifest.sample.json` — committed trimmed schema reference
  (every shape; real values; ~7 KB). Update only on **schema** change.
- `asset-manifest` **skill** (`.agents/skills/asset-manifest/` + `.claude/skills/`
  symlink) — when/how to regenerate, invariants (no pixels; roles preserved),
  layout the scanner expects.
- The live `manifest.json` stays in the Drive folder (not committed). `asset-manifest`
  task doc updated to point at these precursors; remaining task work = fold into
  `nctool` + add `validate` (orphans/missing/drift).


## asset-manifest
**Status:** implemented (uncommitted, in worktree `feat/asset-manifest`)
**Updated:** 2026-07-24

Formalized the precursor `generate_manifest.py` into the `nctool` package and
added the missing `validate` mode + a manifest-driven harness. Stdlib only; the
"never read sample pixels" invariant is preserved (metadata via `nc inspect`,
bytes only streamed to hash).

- **Package seed** `scripts/analysis/nctool/` (minimal — full skeleton lands in
  `conversion-metrics`):
  - `manifest.py` — one implementation of `generate` / `validate` / `roles` plus
    shared directory walkers (`walk_rolls`/`walk_samples`/`walk_converted`, and a
    flat `disk_files` for validate) so generate's structured build and validate's
    on-disk set can never diverge. The generate logic is a faithful port of
    `generate_manifest.py` (same seeds, rename/checksum-identity preservation,
    source_frame retarget, encoding inference, atomic write).
  - `__main__.py` — `python -m nctool manifest {generate,validate,roles}` argparse
    dispatcher (`--asset-root`, defaults `$NC_ASSET_ROOT` → `../nc-assets`).
  - `__init__.py` — dependency-free seed docstring.
- **`generate_manifest.py`** retired to a thin backward-compat shim forwarding its
  historical CLI to `nctool.manifest.cmd_generate` (the skill/docs reference it by
  path; no `PYTHONPATH` needed since it inserts its own dir).
- **`validate`** (new) — reports **drift** (recorded sha256 ≠ file, with a
  byte-size pre-check before hashing hundreds of MB), **missing** (in manifest, off
  disk), **orphans** (on disk, untracked); lists regenerable no-sha outputs as
  `unchecked`. REPORTS only, never deletes. Exit 0 clean / 1 discrepancies / 2
  operational (no/invalid/unsupported-schema manifest). Does **not** need `nc`.
- **Harness** `scripts/real-scan-verify/harness.sh` — replaced the hard-coded
  `ROLLS` array with a `while read` loop (bash 3.2-safe) filling it from
  `nctool manifest roles`. Only rolls with exactly one unexposed + one leader are
  emitted (NLP-source roll skipped), reproducing the original five-roll set. Fails
  loudly (exit 2 + remediation) if the manifest is absent.

**Verified (all on this build, macOS/aarch64):**
- `generate` on the live assets reproduces the existing/live `manifest.json`
  **byte-identical** (full sha256 recompute of ~12 GB, ~10 s): 6 rolls, 5 samples,
  buckets `nc/2026-07-22` (34, regenerable), `nc/V0` (8), `nlp/2026-07-23` (4),
  same coverage_gaps. Shim path reproduces it too.
- `validate` on the clean tree → 0 orphans/missing/drift (exit 0). A synthetic
  tree with a deleted / added / edited file surfaces missing + orphan + drift
  (exit 1).
- `roles` emits the five calibration triples with unexposed/leader **matching the
  hard-coded ROLLS exactly**; `Portra160-2026-07-22` (all real) correctly skipped.
- **Harness parity:** ran `freeze` with the old hard-coded array vs the new
  manifest-driven harness on the same binary → **byte-identical `recipes/`**. The
  new `.json` / `.provenance.json` also match the committed set; the committed
  `.hdr.json` differ **only in JSON key order** (`output` vs `reconstruction`
  position, identical values) — a pre-existing artifact of an older harness jq
  revision, unrelated to this change and reproduced by the old array too. Repo
  `recipes/` left untouched (restored to committed state after the A/B).

**Notes for `conversion-metrics` / `nlp-comparison`:**
- The package is intentionally minimal: `__main__.py` dispatches only the
  `manifest` group. When adding `metrics` / `thumbs`, either extend `__main__.py`
  or introduce the documented `cli.py` and have `__main__` delegate — the shared
  walkers and `iter_meta`/`load_manifest`/`Prev` in `manifest.py` are reusable.
- `python -m nctool` needs `scripts/analysis` on `PYTHONPATH` (the harness and docs
  set it inline); the shim avoids that only because it inserts its own dir.
- Open question for the user: the committed `recipes/*.hdr.json` key order lags the
  current `harness.sh` jq (values identical). Harmless, but a `freeze` re-run will
  reorder those keys — decide whether to refresh the committed recipes.

**Update 2026-07-24 (review-fix round) — hardened `validate` + tests.**
Addressed the `asset-manifest` review findings (all uncommitted, in worktree):
- **Full-tree orphan scan** (`all_disk_images`): `validate`'s orphan check now
  walks the entire asset tree recursively for `.tif`/`.tiff`, so a root-level stray
  or a deeply-nested scan (`samples/icc/sub/x.tif`, `rolls/<roll>/sub/x.tif`) is
  flagged instead of being invisible to the structured generate walkers (left
  unchanged). Non-image companions (`.json`/`.jpg`) and `manifest.json` are excluded
  by the extension filter.
- **Fail on missing checksum for irreplaceable/error entries**: entries carrying an
  `error`/`metadata_source:"none"` (new `ERRORS`) and non-regenerable entries lacking
  `sha256` (new `NO CHECKSUM`) are now PROBLEMS (exit 1). Only entries in an explicitly
  `regenerable: true` bucket may legitimately be `unchecked`.
- **`inspect()` loud on nc-parse-failure**: `nc inspect` exit 0 with unparseable/
  missing-key JSON is now a per-file `error` (`metadata_source:"none"`), not a silent
  downgrade to exiftool placeholders. Non-zero exit (rejection) still falls back.
  Happy path unchanged (byte-identical reproduction preserved).
- **Harness roles exit-code**: `harness.sh` now captures `nctool manifest roles` to a
  temp file and checks `$?` (process substitution discarded it) — a mid-stream `roles`
  crash fails loud (exit 2 + remediation) instead of proceeding on a truncated ROLLS.
- **`cmd_roles` unknown-role guard**: a typo'd role warns loudly and is folded into
  `real` (was silently bucketed into a phantom key via `setdefault`, dropping a frame).
- **nc-absent is loud**: a wholesale nc-absent `generate` now exits 2 with remediation
  by default; the degraded exiftool-only mode is gated behind `--allow-exiftool-fallback`.
- **`write_manifest` durability**: unique `mkstemp` temp + `fsync` before `os.replace`
  (concurrent-run safe; no zero-length file after a crash).
- **New committed test suite** `scripts/analysis/nctool/test_manifest.py` (stdlib
  `unittest`, hermetic synthetic tree, no real assets, `nc` stubbed): 30 tests over
  roles parity, build preservation/rename/encoding/coverage, load_manifest schema,
  `inspect` parse-vs-rejection, and `validate` classification + 0/1/2 exit contract.
  Run: `PYTHONPATH=scripts/analysis python3 -m unittest nctool.test_manifest`.
- **Verified:** `generate` on live assets still reproduces the manifest
  **byte-identical** (sha `7351955…`); `validate` clean → exit 0; 30/30 unittests
  pass; Rust CI green (fmt / clippy -D warnings / build / 411 tests).


## conversion-metrics

**Status:** not started
**Updated:** 2026-08-12

- Goal: Formalize the ad-hoc image-library analysis from real-scan verification into the reusable Python toolkit that is the toolkit's single documented entry point.
- 2026-08-12: Folded the briefly separate `photographic-result-analysis` follow-up into this
  task rather than creating a false dependency between overlapping work. The trigger was the
  Portra 400 Dmax 1.2-versus-1.9 comparison: provenance, channel means, and clipping counters
  establish that the runs differ, but do not explain color and tone distribution,
  shadow/highlight occupancy, range use, or proximity to the endpoints. Metric definitions and
  the final artifact design remain opening questions for implementation.


## drive-asset-migration

**Status:** not started
**Updated:** —

- Goal: Make working from the Google Drive-hosted asset folder robust across machines, now that the assets — inputs *and* conversion outputs — physically live there (moved and reorganized 2026-07-24, with a self-relative `manifest.json` at the root). The move and reorg are done; this task covers the remaining robustness/tooling and the repo path-convention decision.


## nlp-comparison

**Status:** not started
**Updated:** —

- Goal: Ingest Negative Lab Pro (NLP) conversion outputs (the user adds them to `nc-assets`) and compare them against nc's outputs: global per-image metrics side by side, plus side-by-side downscaled thumbnails.


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

**Status:** not started
**Updated:** 2026-08-03

- Goal: promote the ad-hoc review pages built during `algo/reference-anchored-sigmoid` into a
  maintained tool for comparing rendering configurations by eye. Requested explicitly by the
  user rather than continuing to patch the scripts inline.
- Lessons already paid for and worth preserving: **render through the path being measured** (the
  previews originally used the *legacy* path while the metrics measured `pipeline::sdr::render` —
  reviewing one renderer while measuring another); **click, not hover** (a hover popover covers
  the thumbnail you are trying to leave, and inter-thumbnail gaps make it flicker); **one shared
  lightbox**, which is what makes prev/next possible; **single-quote CSS `url()`** inside a
  double-quoted `style` attribute or the attribute terminates and the tile renders black;
  **never publish these pages** (rendered personal photographs — throwaway dir only, never
  `../nc-assets` or the repo); and **`sips` destroys a gain map when downscaling**, so HDR review
  needs full-size files.
- Wanted: one entry point instead of two overlapping script pairs, the configuration matrix as
  data rather than code, HDR review for frames whose range exceeds SDR, and build-vs-build
  comparison so a future default change can be reviewed the same way.

## harness-regression-tests

**Status:** done
**Updated:** 2026-08-11

- Goal: give `scripts/real-scan-verify/harness.sh` automated coverage, so a change
  to nc's CLI surface cannot break it silently. See
  [the task file](../tasks/analysis/harness-regression-tests.md).
- Filed 2026-08-09 out of the `output/presets` review round. The default flip to
  `gain-map-hdr` broke the harness in three places with all four CI gates green:
  `stage_freeze`'s `jq` generator still wrote the removed `output.hdr` key; the four
  `convert` stages passed `.tiff` paths and hit exit 2; and `stage_convert` failed
  **without an error at all** — `nc roll` had become container-aware, so it succeeded
  and wrote `_positive.jpg`, the `for g in "$htmp"/*_positive.tiff` rename glob
  matched nothing, the float-HDR outputs stayed stranded in `.hdrtmp`, and the stage
  printed its usual `converted <roll>: N frames x2 modes`.
- The silent one is the reason the task exists. An exit 2 is found the next time
  someone runs the harness; a success line over the wrong container in the wrong
  directory is not.
- Second, narrower lesson recorded in the task: the checked-in recipes were migrated
  by hand while the `jq` generator that *writes* them was not, so re-running
  `stage_freeze` would have silently restored the broken state. Coverage that ties
  the generator to the committed recipes would catch that class directly.
- Deliberately left open: whether a fixture-only harness run is possible at all,
  whether it belongs in CI (no assets, no `exiftool` there), and what language it
  should be in — `scripts/analysis/`'s 91 Python tests already run under no CI gate,
  which is worth resolving together rather than adding a third untested surface.
- 2026-08-11: Started implementation after inspecting the harness and the existing
  stdlib `nctool` tests. The committed TIFF fixtures are sufficient for a hermetic
  `freeze` → `convert` run against the real debug binary: region-based Dmin/Dmax
  estimation succeeds (a low-Dmax warning is harmless), and neither Drive assets
  nor `exiftool` is needed. Plan: make recipe/output staging test-overridable, add
  exact artifact postconditions, reproduce the successful-wrong-container failure
  with a fake `nc`, and put the full analysis unittest suite into CI.
- 2026-08-11: Completed. `harness.sh` now uses fail-fast shell semantics, accepts
  an isolated `REC`, renders u16/f32 into a fresh per-run staging tree, requires
  exactly one TIFF+sidecar pair per frame per mode, and publishes only after the
  complete set validates. Wrong suffixes, extra files, ordinary command failures,
  and determinism differences are hard failures; the expected strict failure is
  asserted explicitly. `nctool.test_harness` covers the real fixture-backed
  `freeze` → `convert` path and a fake u16-success/f32-JPEG-success regression that
  must fail before publication. CI now runs all 94 stdlib analysis tests on Linux
  and macOS. Verified: targeted harness tests, full Python suite, fmt, clippy with
  warnings denied, build, and the Rust suite (793 passed, 5 ignored). A real Drive-backed `freeze`
  regenerated 21 files across seven rolls, all semantically identical to the
  committed recipes/provenance after normalizing JSON key order.
- 2026-08-11: Review hardening. Staged and published artifacts are now checked by
  content (TIFF magic and JSON-object sidecars), directory and directory-symlink
  publication targets are rejected before any move, and final artifacts are
  revalidated before the success line. Saved roll reports normalize
  `frames[].output` to the durable u16 / `_hdr` publication paths instead of
  retaining deleted `.rsv-*` staging paths; each raw report must name every
  expected successful staging path before any image moves. The intentional strict probe now
  accepts only warning-promotion exit 1 with both the IR-ignored and strict
  diagnostics; usage/crash statuses and unrelated warnings fail. Hermetic tests
  cover each regression. Verified: 7 targeted harness tests; all 99 analysis
  tests; fmt; clippy with warnings denied; build; Rust tests (793 passed, 5
  ignored).
- 2026-08-11: Sidecar contract correction. Artifact validation now distinguishes
  generic JSON-object roll reports from conversion sidecars, which must carry the
  binary's real envelope: object-valued `meta` and `params`. The negative harness
  case uses parseable `{}` to prove a wrong envelope is rejected before
  publication; successful fakes emit the minimal valid envelope.
