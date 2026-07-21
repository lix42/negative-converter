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
- Additional algorithms beyond the two above (follow-up tasks; the sigmoid /
  explicit H&D curve has since shipped post-MVP as `--algorithm sigmoid`, §7.3).
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
   `(input, params) -> output`, deterministic in its image output and free of
   filesystem access. The CLI layer is the only orchestrator. (Aligns with the
   project's Rust style guidance.) *One narrow exception:* the `render` stage
   reads a monotonic wall clock to fill the telemetry record's per-stage
   timings — a report-only channel that leaves the pixels deterministic and
   untouched by the measurement.
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

### Terminology & value domains

**Read this before using "high", "low", "bright", or "dark" anywhere in the code
or docs.** A pixel passes through several value *spaces* between scan and output,
and one runs **backwards** relative to the others, so an unqualified "high value"
is ambiguous. Everything below is **per channel** (RGB) — each pixel carries three
values in every space; the lone exception is `Dmax` (a scalar, below). The IR
plane is a separate single channel, carried but not consumed (§6.1).

| Space | Meaning | "Higher" means | Range (`f32`) | Where in code |
|---|---|---|---|---|
| **transmission** (raw scan value) | fraction of light the film passes | more transparent film, thinner negative, brighter pixel *in the raw scan* — a **darker** scene | `[0, 1]` (= `u16`/65535) | `io::decode`, `LinearImage.rgb` |
| **film base / `Dmin`** | the unexposed rebate's transmission — the per-channel *relative* maximum transmission | (the ceiling of transmission) | `(0, 1]` | `FilmBase`, `film_base::estimate` |
| **density `D` / `B` / `D′`** | `D = −log10(scan / Dmin)`, log-scale opacity; `B = density_scale·D + density_offset` (per-channel corrected density); `D′ = B + shadow_balance·w_lo(D̄) + highlight_balance·w_hi(D̄)` (after regional balance, §7.2) | **denser** negative — a **brighter** scene | `D`: `0` at base, `≈ [0, 6]` (slightly `< 0` if a pixel out-transmits the base); `B`/`D′` shifted by the offset (and, for `D′`, the regional balance) | `density::to_density`, `density::regional_balance`, `DensityImage.density` |
| **positive** (scene-referred linear) | the rendered positive: `10^(γ·(D′ − Dmax))` then print controls | **brighter** positive — a **brighter** scene | `[0, ∞)`, nominally `~[0, 1]`, **unclamped** (HDR); may dip `< 0` after black-point | `density::render`, `stages::render` |
| **output sample** (terminal) | the written TIFF value | brighter | `u16 [0, 65535]` (clamped to `[0,1]` first) or `f32` (unclamped) | `io::encode` |

**The one rule.** As the depicted **scene luminance rises**:
`transmission ↓ · density ↑ · positive ↑ · output ↑`. Transmission is the only
axis that falls.

**"bright" / "dark" / "highlight" / "shadow" always mean the *scene's*
luminance** — never a raw pixel value. A scene highlight is the **densest**
negative and the **lowest** transmission; a scene shadow (including the unexposed
base) is the **thinnest** negative and the **highest** transmission. So in any
mixed or ambiguous context never call a high-transmission value "bright" — say
"high-transmission". (A module working *purely* in the raw-scan transmission
domain may adopt a local "'bright' = raw-scan transmission" convention, stated
with an explicit §4 cross-reference — as the auto-base detector does, §8.) When
naming a numeric value, name its space: "high density", "high transmission",
"bright positive".

**The film-base paradox.** The unexposed base is at once the **highest
transmission**, **zero density** (`D = 0`), and renders to **near-black positive** —
it depicts scene black. *Brightest in the scan = darkest in the positive.*

**`Dmin`** is the film base **transmission** — the divisor the conversion anchors
on. It is the per-channel *relative* maximum (no **genuine picture** pixel
out-transmits it — dust, specular highlights, hot pixels, or noise can, which is
why `D` can dip `< 0` and why the `SCAN_EPSILON` floor exists),
**not** a value near 1: the orange mask and scanner gain pull channels down (real
Ektar base ≈ `[0.53, 0.26, 0.16]`, blue near the bottom). Named for minimum
*density* but stored as a transmission.

**`Dmax`** is nc's **display-white density anchor**: the corrected density `D′`
the render maps to positive `1.0`. It lives in **density space** (a `D′` value,
where the base is `0`) and is a **scalar** pooled across channels — a per-channel
`Dmax` would apply three gains in `10^(γ·(D′ − Dmax))`, i.e. a white balance, which
is the print-render stage's job, not the anchor's. ⚠️ **Distinct from classic
photographic film `Dmax`** (the negative's physical maximum optical density, at the
most-exposed point): nc's `Dmax` is a rendering anchor, though the `dmax-reference`
design derives it *from* a fully-exposed reference frame (near the film's physical
Dmax). Its **acquisition** (per-frame `auto` vs roll-fixed reference) is being
decided in `dmax-reference` (§12); its **meaning here — a scalar display-white
anchor in density units — is fixed.** Never mix it with a transmission (a base
transmission plus a range is a unit error).

**Domain glossary.** *auto-base* — auto-detecting `Dmin` from the unexposed rebate
(`FilmBaseSource::Auto`). *rebate* — the unexposed film leader between holder and
picture; maximum transmission, zero density. *holder* — the opaque scanner carrier;
near-zero transmission (`< 0.05`). *base-region* — a user rectangle sampled for
`Dmin` (`FilmBaseSource::Region`). *scene white / scene black* — the brightest /
darkest depicted scene luminance (highest / lowest `D′`). *display (paper) white /
black* — the output extremes (`1.0` / `0.0`). *uniform / spread* — a region is
uniform when its per-channel relative spread `(p_hi − p_lo) / p_hi ≤ 0.15`; "spread"
is that confidence figure. *candidate* — a holder-backed uniform band the auto
detector proposes as possible rebate.

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
                 │    (simple | density | sigmoid) — pluggable   │
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
2. density correction:       B  = per-channel scale·D + offset (orange-mask comp)
   regional balance:         D̄  = mean(B_r, B_g, B_b)   (scalar tone value)
                             D' = B + shadow_balance·w_lo(D̄) + highlight_balance·w_hi(D̄)
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

> **Under reconsideration (2026-07).** Treating `Dmax` as frame-local `auto`
> effectively *normalizes exposure per frame* — it brightens underexposed frames
> and forces an overcast scene's grey to display white — which conflicts with NC's
> "convert, don't grade" purpose (exposure belongs in Lightroom). The planned
> direction makes `Dmax` a **roll-fixed calibration** measured once from a
> fully-exposed reference frame (the light-struck leader), reused like `Dmin`, with
> per-frame `--auto-d-max` demoted to an opt-in exposure-normalizing mode. See §12
> item 14 (`dmax-reference`). The frame-local `auto` behavior described above is
> what currently ships.

**Regional (shadow/highlight) color balance.** A color *crossover* — a cast that
differs between shadows and highlights (expired film, misprocessing, mixed
lighting) — cannot be fixed by any single global per-channel gain/offset; in
density space it is a per-channel offset that varies with tone. Step 2 therefore
adds density-weighted offsets: `w_lo`/`w_hi` are complementary smoothstep ramps
over the corrected-density range `[lo, hi]` (`w_lo = 1` at `lo` fading smoothly
to `0` at `hi`, `w_hi = 1 − w_lo`, both saturating outside the range), so equal
shadow and highlight balances degenerate to a uniform `density_offset`. The
ramps take the **scalar** per-pixel tone `D̄` (the mean of the pre-regional
corrected channels), never each channel's own density — per-channel weighting
would let one channel of a crossover pixel receive the shadow correction while
another receives the highlight one, misfiring on exactly the pixels this control
exists to fix. Naming is from the **positive's** point of view: low density
(near base) is a *shadow*, high density a *highlight*, and (by the polarity
above) a positive balance value brightens that channel in its region. The range
anchors come from `density.balance_range`: `auto` (default) measures robust
percentiles (0.5 % / 99.5 %, nearest-rank over a deterministic sample) of the
per-pixel tone in a two-pass within step 2 — it cannot anchor on the `auto`
`Dmax`, which is measured *after* step 2 (circular) — while an explicit
`[lo, hi]` (e.g. a frame's reported range reused across a roll) short-circuits
the measuring pass. Neutral `[0,0,0]` balances (the default) skip the regional
pass entirely: the output is bit-exact with the unbalanced render. This runs
*before* the print render's white balance: stage 2 fixes the tone-dependent
crossover, print WB the remaining global cast. See §9.

**Auto neutral white balance (`print.white_balance`).** The step-4 white-balance
gains are a single mutually-exclusive source: explicit `[r, g, b]` gains (the
default, `[1, 1, 1]` = neutral), or one of two deterministic per-frame
estimators — `"gray-world"` (equalize the trimmed per-channel means, ≈ NLP
Auto-AVG) or `"percentile"` (equalize the channels at a matched near-white
percentile, ≈ NLP Auto-Neutral; more robust to a dominant scene color). The
estimators are pure statistics over a neutrally-rendered positive — finite
samples only, distribution extremes excluded (trim / the percentile's top tail)
so clipped speculars and dead pixels can't skew the estimate; no ML. Gains are
**green-anchored** (`g = 1`): auto WB corrects color, not overall brightness
(that is `print_exposure`'s job). The estimated gains are applied through the
**same step-4 slot** as explicit gains — before `black_point` and the highlight
soft-clip, never a post-hoc multiply — and the resolved gains land in the
convert JSON report, so a run that reuses them via `--white-balance` reproduces
the output bit-for-bit (measure once, reuse for the roll; §8). Explicit gains
beat an auto mode **by source**, not value: `--white-balance 1,1,1` over a
recipe's auto mode means neutral gains, not re-estimation. See §9.

Density conversion (steps 1–2) and print rendering (steps 3–4) are kept as
separate, independently parameterized sub-stages — the core fidelity rule from §3.

### 7.3 `sigmoid` — density-domain S-curve (H&D / paper response)

An S-shaped tone curve in density space, giving the shoulder/toe control of a
photographic H&D / print-paper characteristic instead of the `density`
algorithm's straight `10^(gamma·(D'−Dmax))` line. Shares steps 1–2 (and their
parameters, §9) and step 4 (the print render, `print.*`) with `density`; only
step 3 — the density → positive curve — is replaced:

```
t = contrast·(D' − Dmax)                       the straight line, in log10-output space
F = −contrast·Dmax                             paper-black floor (the line's value at D' = 0)
p = F + toe·log10(1 + 10^((t−F)/toe))          toe  FIRST: soft-max with F   (skipped if toe = 0)
v = p − shoulder·log10(1 + 10^(p/shoulder))    shoulder LAST: soft-min with 0 (skipped if shoulder = 0)
lin = 10^v
```

i.e. the straight line passed through two soft knees — a **toe** compressing the
approach to paper black, then a **shoulder** compressing the approach to display
white. **The knee order is deliberate:** the shoulder (soft-min with the
log-output-`0` ceiling) is applied *last*, so nothing lifts the result back above
white — for `shoulder > 0`, this **stage-3 output** is `≤ 1.0` for every finite
density, for any valid params (including a small `Dmax` or low-contrast auto
anchor), so under neutral print params the default u16 encode **cannot clip
highlights** (the stage-4 print render — exposure/gains — can still lift samples
above `1.0`). With `shoulder = 0` there is no roll-off and
highlights follow the (toe-shaped) line, which can exceed `1.0` like `density`.
The toe holds shadows to the paper-black floor `≈ 10^(−contrast·Dmax)` (exact
when `shoulder = 0`; the shoulder nudges it imperceptibly lower otherwise).
`toe`/`shoulder` are knee widths in log10 density units; `contrast` is the
mid-density slope in log-output space. The curve is strictly monotonic; with
`toe = shoulder = 0` both knees are skipped and it reproduces `density`'s step 3
**bit-for-bit** (`contrast` standing in for `density_gamma`), so `density`
remains the debuggable straight-line reference. `contrast` is capped (§9) — an
extreme slope would collapse the curve into a hard threshold that silently
destroys tonal detail.

Because both the white knee and the black floor derive from the anchor, the
S-curve is anchored on `[0, Dmax]` and **requires** one: `density.dmax = none`
(`--no-d-max`) with `--algorithm sigmoid` is a usage error (exit 2) —
scene-referred output stays a `density`-algorithm feature. The anchor is
resolved by the same `density.dmax` machinery (`auto` percentile / explicit
value) and reported the same way. `density.density_gamma` parameterizes the
straight line the S-curve replaces and is **ignored** under `sigmoid` (a report
warning fires when it was customized); `sigmoid.contrast` is the analogue.
`--highlight-compress` (a linear-space soft-clip after exposure/WB) composes
with the shoulder rather than being disabled — with the shoulder on and neutral
print params it simply never engages, since nothing exceeds `1.0`.

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
| `nc inspect` | Read a scan and emit a JSON report of format, channels, bit depth, candidate rebate regions (coordinates + spread, ready for `--base-region`), suggested `Dmin`. No output image. |
| `nc estimate` | Run only film-base/`Dmin` estimation; emit JSON with reuse-ready `--film-base` / recipe-fragment forms. `--grid` adds 5-cell agreement-checked sampling for blank reference frames. |
| `nc params`  | Print the full default/effective parameter set as JSON (for discovery and recipe scaffolding). |

### Recipes (JSON in/out)

- `--params recipe.json` — load a full parameter set from JSON.
- `--dump-params out.json` — write the effective parameters (defaults + overrides)
  to JSON. Individual `--flag` overrides take precedence over the loaded recipe,
  so an agent can load a roll recipe and tweak one value per frame.

The recipe JSON is **grouped into per-stage objects** (`input`, `film_base`,
`density`, `sigmoid`, `print`, `simple`, `output`, plus the top-level
`algorithm`) rather
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
# `estimate` measures Dmin from the sampled rectangle and reports it in
# directly reusable forms: a paste-ready --film-base flag string and a
# `film_base` recipe fragment (emitted only when the measurement is a valid
# explicit base — each channel in (0, 1] — else a warning explains why not).
nc estimate reference.tiff --base-region 200,0,300,3600 --report json
# → { "film_base": { "r": 0.553, "g": 0.271, "b": 0.159 },
#     "film_base_source": { "region": [200, 0, 300, 3600] },
#     "film_base_flag": "--film-base 0.553,0.271,0.159",
#     "film_base_recipe": { "source": { "explicit": [0.553, 0.271, 0.159] } }, … }
nc convert frame01.tiff -o frame01_pos.tiff --film-base 0.553,0.271,0.159
# …or paste film_base_recipe into roll-A.json as its "film_base" section and batch it.

# On a dedicated blank frame, `estimate --grid` samples a fixed 5-cell grid
# (corners + center) over the frame (or over --base-region) instead of a single
# measurement: the report gains a `grid` object (per-cell regions/values, the
# per-channel relative spread, the tolerance, and the agreement verdict), the
# combined base is the per-channel median across cells, and disagreement beyond
# the tolerance is a loud warning (--strict promotes it to a failing exit) —
# it diagnoses light leaks, scanner illumination falloff, or dust.
# A cells-disagree *warning* does NOT suppress the reuse-ready output: when the
# combined median base is in range it is still offered (film_base_flag /
# film_base_recipe), because the median resists a single bad cell. A consumer
# treating that base as authoritative should check `warnings`, or run --strict,
# which promotes the disagreement to a hard failure. (A *degenerate* base — see
# below — is different: it is a hard error, not a warning, and no reuse output.)
# A degenerate combined base (non-finite or <= 0 on any channel — e.g. --grid
# --base-region on the dark holder) is not a usable Dmin anchor, so --grid emits
# the diagnostic report (with grid.cells) and then **fails loudly regardless of
# --strict** (exit 1), matching the single-measurement path's finite-and-positive
# guard.
# --grid conflicts with --film-base (nothing to sample) and --auto-base (the
# grid replaces border detection). Deterministic: fixed layout, fixed percentile.
nc estimate blank.tiff --grid --report json

# Auto neutral white balance: estimate per-frame gains (percentile ≈ NLP
# Auto-Neutral; gray-world ≈ Auto-AVG), read the resolved gains back from the
# report, and freeze them into --white-balance / the roll recipe
# (print.white_balance = {"explicit": [...]}) — the reuse run is bit-identical.
nc convert frame01.tiff -o frame01_pos.tiff --auto-wb percentile --report json
# → { "white_balance": [1.083, 1.0, 0.941], ... }
nc convert frame02.tiff -o frame02_pos.tiff --white-balance 1.083,1.0,0.941
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
  A non-uniform rectangle (one that mixes rebate with image content) keeps its
  sampled value but raises a **uniformity warning** in the report (`--strict`
  promotes it) — a mixed rectangle otherwise yields a plausible-looking bad base
  with no signal.
- `--auto-base` (default) ⇒ `"auto"` — detect the unexposed rebate band behind
  the film holder (the inward-scan detector; see the ladder below). On no
  confident band it **fails loudly** and *suggests* `--base-content` — the opt-in
  content source owned by the separate `film-base-content-fallback` task (ladder
  tier 3 below); auto never silently falls back to it.

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
   (`nc estimate --grid`, §8), which doubles as a light-leak /
   illumination-falloff diagnostic.
2. **The rebate (the unexposed strip around each frame).** Reliable form: point
   `--base-region` at a visible rebate patch manually — `nc inspect` reports the
   detector's candidate rectangles (edge, coordinates, value, spread) so you can
   confirm one instead of measuring it in an image viewer (UI-assisted picking
   is a roadmap item, §12). Convenience form: `--auto-base` (the default) — real
   scans are laid out as
   `dark film holder → thin unexposed rebate → exposed picture`, the rebate being
   a narrow, uniform, bright band *inset behind the holder*, possibly on only
   some edges. The **inward-scan detector** marches 1-px strips in from each edge
   and keeps the first bright, uniform, value-continuous band sitting **behind**
   a contiguous dark-holder run; the base is the highest-transmission such
   candidate, higher-transmission than the frame interior on *every* channel (the
   rebate is per-channel minimum density = maximum transmission — nothing genuine
   can out-transmit clean base; "bright" here is raw-scan transmission, see §4
   Terminology). Requiring the
   holder outside the band defeats the bright-surround false positive (a uniform
   bright scene region bleeding to the frame edge has no holder outside it);
   cross-edge disagreement between surviving candidates is surfaced as a report
   warning. Confidence gates stay **deliberately strict** and detection **fails
   loudly** (naming the recovery flags) rather than emit a silently-wrong base.
   Threshold tuning against full-size scans (`real-scan-verification`) and a
   `--holder white|black` control for light holders are roadmap items (§12).
   **Known residual limit:** a flat, bright *scene* region sitting behind the
   holder on a rebate-less / cropped scan (e.g. sky along one edge) can still
   satisfy every RGB gate and, as the sole candidate, be taken as the base — a
   wrong `Dmin`, which shows up as a correctable global per-channel cast (the §8
   failure geometry), not a crossover. Distinguishing it needs signals this
   single-frame RGB pass lacks — colour-independent corroboration
   (`auto-base-neutral-stock`) and opacity-based film-boundary detection
   (`ir-holder-detection`); until those land, pin the base with
   `--base-region` / `--film-base` for work you're keeping.
3. **Content-based estimation (last resort, opt-in).** When the scan is cropped
   to the image with no unexposed film visible, a per-channel high percentile of
   the *exposed content* approximates the base (the thinnest area of a negative
   is the scene's deepest black, close to true base). This is an **explicit
   opt-in source** owned by the dedicated `film-base-content-fallback` task
   (`--base-content` / `film_base.source = "content"`) — it is **not** part of
   the auto detector: auto refusal only *suggests* it and never silently falls
   back, and the report will record that the base came from content statistics.
   When the assumption fails (foggy/high-key scenes), blacks wash out and pick up
   a cast — recoverable downstream as a global cast (`density_offset` / white
   balance).

**When every source is missing** (no explicit base, auto refuses, content mode
not requested), `convert` **fails loudly** with an actionable message naming the
recovery flags — an agent can catch the exit code and re-run with an explicit
choice. Estimator selection is never silent. **A degenerate resolved base** (a
zero / negative / non-finite channel — e.g. a `--base-region` on the dark holder)
is likewise rejected at the estimation stage rather than left to poison the
density divide or be echoed back by `nc estimate` as a trustworthy `Dmin`. This
holds for the `nc estimate --grid` combined base too: it emits the diagnostic
report (with `grid.cells`) and then fails loudly on a degenerate combined base
regardless of `--strict` (exit 1), the same code the single-measurement guard
returns. A
neutral base `[1,1,1]` is
representable but not recommended: it forfeits the per-channel orange-mask
neutralization (content estimation strictly dominates it). Note the failure
geometry is forgiving: because `D = -log10(scan/base)`, a base error is a
*constant per-channel density offset* — a global cast/exposure error correctable
downstream (`density_offset`, white balance) — never a shadow/highlight
crossover.

### Algorithm select
- `--algorithm simple|density|sigmoid`

### Density stage (algorithm = density, shared by sigmoid)
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
    (`--output-hdr`) workflows. **Density algorithm only** — the sigmoid S-curve
    is anchored on `[0, Dmax]`, so `sigmoid` + `none` is a usage error (§7.3).
- Regional (shadow/highlight) color balance (see §7.2). "Shadow"/"highlight"
  name the **positive's** tone regions (low/high corrected density); a positive
  value brightens that channel in its region. Defaults `[0, 0, 0]` are identity
  — the default output is bit-exact with the unbalanced render:
  - `--shadow-balance R,G,B` ⇒ `density.shadow_balance` — per-channel density
    offset applied in the positive's shadows.
  - `--highlight-balance R,G,B` ⇒ `density.highlight_balance` — per-channel
    density offset applied in the positive's highlights.
  - Tone-ramp anchors — a single mutually-exclusive choice, recipe key
    `density.balance_range` (default `"auto"`); the two flags conflict, and
    whichever is given replaces a recipe's `balance_range`. Only consulted when
    a balance is non-zero:
    - `--auto-balance-range` (default) ⇒ `"auto"` — measure `[lo, hi]` per frame
      from the corrected-density tone distribution (the 0.5th / 99.5th
      percentiles, nearest-rank). The `auto` run echoes the measured `[lo, hi]`
      in its JSON report, so a roll can capture one frame's range and replay it
      on the rest via `--balance-range` for consistent toning. Fails loudly when
      a balance is requested on a frame with no measurable range (uniform
      densities) — pass an explicit range instead.
    - `--balance-range LO,HI` ⇒ `{ "explicit": [lo, hi] }` — fix the ramp anchors
      (`lo < hi`, both finite, and their difference representable in `f32`).
- The `sigmoid` algorithm shares this whole section (including the regional
  balance above) **except `density_gamma`**, which parameterizes the
  straight-line curve it replaces and is ignored under `--algorithm sigmoid` (a
  report warning fires when it was customized; `sigmoid.contrast` is the
  analogue).

### Sigmoid stage (algorithm = sigmoid)
The stage-3 S-curve knobs (§7.3); stages 1–2 and 4 use the `density.*` and
`print.*` sections above. Recipe keys drop the flag prefix (`sigmoid.contrast`,
`sigmoid.toe`, `sigmoid.shoulder`):
- `--sigmoid-contrast <f>` — mid-density slope of the curve in log-output space
  (the `--density-gamma` analogue). Finite and in `(0, 50]`; default `1.0`. The
  upper cap guards against an extreme slope collapsing the S-curve into a hard
  black/white threshold that silently destroys tonal detail (use `--algorithm
  density` for genuinely extreme contrast).
- `--sigmoid-toe <f>` — toe (shadow) knee width in log10 density units; `0`
  disables the toe. In `[0, 10]`; default `0.2`. The upper cap is far beyond the
  ~`0.05–0.9` photographic range and rejects a degenerate width that would
  flatten the image into near-uniform tone without tripping the clip/non-finite
  counters.
- `--sigmoid-shoulder <f>` — shoulder (highlight) knee width in log10 density
  units; `0` disables the shoulder. In `[0, 10]`; default `0.2` (same cap
  rationale as `--sigmoid-toe`).

These caps reject only *nonsense / degenerate-asymptote* values (a knee of `10000`
that flattens the frame); within them, aggressive-but-valid contrast/knees produce
faithful, deliberate output that may posterize or crush — that is the user's
choice and is intentionally **not** warned (a degenerate-band warning would
false-positive on legitimate high-contrast conversions).

### Print / tone render
- `--print-exposure <f>` — overall positive exposure.
- `--black-point <f>` — paper black / shadow floor.
- White balance — a single mutually-exclusive choice, recipe key
  `print.white_balance` (default `{ "explicit": [1, 1, 1] }` = neutral; see
  §7.2). The two flags conflict (passing both is a usage error); whichever is
  given replaces a recipe's `white_balance` entirely. Explicit gains beat an
  auto mode **by source** — `--white-balance 1,1,1` over an auto recipe means
  neutral gains, not re-estimation:
  - `--white-balance R,G,B` ⇒ `{ "explicit": [r, g, b] }` — fixed
    highlight/neutral gains. For backward compatibility the recipe key also
    accepts a **bare `[r, g, b]` array** (the pre-auto-WB on-disk form, when
    `white_balance` was a plain array) as explicit gains, so older recipes /
    sidecars still parse; new output always writes the tagged form.
  - `--auto-wb gray-world` ⇒ `"gray-world"` — equalize the trimmed per-channel
    means (≈ Auto-AVG). Assumes the frame averages to neutral, so a dominant
    scene color biases it.
  - `--auto-wb percentile` ⇒ `"percentile"` — equalize the channels at a
    matched near-white percentile (≈ Auto-Neutral); more robust to dominant
    colors. The resolved gains land in the convert report (`white_balance`,
    green-anchored) ready to freeze into `--white-balance` / a roll recipe (§8).

  An auto mode requires `--algorithm density` or `--algorithm sigmoid` — the
  `simple` algorithm is the only one with no print white-balance stage, so
  `--auto-wb` with `simple` is a usage error (exit 2) rather than a silently
  dropped request (§3 fail-loudly).
- `--highlight-compress <f>` — highlight roll-off amount.

### Simple algorithm
- `--invert-white-balance R,G,B`
- `--clip-low <f>` / `--clip-high <f>`

### Output / encode (stage 5)
- `-o, --output <path>` (required)
- `--output-hdr` — write a 32-bit float TIFF (full HDR, no precision loss);
  without the flag the output is 16-bit integer. Recipe key `output.hdr`
  (bool, default `false`).
- `--output-sdr` — force the default 16-bit integer output, overriding a
  recipe's `output.hdr = true` (the flags-win escape hatch; an absent
  presence flag never clobbers a recipe value). Conflicts with
  `--output-hdr`; passing both is a usage error.
- `--output-profile <srgb|prophoto|acescg|path-to-icc>` (default is depth-aware:
  `srgb` for the 16-bit default, `acescg` for `--output-hdr`)
- `--bigtiff auto|on|off` (default `auto`)

### Global
- `--params <json>`, `--dump-params <json>`
- `--report json|none`, `--report-file <path>`
- `--strict` — promote report warnings (clipping, non-finite samples, grid
  disagreement, …) to a failing exit (see §11); on `convert` and `estimate`
- `-v/--verbose`, `--quiet`

**Telemetry (operational, `convert` only — NOT recipe keys).** Opt-in
performance + context telemetry. These are operational flags like `--report`, so
they are **not** conversion knobs: they never enter the recipe/sidecar and never
affect the output bytes (telemetry on or off ⇒ byte-identical TIFF + sidecar).
- `--telemetry` — append one JSON record for this run to the local JSONL log
  (default `$XDG_DATA_HOME/nc/telemetry.jsonl`, else `$HOME/.local/share/nc/…` on
  Unix / `%APPDATA%\nc\…` on Windows; override with the `NC_TELEMETRY_LOG` env
  var). Create-append; one object per line.
- `--telemetry-file <path>` — also write the record to `<path>` (`-` = stdout;
  overwrites a one-off file). May be combined with `--telemetry` (record lands in
  both sinks). Telemetry is collected iff at least one of these flags is present.
- **Best-effort:** a telemetry *write* failure is warned on stderr and never fails
  the run (exit stays 0; `--strict` does not promote it) — the one deliberate
  deviation from the fail-loudly rule, since telemetry is non-critical
  observability and the image already succeeded. A `--telemetry-file` **or**
  `--telemetry` log path (`NC_TELEMETRY_LOG` or the default path) that would *collide* with the
  input/output/sidecar/report-file is still a loud usage error (a config mistake,
  caught up front — an odd log path must never silently append into the scan).

**Telemetry record shape (`schema_version` 1, serialize-only JSON).** Designed for
a future background uploader (§12, `telemetry-upload`) to drain and ship:
```json
{
  "schema_version": 1,
  "timestamp_ms": 1752566400000,
  "nc_version": "0.1.0",
  "target": "aarch64-apple-darwin",
  "cpu_count": 14,
  "image": {
    "format": "hdri", "width": 502, "height": 462, "megapixels": 0.231924,
    "bit_depth": 16, "channels": 3, "ir_present": true,
    "input_bytes": 2017230, "output_bytes": 1392370
  },
  "timing_ms": {
    "total": 30.0, "decode": 5.0, "film_base": 0.0, "algorithm": 4.4,
    "color": 18.4, "encode": 1.0, "ir_export": 0.6
  },
  "conversion": {
    "algorithm": "density", "params_hash": "92a827ffd2d0aebd",
    "film_base_source": { "explicit": [0.9, 0.55, 0.42] },
    "dmax": 1.6195, "output_hdr": false
  },
  "outcome": { "warnings": 1, "clipped": 3419, "non_finite": 0 }
}
```
`timing_ms.ir_export` is present only when `--export-ir` ran; `conversion.dmax` only
when the density render applied an anchor. `params_hash` is a stable hash of the
effective recipe JSON (the same bytes as the sidecar), so identical conversions
share a hash without the record carrying the whole recipe.

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

A **degenerate resolved film base** (a zero / negative / non-finite channel)
maps to exit 1 (generic error) on both estimate paths: the single-measurement
path via `film_base::estimate`'s finite-and-positive guard, and `nc estimate
--grid` via a post-report guard on the combined base — the latter emits the
diagnostic report (with `grid.cells`) first, then fails regardless of `--strict`
(see §8). This is unconditional, distinct from the `--strict`-only promotion of
the grid *disagreement* warning.

## 12. Roadmap (follow-up tasks, explicitly out of Step 1)

These are deliberately deferred and recorded here so they aren't lost. Items
graduate into tracked tasks in [TASKS.md](TASKS.md) — several already have
(item 2's sigmoid → `algo-sigmoid`; item 3's B&W rendering → `bw-support`;
plus `dmax-white-anchor`, `auto-neutral-wb`, and `regional-color-balance` from
the NLP feature comparison, Phase 6).

1. **IR-based dust & scratch removal.** Consume the IR channel (already preserved
   in Step 1) to build a defect mask and inpaint defects. Parameters: IR
   threshold, mask dilation/morphology, inpainting method/strength. Must handle
   the known limits — disable/guard for silver B&W film and Kodachrome. New
   stages: `defect_mask`, `inpaint`. New flags under an `--ir-*` namespace.
2. **Additional algorithms.** The **sigmoid / explicit H&D-curve** model has
   since **shipped** as `--algorithm sigmoid` (§7.3, task `algo-sigmoid`);
   still open: possibly a power-law/exponent model (RawTherapee-style) for
   camera-scanned negatives. Added via the existing `Converter` trait,
   selectable with `--algorithm`.
3. **Black & white film support.** The *rendering* half has graduated into the
   tracked `bw-support` task (Phase 6): B&W film is still a density medium, so
   the `density` algorithm is the B&W renderer, plus a mono color model that
   pools R,G,B into one gray so scanner channel mismatch can't tint the output.
   What remains here is the *input* half: plain **16-bit RAW** scan files (not
   the SilverFast HDR/HDRi container). Note B&W negatives have no usable orange
   mask and no IR defect channel (silver blocks IR) — item 1's IR dust removal
   must be disabled/guarded for B&W.
4. **Camera RAW input.** Bayer/X-Trans and DNG ingestion (e.g. `rawler`/LibRaw)
   to support camera-scanning workflows.
5. **More output formats.** JPEG/PNG for proofs, EXR for HDR interchange.
6. **Roll-level presets & batch mode.** First-class roll recipes (film stock,
   `Dmin`, curve params, neutral spots) applied across many frames.
7. **Scanner ICC profiling workflow & QA harness.** IT8/target-based calibration
   and ΔE2000 / SSIM regression testing against standard test negatives.
8. **Robust auto film-base detection.** *(Done — implemented as the inward-scan
   detector, see §9 film-base.)* The kept scope shipped together: the detector
   for the real `holder → thin rebate → picture` layout (deterministic,
   fail-loud), the **uniformity warning on `--base-region`** (a mixed
   rebate/image rectangle otherwise yields a plausible-looking bad base
   silently), and `nc inspect` reporting **candidate rebate regions**
   (coordinates + spread) so CLI users confirm instead of measuring — the same
   data a future UI would highlight. The opt-in **content-based source**
   (`film_base.source = "content"` / `--base-content`, §9 ladder tier 3) is
   **reassigned** to the dedicated `film-base-content-fallback` task (item 13)
   and is **not** implemented here — the auto-refusal message only *suggests* it.
   Remaining: threshold tuning against full-size scans rides
   `real-scan-verification`.
9. **Light film holders.** Auto/border logic assumes a dark holder surround; some
   holders are white. Add a `--holder white|black` control (recipe key
   `film_base.holder`) so detection knows the surround polarity.
10. **Reuse-ready `nc estimate` output — shipped** (`estimate-reuse-output`).
    The estimate report now carries the measured base in directly reusable
    forms (`film_base_flag`, `film_base_recipe`) and `--grid` provides the
    5-cell agreement-checked sampling for unexposed-frame calibration (§9
    ladder tier 1) with the spread reported and disagreement warned loudly.
    See §8.
11. **UI-assisted film-base picking.** Once a UI layer exists: visual region
    picking for the rebate/reference frame, highlighting auto-detected
    candidates, and feedback when a chosen region fails the uniformity check
    (the CLI-side uniformity warning and inspect candidates above are the
    building blocks).
12. **Crash reporting & opt-in telemetry.** The **local, opt-in telemetry
    record** has **shipped** as the `perf-telemetry` task: an embedded, opt-in
    JSON record per `nc convert` (image + per-stage timing + run context) written
    to a local JSONL log and/or one-off file (`--telemetry` / `--telemetry-file`,
    `NC_TELEMETRY_LOG`; see §9), best-effort and byte-identical-output-preserving.
    **Remaining follow-ups are gated behind a strategy spike** (`telemetry-strategy`)
    that decides the shape before anything is built — because the questions are
    coupled (the data you collect drives the backend you need and the consent you
    must obtain): (a) **infrastructure** — a hosted/owned collection service, **OTel
    export vs custom ingestion**, and how the local JSONL queue drains (the
    `telemetry-upload` child: background/out-of-critical-path drain, strictly opt-in
    with a documented event list and an `NC_TELEMETRY=0`-style off switch, no stdout
    pollution/blocking/exit-code/output effect); (b) **expanded data** — error/failure
    events (today only successful runs emit a record; likely an `outcome.status` enum),
    a **panic/crash hook** (version, backtrace, params *shape* — never pixels or file
    paths), and coarse usage events (which flags/algorithms were used); and
    (c) **privacy/consent** — the opt-in model for upload and explicit PII/path
    scrubbing rules upholding the no-pixels/no-paths invariant. Note: the original
    LAB-benchmark `perf-instrumentation` task is **parked** (prototype on
    `prototype/perf-bench-instrumentation`); `perf-telemetry` is the real-world
    successor.
13. **Roll workflow & base-acquisition planner** (extends item 6). Two
    conversion workflows: **roll** (resolve `Dmin` + `Dmax` once, convert the
    whole roll from a frozen shared recipe — strongly preferred, keeps frames
    consistent) and **single** (per-frame best-effort). Roll-fixed params
    (`Dmin`, `Dmax`) vs frame-local (print/exposure) is the real model. An
    automatic **acquisition cascade** (unexposed reference → rebate region →
    `--auto-base` → cross-frame agreement → drop-to-single; content estimation
    only on **explicit opt-in**, never automatic) runs as a **plan** phase that
    emits the frozen recipe with provenance +
    confidence; conversion is deterministic replay. "Auto mode" is just roll
    conversion's default on a batch. Tracked: `roll-conversion`,
    `base-acquisition-planner`, `film-base-content-fallback`.
14. **Roll-fixed `Dmax` from a fully-exposed reference frame.** Supersedes the
    frame-local `auto` default (item in §7.2): `Dmax` is a film+scanner
    calibration, measured once from the light-struck leader (near-opaque in RGB,
    the max-density endpoint — always available) and reused per roll like `Dmin`.
    Fixed anchor resolves reference → per-stock constant → a nominal
    corrected-density anchor (in density units, *not* base transmission plus a
    range); `--auto-d-max` (per-frame exposure normalization) demoted to opt-in. Tracked:
    `dmax-reference`.
15. **IR-assisted film-holder detection.** First consumer of the IR channel
    besides item 1. Chromogenic dyes are IR-transparent, so all such film (base,
    picture, even fully-exposed leader) is bright in IR while the opaque holder is
    dark — a content-independent holder mask that RGB can't produce (holder and
    dense film are both dark in RGB). The mask is classified in **sub-edge
    segments** (a holder may cover only part of an edge), and holder segments are
    excluded before the RGB rebate search. Gated by an **explicit film-type signal
    (silver vs chromogenic)** — chromogenic B&W keeps a usable IR plane; silver
    B&W / no-IR (HDR 48-bit) → RGB-only fallback — *not* by color model or IR-plane
    presence. Also sidesteps holder *color* (item 9), since opacity, not color, is
    the IR signal. Tracked: `ir-holder-detection`.
16. **Conversion versioning & baseline comparison.** Stamp every output with
    build identity (crate semver + git commit), a behavioral `pipeline_version`
    (bumps *only* on default-behavior changes, gated by golden-output tests;
    `v0` = current baseline), and a resolved-params hash — in the **report**, and
    mirrored into the sidecar only via a backward-compatible metadata envelope
    (never as bare recipe keys, which would break the `--params`
    `deny_unknown_fields` round-trip). A benchmark manifest + `compare` step diffs the same scan/recipe set
    across two builds (per-channel ΔRGB / clip / timing) so quality and
    performance are trackable version-to-version. Quality metrics (ΔE2000/SSIM)
    extend via item 7's QA harness; timings via `perf-instrumentation`. `v0` is
    recorded in `docs/reports/v0-baseline.md`. Tracked: `conversion-versioning`.
17. **Stdout broken-pipe safety.** Every stdout JSON write — `emit_report`
    (convert/inspect/estimate) and `nc params` — uses `println!`, which
    panics on a closed pipe — the `nc … | head` / `… | jq 'first'` case, where the
    reader exits after
    enough bytes — printing a backtrace and returning failure though the conversion
    already succeeded. Route all stdout writes through a broken-pipe-tolerant helper
    (clean quiet exit on `BrokenPipe`, or reset `SIGPIPE` to `SIG_DFL` at startup),
    reusing the fail-soft `writeln!(stdout)` pattern the `--telemetry-file -` sink
    already uses. Pre-existing on `main`, independent of the telemetry work.
    Tracked: `stdout-broken-pipe-safety`.
18. **Input-side color management & a global (scanner) config tier.** *Consume* an
    input ICC profile — the scan's **embedded** profile or one supplied via
    `--input-profile` / `input.color.profile` — converting the scan into the linear
    working space before the pipeline, and lift the current exit-4 rejection. Beyond
    the transform itself, this introduces the architectural notion of a **global
    (cross-roll) tier**: a scanner IT8 profile is a property of the *device*,
    constant across rolls, and layers *above* the per-roll recipe (global defaults →
    roll recipe → per-frame override, extending items 6/13). Scope is *consuming* a
    profile; **creating** one from an IT8/target (scanner profiling) remains a
    non-goal (§2 out-of-scope). Tracked: `input-color-management`.

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
