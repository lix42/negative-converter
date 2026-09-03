# `nctool` analysis toolkit

`nctool` is the repository's Python command line for asset inventory, repeatable
roll conversion, and conversion analysis.

Run it from the repository root:

```sh
PYTHONPATH=scripts/analysis python3 -m nctool --help
```

### Dependencies

Every command except `metrics` uses only the Python standard library, and stays
that way. `metrics` reads output pixels, which needs `numpy` and `tifffile`:

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -r scripts/analysis/requirements.txt
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool metrics image --help
```

The import is lazy, so a checkout without the venv still runs every other
command. The metrics tests skip when the packages are absent; CI installs them
and sets `NCTOOL_REQUIRE_DEPS=1`, which turns a missing install into a failure
instead of a silent skip.

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

## Measure a converted image's own pixels

Every command above derives its numbers from `nc`'s JSON report, so they exist
only for nc outputs. `metrics` reads the output *image*, so an NLP conversion, a
SmartConvert TIFF, or an export edited by hand can be measured on the same
footing:

```sh
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool metrics image \
  ../nc-assets/converted/nlp/2026-08-04/20260803-film-1235-positive.tif \
  --space linear-srgb --inset 0.05
```

`--space` is required and never inferred. A TIFF's samples do not say whether
they are transfer-encoded — the NLP exports are 32-bit float with a *linear*
sRGB profile, nc writes transfer-encoded u16, and SmartConvert writes u16 with
no profile at all — so guessing produces a plausible wrong table rather than an
error. Supported: `srgb`, `linear-srgb`, `display-p3`, `linear-display-p3`,
`adobe-rgb`, `linear-adobe-rgb`, `prophoto`, `linear-prophoto`, `linear-bt2020`
— which covers the usual Lightroom export choices. PQ and HLG are recognized and
refused with a reason: they are absolute or display-referred, so comparing them
with an SDR rendition needs a reference-white normalization this command does not
implement yet.

`--inset F` trims that fraction off each edge and `--region x,y,w,h` takes an
explicit rectangle; both are **fractions**, because the images being compared do
not share dimensions. Use them to keep the film holder and rebate out of the
statistics until `film-base/ir-holder-detection` can supply a mask — and check
the inset actually clears the holder, which can occupy 10-15% of an edge.

It reads every sample in the region rather than subsampling, so peak memory
scales with the frame: ~1.2 GB at 18.7 MP, ~4.7 GB extrapolated to a 10368x7200
scan. Runtime is ~1.4 s at 18.7 MP.

The record reports endpoint occupancy on the stored (encoded) samples, then
tone statistics in log2 stops relative to 0.18 after decoding to linear light:
the key (geometric mean), a percentile vector, contrast spreads, toe and
shoulder spans, and band occupancy. Colour metrics are not implemented yet.

Two things the numbers mean, which are easy to misread:

- `endpoints.at_or_above_white` is an **upper bound** on what the producer
  clipped: a sample that legitimately landed on the endpoint is indistinguishable
  from one clamped to it. On an nc output the report's `loss.*` counters are the
  independent check, and the two agree to rounding when clipping is what happened.
- `tone.shoulder_span_stops` of 0 means p95, p99 and p99.9 are the same value —
  the top of the distribution is one flat step. On an uncropped scan that is
  usually **the film holder**, not the render: the holder blocks all light, so it
  is maximum density in the negative and renders to white. On one measured frame,
  tightening the inset from 0 to 0.15 took the top-code population from 9.5% to
  0% and the shoulder span from 0.000 to 0.417. Measure a region before concluding
  anything about highlights.

Colorimetry is not restated here: the primaries, white points and Bradford matrix
are transcribed from `src/pipeline/colorimetry/definitions.rs`, and the tests
re-read that file and the generated `derived-artifacts.txt` and fail if the
Python drifts from either. To support a new space, define it there first — that
is why `definitions::ADOBE_RGB` exists although nc renders to no such space.

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
NCTOOL_REQUIRE_DEPS=1 PYTHONPATH=scripts/analysis python3 -m unittest discover \
  -s scripts/analysis -p "test_*.py"
```

The tests are hermetic: they use temporary asset manifests, committed tiny TIFF
fixtures, and images synthesized in the test itself, rather than the Drive-hosted
scans. `NCTOOL_REQUIRE_DEPS=1` makes a missing `numpy`/`tifffile` a failure
instead of letting the metrics tests skip while the run still prints `ok`; leave
it unset locally if you have not made the venv. The harness tests additionally
need `cargo build` to have produced `target/debug/nc`.
