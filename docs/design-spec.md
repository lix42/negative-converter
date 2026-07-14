# Negative Converter — High-Level Design Spec (Step 1)

> Target: Step 1 (MVP) · Language: Rust
>
> This document is the machine-readable (Markdown) companion to `design-spec.html`.
> Both contain the same content; the HTML version is for humans, this one is for agents.

## 1. Purpose

A command-line tool that reads a **film negative scan** (SilverFast HDR/HDRi
format first) and produces a **positive image** as a TIFF file. Every step of the
conversion is controlled by explicit CLI parameters so that an automated agent —
or a human — can drive the full pipeline reproducibly.

### What "AI-friendly" means here

This was the key clarification that reshaped the design. "AI-friendly" does **not**
mean "use AI/ML models to process the image" (auto-crop, generative restoration,
etc.). It means:

- **Every parameter of the conversion is exposed as a CLI flag.** A negative
  converter naturally has many knobs (film-base estimation, density, white balance,
  tone, gamma, color management, output bit depth). All of them are addressable
  from the command line.
- **The tool is deterministic and scriptable.** The same inputs and parameters
  always produce the same output. No hidden state, no interactive prompts in the
  conversion path.
- **Machine-readable I/O.** Parameters can be loaded from / dumped to a JSON
  "recipe" file, and the tool can emit JSON reports (estimated values, warnings,
  metadata) so an agent can read results and adjust on the next call.

The deterministic core owns the image science. Any future ML assistance (see
§12 Roadmap) is strictly opt-in and sits *around* this core, never replacing it.

## 2. Scope

### In scope (Step 1)

- Read SilverFast **HDR (48-bit RGB)** and **HDRi (64-bit RGB + infrared)** scans.
- Parse and **preserve** the IR channel (carry it through the pipeline; optional
  export). Do **not** yet act on it. See §6.1 and §12.
- Convert negative → positive entirely in a **32-bit float linear working space**.
- **Pluggable algorithm** architecture, shipping **two** algorithms:
  1. `simple` — channel inversion + white balance (baseline / debug / B&W).
  2. `density` — density-domain inversion (Kodak Cineon / darktable `negadoctor`
     style) — the real default for color negatives.
- All conversion parameters controllable via CLI flags and/or a JSON recipe file.
- Write **TIFF** output, selectable as **16-bit integer** or **32-bit float**
  (HDR) via a flag.
- Auto-estimate film base (`Dmin`) from the unexposed border, with full CLI override.
- JSON report output (estimated parameters, warnings) and JSON recipe load/dump.

### Out of scope (Step 1) — see §12 Roadmap

- IR-based dust/scratch removal (follow-up task).
- Additional algorithms beyond the two above, e.g. sigmoid / explicit H&D curve
  (follow-up task).
- Black & white film support, incl. plain 16-bit RAW scans (follow-up task).
- Camera RAW (Bayer/X-Trans) input, DNG processing.
- ML/AI assistance of any kind (auto-crop, neutral-patch detection, inpainting).
- Batch/roll preset management UI, GUI, scanner ICC profiling workflow.
- Output formats other than TIFF (JPEG/PNG/EXR can come later).

## 3. Design principles

1. **Separate capture from rendering.** The scan is an archival record of
   transmitted light, not just an image to invert. The pipeline keeps a clean
   linear capture representation separate from the positive-rendering stage.
2. **Density conversion and print rendering are separate stages.** This is the
   single most important architectural rule for color fidelity.
3. **Float-first, lossless internal pipeline.** All math is `f32` in a linear
   working space. Bit-depth reduction happens only at the final encode step.
