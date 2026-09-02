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
- **Tagged reconstruction** architecture (shipped): `simple` (channel
  inversion — a debugging baseline; **not** the B&W path, which runs through
  `density`) or `density` (density-domain
  reconstruction, Kodak Cineon / darktable `negadoctor` style — the default),
  where density owns a tagged **sigmoid** (H&D-style S-curve, the default since
  `pipeline_version` 2) or **exponential** (the straight line) density curve.
- All conversion parameters controllable via CLI flags and/or a JSON recipe file.
- Write **TIFF** output, selectable as **16-bit integer** or transitional
  **32-bit rendered float** via a flag.
- Auto-estimate film base (`Dmin`) from the unexposed border, with full CLI override.
- JSON report output (estimated parameters, warnings) and JSON recipe load/dump.

### Out of scope (Step 1) — see §12 Roadmap

- IR-based dust/scratch removal (follow-up task).
- Additional reconstruction/curve models beyond those listed above (follow-up
  tasks; the sigmoid / explicit H&D curve has since shipped post-MVP as the
  tagged `sigmoid` density curve, §7.3).
- Black & white film support, incl. plain 16-bit RAW scans (follow-up task).
- Camera RAW (Bayer/X-Trans) input, DNG processing.
- ML/AI assistance of any kind (auto-crop, neutral-patch detection, inpainting).
- Batch/roll preset management UI, GUI, scanner ICC profiling workflow.
- Output formats other than TIFF in Step 1. The post-MVP display-output roadmap
  now targets ISO gain-map HDR in JPEG, single-rendition HDR in AVIF, lossless
  linear/PQ/HLG HDR interchange in TIFF, plus PNG/EXR as appropriate.

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
| **`D′` at the reconstruction→curve handoff** | the same corrected density `D′` (row above), named at the point it is passed to the selected density-to-positive curve | **denser** negative — a **brighter** scene | density units — `D′`'s range as defined in the row above (no re-clamping at the boundary) | reconstruction→curve handoff inside `density::reconstruct` |
| **NC film RGB v1** (`FilmRgbImage`) | intentional positive film rendering from simple inversion or the exponential/sigmoid density curve; interpreted consistently as linear Rec.709/D65 | **brighter** positive — a **brighter** rendered scene | curve-defined and unclamped `f32` | `algo::FilmRgbImage`, `algo::reconstruct` (shipped typed reconstruction output) |
| **ACEScg film rendering** (`AcesCgImage`) | NC film RGB v1 transformed/adapted into linear ACEScg/D60; preserves film/lens/development/scanner character and is not physical scene recovery | **brighter** rendered value | unclamped `f32`; nominal diffuse white is workflow-defined | `pipeline::working_space` mapper (implemented; wired into the `film-master` render branch, not the legacy TIFF path) |
| **rendered display positive** | linear ACEScg film rendering after shared white balance/exposure/black/range placement, then output-specific highlight/reference-white/tone and destination gamut mapping | **brighter** rendered value | unclamped until the chosen display policy requires limiting | `pipeline::sdr` / `pipeline::hdr` |
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

- **Current implemented containers:** TIFF (BigTIFF when size requires 64-bit
  offsets), gain-map JPEG for the explicit `gain-map-hdr` / `ultra-hdr-v1` presets,
  and 10-bit 4:4:4 AVIF for the explicit `hdr-pq` / `hdr-hlg` presets.
- **Current implemented preset selection:**
  `--output-preset <gain-map-hdr|ultra-hdr-v1|display-p3|compatibility|film-master|hdr-pq|hdr-hlg|hdr-linear-tiff|hdr-pq-tiff|hdr-hlg-tiff|legacy|custom>`
  / recipe key `output.preset` (**default `gain-map-hdr`** since `pipeline_version` 3).
  Exactly **twelve** names are
  accepted today — there is no planned-but-unaccepted tier left, so an unknown name
  always means a typo; the pre-release `scene-master` is still rejected as an
  unreleased-schema break (no alias). `legacy` and `custom` are the two **non-atomic**
  presets, staying compatible with the depth/profile/container selectors; every other
  name is atomic and resolves them itself. Since the default is `gain-map-hdr`,
  reaching the legacy TIFF path takes an explicit `--output-preset legacy` (or
  `custom`), and `nc convert -o out.tif` with no preset is a usage error naming the
  accepted suffixes.
- **Current implemented bit depth:**
  - default (no preset) → the `gain-map-hdr` JPEG; `output.depth` is not consulted.
    The 16-bit integer TIFF is `--output-preset legacy` / `custom` / `display-p3` /
    `compatibility`.
  - `--out-depth f32` (legacy / `custom` only) → 32-bit float TIFF with unclamped values
    **after the current print-render controls**. This is a transitional rendered
    float TIFF, neither `film-master` nor a Rec.2100 display-HDR image, and it is
    **never** an alias for the preset.
  - `--output-preset film-master` → 32-bit float TIFF, unclamped, taken directly
    from the NC film RGB v1 mapped linear ACEScg with the ACEScg profile embedded
    and **no** working→output transform, print control, or display rendering. The
    depth follows from the preset, not from `output.depth` (which must stay at its
    default under a named preset).
  - `--output-preset gain-map-hdr` / `ultra-hdr-v1` → fixed 8-bit Display P3 SDR
    primary JPEG plus a half-resolution grayscale gain-map JPEG. Identical pixels;
    they differ only in the metadata attached — `gain-map-hdr` carries ISO 21496-1
    segments in both images *and* the legacy Ultra HDR v1 XMP/MPF, `ultra-hdr-v1`
    only the latter. Apple platforms read the ISO dialect alone, so `gain-map-hdr`
    is the form that decodes as HDR there.
  - `--output-preset hdr-pq` / `hdr-hlg` → fixed 10-bit, full-range, 4:4:4 AVIF
    (AV1 High Profile). The depth follows from the preset; `output.depth` must stay
    at its default. Both require an `.avif` output path and are `convert`-only.
  - `--output-preset hdr-pq-tiff` / `hdr-hlg-tiff` → fixed **16-bit unsigned**
    TIFF holding full-range Rec.2100 PQ/HLG code values. The depth follows from the
    preset (for the primary *and* the optional IR plane); `output.depth` must stay at
    its default. Both require a `.tif`/`.tiff` path and are `convert`-only.
  - `--output-preset hdr-linear-tiff` → 32-bit float TIFF holding the HDR
    renderer's **pre-transfer display-linear BT.2020/D65** samples verbatim, with a
    synthesized linear-BT.2020 ICC profile. Bit-exact: nothing is clamped,
    normalized, or transfer-encoded, so samples run from black past the 203 cd/m²
    reference white (`1.0`) to the 1000 cd/m² peak (≈4.926108) — verified on a real
    18.66 MP scan whose maximum sample is exactly that headroom with 7.92% of
    samples above reference white. The depth follows from the preset; `output.depth`
    must stay at its default (`--out-depth f32` is the *print*-rendered float TIFF in
    the selected output space, a different image). Requires a `.tif`/`.tiff` path
    and is `convert`-only. Because the ICC PCS stops at the media white, the
    profile cannot express those luminance semantics: the report's
    `hdr_linear_tiff` block and the sidecar are authoritative for reference white,
    peak, and headroom, and the profile must never be claimed to carry them.
- **Current legacy-TIFF color selection:** the output color space is a CLI
  option (`--output-profile`). The default depends on output depth:
  - 16-bit (default) output → **sRGB** (standard, display-ready positive).
  - float (`--out-depth f32`) output → provisionally transformed/tagged **linear
    ACEScg**, but still after the current print renderer; it is not the target
    `film-master`. (`prophoto`, `display-p3`, and user ICC files are also
    accepted; `display-p3` is a wide-gamut SDR destination — P3/D65 with the
    piecewise sRGB TRC and a synthesized ICC v4 profile. As shipped it, like every
    output space, transforms *from* the linear Rec.709 working space: a lossless
    Rec.709→P3 primaries remap — Rec.709 ⊂ P3, no gamut compression — plus the sRGB
    TRC. The shipped `ultra-hdr-v1` path instead takes rendered-linear Display P3
    from the SDR stage and applies the matching transfer encoding without a second
    gamut transform. The shipped `display-p3` *preset* now takes that same boundary,
    which is what distinguishes it from this profile axis.)
  Either default can be overridden explicitly. Output is tagged with the embedded
  ICC profile for the chosen space.
- **Working-space intent:** the current implementation treats reconstructed
  scanner/film RGB as linear Rec.709 before its output transform. The replacement
  pipeline standardizes that existing interpretation as **NC film RGB v1** and
  transforms/adapts it into linear ACEScg/D60 for every named output. This
  preserves NC's intentional film rendering; it is not a provisional claim about
  physical scene color. Measured correction is an optional explicit profile.
- **Product default (shipped, `pipeline_version` 3):** `gain-map-hdr` — a standards-neutral,
  backward-compatible Display P3 JPEG rendition plus an ISO 21496-1 gain map and
  Android Ultra HDR v1 compatibility metadata. Aware readers reconstruct the HDR
  rendition and unaware readers show the SDR base. This is not Apple-only:
  ISO 21496-1 is the public model and non-Apple support is an acceptance
  requirement. HEIC gain maps are deferred pending a portable final-standard
  encoder and approved HEVC licensing/packaging policy.
- **Presets** (every one is accepted by the current CLI; there is no
  planned-but-unaccepted tier left):
  - `legacy` — the transitional TIFF path, now reached only by naming it;
  - `film-master` — unclamped 32-bit float linear ACEScg TIFF preserving NC's film rendering;
  - `ultra-hdr-v1` — explicit legacy Display P3 gain-map JPEG (reads as plain
    SDR on Apple platforms, which ignore the legacy XMP dialect);
  - `gain-map-hdr` — the same file carrying **both** dialects; backward-compatible
    display HDR, and **the product default** since `pipeline_version` 3;
  - `display-p3` — 16-bit losslessly stored wide-gamut SDR TIFF;
  - `compatibility` — 16-bit losslessly stored sRGB SDR TIFF;
  - `hdr-pq` — single-rendition BT.2020 / Rec.2100 PQ AVIF;
  - `hdr-hlg` — explicit HLG/broadcast-oriented AVIF;
  - `hdr-linear-tiff` — 32-bit float display-linear BT.2020 HDR interchange TIFF;
  - `hdr-pq-tiff` — losslessly stored 16-bit BT.2020 / Rec.2100 PQ TIFF;
  - `hdr-hlg-tiff` — losslessly stored 16-bit BT.2020 / Rec.2100 HLG TIFF;
  - `custom` — expert-selected format/profile policy: the legacy TIFF pipeline,
    explicitly named, and the one named preset that accepts the depth/profile/
    container selectors.
  A preset resolves container, bit depth, primaries/profile, transfer function,
  tone/gamut mapping, and metadata together. The old `--output-hdr` name was
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
  preset. **As shipped**, that rejection runs on the *resolved* config
  (`cli::validate`), so a value is rejected identically whether it came from a
  recipe, a flag, or a removed simple-control migration — and a flag that resets a
  recipe value back to its documented default is legitimately accepted, which is how
  a roll recipe carrying print controls can still be re-exported as a master. The
  atomicity rule uses the **same resolved-value semantics** for the three selectors: a
  general presence rule cannot be made to behave identically for a recipe key (only the
  resolved value can) and would contradict the escape hatch above. So `--bigtiff auto`
  and an explicit `"hdr": false` are accepted next to a named preset — `auto` means
  "decide for me" and `hdr: false` is the `serde` default, so neither asserts anything
  the preset does not already do — while `--out-depth f32`, a non-default
  `output.output_profile`, and `--bigtiff on` are rejected from either provenance.
  Being value-based, the rule is gated on "is this an *atomic* preset", so every future
  preset inherits it — `custom` is the sole exclusion, since accepting these selectors
  explicitly is what it is for.
  **The `--out-depth` flag is the one deliberate presence check** (`cli`, before
  `validate`), and the reason it does not follow the value rule is `--out-depth u16`:
  that resolves the documented *default*, so a value rule cannot see it, yet it
  **forces** 16-bit integer output an atomic preset cannot produce — honouring the
  preset would silently discard an explicit request. (`--out-depth f32` is caught by
  the value rule as an ordinary non-default.) The recipe side needs no mirror:
  `"depth": "u16"` is the serde default and asserts nothing, so no recipe form is left
  behaving differently. Exit 2, not a warning. `film-master` additionally rejects the **other** frame-local
  measurement, for the same cross-frame reason as auto Dmax: an `auto`
  `reconstruction.density.balance_range` *when a balance is actually applied*, because
  the tone-ramp anchors would then be measured from each frame's own density
  percentiles. An `auto` range with equal shadow and highlight balances — including the
  neutral default — consults no range and stays accepted.
  The resolved-branch record lands in the report as `output_render` (§8). The
  suffix table in `cli::required_extensions` is now **complete**: `.jpg`/`.jpeg`
  for `gain-map-hdr` and `ultra-hdr-v1`, `.avif` for `hdr-pq`/`hdr-hlg`, and `.tif`/`.tiff` for
  `hdr-linear-tiff`, `hdr-pq-tiff`, `hdr-hlg-tiff`, `display-p3`,
  `compatibility`, **`film-master` and `legacy`**. The last two previously pinned
  no row, so `nc convert -o out.jpg` wrote a TIFF named `.jpg` with exit 0 and no
  warning — the silently-misnamed-file mistake every newer preset was already
  guarded against.

  Since every preset now states a rule, an **extensionless** output path
  (`-o positive`) is a usage error too — a decision, not a side effect: a file with
  no extension misleads about its contents exactly as a wrongly-named one does, and
  nc is unreleased, so the strict rule costs nothing now. `nc convert -o positive`
  previously exited 0. The **diagnosis varies on where the preset came from, not on
  which preset it is**: that stopped being derivable from the value once the default
  became a *named* preset, since `gain-map-hdr` now arrives both ways. A preset the
  user selected — by `--output-preset` **or** by `output.preset` in a `--params`
  recipe — is blamed by name. With neither, the message names the default explicitly
  ("with no `--output-preset`, nc writes `gain-map-hdr`") rather than pointing at a
  flag that is not in the command line. Both halves matter: reporting a
  recipe-selected `legacy` as the no-preset default states something false and sends
  the reader hunting for a default that does not exist.

  **Roll capability is a separate axis**, not derived from that table. It used to
  be ("pins a suffix" ⇒ `convert`-only), and that inference died when the table
  was completed — deriving it would refuse every preset and leave `nc roll` with
  nothing to run. **Every preset is roll-capable now:** roll derives
  `<stem>_positive.<ext>` from the frame's own resolved preset, and an explicit
  manifest `output` goes through the same suffix rule `convert` uses. The derived
  spelling comes from `cli::derived_extension`, deliberately *not* from the head of
  the accepted list — that lists `tif` before `tiff`, so taking it would silently
  rename every existing roll output. A test asserts the derived spelling is always
  a member of the preset's accepted set, which is why derived names are not
  re-checked.
