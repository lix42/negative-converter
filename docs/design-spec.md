# Negative Converter — High-Level Design Spec (Step 1)

> Target: Step 1 (MVP) · Language: Rust

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
- Convert negative → positive with **normally 32-bit float linear image buffers**, while
  keeping the domains explicit: scanner measurement RGB through Dmin/density,
  typed `FilmRgbImage` after inversion/the selected density curve, then typed
  linear ACEScg after NC film RGB v1 mapping.
- **Current shipped pluggable algorithm** architecture, shipping **three**
  algorithm selections:
  1. `simple` — channel inversion + white balance (baseline / debug / B&W).
  2. `density` — density-domain inversion (Kodak Cineon / darktable `negadoctor`
     style) — the real default for color negatives.
  3. `sigmoid` — the same density reconstruction with an H&D-style S-curve.
  The target architecture replaces these top-level choices with tagged `simple`
  or `density` reconstruction; density owns a tagged exponential or sigmoid
  curve.
- All conversion parameters controllable via CLI flags and/or a JSON recipe file.
- Write **TIFF** output, selectable as **16-bit integer** or transitional
  **32-bit rendered float** via a flag.
- Auto-estimate film base (`Dmin`) from the unexposed border, with full CLI override.
- JSON report output (estimated parameters, warnings) and JSON recipe load/dump.

### Out of scope (Step 1) — see §12 Roadmap

- IR-based dust/scratch removal (follow-up task).
- Additional reconstruction/curve models beyond those listed above (follow-up
  tasks; the sigmoid /
  explicit H&D curve has since shipped post-MVP in the legacy
  `--algorithm sigmoid` schema, §7.3).
- Black & white film support, incl. plain 16-bit RAW scans (follow-up task).
- Camera RAW (Bayer/X-Trans) input, DNG processing.
- ML/AI assistance of any kind (auto-crop, neutral-patch detection, inpainting).
- Batch/roll preset management UI, GUI, scanner ICC profiling workflow.
- Output formats other than TIFF in Step 1. The post-MVP display-output roadmap
  now targets ISO gain-map HDR (initially HEIC) plus JPEG/PNG/EXR as appropriate.

## 3. Design principles

1. **Separate capture from rendering.** The scan is an archival record of
   transmitted light, not just an image to invert. The pipeline keeps a clean
   linear capture representation separate from the positive-rendering stage.
2. **Density conversion and print rendering are separate stages.** This is the
   single most important architectural rule for color fidelity.
3. **Float-first, explicit-domain internal pipeline.** Image buffers are normally
   linear `f32`, but "linear" does not imply one color space: values are scanner
   measurements before reconstruction, NC film RGB after the curve, and defined
   ACEScg film rendering afterward. Bit-depth reduction happens only at the final
   encode step.
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

After decode, the image is normalized to **linear `f32` scanner RGB measurement
coordinates** in `[0,1]` (plus an optional `f32` IR plane). These values are not
silently Rec.709, sRGB, ACEScg, or another colorimetric working space. The input
semantic resolver (`pipeline::input_semantics`, task `input-data-semantics`)
verifies the transfer encoding and measurement meaning as two independent axes
before Dmin/density — only a supported linear transfer paired with scanner-device
meaning enters the pipeline, and ambiguity fails loudly (§9 Input/decode);
nothing in the negative algorithm needs to know the on-disk container.

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
| **`D′` at the reconstruction→curve handoff** | the same corrected density `D′` (row above), named at the point it is passed to the selected density-to-positive curve | **denser** negative — a **brighter** scene | density units — `D′`'s range as defined in the row above (no re-clamping at the boundary) | replacement reconstruction/curve boundary |
| **NC film RGB v1** (`FilmRgbImage`) | intentional positive film rendering from simple inversion or the exponential/sigmoid density curve; interpreted consistently as linear Rec.709/D65 | **brighter** positive — a **brighter** rendered scene | algorithm-defined and unclamped `f32` | planned typed reconstruction output |
| **ACEScg film rendering** (`AcesCgImage`) | NC film RGB v1 transformed/adapted into linear ACEScg/D60; preserves film/lens/development/scanner character and is not physical scene recovery | **brighter** rendered value | unclamped `f32`; nominal diffuse white is workflow-defined | planned working-space mapper |
| **rendered display positive** | linear ACEScg film rendering after shared white balance/exposure/black/range placement, then output-specific highlight/reference-white/tone and destination gamut mapping | **brighter** rendered value | unclamped until the chosen display policy requires limiting | planned SDR/HDR display-render stages |
| **output sample** (terminal) | the written image value | brighter | preset/container-defined integer or float encoding | `io::encode` and planned HDR encoders |

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

**`Dmax`** is named for nc's shipped **legacy display-white density anchor**: in
the current density render, corrected density `D′ = Dmax` maps to positive
`1.0`. It lives in **density space** (a `D′` value,
where the base is `0`) and is a **scalar** pooled across channels — a per-channel
`Dmax` would apply three gains in `10^(γ·(D′ − Dmax))`, i.e. a white balance, which
is the print-render stage's job, not the anchor's. ⚠️ **Distinct from classic
photographic film `Dmax`** (the negative's physical maximum optical density, at
the most-exposed point). In the replacement pipeline Dmax belongs to the selected
density curve: exponential uses it for scalar placement and sigmoid uses it as a
curve-shaping input. SDR/HDR rendering owns display reference white. The shipped
`dmax-reference`
workflow derives it *from* a fully-exposed reference frame (near the film's
physical Dmax) and freezes it as a roll-fixed calibration by default; the demoted
per-frame `auto` remains opt-in exposure normalization. Never mix it with a
transmission (a base transmission plus a range is a unit error).

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

- **Current implemented container:** TIFF (BigTIFF when size requires 64-bit
  offsets).
- **Current implemented bit depth (flag-controlled):**
  - default (no `--output-hdr`) → 16-bit integer TIFF (standard archival positive).
  - `--output-hdr` → 32-bit float TIFF with unclamped values **after the current
    print-render controls**. This is a transitional rendered float TIFF, neither
    the future `film-master` nor a Rec.2100 display-HDR image.
- **Current color selection:** the output color space is a CLI
  option (`--output-profile`). The default depends on output depth:
  - 16-bit (default) output → **sRGB** (standard, display-ready positive).
  - float (`--output-hdr`) output → provisionally transformed/tagged **linear
    ACEScg**, but still after the current print renderer; it is not the target
    `film-master`. (`prophoto` and user ICC files are also accepted.)
  Either default can be overridden explicitly. Output is tagged with the embedded
  ICC profile for the chosen space.
- **Working-space intent:** the current implementation treats reconstructed
  scanner/film RGB as linear Rec.709 before its output transform. The replacement
  pipeline standardizes that existing interpretation as **NC film RGB v1** and
  transforms/adapts it into linear ACEScg/D60 for every named output. This
  preserves NC's intentional film rendering; it is not a provisional claim about
  physical scene color. Measured correction is an optional explicit profile.
- **Target product default (post-MVP):** `gain-map-hdr` — a standards-neutral,
  backward-compatible SDR rendition plus an ISO 21496-1 gain map, initially in
  HEIC subject to the encoder/licensing spike. The SDR base is Display P3; aware
  readers reconstruct the HDR rendition and unaware readers show the SDR base.
  This is not Apple-only: ISO 21496-1 is the public model and non-Apple support is
  an acceptance requirement.
