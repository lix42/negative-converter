# Render defaults v3 measurement

`measure.py` is a compact study of the pipeline-version-3 default-output change.
For the first real frame of each manifest roll that has a frozen recipe, it
renders:

- explicit `legacy` output as a TIFF, representing v2;
- the bare default `gain-map-hdr` output as a JPEG, representing v3.

It prints JSON rows with total clipping percentage and per-channel means, and
leaves both rendered files in a caller-selected directory for inspection.

## Run

The script currently assumes it is launched from the repository root, uses
`target/debug/nc`, reads `../nc-assets/manifest.json`, and requests a 16 GiB
memory budget:

```sh
cargo build
python3 scripts/render-defaults-v3/measure.py /tmp/nc-render-defaults-v3
```

This is a historical/ad-hoc measurement script rather than a stable CLI. For a
new roll-wide configuration study, prefer `nctool roll convert|analyze`, which
records calibration and recipe provenance.