- **Metadata:** the effective parameter set (recipe) and key estimated values are
  written to a **sidecar JSON** next to the output (paired by name). The sidecar is
  the two-key envelope `{ "meta": {…identity…}, "params": {…recipe…} }`, where
  `params` is exactly the `--dump-params` document and `meta` is the run's
  conversion identity (§9) **plus any preset-specific output contract block**.
  Identity sits *beside* the recipe, never inside it:
  every recipe struct is `deny_unknown_fields`, so a bare `pipeline_version` key
  would make every new sidecar fail to reload through `--params`.

  `meta` is therefore **not identity-only**. The coded and linear HDR TIFF presets
  add an `hdr_linear_tiff` / `hdr_coded_tiff` block inside it, carrying the same
  reference-white / peak / headroom (and, for the coded presets, transfer and CICP)
  values as the corresponding report block (§8) — and for those presets the sidecar
  is *authoritative* for that contract, because the ICC profile cannot express it.
  A consumer that parses only identity fields out of `meta` will miss them, which is
  precisely the `--report none` case where the sidecar is the only place they appear.
  They live inside `meta` rather than as a third sibling key because the envelope's
  read side is `deny_unknown_fields`: a sibling of `meta`/`params` would make the
  sidecar unloadable through `--params`. `--params`
  accepts **both** the envelope and a bare recipe object (a hand-written recipe,
  `--dump-params` output, or a pre-envelope sidecar); `meta` is read as provenance
  and never applied. Current TIFF output embeds the ICC profile of the chosen
  space; future HDR containers carry the profile/CICP and gain/headroom metadata
  required by their preset. The recipe is deliberately *not* embedded in the
  image container (resolved, §13).

## 6. Pipeline architecture

The conversion is a linear sequence of pure-function stages. Each stage has its
own parameter struct and can be unit-tested in isolation.

The diagram below depicts the **target / replacement** architecture (tagged
reconstruction, NC film RGB v1 working-space mapping, and the film-master /
display-render split). The **current shipped** pipeline implements decode,
input-semantics resolution, and film-base / `Dmin` estimation before preset
dispatch. Dispatch then selects a container-specific stage entrypoint; that
entrypoint invokes tagged `algo::reconstruct` (including the selected density
curve) and owns the resulting `FilmRgbImage` boundary:

- `legacy` (and `custom`, which resolves the same branch) — `pipeline::stages::render`
  owns `reconstruct → FilmRgbImage → finish_print → output ICC transform → TIFF`.
  Since the default became `gain-map-hdr` this branch is reached only by naming one
  of those two presets. The legacy print render sits *after* the typed boundary but
  before the working→output ICC transform. Its pixels are frozen until
  the output-preset migration, pinned by two complementary tests:
  `pipeline::stages::golden` freezes the **pre-colour-transform** values bit-for-bit
  (it calls `reconstruct_and_print` directly, so it never crosses the preset
  `match`), and
  `stages::legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence`
  pins that the `legacy` branch of `render` is still exactly that sequence composed
  with `color::to_output`.
- `film-master` — `pipeline::stages::render` owns
  `reconstruct → FilmRgbImage → NC film RGB v1 → linear ACEScg → TIFF`; the
  unclamped f32 buffer carries the ACEScg profile and receives no output transform.
- `ultra-hdr-v1` — `pipeline::stages::render_display_source` owns
  `reconstruct → FilmRgbImage → NC film RGB v1 → linear ACEScg → shared print
  controls`. The orchestrator then feeds that one adjusted source to the SDR and
  HDR renderers and packages their half-resolution luminance gain map with the
  Display P3 base as an explicitly legacy XMP/MPF JPEG (no ISO claim).
- `hdr-pq` / `hdr-hlg` — the same `pipeline::stages::render_display_source` shared
  source, then **one** rendition: `pipeline::hdr` renders display-linear BT.2020 and
  encodes Rec.2100 PQ or HLG in place, and `io::avif` codes it as 10-bit full-range
  4:4:4 AV1 (High Profile) inside an nc-written MIAF container. `av1C` is filled from
  the encoded sequence header, and the `MA1A` brand is written only inside the AVIF
  v1.2 Advanced Profile's published limits — otherwise the file is a valid
  general-brand AVIF and the report says which limit it exceeded.
- `hdr-linear-tiff` — the same shared source and the same `pipeline::hdr` linear
  render, stopped one stage earlier: `render_linear` runs, `encode_transfer` does
  **not**, and `io::encode::encode_hdr_linear` writes the display-linear BT.2020
  samples verbatim as 32-bit float with the linear-BT.2020 profile from
  `color::hdr_linear_bt2020_icc`. Its peak memory phase is the render rather
  than the encode — f32 needs no quantization buffer and the `tiff` writer streams
  strips instead of assembling a container in memory. It is not alone in that:
  `hdr-pq-tiff`/`hdr-hlg-tiff` and the SDR presets `display-p3`/`compatibility`
  peak at render for the same reason, and `ultra-hdr-v1` does because its four
  simultaneous display buffers outweigh its encode set. Which phase peaks is per profile and pinned by
  `pipeline::memory`'s `which_phase_peaks_is_per_profile_and_measured_not_assumed`.
- `hdr-pq-tiff` / `hdr-hlg-tiff` — the **same rendition** `hdr-pq` / `hdr-hlg`
  produce (identical `render_linear` + `encode_transfer`), quantized once to
  full-range 16-bit codes by `io::encode::encode_hdr_coded` and stored exactly, with
  the `cicp`-tagged A2B profiles from `color::hdr_pq_tiff_icc` /
  `hdr_hlg_tiff_icc`. Because a transfer and a container are independent choices,
  `convert_frame` dispatches on the **preset** exhaustively rather than on
  `hdr::transfer_for`, which two presets legitimately share.

Stage 5b's **shared print controls** (`render_split::display_source`) and both
pure display renderers are implemented and unit-tested: `pipeline::sdr`
produces rendered-linear Display P3/sRGB, while `pipeline::hdr` produces
display-linear BT.2020 plus in-place Rec.2100 PQ/HLG encoding. **Every** display
preset consumes the shared stage and accepts a non-default `print.linear_range` —
`gain-map-hdr` (the default) and `ultra-hdr-v1`, `display-p3` / `compatibility`,
`hdr-pq` / `hdr-hlg`, `hdr-linear-tiff`, and `hdr-pq-tiff` / `hdr-hlg-tiff`. The rule
is keyed on the *branch*, not on a preset list: only the legacy TIFF path
(`legacy` / `custom`) rejects that control, because its frozen ordering does not
apply it, and `film-master` rejects it under its own bypass rule.
See the "Architecture" section of `CLAUDE.md` for the current-vs-target framing.

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
parameters and a tagged `sigmoid` (default) or `exponential` curve. It preserves
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
rather than a physical scene-linear recovery. Legacy-preset TIFF calls retain
their current ordering until preset activation.

Within the display branch, SDR and HDR share the same resolved linear white
balance, exposure, and black/range placement. They diverge only for output-specific
highlight/reference-white, tone, gamut, and transfer rendering, so a gain-map
pair starts from one consistently adjusted source without forcing SDR highlight
compression onto the HDR rendition.
The shared adjustment order is WB → exposure → the existing black-point
operation → `print.linear_range` affine placement → branch-specific work;
`linear_range` defaults to `[0,1]` and requires finite `low < high`.
Mechanically (as shipped in `pipeline::render_split`), the controls are resolved
**once** into a `ResolvedPrintControls` — an `auto` white balance becomes concrete
gains there, so the branches cannot re-estimate and drift — applied once, and then
*borrowed* by both branches from one `SharedDisplaySource`. "SDR and HDR receive
the identical adjusted source" is therefore structural, not a convention the two
renderers have to remember. `display_tone` and `highlight_compress` are deliberately **not applied**
by the shared stage: they resolve once into a single tone value that each named
display branch scales into its own domain, so SDR and HDR can use different
display-domain knees — or no curve at all — without drifting in their common
adjustments, and without being able to disagree about which curve ran.

The resolved SDR policy is deterministic and display-referred. It transforms
AP1/D60 ACEScg to the selected D65 destination (Display P3 or sRGB) with pinned
AP1→XYZ, Bradford D60→D65, and XYZ→destination matrices; no installed ICC or CMM
participates in rendering. Adjusted linear `1.0` is the binding **203 cd/m²
reference white**. Under the default `print.display_tone = shoulder`, named SDR
applies a C¹ Hermite shoulder: the baseline
(`highlight_compress = 0`) starts at `0.75`, reaches `1.0` with zero slope, and
plateaus at `1.0`; positive highlight compression moves the start earlier using
the bounded resolution
`shoulder_start = 0.5 + 0.25 / (1 + highlight_compress)`, so even an extreme
finite value cannot move it below `0.5` and flatten the whole tonal range.
Under `display_tone = none` no tone curve runs at all and the reconstruction alone
places every tone; the range check below is then the operative bound, so a sample
above reference white is a loud pixel-specific error rather than a clip. The
renderer then maps out-of-gamut color to the RGB-cube boundary with one common
chroma scale around the same-luminance neutral axis. This preserves the neutral
axis and chroma direction instead of independently clipping channels. Finite
input that produces non-finite matrix/tone/gamut arithmetic is a loud conversion
error naming the pixel; the renderer never substitutes black or white. Its typed
result owns finite non-negative `[0,1]` **pre-transfer**, destination-linear
pixels together with the resolved gamut metadata. The radial intersection is
calculated in binary64 and sets its limiting channel to the exact computed cube
boundary; this is the gamut mapping itself, not a terminal per-channel clamp.
Any final sample outside the cube is a loud pixel-specific error. A separate
destination stage derives the matching Display P3 or sRGB profile from that
metadata, applies only the piecewise sRGB transfer curve, and returns the
metadata with the encoded pixels. The gain-map path borrows the typed
pre-transfer pixels.

The resolved HDR policy uses the same tone selection and control amount but a
distinct reference-white-relative domain. Adjusted linear `1.0` remains the binding
**203 cd/m² reference white**, the fixed peak is `1000/203 = 4.926108...`, and
the normalized knee position is
`0.5 + 0.25 / (1 + highlight_compress)`. The HDR shoulder therefore starts at
`1 + (1000/203 - 1) * knee_position`, reaches `1000/203` with zero slope, and
plateaus there. A positive amount moves the HDR knee earlier while reference
white and peak stay fixed. `display_tone = none` skips this shoulder too — the knee
sits at ≈3.94, far above anything a reconstruction bounded at reference white
produces, so on such a source the HDR rendition is *pixel-identical* either way and
only the reported policy differs; the peak bound stays the operative check. After the shoulder, same-luminance radial gamut
mapping produces display-linear BT.2020. Single-rendition output transfer-encodes
that typed value as PQ/HLG; gain-map construction must first transform it to the
common linear Display P3 domain used by the SDR rendition.

### 6.1 IR channel handling (Step 1)

