# real-scan-verify

Tooling for the [`real-scan-verification`](../../docs/tasks/real-scan-verification.md)
task: drive the compiled `nc` binary over the user's full-size real scans and
record **derived numbers only** (JSON reports, `exiftool` structure, RSS). It
**never reads sample pixels into an agent context**. Rerun it whenever the assets
or the conversion defaults change.

Results write-up: [`docs/reports/real-scan-verification.md`](../../docs/reports/real-scan-verification.md).

## Contents

- `harness.sh` — the staged verification runner.
- `recipes/` — frozen per-roll calibration produced by the `freeze` stage:
  `<roll>.json` (16-bit), `<roll>.hdr.json` (float), `<roll>.provenance.json`
  (source frame/region + estimator warnings). Committed; reused by
  `display-output-acceptance`.

## Prerequisites

- A release build of `nc`: `cargo build --release` (the harness auto-locates
  `target/release/nc` at the repo root).
- The real scans at `../nc-assets/` (sibling of the repo, per CLAUDE.md).
- `jq`, `python3`, and macOS `/usr/bin/time` on `PATH`.

## Usage

```bash
# from the repo root (or anywhere — paths resolve relative to the script)
bash scripts/real-scan-verify/harness.sh [stage]
```

Stages (no argument runs `freeze → convert → ir → determinism → resource`):

| Stage | What it does |
|---|---|
| `classify` | grid-classify every frame per roll → unexposed / fully-exposed / real |
| `freeze` | measure per-roll `Dmin` (unexposed frame) + `Dmax` (leader), write `recipes/` |
| `convert` | roll-convert every real frame, 16-bit + float HDR, into the output dir |
| `ir` | export the IR plane; check `--strict` promotes warnings to a hard error |
| `determinism` | re-run byte-identical + `--dump-params` reload byte-identical |
| `resource` | `/usr/bin/time -l` peak RSS + wall-clock on the largest scan |

## Configuration (env overrides)

| Var | Default | Meaning |
|---|---|---|
| `NC` | `<repo>/target/release/nc` | the binary under test |
| `A` | `<repo>/../nc-assets` | assets root |
| `OUTDIR` | `$A/converted/2026-07-22` | converted-image output dir |
| `ART` | `/private/tmp/rsv-artifacts` | per-run JSON reports (not committed) |

Example — verify a debug build against a scratch output dir:

```bash
NC=target/debug/nc OUTDIR=/tmp/out bash scripts/real-scan-verify/harness.sh convert
```

## Notes

- The roll → {unexposed, fully-exposed, real frames} mapping is hard-coded in the
  `ROLLS` array at the top of `harness.sh`; update it when assets change. A future
  [`conversion-analysis-tooling`](../../docs/tasks/conversion-analysis-tooling.md)
  task will drive this from an asset **manifest** instead and add image-library
  analysis + NLP-vs-nc comparison.
- Converted images are large and **not committed**; regenerate with `convert`.
