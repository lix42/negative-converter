# `nctool` analysis toolkit

`nctool` is the repository's Python command line for asset inventory, repeatable
roll conversion, and conversion analysis. It uses only the Python standard
library today.

Run it from the repository root:

```sh
PYTHONPATH=scripts/analysis python3 -m nctool --help
```

The default asset root is `../nc-assets`. Override it with `--asset-root` or the
`NC_ASSET_ROOT` environment variable.

## Asset manifest

```sh
PYTHONPATH=scripts/analysis python3 -m nctool manifest generate \
  --asset-root ../nc-assets --nc target/release/nc
PYTHONPATH=scripts/analysis python3 -m nctool manifest validate \
  --asset-root ../nc-assets
PYTHONPATH=scripts/analysis python3 -m nctool manifest roles \
  --asset-root ../nc-assets
```

- `generate` inventories source rolls, samples, and converted outputs; obtains
  derived metadata from `nc inspect`; and streams files through SHA-256. Existing
  human fields such as roles, stock names, and notes are preserved.
- `validate` reports checksum drift, missing files, misplaced/orphaned TIFFs, and
  integrity gaps. It never deletes or moves anything.
- `roles` emits the unexposed/leader/real-frame grouping consumed by the legacy
  real-scan harness.

`generate_manifest.py` is a compatibility wrapper for older callers. New code
should use `python -m nctool manifest generate`. `manifest.sample.json` is a
trimmed schema example; the live manifest belongs at the asset root.

## Manifest-driven roll conversion

This command automates the calibrate-once/apply-many workflow from
`docs/using-nc.md`:

```sh
PYTHONPATH=scripts/analysis python3 -m nctool roll convert Ektar \
  --nc target/release/nc \
  --config sigmoid-p3 \
  --output-preset display-p3 \
  --strict-estimate
```

It performs these operations:

1. Finds the roll's single `unexposed`, single `leader`, and all `real` frames in
   `manifest.json`.
2. Verifies every source frame against its manifest SHA-256, so stale asset bytes
   cannot be attributed to a configuration change.
3. Measures Dmin from the unexposed frame and Dmax from the leader. Both default
   sample rectangles are the center 80% (`x=10%`, `y=10%`, `width=80%`,
   `height=80%`). Override either with
   `--dmin-region` or `--dmax-region`. Dmin uses a five-cell grid by default;
   pass `--dmin-mode region` to aggregate its selected region without the grid.
   If the leader is clipped beyond the scanner boundary, pass a deliberately
   chosen positive `--d-max D` to skip leader estimation; calibration and tags
   record it as an `explicit-override`, not a measured reference.
4. Reads the tested binary's complete `nc params` document, overlays the optional
   partial recipe, then freezes both measurements. This pins defaults such as the
   sigmoid anchor instead of letting a later build reinterpret an underspecified
   recipe.
5. Runs `nc roll` over the real frames with that shared recipe.
6. Writes `recipe.json`, `calibration.json`, `roll-report.json`, and `tags.json`
   beside the converted images.

The default destination is:

```text
<asset-root>/converted/nc/<config>/<roll>/
```

If `--config` is omitted, a stable ID is derived from the frozen recipe. A
non-empty destination is refused so a new run cannot silently mix with or
overwrite an old configuration.

Use `--recipe FILE` for the full configuration surface. It accepts a partial nc
recipe or an image sidecar envelope; measured Dmin and Dmax deliberately replace
any calibration values in it. `--output-preset`, `--print-exposure`, and
`--film-type` are convenience overrides. `--strict-estimate` is recommended for
calibration; `--strict-roll` is separate because a frozen explicit base on an IR
scan can legitimately emit the documented unused-IR warning.

### Tags

`tags.json` is a small index for the run. It records the configuration ID, source
roll, source-frame checksums, frozen recipe, calibration frames/regions/values, build identity, report
path, and roll summary. `calibration.json` retains the complete `nc estimate`
reports. Individual image sidecars remain the authoritative per-output recipe
and identity record.

After a successful TIFF-producing conversion, regenerate the asset manifest so
its converted bucket includes the new TIFFs:

```sh
PYTHONPATH=scripts/analysis python3 -m nctool manifest generate \
  --asset-root ../nc-assets --nc target/release/nc
```

The current manifest schema inventories TIFF artifacts only. A default
gain-map JPEG or HDR AVIF run is still fully described by its `tags.json`, roll
report, and optional `analysis.json`, but `manifest generate` will not add those
container files to `manifest.json` yet.

## Analyze a converted roll, then compare with `diff`

```sh
PYTHONPATH=scripts/analysis python3 -m nctool roll analyze Ektar sigmoid-p3
PYTHONPATH=scripts/analysis python3 -m nctool roll analyze Ektar exponential-p3
diff -u \
  ../nc-assets/converted/nc/sigmoid-p3/Ektar/analysis.json \
  ../nc-assets/converted/nc/exponential-p3/Ektar/analysis.json
```

The run operand is a configuration ID or an explicit path to `tags.json`.
`analyze` writes `analysis.json` beside that tag by default; use `--out FILE` to
choose another destination. The artifact contains:

- the frozen recipe, calibration, build identity, and source checksums;
- stable per-frame film-base, Dmax, input-semantics, output-statistics, clipping,
  identity, status, and warning fields;
- deterministic key and frame ordering.

It deliberately omits timestamps, elapsed time, memory/machine facts, and
absolute input/output paths, which would create irrelevant diffs. It does not
reread pixels, so equal analysis files mean the recorded conversion facts agree;
they do not prove that output files are byte- or pixel-identical.

## Compare two builds

The older `compare run|diff` workflow answers a different question: how one fixed
benchmark behaves under two `nc` builds.

```sh
PYTHONPATH=scripts/analysis python3 -m nctool compare run \
  --nc /path/to/baseline/nc --out before.json
PYTHONPATH=scripts/analysis python3 -m nctool compare run \
  --nc /path/to/candidate/nc --out after.json
PYTHONPATH=scripts/analysis python3 -m nctool compare diff before.json after.json
```

Cases come from `benchmark.json`. The default `fixtures` set is self-contained;
the `rolls` set resolves real scans and checksums through the asset manifest.
Run records include build identity, pipeline version, input digest, parameter
hash, output depth, means, clipping counts, and telemetry timings. Timing changes
are informational and never decide the deterministic-statistics verdict.

## Tests

The CI command is:

```sh
PYTHONPATH=scripts/analysis python3 -m unittest discover \
  -s scripts/analysis -p "test_*.py"
```

The tests are hermetic: they use temporary asset manifests and committed tiny
TIFF fixtures rather than the Drive-hosted scans.