The IR plane (when present) is decoded and carried alongside RGB. With one
exception it is **not consumed** by any conversion stage in Step 1: when the scan
is declared chromogenic (`--film-type chromogenic`) **and** carries an IR plane,
**film-base estimation (stage 2) consumes it** — the opaque scanner holder reads
dark in IR while all film (base, rebate, picture, leader) reads bright, so
holder-occluded spans are excluded from the auto rebate/`Dmin` search (the
`ir-holder-detection` feature, adjacent to the roadmap's IR item 1; §9 film base).
Silver B&W (IR-opaque) and the `unknown` default keep this path off, and a scan
with no IR plane always falls back to RGB-only detection. Otherwise the IR plane
is only carried, and can be exported with `--export-ir <path>` for inspection or
downstream tooling. The broader dust-removal stage that *consumes* the IR mask for
defect inpainting is a deliberate follow-up (§12).

*Why IR is powerful and why we defer it:* the color dye image is transparent to
infrared while physical defects (dust, scratches, hair) are opaque to it, so the
IR channel is a near-clean defect map. Acting on it requires a separate
mask + inpainting stage with its own parameters, and it does **not** work for
traditional silver B&W film (silver blocks IR like dust) or reliably for
Kodachrome. So Step 1 preserves the data cheaply now and adds the consuming stage
later.

## 7. Reconstruction and density curves

The shipped implementation selects the tagged reconstruction with
`--reconstruction simple|density`; density then selects
`--density-curve exponential|sigmoid`. Every reconstruction path returns the
typed `FilmRgbImage` boundary (`algo::reconstruct`), so only the working-space
mapper (`pipeline::working_space`) can construct `AcesCgImage`. It is wired into
the `film-master` render branch; the legacy TIFF path does not cross it. The pre-reconstruction
`--algorithm simple|density|sigmoid` selector (a boxed `Converter` returning an
untyped `LinearImage`) is **removed** — the flag and the old recipe forms are
rejected with a migration error (nc is unreleased; no aliases).

### 7.1 `simple` — inversion baseline

Channel inversion plus white balance / border neutralization. Cheap, predictable,
useful for B&W negatives and as a debugging reference. Not a strong endpoint for
color negatives (ignores density behavior and the orange mask).

The pre-reconstruction converter ran `positive = 1 - scan/Dmin` →
`invert_white_balance` gain per channel → the `clip_low`/`clip_high` affine
black/white remap. In the shipped pipeline, stage 3 ends at unclamped
`U_c = 1 - scan_c/Dmin_c` and returns `FilmRgbImage`. `simple` has no Dmax.
Inversion WB and clip remapping move after the ACEScg boundary to the downstream
shared WB/black/range-placement contract. **As shipped**, both replacement homes
now exist — explicit `print.white_balance` and `print.linear_range` /
`--linear-range LOW,HIGH` — and every display preset consumes them (see §6). The
legacy TIFF path still rejects a non-default `print.linear_range`, and `film-master`
bypasses print controls. The old flags and
`simple.*` recipe keys remain **rejected with a migration error** that names the
concrete replacement; convenient alias acceptance is deferred to the complete
output-preset migration so its warnings, provenance, roll handling, and version
boundary land together. That migration resolves `--invert-white-balance` to explicit
`print.white_balance` and clip endpoints to
`print.linear_range = [low, high]` / atomic `--linear-range LOW,HIGH`. Range
merge starts from the recipe pair or `[0,1]`; the atomic flag replaces both
endpoints and conflicts with either legacy flag. Without it, `--clip-low` and
`--clip-high` independently override their endpoint, after which finite
`low < high` is validated. Reports warn and record each endpoint's provenance;
new recipes/reports emit only replacement names. Named presets apply the values
only after NC film RGB mapping. Legacy-preset TIFF calls keep current ordering
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

**Curve-stage Dmax.** In the shipped curve stage, `Dmax` is the corrected
density of scene white and the exponential expression `10^(gamma·(D'−Dmax))`
guarantees that `D' = Dmax` maps to `1.0` under the `white-at-dmax` placement. The
base maps to `10^(−gamma·Dmax) ≈ 0`; with `none` *and that placement*, the
exponential curve reproduces its unanchored output bit-for-bit (base `1.0`, detail
above). `none` resolves the reference to `0`, so any other placement still derives a
non-zero anchor from the slope and renders differently. Current
`--out-depth f32` is still a rendered float TIFF, not the target `film-master`
branch.

In the shipped schema, exponential retains Dmax as scalar placement and
sigmoid retains it as a nonlinear curve-shaping input. Display reference white
belongs to the later SDR/HDR render. Fixed/roll Dmax preserves cross-frame
exposure; frame-local auto Dmax is exposure normalization and is rejected for
`film-master`.

`Dmax` is a **roll-fixed calibration** — a film + scanner property reused across
the roll, like the `Dmin` base — **not** a per-frame measurement. Anchoring each
frame's densest pixel to display white *normalizes exposure per frame* (it
brightens underexposed frames and forces an overcast scene's grey to white), which
conflicts with NC's "convert faithfully, grade in Lightroom" purpose. The
default `reconstruction.curve.dmax = "fixed"` therefore resolves a **fixed** anchor in the order
**measured reference → per-stock constant → nominal**: a value measured once from
a fully-exposed reference frame (§8, `estimate --d-max-region`) or a known
per-stock constant is carried as `{ "explicit": <d> }`; with no calibration a
**nominal** corrected-density anchor (`Dmax = 1.3`, a scene-independent placement
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
anchors come from `reconstruction.density.balance_range`: `auto` (default)
measures robust
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
A = anchor(R, contrast)                        anchor density: the D' rendering to 1.0 (below)
t = contrast·(D' − A)                          the straight line, in log10-output space
F = −contrast·A                                paper-black floor (the line's value at D' = 0)
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
anchor), so under neutral print params the default u16 encode **reports no clipped
highlights** (the later print/display render — exposure/gains — can still lift samples
above `1.0`).

That is a statement in ℝ, and it must not be read as "no highlight information is
lost". The approach to white is asymptotic, so in `f32` the gap closes at a finite
density — at the shipped defaults with the nominal 1.3 anchor, the rendered value is
exactly `1.0f32` from just above `D′ ≈ 3.1` onward. The encoder counts a clip as
`v > 1.0` **strictly**, so those densities all encode to 65535 with
`loss.clipped_high = 0` and `--strict` green. The region is narrow (near-opaque
negative), but part of "0% clipped" is loss that moved from a counted category into
an uncounted one; `docs/reports/render-defaults-v2.md` records the measurement.

With `shoulder = 0` there is no roll-off and
highlights follow the (toe-shaped) line, which can exceed `1.0` like `density`.
The toe holds shadows to the paper-black floor `≈ 10^(−contrast·A)` (exact
when `shoulder = 0`; the shoulder nudges it imperceptibly lower otherwise).
`toe`/`shoulder` are knee widths in log10 density units; `contrast` is the
mid-density slope in log-output space. The curve is strictly monotonic; with
`toe = shoulder = 0` **and `anchor = "white-at-dmax"`** both knees are skipped
and it reproduces `density`'s step 3 **bit-for-bit** (`contrast` standing in for
`density_gamma`), so `density` remains the debuggable straight-line reference.
Under the default mid-grey placement the line is the same shape but offset,
since `A ≠ Dmax` — the equivalence is a property of that anchoring rule, not of
zero knees alone. `contrast` is capped (§9) — an
extreme slope would collapse the curve into a hard threshold that silently
destroys tonal detail.

Because both the white knee and the black floor derive from the anchor, the
S-curve **requires** one: `curve.dmax = "none"` with `curve.type = "sigmoid"` is
a usage error (exit 2); unity placement is supported only by the exponential
curve. `curve.dmax` resolves by the same fixed, explicit/reference-derived, or
opt-in auto policy as the exponential curve.

**`curve.dmax` supplies the roll's *reference* density `R`; `curve.anchor` decides
which tone that reference places.** Placement is **shared by both curves** — it is
orthogonal to curve shape — so `curve.anchor` is a key of the exponential variant as
well as the sigmoid, and one `--anchor-*` CLI family serves both. Pinning display
white at a reference measured from a fully-exposed leader put midtones 2.5–3.6 stops
too dark once the contrast became photographic: steepening the slope pivots the line
about whatever point is pinned, so pinning the top end drags everything below it
down. The four rules are:

| `curve.anchor` | anchor `A` | meaning |
|---|---|---|
| `{"mid-at-dmax-fraction": f}` (sigmoid default, `f = 0.5`) | `f·R + 0.745/contrast` | mid-grey (18%) renders at fraction `f` of the reference; display white lands `0.745/contrast` above it, and densities beyond it are compressed by the shoulder rather than clipped |
| `"white-at-dmax"` (exponential default) | `R` | display white renders *at* the reference — the pre-2026-08 rule, retained as an explicit diagnostic and as the exponential's debuggable straight line |
| `{"black-at-base": floor}` | `−log10(floor)/contrast` | the **film base** renders to `floor`, a linear output value against the 203-nit reference white; display white falls where the slope puts it |
| `{"mid-at-base-offset": d}` | `d + 0.745/contrast` | mid-grey renders at density `d` *above the film base* rather than at a fraction of the reference |

`0.745 = −log10(0.18)` is mid-grey's fixed distance below white on the *output*
axis. Every rule is a **roll-level** placement, never derived from frame content.

**A `Dmax` policy is only frame-local if the placement reads it.** `black-at-base` and
`mid-at-base-offset` discard the resolved reference, so `dmax = "auto"` under either is
computed and thrown away: the render is deterministic and identical across frames. Every
`Dmax`-policy gate therefore tests the *placement*, not the `DmaxSource` — `film-master`
accepts `auto` under a reference-free rule (its rejection exists to keep frame-local
adaptation out of a master, and there is none), and `roll` does not call such a recipe
"not frozen". Gating on the source alone hard-rejected a valid master and made `--strict`
fail a consistent roll.

**A curve-type switch resets `curve.anchor`; it does not carry it.** Both variants
accept the key, but only `curve.dmax` is shared in *meaning* — a measured reference
density is curve-independent, so `--density-curve` and a `roll` per-frame `type`
override both carry it. Placement is not: each curve's default is chosen for that curve
(`white-at-dmax` is the exponential's diagnostic straight line and the sigmoid's
warned-against setting), so carrying it would make one curve's default the other's
accident. A switch that discards a **non-default** placement emits a loud,
`--strict`-promotable warning naming it — silence would be a different tonal rule with
nothing in the report to show it, and on `roll` the override that causes it names only
`type`, so no key-presence check can see it.

**The last two are reference-free, and that is their point.** `R` is a leader
density — film saturation rather than diffuse white — and two rolls of one stock have
measured 0.295 apart while their bases agreed to 0.0005. Since `dA/dR` is the
reference's coefficient, that spread reaches the render multiplied by it: 1.96 stops
at `white-at-dmax`, 0.98 at `mid-at-dmax-fraction` with `f = 0.5`, and **zero** for
the base-derived pair. The base needs no separate measurement — step 1 divides it out,
so the film base *is* `D′ = 0` by construction, modulo `density.offset` and any
regional balance. On the sigmoid, `black-at-base` places the straight-line-extrapolated
base rather than the toe-rendered one, the same approximation `mid-at-dmax-fraction`
already makes there.

**Archived recipes are warned about, not silently reinterpreted.** `curve.anchor` did not
exist before 2026-08-03, so a recipe frozen earlier omits it and picks up this build's
default placement — a different render even with `contrast`, `toe`, `shoulder` and `dmax`
all pinned. Loading a recipe that selects `sigmoid` without an `anchor` therefore emits a
loud, `--strict`-promotable warning naming `"white-at-dmax"` as the way to reproduce the
old placement. An `exponential` recipe without an `anchor` warns too, in the
moved-defaults form it shares with a floating `gamma`/`dmax`: this build always writes the
key, so its absence marks a file some other build wrote. The exponential's default
placement is behaviour-preserving *today*, which is why it is the milder warning and not
the placement-moved one. This is deliberately *not* a `reconstruction.schema_version` bump: that
constant versions the schema **shape** and is checked for exact equality, so bumping it
would reject every archived recipe outright, including the majority that select the
exponential curve and are unaffected. Preserving per-version semantics instead (a
historical default table, which would also have to cover `contrast` and `shoulder`) is
policy owned by `core/conversion-versioning`. Mid-placement is also half as sensitive to a
reference error (`dA/dR = f`), which matters because a leader's density records
how the roll was loaded, not the film. `f` must be in `(0, 1]` (§9). Those two rules
keep `curve.dmax` as the normalisation reference, so the roll-fixed invariant
holds; the base-derived pair never reads it, which is what removes their
roll-to-roll term entirely. Gamma exists only in the exponential variant. Supplying
`--density-gamma` while the resolved curve is sigmoid is an invalid combination
after merge (exit 2), never a warning or ignored value (the pre-reconstruction
implementation's ignored-gamma warning is gone).
On the frozen legacy path, `--highlight-compress` remains the existing
linear-space above-`1.0` soft clip after exposure/WB: `0` disables that legacy
operation, and with the sigmoid shoulder plus neutral print parameters it simply
never engages because nothing exceeds `1.0`. Named SDR and HDR deliberately
give the same control different target semantics: display tone mapping is
mandatory, `0` selects each branch's baseline Hermite shoulder, and positive
values request progressively earlier/stronger additional roll-off. SDR resolves
the bounded `[0.5, 0.75]` knee in `[0,1]`; HDR resolves the same normalized knee
position across `[1, 1000/203]`, as described in §6. Product activation and the
associated conversion-version boundary remain owned by `output/presets` and
`conversion-versioning`; until then the legacy behavior is unchanged.

### Shipped and target interfaces (sketch)

```rust
// Shipped stage boundary. Fields are private; only the reconstruction stage
// constructs a FilmRgbImage, so no raw scan/density buffer can impersonate
// film RGB downstream.
pub struct FilmRgbImage { /* private */ }

pub fn reconstruct(
    image: &LinearImage,
    base: &FilmBase,
    config: &Reconstruction,
) -> Result<(FilmRgbImage, ReconstructionReport)>;

// Shipped legacy bridge: applies the stage-4 print controls before the output
// color transform (named presets later move them after the ACEScg boundary).
pub fn finish_print(
    film: FilmRgbImage,
    config: &Reconstruction,
    print: &PrintParams,
) -> Result<(LinearImage, Option<[f32; 3]>)>;

// Working-space boundary (film-rgb-working-space, `pipeline::working_space`):
// implemented and wired into the `film-master` render branch. Named output code
// accepts AcesCgImage rather than FilmRgbImage. The mapping is a total pure matrix
// transform (no failure mode — non-finite inputs pass through, counted later at
// encode), so it returns the value directly rather than a Result.
pub struct AcesCgImage { /* private; constructor module-private to the mapper */ }

pub fn map_nc_film_rgb_v1(image: FilmRgbImage) -> AcesCgImage;

// Named-output split (film-master-render-pipeline, `pipeline::render_split`):
// every entry point accepts AcesCgImage and nothing else. `film_master` is a pure
// unwrap (the bypass IS the master); the display half resolves the shared print
// controls once, and both branches then borrow the ONE AdjustedAcesCgImage the
// resulting SharedDisplaySource owns (`&shared.source`) — so "SDR and HDR receive
// the identical adjusted source" is structural: there is no per-branch buffer to
// diverge. AdjustedAcesCgImage and ResolvedPrintControls both have private fields
// and module-private constructors, so a display renderer cannot be handed a buffer
// that skipped the shared stage, cannot be handed the master (a LinearImage), and
// cannot receive controls that skipped the gain/exposure/range validation.
// DisplayBranch is the seam a consumer matches on to select its renderer; it has no
// influence on the shared stage, which is why no function here takes one.
pub fn film_master(aces: AcesCgImage) -> LinearImage;
pub fn resolve_shared_controls(aces: &AcesCgImage, print: &PrintParams)
    -> Result<ResolvedPrintControls>;   // the only ResolvedPrintControls producer
pub fn apply_shared_controls(aces: AcesCgImage, controls: &ResolvedPrintControls)
    -> AdjustedAcesCgImage;
pub fn display_source(aces: AcesCgImage, print: &PrintParams)
    -> Result<SharedDisplaySource>;
```

## 8. CLI design

A single binary (working name `nc`) with subcommands. The agent-facing surface is
optimized for scripting: flags for everything, JSON in/out, stable exit codes,
no interactive prompts.

### Subcommands

| Command | Purpose |
|---|---|
| `nc convert` | The main pipeline: negative file → positive image in the resolved preset's container (a gain-map JPEG by default; a TIFF or AVIF under the presets that say so). |
| `nc roll` | Convert a batch of frames from one shared, frozen recipe (the batch-**apply** scaffold). Per-frame outputs into `--out-dir` + a roll-level JSON report. Single-frame `convert` is unchanged; roll is additive. |
| `nc inspect` | Read a scan and emit a JSON report of format, channels, bit depth, candidate rebate regions (coordinates + spread, ready for `--base-region`), suggested `Dmin`. No output image. |
| `nc estimate` | Run only film-base/`Dmin` estimation; emit JSON with reuse-ready `--film-base` / recipe-fragment forms. `--grid` adds 5-cell agreement-checked sampling for blank reference frames. `--d-max-region` additionally measures the roll-fixed display-white anchor `Dmax` from a fully-exposed reference frame, emitting reuse-ready `--d-max` / `reconstruction.curve.dmax` forms. |
| `nc params`  | Print the full default/effective parameter set as JSON (for discovery and recipe scaffolding). The scaffold is a **template to edit, not a runnable recipe**: `film_base.source` has no default, so it prints as `null` and `convert`/`roll` reject it until you state a base. |

### Recipes (JSON in/out)

- `--params recipe.json` — load a full parameter set from JSON.
- `--dump-params out.json` — write the effective parameters (defaults + overrides)
  to JSON. Individual `--flag` overrides take precedence over the loaded recipe,
  so an agent can load a roll recipe and tweak one value per frame.

The shipped recipe is grouped into `reconstruction`, `input`, `film_base`,
`print`, and `output`. The algorithm selection is exactly one tagged
`reconstruction` object; the removed legacy forms (top-level `algorithm` and the
sibling `density`/`sigmoid`/`simple` sections) are rejected at recipe load with
a migration error — they are not aliases. These are the complete reconstruction
shapes (other stage objects are omitted here):

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
      "type": "sigmoid",
      "contrast": 2.0686874,
      "toe": 0.2,
      "shoulder": 0.6,
      "dmax": "fixed",
      "anchor": {"mid-at-dmax-fraction": 0.5}
    }
  }
}
```

That first density example is the **resolved default document** as of
`pipeline_version` 2 — copying it reproduces the shipped render. The exponential
curve is still a first-class variant, selected explicitly (here with a custom
density block and a calibrated anchor, to show the other fields too):

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
      "type": "exponential",
      "gamma": 2.0,
      "dmax": {"explicit": 1.29},
      "anchor": "white-at-dmax"
    }
  }
}
```