- **Planned presets:**
  - `gain-map-hdr` — default, backward-compatible display HDR;
  - `display-p3` — wide-gamut SDR;
  - `compatibility` — sRGB SDR;
  - `film-master` — unclamped 32-bit float linear ACEScg TIFF preserving NC's film rendering;
  - `hdr-pq` — single-rendition BT.2020 / Rec.2100 PQ;
  - `hdr-hlg` — explicit HLG/broadcast-oriented output;
  - `custom` — expert-selected format/profile policy.
  A preset resolves container, bit depth, primaries/profile, transfer function,
  tone/gamut mapping, and metadata together. The current `--output-hdr` name is
  therefore temporary/ambiguous and will not be used to mean both float master
  data and display HDR. The output path remains required and is never silently
  renamed: its suffix must match the preset's resolved container or conversion
  fails with the accepted suffixes. Named presets are atomic and cannot be mixed
  with legacy depth/profile/container flags; advanced explicit combinations use
  `custom`. Legacy output flags without a preset retain the transitional TIFF
  behavior until migration is complete. `film-master` branches directly from NC
  film RGB v1 mapped linear ACEScg and bypasses white balance, exposure, black/range placement,
  highlight compression, and all display tone/gamut rendering; a creatively or
  print/display-adjusted linear master is an explicit `custom` workflow, not the
  default master. An explicitly selected measured correction is exempt: it
  remains `film-master` and must record profile identity/hash/scope provenance. This
  master contains intentional film/lens/development/scanner character and is not
  physical scene-linear recovery. To preserve cross-frame exposure,
  `film-master` rejects frame-local auto Dmax. Exponential accepts supported
  `none` or fixed/roll placement; sigmoid uses fixed Dmax as a curve-shaping
  input; simple has no Dmax. After recipe/CLI merge it also rejects any
  non-default downstream WB/exposure/black/white/highlight/tone/gamut/display-transfer
  control; there is no silent ignore mode. Creatively or print/display-adjusted
  linear output is `custom`; measured correction alone does not rename the
  preset.
- **Metadata:** the effective parameter set (recipe) and key estimated values are
  written to a **sidecar JSON** next to the output (paired by name; the same shape
  as `--dump-params`). Current TIFF output embeds the ICC profile of the chosen
  space; future HDR containers carry the profile/CICP and gain/headroom metadata
  required by their preset. The recipe is deliberately *not* embedded in the
  image container (resolved, §13).

## 6. Pipeline architecture

The conversion is a linear sequence of pure-function stages. Each stage has its
own parameter struct and can be unit-tested in isolation.

The diagram below depicts the **target / replacement** architecture (tagged
reconstruction, NC film RGB v1 working-space mapping, and the film-master /
display-render split). The **current shipped** pipeline implements: decode +
input-semantics resolution, film-base / `Dmin` estimation, an `Algorithm` +
`Converter` render with print controls applied *inside* it, a working→output ICC
transform, and TIFF encode. See the "Architecture" section of `CLAUDE.md` for the
current-vs-target framing.

```
                 ┌──────────────────────────────────────────────┐
  input file ──▶ │ 1. Decode + resolve input semantics             │
                 │    (SilverFast HDR/HDRi → f32 scanner RGB[+IR]) │
                 └──────────────────────────────────────────────┘
                                     │ linear scanner RGB (f32), IR (f32, opt)
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 2. Film-base / Dmin estimate (auto or CLI)    │
                 └──────────────────────────────────────────────┘
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 3. Tagged negative reconstruction              │
                 │    simple | density                            │
                 │    density curve: exponential | sigmoid        │
                 └──────────────────────────────────────────────┘
                                     │ FilmRgbImage
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 4. NC film RGB v1 working-space mapping        │
                 │    (linear Rec.709/D65 → linear ACEScg/D60)    │
                 └──────────────────────────────────────────────┘
                          ┌──────────┴──────────┐
                          ▼                     ▼
              ┌──────────────────────┐  ┌─────────────────────────┐
              │ 5a. Film master      │  │ 5b. Display rendering   │
              │ linear ACEScg direct │  │ print controls + SDR/HDR│
              └──────────────────────┘  └─────────────────────────┘
                          └──────────┬──────────┘
                                     ▼
                 ┌──────────────────────────────────────────────┐
                 │ 6. Encode + profile/metadata + sidecar         │
                 └──────────────────────────────────────────────┘
                                     ▼  output image (+ output.json)
```

Stage 1's semantic resolution is **implemented** (`pipeline::input_semantics`,
task `input-data-semantics`; see §4 and §9). The replacement stage 3 adopts a
tagged reconstruction schema: `simple`, or `density` containing density
parameters and a tagged `exponential` (default) or `sigmoid` curve. It preserves
the current exponential pixels and exact sigmoid equation. Dmax belongs to the
curve stage—scalar placement for exponential, curve shaping for sigmoid. Every
path returns private-field `FilmRgbImage`.

Stage 4 defines **NC film RGB v1** as the existing intentional interpretation of
that film rendering as linear Rec.709/D65, followed by the pinned standard
transform/adaptation into linear ACEScg/D60. It returns private-field
`AcesCgImage`. This one mapping is shared by simple and both density curves,
preserves film/lens/development/scanner differences, and makes no claim of
physical scene recovery. Named color outputs cannot merely tag `FilmRgbImage`.
Optional measured correction profiles may be explicitly selected later, but
they are not part of this default mapping and block no output work.
The optional task owns `--correction-profile PATH` /
`correction.profile = {"file": "PATH"}` (default `null`) and inserts correction
immediately after NC film RGB v1 mapping, before the film-master/display split.
An explicitly corrected film master remains the `film-master` preset but records
the profile identity/hash, corrected scope, and provenance; absence is a
bit-identical no-op.

For simple, stage 3 ends at raw unclamped `1 - scan/Dmin`; current inversion-WB
and clip-low/high remapping move to the downstream shared contract for named
presets. White balance, exposure, black/range placement, highlight compression,
and output tone/gamut mapping live after ACEScg on the display branch. The
`film-master` branch bypasses them and encodes stage 4 directly, rejects
frame-local auto Dmax, and records that it contains intentional film rendering
rather than a physical scene-linear recovery. Legacy no-preset TIFF calls retain
their current ordering until preset activation.

Within the display branch, SDR and HDR share the same resolved linear white
balance, exposure, and black/range placement. They diverge only for output-specific
highlight/reference-white, tone, gamut, and transfer rendering, so a gain-map
pair starts from one consistently adjusted source without forcing SDR highlight
compression onto the HDR rendition.
The shared adjustment order is WB → exposure → the existing black-point
operation → `print.linear_range` affine placement → branch-specific work;
`linear_range` defaults to `[0,1]` and requires finite `low < high`.

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

## 7. Reconstruction and density curves

The currently shipped implementation selects a boxed `Converter` with
`--algorithm simple|density|sigmoid` and returns an untyped `LinearImage`. That
is the **legacy, transitional implementation**, not the target interface. The
replacement accepts `--reconstruction simple|density`; density then selects
`--density-curve exponential|sigmoid`. Every target path returns
`FilmRgbImage`, so only the working-space mapper can construct `AcesCgImage`.

### 7.1 `simple` — inversion baseline

Channel inversion plus white balance / border neutralization. Cheap, predictable,
useful for B&W negatives and as a debugging reference. Not a strong endpoint for
color negatives (ignores density behavior and the orange mask).

