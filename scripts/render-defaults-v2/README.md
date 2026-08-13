# Render defaults v2 measurement

`measure.py` reproduces the real-scan table behind
`docs/reports/render-defaults-v2.md`. It compares the former v1 exponential
settings with the then-new v2 defaults on four fixed Ektar and Portra 160 scans.

The script runs `nc convert` with explicit argument lists, reads only the JSON
report, and prints a Markdown table containing high-clipping percentage and mean
green output value. It also prints the resolved Dmax values as a calibration
sanity check. Rendered TIFFs live in a temporary directory and are deleted.

## Run

Prerequisites are a release build and the `../nc-assets` sibling path:

```sh
cargo build --release
python3 scripts/render-defaults-v2/measure.py
```

The frame list and film-base values are intentionally frozen in the script. This
is a historical report reproducer, not the general configuration-comparison
interface; use `nctool roll convert|analyze` for new studies.