`reconstruction.schema_version` is exactly `1`. Partial input may omit it and
defaults to 1; resolved recipes always emit it. `curve.dmax` accepts
`"fixed"`, `"auto"`, `"none"`, or
`{"explicit": <density>}`; `"none"` is valid only for exponential. `curve.anchor`
(**both curves**) accepts `"white-at-dmax"`, `{"mid-at-dmax-fraction": <f>}` with `f`
in `(0, 1]`, `{"black-at-base": <floor>}` with `floor` in `(0, 1)`, or
`{"mid-at-base-offset": <d>}` with `d > 0`. It defaults to
`{"mid-at-dmax-fraction": 0.5}` under sigmoid and `"white-at-dmax"` under exponential
(§7.3). Omitted
density fields take the displayed defaults. Partial input may omit
`reconstruction.curve`, which selects the default curve (sigmoid) with its defaults; every
resolved recipe/report emits exactly one tagged curve. Partial objects are
otherwise permitted. Unknown fields are rejected at every level.

`print.linear_range` **has shipped** (with the shared display stage), so simple's
WB/range adjustments already have their replacement homes under `print`; what target
preset migration still adds is the *alias acceptance*, not the keys. Legacy
`simple.invert_white_balance`, `simple.clip_low`, and `simple.clip_high` are to be
accepted solely as warned input aliases during the preset migration described in
§9; they are not part of `reconstruction` and are currently rejected with a
migration error (as are the flags). The replacements now have an explicit display
consumer in `ultra-hdr-v1`, but alias acceptance remains deliberately deferred to
the complete `output/presets` migration so help, warnings, recipe provenance, roll
handling, and the behavioral-version boundary land together. Legacy-preset TIFF
calls retain their current pixel ordering until migration.

### Target: recipe composition and the calibrate/profile split

> **This subsection describes the target, not the shipped surface** — like the
> "Target replacement architecture" block in `TASKS.md`. Owned by
> `core/recipe-composition`, `core/profile-authoring`,
> `core/base-acquisition-planner` and `core/value-domain-terminology`. Everything
> above this heading is what ships today.

**Two kinds of configuration, distinguished by lifetime.** The shipped recipe
conflates them, which is why a "frozen" recipe is neither reusable nor frozen:

| | Scope | Origin | Reused |
|---|---|---|---|
| **pipeline profile** — reconstruction, curve shape, print controls, output policy | a look | chosen | across many rolls |
| **roll calibration** — `film_base`, `dmax` | one roll | measured from film | never |

**The measurements move into their own section**, so the split is structural
rather than a convention about which keys go in which file. `dmax` leaves
`reconstruction.curve` — where it sits today only because it was a parameter of
the exponential equation — and joins the film base:

```json
{
  "calibration": {
    "film_base": {"explicit": [0.163, 0.080, 0.0377]},
    "dmax": {"explicit": 1.276}
  }
}
```

`curve.anchor` **stays** in the curve: the anchor is the *rule* for what the
reference places, which is part of the look. Only the measurement leaves. A
pipeline profile is then "a recipe with no `calibration` section", and a
calibration is "a recipe with nothing else". The name `dmax` is kept — it
accurately names the maximum density; the historic confusion was its *role*,
which `anchor` now carries explicitly.

**Composition is layered, over one schema.** `--params` is repeatable and accepts
`-` for stdin. Later layers win, and individual flags still win over all of them:

```text
defaults  <  --params A  <  --params B  <  …  <  individual flags
```

`roll` gains the same per-knob override flags `convert` has, so a one-off roll
needs no file at all. Per-frame overrides stay in the `--frames` manifest.

**The workflow.** Freezing no longer runs a conversion:

```sh
nc inspect scan.tif                                       # optional: what is this file
nc calibrate --unexposed blank.tif --leader exposed.tif              --out roll-cal.jsonc                          # measure the roll, once
nc profile --density-curve sigmoid --sigmoid-contrast 2.4            --output-preset display-p3 --out my-look.jsonc  # author a look, no image
nc roll frames/*.tif --out-dir positives/         --params my-look.jsonc --params roll-cal.jsonc     # apply
```

Both `--unexposed` and `--leader` are independently optional: either alone
resolves its own half and leaves the other at its default. Agents can skip the
files entirely — the report stays on stdout, so
`nc calibrate … | jq .calibration | nc roll … --params -` composes.

**Authored files are JSONC** (JSON plus comments). It is a superset, so every
existing recipe, sidecar and `--params` file stays valid, the tagged enums the
schema leans on keep working, and the machine contracts — report on stdout, the
output sidecar — remain plain JSON. Comments are **generated from the schema, not
preserved**: serde round-trips discard them, so nc writes an annotated file once
and never rewrites a user's file in place.

**Renames and removals.** `nc estimate` becomes **`nc calibrate`** (it resolves a
roll, not one value) and `nc params` becomes **`nc profile`** (it authors a
reusable look, not a parameter dump). `--dump-params` is **deleted** rather than
aliased: it is byte-identical to the sidecar every conversion already writes, and
it captures none of the measured values, so a "frozen" recipe produced by it still
re-measures per frame. `--grid` retires separately with
`film-base/tiling-uniformity-validator`.

### Reports & determinism

- `--report json` — emit a machine-readable result (estimated values, clip
  warnings, timings, output path) to stdout or `--report-file`.
- `--seed <n>` — fix any stochastic step (none in Step 1, reserved).
- Stable, documented **exit codes** (see §11).

**Conversion identity (`identity`, every report).** Three independent layers that
make an output attributable, all **operational** metadata in the same class as
`--report` and the telemetry flags: no CLI flag, no recipe key, and never a
changed output pixel.

```json
{
  "identity": {
    "nc_version": "0.1.0",
    "git_commit": "0d05c800c092",
    "git_dirty": true,
    "pipeline_version": 2,
    "target": "aarch64-apple-darwin",
    "params_hash": "3575c9feb5d42b2b"
  }
}
```

- `nc_version` / `git_commit` / `git_dirty` / `target` — **build identity**: which
  binary. Captured by `build.rs`; `git_commit` and `git_dirty` are **omitted**
  (never the string `"unknown"`) when the build tree had no usable git, so a
  source-tarball build degrades honestly instead of claiming a clean checkout.
  `git_dirty: true` means the commit alone does not identify the source.
- `pipeline_version` — the **behavioral** version, an integer **independent of
  semver** that bumps *only* when **default** conversion behavior changes. `0` is
  the Step-1 baseline in `docs/reports/v0-baseline.md`; `1`
  **collapses every default change since that baseline into one label**:
  `dmax-reference` replaced the per-frame anchor with the roll-fixed nominal
  `Dmax = 2.0` **density**, `auto-base-redesign` replaced the auto film-base
  detector, and `input-semantics` added stage-1b transfer/meaning resolution. (The
  v0 baseline report measured its numbers with an *explicit* `--film-base`, so those
  numbers stay comparable; the *default* render crossed three boundaries with only
  one label available to record them.) `2` is current: the nominal anchor moved to
  `1.3`, the default curve to the mid-grey-anchored sigmoid, and the exponential's
  own gamma to `2.0` — measured in `docs/reports/render-defaults-v2.md`, where v1
  clipped between 0% and 4.9% of a real frame's samples and v2 clips none of any of
  the four measured. That report also records why the clip counter alone overstates
  the win. This is the axis a version comparison is keyed on.
- **What the drift gate does and does not cover.** A golden drift test
  (`version::PIPELINE_FINGERPRINTS`) pairs each version with three fingerprints —
  the default **render** (the curated per-pixel vectors in
  `pipeline::stages::golden`), the default **film-base estimate** (stage 2, `auto`
  over the frozen scan in `pipeline::film_base::golden`, because the render
  fingerprint is handed a hardcoded base and the recipe fingerprint sees only
  `null` — `film_base.source` has no default, so the base fingerprint names `auto`
  explicitly), and the default **recipe values**. Change a default in those
  stages and the test fails until the version and the fingerprints are updated
  together. It does **not** cover decode, stage-1b input semantics, the lcms2 output
  transform or embedded ICC bytes (excluded deliberately — both differ by target, so
  no cross-platform hash of them exists), encode/quantization, the non-default
  film-base sources, or the auto detector's behavior on *real* scan geometry. A
  change confined to those can move default output with every test green;
  `scripts/real-scan-verify/` and `nctool compare` are the tools for that half.
- `params_hash` — a stable 64-bit FNV-1a hash of the canonical resolved-recipe
  JSON: **the exact bytes `--dump-params` writes**, so an agent can reproduce it
  (`nc convert --dump-params f.json …` then hash `f.json`) and identical
  configurations are detectable across frames and versions. The sidecar's `params`
  body is the same **document** but not the same bytes — nesting it under `params`
  indents every line two extra spaces — so reproduce the hash from a
  `--dump-params` file, and compare the sidecar as parsed JSON. Omitted for
  `inspect`/`estimate`, which resolve no full recipe. `nc roll` stamps one
  `identity` for the **shared** frozen recipe; a per-frame override changes that
  frame's own hash, which is why each roll frame also reports its own `identity`.

**Comparison basis (`output_stats`, `convert` and each roll frame).** Report-only,
alongside `loss`:

```json
{ "output_stats": { "mean": [0.512, 0.487, 0.443] } }
```

`mean` is the per-channel mean of the samples **as written**, and it is the numeric
basis `nctool compare` diffs across two builds (per-channel mean ΔRGB is the
difference of two runs' means, so no output is ever re-read or shipped). Its units
follow the output depth: the u16 path reports the quantized value scaled back to
`[0, 1]` (exact integer accumulation, so it is reproducible on every target given
identical pixels); the `--out-depth f32` path reports the verbatim, **unclamped**
float mean over the *finite* samples, so it may exceed `1.0` and one `NaN` cannot
swallow the statistic (`loss.non_finite` is where that fault is reported). A u16
mean and an f32 mean are therefore not comparable, and `compare` refuses to subtract
them. Only the mean is recorded; ΔE2000 / SSIM need real pixel access and belong to
§12 item 7's QA harness.

`nc --version` prints the same build identity (semver, `pipeline_version` with a
one-line description of its default render, commit with a `-dirty` marker — or
`(dirty unknown)` when cleanliness could not be read, target) so an output can be
attributed without running a conversion.

Replaying a recipe whose `meta.pipeline_version` differs from the running build's
is a **loud, `--strict`-promotable warning**: the parameters still apply, but the
default render changed underneath them, so the pixels will not match the original.

The convert report's `recipe` echoes the effective (resolved) config — the
sidecar's exact object — so `recipe.reconstruction` is the exact tagged object
above. Resolution diagnostics use this exact additional shape (unrelated report
fields omitted; `working_mapping` is added by the `film-rgb-working-space`
task):

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
      "curve": {
        "type": "sigmoid",
        "contrast": 2.0686874,
        "toe": 0.2,
        "shoulder": 0.6,
        "dmax": "fixed",
        "anchor": {"mid-at-dmax-fraction": 0.5}
      }
    }
  },
  "reconstruction_result": {
    "type": "density",
    "curve": {
      "type": "sigmoid",
      "dmax": {
        "policy": "fixed",
        "value": 1.3,
        "provenance": "default"
      },
      "anchor": {"mid-at-dmax-fraction": 0.5},
      "anchor_value": 1.01
    }
  },
  "working_mapping": "nc-film-rgb-v1",
  "output_render": {
    "preset": "legacy",
    "print_controls": true,
    "display_render": true,
    "encoding": "rendered-u16-tiff",
    "content": "print-rendered positive in the selected output colour space; …",
    "working_mapping": "nc-film-rgb-v1",
    "reconstruction_schema_version": 1
  }
}
```

`output_render` (`convert` only) records **which branch out of the ACEScg boundary
ran and what it applied**, so a consumer never has to re-derive it from the recipe.
`preset` is the resolved `output.preset`; `print_controls` says whether the
print/tone sub-stage ran *at all* (not whether its values were non-default — it is
`false` for `film-master`, and also for legacy `simple`, whose positive passes
through untouched); `display_render` says whether any tone/gamut/transfer operation
ran; `display_tone` is the resolved `print.display_tone` selector — the display
branch's tone policy, **absent** on `legacy` / `custom` and `film-master`, which have
no display tone stage at all. It rides in `output_render`, the one block *every* preset
emits, so the two SDR presets (which emit no per-preset block at all) and the AVIF pair
(whose `avif` block states no rendering policy) still say which tone curve ran;
`content` therefore states what the branch does *besides* tone and never names a curve. `encoding` is a stable identifier — one of
`rendered-u16-tiff` | `transitional-rendered-float-tiff` |
`unclamped-linear-acescg-float-tiff` | `legacy-ultra-hdr-v1-xmp-mpf-jpeg` |
`dual-dialect-gain-map-jpeg` |
`rec2100-pq-10bit-444-avif` | `rec2100-hlg-10bit-444-avif` |
`display-linear-bt2020-float-tiff` | `rec2100-pq-u16-tiff` |
`rec2100-hlg-u16-tiff` | `display-p3-u16-tiff` | `srgb-u16-tiff` — and the
legacy float name deliberately reads
as *rendered*, because `--out-depth f32` is never a film master. The three float names
are mutually exclusive on purpose: `unclamped-linear-acescg-float-tiff` is the
pre-display master, `display-linear-bt2020-float-tiff` is display-rendered but
pre-transfer, and `transitional-rendered-float-tiff` is print-rendered in the
selected output space. `content` states what
the pixels contain; for `film-master` it names the intentional
film/lens/development/scanner/reconstruction/curve rendering and explicitly
disclaims physical scene recovery. It names *which* anchor placement the run made:
the resolved roll-fixed `Dmax` one, a film-base-derived one that read no `Dmax`, or
none at all — `simple` has no anchor, and exponential `dmax = none` under
`white-at-dmax` places none. Keying that only on `curve.dmax` would give a stated
base-derived anchor and a genuinely unanchored run the same provenance, and let a
render that never read `Dmax` claim the roll-fixed placement. `working_mapping` is repeated inside the block
so a master's provenance is self-contained, and
`reconstruction_schema_version` mirrors `reconstruction.schema_version`. The
behavioral `pipeline_version` is a **separate** field owned by
`conversion-versioning`; this build stamps none, so it is absent rather than
guessed.

The block above is the **default** (sigmoid) shape: `curve.type` is `"sigmoid"` with
the resolved `dmax` object plus two
placement fields: `anchor` (the resolved placement *rule*, emitted for **both** curves
— they share the rule) and `anchor_value` (the **derived** anchor — the
corrected density this render mapped to `1.0`, hence the black floor at
`10^(−contrast·anchor_value)`). `dmax.value` is the *reference*; under the default
mid-grey placement the two differ, so reporting only the reference would document a
number the render did not use. Under `white-at-dmax` on either curve `anchor_value`
equals `dmax.value`; under any other rule it does not, and under the base-derived pair
it does not depend on `dmax.value` at all — which is precisely why `anchor` has to be
reported alongside it;
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
change. This bit-identical refactor preserves legacy-path pixels and does
not itself bump that field. Activating named presets and the new simple ordering
does change pixels and must cross a prospective, golden-tested
`pipeline_version` boundary owned by `conversion-versioning`. Recipe/report
round trips, fixtures, and migration errors pin the reconstruction schema.

**Memory preflight block.** Every command that decodes a scan reports what the
preflight decided before it allocated anything (§9 Global, `--max-memory`; §11
exit 6). Byte counts are exact; the per-phase fields are the accounted
full-frame buffers that are simultaneously live in that phase, and
`estimated_peak_bytes` is the peak of them plus a calibrated allowance for
allocator slack and fixed costs — the number the gate compares:

```json
{
  "memory": {
    "estimated_peak_bytes": 3396405248,
    "accounted_bytes": 2836684800,
    "decode_bytes": 1343692800,
    "film_base_bytes": 1811496960,
    "render_bytes": 2388787200,
    "encode_bytes": 2836684800,
    "budget_bytes": 4294967296,
    "budget_source": "default",
    "decision": "ok",
    "detected_total_ram_bytes": 51539607552
  }
}
```

(A 10368x7200 HDRi `convert` at `u16`, default budget, auto film base. The
`film_base_bytes` figure is the decoded image plus the three `f32` channel vectors
of the frame-interior rectangle the auto detector samples — ~69% of the frame; an
explicit `--film-base` samples nothing and the phase is the decoded image alone.)

`budget_source` is `default|flag`, `decision` is `ok|warn` (a rejected run emits
no report at all), and `detected_total_ram_bytes` is omitted when the platform
can't report it (which also disables the warn tier). `render_bytes`/`encode_bytes`
are `0` on `inspect`/`estimate`, which decode, sample, and stop — so for them the
**film-base** phase, not decode, is usually the peak. `nc roll` reports the same
block **per frame** (frames may differ in dimensions, and the gate runs per
frame), not once for the roll — including for a frame that passed the gate and then
failed for another reason, whose entry carries both its `memory` block and its
`error`.

### Example invocations

```bash
# Default density conversion: fixed/roll nominal Dmax, gain-map JPEG, JSON report.
# The two selector flags are optional (both are the defaults); the film-base flag
# is **not** — `film_base.source` has no default, so every `convert` must state
# one of `--film-base` / `--base-region` / `--auto-base`. The `.jpg` suffix is not
# optional either: the default preset is `gain-map-hdr`, and nc never renames the
# path you give it (add `--output-preset legacy` for the transitional TIFF).
nc convert in.tiff -o out.jpg --reconstruction density \
  --density-curve exponential --auto-base --report json