The currently shipped stages are `positive = 1 - scan/Dmin` →
`invert_white_balance` gain per channel → the `clip_low`/`clip_high` affine
black/white remap. In the replacement pipeline, stage 3 ends at unclamped
`U_c = 1 - scan_c/Dmin_c` and returns `FilmRgbImage`. `simple` has no Dmax.
Inversion WB and clip remapping move after the ACEScg boundary to the downstream
shared WB/black/range-placement contract. The old flags/`simple.*` recipe keys use a warned
migration: target presets resolve `--invert-white-balance` to explicit
`print.white_balance` and clip endpoints to
`print.linear_range = [low, high]` / atomic `--linear-range LOW,HIGH`. Range
merge starts from the recipe pair or `[0,1]`; the atomic flag replaces both
endpoints and conflicts with either legacy flag. Without it, `--clip-low` and
`--clip-high` independently override their endpoint, after which finite
`low < high` is validated. Reports warn and record each endpoint's provenance;
new recipes/reports emit only replacement names. Named presets apply the values
only after NC film RGB mapping. Legacy no-preset TIFF calls keep current ordering
until migration. Aliases preserve requested values, not legacy
pixels: per-channel gains generally do not commute with the working-space
matrix. Target activation warns; because preset/default pixels change,
`conversion-versioning` owns the corresponding golden-tested
`pipeline_version` bump.

### 7.2 `density` — density-domain inversion (default)

The credible baseline for color negatives, following Kodak Cineon / darktable
`negadoctor` ideas:

```
1. transmission → density:   D  = -log10(scan / Dmin_transmission)   (per channel)
2. density correction:       B  = per-channel scale·D + offset (orange-mask comp)
   regional balance:         D̄  = mean(B_r, B_g, B_b)   (scalar tone value)
                             D' = B + shadow_balance·w_lo(D̄) + highlight_balance·w_hi(D̄)
3. density curve:            exponential { gamma, Dmax } or
                             sigmoid { contrast, toe, shoulder, Dmax }
4. typed film positive:      FilmRgbImage
5. NC film RGB v1 mapping:   linear Rec.709/D65 → linear ACEScg/D60
6. print/display controls:   white balance, exposure, black/range placement
```

Steps 1–2 are density reconstruction. The tagged curve owns the positive mapping
and Dmax semantics. Exponential preserves the current
`10^(gamma·(D'−Dmax))` pixels, with Dmax as scalar placement; sigmoid preserves
the current S-curve exactly and uses Dmax to shape that curve. Both return the
same typed film RGB boundary before the shared working-space transform. Step 6
currently executes inside the density renderer, but named presets move it after
the ACEScg boundary.

**Polarity.** With `D = -log10(scan / Dmin)` the density is `≥ 0` and *grows* with
the film's optical density — the unexposed base (scene black) sits at `D = 0`, a
dense negative area (a scene highlight) at large `D`. A positive must brighten as
`D` grows, so step 3 uses `10^(+gamma·D')`, **not** `10^(−gamma·D')` (which would
reproduce the negative). This matches darktable `negadoctor` (denser negative →
brighter print).

**Legacy display-white anchor; curve-stage Dmax.** In the currently shipped
density renderer, `Dmax` is the corrected density
of scene white and the expression `10^(gamma·(D'−Dmax))` guarantees that
`D' = Dmax` maps to `1.0`. The base maps to `10^(−gamma·Dmax) ≈ 0`; with `none`,
the current renderer reproduces its pre-anchor output bit-for-bit (base `1.0`,
detail above). Current `--output-hdr` is still a rendered float TIFF, not the
target `film-master` branch.

In the replacement schema, exponential retains Dmax as scalar placement and
sigmoid retains it as a nonlinear curve-shaping input. Display reference white
belongs to the later SDR/HDR render. Fixed/roll Dmax preserves cross-frame
exposure; frame-local auto Dmax is exposure normalization and is rejected for
`film-master`.

`Dmax` is a **roll-fixed calibration** — a film + scanner property reused across
the roll, like the `Dmin` base — **not** a per-frame measurement. Anchoring each
frame's densest pixel to display white *normalizes exposure per frame* (it
brightens underexposed frames and forces an overcast scene's grey to white), which
conflicts with NC's "convert faithfully, grade in Lightroom" purpose. The
current `density.dmax = fixed` (target
`reconstruction.curve.dmax = "fixed"`) therefore resolves a **fixed** anchor in the order
**measured reference → per-stock constant → nominal**: a value measured once from
a fully-exposed reference frame (§8, `estimate --d-max-region`) or a known
per-stock constant is carried as `{ "explicit": <d> }`; with no calibration a
**nominal** corrected-density anchor (`Dmax ≈ 2.0`, a scene-independent placement
*in density units* — not a base transmission plus a range) applies. The brightest
pixel then maps to *wherever it falls* (below white for a dim frame, clipping above
for a specular), the faithful behavior. `auto` (`--auto-d-max`) — the demoted
per-frame percentile measurement — remains as an opt-in **exposure-normalizing**
mode, and `"none"` (`--no-d-max`) selects unity exponential placement: an
unanchored film rendering whose base is `1.0` and whose detail rises above it.
This does not recover physical scene values. See §9. (This is the
`dmax-reference` design, §12 item 14; it supersedes
the earlier frame-local `auto` default.)

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
anchors come from target `reconstruction.density.balance_range` (current
`density.balance_range`): `auto` (default) measures robust
percentiles (0.5 % / 99.5 %, nearest-rank over a deterministic sample) of the
per-pixel tone in a two-pass within step 2 — it cannot anchor on the `auto`
`Dmax`, which is measured *after* step 2 (circular) — while an explicit
`[lo, hi]` (e.g. a frame's reported range reused across a roll) short-circuits
the measuring pass. Neutral `[0,0,0]` balances (the default) skip the regional
pass entirely: the output is bit-exact with the unbalanced render. This runs
*before* the print render's white balance: stage 2 fixes the tone-dependent
crossover, print WB the remaining global cast. See §9.

**Auto neutral white balance (`print.white_balance`).** The print/display-stage white-balance
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
**same print/display slot** as explicit gains — before `black_point` and the highlight
soft-clip, never a post-hoc multiply — and the resolved gains land in the
convert JSON report, so a run that reuses them via `--white-balance` reproduces
the output bit-for-bit (measure once, reuse for the roll; §8). Explicit gains
beat an auto mode **by source**, not value: `--white-balance 1,1,1` over a
recipe's auto mode means neutral gains, not re-estimation. See §9.

Negative reconstruction, density-to-positive curve, working-space mapping, and
print/display rendering remain separate, independently parameterized
stages — the core fidelity rule from §3.

### 7.3 `sigmoid` — density-domain S-curve (H&D / paper response)

An S-shaped tone curve in density space, giving the shoulder/toe control of a
photographic H&D / print-paper characteristic instead of the `density`
algorithm's straight `10^(gamma·(D'−Dmax))` line. It shares density correction
(steps 1–2 and their parameters, §9) and the later print/display render
(`print.*`) with `density`; only step 3 — the density → positive curve — is
replaced:

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
highlights** (the later print/display render — exposure/gains — can still lift samples
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
S-curve is anchored on `[0, Dmax]` and **requires** one. In the target schema,
`curve.dmax = "none"` with `curve.type = "sigmoid"` is a usage error (exit 2);
unity placement is supported only by the exponential curve. The anchor uses the
same fixed, explicit/reference-derived, or opt-in auto resolution policy as the
exponential curve. Gamma exists only in the exponential variant. Supplying
`--density-gamma` while the resolved curve is sigmoid is an invalid combination
after merge (exit 2), never a warning or ignored value. The currently shipped
legacy implementation instead reads sibling `density.density_gamma` and
`sigmoid.*`; its historical ignored-gamma warning disappears at migration.
`--highlight-compress` (a linear-space soft-clip after exposure/WB) composes
with the shoulder rather than being disabled — with the shoulder on and neutral
print params it simply never engages, since nothing exceeds `1.0`.

### Current and target interfaces (sketch)

```rust
// Current shipped, transitional interface.
pub trait Converter {
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage>;
    fn convert_reported(&self, image: &LinearImage, base: &FilmBase)
        -> Result<(LinearImage, ConvertReport)> { /* default provided */ }
}

