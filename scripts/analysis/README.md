# `nctool` analysis toolkit

`nctool` is the repository's Python command line for asset inventory, repeatable
roll conversion, and conversion analysis.

Run it from the repository root:

```sh
PYTHONPATH=scripts/analysis python3 -m nctool --help
```

### Dependencies

Every command except `metrics` uses only the Python standard library, and stays
that way. `metrics` reads output pixels, which needs `numpy`, `tifffile` and
`Pillow` (the last for JPEG):

```sh
uv venv --python 3.12
uv pip install -r scripts/analysis/requirements.txt
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool metrics image --help
```

**[uv](https://docs.astral.sh/uv/) is how the environment is made**, here and in
CI — one shape, and about twenty times faster than `venv` + `pip` (3.8 s to 0.2 s
warm, measured). What it produces is an ordinary virtual environment in `.venv/`,
so everything downstream is unchanged and `python3 -m venv .venv && .venv/bin/pip
install -r …` still works if you have no uv.

**The Python version is stated, not inherited.** `uv venv` otherwise takes
whichever interpreter it finds first — which is not the system one, and differs
between machines and between the two CI runner images. uv fetches 3.12 itself, so
nothing has to be installed first.

`.venv/` is gitignored, as a virtual environment always should be: it holds
compiled, platform-specific wheels and absolute paths, so it is build output
rather than source. A fresh checkout therefore runs the two lines above once.

The import is lazy, so a checkout without the venv still runs every other
command. The metrics tests skip when the packages are absent; CI installs them
and sets `NCTOOL_REQUIRE_DEPS=1`, which turns a missing install into a failure
instead of a silent skip.

The default asset root is `../nc-assets`. Override it with `--asset-root` or the
`NC_ASSET_ROOT` environment variable.

## Asset manifest

```sh
PYTHONPATH=scripts/analysis python3 -m nctool manifest generate \
  --asset-root ../nc-assets --nc target/release/hanten
PYTHONPATH=scripts/analysis python3 -m nctool manifest validate \
  --asset-root ../nc-assets
PYTHONPATH=scripts/analysis python3 -m nctool manifest roles \
  --asset-root ../nc-assets
```

- `generate` inventories source rolls, samples, and converted outputs; obtains
  derived metadata from `hanten inspect`; and streams files through SHA-256. Existing
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
  --nc target/release/hanten \
  --config sigmoid-p3 \
  --output-preset display-p3 \
  --strict-estimate
```

It performs these operations:

1. Finds the roll's single `unexposed` frame and all `real` frames in
   `manifest.json`.
2. Verifies every source frame against its manifest SHA-256, so stale asset bytes
   cannot be attributed to a configuration change.
3. Measures Dmin from the unexposed frame, over the center 80% (`x=10%`, `y=10%`,
   `width=80%`, `height=80%`) unless `--dmin-region` says otherwise. Dmin uses a
   five-cell grid by default; pass `--dmin-mode region` to aggregate its selected
   region without the grid. No `Dmax` is measured: the roll reference density
   retired with the placements that read it (`nf-retire/dmax-machinery`).
4. Reads the tested binary's complete `hanten params` document, overlays the optional
   partial recipe, then freezes the measurements. This pins defaults such as the
   curve's anchor placement instead of letting a later build reinterpret an
   underspecified recipe.
5. Runs `hanten roll` over the real frames with that shared recipe.
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
recipe or an image sidecar envelope; the measured Dmin deliberately replaces
any film base in it. `--output-preset`, `--print-exposure`, and
`--film-type` are convenience overrides. `--strict-estimate` is recommended for
calibration; `--strict-roll` is separate because a frozen explicit base on an IR
scan can legitimately emit the documented unused-IR warning.

### Tags

`tags.json` is a small index for the run. It records the configuration ID, source
roll, source-frame checksums, frozen recipe, calibration frames/regions/values, build identity, report
path, and roll summary. `calibration.json` retains the complete `hanten estimate`
reports. Individual image sidecars remain the authoritative per-output recipe
and identity record.

After a successful TIFF-producing conversion, regenerate the asset manifest so
its converted bucket includes the new TIFFs:

```sh
PYTHONPATH=scripts/analysis python3 -m nctool manifest generate \
  --asset-root ../nc-assets --nc target/release/hanten
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
- stable per-frame film-base, input-semantics, output-statistics, clipping,
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

It reads **TIFF and JPEG**, dispatched on the file's magic bytes rather than its
extension.

`--space` is required and never inferred. A file's samples do not say whether
they are transfer-encoded — the NLP TIFF exports are 32-bit float with a *linear*
sRGB profile, nc writes transfer-encoded u16, and SmartConvert writes u16 with
no profile at all — so guessing produces a plausible wrong table rather than an
error. Supported: `srgb`, `linear-srgb`, `display-p3`, `linear-display-p3`,
`adobe-rgb`, `linear-adobe-rgb`, `prophoto`, `prophoto-gamma1.8`,
`linear-prophoto`, `linear-bt2020`, `linear-acescg` — which covers the usual
Lightroom export choices. The two ProPhoto entries are deliberate: `prophoto` is
ISO 22028-2 as specified, with the linear toe, and is right for a third-party
export; `prophoto-gamma1.8` is the pure power law nc's retired
`--output-profile prophoto` wrote (the reference build still writes it). They agree above encoded 0.03125 and diverge sharply
below it, so the wrong one silently rewrites the deep-shadow statistics. PQ and HLG are
recognized and refused with a reason: they are absolute or display-referred, so
comparing them with an SDR rendition needs a reference-white normalization this
command does not implement yet.

### JPEG input

8-bit JPEG is read as well as TIFF, which is what makes an NLP or Lightroom JPEG
export measurable without a re-render. Two things ride in the record because a
lossy read needs them: `image.bits_per_sample` (8 bits is 256 levels, and the
tone metrics are logarithmic, so a JPEG's deep-shadow percentiles and `toe_span`
are quantization-limited — cross-checked against a 16-bit TIFF of the same
content, the key and p95 agreed exactly while p0.1 moved ~0.015 stops), and
`image.decoder`, naming the Pillow/libjpeg build that produced the samples, since
a JPEG's pixels are whatever its decoder says they are.

`--jpeg-image sdr|hdr` chooses which rendition of a **gain-map** JPEG to measure.
`sdr` is the default and reads the base image — which is also all a plain JPEG
has. The record marks `gain_map_present` so the base of a dual-image file is never
mistaken for the rendition an HDR-aware viewer shows. `hdr` is **not implemented**
and says so: reconstructing it means applying the gain map with its ISO 21496-1 /
Ultra HDR metadata, and a reconstruction that is subtly wrong yields plausible
wrong numbers rather than an error. Measure nc's own `hdr-linear-tiff` render of
the same source instead.

`--inset F` trims that fraction off each edge and `--region x,y,w,h` takes an
explicit rectangle; both are **fractions**, because the images being compared do
not share dimensions. Use them to keep the film holder and rebate out of the
statistics until `film-base/ir-holder-detection` can supply a mask — and check
the inset actually clears the holder, which can occupy 10-15% of an edge.

It reads every sample in the region rather than subsampling, so peak memory
scales with the frame: ~1.4 GB at 18.7 MP, ~5.7 GB extrapolated to a 10368x7200
scan. Runtime is ~3.0 s at 18.7 MP.

The record reports endpoint occupancy on the stored (encoded) samples, then,
after decoding to linear light:

- **tone**, in log2 stops relative to 0.18 — the key (geometric mean), a
  percentile vector, contrast spreads, toe and shoulder spans, band occupancy,
  and an L\*-binned histogram for luminance and each channel;
- **colour**, in CIELAB — per-channel balance in stops, mean cast and chroma,
  neutral share, chroma percentiles, six hue sectors, and the cast of each tone
  band separately.

### Tone bands

The bands are cut in **CIELAB lightness** — every 15 L\* up to 75, then diffuse
white (L\* 100), then an overflow band above it. `record.bands` states the cut,
in both L\* and stops, inside every record it applies to.

Lightness rather than stops because equal steps of lightness are unequal steps of
exposure, and a cut even in stops is even in nothing a viewer sees. Until schema
2 the edges were -4 / -2 / +2 stops and diffuse white, after Zones III and VII;
across 33 real renders (six frames x five `hanten convert --preset` bundles plus
three Negative Lab Pro references) that put a median 83% of the frame — 95% at
worst — in `mid` alone, while `highlight` spanned 0.47 stops and read 0.00 on
four of the five renders of one frame. The lightness cut's largest band holds a
median 46% and 56% at worst. On the five renders of one frame the old band vector
spread 2.1 percentage points from preset to preset, so the five presets were
effectively one reading; the new one spreads 24.0.

`above_diffuse_white` is an **overflow bin, not a seventh of the range**. An SDR
rendition essentially cannot populate it, and on a float or HDR output it is the
only place in the tone stage where headroom above display white appears.

`color.cast_by_tone_band` is the one to read first on a negative conversion. The
characteristic fault is **crossover** — the cast drifting one way in the shadows
and the other as the frame brightens — and a whole-frame cast averages exactly
that out to nothing. On one measured nc-versus-NLP pair the nc render went from
`b* = -0.7` in deep shadow to `-32.2` in midtones where NLP moved `-0.1` to
`-3.8`; the whole-frame means alone would have understated it.

The rollup's `crossover_a` / `crossover_b` axes difference the **`shadow` and
`mid`** bands specifically — not shadow and highlight. Across those 33 renders
the smallest `shadow` was 4.2% of the region and the smallest `mid` 5.7%, while
`highlight` legitimately empties on a dark frame, which would make the axis
vanish exactly where a render is darkest.

Each `cast_by_tone_band` entry carries its own denominator — `pixels`, and
`sparse` when the band holds under 0.1% of the region. Sparse entries are kept
rather than dropped, because a band set that varies frame to frame cannot be
diffed, but **a sparse band's cast is not a measurement**: on the old cut the
largest colour excursion in one measured record, a\* = -19.5 in `highlight`, was
the colour of a **single pixel** out of 15.1 million, printed beside a `mid` cast
resting on 91.7% of the frame with nothing to tell the two apart. The
rollup's `crossover_a` / `crossover_b` are withheld outright when either
contributing band is sparse.

### The histogram

`tone.histogram` is the record's only list-valued field, and the one thing in it
a review tool can draw rather than read. Four series — `luminance`, `r`, `g`, `b`
— each 200 counts, one per L\* unit, plus `above_range` for anything past the top
and separate counters for samples with no lightness at all (non-positive,
non-finite). Those four numbers partition the region. It states its own domain in
the record — `domain`, `lstar_range`, `bins`, `bin_width_lstar`, and the
`mid_grey_bin` / `diffuse_white_bin` reference lines a chart wants — so a
consumer never has to infer the bins from the shape of the data, and never has to
re-derive the L\* formula to place white.

`luminance` uses the **declared space's own luma weighting**, the same one
`tone.percentiles_stops` is built from, so the histogram and the percentile curve
describe one quantity and cannot disagree. That is also why luminance is emitted
rather than left to be derived at draw time: luma is a weighted sum of linear
channel values and is **not** recoverable from three independent per-channel
histograms.

The axis runs to **twice diffuse white in lightness**, not to diffuse white.
L\* 200 is 6.46x diffuse white (+5.17 stops), which covers nc's own 1000/203 HDR
ceiling (L\* 181.4) with margin, so a `film-master` or `hdr-linear-tiff` render's
headroom can be *drawn* rather than reduced to one overflow number. It also keeps
white inside the axis rather than at its edge, which is what makes the commoner
SDR question readable: how far short of diffuse white the highlights stop. On the
five preset renders of one frame the last non-empty luminance bin sits at L\* 88 /
92 / 88 / 92 / 98 — 12, 8, 12, 8 and 2 L\* short of white — against a
`diffuse_white_bin` of 100.

L\* and not the stored code values: those describe the file's encoding as much as
the picture, which is the whole reason this command decodes to linear light
first. L\* and not stops: stops give black an unbounded tail no chart can draw.
And because the bands are cut on the same axis, every band edge falls exactly on
a bin edge — one chart can shade the bands over the bars without interpolating.

The channel series apply the same L\* curve to one channel. That is a level, not
a colorimetric lightness — only `luminance` is that — but it is the one monotone
mapping that puts all four series on one axis, which is what makes a cast read as
a shape rather than as `color.balance_stops`' one number per channel.

It costs ~0.6 s at 18.7 MP and no measurable memory: it streams in row blocks, so
only the 200 accumulators per series survive a block. A record grows to ~8 KB.

### Reading the record

`color.balance_support` states what fraction of the region each channel's
geometric mean rests on. When they disagree — a channel crushed to black over
part of the frame — `r_over_g` / `b_over_g` are **omitted** rather than reported
against different pixel sets, which once made a heavily cast frame read as
perfectly balanced.

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
Python drifts from either. To support a new space, define it there first — which
is how `definitions::ADOBE_RGB` came to exist before nc rendered to it, and why
`definitions::PROPHOTO` stays although nc no longer does.

### A whole roll at once

```sh
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool metrics roll Ektar sigmoid-p3 \
  --inset 0.08 --markdown docs/reports/ektar-sigmoid-p3.md
```

The run operand is a configuration ID or a path to `tags.json`, as for `roll
analyze`. It measures every successfully converted frame and writes
`metrics.json` beside the tag, with per-frame records embedded plus a spread
table; `--markdown` also renders the table, and `metrics table <metrics.json>`
re-renders it later without re-reading pixels.

The colour space is **resolved from the run's frozen recipe** here rather than
declared — that is recorded provenance, not a guess at the pixels — and an
under-determined one is refused rather than defaulted:

| preset | space | notes |
|---|---|---|
| `legacy`, `custom` (default profile) | `srgb` | retired presets, still read from reference-build renders |
| `legacy`, `custom` + `--output-profile` | that profile's space | `prophoto` resolves to `prophoto-gamma1.8` |
| `compatibility` | `srgb` | |
| `display-p3` | `display-p3` | |
| `gain-map-hdr`, `ultra-hdr-v1` | `display-p3` | the JPEG's **SDR base**, per `--jpeg-image` |
| `film-master` | `linear-acescg` | |
| `hdr-linear-tiff` | `linear-bt2020` | |

Everything else is refused with the reason: `hdr-pq`/`hdr-hlg` write AVIF, the
coded HDR TIFFs are PQ/HLG encoded, an `--output-profile` path has no primaries
here, and an f32 `legacy` TIFF's transfer was never established. `--space`
overrides all of it.

nc's default preset writes a gain-map JPEG, and a default roll therefore measures
as its **SDR base**. That is a real rendition, not a fallback — it is what a
non-HDR viewer shows — but it is not what an HDR-aware viewer shows, and the
per-frame records mark `gain_map_present` accordingly.

A frame that fails to measure is recorded in `skipped` and the command exits 1 —
the rest of the roll is still measured and written, but a partial roll never
reports success.

Read the spread, not the mean: frame 3 is a backlit portrait and frame 11 a
shaded street, so averaging their exposures describes the subjects. What the
spread is **not** is attributable — one frozen recipe served every frame, so
variation combines scene content with how well that calibration fits, and those
cannot be separated from one roll's numbers. The extremes are named so you can
look at those frames. There is deliberately no outlier rule and no verdict.

## Render a review set — `review generate`

Comparing conversions **by eye** goes through `tools/review-app`, and this is what
produces what it reads:

```sh
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool review generate \
  scripts/preset-review/presets.matrix.json --out ../temp/preset-review
```

The matrix is **data** (`scripts/preset-review/presets.matrix.json` is the worked
example): it names the configurations, the flags each one passes, and the
per-roll values those flags need. Every cell is one `hanten convert`; beside each
rendition the command writes that image's metric record, so the app can draw the
tone and cast charts next to the picture. Frames and per-roll `Dmin` come from
`scripts/analysis/fixtures.json` — the same declaration the metrics read,
so the two cannot drift.

Four rules it holds to, each of which has a reason rather than a preference:

- **The matrix states the preset once**, as `output_preset` — or, for the new
  chain, the `destination` it renders (the recipe `output` value with all four axes
  stated, `{"display": {"range", "transfer", "gamut", "container"}}`, or
  `"film-master"`; the generator then passes `--new-flow` and the destination flags).
  A config may not restate it or any other flag the generator supplies (`-o`,
  `--report`, `--new-flow`, the destination flags), because `nc` takes the last
  occurrence of such a flag and the override would be silent. Every axis is stated
  rather than left to `nc`'s derivation, so the suffix and the metrics' colour space
  are read off the matrix, keyed on the container and on (gamut, transfer).
- **Each cell is measured in the space its own resolved recipe reports**, not in
  whatever the preset's name usually implies — the reference build's `legacy` and
  `custom` accept `--output-profile`, and measuring ProPhoto pixels as sRGB yields a
  table where every number is wrong and every number looks reasonable.
- **A cell that fails costs only itself.** A roll that states no film stock loses
  the one column that needs it; a failed render leaves its config without a
  rendition, which the app draws as a visible gap.
- **Re-measuring is keyed to the image's checksum**, not its mtime — a run
  re-renders every cell, so an mtime always looks new while the bytes rarely are.

It needs `../nc-assets` and the venv, so it is not in CI, and it refuses an output
directory inside the repository: the frames are the user's own photographs and are
never committed.

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

## Datasheet digitization — `digitize_datasheets.py`

Not part of `nctool`, and not stdlib-only: it reads the vector characteristic curves in
`docs/datasheets/` and writes `src/film_stock/curves.json`, the intermediate that
`film_stock::curves`'s pinned Rust literals are audited against.

```sh
python3 scripts/analysis/digitize_datasheets.py            # rewrite curves.json
python3 scripts/analysis/digitize_datasheets.py --check    # verify, change nothing
```

It needs poppler (`brew install poppler`) and is run **by hand**, never in CI — the same
split as `pipeline/colorimetry/`: extraction needs a toolchain, while "the literals match
the extraction" is a plain `cargo test`. The file's own module docstring carries the
extraction traps; read it before editing.

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
need `cargo build` to have produced `target/debug/hanten`.
