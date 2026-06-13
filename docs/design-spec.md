# Negative Converter — High-Level Design Spec (Step 1)

> Status: Draft for review · Target: Step 1 (MVP) · Language: Rust
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

| Variant | Channels | Bit depth | Notes |
|---|---|---|---|
| HDR   | R, G, B            | 48-bit (16/ch) | No infrared. |
| HDRi  | R, G, B, IR        | 64-bit (16/ch) | 4th channel = infrared defect data. |

The tool reads both. On HDRi input the IR channel is parsed and kept; on HDR
input there simply is no IR channel.

**Caveat (carried from research):** there is no published low-level spec for the
exact SilverFast HDRi tag/IFD layout. Implementation must be validated against
real sample files, and the reader should degrade gracefully (treat unknown extra
channels conservatively, log what it found via the JSON report).

### Internal representation

After decode, the image is normalized to **linear `f32` scanner RGB** in `[0,1]`
(plus an optional `f32` IR plane). This is the single input contract every
algorithm consumes — nothing downstream needs to know the on-disk format.

## 5. Output formats

- **Container:** TIFF (BigTIFF when size requires 64-bit offsets).
- **Bit depth (flag-controlled):**
  - `--out-depth u16` → 16-bit integer TIFF (default; standard archival positive).
  - `--out-depth f32` → 32-bit float TIFF (full HDR, no precision loss).
- **Color (selectable, depth-aware default):** the output color space is a CLI
  option (`--output-profile`). The default depends on output depth:
  - `u16` output → **sRGB** (standard, display-ready positive).
  - `f32` output → a **wide-gamut** space (e.g. ProPhoto / linear ACEScg) to
    avoid clipping the extended range of HDR data.
  Either default can be overridden explicitly. Output is tagged with the embedded
  ICC profile for the chosen space.
- **Metadata:** the effective parameter set (recipe) and key estimated values are
  written to a sidecar JSON next to the output, and core provenance is embedded in
  the TIFF where practical.

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
1. transmission → density:   D = -log10(scan / Dmin_transmission)   (per channel)
2. density correction:       per-channel scale/offset, orange-mask compensation
3. map density → positive:   exponential / print-curve back-transform
4. print render controls:    exposure, black point, gamma, highlight compression
```

Density conversion (steps 1–2) and print rendering (steps 3–4) are kept as
separate, independently parameterized sub-stages — the core fidelity rule from §3.

### Pluggable interface (sketch)

```rust
/// A negative→positive conversion algorithm.
/// Pure: no I/O, no hidden state.
pub trait Converter {
    type Params;
    fn convert(&self, img: &LinearImage, base: &FilmBase, p: &Self::Params)
        -> Result<LinearImage, ConvertError>;
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

### Reports & determinism

- `--report json` — emit a machine-readable result (estimated values, clip
  warnings, timings, output path) to stdout or `--report-file`.
- `--seed <n>` — fix any stochastic step (none in Step 1, reserved).
- Stable, documented **exit codes** (see §11).

### Example invocations

```bash
# Default density conversion, 16-bit TIFF out, auto Dmin, JSON report.
nc convert in.tiff -o out.tiff --algorithm density --report json

# Full HDR float output, manual film base, explicit print controls.
nc convert in.tiff -o out.tiff \
  --algorithm density --out-depth f32 \
  --film-base 0.92,0.55,0.42 \
  --density-gamma 1.8 --print-exposure 0.0 --black-point 0.002 \
  --highlight-compress 0.3 --output-profile sRGB

# Reuse a roll recipe but override one knob for this frame.
nc convert frame12.tiff -o frame12_pos.tiff \
  --params roll-A.json --print-exposure 0.15

# Inspect only; let an agent read the JSON and decide parameters.
nc inspect in.tiff --report json
```

## 9. Parameter reference (grouped by stage)

All flags are also valid keys in the JSON recipe. Names are indicative.

### Input / decode
- `--export-ir <path>` — write the IR plane to a separate file (HDRi only).
- `--assume-linear` / `--input-profile <icc>` — input color handling.

### Film base / Dmin (stage 2)
- `--film-base R,G,B` — explicit base transmission per channel (overrides auto).
- `--base-region x,y,w,h` — region of unexposed border to sample.
- `--auto-base` (default) — estimate base from detected border.

### Algorithm select
- `--algorithm simple|density`

### Density stage (algorithm = density)
- `--density-scale R,G,B` — per-channel density gain.
- `--density-offset R,G,B` — per-channel density offset (orange-mask comp).
- `--density-gamma <f>` — film/print curve gamma.

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
- `--out-depth u16|f32` (default `u16`)
- `--output-profile <icc|sRGB|prophoto|acescg|...>` (default is depth-aware:
  `sRGB` for `u16`, wide-gamut for `f32`)
- `--bigtiff auto|on|off` (default `auto`)

### Global
- `--params <json>`, `--dump-params <json>`
- `--report json|none`, `--report-file <path>`
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

These are deliberately deferred and recorded here so they aren't lost.

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

## 13. Open questions

- Exact on-disk SilverFast HDRi tag/channel layout — must be confirmed against
  real sample files (user will provide); the decoder should log what it finds.
- Which specific wide-gamut space to default to for `f32` HDR output (ProPhoto
  RGB vs linear ACEScg vs Rec.2020) — the *option* is decided; the default value
  is still open.
- Whether the embedded TIFF metadata should carry the full recipe or just a
  pointer to the sidecar JSON.