# Transitional rendered float TIFF: --no-d-max selects the exponential curve's
# unity placement (base → 1.0, detail above), then the current print controls
# still run and the depth-aware default profile (acescg for f32) applies. This
# is NOT film-master.
nc convert in.tiff -o out.tiff \
  --output-preset legacy --out-depth f32 --density-curve exponential --no-d-max \
  --film-base 0.92,0.55,0.42 \
  --density-gamma 1.8 --print-exposure 0.0 --black-point 0.002 \
  --highlight-compress 0.3

# The film master: unclamped 32-bit float linear ACEScg straight out of the
# NC film RGB v1 mapping, ACEScg profile embedded, no print or display controls at
# all. Reconstruction + the density curve + the roll-fixed Dmax placement ARE in
# the master (that is the intentional film rendering); WB/exposure/black/range and
# every display operation are not. The preset is atomic, so a NON-DEFAULT
# --out-depth / --output-profile / --bigtiff alongside it is a usage error (a
# default-valued --bigtiff auto is fine; the --out-depth flag is rejected outright,
# since `u16` resolves the default while forcing a container the master cannot
# produce) — as is a print control,
# --auto-d-max, or a measured --auto-balance-range. Never silently dropped.
nc convert frame12.tiff -o frame12_master.tiff \
  --output-preset film-master \
  --film-base 0.92,0.55,0.42 --d-max 1.64
# → report.output_render = { "preset": "film-master", "print_controls": false,
#     "display_render": false, "encoding": "unclamped-linear-acescg-float-tiff",
#     "working_mapping": "nc-film-rgb-v1", … }
# Re-exporting a graded roll recipe as a master: reset its print controls on the
# command line (flags win, and the rejection is on the RESOLVED value).
nc convert frame12.tiff -o frame12_master.tiff --params roll-A.json \
  --output-preset film-master --print-exposure 0 --white-balance 1,1,1

# Reuse a roll recipe but override one knob for this frame.
nc convert frame12.tiff -o frame12_pos.jpg \
  --params roll-A.json --print-exposure 0.15

# Convert a whole roll from ONE shared, frozen recipe (batch-apply). The shared
# recipe config (roll-fixed film base + Dmax) lives in roll-A.json and appears
# once at the top of the roll report; each frame additionally echoes the resolved
# base/Dmax it used. Per-frame outputs go to out/ as <stem>_positive.<ext>, the
# suffix following the resolved output preset.
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
nc convert frame01.tiff -o frame01_pos.jpg --film-base 0.553,0.271,0.159
# …or paste film_base_recipe into roll-A.json as its "film_base" section and batch it.

# Calibrate the roll-fixed display-white anchor `Dmax` the same way: point
# `--d-max-region` at a fully-exposed (near-opaque) reference frame — the
# light-struck roll leader — with the roll's Dmin as --film-base. `estimate`
# reduces that region's per-channel base-relative density D (= corrected density
# under default density-scale/offset) to one scalar (a gray-density
# reduction) and reports it in reusable forms: a --d-max flag and a
# `reconstruction.curve` recipe fragment. The region is recorded as provenance (dmax_region), NOT as a
# re-read directive — the frozen recipe carries the scalar so the apply phase is
# deterministic. `Dmax` is roll-fixed like `Dmin` (see §7.2/§9).
nc estimate leader.tiff --film-base 0.553,0.271,0.159 --d-max-region 200,0,300,3600 --report json
# → { "film_base": { … },
#     "dmax": 1.6428, "dmax_region": [200, 0, 300, 3600],
#     "d_max_flag": "--d-max 1.6428",
#     "d_max_recipe": { "dmax": { "explicit": 1.6428 } }, … }
nc convert frame01.tiff -o frame01_pos.jpg --film-base 0.553,0.271,0.159 --d-max 1.6428
# …or paste d_max_recipe's "dmax" key into roll-A.json's tagged
# "reconstruction"."curve" object. With no reference frame, omit it: the default
# `reconstruction.curve.dmax = fixed` nominal anchor still renders a viewable
# positive (darker frames stay faithfully darker).

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
nc convert frame01.tiff -o frame01_pos.jpg --film-base 0.92,0.55,0.42 \
  --auto-wb percentile --report json
# → { "white_balance": [1.083, 1.0, 0.941], ... }
nc convert frame02.tiff -o frame02_pos.jpg --film-base 0.92,0.55,0.42 \
  --white-balance 1.083,1.0,0.941
```

## 9. Parameter reference (grouped by stage)

Every conversion flag has a recipe key (for example, `--out-depth` ⇒
`output.depth`); reconstruction entries live under the tagged `reconstruction`
object (§8). Names are binding and unknown keys are rejected
(`deny_unknown_fields`).

### Input / decode
- `--export-ir <path>` — write the IR plane to a separate TIFF (HDRi only).
  Recipe key `input.export_ir`. The **IR TIFF** follows the resolved output depth
  (`OutputParams::depth()`), which is *not* the primary container's depth for the
  display presets. In full:
  - 16-bit — the legacy default; `--output-preset hdr-pq-tiff` / `hdr-hlg-tiff`
    and `display-p3` / `compatibility` (whose primaries are themselves 16-bit);
    and the four presets whose primary is not a TIFF at all: `gain-map-hdr` and
    `ultra-hdr-v1` (fixed 8-bit JPEG) and `hdr-pq` / `hdr-hlg` (10-bit AVIF).
  - 32-bit float — `--out-depth f32` on the legacy / `custom` path;
    `--output-preset film-master`; and `--output-preset hdr-linear-tiff`. The last
    two resolve f32 from the preset without consulting `output.depth`.

  The IR *samples* never
  change: the plane is carried through the pipeline untouched (Step-1 rule: preserve,
  don't consume), so only the quantization headroom differs.
- `--film-type <silver|chromogenic|unknown>` ⇒ `input.film_type` (default
  `"unknown"`) — the declared film chemistry. `chromogenic` (C-41 colour or
  C-41-process B&W) is IR-transparent, so on an HDRi scan that carries an IR plane
  it enables **IR-assisted film-holder detection** for the film base (§6.1): the
  opaque scanner holder reads dark in IR while all film reads bright, so
  holder-occluded spans are excluded from the auto rebate search. `silver`
  (silver-halide blocks IR) and the `unknown` default keep the IR path off; a scan
  with no IR plane (HDR 48-bit) always falls back to RGB-only detection. Shared
  input-medium axis — the deferred IR dust-removal stage (§12 item 1) gates on the
  same declaration. Accepted on `convert`, `estimate`, and `inspect`; declaring
  `chromogenic` on a scan with no IR plane (where auto detection runs) is a report
  warning (RGB-only fallback), promotable under `--strict`. `nc inspect
  --film-type chromogenic` additionally reports a `holder_mask`: the per-edge
  along-edge segments, each with its span `[start, end)`, holder/film class, and
  representative median IR transmission, so the occluded spans are inspectable.
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
`film_base.source` — **required, with no default**. `convert` and `roll` reject a
config that does not state one (exit 2, naming the three ways to supply it —
`roll` accepts none of the flags, so its message points at the shared `--params`
recipe instead). The measurement commands exist to *produce* a base, so requiring
one first would be circular: `estimate` resolves an unstated source to `"auto"`,
and `inspect` takes no film-base flags at all — it always runs the detector.

Why it is required: `Dmin` is the divisor of the density conversion, so it sets
the black point and the colour balance together, and auto-detection is
best-effort on real scans (the rebate is a thin inset band behind the holder, not
the outer margin). Defaulting silently meant the single most consequential
parameter of a conversion was one nobody had decided. `--auto-base` is still one
flag — the requirement is that the choice be *stated*, not that it be explicit.

The three flags conflict (passing more
than one is a usage error); whichever is given replaces a recipe's source:
- `--film-base R,G,B` ⇒ `{ "explicit": [r, g, b] }` — explicit base transmission.
- `--base-region x,y,w,h` ⇒ `{ "region": [x, y, w, h] }` — sample this rectangle.
  A non-uniform rectangle (one that mixes rebate with image content) keeps its
  sampled value but raises a **uniformity warning** in the report (`--strict`
  promotes it) — a mixed rectangle otherwise yields a plausible-looking bad base
  with no signal.
- `--auto-base` ⇒ `"auto"` — detect the unexposed rebate band behind
  the film holder (the inward-scan detector; see the ladder below). On no
  confident band it **fails loudly** and *suggests* `--base-content` — the opt-in
  content source owned by the separate `film-base/content-fallback` task (ladder
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
   is a roadmap item, §12). Convenience form: `--auto-base` — real
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
   opt-in source** owned by the dedicated `film-base/content-fallback` task
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
- CLI: `--reconstruction simple|density` (default `density`).
- With density: `--density-curve exponential|sigmoid` (default `sigmoid`).
  `--density-curve` with `simple` is a usage error (no curve stage).
- Recipe: `reconstruction.type`, then for density
  `reconstruction.density` and tagged `reconstruction.curve`, exactly as shown
  in §8. There are no sibling top-level density or curve sections.
- The removed `--algorithm` flag and old `algorithm` recipe form are rejected
  with a migration error rather than retained as aliases, and the old top-level
  `density`, `sigmoid`, and `simple` forms are rejected at the same boundary
  (nc is unreleased).

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
- The `--anchor-*` family ⇒ `reconstruction.curve.anchor`, and unlike the
  `--sigmoid-*` flags above it is **curve-neutral** — placement is orthogonal to
  curve shape, so the same flags apply under either curve:
  `--anchor-mid-fraction <f>` ⇒ `{"mid-at-dmax-fraction": f}`,
  `--anchor-white-at-reference` ⇒ `"white-at-dmax"`,
  `--anchor-black-floor <floor>` ⇒ `{"black-at-base": floor}`, and
  `--anchor-mid-offset <d>` ⇒ `{"mid-at-base-offset": d}`. All four conflict (the
  placement is one rule, not four independent fields); whichever is given replaces a
  recipe's `anchor` entirely. `--sigmoid-mid-fraction` and
  `--sigmoid-white-at-d-max` remain accepted as **aliases** of the first two: they
  predate the sharing and appear in committed recipes and docs.
- `Dmax` is owned by the tagged curve. Its target recipe key is
  `reconstruction.curve.dmax` (default `"fixed"`; see §7.2). It is a
  **roll-fixed
  calibration** like `Dmin`. The four flags conflict (passing more than one is a
  usage error); whichever is given replaces a recipe's `dmax`:
  - `--fixed-d-max` (default) ⇒ `"fixed"` — the roll-fixed **nominal** anchor: a
    scene-independent corrected-density placement (`Dmax = 1.3`, in density units),
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
    bit-for-bit **under the default `white-at-dmax` placement**; `none` resolves the
    reference to `0`, and the other rules still derive an anchor from the slope. This
    is an unanchored film rendering, not a physical-scene
    Current `--out-depth f32` remains a rendered float TIFF,
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
`reconstruction.curve.contrast`, `.toe`, `.shoulder`, and `.anchor`:
- `--sigmoid-contrast <f>` — mid-density slope of the curve in log-output space
  (the `--density-gamma` analogue). Finite and in `(0, 50]`; default
  `0.745/0.36 ≈ 2.0687`, derived rather than chosen: `0.36` is the mid-grey →
  diffuse-white density difference the manufacturers' own *Judging Negative
  Exposures* aim tables state (constant across stocks, though the absolute
  densities are not), and `0.745 = −log10(0.18)` is the same interval on the
  output axis. Cross-checks: it implies a film gamma of `0.52`, textbook for
  colour negative, and an overall system gamma of `1.07`, i.e. near-faithful
  reproduction. The
  upper cap guards against an extreme slope collapsing the S-curve into a hard
  black/white threshold that silently destroys tonal detail (use the
  `exponential` density curve for genuinely extreme contrast).
- `--sigmoid-toe <f>` — toe (shadow) knee width in log10 density units; `0`
  disables the toe. In `[0, 10]`; default `0.2`. The upper cap is far beyond the
  ~`0.05–0.9` photographic range and rejects a degenerate width that would
  flatten the image into near-uniform tone without tripping the clip/non-finite
  counters.
- `--sigmoid-shoulder <f>` — shoulder (highlight) knee width in log10 density
  units; `0` disables the shoulder. In `[0, 10]`; default `0.6` (same cap
  rationale as `--sigmoid-toe`). At the default contrast that width begins bending
  at `D' ≈ 0.70`, essentially at mid-grey — where a print shoulder belongs, and
  what makes the anchor's headroom above white a roll-off rather than a clip.
  Narrower widths (`0.2`) give visibly crisper midtones at a measurable cost in
  highlight separation.