// Target stage boundaries. Fields are private; only the owning stage constructs
// each value, and named output code accepts AcesCgImage rather than FilmRgbImage.
pub struct FilmRgbImage { /* private */ }
pub struct AcesCgImage { /* private */ }

pub fn reconstruct(
    image: &LinearImage,
    base: &FilmBase,
    config: &Reconstruction,
) -> Result<(FilmRgbImage, ReconstructionReport)>;

pub fn map_nc_film_rgb_v1(image: FilmRgbImage) -> Result<AcesCgImage>;
```

## 8. CLI design

A single binary (working name `nc`) with subcommands. The agent-facing surface is
optimized for scripting: flags for everything, JSON in/out, stable exit codes,
no interactive prompts.

### Subcommands

| Command | Purpose |
|---|---|
| `nc convert` | The main pipeline: negative file → positive TIFF. |
| `nc roll` | Convert a batch of frames from one shared, frozen recipe (the batch-**apply** scaffold). Per-frame outputs into `--out-dir` + a roll-level JSON report. Single-frame `convert` is unchanged; roll is additive. |
| `nc inspect` | Read a scan and emit a JSON report of format, channels, bit depth, candidate rebate regions (coordinates + spread, ready for `--base-region`), suggested `Dmin`. No output image. |
| `nc estimate` | Run only film-base/`Dmin` estimation; emit JSON with reuse-ready `--film-base` / recipe-fragment forms. `--grid` adds 5-cell agreement-checked sampling for blank reference frames. `--d-max-region` additionally measures the roll-fixed display-white anchor `Dmax` from a fully-exposed reference frame, emitting reuse-ready `--d-max` / `density.dmax` forms. |
| `nc params`  | Print the full default/effective parameter set as JSON (for discovery and recipe scaffolding). |

### Recipes (JSON in/out)

- `--params recipe.json` — load a full parameter set from JSON.
- `--dump-params out.json` — write the effective parameters (defaults + overrides)
  to JSON. Individual `--flag` overrides take precedence over the loaded recipe,
  so an agent can load a roll recipe and tweak one value per frame.

The currently shipped recipe is grouped into `input`, `film_base`, `density`,
`sigmoid`, `print`, `simple`, and `output`, plus top-level `algorithm`. This is
the **legacy transitional schema** implemented today:

```json
{
  "algorithm": "density",
  "density": { "density_gamma": 1.8 },
  "print":   { "print_exposure": 0.0, "black_point": 0.002 },
  "output":  { "hdr": true }
}
```

The target recipe replaces all top-level `algorithm`, `density`, `sigmoid`, and
`simple` selection forms with exactly one tagged `reconstruction` object.
These are the complete target reconstruction shapes (other stage objects are
omitted here):

```json
{
  "reconstruction": {
    "schema_version": 1,
    "type": "simple"
  }
}
```

```json
{
  "reconstruction": {
    "schema_version": 1,
    "type": "density",
    "density": {
      "scale": [1.0, 1.0, 1.0],
      "offset": [0.0, 0.0, 0.0],
      "shadow_balance": [0.0, 0.0, 0.0],
      "highlight_balance": [0.0, 0.0, 0.0],
      "balance_range": "auto"
    },
    "curve": {
      "type": "exponential",
      "gamma": 1.0,
      "dmax": "fixed"
    }
  }
}
```

```json
{
  "reconstruction": {
    "schema_version": 1,
    "type": "density",
    "density": {
      "scale": [1.0, 1.0, 1.0],
      "offset": [0.0, 0.0, 0.0],
      "shadow_balance": [0.0, 0.0, 0.0],
      "highlight_balance": [0.0, 0.0, 0.0],
      "balance_range": {"explicit": [0.1, 1.9]}
    },
    "curve": {
      "type": "sigmoid",
      "contrast": 1.0,
      "toe": 0.2,
      "shoulder": 0.2,
      "dmax": {"explicit": 2.0}
    }
  }
}
```

`reconstruction.schema_version` is exactly `1`. Partial input may omit it and
defaults to 1; resolved recipes always emit it. `curve.dmax` accepts
`"fixed"`, `"auto"`, `"none"`, or
`{"explicit": <density>}`; `"none"` is valid only for exponential. Omitted
density fields take the displayed defaults. Partial input may omit
`reconstruction.curve`, which selects exponential with its defaults; every
resolved recipe/report emits exactly one tagged curve. Partial objects are
otherwise permitted. Unknown fields are rejected at every level. The legacy
top-level forms are rejected with a migration error once this schema activates;
they are not aliases.

Target preset migration also adds `print.linear_range` and emits simple
WB/range adjustments only under `print`. Legacy
`simple.invert_white_balance`, `simple.clip_low`, and `simple.clip_high` are
accepted solely as warned input aliases during the preset migration described in
§9; they are not part of `reconstruction`. Legacy no-preset TIFF calls retain
their current pixel ordering until migration.

### Reports & determinism

- `--report json` — emit a machine-readable result (estimated values, clip
  warnings, timings, output path) to stdout or `--report-file`.
- `--seed <n>` — fix any stochastic step (none in Step 1, reserved).
- Stable, documented **exit codes** (see §11).

At target-schema activation, the report's effective `recipe.reconstruction` is
the exact tagged object above. Resolution diagnostics use this exact additional
shape (unrelated report fields omitted):

```json
{
  "recipe": {
    "reconstruction": {
      "schema_version": 1,
      "type": "density",
      "density": {
        "scale": [1.0, 1.0, 1.0],
        "offset": [0.0, 0.0, 0.0],
        "shadow_balance": [0.0, 0.0, 0.0],
        "highlight_balance": [0.0, 0.0, 0.0],
        "balance_range": "auto"
      },
      "curve": {"type": "exponential", "gamma": 1.0, "dmax": "fixed"}
    }
  },
  "reconstruction_result": {
    "type": "density",
    "curve": {
      "type": "exponential",
      "dmax": {
        "policy": "fixed",
        "value": 2.0,
        "provenance": "default"
      }
    }
  },
  "working_mapping": "nc-film-rgb-v1"
}
```

For sigmoid, `curve.type` is `"sigmoid"` with the same resolved `dmax` object;
for exponential `"none"`, `dmax` is
`{"policy":"none","value":null,"provenance":"recipe"}`; for simple,
`reconstruction_result` is exactly `{"type":"simple"}`. `policy` is one of
`fixed|explicit|auto|none`, and `provenance` is one of
`default|recipe|cli|auto-frame`. A scalar measured from a reference and frozen
into a recipe has `policy = "explicit"` and `provenance = "recipe"`; its capture
region remains estimate/report provenance rather than a runtime re-read
directive. `reconstruction.schema_version = 1` versions the wire schema; it is
not the behavioral `pipeline_version`. The `conversion-versioning` task owns
stamping and bumping `pipeline_version`, and does so only when default pixels
change. This bit-identical refactor preserves legacy no-preset pixels and does
not itself bump that field. Activating named presets and the new simple ordering
does change pixels and must cross a prospective, golden-tested
`pipeline_version` boundary owned by `conversion-versioning`. Recipe/report
round trips, fixtures, and migration errors pin the reconstruction schema.

### Example invocations

```bash
# Current shipped default density conversion: auto Dmin, fixed/roll nominal Dmax,
# 16-bit TIFF, JSON report. This uses the transitional CLI.
nc convert in.tiff -o out.tiff --algorithm density --report json

# Target equivalent after reconstruction-schema migration.
nc convert in.tiff -o out.tiff --reconstruction density \
  --density-curve exponential --report json

