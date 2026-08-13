# Repository scripts

These tools verify, measure, and review `nc`; they are not part of the shipped
binary. Run commands from the repository root unless a tool's README says
otherwise.

| Directory or script | Purpose |
|---|---|
| [`analysis/`](analysis/) | The `nctool` CLI: asset manifests, manifest-driven roll conversion, diff-friendly roll analysis, and build comparison. |
| [`real-scan-verify/`](real-scan-verify/) | The older staged full-resolution verification harness and its frozen recipes. |
| [`iso-decoder-oracle/`](iso-decoder-oracle/) | A macOS ImageIO interoperability oracle for ISO and legacy gain-map JPEGs. |
| [`render-defaults-v2/`](render-defaults-v2/) | Reproduce the historical v1-to-v2 default-render measurements. |
| [`render-defaults-v3/`](render-defaults-v3/) | Render and measure the legacy-TIFF-to-gain-map-JPEG default transition. |
| [`sigmoid-baseline/`](sigmoid-baseline/) | Generate local visual-review pages used by the sigmoid calibration study. |
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

## Privacy boundary

Most analysis commands consume only JSON metadata or stream files for hashing.
The sigmoid review tools deliberately render personal photographs for a human,
but write them only to local throwaway directories outside the repository.