- The `--anchor-*` family — which tone the curve pins, and where (§7.3).
  `--anchor-mid-fraction <f>` is finite and in `(0, 1]` (sigmoid default `0.5`);
  outside it the anchor either detaches from the reference entirely (`f ≤ 0` pins
  mid-grey at or below the film base, rendering the whole frame above mid-grey) or
  places mid-grey past display white (`f > 1`). `--anchor-black-floor <floor>` is in
  `(0, 1)`: at or below 0 there is no logarithm to take, and at 1 the film base
  renders as display white. `--anchor-mid-offset <d>` is strictly positive — it is a
  density *above* the base, and 0 would pin mid-grey on the base itself. All are
  usage errors rather than clamped values. Every placement but
  `--anchor-white-at-reference` divides by the slope, so validation **resolves the rule**
  and rejects a non-finite anchor, naming the slope flag (`--density-gamma` or
  `--sigmoid-contrast`). There is no single slope bound to quote: the mid-grey rules
  divide the fixed `0.745`, which overflows below ~`2.2e-39`, while
  `--anchor-black-floor` divides `−log10(floor)`, which is unbounded as the floor
  shrinks. `--anchor-white-at-reference` performs no division and is exempt. Slope
  positivity/finiteness is diagnosed **first** — "the slope is 0" is the more specific
  fault than "the anchor derived from it overflowed", and the division's remedy does not
  apply to it.

  A **finite** anchor is separately checked against the slope: the curve evaluates
  `slope · (density − anchor)`, so a large finite anchor overflows that *product* to
  `−inf` and `10^(−inf)` is exactly `0.0` — a silently black frame with no clip and no
  non-finite count. `--anchor-mid-offset 2e38` reaches it at the shipped default gamma.
  The rule is that no intermediate may overflow, not merely that the anchor is finite;
  a large anchor whose product stays finite is honest arithmetic on absurd input and is
  accepted (bounding *that* belongs to `algo/density-safety-bounds`).

These caps reject only *nonsense / degenerate-asymptote* values (a knee of `10000`
that flattens the frame); within them, aggressive-but-valid contrast/knees produce
faithful, deliberate output that may posterize or crush — that is the user's
choice and is intentionally **not** warned (a degenerate-band warning would
false-positive on legitimate high-contrast conversions).

### Print / tone render
- `--print-exposure <f>` — overall positive exposure.
- `--black-point <f>` — paper black / shadow floor.
- `--linear-range LOW,HIGH` / `print.linear_range` (default `[0,1]` = the exact
  identity) — the shared render contract's exact affine `(x-low)/(high-low)`
  black/range placement (black/white-point placement). It is distinct from the
  existing density print `black_point` and from SDR/HDR reference white in nits. An
  **atomic pair**: the flag replaces both endpoints, and passing the default `0,1`
  is the flags-win reset of a recipe's non-default pair. Validated after merge for
  finite `low < high` **and** a representable span (two individually-finite
  endpoints whose difference overflows would silently collapse every sample). A
  negative `LOW` is legal, so a leading `-` is accepted.
  **Shipped state:** the shared display stage applies it for the explicit
  `ultra-hdr-v1` preset. The legacy TIFF path keeps its frozen ordering and
  therefore rejects a non-default value, while `film-master` bypasses and rejects
  all print controls. Remaining alias/default activation belongs to
  `output/presets`.
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

  Auto estimation requires `--reconstruction density` (either curve);
  `--auto-wb` with `simple` remains a usage error (exit 2). The preset
  migration gives simple the same **explicit** downstream WB slot, but does not
  imply that density-based auto estimators support simple without a separately
  specified generalization.
- `--highlight-compress <f>` — highlight roll-off amount. Frozen legacy
  semantics use `0` as off and positive values for the existing above-`1.0`
  soft clip. Under `print.display_tone = shoulder` the named SDR and HDR branches
  apply their baseline display shoulder: `0` selects the branch baseline and
  positive values move its resolved knee earlier, bounded in the branch's domain as
  specified in §6. Preset activation/versioning owns the semantic switch; this is
  not a second conversion knob.
- `--display-tone <shoulder|none>` / `print.display_tone` (default `shoulder`) —
  which tone curve the named display renderers apply. `none` skips the display
  shoulder entirely; gamut mapping and the transfer encode still run, so it removes
  *tone*, not display rendering. A **selector**, not a width: `highlight_compress`
  moves the knee within a bounded `[0.5, 0.75]` and no value of it removes the
  curve, which is why "off" cannot be spelled as a highlight-compression amount.
  Resolved once per frame together with `highlight_compress` into the single tone
  value both display branches consume, so SDR and HDR cannot diverge.
  Three rules, all loud (never a silently-dropped knob): a non-default value is
  rejected on the legacy branch (`legacy` / `custom`, which apply no display tone
  curve) and by `film-master` (which bypasses display rendering); and a
  **non-default** `highlight_compress` beside `none` is a usage error, since a knee
  width describes nothing without a knee (the default `0` is the identity and is
  accepted — every rule here is on the resolved value). The mode is otherwise **self-policing** rather than gated
  on a curve type: each renderer already refuses a sample outside its range, so
  pairing `none` with a reconstruction that overshoots the branch's ceiling fails
  naming the pixel instead of clipping. The ceilings differ — reference white for
  SDR, the 1000-nit peak (`LINEAR_HEADROOM`) for HDR — so an overshoot that SDR
  refuses can render legitimately on an HDR preset, which is where the headroom a
  gain map needs would come from. A sigmoid curve with `shoulder > 0` and neutral
  print gains is bounded by construction and is the intended pairing.

### Simple algorithm
- Removed legacy controls: `--invert-white-balance R,G,B` and
  `--clip-low <f>` / `--clip-high <f>`. In the pre-split converter they ran before
  the output transform; **as shipped they are rejected flags** (hidden args carrying
  a migration error that names the concrete replacement) and the matching `simple.*`
  recipe keys are rejected too, because they are not simple reconstruction
  parameters in the film-preserving pipeline. Preset migration is to accept them as
  warned aliases with the named *display* presets, not with `film-master`. Although
  `ultra-hdr-v1` now consumes the replacement fields directly, alias activation
  remains deferred to the complete `output/presets` migration so help text,
  warnings, provenance, roll handling, and the conversion-version boundary land
  atomically:
  inversion WB maps to explicit `print.white_balance`, while clip endpoints map
  to `print.linear_range` / atomic `--linear-range LOW,HIGH`. Resolve the recipe
  pair or `[0,1]` first. The atomic flag replaces both endpoints and conflicts
  with either legacy range flag; otherwise `--clip-low`/`--clip-high`
  independently override their endpoint. Validate finite `low < high` after
  merge, warn, and report endpoint provenance. New recipes/reports emit only
  replacement names, and named presets apply them only after NC film RGB mapping.
  `film-master` rejects every final non-default range regardless of source;
  legacy flags may reset recipe endpoints to `[0,1]`. Legacy-preset TIFF calls keep current ordering until
  migration. Aliases preserve parameter values, not bit-identical output through
  the working-space matrix; target activation warns, and
  `conversion-versioning` owns the prospective behavioral-version bump when the
  changed preset/default pixels activate.

### Output / encode (current terminal stage; target stages 5–6)

**How artifacts reach disk (`io/transactional-output-writes`).** Every file `nc`
writes — the primary output, the IR export, the sidecar, `--dump-params`,
`--report-file` — is written to a **same-directory temp**, flushed, **fsynced**, and
only then renamed onto its final path. Two guarantees follow, and one deliberately
does not:

- **No truncated file ever appears at a final path.** The final path holds either the
  previous content or nothing — unconditionally, including on `SIGINT`/`SIGKILL` and
  power loss, because the final path is never opened for writing. Overwrite remains
  **atomic replace**: `nc` keeps overwriting its own output rather than refusing, a
  **symlinked** target is followed so the *referent* is replaced and the link survives,
  and an existing file's **permissions are carried onto** the replacement so a `0600`
  output does not silently widen to `0644` (mode only — not ACLs or xattrs). The staging
  temp is created at the target's mode too, so a killed run cannot leave a wider-than-final
  copy of the pixels behind.
- **Three targets are refused rather than replaced**, because `rename` is more permissive
  than the `File::create` it replaced: an existing **read-only** file (rename needs write
  permission on the *directory*, so a deliberate `0400` output would otherwise be silently
  overwritten), a **non-regular** file (FIFO, socket, device node — `create` opened those;
  a rename destroys them), and **two artifacts that resolve to the same file** (possible
  when a symlinked output points at another artifact's path, which the up-front collision
  check cannot see because it compares the paths as given). Each is exit 5 with a message
  naming the path and the reason.
- **Hard links are reported, not refused.** An atomic replace necessarily breaks them — the
  other names keep the previous file's bytes — and writing through the shared inode instead
  *is* the non-atomic behaviour this removes. So a target with `nlink > 1` converts and emits
  a warning (report + stderr, `--strict`-promotable) rather than failing or going quiet.
- **Temp cleanup is narrower than that.** Ordinary error paths remove the staging file;
  a signal that kills the process does **not** run destructors, so `SIGINT`/`SIGKILL`
  can leave an inert `*.nctmp` beside the output. No signal handler or startup
  scavenging is installed, so the guarantee is stated for ordinary error paths only.
- **One conversion's artifacts commit together.** The IR export, primary and sidecar
  are all staged before any is renamed, so a failure in a later one leaves *no*
  primary output — the "complete TIFF with no sidecar" case is gone. The renames are
  pre-checked (a target occupied by a directory fails before anything is promoted) and
  the **primary is renamed last**, because its presence is what reads as success.
- **Not a multi-file transaction.** POSIX `rename` is atomic per *file*; a set cannot
  be flipped as one unit. A crash between two renames, or a rename failure no cheap
  check predicts, can still leave one final path updated and another not. This is
  inherent and stated rather than papered over.