# Transitional rendered float TIFF: --no-d-max disables the current legacy Dmax factor
# (base → 1.0, detail above), then the current print controls still run and the
# depth-aware default profile (acescg for HDR) applies. This is NOT film-master.
nc convert in.tiff -o out.tiff \
  --algorithm density --output-hdr --no-d-max \
  --film-base 0.92,0.55,0.42 \
  --density-gamma 1.8 --print-exposure 0.0 --black-point 0.002 \
  --highlight-compress 0.3

# Reuse a roll recipe but override one knob for this frame.
nc convert frame12.tiff -o frame12_pos.tiff \
  --params roll-A.json --print-exposure 0.15

# Convert a whole roll from ONE shared, frozen recipe (batch-apply). The shared
# recipe config (roll-fixed film base + Dmax) lives in roll-A.json and appears
# once at the top of the roll report; each frame additionally echoes the resolved
# base/Dmax it used. Per-frame outputs are written to out/ as <stem>_positive.tiff.
nc roll frame01.tiff frame02.tiff frame03.tiff --out-dir out/ --params roll-A.json
nc roll scans/ --out-dir out/ --params roll-A.json   # a directory expands to its .tif/.tiff
# Per-frame overrides via a manifest: each frame may carry its own output path
# and a partial-recipe `params` deep-merged onto the shared recipe for that frame
# only (the "frame-local" knobs, e.g. print exposure). The manifest is the shape
# the base-acquisition-planner will emit.
#   frames.json: { "frames": [
#     { "input": "frame01.tiff" },
#     { "input": "frame02.tiff", "params": { "print": { "print_exposure": 0.15 } } } ] }
nc roll --frames frames.json --out-dir out/ --params roll-A.json
# The roll report: { "command": "roll", "recipe": { …shared frozen recipe… },
#   "warnings": [ …roll-level, e.g. base-not-frozen… ],
#   "frames": [ { "input": …, "output": …, "status": "ok", "film_base": …,
#     "dmax": …, "warnings": […], "overrides": … }, … ],
#   "summary": { "total": 3, "succeeded": 3, "failed": 0 } }
# (each frame's "film_base"/"dmax" is the *resolved* value it used, alongside the
#  shared recipe config above — not a second copy of a per-frame-varying knob)
# A frame's failure is recorded (status "failed" + error) and the roll continues;
# the process then exits non-zero. Determinism: same batch + same recipe ⇒
# byte-identical output per frame (each frame runs the same core as `convert`).

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

# Calibrate the roll-fixed display-white anchor `Dmax` the same way: point
# `--d-max-region` at a fully-exposed (near-opaque) reference frame — the
# light-struck roll leader — with the roll's Dmin as --film-base. `estimate`
# reduces that region's per-channel base-relative density D (= corrected density
# under default density-scale/offset) to one scalar (a gray-density
# reduction) and reports it in reusable forms: a --d-max flag and a `density`
# recipe fragment. The region is recorded as provenance (dmax_region), NOT as a
# re-read directive — the frozen recipe carries the scalar so the apply phase is
# deterministic. `Dmax` is roll-fixed like `Dmin` (see §7.2/§9).
nc estimate leader.tiff --film-base 0.553,0.271,0.159 --d-max-region 200,0,300,3600 --report json
# → { "film_base": { … },
#     "dmax": 1.6428, "dmax_region": [200, 0, 300, 3600],
#     "d_max_flag": "--d-max 1.6428",
#     "d_max_recipe": { "dmax": { "explicit": 1.6428 } }, … }
nc convert frame01.tiff -o frame01_pos.tiff --film-base 0.553,0.271,0.159 --d-max 1.6428
# …or paste d_max_recipe into roll-A.json's "density" section. With no reference
# frame, omit it: the default `density.dmax = fixed` nominal anchor still renders a
# viewable positive (darker frames stay faithfully darker).

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

Every conversion flag has a recipe key. Unchanged shipped stages use the paths
shown below (for example, `--output-hdr` ⇒ `output.hdr`). Reconstruction entries
show their **target** paths under the tagged `reconstruction` object; §8
separately records the currently shipped legacy `algorithm`/sibling-section
shape. Names are binding and unknown keys are rejected (`deny_unknown_fields`).

### Input / decode
- `--export-ir <path>` — write the IR plane to a separate file (HDRi only).
  Recipe key `input.export_ir`.
- Input color is resolved as **two independent axes** before Dmin/density — the
  transfer encoding and the measurement meaning — never a single combined
  assertion. Each is a mutually-exclusive assertion with its own recipe key; the
  two never conflict (they describe different facts), so each flag replaces only
  its own axis:
  - `--input-transfer <auto|linear>` ⇒ `input.transfer` (default `"auto"`) —
    how the samples are *encoded*. `linear` asserts a linear transfer (no
    inverse-transfer decoding); it does **not** prove scanner-device provenance.
  - `--input-meaning <auto|scanner-device|colorimetric>` ⇒ `input.meaning`
    (default `"auto"`) — what the pixel axes *are*. Only `scanner-device` (with a
    supported linear transfer) enters Dmin/density without a source→working
    transform. `colorimetric` is recognized but **unsupported** (no inverse
    transfer/reconstruction path exists yet); `convert` rejects it even when
    asserted (an override cannot make it supported).
  - `auto` on either axis resolves from container evidence and **fails loudly in
    `convert`** when it stays ambiguous — nothing is silently labelled linear
    Rec.709 for lacking an ICC. `nc inspect` still reports the evidence so the
    file is diagnosable.
  Resolution and precedence (deterministic, `pipeline::input_semantics`):
  an explicit assertion outranks a descriptive tag which outranks the
  absence-of-evidence default; authoritative container structure (SilverFast
  HDR/HDRi raw mode) proves *both* a linear transfer and scanner-device meaning.
  Raw-mode provenance is **detected from SilverFast's XMP mode metadata** (TIFF
  tag 700), not assumed from "we decoded it" and not keyed on spoofable signals:
  the decoder accepts any 3-channel 16-bit chunky RGB TIFF, so a file is treated
  as SilverFast raw mode only when its XMP carries `Silverfast:Company =
  "LaserSoft Imaging"` **and** `Silverfast:HDRScan = Yes` (grounded in the real
  sample scans). The `Software` string and IR-plane presence are deliberately
  **not** provenance — a processed export keeps the `Software` tag, and a generic
  RGB16 + Gray16 multipage forges an IR-like plane; both are rejected. The XMP
  `Silverfast:Gamma` feeds the transfer axis (`Gamma ≈ 1` corroborates linear; a
  non-linear gamma on a raw-mode scan makes the transfer ambiguous). A gamma value
  that is **present but uninterpretable** (e.g. a locale-formatted `"2,2"`) is
  treated as ambiguous, **not** linear (transfer → `unknown`, with a decode
  warning naming the value) — nc does not guess the locale. A tag-700 packet that
  is present but yields **no recognizable SilverFast metadata** (malformed, or an
  unrecognized namespace/layout — e.g. a future scanner) emits a warning and
  establishes no provenance rather than silently dropping it. A **generic /
  colorimetric / processed RGB16 TIFF** (e.g. one carrying an sRGB ICC) therefore
  resolves `meaning: unknown` and is **rejected by `convert`** (exit 4, with an
  error suggesting `--input-transfer linear --input-meaning scanner-device` if the
  user knows it is a raw scan) — never silently converted as a raw negative.
  Gamma 1 establishes **only** the transfer axis (never raw-mode provenance or
  meaning). An explicit assertion that contradicts authoritative structure (e.g.
  `--input-meaning colorimetric` on a raw-mode scanner scan) **fails** rather than
  overriding it (exit 2); an explicit assertion that overrides a descriptive tag
  is honored and records the displaced tag. Every explicit override is reported
  with its CLI-vs-recipe provenance. A descriptive gamma tag that contradicts
  raw-mode linear semantics makes the transfer **ambiguous** (rejected by
  `convert`, explained by `inspect`) unless an explicit `--input-transfer linear`
  resolves it.
  - An **embedded scanner ICC** (TIFF tag 34675) is retained and reported as
    device-characterization metadata (a safe class/space/PCS/version/description
    summary — never a raw byte dump), but it is **never applied before density**
    and does not by itself establish either axis.
  - IR remains measurement data — never color-transformed, bit-identical before
    and after input resolution.
  - The removed combined key `input.color` (and the `--assume-linear` flag) is
    rejected with a pinned migration error — it must never silently assert both
    axes.
  - `--input-profile <icc>` stays **rejected for normal conversion** (exit 4):
    input-side ICC application has no validated placement and is reserved for the
    deferred `scanner-profile-before-density-experiment`.
  - A SilverFast **positive-mode** scan (XMP `Silverfast:Negative = No`) is raw
    linear scanner data, so it passes the transfer/meaning gate — but converting
    it as a negative would be silently wrong, so `convert` **rejects it loudly**
    (exit 4) with a distinct "positive-mode not yet supported" message.
    Positive-mode support (and embedded-ICC handling) is a follow-up.
  `nc inspect`, the `convert` report, **and each `nc roll` frame report** expose
  the resolved `input_color`: both axes with per-axis evidence, whether an ICC is
  embedded plus the safe summary, and `transfer_decoded` (whether any
  inverse-transfer decoding was performed — always `false` in Step 1, which
  accepts only already-linear samples). `meaning` is always a flat string
  (`scanner-device` / `colorimetric` / `unknown`); the colorimetric detail rides
  in a sibling `meaning_reference` field so consumers can key `meaning` uniformly.
  In roll mode a shared recipe (or per-frame override) asserting the
  unconditionally-unsupported `input.meaning: colorimetric` is rejected up front
  (exit 4), before any frame is decoded.

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

