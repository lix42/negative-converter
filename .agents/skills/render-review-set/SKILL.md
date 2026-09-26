---
name: render-review-set
description: >-
  Build a visual review set — an image set converted through two or more
  configurations, optionally beside another tool's output (Negative Lab Pro,
  SmartConvert, a hand-edited target) — write its review.json, and offer to serve it
  in tools/review-app. Use when asked to compare presets, parameters or builds by
  eye, to "render a review set", "build a comparison", "see how X looks across the
  roll", to judge a default before changing it, or to put an outside reference beside
  Hanten's renders.
---

# Render a review set

Comparing renders by eye goes through [`tools/review-app`](../../../tools/review-app/README.md),
never a one-off HTML page. Every configuration of a frame occupies **one grid cell**, so
switching between them cannot move the picture by a pixel — which is what makes highlight
differences visible at all.

A review set has four inputs. Settle each with the user before rendering anything, because a
wrong one costs a full re-render:

| Input | What it is | Where it comes from |
|---|---|---|
| **Image set** | which frames | `../nc-assets/manifest.json` (`role: "real"`), a named subset, or arbitrary files |
| **Config set** | what to compare | one matrix entry per configuration |
| **References** | another tool's output | `../nc-assets/converted/<producer>/`, converted to sRGB JPEG |
| **Destination** | where it lands | a fresh folder under `../temp/`, never inside the repo |

## Before you start

- `cargo build --release` — the generator shells out to the binary.
- Venv, for measuring only: `uv venv --python 3.12 .venv && uv pip install --python .venv -r
  scripts/analysis/requirements.txt`. The generator is stdlib-only apart from measuring, so a
  set still renders without it — but then run `python3` in place of `.venv/bin/python`, since
  the command in step 4 names an interpreter that will not exist, and pass `--no-metrics`.
  A set built that way has no charts.
- `../nc-assets` must resolve (machine-local symlink).
- **The frames are the user's own photographs**: output goes outside the repo and is never
  committed or published. `nctool review generate` refuses an output directory inside it.

## 1. The image set

`../nc-assets/manifest.json` is the inventory. Keep `role: "real"` frames —
`unexposed` / `leader` / `calibration` are reference frames, not pictures. Frame counts go
stale fast, so derive them at run time rather than trusting any written-down number.

Narrow the set with the matrix's `frames` list, or `--frames a,b,c` on the command line.
Prefer a whole roll when judging a default, a handful when iterating on one parameter.

**Sources outside `../nc-assets/rolls/`** are not addressable by the generator, which builds
each path as `rolls/<roll>/<file>`. For those, run `hanten convert` directly and hand-write
`review.json` — the schema is [`tools/review-app/SCHEMA.md`](../../../tools/review-app/SCHEMA.md)
and a rendition is just a path.

## 2. The film base, per roll

`dmin` is the one fact the manifest does not carry, and every conversion needs it. Measure it
once per roll from that roll's unexposed frame:

```sh
./target/release/hanten estimate ../nc-assets/rolls/<roll>/base.tif --grid --report json
# take film_base_flag / the film_base object
```

`--grid` samples five cells and reports `agreement`. **Never silently reuse a value where that
is false** — say so, and check whether the cause is local (dust, a mark) or a smooth
illumination gradient, which is common on these scans and benign. The reported base is the
per-channel median of the five cells, so a gradient leaves it mid-spread and usable.

## 3. The config set

The matrix is **data**, so comparing something new is a JSON edit, not code. Write copies;
never edit `scripts/preset-review/presets.matrix.json` or
`scripts/analysis/fixtures.json` in place — they describe a curated study and their
roll names predate the asset rename.

Two files, built by a short script kept beside the set. The frame lists come from
`manifest.json`; the `dmin` values do **not** — the manifest does not carry them, so the script
has to merge in the per-roll measurements from step 2. Miss that and `review generate` skips
every cell whose `common_args` reference `{dmin}`.

- **fixtures copy** — `{"rolls": {<roll>: {"dmin": [r,g,b]}}, "frames": {<key>: {"roll",
  "file"}}}`. Nothing else per frame is read. Keys must be filename-safe and unique across
  rolls: `<short-roll-tag>-<serial>` (`g200-1137`, `ektar0909-1605`).
  **`file` is relative to the roll directory, not to the asset root** — the generator builds
  `<assets>/rolls/<roll>/<file>`, so copying the manifest's own `rolls/<roll>/<serial>.tif`
  produces a doubled path and every frame is reported missing and skipped.
