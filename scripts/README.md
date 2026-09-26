# Repository scripts

These tools verify, measure, and review `nc`; they are not part of the shipped
binary. Run commands from the repository root unless a tool's README says
otherwise.

| Directory or script | Purpose |
|---|---|
| [`analysis/`](analysis/) | The `nctool` CLI: asset manifests, manifest-driven roll conversion, diff-friendly roll analysis, build comparison, and pixel-derived image metrics. |
| [`real-scan-verify/`](real-scan-verify/) | The older staged full-resolution verification harness and its frozen recipes. |
| [`reference-snapshot/`](reference-snapshot/) | Build and cache the frozen pre-migration reference binary (`reserve`), and the pinned reference invocation. |
| [`iso-decoder-oracle/`](iso-decoder-oracle/) | A macOS ImageIO interoperability oracle for ISO and legacy gain-map JPEGs. |
| [`render-defaults-v2/`](render-defaults-v2/) | Reproduce the historical v1-to-v2 default-render measurements. |
| [`render-defaults-v3/`](render-defaults-v3/) | Render and measure the legacy-TIFF-to-gain-map-JPEG default transition. |
| `check-vendored-native.py` | Verify the checked-in libultrahdr and libjpeg-turbo source snapshots. |

## Vendored native-source check

`check-vendored-native.py` hashes every path and file payload in the two native
source snapshots. It also checks that the files present on disk are represented
in Git's index, catching upstream `.gitignore` rules that would otherwise make a
local snapshot pass while files were absent from a fresh checkout.

```sh
python3 scripts/check-vendored-native.py
```

The command exits non-zero if either snapshot differs from
`vendor/ultrahdr-sys/VENDORED_SNAPSHOT.json`. After intentionally changing and
reviewing the native sources, update the recorded snapshot with:

```sh
python3 scripts/check-vendored-native.py --write
```

`--write` accepts the current source tree as the new baseline; it is not a repair
operation and should only follow review of the native-source diff and pinned
revision.

## `../nc-assets` roll/file rename (2026-09-13)

`../nc-assets` rolls and `converted/nlp/*` folders were renamed to
`yyyy-mm-dd-stock` (date = the roll's last frame), and frame files inside each
roll/nlp folder were renamed to `<serial-number>.tif`, except each roll's
leader/base/calibration frames, now `leader.tif`/`base.tif`/`calibration.tif`.
`nctool manifest generate`/`validate` (see [`analysis/`](analysis/)) followed the
renames automatically via checksum matching, so `manifest.json` is current.

Old → new roll names:

| Old | New |
|---|---|
| `Ektar` | `2026-07-15-Ektar100` |
| `Portra160-2026-07-22` | `2026-07-23-Portra160` |
| `2026-09-09-Ektar` | `2026-09-09-Ektar100` |
| `2026-07-24-Gold200`, `2026-09-11-Portra400`, `2026-09-13-Portra400` | unchanged |

**This broke every script/fixture below that hardcodes the old roll names or
exact old frame filenames** (e.g. `20260713-nikon-971.tif`) — they were
*deliberately not updated* as part of this rename; fix them the next time you
touch that tool:

- [`analysis/fixtures.json`](analysis/fixtures.json) — every
  frame's `file`/`dmin_frame`/`dmax_frame`, plus the `Ektar`/`Portra160-2026-07-22`
  roll keys (moved from the retired `sigmoid-baseline/`)
- [`analysis/benchmark.json`](analysis/benchmark.json) and
  [`analysis/README.md`](analysis/README.md) — usages of the roll name `Ektar`
  as a live `nctool roll ...` argument
- `real-scan-verify/recipes/{Ektar,Portra160-2026-07-22}.provenance.json` — the
  `roll`/`frame` fields are now stale narration (the frozen `.json` recipes
  themselves hold only numeric values and still work unmodified)

## Privacy boundary

Most analysis commands consume only JSON metadata or stream files for hashing.
The review tools deliberately render personal photographs for a human,
but write them only to local throwaway directories outside the repository.