### Reconstruction and density-curve select
- Target CLI: `--reconstruction simple|density` (default `density`).
- With density: `--density-curve exponential|sigmoid` (default `exponential`).
- Target recipe: `reconstruction.type`, then for density
  `reconstruction.density` and tagged `reconstruction.curve`, exactly as shown
  in §8. There are no sibling top-level density or curve sections.
- When the tagged reconstruction schema activates, the `--algorithm` flag and old
  `algorithm` recipe form will be rejected with a migration error rather than
  retained as aliases, and the old top-level `density`, `sigmoid`, and `simple`
  forms will be rejected at the same boundary. (Today's shipped CLI still
  **accepts** `--algorithm`; see §8 and the current-keys note below.)

> **Current shipped keys.** The released binary implements the legacy transitional
> schema (§8), **not** the target `reconstruction.*` paths in this section. Today a
> recipe selects the algorithm with a top-level `algorithm` key and configures it
> via top-level `density.{density_scale, density_gamma, dmax, shadow_balance,
> highlight_balance, balance_range}` and `sigmoid.{contrast, toe, shoulder}` (plus
> `simple`). Write recipes for today's binary against §8's legacy schema; the
> `reconstruction.*` paths below are the target migration.

### Density stage (`reconstruction = density`)
- `--density-scale R,G,B` ⇒ `reconstruction.density.scale` — per-channel
  density gain.
- `--density-offset R,G,B` ⇒ `reconstruction.density.offset` — per-channel
  density offset (orange-mask compensation).
- `--shadow-balance R,G,B` ⇒
  `reconstruction.density.shadow_balance`.
- `--highlight-balance R,G,B` ⇒
  `reconstruction.density.highlight_balance`.
- `--auto-balance-range` ⇒
  `reconstruction.density.balance_range = "auto"`;
  `--balance-range LO,HI` ⇒
  `reconstruction.density.balance_range = {"explicit": [lo, hi]}`.
- `--density-gamma <f>` ⇒ `reconstruction.curve.gamma`, valid only when the
  resolved curve type is `exponential`.
- `--sigmoid-contrast <f>`, `--sigmoid-toe <f>`, and
  `--sigmoid-shoulder <f>` ⇒ `reconstruction.curve.contrast`, `.toe`, and
  `.shoulder`, valid only when the resolved curve type is `sigmoid`.
- `Dmax` is owned by the tagged curve. Its target recipe key is
  `reconstruction.curve.dmax` (default `"fixed"`; see §7.2). It is a
  **roll-fixed
  calibration** like `Dmin`. The four flags conflict (passing more than one is a
  usage error); whichever is given replaces a recipe's `dmax`:
  - `--fixed-d-max` (default) ⇒ `"fixed"` — the roll-fixed **nominal** anchor: a
    scene-independent corrected-density placement (`Dmax ≈ 2.0`, in density units),
    reused across the roll. The default when no reference / per-stock value has
    been calibrated.
  - `--d-max <d>` ⇒ `{ "explicit": <d> }` — the roll-fixed **calibrated** anchor: a
    scalar measured once from a fully-exposed reference frame
    (`estimate --d-max-region`, §8) or a known per-stock constant, reused across
    the roll exactly like an explicit `--film-base`. This is the form a roll recipe
    freezes.
  - `--auto-d-max` ⇒ `"auto"` — measure the anchor **per frame** from the
    corrected-density distribution (a high percentile). This is **per-frame
    exposure normalization** (it brightens underexposed frames and breaks
    roll-to-roll consistency), an opt-in grading convenience *demoted* from the
    former default — not the faithful-conversion default.
  - `--no-d-max` ⇒ `"none"` — choose unity exponential placement (base `1.0`,
    detail above), reproducing the current pre-anchor film rendering
    bit-for-bit. This is an unanchored film rendering, not a physical-scene
    recovery. Current `--output-hdr` remains a rendered float TIFF,
    not the target `film-master`.
    The sigmoid curve is anchored on `[0, Dmax]`, so `sigmoid` + `none` is a
    usage error (§7.3).
- Regional (shadow/highlight) color balance (see §7.2). "Shadow"/"highlight"
  name the **positive's** tone regions (low/high corrected density); a positive
  value brightens that channel in its region. Defaults `[0, 0, 0]` are identity
  — the default output is bit-exact with the unbalanced render:
  - `--shadow-balance R,G,B` — per-channel density offset applied in the
    positive's shadows.
  - `--highlight-balance R,G,B` — per-channel density offset applied in the
    positive's highlights.
  - Tone-ramp anchors — a single mutually-exclusive choice, recipe key
    `reconstruction.density.balance_range` (default `"auto"`); the two flags conflict, and
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
- Both density curves share the `reconstruction.density` object, including
  regional balance. The curve variants have disjoint fields. After recipe/CLI
  merge, `--density-gamma` with sigmoid, any sigmoid flag with exponential,
  any curve/Dmax flag with simple, `curve.dmax = "none"` with sigmoid, and
  `--density-curve` with simple fail as usage errors. Customized gamma under
  sigmoid is never ignored.

