---
name: asset-manifest
description: >-
  Scan the nc-assets folder and generate or update its manifest.json — the tracked
  inventory + source-of-truth for every roll frame (with role), standalone sample,
  and converted output (nc + NLP). Use when the user adds/removes/renames assets,
  moves the assets folder, asks to "regenerate/update the manifest", "re-scan
  nc-assets", "rebuild the asset inventory", check what assets exist, or find
  stale/orphaned files. Runs a stdlib Python generator that reads only derived
  numbers (nc inspect) + checksums — never sample pixels.
---

# asset-manifest

The nc-assets **manifest** (`<asset-root>/manifest.json`) is the tracked
inventory and source-of-truth for the asset folder. It records every file's
derived facts (dimensions, format, `ir_present`, `sha256`, bytes), each roll
frame's **role**, and links every converted output back to its source frame.

It lives **at the assets root** (not in the repo) with paths **relative to its
own directory**, so it travels with the assets over Google Drive and works on any
machine regardless of where Drive mounts. A trimmed schema reference is committed
at [`scripts/analysis/manifest.sample.json`](../../../scripts/analysis/manifest.sample.json).

## Invariants (do not break)

- **Never read sample pixels into an agent context.** The generator shells out to
  `nc inspect` for metadata and streams bytes only to hash them; it emits derived
  numbers only. Keep it that way.
- **Roles and other human fields are data, preserved across updates** — never
  overwrite them from a re-scan.
- Analysis/comparison tooling keys off this manifest; keep it the single source of
  truth.

## Generate / update

Run the committed generator from the repo root:

```bash
python3 scripts/analysis/generate_manifest.py [ASSET_ROOT] [--nc PATH] [--reuse-hash] [--dry-run]
```

- `ASSET_ROOT` defaults to `$NC_ASSET_ROOT`, else `../nc-assets` (the machine-local
  symlink to the Drive folder). Point it elsewhere to scan a different copy.
- The `nc` binary is found via `--nc`, `$NC`, `./target/release/nc`,
  `./target/debug/nc`, or `nc` on `PATH` — each candidate is **verified** to be
  this project's CLI (`--version` prints `nc <ver>`), so the system netcat
  (`/usr/bin/nc`) is never mistaken for it. An **explicit** `--nc`/`$NC` that is
  missing or not this CLI is a **hard error** (exit 2), not a silent fallback;
  only failed *auto-discovery* falls back to `exiftool` (losing authoritative
  `format`/`ir_present`) with a warning. Build it if missing: `cargo build --release`.
- `NC_MANIFEST_DATE=$(date +%F)` sets the `generated` field; the script leaves it
  stable rather than stamping wall-clock time (so re-runs stay byte-identical) —
  pass it for a real date.
- The run prints a summary (roll/sample/bucket counts, coverage gaps) and writes
  `manifest.json` in place. `--dry-run` prints the summary without writing.

The generator is **idempotent and update-aware**: it loads any existing
`manifest.json` and preserves human-maintained fields — roll frame `role`, roll
`stock`/`note`, sample `kind`/`note`, per-output `note`, and each converted
bucket's `regenerable`/`nc_version`/`recipe_dir`/`note`. Preserved fields survive
an asset **rename** by matching sha256 identity — but only when the old path is
gone (a *copy* keeps its own default, so a calibration role isn't cloned).
Checksums are **recomputed every run** by default (source-of-truth integrity);
`--reuse-hash` reuses an existing `sha256` when the byte size is unchanged (faster
warm updates, but a same-size edit would go undetected).

## Folder layout the scanner expects

```
<asset-root>/
  manifest.json
  rolls/<roll>/<frame>.tif          # roles: unexposed | leader | real
  samples/<file>.tif  samples/<sub>/<file>.tif   # standalone fixtures
  converted/<producer>/<version>/[<roll>/]<file> # producer: nc | nlp
```

- New rolls/frames default `role: "real"`; mark `unexposed`/`leader` by editing
  `manifest.json` (preserved thereafter). Known nc rolls have seeded
  roles/stock for first-ever generation.
- `converted/nc/*` buckets default `regenerable: true` (sha256 skipped, since the
  harness reproduces them) except `V0`; `nlp/*` and everything else are hashed.
- `source_frame` for a converted output is resolved by matching its filename stem
  against the source roll's frames; NLP outputs map to `Portra160-2026-07-22`.
- The inventory tracks **image artifacts** (`.tif`/`.tiff`). Companion files that
  share an image's stem — `.json` recipe/report sidecars and `.jpg` previews — are
  intentionally **not** separate entries; they travel with their image. A future
  orphan/missing validator must treat a stem-matched `.json`/`.jpg` as a companion
  of its tracked image, not as an untracked orphan.

## Metadata source & fallback

`format`/`ir_present` are authoritative only from `nc inspect`. When `nc` can't
decode a file (e.g. 32-bit float outputs — nc's `_positive_hdr` floats and NLP
positives — which it rejects as unsupported), the generator falls back to
`exiftool` and tags that entry `metadata_source: "exiftool"` with best-effort
`format: "tiff"` / `ir_present: false`. Absence of the tag means the values are
nc-authoritative. If `nc` is missing/fails on a file that previously had
authoritative values, the generator **carries the prior values forward** (and
says so) — but only when the current bytes still match the prior `sha256`, so a
file replaced at the same path is never paired with stale metadata. Carry-forward
is therefore **checksum-gated**: `regenerable` outputs skip checksums, so they are
not carried (they are reproducible — just re-run with `nc` present). The run exits
non-zero only when a file fails *all* inspection (an `error` entry).

## After generating

- Skim the printed summary and `coverage_gaps` for anything unexpected (a new
  orphan, a frame that failed inspection, a missing NLP counterpart). A count of
  `exiftool`-fallback entries is expected (the float outputs); an `error` count is
  not.
- To surface stale/removed files, compare disk vs manifest — files in the manifest
  whose paths no longer exist are stale; files on disk absent from the manifest are
  untracked. (A dedicated `validate` mode is the `asset-manifest` task's follow-up.)
- The manifest is not committed (it lives with the assets); update the committed
  `manifest.sample.json` only if the **schema** changes, not on data changes.
