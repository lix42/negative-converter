# Asset Manifest

## Goal

Replace the hard-coded `ROLLS` array in `scripts/real-scan-verify/harness.sh`
with a tracked, generated **manifest** of the nc-assets folder — the single
source of truth for every file: which frames exist, their role (unexposed /
leader / real), their derived facts (dimensions, format, `ir_present`, checksum),
the standalone samples, and the converted outputs (nc versions + NLP).
Foundational for the whole conversion-analysis toolkit: analysis and comparison
both key off it.

A first manifest already exists — `manifest.json` at the assets root, generated
2026-07-24 (see `progress.md`) — along with precursor tooling:

- `scripts/analysis/generate_manifest.py` — the update-aware, stdlib generator
  (idempotent; preserves human fields; reuses unchanged checksums).
- `scripts/analysis/manifest.sample.json` — committed trimmed schema reference.
- the `asset-manifest` **skill** (`.agents/skills/asset-manifest/`) — how to run
  the generator.

This task **formalizes** that into the `nctool` package and adds the missing
`validate` mode (orphans / missing / drift). The generator's `generate` behavior
is largely done; fold it into `nctool manifest generate` and keep the script (or
retire it to the package). The shipped `manifest.json` / sample are the schema
reference.

## Design

The manifest lives **at the assets root** (`<asset-root>/manifest.json`), not in
the repo — so it travels with the assets over Google Drive and is available on
every machine. Consequently **paths inside it are relative to its own directory**
and there is no `asset_root` field: the tool is pointed at the folder (flag/env),
finds `manifest.json` there, and resolves relative paths against it. This is
what makes it machine-portable regardless of where Drive mounts.

Consumed by a small Python entry point (`python -m nctool manifest …` — the
package skeleton lands in [`conversion-metrics`](conversion-metrics.md); this task
may seed a minimal `manifest.py` ahead of it). JSON, not TOML, for repo
consistency (agents pipe JSON; recipes/reports are JSON).

Scope: **full inventory** — `rolls` (with roles), `samples` (standalone
fixtures, incl. the `largest.tif` perf worst-case), and `converted` (nc + nlp
buckets). Experiment fixtures were dropped from the folder (2026-07-24); the
committed decoder fixtures live in the repo under `tests/fixtures/`.

Schema (`schema_version: 1`), abbreviated — the live `manifest.json` is canonical:

```jsonc
{
  "schema_version": 1,
  "generated": "2026-07-24",
  "note": "Paths relative to this file's directory (the asset root).",
  "rolls": {
    "Ektar": {
      "stock": "Kodak Ektar 100",
      "frames": [
        { "file": "rolls/Ektar/20260713-nikon-963.tif", "role": "unexposed",
          "width": 4715, "height": 3297, "channels": 3, "bits": 16,
          "format": "hdri", "ir_present": true, "megapixels": 15.55,
          "sha256": "…", "bytes": 123456789 }
        // … leader + real frames
      ]
    },
    "Portra160-2026-07-22": { "note": "NLP source; no in-roll reference", "…": "…" }
  },
  "samples": [
    { "file": "samples/largest.tif", "kind": "perf-worst-case",
      "megapixels": 74.65, "ir_present": true, "…": "…" },
    { "file": "samples/icc/negative-embeded.tif", "kind": "icc-embed-test", "…": "…" }
  ],
  "converted": {
    "nc/2026-07-22": {
      "producer": "nc", "nc_version": "0.1.0", "regenerable": true,
      "outputs": [
        { "file": "converted/nc/2026-07-22/Ektar/…_positive.tiff",
          "source_frame": "rolls/Ektar/20260713-nikon-971.tif",
          "encoding": "u16-srgb" }        // sha256 omitted (regenerable)
      ]
    },
    "nc/V0": { "producer": "nc", "regenerable": false, "…": "…" },
    "nlp/2026-07-23": {
      "producer": "nlp", "regenerable": false,
      "outputs": [
        { "file": "converted/nlp/2026-07-23/…-positive.tif",
          "source_frame": "rolls/Portra160-2026-07-22/…tif",
          "note": "cropped + float vs source; no registration", "…": "…" }
      ]
    }
  },
  "coverage_gaps": ["NLP: … frames without an NLP output: …"]
}
```

- **role** ∈ `unexposed | leader | real`. `unexposed` → Dmin reference; `leader`
  → fully-exposed Dmax reference; everything else `real`. Seed roles from the
  classification recorded in `docs/reports/real-scan-verification.md`.
- **`regenerable`** distinguishes nc outputs reproducible from a recipe (sha256
  omitted to keep generation fast) from irreplaceable data (rolls, samples, NLP,
  V0 — all hashed).
- **`source_frame`** links every converted output back to its roll frame; it is
  how NLP↔nc↔source are aligned by identity (no pixel registration).
- **`coverage_gaps`** records known holes (e.g. source frames with no NLP output).

Commands:
- `manifest generate` — locate the assets root, walk it, fill derived fields from
  `nc inspect` (width/height/format/ir_present) + a streamed `sha256` + byte size.
  Roles are preserved from the existing manifest (human-seeded), defaulting new
  frames to `real`.
- `manifest validate` — check checksum drift, **orphans** (on disk, not in
  manifest) and **missing** (in manifest, not on disk). This is the cleanup
  surfacing mechanism: it reports disposable/stale candidates for the user's
  explicit decision — it never deletes.

The harness reads roles from the manifest instead of `ROLLS` (either directly, or
`nctool` emits the per-roll `unexposed|leader|real` triples the bash currently
hard-codes).

## Implementation Suggestion

- Reuse `nc inspect <file>` (already emits `decode.width/height`, format,
  `ir_present`) rather than decoding pixels in Python — keeps the "no pixels in
  context" invariant and avoids duplicating the decoder.
- Stream the sha256 in fixed-size chunks; never load a whole 50–224 MB file into
  memory at once.
- Keep `manifest generate` idempotent and role-preserving so re-running after
  adding frames is safe.

## How to Verify

- `manifest generate` on the current assets reproduces the six rolls (the five
  original + `Portra160-2026-07-22`) with the same unexposed/leader/real
  assignment recorded in `docs/reports/real-scan-verification.md`, the `samples`
  inventory, and the `nc/2026-07-22`, `nc/V0`, and `nlp/2026-07-23` converted
  buckets — matching the committed `manifest.json`.
- `manifest validate` on a clean tree reports zero orphans/missing; deleting or
  renaming a file makes it show up as missing/orphan; editing a file changes its
  checksum and is flagged as drift.
- The harness, driven from the manifest, produces the same frozen recipes as the
  hard-coded `ROLLS` run (byte-identical `recipes/`).

## Dependencies

- [Conversion-analysis tooling (spike)](conversion-analysis-tooling.md)