### Sigmoid density curve (`reconstruction = density`, `density-curve = sigmoid`)
The stage-3 S-curve knobs (§7.3); density correction and the later print/display
render use `reconstruction.density` and `print`. The exact recipe keys are
`reconstruction.curve.contrast`, `.toe`, and `.shoulder`:
- `--sigmoid-contrast <f>` — mid-density slope of the curve in log-output space
  (the `--density-gamma` analogue). Finite and in `(0, 50]`; default `1.0`. The
  upper cap guards against an extreme slope collapsing the S-curve into a hard
  black/white threshold that silently destroys tonal detail (use the
  `exponential` density curve for genuinely extreme contrast).
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
- Target shared render contract adds `--linear-range LOW,HIGH` /
  `print.linear_range` (default `[0,1]`) for the exact affine
  `(x-low)/(high-low)` black/range placement (black/white-point placement). It is distinct from the existing
  density print `black_point` and from SDR/HDR reference white in nits.
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

  Current auto estimation requires `--algorithm density` or `--algorithm
  sigmoid`; `--auto-wb` with `simple` remains a usage error (exit 2). The target
  refactor gives simple the same **explicit** downstream WB slot, but does not
  imply that density-based auto estimators support simple without a separately
  specified generalization.
- `--highlight-compress <f>` — highlight roll-off amount.

### Simple algorithm
- Current legacy controls: `--invert-white-balance R,G,B` and
  `--clip-low <f>` / `--clip-high <f>`. They currently run before the output
  transform. In the target film-preserving pipeline they are not simple
  reconstruction parameters. Preset migration accepts them as warned aliases:
  inversion WB maps to explicit `print.white_balance`, while clip endpoints map
  to `print.linear_range` / atomic `--linear-range LOW,HIGH`. Resolve the recipe
  pair or `[0,1]` first. The atomic flag replaces both endpoints and conflicts
  with either legacy range flag; otherwise `--clip-low`/`--clip-high`
  independently override their endpoint. Validate finite `low < high` after
  merge, warn, and report endpoint provenance. New recipes/reports emit only
  replacement names, and named presets apply them only after NC film RGB mapping.
  `film-master` rejects every final non-default range regardless of source;
  legacy flags may reset recipe endpoints to `[0,1]`. Legacy no-preset TIFF calls keep current ordering until
  migration. Aliases preserve parameter values, not bit-identical output through
  the working-space matrix; target activation warns, and
  `conversion-versioning` owns the prospective behavioral-version bump when the
  changed preset/default pixels activate.

### Output / encode (current terminal stage; target stages 5–6)
- `-o, --output <path>` (required)
- `--output-hdr` — current transitional flag: write a 32-bit float unclamped
  **rendered** TIFF after the current print controls, not the future `film-master`
  or Rec.2100 display HDR; without it output is 16-bit integer. Recipe key
  `output.hdr` (bool, default `false`). It will be replaced by the explicit
  `film-master`/display-HDR preset model.
- `--output-sdr` — force the default 16-bit integer output, overriding a
  recipe's `output.hdr = true` (the flags-win escape hatch; an absent
  presence flag never clobbers a recipe value). Conflicts with
  `--output-hdr`; passing both is a usage error.
- `--output-profile <srgb|prophoto|acescg|path-to-icc>` (default is depth-aware:
  `srgb` for the 16-bit default, `acescg` for `--output-hdr`)
- `--bigtiff auto|on|off` (default `auto`)

Planned `output-presets` replaces the depth-only default with `gain-map-hdr` and
explicit `display-p3`, `compatibility`, `film-master`, `hdr-pq`, `hdr-hlg`, and
`custom` policies. `film-master` encodes NC film RGB v1 mapped unclamped linear
ACEScg before print/display controls and rejects frame-local auto Dmax.
Exponential accepts supported `none` or fixed/roll placement, sigmoid uses fixed
Dmax for curve shaping, and simple has none. Named display presets use the SDR/HDR render
branches. The output path stays required; its suffix must match the
resolved container and is never rewritten silently. A named non-`custom` preset
conflicts with legacy output-selection flags (`--output-hdr`, `--output-sdr`,
`--output-profile`, `--bigtiff`); legacy flag-only invocations retain their
transitional TIFF behavior. After merge, `film-master` also rejects every
non-default effective WB, exposure, black, white, highlight, SDR/HDR tone, gamut, or
display-transfer control from recipe or CLI; it never ignores one. Flags may
explicitly reset recipe values to defaults, and the resolved report records the
effective values/provenance and that no display transfer ran. A selected
`correction.profile` is not a downstream creative/print/display control:
corrected output remains `film-master` and records mandatory profile
identity/hash/scope provenance. Those preset names are not accepted by the current
CLI yet. `nc roll` migration is part of the preset task: automatic names use
each resolved container suffix, manifest/per-frame overrides validate
independently, and each sidecar derives from its final image path. The single roll
report remains on stdout or the explicit `--report-file`; that destination is
collision-checked against all inputs, outputs, and sidecars before writing.

### Global
- `--params <json>`, `--dump-params <json>`
- `--report json|none`, `--report-file <path>`
- `--strict` — promote report warnings (clipping, non-finite samples, grid
  disagreement, …) to a failing exit (see §11); on `convert`, `roll`, and `estimate`
- `-v/--verbose`, `--quiet`

**Roll (batch, `nc roll` only — orchestration flags, NOT recipe keys).** `nc roll`
converts many frames from one shared `--params` recipe; it reuses the exact recipe
shape above and adds no new conversion knobs. Its flags are operational (like
`--report`): `--out-dir <dir>` (per-frame outputs `<stem>_positive.tiff`),
positional `inputs` (files and directories — a directory is expanded to its
`.tif`/`.tiff` files, sorted; shell globs are expanded by the shell, not by nc)
**or** `--frames <manifest.json>` (explicit per-frame `input`/`output`/partial-recipe
`params` overrides, deep-merged onto the shared recipe for that frame only).
The shipped schema stores roll-fixed Dmax at `density.dmax`; the target schema
stores it at `reconstruction.curve.dmax`. The shared recipe configuration
appears once at the top of the roll report; each frame additionally reports
the *resolved* base / `Dmax` it used — a redundant echo when the recipe pins an
explicit base, but meaningful under an `auto`/`region` base that resolves per
frame. Frame-local knobs are the per-frame `params` overrides. Roll-fixed
invariant violations are **loud, `--strict`-promotable warnings** rather than
hard errors, so a deliberate best-effort batch remains usable: (1) a shared
`film_base.source` other than `explicit` re-estimates Dmin per frame; (2) the
active Dmax key set to `auto` measures Dmax per frame; (3) a per-frame override
that sets `film_base` changes that frame's Dmin; and (4) a per-frame override
that changes the active Dmax key changes that frame's placement. Shared `fixed`, explicit, or
`none` Dmax policies remain deterministic across the roll. `input.export_ir` is rejected in roll mode (one
path, N frames). Determinism: same batch + same recipe ⇒ byte-identical output per
frame.

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

**Input-semantic resolution** (§9 Input/decode) maps to these codes: an
ambiguous or unsupported input (transfer/meaning that cannot reach a supported
linear + scanner-device resolution — including an asserted `colorimetric`
meaning) is an **unsupported** input, exit 4; an explicit assertion that
contradicts authoritative container structure, the removed combined `input.color`
recipe key, and the deprecated `--assume-linear` flag are **usage** errors, exit
2; `--input-profile` (reserved, not applied) is unsupported, exit 4. `nc inspect`
never fails on ambiguity — it reports the per-axis evidence so the file stays
diagnosable.

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
6. **Roll-level presets & batch mode.** The **batch-apply scaffold has shipped**
   as `nc roll` (task `roll-conversion`): convert N frames from one shared, frozen
   recipe (`--params`), with per-frame overrides via a `--frames` manifest and a
   roll-level JSON report (per-frame status + the shared recipe once). See §8.
   What remains: the auto-cascade that *generates* the shared recipe (detect the
   film base / `Dmax` once for the roll and emit the frozen recipe roll applies) —
   the dependent `base-acquisition-planner` task — plus first-class named presets
   (film stock, neutral spots).
