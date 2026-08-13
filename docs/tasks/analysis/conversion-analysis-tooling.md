# Conversion-analysis tooling (spike)

## Goal

Grow the [`real-scan-verify`](../../../scripts/real-scan-verify/) harness into a
reusable **conversion-analysis toolkit**: an asset manifest, image-library-based
analysis of conversion results, and Negative Lab Pro (NLP) vs nc comparison — so
verifying and judging conversion quality is more powerful and easier to use.

**This is a spike first.** Discuss and decide the scope, structure, and tooling
before building; the deliverable is a short design note plus concretely-scoped
child tasks (or a decision to implement directly).

## Why

`real-scan-verification` produced a single staged bash harness driven by a
hard-coded roll→frame mapping (`ROLLS` in `harness.sh`), and the quantitative
analysis during that task (numpy/tifffile + ImageMagick: per-channel percentiles,
black/white points, saturation, clip %, JPG previews) was done ad hoc. To make
verification repeatable and to judge quality — including against NLP — that needs
to become tracked tooling.

## Scope (spike — decide before building)

1. **Asset manifest.** A tracked manifest of `../nc-assets/`: per roll, each
   frame's role (unexposed / fully-exposed leader / real), dimensions, format,
   `ir_present`, and a checksum. Drives the harness instead of the hard-coded
   `ROLLS` array, and lets us track every file. Decide schema, format (JSON/TOML),
   and how it is generated + validated.
2. **Conversion-result analysis.** Formalize the image-library analysis into a
   reusable script: per-channel percentiles, black/white points, contrast,
   saturation, clip %, and thumbnail/preview generation. Decide the metric set and
   the tooling (numpy+tifffile vs ImageMagick vs OpenImageIO vs a small Rust
   helper).
3. **NLP comparison.** Ingest NLP conversion outputs (the user will add them to
   `nc-assets`) and compare against nc's — metrics + side-by-side previews. Decide
   how to align them, since NLP and nc differ in color space, encoding, and
   framing (normalization / registration may be needed for a fair comparison).
4. **Harness UX.** Organize the scripts, provide a single documented entry point,
   and decide whether to stay in bash or move to a small Python/toolkit.

## Constraints

- **Tooling / analysis only** — does not touch the conversion pipeline or its
  determinism.
- **Never read sample pixels into an agent context** — derived numbers and
  downscaled thumbnails only, consistent with the harness invariant.

> Kept high-level on purpose: the manifest schema, metric set, comparison method,
> and language are exactly what the spike decides.

## Spike outcome (decided 2026-07-23)

Scope decided with the user. The spike produces this design note plus four
concretely-scoped child tasks; no code is written here.

**Decisions:**

- **Tooling: a Python package** `scripts/analysis/nctool/` (numpy + tifffile +
  Pillow, isolated venv — none installed system-wide; system Python is 3.14). It
  becomes the toolkit's single documented entry point (`python -m nctool …`) and
  **subsumes** `real-scan-verify`; `harness.sh` retires or reduces to a shim.
- **Asset root: configurable, local for now.** Keep reading `../nc-assets`;
  make `asset_root` a single overridable value with relative paths + portable
  checksums so a later Google Drive switch is a one-line change. Drive handling is
  its own deferred task.
- **Manifest: JSON, rolls + converted, experiments excluded.** Roles
  (`unexposed | leader | real`) are human-seeded; derived facts (dims / format /
  `ir_present` / sha256 / bytes) are generated from `nc inspect`. Replaces the
  hard-coded `ROLLS` array. `manifest validate` (orphans / missing / drift) is the
  cleanup surfacing mechanism — it reports, never deletes.
- **NLP comparison: global metrics + side-by-side thumbnails, no registration.**
  Align nc↔NLP↔source by manifest `source_frame` identity; per-pixel/registration
  is explicitly out of scope (differing color space + framing).

**Invariant preserved throughout:** only derived numbers (→ JSON) and downscaled
thumbnails leave the tools; full-res pixels are read one image at a time and never
surfaced to an agent context.

**Immediate cleanup done:** removed 4 stray `.DS_Store` files from `../nc-assets`.
`converted/V0/` is **kept** — it is the v0-baseline artifact set behind
`docs/reports/v0-baseline.md` (a `conversion-versioning` reference), and is tracked
as a manifest `converted` version.

**Update (2026-07-24) — assets moved to Google Drive, superseding two decisions
above.** The user relocated nc-assets from local `../nc-assets` to a Google
Drive folder and added new assets (a 74.6 MP `largest.tif` perf worst-case; an
NLP-converted set of the new `Portra160-2026-07-22` roll). In response:

- **Asset root** is now the Drive folder (not "local for now"); the migration is
  in progress, not deferred. The `manifest.json` lives **at the assets root** with
  paths relative to its own directory, so the `asset_root`-env indirection is
  gone — the tool is pointed at the folder and self-locates. Repo `../nc-assets`
  references get a machine-local symlink bridge (see
  [`drive-asset-migration`](drive-asset-migration.md)).
- **Manifest scope** broadened from "rolls + converted" to a **full inventory**
  (adds `samples`); experiment fixtures were **dropped** from the folder.
- The folder was **reorganized** into `rolls/ samples/ converted/{nc,nlp}/` and a
  first `manifest.json` generated (2026-07-24). NLP outputs are recorded as a
  `converted` bucket with `source_frame` links; the 2 uncovered source frames are
  in `coverage_gaps`.

**Child tasks:**

- [Asset Manifest](asset-manifest.md) — schema + `generate`/`validate`; retire
  `ROLLS`. (Foundational.)
- [Conversion Metrics & Photographic Analysis](conversion-metrics.md) — the Python toolkit
  skeleton + metric set + thumbnails → JSON/Markdown. Folds in the harness-UX /
  single-entry-point item.
- [NLP vs nc Comparison](nlp-comparison.md) — ingest NLP outputs, global-diff +
  contact sheets. Startable once the user adds NLP outputs.
- [Drive Asset Migration](drive-asset-migration.md) — deferred: path portability,
  materialization guard, sync hygiene.

## Dependencies

- [Real-scan core verification](real-scan-verification.md) — provides the harness,
  frozen recipes, and the asset set this builds on.