- **matrix copy** — `schema_version` (must be `1`; the loader refuses the document
  otherwise), `output_dir`, `output_preset` (or, for `--new-flow`, `destination` — see
  below), `common_args` (usually `["--film-base",
  "{dmin}"]`), `metrics.inset`, a `rolls` block mapping roll → `film_stock` if any config uses
  `{film_stock}`, and one entry per configuration under `configs`.
- **a build axis, when comparing two binaries** — an optional top-level `builds`, one
  `{"id", "label", "nc"}` per binary (`note` optional). Every config is then rendered by
  every build, as cells with the composed id `<config>@<build>` and the label
  `<config> · <build>`; a config may state its own `"builds": ["after"]` to opt out of one,
  which is what to do when a flag only the new binary accepts would otherwise fail on every
  frame. `--nc` is refused beside a `builds` block — repoint one arm with
  `--build <id>=<path>` instead, which is the flag for the rebuild loop. A build may add
  `"expect_commit": "<hex>"`: its binary must then be a clean build of that commit — checked
  on `--version` before anything renders, and on every cell's report — or the run stops. Use
  it on a reference arm, where a wrong path otherwise renders
  and is merely *labelled* with the wrong commit. The pre-migration reference arm is built by
  [`scripts/reference-snapshot/`](../../../scripts/reference-snapshot/README.md).

Rules, each with a reason:

- A config may not restate any flag the generator owns — `--output-preset`, `-o` /
  `--output`, `--report`, `--report-file`. `hanten` takes the **last** occurrence, so an output
  override would be silent, and redirecting the report to a file stops the generator reading
  the resolved recipe back from stdout. State the preset once as `output_preset`. The loader
  rejects all five by name, so a config that restates one fails before anything renders.
- A new-chain set states `destination` instead of `output_preset`: the recipe `output` value
  with **every** axis stated (`{"display": {"range": "sdr", "transfer": "native", "gamut":
  "display-p3", "container": "tiff"}}`) or `"film-master"`. The generator passes `--new-flow`
  and the destination flags itself, so a config may not state them either. A reference-build
  arm (the pre-migration binary) cannot render a `destination` set — it has no `--new-flow`.
- A cell that fails costs only itself; the app draws the gap.
- Re-measuring is keyed to the image's **checksum**, not mtime, because a rerun re-renders
  everything.
- `metrics.inset` should clear the film holder — check it, since a luminance histogram with a
  hard spike at the bottom is measuring the holder, not the picture.

## 4. Render

```sh
PYTHONPATH=scripts/analysis .venv/bin/python -m nctool review generate \
  <matrix copy> --fixtures <fixtures copy> --nc target/release/hanten --out ../temp/<set>
```

`--no-metrics` renders without charts; `--force` re-measures. Budget ~3 s per cell on
17–50 MP frames, so a few hundred cells is tens of minutes — run it in the background.

**With a build axis, keep the config list short.** The cells are the product, and only ten
have a number key: two builds over five configs already spends all of them.

Three things the generator checks for you, none of which needs a flag. **The first two are
not build-axis features** — a matrix with no `builds` is one unnamed build, the `--nc`
binary, and both rules apply to it verbatim:

- Every binary is verified **before anything renders** — it exists, and its `--version`
  really is this project's CLI. That includes a plain `--nc` binary, which used to fail at
  the first render instead. The **pre-rename `nc` banner is accepted**, which matters because
  the reference arm of a before/after is usually the pre-rename reference build, which
  prints it.
- A binary that reports **one identity and then another** aborts the run — a build by name,
  or the `--nc` binary by path. It writes no `review.json`, and it *deletes* an earlier run's
  `review.json` from the output directory **when this run overwrote a cell that file names**
  (otherwise it leaves it and says so). The binary changed underneath, so every cell already
  rendered under it is suspect too — including the ones overwritten in place, which a stale
  set would go on attributing to the previous binary while the app refreshed onto the new
  pixels.
- Two builds that are the **same file** are a note, not a refusal — that is how you check the
  set is honest, since their cells must come out byte-identical. Two *different* files that
  report the same identity (two dirty builds of one commit) are also a note: the labels
  cannot tell them apart, and nothing can. (This one *is* build-axis-only; one binary cannot
  collide with itself.)