7. **Optional color-correction QA harness.** Target-based fitting and ΔE2000 /
   SSIM regression testing against controlled negatives may support explicitly
   selected correction profiles. It is not part of the default film-preserving
   pipeline and is distinct from blindly applying a conventional positive-scanner
   ICC before density.
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
13. **Roll workflow & base-acquisition planner** (extends item 6). The
    deterministic **apply** half has shipped as `nc roll`: it converts a batch
    from one shared recipe, supports per-frame manifest overrides, and emits one
    roll report while preserving the single-frame conversion core. Roll-fixed
    parameters (`Dmin`, `Dmax`) versus frame-local print controls remain the
    model. The open `base-acquisition-planner` owns the automatic **plan** half:
    an acquisition cascade (unexposed reference → rebate region → `--auto-base`
    → cross-frame agreement → drop-to-single; content estimation only on explicit
    opt-in) emits the frozen recipe and provenance that `nc roll` replays.
    Tracked: shipped `roll-conversion`; open `base-acquisition-planner` and
    `film-base-content-fallback`.
14. **Roll-fixed `Dmax` from a fully-exposed reference frame.** *(Implemented —
    `dmax-reference`.)* Supersedes the frame-local `auto` default: `Dmax` is a
    film+scanner calibration reused per roll like `Dmin`. The default
    `density.dmax = fixed` resolves reference → per-stock constant → a nominal
    corrected-density anchor (`Dmax ≈ 2.0`, in density units — *not* base
    transmission plus a range); a value measured once from the light-struck leader
    (near-opaque in RGB, the max-density endpoint — always available) via
    `estimate --d-max-region` is frozen as `{ "explicit": <d> }`. `--auto-d-max`
    (per-frame exposure normalization) is demoted to opt-in. This changes the
    default render, which is a `pipeline_version` bump — a **deferred** obligation:
    there is no `pipeline_version` code constant yet (`conversion-versioning`,
    item 16, is unshipped), so when it lands this default must be labeled
    `pipeline_version 1` (the v0→v1 boundary for the density default). In the
    replacement pipeline, Dmax belongs to the selected density curve: scalar
    placement for exponential and curve shaping for sigmoid. SDR/HDR rendering
    owns display reference white.
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
18. **Input data semantics and validation — DELIVERED** (`input-data-semantics`;
    the contract is now §4 + §9). Transfer encoding is resolved separately from
    whether values are scanner-device measurements, colorimetric RGB, or unknown,
    with evidence reported for both; Gamma 1 establishes only a linear transfer and
    does not prove raw-mode provenance, and an embedded ICC does not authorize
    mixing channels before Dmin. Only inputs with positive raw-mode evidence and a
    linear transfer stay in scanner coordinates through density; ambiguity fails
    loudly and IR remains untouched. The combined `--assume-linear` / `input.color`
    assertion was replaced by the independent `input.transfer` / `input.meaning`
    CLI/recipe axes (the old forms now emit a pinned migration error); explicit
    overrides have deterministic evidence precedence and reported provenance but
    cannot make unsupported colorimetric/encoded negatives valid.
19. **Conventional scanner ICC before density — deferred experiment.** Compare
    `scanner RGB → Dmin/log density` against applying the same scanner ICC to image
    and Dmin first, using only a defined linear destination and controlled target
    error. This alternative workflow neither blocks nor substitutes for the
    normal film-preserving mapping or optional correction profiles.
    `--input-profile` stays rejected for
    normal conversion unless this experiment validates a supported path. Tracked:
    `scanner-profile-before-density-experiment`.
20. **Film-preserving reconstruction and working pipeline.** Replace the
    algorithm enum with tagged simple/density reconstruction and tagged
    exponential/sigmoid density curves. Preserve current exponential pixels and
    the exact sigmoid equation; move Dmax ownership into the curve. Every path
    returns typed `FilmRgbImage`. NC film RGB v1 intentionally interprets those
    values as linear Rec.709/D65 and transforms/adapts them into typed linear
    ACEScg/D60. This is NC's film-rendering intent, not physical scene recovery.
    `film-master` encodes the unclamped ACEScg film rendering directly; named
    display branches apply shared WB → exposure → black/range placement before
    SDR/HDR-specific tone, gamut, and transfer work. Legacy no-preset TIFF
    ordering remains during migration. Optional correction profiles may
    explicitly neutralize declared scanner/film/development/lens behavior, but
    block no output task. Tracked:
    `negative-reconstruction-density-curves`,
    `film-rgb-working-space`,
    `film-master-render-pipeline`,
    `optional-color-correction-profiles`.
21. **Display P3 SDR output.** The SDR renderer solely maps ACEScg into rendered
    linear Display P3, including adaptation/gamut policy. The P3 output task then
    applies the piecewise sRGB TRC and attaches a deterministic ICC v4 profile:
    the encoding is D65, while ICC PCS/media white is D50 with Bradford-adapted
    colorants and the required chromatic-adaptation tag. It performs no second
    ACEScg transform. Tracked: `display-p3-output`,
    `sdr-display-rendering`.
22. **Display HDR rendering and format spike.** First decide the encoder,
    HEIC/JPEG container details, ISO HDR and ISO 21496-1 metadata, licensing,
    reference white/headroom, and cross-platform support. Then render linear
    ACEScg into BT.2020 Rec.2100 PQ (primary still path) or explicit HLG with
    documented tone and gamut mapping. Rec.2100 is an output encoding, not the
    density or internal working space. Tracked: `hdr-output-spike`,
    `hdr-display-rendering`.
23. **ISO gain-map HDR and output presets.** Combine a valid Display P3 SDR base
    with the HDR rendition and ISO 21496-1 metadata, initially targeting HEIC and
    requiring both Apple and non-Apple verification. Public terminology is
    standards-neutral (`gain-map-hdr`, not a platform brand). Both renditions
    share the identical mapped/adjusted film source; gain ratios are derived in
    the standard-required common linear color domain, never by dividing encoded
    P3 and PQ/BT.2020 values. Once verified,
    `gain-map-hdr` becomes the default; explicit presets retain Display P3 SDR,
    sRGB compatibility, linear ACEScg film master, PQ, HLG, and custom workflows.
    `nc roll` naming/manifests migrate with presets so suffixes derive from each
    resolved container and per-image sidecars derive from final image paths. One
    roll report remains on stdout or explicit `--report-file`, collision-checked
    against all batch inputs/outputs/sidecars. Core full-size TIFF/resource verification remains independently runnable;
    final gain-map/preset metadata, faithful film-rendering consistency, and
    cross-device behavior are a separate gate.
    Tracked: `gain-map-hdr-output`, `output-presets`,
    `display-output-acceptance`.

## 13. Open questions

All of the Step-1 open questions have since been resolved (kept here as a record):

- ~~Exact on-disk SilverFast HDRi tag/channel layout~~ — **resolved 2026-06**:
  reverse-engineered and verified against real sample files; documented in §4
  (separate full-resolution grayscale IR IFD, optional preview IFD, structural
  HDR/HDRi detection).
- ~~Which wide-gamut space to use for the target `film-master` output~~ —
  **resolved**: **linear ACEScg**. The current `--output-hdr` path can tag its
  rendered float values as ACEScg but is not that master. The target branch lands
  after NC film RGB v1 mapping and before print/display controls; it is not
  physical scene recovery or Rec.2100 display HDR. See §5.
- ~~Whether the embedded TIFF metadata should carry the full recipe~~ —
  **resolved**: the recipe lives in the sidecar JSON only (paired by name with
  the output); the TIFF embeds just the ICC profile. See §5.