4. **Deterministic and reproducible.** Same inputs + same params ⇒ identical output.
5. **Every knob is a flag.** No conversion behavior is reachable only through code.
6. **Pure functions over classes.** Each pipeline stage is a pure function
   `(input, params) -> output`. The CLI layer is the only orchestrator. (Aligns
   with the project's Rust style guidance.)
7. **Fail loudly, never silently.** Bad input, clipped data, or impossible
   parameters produce explicit errors/warnings with non-zero exit codes — never
   a quietly wrong image.

## 4. Input formats

### Step 1: SilverFast HDR / HDRi (TIFF-family)

SilverFast HDR/HDRi files are TIFF-family containers holding high-bit-depth,
linear (raw-ish) scanner data:

| Variant | Channels | Bit depth | On-disk layout |
|---|---|---|---|
| HDR   | R, G, B            | 48-bit (16/ch) | Single IFD: 3-sample chunky RGB, no IR. |
| HDRi  | R, G, B + IR       | 64-bit (16/ch) | IFD0 = 3-sample RGB (as HDR); a 1-sample grayscale IR plane in a later IFD. High-res scans also embed a reduced-resolution RGB preview IFD between them. |

The tool reads both. On HDRi input the IR plane is parsed and kept; on HDR input
there simply is no IR channel.

**On-disk layout (verified against real sample files, 2026-06):** these are
uncompressed little-endian ClassicTIFFs, `PlanarConfiguration=1` (chunky), 16-bit
**unsigned** samples (no `SampleFormat` tag). The IR channel is **not** a 4th
sample interleaved into the RGB pixels — HDRi files carry it as a **separate IFD**
(`NewSubfileType=4`, `Photometric=BlackIsZero`, `SamplesPerPixel=1`,
`BitsPerSample=16`) at the same dimensions as IFD0. High-resolution scans also
embed a **reduced-resolution RGB preview** IFD (`NewSubfileType` bit 0) between the
RGB image and the IR plane, so the IR plane is not always the second IFD; the
decoder skips previews (by their reduced dimensions) and locates the IR plane by
its full-resolution grayscale shape. So it distinguishes HDR from HDRi
**structurally** — by the presence of that IR image — not from metadata: the
`Silverfast:HDRScan="Yes"` XMP flag is present on *both* variants and cannot be
used to detect IR.

**Caveat (carried from research):** there is still no published low-level spec for
the SilverFast layout; the above is reverse-engineered from sample scans. The
reader degrades gracefully — recognized-but-unhandled layouts return an
`Unsupported` error, and what was found is logged via the JSON report.

### Internal representation

After decode, the image is normalized to **linear `f32` scanner RGB** in `[0,1]`
(plus an optional `f32` IR plane). This is the single input contract every
algorithm consumes — nothing downstream needs to know the on-disk format.

## 5. Output formats

- **Container:** TIFF (BigTIFF when size requires 64-bit offsets).
- **Bit depth (flag-controlled):**
  - default (no `--output-hdr`) → 16-bit integer TIFF (standard archival positive).
  - `--output-hdr` → 32-bit float TIFF (full HDR, no precision loss).
- **Color (selectable, depth-aware default):** the output color space is a CLI
  option (`--output-profile`). The default depends on output depth:
  - 16-bit (default) output → **sRGB** (standard, display-ready positive).
  - HDR (`--output-hdr`) output → **linear ACEScg** (wide-gamut), avoiding
    clipping of the extended range of HDR data. (`prophoto` and user ICC files
    are also accepted.)
  Either default can be overridden explicitly. Output is tagged with the embedded
  ICC profile for the chosen space.
- **Metadata:** the effective parameter set (recipe) and key estimated values are
  written to a **sidecar JSON** next to the output (paired by name; the same shape
  as `--dump-params`). The TIFF itself embeds the ICC profile of the chosen output
  space; the recipe is deliberately *not* embedded in the TIFF (resolved, §13).

## 6. Pipeline architecture

The conversion is a linear sequence of pure-function stages. Each stage has its
own parameter struct and can be unit-tested in isolation.

```
                 ┌──────────────────────────────────────────────┐
  input file ──▶ │ 1. Decode  (SilverFast HDR/HDRi → f32 RGB[+IR])│
                 └──────────────────────────────────────────────┘
                                     │ linear scanner RGB (f32), IR (f32, opt)
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 2. Film-base / Dmin estimate (auto or CLI)    │
                 └──────────────────────────────────────────────┘
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 3. Algorithm: negative → positive             │
                 │    (simple | density)  — pluggable            │
                 │    sub-stages: density convert, correct,      │
                 │    invert, tone/print render                  │
                 └──────────────────────────────────────────────┘
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 4. Output color transform (working → output)  │
                 └──────────────────────────────────────────────┘
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 5. Encode  (f32 → u16/f32 TIFF + ICC + sidecar)│
                 └──────────────────────────────────────────────┘
                                     ▼  output.tiff (+ output.json)
```

### 6.1 IR channel handling (Step 1)

The IR plane (when present) is decoded and carried alongside RGB but is **not
consumed** by any conversion stage in Step 1. It can be exported with
`--export-ir <path>` for inspection or downstream tooling. The dust-removal stage
that *consumes* the IR mask is a deliberate follow-up (§12).

*Why IR is powerful and why we defer it:* the color dye image is transparent to
infrared while physical defects (dust, scratches, hair) are opaque to it, so the
IR channel is a near-clean defect map. Acting on it requires a separate
mask + inpainting stage with its own parameters, and it does **not** work for
traditional silver B&W film (silver blocks IR like dust) or reliably for
Kodachrome. So Step 1 preserves the data cheaply now and adds the consuming stage
later.

## 7. Conversion algorithms

Algorithms implement a common trait so new ones can be added without touching the
rest of the pipeline. Selected with `--algorithm <name>`; algorithm-specific
flags are namespaced.

### 7.1 `simple` — inversion baseline

Channel inversion plus white balance / border neutralization. Cheap, predictable,
useful for B&W negatives and as a debugging reference. Not a strong endpoint for
color negatives (ignores density behavior and the orange mask).

Stages: `positive = invert(linear_rgb)` → white-balance gain per channel →
optional black/white point.

### 7.2 `density` — density-domain inversion (default)

The credible baseline for color negatives, following Kodak Cineon / darktable
`negadoctor` ideas:

```
1. transmission → density:   D  = -log10(scan / Dmin_transmission)   (per channel)
2. density correction:       D' = per-channel scale·D + offset (orange-mask comp)
3. map density → positive:   lin = 10^(gamma · (D' − Dmax))          (per channel)
4. print render controls:    exposure, black point, white balance, highlight compression
```

**Polarity.** With `D = -log10(scan / Dmin)` the density is `≥ 0` and *grows* with
the film's optical density — the unexposed base (scene black) sits at `D = 0`, a
dense negative area (a scene highlight) at large `D`. A positive must brighten as
`D` grows, so step 3 uses `10^(+gamma·D')`, **not** `10^(−gamma·D')` (which would
reproduce the negative). This matches darktable `negadoctor` (denser negative →
brighter print).

**Display-white anchor (`Dmax`).** Step 3 renders density *relative to* `Dmax`, the
corrected density of scene white: scene white (`D' = Dmax`) maps to display white
`1.0`, and the base (`D' = 0`) to `10^(−gamma·Dmax) ≈ 0`. Without it the base maps
to `1.0` and all real detail sits above `1.0`, so a default u16 encode clips the
whole image; the anchor makes the default output fill the display range. Mathematically the
anchor factors out as a constant gain `10^(−gamma·Dmax)`, but it is applied **in
the exponent** — `10^(gamma·(D' − Dmax))` — so an extreme `gamma·D'` cannot
overflow `f32` before the anchor cancels it. `Dmax` is **frame-local** — a property of the
scene's own white, unlike the roll-level `Dmin` base — so it is measured per frame
by default (`density.dmax = auto`); it can be fixed (`{ "explicit": <d> }`) or
disabled (`"none"`, for bit-exact scene-referred HDR output). See §9.

Density conversion (steps 1–2) and print rendering (steps 3–4) are kept as
separate, independently parameterized sub-stages — the core fidelity rule from §3.

### Pluggable interface (sketch)

```rust
/// A negative→positive conversion algorithm.
/// Pure: no I/O, no hidden state. Parameters live in the implementing struct
/// (e.g. `Density { density, print }`), keeping the trait object-safe; a
/// factory maps `--algorithm` + the resolved params to a boxed converter.
pub trait Converter {
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage>;

    /// Optional per-conversion diagnostics for the JSON report (e.g. the
    /// resolved `Dmax`). Defaulted to `convert` + an empty `ConvertReport`,
    /// so algorithms with nothing to report only implement `convert`.
    fn convert_reported(&self, image: &LinearImage, base: &FilmBase)
        -> Result<(LinearImage, ConvertReport)> { /* default provided */ }
}
```

## 8. CLI design

A single binary (working name `nc`) with subcommands. The agent-facing surface is
optimized for scripting: flags for everything, JSON in/out, stable exit codes,
no interactive prompts.

### Subcommands

| Command | Purpose |
|---|---|
| `nc convert` | The main pipeline: negative file → positive TIFF. |
| `nc inspect` | Read a scan and emit a JSON report of format, channels, bit depth, detected film border, suggested `Dmin`. No output image. |
| `nc estimate` | Run only film-base/`Dmin` estimation; emit JSON. |
| `nc params`  | Print the full default/effective parameter set as JSON (for discovery and recipe scaffolding). |

### Recipes (JSON in/out)

- `--params recipe.json` — load a full parameter set from JSON.
- `--dump-params out.json` — write the effective parameters (defaults + overrides)
  to JSON. Individual `--flag` overrides take precedence over the loaded recipe,
  so an agent can load a roll recipe and tweak one value per frame.

The recipe JSON is **grouped into per-stage objects** (`input`, `film_base`,
`density`, `print`, `simple`, `output`, plus the top-level `algorithm`) rather
than one flat bag of keys. The grouping lets the tool **reject unknown/typo'd
keys at every level** (a misspelled knob is a hard error, not a silently-ignored
default → a quietly wrong image). A recipe may be **partial** — any omitted key or
section falls back to its default. `nc params` prints this exact shape fully
populated with defaults, so it doubles as a recipe template; `--dump-params`
writes the same shape with the resolved values. Example:

```json
{
  "algorithm": "density",
  "density": { "density_gamma": 1.8 },
  "print":   { "print_exposure": 0.0, "black_point": 0.002 },
  "output":  { "hdr": true }
}
```

### Reports & determinism

- `--report json` — emit a machine-readable result (estimated values, clip
  warnings, timings, output path) to stdout or `--report-file`.
- `--seed <n>` — fix any stochastic step (none in Step 1, reserved).
- Stable, documented **exit codes** (see §11).

### Example invocations

```bash
# Default density conversion, 16-bit TIFF out, auto Dmin & Dmax, JSON report.
nc convert in.tiff -o out.tiff --algorithm density --report json

# Full scene-referred HDR float output: --no-d-max disables the display-white
# anchor (base → 1.0, detail above), and the depth-aware default profile
# (acescg for HDR) applies. Manual film base, explicit print controls.
nc convert in.tiff -o out.tiff \
  --algorithm density --output-hdr --no-d-max \
  --film-base 0.92,0.55,0.42 \
  --density-gamma 1.8 --print-exposure 0.0 --black-point 0.002 \
  --highlight-compress 0.3

# Reuse a roll recipe but override one knob for this frame.
nc convert frame12.tiff -o frame12_pos.tiff \
  --params roll-A.json --print-exposure 0.15

# Inspect only; let an agent read the JSON and decide parameters.
nc inspect in.tiff --report json

# Calibrate once from an unexposed reference frame, then reuse for the roll.
# (Product tip: wind past the light-struck leader, shoot a lens-cap frame, and
# scan it — a full frame of clean base beats sampling the thin rebate. Don't use
# the auto-burned wind-on frames; they are fogged leader. See §9 film-base.)
# `estimate` measures Dmin from the sampled rectangle and reports it in a form
# ready to drop into --film-base or a recipe's film_base.source.
nc estimate reference.tiff --base-region 200,0,300,3600 --report json
# → { "film_base": [0.553, 0.271, 0.159], ... }
nc convert frame01.tiff -o frame01_pos.tiff --film-base 0.553,0.271,0.159
# …or bake the value into roll-A.json (film_base.source = explicit) and batch it.
```

## 9. Parameter reference (grouped by stage)

Every flag is also a recipe key, nested under the stage object shown in each
heading below (e.g. `--density-gamma` ⇒ `density.density_gamma`, `--output-hdr` ⇒
`output.hdr`, `--algorithm` ⇒ top-level `algorithm`). Names are binding —
the recipe structs and this section are kept in sync (`deny_unknown_fields`).
Unknown keys are rejected (see §8).

### Input / decode
- `--export-ir <path>` — write the IR plane to a separate file (HDRi only).
  Recipe key `input.export_ir`.
- Input color handling is a single mutually-exclusive choice, recipe key
  `input.color` (default `"auto"` — the file's embedded / default profile):
  - `--assume-linear` ⇒ `input.color = "linear"` — data is already linear.
  - `--input-profile <icc>` ⇒ `input.color = { "profile": "<icc>" }` —
    accepted in the recipe shape but **not yet applied**: `convert` rejects it
    loudly (exit 4) until input-side color management lands; scans are decoded
    as linear.
  Passing both is a usage error; either flag replaces a recipe's `input.color`.

### Film base / Dmin (stage 2)
The base source is a single mutually-exclusive choice, recipe key
`film_base.source` (default `"auto"`). The three flags conflict (passing more
than one is a usage error); whichever is given replaces a recipe's source:
- `--film-base R,G,B` ⇒ `{ "explicit": [r, g, b] }` — explicit base transmission.
- `--base-region x,y,w,h` ⇒ `{ "region": [x, y, w, h] }` — sample this rectangle.
- `--auto-base` (default) ⇒ `"auto"` — best-effort estimate from the film border.

**How to obtain `Dmin` — the acquisition ladder.** `Dmin` is a property of the
*film stock + development + scanner settings*, not of an individual frame, so
measure it **once per roll** and reuse it (recipe / `--film-base`) rather than
re-detecting per frame — measured this way the base is identical across frames,
keeping the roll color-consistent. The sources, in decreasing reliability:

1. **A dedicated unexposed frame (best).** Recommended shooting workflow: after
   loading a roll and winding past the light-struck leader (the frame counter
   reaching 1), take a deliberate exposure with the lens cap on, then scan that
   blank frame alongside the roll. Do **not** rely on the 1–2 auto-burned
   wind-on frames — that leader area was exposed while loading with the back
   open, so it is fogged film, denser than clean base, and would bake a wrong
   `Dmin` into the whole roll. A true cap-on frame provides a full frame of
   clean base — far more area than the rebate
   — measured with `nc estimate` and frozen into the roll recipe (§8 example).
   The large area also enables multi-region sampling with an agreement check
   (roadmap §12), which doubles as a light-leak / illumination-falloff
   diagnostic.
2. **The rebate (the unexposed strip around each frame).** Reliable form: point
   `--base-region` at a visible rebate patch manually (read the coordinates from
   any image viewer; UI-assisted picking is a roadmap item, §12). Convenience
   form: `--auto-base` (the default) — real scans are laid out as
   `dark film holder → thin unexposed rebate → exposed picture`, the rebate being
   a narrow, uniform, bright band *inset behind the holder*, possibly on only
   some edges. Auto detection keeps **deliberately strict** confidence gates
   (uniformity, brighter-than-interior) and **fails loudly** rather than emit a
   silently-wrong base — note the Step-1 heuristic often refuses on real
   `holder → rebate → picture` layouts. The robust inward-scan detector,
   thresholds tuned against real scans (`real-scan-verification`), and a
   `--holder white|black` control for light holders are roadmap items (§12).
3. **Content-based estimation (last resort, opt-in).** When the scan is cropped
   to the image with no unexposed film visible, a per-channel high percentile of
   the *exposed content* approximates the base (the thinnest area of a negative
   is the scene's deepest black, close to true base). This is an **explicit
   opt-in source** (roadmap §12): it changes the assumption from "physical base
   measured" to "scene contains a near-black", so the tool never falls back to
   it silently, and the report records that the base came from content
   statistics. When the assumption fails (foggy/high-key scenes), blacks wash out
   and pick up a cast.

**When every source is missing** (no explicit base, auto refuses, content mode
not requested), `convert` **fails loudly** with an actionable message naming the
recovery flags — an agent can catch the exit code and re-run with an explicit
choice. Estimator selection is never silent. A neutral base `[1,1,1]` is
representable but not recommended: it forfeits the per-channel orange-mask
neutralization (content estimation strictly dominates it). Note the failure
geometry is forgiving: because `D = -log10(scan/base)`, a base error is a
*constant per-channel density offset* — a global cast/exposure error correctable
downstream (`density_offset`, white balance) — never a shadow/highlight
crossover.

### Algorithm select
- `--algorithm simple|density`

### Density stage (algorithm = density)
- `--density-scale R,G,B` — per-channel density gain.
- `--density-offset R,G,B` — per-channel density offset (orange-mask comp).
- `--density-gamma <f>` — film/print curve gamma.
- Display-white anchor (`Dmax`) — a single mutually-exclusive choice, recipe key
  `density.dmax` (default `"auto"`; see §7.2). The three flags conflict (passing
  more than one is a usage error); whichever is given replaces a recipe's `dmax`:
  - `--auto-d-max` (default) ⇒ `"auto"` — measure the anchor per frame from the
    corrected-density distribution (a high percentile).
  - `--d-max <d>` ⇒ `{ "explicit": <d> }` — fix the anchor to a scalar density.
    Reusing one frame's measured value across a roll is a deliberate
    fixed-print-exposure look; the tradeoff is that darker frames render dim and
    denser highlights clip against the foreign anchor (it is **not** a
    calibrate-once property like `Dmin`).
  - `--no-d-max` ⇒ `"none"` — disable the anchor; scene-referred output (base →
    `1.0`, detail above), reproducing the pre-anchor render bit-for-bit for HDR
    (`--output-hdr`) workflows.

### Print / tone render
- `--print-exposure <f>` — overall positive exposure.
- `--black-point <f>` — paper black / shadow floor.
- `--white-balance R,G,B` — highlight/neutral white balance gains.
- `--highlight-compress <f>` — highlight roll-off amount.

### Simple algorithm
- `--invert-white-balance R,G,B`
- `--clip-low <f>` / `--clip-high <f>`

### Output / encode (stage 5)
- `-o, --output <path>` (required)
- `--output-hdr` — write a 32-bit float TIFF (full HDR, no precision loss);
  without the flag the output is 16-bit integer. Recipe key `output.hdr`
  (bool, default `false`).
- `--output-profile <srgb|prophoto|acescg|path-to-icc>` (default is depth-aware:
  `srgb` for the 16-bit default, `acescg` for `--output-hdr`)
- `--bigtiff auto|on|off` (default `auto`)

### Global
- `--params <json>`, `--dump-params <json>`
- `--report json|none`, `--report-file <path>`
- `--strict` — promote report warnings (clipping, non-finite samples, …) to a
  failing exit (see §11)
- `-v/--verbose`, `--quiet`

## 10. Code architecture (Rust)

Pure functions per stage; the CLI is the only orchestrator. Suggested layout:

```
nc/
├── Cargo.toml
└── src/
    ├── main.rs           # CLI parsing (clap) → orchestration only
    ├── cli.rs            # arg structs, recipe load/merge, report emit
    ├── io/
    │   ├── decode.rs     # SilverFast HDR/HDRi (TIFF) → LinearImage(+IR)
    │   └── encode.rs     # LinearImage → u16/f32 TIFF + ICC + sidecar
    ├── pipeline/
    │   ├── film_base.rs  # Dmin estimation (pure)
    │   ├── color.rs      # working/output color transforms (lcms2)
    │   └── stages.rs     # stage wiring as pure functions
    ├── algo/
    │   ├── mod.rs        # Converter trait
    │   ├── simple.rs     # baseline inversion
    │   └── density.rs    # density-domain inversion
    └── types.rs          # LinearImage, FilmBase, params, errors
```

### Candidate crates

| Concern | Crate(s) |
|---|---|
| CLI parsing | `clap` |
| TIFF decode/encode | `tiff` (custom handling for scanner extras) |
| Image ops / buffers | `image` |
| Color spaces (linear vs encoded) | `palette` |
| ICC color management | `lcms2` (rust-lcms2) |
| EXIF/metadata | `kamadak-exif` (read), `rexiv2` if richer writing needed |
| Recipe / report JSON | `serde`, `serde_json` |
| Parallelism | `rayon` |

## 11. Error handling & exit codes

| Code | Meaning |
|---|---|
| 0 | Success. |
| 1 | Generic / unexpected error. |
| 2 | Invalid CLI usage or parameters. |
| 3 | Input read/decode error (unreadable or unsupported file). |
| 4 | Unsupported variant (e.g. channel layout we can't handle yet). |
| 5 | Output write error. |

Warnings (e.g. clipped highlights/shadows, IR present but ignored, BigTIFF
auto-promoted) are surfaced in the JSON report and on stderr, without failing the
run unless `--strict` is set.

## 12. Roadmap (follow-up tasks, explicitly out of Step 1)

These are deliberately deferred and recorded here so they aren't lost. Items
graduate into tracked tasks in [TASKS.md](TASKS.md) — several already have
(item 2's sigmoid → `algo-sigmoid`; plus `dmax-white-anchor`, `auto-neutral-wb`,
and `regional-color-balance` from the NLP feature comparison, Phase 6).

1. **IR-based dust & scratch removal.** Consume the IR channel (already preserved
   in Step 1) to build a defect mask and inpaint defects. Parameters: IR
   threshold, mask dilation/morphology, inpainting method/strength. Must handle
   the known limits — disable/guard for silver B&W film and Kodachrome. New
   stages: `defect_mask`, `inpaint`. New flags under an `--ir-*` namespace.
2. **Additional algorithms.** At minimum a **sigmoid / explicit H&D-curve** model
   (NegPy-style) for more photographically meaningful controls; possibly a
   power-law/exponent model (RawTherapee-style) for camera-scanned negatives.
   Added via the existing `Converter` trait, selectable with `--algorithm`.
3. **Black & white film support.** B&W negatives, including plain **16-bit RAW**
   scan files (not the SilverFast HDR/HDRi container). Note these typically have
   no usable orange mask and no IR defect channel (silver blocks IR), so they
   lean on the `simple` algorithm or a B&W-tuned density path rather than the
   color orange-mask compensation.
4. **Camera RAW input.** Bayer/X-Trans and DNG ingestion (e.g. `rawler`/LibRaw)
   to support camera-scanning workflows.
5. **More output formats.** JPEG/PNG for proofs, EXR for HDR interchange.
6. **Roll-level presets & batch mode.** First-class roll recipes (film stock,
   `Dmin`, curve params, neutral spots) applied across many frames.
7. **Scanner ICC profiling workflow & QA harness.** IT8/target-based calibration
   and ΔE2000 / SSIM regression testing against standard test negatives.
8. **Robust auto film-base detection.** Replace the Step-1 margin heuristic with an
   inward-scan detector for the real `holder → thin rebate → picture` layout:
   march strips in from each edge and pick the brightest uniform band past the
   holder (the rebate can be thin and on only some edges). Keep deterministic;
   still fail loudly when no confident band exists, with thresholds tuned
   against the real-scan verification results. This task family also includes: an explicit
   opt-in **content-based source** (`film_base.source = "content"`, §9 ladder
   tier 3) recorded in the report; a **uniformity warning on `--base-region`**
   (a mixed rebate/image rectangle currently yields a plausible-looking bad
   base silently); and `nc inspect` reporting **candidate rebate regions**
   (coordinates + confidence) so CLI users confirm instead of measuring — the
   same data a future UI would highlight.
9. **Light film holders.** Auto/border logic assumes a dark holder surround; some
   holders are white. Add a `--holder white|black` control (recipe key
   `film_base.holder`) so detection knows the surround polarity.
10. **Reuse-ready `nc estimate` output.** Emit the measured base in a directly
    reusable form — a `--film-base R,G,B` string and/or a `film_base` recipe
    fragment — so the calibrate-once → reuse workflow (§8) is copy-paste smooth.
    This includes **grid / multi-region sampling with an agreement check** for
    unexposed-frame calibration (§9 ladder tier 1): sample center + corners,
    require per-channel agreement within a tolerance, and report the spread —
    disagreement diagnoses light leaks and scanner illumination falloff.
11. **UI-assisted film-base picking.** Once a UI layer exists: visual region
    picking for the rebate/reference frame, highlighting auto-detected
    candidates, and feedback when a chosen region fails the uniformity check
    (the CLI-side uniformity warning and inspect candidates above are the
    building blocks).
12. **Crash reporting & opt-in telemetry.** Local first: a panic hook writing a
    crash file (version, backtrace, params shape — never pixels or file paths)
    the user can attach to a bug report. Remote collection (error reporting,
    usage analytics) only later and strictly **opt-in**: documented event list,
    an `NC_TELEMETRY=0`-style off switch, no stdout pollution, no blocking, no
    effect on exit codes or output bytes. Performance instrumentation is *not*
    deferred with this — per-stage timings/tracing are the tracked pre-release
    `perf-instrumentation` task.

## 13. Open questions

All of the Step-1 open questions have since been resolved (kept here as a record):

- ~~Exact on-disk SilverFast HDRi tag/channel layout~~ — **resolved 2026-06**:
  reverse-engineered and verified against real sample files; documented in §4
  (separate full-resolution grayscale IR IFD, optional preview IFD, structural
  HDR/HDRi detection).
- ~~Which wide-gamut space to default to for `f32` HDR output~~ — **resolved**:
  **linear ACEScg** is the `f32` default (`prophoto` and user ICC remain
  selectable); see §5.
- ~~Whether the embedded TIFF metadata should carry the full recipe~~ —
  **resolved**: the recipe lives in the sidecar JSON only (paired by name with
  the output); the TIFF embeds just the ICC profile. See §5.