`--dump-params` and `--report-file` are staged individually but *not* held back to
join that set: the former is written before anything is decoded, and the latter must
land even when `--strict` then fails the run (and under `roll` it is a roll-level
artifact no single frame's set could hold). Telemetry is unchanged — after the
finalized output, best-effort, never part of the set. Directory fsync (power-loss
durability for the rename itself) is out of scope: the temp+rename pattern already
covers a full disk, a permissions error, a crash and `SIGINT`, and the remaining gain
would cost a Unix-only code path for output that is reproducible by re-running.

- `-o, --output <path>` (required)
- `--output-preset <gain-map-hdr|ultra-hdr-v1|display-p3|compatibility|film-master|hdr-pq|hdr-hlg|hdr-linear-tiff|hdr-pq-tiff|hdr-hlg-tiff|legacy|custom>`
  — the atomic output **policy** choice;
  recipe key `output.preset` (**default `gain-map-hdr`**). One mutually-exclusive enum field,
  never parallel bools: a preset resolves a whole coherent container/depth/profile
  policy plus which branch of the ACEScg boundary runs.
  - `legacy` is the transitional TIFF path — the print controls run before the
    working→output ICC transform — and, with `custom`, one of the two **non-atomic**
    presets, so it stays compatible with `--out-depth`/`--output-profile`/`--bigtiff`.
    It is no longer what a bare invocation resolves; since `pipeline_version` 3 that
    is `gain-map-hdr`, and reaching this path takes naming it.
  - `custom` is `legacy` under a name that says "I am combining the selectors
    myself": same branch, same bytes for the same combination, different provenance
    in the report. It exists because omitting a preset no longer means the selectors
    apply.
  - `film-master` is a *named* preset: an unclamped 32-bit float linear ACEScg TIFF
    taken directly from the NC film RGB v1 mapping with the ACEScg profile embedded
    and no transform. Being named it is **atomic** — a **non-default** resolved value
    for any of the legacy selectors below is a usage error next to it, from a flag or
    a recipe key alike; a value that already equals the documented default
    (`--bigtiff auto`, `"depth": "u16"`) is accepted, since it asks the preset for
    nothing it does not already do. The `--out-depth` **flag** is the exception and is
    rejected by flag **presence**: `u16` resolves that very default while still
    *forcing* 16-bit integer output the master cannot produce, so a value rule would
    silently ignore a contradicted request (the recipe key needs no mirror — the
    default asserts nothing). It resolves f32 itself rather than through
    `output.depth`, and after recipe/CLI merge it rejects the frame-local measurements
    `auto` `Dmax` and (when a balance is actually applied) `auto`
    `reconstruction.density.balance_range`, plus every non-default print control
    (`print_exposure`, `black_point`, `white_balance`, `display_tone`,
    `highlight_compress`, `linear_range`) whatever their source. There is no ignore-conflicting-controls
    mode; a linear export that also wants a creative/print/display adjustment is the
    `custom` workflow. Supported anchors by curve: exponential — `fixed` (default),
    explicit/roll, or `none` (unity placement); sigmoid — `fixed` or explicit/roll
    (`none` is rejected for the S-curve regardless of preset); `simple` has no
    `Dmax`.
  - `gain-map-hdr` and `ultra-hdr-v1` are the two gain-map JPEG presets, and are
    one render packaged twice: identical pixels, differing
    only in metadata. `gain-map-hdr` attaches ISO 21496-1 segments to **both** images
    on top of the legacy dialect, and is the only form Apple platforms decode as HDR;
    `ultra-hdr-v1` stays contractually ISO-free as the legacy-only compatibility
    output. Both write an 8-bit Display P3 SDR primary plus a
    half-resolution grayscale Ultra HDR v1 gain-map JPEG and legacy XMP/MPF/
    GContainer metadata. They require a `.jpg`/`.jpeg` output, consume the shared
    post-ACEScg print controls. Only `ultra-hdr-v1` makes no ISO 21496-1 claim; it
    is the compatibility form. The canonical
    internal gain model remains RGB; the legacy serializer derives the
    single-channel Display P3 luminance gain that XMP mode can signal.
    **Interop caveat, measured 2026-08-06:** Apple platforms ignore the legacy
    Ultra HDR v1 XMP dialect entirely, so this preset's output opens as an
    ordinary **SDR** JPEG on macOS/iOS — correct and backward-compatible, but not
    HDR there. Apple ImageIO reports no gain map of either kind and decodes at
    headroom 1.0; only the ISO 21496-1 dialect
    (which `gain-map-hdr` writes) is read. Android and
    libultrahdr-based readers do consume the legacy dialect. This is why the
    future `gain-map-hdr` default is dual-dialect rather than legacy-only; see
    `scripts/iso-decoder-oracle/` for the harness that measures it.
  - `display-p3` and `compatibility` are explicit single-rendition **SDR**
    presets, each requiring a `.tif`/`.tiff` output.
    They write 16-bit integer TIFF — lossless, no lossy codec — with a 203 cd/m²
    reference white, through the modern display stage: NC film RGB v1 → linear
    ACEScg → the shared print controls → `pipeline::sdr`, including its
    reference-white-preserving shoulder and gamut mapping into the destination.
    They differ **only** in that destination: Display P3 for the first, sRGB for
    the second, which is the widest-support output nc writes.

    The distinction from the `legacy` TIFF path is the *pipeline*, not the
    profile: `legacy` applies the print controls **before** a plain
    working→output ICC transform and never crosses the ACEScg boundary. Making
    `display-p3` the default in place of the incumbent `gain-map-hdr` is
    `output/sdr-preset-followups` — decided, not yet executed, because it is both a
    pixel change and a container change.
  - `hdr-pq` and `hdr-hlg` are explicit single-rendition display-HDR presets,
    each requiring an `.avif` output path. They write
    10-bit, full-range, 4:4:4 AVIF (AV1 High Profile, level capped at 6.0 for the
    Advanced Profile) with CICP `9/16/9` for PQ and `9/18/9` for HLG, a 203 cd/m²
    reference white and a 1000 cd/m² mastering peak. PQ additionally carries a
    `clli` content-light box **measured from the frame** — `MaxCLL` is its
    brightest pixel's luminance in cd/m² and `MaxPALL`/`MaxFALL` its frame
    average, both per CTA-861.3, so a dark frame reports dark numbers and never
    the renderer's peak; HLG omits the box because HLG is display-referred and
    absolute values would be a false claim. Being named
    presets they are **atomic** on the same terms as `film-master`, and they consume
    the shared post-ACEScg print controls. Encoder settings (quality, speed, one
    thread, no tiling) are pinned parts of the preset, not knobs: repeated encodes on
    one build are byte-identical. No EXIF, XMP, ICC, timestamp or identifier is
    written.
  - `hdr-linear-tiff` is the display-linear HDR **interchange master**, accepted by
    requiring a `.tif`/`.tiff` output path. It writes the
    pre-transfer BT.2020/D65 samples of the same `pipeline::hdr` render verbatim as
    unclamped 32-bit float, with a synthesized linear-BT.2020 ICC profile —
    bit-exact, so an independent decoder recovers identical `f32` bits including the
    HDR values between the 203 cd/m² reference white (`1.0`) and the 1000 cd/m² peak
    (≈4.926108). It is named and therefore **atomic** on the same terms as
    `film-master`, and it consumes the shared post-ACEScg print controls (including
    a non-default `print.linear_range`, which the legacy path rejects). Three
    distinctions it exists to keep separate: it is **not** `film-master` (that is
    linear ACEScg *before* any display rendering), **not** `hdr-pq`/`hdr-hlg` (no
    transfer function has been applied), and **not** `--out-depth f32` (a *print*-
    rendered float TIFF in the selected output space). Because the ICC PCS stops at
    the media white, no v4 profile can state the luminance mapping: the report's
    `hdr_linear_tiff` block and the sidecar are authoritative for reference white,
    peak, headroom, tone/gamut policy, and the frame's measured content-light
    levels. The profile deliberately carries **no** `cicpTag` — H.273's
    full-range flag describes a bounded code range, and these samples exceed 1.0 by
    design, so the claim would over-state the encoding while adding nothing the
    colorants and linear TRC do not already say.
  - `hdr-pq-tiff` and `hdr-hlg-tiff` store the same Rec.2100 rendition `hdr-pq` /
    `hdr-hlg` code as AVIF, but as **full-range 16-bit TIFF code values**, accepted
    requiring a `.tif`/`.tiff` path. "Lossless" here means
    *relative to the quantized signal*: the renderer's normalized output is
    quantized once with one pinned rounding rule (`round`, half away from zero) and
    TIFF stores every resulting code exactly, with the measured max and RMS
    quantization error reported in code units. A sample outside `[0, 1]` is
    **rejected, not clipped** — the transfer stage guarantees the domain, so an
    out-of-domain sample means that stage is broken. **16 bits is TIFF's
    quantization, not one of BT.2100's own bit depths** (it specifies 10 and 12), so
    the file carries BT.2100's transfer function at TIFF's precision and the report
    says exactly that. TIFF has no CICP tag of its own, so the signalling lives in
    the embedded ICC profile's `cicpTag` (ICC.1:2022 §9.2.17/§10.3): `9-16-0-1` for
    PQ and `9-18-0-1` for HLG, with **MatrixCoefficients 0** because the data colour
    space is RGB — the same rendition's AVIF carries 9 because AVIF stores Y'CbCr,
    and confusing the two would be non-conformant. Because only a CICP-aware
    colour-managed reader honours that tag, these are documented as
    **limited-interoperability interchange, never "display-ready"**; the AVIF and
    gain-map presets remain the delivery paths. The PQ profile is an
    **extended-range A2B** (`lutAtoBType`) whose PCS is `Y = L / 203`, unclipped to
    ≈49.26 — a matrix-shaper profile cannot express that, since a TRC output is
    confined to `[0, 1]`. The HLG profile is deliberately **scene-referred**: HLG's
    OOTF scales each channel by a function of the pixel's own scene luminance, so it
    is not per-channel separable and no 1D curve set can carry it; the
    display-referred contract (1000-nit peak, zero black, system gamma 1.2) lives in
    the report's **`hdr_coded_tiff`** block instead — the coded counterpart of
    `hdr-linear-tiff`'s `hdr_linear_tiff` block, and mirrored into the sidecar's
    `meta` for the same `--report none` reason (§5).
  - No planned-but-unaccepted name is left, so an unknown one always means a typo.
    The pre-release `scene-master` is still rejected as an unreleased-schema break
    naming the rename — **not** an alias. The flag and the recipe key share one
    parser, so a name gets the same diagnosis wherever it appears.
- `--out-depth <u16|f32>` — encoder bit depth for the TIFF paths. Recipe key
  `output.depth` (default `u16`). `f32` writes an unclamped **rendered** TIFF after
  the print controls — never the `film-master` preset (a *different* selector; see
  `--output-preset`) and never Rec.2100 display HDR.
  Consulted only by `legacy` and `custom`; every other preset resolves depth
  itself, so the **flag** next to one is a usage error checked by *presence*
  (`--out-depth u16` resolves the documented default, so a value rule could not see
  it while it still forces a depth the preset cannot produce).
  **Replaced `--output-hdr` / `--output-sdr` and the `output.hdr` bool** — one
  mutually-exclusive choice modelled as two presence flags, and "HDR" named neither
  thing the float TIFF is. Both old flags and the old recipe key are rejected with
  a migration error; nc is unreleased, so there is no alias.
- `--output-profile <srgb|prophoto|acescg|display-p3|path-to-icc>` (default is
  depth-aware: `srgb` for the 16-bit default, `acescg` for `--out-depth f32`).
  `display-p3` tags a wide-gamut SDR Display P3 destination (P3 primaries, D65
  encoding white, piecewise sRGB TRC) with a deterministically synthesized ICC v4
  profile (D50 PCS/media white, Bradford-adapted colorants, `chromaticAdaptationTag`).
  As shipped it transforms from the linear Rec.709 working space like every other
  output space: a lossless Rec.709→P3 primaries remap (Rec.709 ⊂ P3, no gamut
  compression) plus the sRGB TRC. Consuming already-rendered linear-P3 values as a
  pure transfer-encode — and the ACEScg→P3 render and SDR gamut policy that produce
  them — is the target state owned by `sdr-display-rendering`, not this axis. This
  is the profile/encoding axis, and it stays distinct from the shipped `display-p3`
  *preset*, which resolves container, depth and tone/gamut policy together and
  reaches P3 through the modern display stage rather than through this transform.
- `--bigtiff auto|on|off` (default `auto`)

Planned `output/presets` replaces the depth-only default with `gain-map-hdr` and
the explicit `custom` policy. Already accepted: `film-master`, `ultra-hdr-v1`,
`hdr-pq`, `hdr-hlg`, — since `output/lossless-hdr-tiff` — `hdr-linear-tiff`,
`hdr-pq-tiff` and `hdr-hlg-tiff`, and — since the SDR half of `output/presets` —
`display-p3` and `compatibility`.
`display-p3` and `compatibility` are 16-bit losslessly stored TIFF; `hdr-pq` and
`hdr-hlg` are AVIF, while the three shipped HDR TIFF policies provide
linear-float or losslessly stored PQ/HLG interchange. `film-master` encodes NC
film RGB v1 mapped unclamped linear ACEScg before print/display controls and
rejects frame-local auto Dmax.
Exponential accepts supported `none` or fixed/roll placement, sigmoid uses fixed
Dmax for curve shaping, and simple has none. Named display presets use the SDR/HDR render
branches. The output path stays required; its suffix must match the
resolved container and is never rewritten silently. A named non-`custom` preset
conflicts with legacy output-selection flags (`--out-depth`,
`--output-profile`, `--bigtiff`); legacy flag-only invocations retain their
transitional TIFF behavior. After merge, `film-master` also rejects every
non-default effective WB, exposure, black, white, highlight, SDR/HDR tone, gamut, or
display-transfer control from recipe or CLI; it never ignores one. Flags may
explicitly reset recipe values to defaults, and the resolved report records the
effective values/provenance and that no display transfer ran. A selected
`correction.profile` is not a downstream creative/print/display control:
corrected output remains `film-master` and records mandatory profile
identity/hash/scope provenance. **The migration list is complete**: all twelve
presets are live and `gain-map-hdr` is the default as of `pipeline_version` 3
(measured in [reports/render-defaults-v3.md](reports/render-defaults-v3.md)).
`nc roll` migration is part of the preset task: automatic names use
each resolved container suffix, manifest/per-frame overrides validate
independently, and each sidecar derives from its final image path. The single roll
report remains on stdout or the explicit `--report-file`; that destination is
collision-checked against all inputs, outputs, and sidecars before writing.

### Global
- `--params <json>`, `--dump-params <json>`
- `--report json|none`, `--report-file <path>`
- `--strict` — promote report warnings (clipping, non-finite samples, grid
  disagreement, …) to a failing exit (see §11); on `convert`, `roll`, and `estimate`
- `--max-memory <bytes>` — peak-memory budget for the run (`8GiB`, `512MB`, or raw
  bytes). Every command that decodes a scan (`convert`, `roll`, `inspect`,
  `estimate`) estimates its peak allocation from a **metadata-only header probe
  before decoding** and fails with exit 6 when it would exceed the budget. `roll`
  gates **per frame**, and follows its usual per-frame error handling: the frame's
  resource error is recorded in its report entry, sibling frames are still
  converted and written, and the roll exits **1** ("frames failed"), not 6.
  Default **6 GiB** — deliberately a fixed constant, not a
  fraction of detected RAM, so the pass/fail decision is the same on every
  machine. An estimate that fits the budget but exceeds ~70% of detected physical
  RAM warns instead — `--strict`-promotable on `convert`/`roll`/`estimate`, and
  report-only on `inspect`, which has no `--strict`. Like `--report`/`--strict`/telemetry
  this is **operational**: not a recipe key, never in the sidecar, and it can
  never change an output byte. The estimate, its per-phase breakdown, the budget,
  and the decision ride out in the JSON report's `memory` block.
  **Second effect to know about:** the budget also caps the `tiff` crate's read
  buffers (`min(4 GiB, budget)`), so a budget that admits the run but sits below a
  single plane's read buffer turns a decodable file into a decode failure (exit 3)
  rather than a resource error. A passing preflight makes that nearly unreachable —
  the estimate is a multiple of the read buffer — but it is the one way this
  operational flag changes an outcome other than the gate's own verdict.
- `-v/--verbose`, `--quiet`

**Roll (batch, `nc roll` only — orchestration flags, NOT recipe keys).** `nc roll`
converts many frames from one shared `--params` recipe; it reuses the exact recipe
shape above and adds no new conversion knobs. Its flags are operational (like
`--report`): `--out-dir <dir>` (per-frame outputs `<stem>_positive.<ext>`, the
suffix following each frame's resolved preset),
positional `inputs` (files and directories — a directory is expanded to its
`.tif`/`.tiff` files, sorted; shell globs are expanded by the shell, not by nc)
**or** `--frames <manifest.json>` (explicit per-frame `input`/`output`/partial-recipe
`params` overrides, deep-merged onto the shared recipe for that frame only).
**A `--params` recipe is effectively mandatory for `roll`**, because `roll`
converts and `film_base.source` has no default while `RollArgs` accepts none of
the three film-base flags — the recipe is the only place a roll can state its
base, and a roll with no recipe (or one omitting `film_base.source`) exits 2 with
a message that says so. That is the intended workflow rather than a limitation:
`Dmin` is measured once for the roll (`nc estimate`) and frozen into the shared
recipe as `film_base.source.explicit`, which is also the only source that keeps
every frame on one base — see the roll-fixed invariant warnings below.
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
that sets `film_base` changes that frame's Dmin; (4) a per-frame override
that changes the active Dmax key changes that frame's placement; (5) a per-frame override
that sets `reconstruction.curve.anchor` changes which *tone* that frame pins to the roll's
reference density — a placement break that survives even when every frame shares one Dmax,
and subtler than a different number because the frame renders on a different *rule*; and
(6) a per-frame override that sets `output.preset` gives that frame a different output
**policy** —
a different branch out of the ACEScg boundary, so a different *image class* (unclamped
linear master vs rendered TIFF), not merely a different rendering. (5) and (6) warn even when
the override restates the shared preset, because `frames[].status` carries no
`output_render` block (that is a `convert`-only report field), leaving the
`frames[].overrides` echo as the only other trace. Shared `fixed`, explicit, or
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

**Telemetry record shape (`schema_version` 3, serialize-only JSON).** Designed for
a future background uploader (§12, `telemetry/upload`) to drain and ship:
```json
{
  "schema_version": 3,
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
    "preset": "legacy",
    "reconstruction": "density", "curve": "exponential",
    "params_hash": "92a827ffd2d0aebd",
    "film_base_source": { "explicit": [0.9, 0.55, 0.42] },
    "dmax": 1.6195, "output_hdr": false
  },
  "outcome": { "warnings": 1, "clipped": 3419, "non_finite": 0 }
}
```
`timing_ms.ir_export` is present only when `--export-ir` ran; `conversion.curve`
only for density reconstruction (`simple` has no curve stage) and
`conversion.dmax` only when the curve applied an anchor (schema v2 replaced
v1's `conversion.algorithm` with the `reconstruction` + `curve` pair).
`conversion.preset` is the resolved `output.preset` — v3 added it, because without it
a `film-master` run is indistinguishable from a legacy one. `conversion.output_depth`
names the **primary** artifact's depth (`OutputParams::primary_depth_label()`), not the
`output.depth` knob: an atomic preset pins that knob at its default while resolving its
own container, so the knob alone reports `u16` for an f32 master — and `u8`/`u10` for
the JPEG and AVIF presets are depths it cannot spell at all.
`params_hash` is a stable hash of the
effective recipe JSON (the same bytes as the sidecar), so identical conversions
share a hash without the record carrying the whole recipe. The value shown above is
**illustrative**: because it covers the *whole* recipe it changes whenever any key is
added, removed, or re-defaulted (adding `print.linear_range` and `output.preset` changed
it, and the next schema change will again). Nothing asserts it — treat it as a shape
example, not a reproducible constant.`params_hash` is a stable hash of the canonical effective-recipe JSON — the same
bytes `--dump-params` writes — so identical conversions share a hash without the
record carrying the whole recipe. The sidecar is an envelope, so its `params` body
is the same recipe document re-indented rather than the same bytes. The hash is
computed by the same function as the report's `identity.params_hash`, so a
telemetry record and report for one run agree. The value shown above is
**illustrative**: it covers the whole recipe and changes whenever any key is added,
removed, or re-defaulted. Nothing asserts it as a constant.

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
    │   ├── encode.rs     # LinearImage → u16/f32 TIFF + ICC + sidecar
    │   ├── ultra_hdr.rs  # gain-map JPEG packaging (XMP/MPF + ISO 21496-1)
    │   ├── avif.rs       # AVIF container written here; libaom codes only
    │   └── staged.rs     # write to a temp beside the target, fsync, rename
    ├── pipeline/
    │   ├── film_base.rs  # Dmin estimation (pure)
    │   ├── color.rs      # working/output color transforms (lcms2, no copy)
    │   ├── colorimetry/  # every standards-based matrix + luma vector, with provenance
    │   ├── input_semantics.rs # transfer + measurement-meaning resolver (stage 1b)
    │   ├── working_space.rs   # NC film RGB v1 → linear ACEScg mapper
    │   ├── render_split.rs    # film-master bypass + the shared print controls
    │   ├── display_tone.rs    # resolves which display tone curve runs (+ its knee)
    │   ├── sdr.rs        # SDR display render (P3/sRGB, tone + gamut mapping)
    │   ├── hdr.rs        # Rec.2100 PQ/HLG display render
    │   ├── gain_map.rs   # SDR+HDR → canonical gain map
    │   ├── memory.rs     # peak-memory sizing model + budget preflight
    │   └── stages.rs     # stage wiring as pure functions
    ├── algo/
    │   ├── mod.rs        # FilmRgbImage + reconstruct/finish_print
    │   ├── simple.rs     # baseline inversion
    │   ├── density.rs    # density reconstruction + exponential curve
    │   └── sigmoid.rs    # sigmoid density curve
    ├── telemetry.rs      # opt-in JSONL perf/context record (never perturbs output)
    ├── version.rs        # build identity, pipeline_version, params hash
    └── types.rs          # LinearImage, FilmBase, Reconstruction, params, errors
```

The tree is the shipped module set, not a proposal — it had drifted by nine modules
and is worth re-checking whenever one is added.

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
| 6 | Resource limit — the run's estimated peak memory exceeds its budget. |

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

**Output write failures** map to exit **5**, and since
`io/transactional-output-writes` that exit carries a stronger promise: no truncated
artifact is left at a final path, and a failure while writing any of one conversion's
artifacts leaves *no* primary output rather than an orphaned one (§9 Output/encode).
A run that fails through an ordinary error path also leaves no `*.nctmp` staging files;
a run killed by a signal may leave one, since destructors do not run then.

**Memory preflight** (§9 Global, `--max-memory`) maps to exit **6**: before any
input is decoded, every command that reads a scan estimates the run's peak
allocation from a metadata-only header probe and compares it against the budget.
Over budget is a **resource** error, deliberately distinct from *unsupported*
(exit 4) — the input is fine; it is this run on this budget that cannot proceed,
so an agent can retry with a larger `--max-memory` (or on a bigger machine)
rather than discard the file. On `convert`, `inspect`, and `estimate` no image,
sidecar, or report is produced on that path — though `--dump-params`, which is
written during argument resolution, lands before the gate runs and so survives a
rejection. On **`roll`** the same rejection is
one frame's error: it is recorded in that frame's report entry, the roll continues
(sibling frames are converted and written), the report is emitted, and the roll
exits **1** — the batch-level "frames failed" code, as for any per-frame error.
An estimate that fits the budget but exceeds ~70% of detected physical RAM
is a `--strict`-promotable **warning**, not a failure. A malformed
`--max-memory` value is a usage error (exit 2).

Determinism note: the *image output* is unaffected by any of this, and the
pass/fail decision is machine-independent because the default budget is a fixed
constant. The **warning** tier is the one deliberately environment-dependent
piece — so under `--strict` the same input can exit differently on a small
machine than on a large one.

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
(item 2's sigmoid → `algo/sigmoid`; item 3's B&W rendering → `bw-support`;
plus `dmax-white-anchor`, `auto-neutral-wb`, and `regional-color-balance` from
the NLP feature comparison, Phase 6).

1. **IR-based dust & scratch removal.** Consume the IR channel (already preserved
   in Step 1) to build a defect mask and inpaint defects. Parameters: IR
   threshold, mask dilation/morphology, inpainting method/strength. Must handle
   the known limits — disable/guard for silver B&W film and Kodachrome. New
   stages: `defect_mask`, `inpaint`. New flags under an `--ir-*` namespace.
2. **Additional curve/reconstruction models.** The **sigmoid / explicit
   H&D-curve** model has since **shipped** as the tagged sigmoid density curve
   (§7.3, task `algo/sigmoid`); still open: possibly a power-law/exponent model
   (RawTherapee-style) for camera-scanned negatives. Added as a new tagged
   `reconstruction.curve` variant.
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
   **reassigned** to the dedicated `film-base/content-fallback` task (item 13)
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
    The `telemetry/strategy` spike is **complete**; its approved
    [design note](telemetry-strategy.md) fixes the remaining shape. The client
    keeps custom JSON (no embedded OTel SDK/Collector) and sends a separately
    versioned, allowlisted upload projection to an nc-owned Cloudflare Worker +
    D1 service. Persistent `nc telemetry enable` consent opts into automatic
    `convert` success/failure/panic collection and detached, crash-safe queue
    draining from exactly one consent-stored active JSONL plus its derived private
    sibling spool and immutable generation. Collection consent is an
    invocation-start snapshot: disable stops new snapshots/helpers and waits for
    bounded network requests, but an already-running convert may finish one local
    queued event afterward. Inactive-only purge waits those invocations. Other
    commands are out of v1; explicit per-run telemetry does not independently
    enable upload or install the panic hook. Active queue retargeting is rejected;
    inactive retarget requires the old queue empty. Purge preserves the private
    spool and stable lock inodes while clearing its data. Active same-path enable
    is a no-op; inactive same-path enable waits old invocations and the old helper
    before publishing a fresh generation and launching one replacement.
    `NC_TELEMETRY=0` disables automatic collection/networking. Upload carries no
    persistent identity, `params_hash`, exact paths/timestamps/dimensions/sizes,
    messages, recipe/parameter values, or raw backtraces. The implementation is
    split into `telemetry/schema-v2`, `telemetry/ingestion-service`,
    `telemetry/upload`, and `telemetry/panic-hook`; the latter is deliberately
    described as sanitized Rust **panic reporting**, not general native-crash
    capture. The anonymous endpoint cannot prove event provenance, so results are
    advisory/opt-in/unverified rather than exact population rates. V1 is hard
    capped in a dedicated Cloudflare FREE-plan account with no billing-enabled
    resources; any paid migration requires explicit approval. Note: the original
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
    `film-base/content-fallback`.
14. **Roll-fixed `Dmax` from a fully-exposed reference frame.** *(Implemented —
    `dmax-reference`.)* Supersedes the frame-local `auto` default: `Dmax` is a
    film+scanner calibration reused per roll like `Dmin`. The default
    `density.dmax = fixed` resolves reference → per-stock constant → a nominal
    corrected-density anchor (`Dmax = 1.3`, in density units — *not* base
    transmission plus a range); a value measured once from the light-struck leader
    (near-opaque in RGB, the max-density endpoint — always available) via
    `estimate --d-max-region` is frozen as `{ "explicit": <d> }`. `--auto-d-max`
    (per-frame exposure normalization) is demoted to opt-in. This changes the
    default render, which is a `pipeline_version` bump — **discharged** by
    `conversion-versioning` (item 16). Two boundaries, not one: making the density
    conversion the default was the v0→v1 bump and `pipeline_version 1` records it
    with the nominal at **2.0**; moving the nominal to **1.3** (with the sigmoid as
    the default curve) is the v1→v2 bump, so an archived recipe's render is only
    recoverable from the version its sidecar records. In the
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
16. **Conversion versioning & baseline comparison.** *(Built, not yet shipped —
    `conversion-versioning`.)* Every report carries an `identity` block (§9):
    build identity (crate semver + git commit + dirty flag + target), a behavioral
    `pipeline_version` (bumps *only* on default-behavior changes, gated by a golden
    drift test over the default render, the default film-base estimate, and the
    default recipe values — see §9 for what that gate does **not** cover; `0` = the
    `v0` baseline, `1` = current), and a resolved-params hash.
    It is mirrored into the sidecar only via the backward-compatible
    `{ "meta", "params" }` envelope — never as bare recipe keys, which would break
    the `--params` `deny_unknown_fields` round-trip; `--params` still accepts a bare
    legacy recipe. The benchmark manifest `scripts/analysis/benchmark.json` plus
    `python -m nctool compare run|diff` converts a fixed scan/recipe set under a
    build and diffs two builds keyed on `pipeline_version` + commit (per-channel
    mean ΔRGB, clip-fraction delta, per-stage timings); re-running one build yields
    a zero diff, where the verdict deliberately covers only the deterministic
    fields (timings are informational). Quality metrics (ΔE2000/SSIM)
    extend via item 7's QA harness; timings reuse the telemetry record. `v0` is
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
    SDR/HDR-specific tone, gamut, and transfer work. Legacy-preset TIFF
    ordering remains during migration. Optional correction profiles may
    explicitly neutralize declared scanner/film/development/lens behavior, but
    block no output task. Tracked:
    `negative-reconstruction-density-curves`,
    `film-rgb-working-space`,
    `film-master-render-pipeline`,
    `optional-color-correction-profiles`.
21. **Display P3 SDR output.** The SDR renderer solely maps ACEScg into rendered
    linear Display P3 or sRGB. It uses pinned AP1/D60→D65 target matrices,
    binding 203-nit reference white, the mandatory bounded Hermite
    baseline/additional shoulder, and same-luminance radial RGB-cube boundary
    gamut mapping; non-finite render arithmetic fails loudly. The opaque result
    couples its finite pre-transfer pixels to resolved gamut metadata. The
    destination output task derives its choice from that metadata, applies only
    the piecewise sRGB TRC, and attaches the matching deterministic ICC v4
    profile: the encoding is D65, while ICC PCS/media white is D50 with
    Bradford-adapted colorants and the required chromatic-adaptation tag. It
    performs no second ACEScg transform. CLI preset activation remains
    `output/presets` work. Tracked: `display-p3-output`,
    `sdr-display-rendering`.
22. **Display HDR rendering and format spike.** The spike selects 10-bit 4:4:4
    AVIF for single-rendition HDR and JPEG for gain maps. The spike's remaining
    gate is normative-text review; encoder conformance and device evidence are
    downstream pre-shipping gates. The implemented pure renderer consumes the
    shared adjusted ACEScg source, maps it into display-linear BT.2020 with a
    reference-white-preserving Hermite shoulder and same-luminance radial gamut
    compression, then encodes Rec.2100 PQ (primary still path) or explicit HLG.
    It fixes reference white at 203 cd/m² and peak at 1000 cd/m²; HLG records the
    1000-nit, zero-black reference OOTF with system gamma 1.2. Its typed linear
    seam feeds gain-map construction, while its in-place PQ/HLG seam carries the
    full-range CICP 9/16/9 or 9/18/9 contract for AVIF. Rec.2100 is an output
    encoding, not the density or internal working space. A separate encoder task
    owns AVIF v1.2 Advanced Profile conformance, AV1 High Profile level ≤ 6.0, container brands,
    oversized-image/grid behavior, metadata, codec bounds, and static
    libavif/libaom packaging. Tracked: `hdr-output-spike`,
    `hdr-display-rendering`, `hdr-avif-output`.
23. **ISO gain-map HDR and output presets.** Combine a valid Display P3 SDR base
    with the HDR rendition in JPEG, carrying final ISO 21496-1 metadata and
    Android Ultra HDR v1 compatibility metadata while requiring both Apple and
    non-Apple verification. Public terminology is standards-neutral
    (`gain-map-hdr`, not a platform brand). Both renditions share the identical
    mapped/adjusted film source; an RGB gain map is derived in common linear
    Display P3, never by dividing encoded P3 and PQ/BT.2020 values. Each ISO
    21496-1 and Ultra HDR v1 metadata dialect must independently reconstruct the
    same canonical HDR/headroom within pinned bounds, their parameter meanings
    must agree after linear/log2 unit conversion, and dual-aware decoders must
    prefer ISO when both are present. The 203/1000 ratio is linear headroom
    `4.926108...` but log2 capacity `2.300448...`. Before gain math, both linear
    Display P3 renderings use reference-white-relative units: SDR/reference white
    is `1.0`, and HDR absolute luminance is divided by 203 cd/m², making 203 nits
    `1.0` and 1000 nits `4.926108...`. Offsets use this same domain; mixing
    absolute-nit HDR with normalized SDR is a fail-loud unit error. After positive
    finite offsets are pinned, each per-channel gain is exactly
    `(HDR_c + offset_hdr,c) / (SDR_c + offset_sdr,c)` in common linear Display
    P3. Samples must be finite and nonnegative; offsets, adjusted denominators,
    and gains must be finite and positive before logarithm/serialization, with
    fail-loud handling rather than epsilon injection or `0/0`. Per-pixel extrema
    derive from this formula over the independently tone-mapped renderings and
    need not equal either global value. Both dialect serializers consume this
    same canonical calculation. Equal reference-white samples with equal offsets
    yield gain 1. A peak sample enters as `4.926108...`, but its gain still uses
    the actual independently tone-mapped SDR sample and offsets and is not
    assumed to equal display headroom. Once verified,
    `gain-map-hdr` becomes the default; explicit presets retain 16-bit TIFF
    Display P3 SDR and sRGB compatibility, linear ACEScg film master, PQ/HLG
    AVIF, linear/PQ/HLG HDR TIFF interchange, and custom workflows.
    `nc roll` naming/manifests migrate with presets so suffixes derive from each
    resolved container and per-image sidecars derive from final image paths. One
    roll report remains on stdout or explicit `--report-file`, collision-checked
    against all batch inputs/outputs/sidecars. Core full-size TIFF/resource verification remains independently runnable;
    final gain-map/preset metadata, faithful film-rendering consistency, and
    cross-device behavior are a separate gate.
    The standards-derived matrices, luma coefficients, primaries, and transfer
    definitions are consolidated and made auditable before the lossless HDR TIFF
    encoders add another profile/signaling surface. The TIFF task owns exact
    float/code-value round trips and truthful signaling/interoperability claims.
    Tracked: `gain-map-hdr-output`, `hdr-avif-output`,
    `colorimetry-source-of-truth`, `lossless-hdr-tiff`, `output/presets`,
    `display-output-acceptance`.

## 13. Open questions

All of the Step-1 open questions have since been resolved (kept here as a record):

- ~~Exact on-disk SilverFast HDRi tag/channel layout~~ — **resolved 2026-06**:
  reverse-engineered and verified against real sample files; documented in §4
  (separate full-resolution grayscale IR IFD, optional preview IFD, structural
  HDR/HDRi detection).
- ~~Which wide-gamut space to use for the target `film-master` output~~ —
  **resolved**: **linear ACEScg**. The current `--out-depth f32` path can tag its
  rendered float values as ACEScg but is not that master. The target branch lands
  after NC film RGB v1 mapping and before print/display controls; it is not
  physical scene recovery or Rec.2100 display HDR. See §5.
- ~~Whether the embedded TIFF metadata should carry the full recipe~~ —
  **resolved**: the recipe lives in the sidecar JSON only (paired by name with
  the output); the TIFF embeds just the ICC profile. See §5.