**Smoke-test unusual flag combinations on one frame first.** A configuration Hanten refuses
(`--display-tone-headroom 0` on a frame with highlights above white, say) is worth finding in 15 seconds rather than after
a full run.

## 5. References from another tool

`review.json` is producer-agnostic, so an outside image can be a cell —
[`analysis/review-reference-cells`](../../../docs/tasks/analysis/review-reference-cells.md)
would make this first-class; until then it is manual. Three things to get right:

1. **Convert to a common SDR sRGB JPEG.** The app streams bytes and does no colour
   conversion, and these exports are not browser-displayable as shipped. Encodings differ per
   batch — under `../nc-assets/converted/nlp/` both 32-bit float **linear sRGB** and 16-bit
   **Adobe RGB (1998)** exist. Read each file's own ICC profile rather than assuming; apply
   the right decode, a primaries-derived matrix to sRGB where needed, then the sRGB OETF.
   **Verify one file per encoding** against an independent decode (`sips -m` with the
   embedded profile) before converting a batch — a wrong matrix still looks plausible.

   **Converting only the reference is not enough, and no preset fixes it.** With the usual
   `gain-map-hdr` matrix, Hanten's cells are gain-map JPEGs: on an HDR display the browser shows
   the *HDR* rendition while the reference stays plain SDR, so the two cells differ in
   rendering intent before any conversion is compared — and `nctool metrics` meanwhile reads
   Hanten's **SDR base**, so the charts and the picture describe different renditions. Hanten cannot
   be asked for a plain SDR JPEG instead: the only JPEG writers are `gain-map-hdr` and
   `ultra-hdr-v1`, both gain-map carriers, and every SDR preset writes TIFF, which browsers
   will not display. So either strip the gain map from Hanten's JPEGs, or review on an SDR display
   and **say** that is what was done. Making this first-class belongs to
   `analysis/review-reference-cells`.

   **Stripping the gain map fixes the rendering intent, not the gamut.** Hanten's SDR base is
   **Display P3** for both gain-map presets (`metrics.py`'s `PRESET_SPACES`), so a strip that
   drops the ICC profile leaves P3 numbers that a viewer then shows as sRGB — saturation
   errors that look like a conversion difference. Either convert the stripped base to sRGB as
   well, so "a common sRGB pair" is true, or keep its P3 profile and stop calling the pair
   common-sRGB. Whichever you pick, measure each side in the space it is actually in.
2. **Pair by filename identity, never registration.** An export usually carries its source's
   serial (`converted/<producer>/<roll>/<serial>.tif` ↔ `rolls/<roll>/<serial>.tif`). Exports
   are often cropped differently, so never expect pixel alignment. A frame with no match gets
   no key and the app draws a gap — never force a mapping.
   The manifest carries a `source_frame` link meant to be this identity and to survive a
   rename, and it is what `analysis/review-reference-cells` plans to pair on — but **check it
   before relying on it**: on the current manifest it is `null` on all 122 converted entries,
   so pairing on it today yields nothing.
3. **Measure it in the space it is now in, and write the record to disk**:

   ```sh
   PYTHONPATH=scripts/analysis .venv/bin/python -m nctool metrics image <converted>.jpg \
     --space srgb --inset <same as the matrix> \
     --out ../temp/<set>/<frame>-<producer>.metrics.json
   ```

   Run from the repo root: `nctool` is not installed as an executable, and this is the half
   that genuinely needs the venv.

   `srgb`, because the record must describe *that* rendition. `--out`, because the record
   otherwise goes to stdout and there is no file to name in the rendition's `metrics` field —
   so the reference cell silently renders with no charts beside the Hanten cells that have them.

   **The matrix's inset is a fraction, so it does not survive a different crop.** These
   exports are usually cropped differently — the Gold batch is 4897x3265 against a 5184x3600
   source — and the same fractional inset then selects different scene content on each side,
   which `docs/progress/analysis.md` records as the reason those cross-producer numbers are
   not comparable at this precision. Derive a region that covers the *same scene area* on the
   reference, or leave the reference without charts. Do not put a record measured over
   unmatched content beside Hanten's and present the two as comparable; if you keep it anyway,
   mark it in the rendition's note as measuring a different region.

   **The record belongs to the set, not to the reference image.** It is measured over *that
   set's* `metrics.inset`, so a fixed path beside the shared reference is overwritten by the
   next set that uses a different inset — and because every earlier `review.json` still points
   at that same path, its reference charts quietly begin describing another region while its
   Hanten charts describe the original. Nothing detects this. Write the record into the set.

Keep the converted reference **images** in their own folder, shared across sets; their
**measurements** are per-set and live with the set that measured them.

## 6. Assemble `review.json`

The generator writes one review file per run. Merge in anything rendered elsewhere — other
configurations, references — writing every path **relative to the review file**, so sibling
folders are referenced rather than copied. Verify every `src` and `metrics` path resolves
before handing it over; a broken path is a silent gap.

**Declare a `configs` entry for every merged rendition, before adding its paths.** The
generator builds `configs` from the matrix, so it names only the Hanten configurations. A
rendition keyed by an id that is not declared there makes the app refuse the **whole set**
(`… not one of the declared configs`), and quietly reusing an existing id is worse than the
error — it replaces that Hanten rendition rather than sitting beside it, so the cell you wanted
to compare against is the one you lose. Add `{"id": "nlp", "label": "NLP"}` first.

**With a build axis the generator's ids are composed**, `<config>@<build>` — so a merged
rendition sits beside `chr-generic@after`, not beside `chr-generic`, which names no cell in
that set. Key the merged entry against the composed id it belongs with, or give it an id of
its own; a bare config name there declares a cell nothing renders.

The order of `configs` sets the `1`–`9` keys, and only ten cells have one — the same ceiling
step 4 warns about, counted over the **merged** list rather than the matrix's, so a merge is
what spends the last of them.

Split large sets per roll (`review-<roll>.json`) alongside the combined one: a reviewer works
through one roll at a time.

## 7. Ask before serving

**Do not start the dev server unprompted.** Report what was built — frames, configurations,
failures, where it is — then ask whether to start it or just hand over the path:

```sh
cd tools/review-app
corepack enable pnpm   # once per machine
pnpm install           # once per checkout — `pnpm dev` does not install, and `vp` is local
pnpm dev ../../../temp/<set>/review.json
```

Serving costs memory and only one set is visible at a time, so the user may prefer the path,
a specific roll, or a different set first. If you do start it, confirm which port it bound
(another server may hold the usual one) and that it is serving *this* set.

The app **watches the set**: re-rendering updates the page in place, keeping the selected
config and scroll position. Keys: `1`–`9`/`0` select a config, `f` toggles fit/fullsize.
`pnpm` is pinned by `packageManager` — **never run `pnpm self-update`** in the repo.

## Layout

One folder per kind of output, so configurations never mix:

```
../temp/
  README.md          # index of every folder — keep it current
  <set>/             # one rendering configuration set
    review.json  review-<roll>.json
    <frame>-<config>.jpg[.json][.metrics.json]
    scripts/         # the matrix, the fixtures copy, and the scripts that built them
  <producer>-srgb/   # converted outside references, shared by every set
  notes/             # what was observed, frame by frame
```

Keep each set's scripts beside it: a set nobody can rebuild is a set nobody can trust six
weeks later.

## Traps

- **Never publish or commit a review set.** The images are personal photographs.
- **A `--new-flow` cell cannot be generated yet**: the generator passes
  `--output-preset` to every cell and `--new-flow` refuses it (exit 2). Render that side by
  hand until `nf-destinations/preset-set` gives the new flow a destination to name.
- **Nothing checks that a metric record describes the pixels beside it.** Re-render by hand
  and the charts go on describing the previous render; `nctool review generate` re-measures on
  checksum change, which is why it is the way to rebuild a set.
- **A preset's calibration can move, and explicit flags do not freeze a render.** Stating
  every flag pins those *values*, not the omitted defaults and not the algorithm inside the
  `--nc` binary, so the same matrix re-run after a pipeline change can produce different
  pixels. A matrix naming only `--preset` is looser still: it renders what that preset means
  *today*. **A set records which binary made it only when it declares `builds`**: each config
  then carries a `producer` block, derived from what that binary reported rather than typed
  into the matrix, and the app shows it under the picture and in the button's tooltip. A
  matrix with no build axis still carries none — every cell writes its own `<image>.json`
  sidecar beside it, which is where the commit is in that case, but `review.json` does not
  name it. So for a set meant to be trusted months later, either give it a `builds` block or
  write the commit and `hanten --version` into the set's own `scripts/` folder yourself.
- **Deleting source frames breaks later reruns, not the existing set.** Rendered JPEGs and
  their records survive; the generator simply skips the missing sources.
