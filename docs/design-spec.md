# Hanten — High-Level Design Spec (Step 1)

> Target: Step 1 (MVP) · Language: Rust

## 1. Purpose

**Hanten** (the binary is `hanten`) is a command-line tool that reads a **film
negative scan** (SilverFast HDR/HDRi format first) and produces a **positive
image**. Every step of the conversion is controlled by explicit CLI parameters so
that an automated agent — or a human — can drive the full pipeline reproducibly.

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
- **Density reconstruction** (shipped): density-domain reconstruction, Kodak Cineon /
  darktable `negadoctor` style, owning one **exponential** density curve (the straight
  line, at the fixed decode's configuration since `pipeline_version` 6). The `simple`
  channel inversion and the **sigmoid** S-curve (the default from `pipeline_version` 2
  to 5) retired in `nf-retire/sigmoid-and-simple`, and the **characteristic** curve (a
  stock's published curve, inverted) in `nf-retire/characteristic`; §7.1 and §7.3 keep
  their record.
- All conversion parameters controllable via CLI flags and/or a JSON recipe file.
- Write **TIFF** output, selectable as **16-bit integer** or transitional
  **32-bit rendered float** via a flag.
- Auto-estimate film base (`Dmin`) from the unexposed border, with full CLI override.
- JSON report output (estimated parameters, warnings) and JSON recipe load/dump.

### Out of scope (Step 1) — see §12 Roadmap

- IR-based dust/scratch removal (follow-up task).
- Additional reconstruction/curve models beyond those listed above (follow-up
  tasks; the sigmoid / explicit H&D curve shipped post-MVP as the tagged `sigmoid`
  density curve, §7.3, and has since retired).
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
8. **One recipe per roll, not per frame.** Calibration (`Dmin`, the stock's
   response, a roll's white balance) is measured per roll and then *frozen*: Hanten does not
   auto-optimize each frame to its own content. Frames from one roll stay
   comparable, and a difference between two of them is a difference in the
   scene rather than in the tool's reaction to it. This is deliberate and it is
   the largest behavioural difference from per-frame converters — measured
   against Negative Lab Pro on three frames of one roll, Hanten's `p95 − p5` moves
   **0.96 stops** where NLP's moves **4.3–4.5** (see
   `docs/progress/analysis.md`, `analysis/nlp-comparison`). Per-frame adaptation
   is a possible **opt-in** later, never the default, and at low priority. The
   consequence for contrast is an open question, not a settled trade —
   `algo/contrast-latitude-spike`.

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
values in every space; the lone exception is the anchor `A` (a scalar, below). The IR
plane is a separate single channel, carried but not consumed (§6.1).

| Space | Meaning | "Higher" means | Range (`f32`) | Where in code |
|---|---|---|---|---|
| **transmission** (raw scan value) | fraction of light the film passes | more transparent film, thinner negative, brighter pixel *in the raw scan* — a **darker** scene | `[0, 1]` (= `u16`/65535) | `io::decode`, `LinearImage.rgb` |
| **film base / `Dmin`** | the unexposed rebate's transmission — the per-channel *relative* maximum transmission | (the ceiling of transmission) | `(0, 1]` | `FilmBase`, `film_base::estimate` |
| **density `D` / `D′`** | `D = −log10(scan / Dmin)`, log-scale opacity; `D′ = density_scale·D + density_offset` (per-channel corrected density, §7.2) | **denser** negative — a **brighter** scene | `D`: `0` at base, `≈ [0, 6]` (slightly `< 0` if a pixel out-transmits the base); `D′` shifted by the offset | `density::to_density`, `DensityImage.density` |
| **`D′` at the reconstruction→curve handoff** | the same corrected density `D′` (row above), named at the point it is passed to the selected density-to-positive curve | **denser** negative — a **brighter** scene | density units — `D′`'s range as defined in the row above (no re-clamping at the boundary) | reconstruction→curve handoff inside `density::reconstruct` |
| **NC film RGB v1** (`FilmRgbImage`) | intentional positive film rendering from the exponential density curve or the fixed decode; interpreted consistently as linear Rec.709/D65 | **brighter** positive — a **brighter** rendered scene | curve-defined and unclamped `f32` | `algo::FilmRgbImage`, `algo::reconstruct` (shipped typed reconstruction output) |
| **ACEScg film rendering** (`AcesCgImage`) | NC film RGB v1 transformed/adapted into linear ACEScg/D60; preserves film/lens/development/scanner character and is not physical scene recovery | **brighter** rendered value | unclamped `f32`; nominal diffuse white is workflow-defined | `pipeline::working_space` mapper (implemented; every preset crosses it) |
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

**The anchor `A`** is the corrected density `D′` that renders to positive `1.0`. It
is a **scalar** pooled across channels (a per-channel anchor would apply three gains
in `10^(γ·(D′ − A))`, i.e. a white balance, which is the print-render stage's job) and
is **derived from the film base**, never measured: `A = d + 0.745/contrast` with
mid-grey at `d` above the base (§7.2). It is reported, not an input.

**`Dmax`** (historical) named the roll-fixed **reference density** the anchor used to
be placed from — a nominal constant, a value measured from a light-struck leader, or a
per-frame percentile. `nf-retire/dmax-machinery` retired it: **it is no longer an input
anywhere**, and nothing measures, reads or reports it. ⚠️ Distinct from classic
photographic film `Dmax` (the negative's physical maximum density), from diffuse white
(a datasheet reference number, `d + 0.36`), and from a roll's content white (not
measured) — `src/algo/fixed.rs`'s "Five quantities" table keeps them apart.

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
  `--output-preset <gain-map-hdr|ultra-hdr-v1|display-p3|compatibility|film-master|hdr-pq|hdr-hlg|hdr-linear-tiff|hdr-pq-tiff|hdr-hlg-tiff>`
  / recipe key `output.preset` (**default `gain-map-hdr`** since `pipeline_version` 3).
  Exactly **ten** names are accepted — there is no planned-but-unaccepted tier, so an
  unknown name always means a typo; the pre-release `scene-master` is rejected as an
  unreleased-schema break (no alias). **Every preset is atomic**: it resolves
  container, depth and profile itself, and no other knob states them.
  `hanten convert -o out.tif` with no preset is a usage error naming the accepted
  suffixes.
- **Retired (`nf-retire/legacy-custom`):** the `legacy` and `custom` presets — the
  older TIFF path that ran the print controls on film RGB before an output ICC
  transform — and the three selectors only they read, `--out-depth` /
  `--output-profile` / `--bigtiff` (recipe keys `output.depth`,
  `output.output_profile`, `output.bigtiff`). Each is a removed-value usage error
  (exit 2) naming its replacement (`display-p3`/`compatibility` for a 16-bit TIFF,
  `film-master`/`hdr-linear-tiff` for float); there is no alias. One exception keeps
  old recipes replayable: a recipe key carrying the value every earlier build wrote
  by default (`"depth": "u16"`, `"output_profile": null`, `"bigtiff": "auto"`) is
  dropped on load, since it asked for nothing. The old rendering is
  reproducible only from the reference build (`scripts/reference-snapshot/`).
  ProPhoto and user-ICC output left with them; BigTIFF promotion is always automatic.
- **Current implemented bit depth:**
  Depth always follows from the preset.
  - default (no preset) → the `gain-map-hdr` JPEG. The 16-bit integer TIFF is
    `--output-preset display-p3` / `compatibility`.
  - `--output-preset film-master` → 32-bit float TIFF, unclamped, taken directly
    from the NC film RGB v1 mapped linear ACEScg with the ACEScg profile embedded
    and **no** working→output transform, print control, or display rendering.
  - `--output-preset gain-map-hdr` / `ultra-hdr-v1` → fixed 8-bit Display P3 SDR
    primary JPEG plus a half-resolution grayscale gain-map JPEG. Identical pixels;
    they differ only in the metadata attached — `gain-map-hdr` carries ISO 21496-1
    segments in both images *and* the legacy Ultra HDR v1 XMP/MPF, `ultra-hdr-v1`
    only the latter. Apple platforms read the ISO dialect alone, so `gain-map-hdr`
    is the form that decodes as HDR there.
  - `--output-preset hdr-pq` / `hdr-hlg` → fixed 10-bit, full-range, 4:4:4 AVIF
    (AV1 High Profile). Both require an `.avif` output path.
  - `--output-preset hdr-pq-tiff` / `hdr-hlg-tiff` → fixed **16-bit unsigned**
    TIFF holding full-range Rec.2100 PQ/HLG code values (the primary *and* the
    optional IR plane). Both require a `.tif`/`.tiff` path.
  - `--output-preset hdr-linear-tiff` → 32-bit float TIFF holding the HDR
    renderer's **pre-transfer display-linear BT.2020/D65** samples verbatim, with a
    synthesized linear-BT.2020 ICC profile. Bit-exact: nothing is clamped,
    normalized, or transfer-encoded, so samples run from black past the 203 cd/m²
    reference white (`1.0`) to the 1000 cd/m² peak (≈4.926108) — verified on a real
    18.66 MP scan whose maximum sample is exactly that headroom with 7.92% of
    samples above reference white. It is the float TIFF that has been through the
    print controls and display rendering, unlike `film-master`. Requires a
    `.tif`/`.tiff` path. Because the ICC PCS stops at the media white, the
    profile cannot express those luminance semantics: the report's
    `hdr_linear_tiff` block and the sidecar are authoritative for reference white,
    peak, and headroom, and the profile must never be claimed to carry them.
- **Color selection:** each preset embeds the profile its pixels are in — Display P3
  (`display-p3`, the gain-map base), sRGB (`compatibility`), linear ACEScg
  (`film-master`), and the synthesized BT.2020/Rec.2100 profiles of the HDR presets.
  A display preset's renderer produces pixels already in its destination primaries,
  so the encode applies only the transfer function, never a second gamut transform.
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
  - `hdr-hlg-tiff` — losslessly stored 16-bit BT.2020 / Rec.2100 HLG TIFF.
  A preset resolves container, bit depth, primaries/profile, transfer function,
  tone/gamut mapping, and metadata together. The old `--output-hdr` name was
  therefore temporary/ambiguous and will not be used to mean both float master
  data and display HDR. The output path remains required and is never silently
  renamed: a suffix it states must match the preset's resolved container or
  conversion fails with the accepted suffixes. A suffix it *omits* is **completed**
  from that container — appended, never substituted, so no byte that decides *which
  file* is named is altered or dropped. (Redundant separators and interior `.`
  segments still normalise, as the platform's path rules do; they denote the same
  file.) The rule that follows from this: a path with **nothing to append to** is
  refused rather than completed — whenever the trailing component does not name a
  file, whether because there is no file name at all or because it names a
  directory, since completing would silently write that directory's sibling.
  `film-master` branches directly from NC
  film RGB v1 mapped linear ACEScg and bypasses white balance, exposure, black/range placement,
  highlight compression, and all display tone/gamut rendering; the float output that
  does apply them is `hdr-linear-tiff`. An explicitly selected measured correction is exempt: it
  remains `film-master` and must record profile identity/hash/scope provenance. This
  master contains intentional film/lens/development/scanner character and is not
  physical scene-linear recovery. After recipe/CLI merge it rejects any
  non-default downstream WB/exposure/black/white/highlight/tone/gamut/display-transfer
  control; there is no silent ignore mode. Measured correction alone does not
  rename the preset. **As shipped**, that rejection runs on the *resolved* config
  (`cli::validate`), so a value is rejected identically whether it came from a
  recipe, a flag, or a removed simple-control migration — and a flag that resets a
  recipe value back to its documented default is legitimately accepted, which is how
  a roll recipe carrying print controls can still be re-exported as a master.
  The resolved-branch record lands in the report as `output_render` (§8). The
  suffix table in `cli::required_extensions` is now **complete**: `.jpg`/`.jpeg`
  for `gain-map-hdr` and `ultra-hdr-v1`, `.avif` for `hdr-pq`/`hdr-hlg`, and `.tif`/`.tiff` for
  `hdr-linear-tiff`, `hdr-pq-tiff`, `hdr-hlg-tiff`, `display-p3`,
  `compatibility` and `film-master`. `film-master` (and the retired `legacy`) once
  pinned no row, so `hanten convert -o out.jpg` wrote a TIFF named `.jpg` with exit 0
  and no warning.

  Since every preset states a container, an **extensionless** output path
  (`-o positive`) does not need one: Hanten **completes** it, writing
  `positive.jpg` under the default and `positive.tiff` under a TIFF preset. The
  preset exists to encapsulate the container, so requiring the user to restate it
  was the requirement backwards. (It was an exit-2 usage error between the suffix
  table's completion and this change.) The spelling Hanten supplies is one
  canonical choice per container — `tiff`, `jpg`, `avif` — deliberately separate
  from what it *accepts*, whose TIFF list heads with `tif`.

  Completing is not renaming, and the distinction is the rule: **a stated suffix is
  never rewritten, only an absent one is supplied.** A trailing dot-segment counts
  as a suffix only when it is a spelling *some* preset accepts, so `-o out.tiff`
  under a JPEG preset is still the usage error below, while `-o out.v2` and
  `-o roll-1.2` are stems and keep their dot (`out.v2.jpg`). A stated spelling the
  container accepts survives byte for byte, case included: `-o out.jpeg` writes
  `out.jpeg`, never `out.jpg`. The completed path is what the report's `output`
  field, the sidecar, and the write-target collision guard all name — an agent
  reads the container from the report and never infers it from the preset name.

  For a **stated** suffix the container refuses, the **diagnosis varies on where
  the preset came from, not on
  which preset it is**: that stopped being derivable from the value once the default
  became a *named* preset, since `gain-map-hdr` now arrives both ways. A preset the
  user selected — by `--output-preset` **or** by `output.preset` in a `--params`
  recipe — is blamed by name. With neither, the message names the default explicitly
  ("with no `--output-preset`, Hanten writes `gain-map-hdr`") rather than pointing at a
  flag that is not in the command line. Both halves matter: reporting a
  recipe-selected `display-p3` as the no-preset default states something false and sends
  the reader hunting for a default that does not exist.

  **Roll capability is a separate axis**, not derived from that table. It used to
  be ("pins a suffix" ⇒ `convert`-only), and that inference died when the table
  was completed — deriving it would refuse every preset and leave `hanten roll` with
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

- `film-master` — `pipeline::stages::render_film_master` owns
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
  4:4:4 AV1 (High Profile) inside a Hanten-written MIAF container. `av1C` is filled from
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
is keyed on the *branch*, not on a preset list: only `film-master` rejects that
control, under its own bypass rule. (The retired `legacy` / `custom` branch ran the
print controls on film RGB before an output ICC transform; `pipeline::stages::golden`
still pins the reconstruction it shared with every branch.)
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
                 │ 3. Density reconstruction                      │
                 │    density curve: exponential                  │
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
task `input-data-semantics`; see §4 and §9). Stage 3 is density reconstruction:
density parameters plus the exponential curve, which places its anchor from the film
base alone (§7.2) and reads no reference density. It returns private-field
`FilmRgbImage`.

Stage 4 defines **NC film RGB v1** as the existing intentional interpretation of
that film rendering as linear Rec.709/D65, followed by the pinned standard
transform/adaptation into linear ACEScg/D60. It returns private-field
`AcesCgImage`. This one mapping is shared by every density curve and the fixed decode,
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

White balance, exposure, black/range placement, highlight compression,
and output tone/gamut mapping live after ACEScg on the display branch. The
`film-master` branch bypasses them and encodes stage 4 directly, and records that
it contains intentional film rendering rather than a physical scene-linear
recovery.

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
renderers have to remember. The display tone is deliberately **not applied** by the
shared stage: its one parameter, `fit_range.headroom_stops`, resolves once into a
checked headroom that each named display branch applies in its own domain, so SDR and
HDR cannot disagree about which curve ran. Every display preset applies it;
`film-master` has no display tone stage and refuses a non-default headroom (§9). (The
`shoulder` and `none` tones and `highlight_compress` retired in
`nf-retire/display-tones`: both tones existed for reconstructions bounded at white, and
none ships.)

The resolved SDR policy is deterministic and display-referred. It transforms
AP1/D60 ACEScg to the selected D65 destination (Display P3 or sRGB) with pinned
AP1→XYZ, Bradford D60→D65, and XYZ→destination matrices; no installed ICC or CMM
participates in rendering. Adjusted linear `1.0` is the binding **203 cd/m²
reference white**. The branch applies extended Reinhard
`u·(1 + u/W²)/(1 + u)` over `u = gain·v`, against the white point
`W = 2^headroom_stops`. The input `gain` is solved so **scene mid-grey (`0.18`) is
preserved exactly at every white point**, which costs the operator its old property of
mapping `W` to reference white: no member of this family can do both (the curve's
white-to-mid ratio cannot fall below 6.17, while pinning both ends needs 5.56), so `W`
is the curve's scale parameter and the unity point sits at `W / gain`. That trade lands
where this branch already accepts overshoot: its output is **not** bounded by the
branch's ceiling — that is its purpose, to carry specular content several stops above
diffuse white — so content above `W` still exceeds `1.0` and its loss is counted at the
u16 encode boundary rather than refused. **`headroom_stops = 0` is the exact identity**
(`W = 1`), and there nothing rolls an overshoot off, so a sample above reference white
is a loud pixel-specific error instead of a clip — the self-policing the retired `none`
tone provided. The **HDR** branch applies a lifted form of the same operator:
`f(v) · (1 + (C − 1)·s(v))`, with `s` a smoothstep in `log₂` from reference white to
the white point and `f` taken **asymptotically** (`v/(1 + v)`), so the composite
stays strictly inside the 1000-nit peak instead of plateauing at it. Below reference
white the lift is identically zero, which is what lets an HDR and an SDR rendition of
the same frame agree there — the condition a gain map needs. The **gain-map presets take it too**, and what
made that safe is narrower than "both renditions in range": the ratio must be taken
against the base **as stored**, because that is what a decoder multiplies and the encode
clamps it. Ratioing against the *rendered* SDR stored a gain short by whatever was
clamped, reconstructing up to 23% dark; the per-channel gains and the legacy luminance
map therefore both ratio against `min(sdr, 1)`, which is the admission condition and not
a relaxed check. The
renderer then maps out-of-gamut color to the RGB-cube boundary with one common
chroma scale around the same-luminance neutral axis. This preserves the neutral
axis and chroma direction instead of independently clipping channels. The cube
ceiling **follows the pixel** (`max(1.0, this pixel's rendered luminance)`) rather
than being pinned at `1.0`: it keeps the boundary continuous above display white, so
highlights desaturate toward white instead of gaining a hard ring where full chroma
snaps back. Finite
input that produces non-finite matrix/tone/gamut arithmetic is a loud conversion
error naming the pixel; the renderer never substitutes black or white. Its typed
result owns finite non-negative **pre-transfer**, destination-linear pixels
together with the resolved gamut metadata. Values above `1.0` are carried to the u16
encode step and counted there (`EncodeReport`, `--strict`-promotable); at zero headroom
the input was already refused above reference white, so an output outside the cube is a
loud renderer error. Negativity is a hard error at every headroom — nothing downstream
is defined below black. The radial
intersection is
calculated in binary64 and sets its limiting channel to the exact computed cube
boundary; this is the gamut mapping itself, not a terminal per-channel clamp. A separate
destination stage derives the matching Display P3 or sRGB profile from that
metadata, applies only the piecewise sRGB transfer curve, and returns the
metadata with the encoded pixels. The gain-map path borrows the typed
pre-transfer pixels.

The resolved HDR policy uses the same headroom in a distinct reference-white-relative
domain: the lifted form described above. Adjusted linear `1.0` remains the binding
**203 cd/m² reference white**, and the fixed peak is `1000/203 = 4.926108...`, which the
lifted form's asymptotic base keeps every sample strictly under — so a sample above it
is a renderer error, never a clip. At zero headroom the lift has no span and the
operator is the identity, so input above the peak is refused naming the pixel; content
between reference white and the peak renders, which is the headroom an HDR rendition
exists to carry. After the tone, same-luminance radial gamut
mapping produces display-linear BT.2020. Single-rendition output transfer-encodes
that typed value as PQ/HLG; gain-map construction must first transform it to the
common linear Display P3 domain used by the SDR rendition.

### 6.1 IR channel handling (Step 1)

The IR plane (when present) is decoded and carried alongside RGB. With one
exception it is **not consumed** by any conversion stage in Step 1: when the scan
carries a marker-verified IR plane that **measures able to separate holder from
film on that frame**, **film-base estimation (stage 2) consumes it** — the opaque
scanner holder reads dark in IR while IR-transparent film (base, rebate, picture)
reads bright, so holder-occluded spans are excluded from the auto rebate/`Dmin`
search (the `ir-holder-detection` feature, adjacent to the roadmap's IR item 1;
§9 film base).

The usability verdict is **measured, not declared** (`ir-usability-detection`).
The interior IR transmission is sampled and compared against a threshold
(2.5x the holder classifier's); below it, film and holder cannot be told apart and
detection falls back to RGB-only with a report warning naming the measurement.
`--film-type` does not gate this: silver-halide blocks IR *in proportion to
accumulated density*, so an unexposed silver frame is IR-transparent against an
opaque holder (measured ~20:1) while its own fully-exposed leader is opaque
throughout — the declared chemistry mispredicts both.

Detection falls back to RGB-only in **four** cases: no IR plane; an IR page
identified by shape alone; a frame measured unable to separate holder from film;
and a mask that classifies *every* edge entirely as holder, which would leave the
rebate search no span to scan on any edge and is therefore strictly worse than not
masking (the RGB-only search still scans inward past the holder). In the last case
the plane is carried but unconsumed, and the report says so — the orchestrator
reads what stage 2 actually did rather than predicting it from the inputs. Otherwise the IR plane
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

The shipped implementation is density reconstruction through one curve, the
exponential. `simple` and the sigmoid retired in `nf-retire/sigmoid-and-simple`
(`pipeline_version` 6), and the per-stock `characteristic` curve in
`nf-retire/characteristic` (no pixel moved: it was never the default). Their flags
(`--density-curve`, `--film-stock`, `--preset`), recipe values and presets are
migration errors, and §7.1 / §7.3 below are kept as the record of the first two — the
reference build (`scripts/reference-snapshot/`) still renders all three. Every reconstruction path returns the
typed `FilmRgbImage` boundary (`algo::reconstruct`), so only the working-space
mapper (`pipeline::working_space`) can construct `AcesCgImage`, and every preset
crosses it. The pre-reconstruction
`--algorithm simple|density|sigmoid` selector (a boxed `Converter` returning an
untyped `LinearImage`) is **removed** — the flag and the old recipe forms are
rejected with a migration error (Hanten is unreleased; no aliases).

### 7.1 `simple` — inversion baseline (retired)

> **Retired** in `nf-retire/sigmoid-and-simple`: an affine inversion of the scan rather
> than a decode of the film. `--reconstruction` and `reconstruction.type = "simple"` are
> migration errors; the text below records what it was.

Channel inversion plus white balance / border neutralization. Cheap, predictable,
useful for B&W negatives and as a debugging reference. Not a strong endpoint for
color negatives (ignores density behavior and the orange mask).

The pre-reconstruction converter ran `positive = 1 - scan/Dmin` →
`invert_white_balance` gain per channel → the `clip_low`/`clip_high` affine
black/white remap. In the shipped pipeline, stage 3 ends at unclamped
`U_c = 1 - scan_c/Dmin_c` and returns `FilmRgbImage`.
Inversion WB and clip remapping move after the ACEScg boundary to the downstream
shared WB/black/range-placement contract. **As shipped**, both replacement homes
now exist — explicit `print.white_balance` and `print.linear_range` /
`--linear-range LOW,HIGH` — and every display preset consumes them (see §6);
`film-master` bypasses print controls. The old flags and
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
new recipes/reports emit only replacement names. Every preset applies the values
only after NC film RGB mapping. Aliases preserve requested values, not the old
pixels: per-channel gains generally do not commute with the working-space
matrix. Target activation warns; because preset/default pixels change,
`conversion-versioning` owns the corresponding golden-tested
`pipeline_version` bump.

### 7.2 `density` — density-domain inversion (default)

The credible baseline for color negatives, following Kodak Cineon / darktable
`negadoctor` ideas:

```
1. transmission → density:   D  = -log10(scan / Dmin_transmission)   (per channel)
2. density correction:       D' = per-channel scale·D + offset (orange-mask comp)
3. density curve:            exponential { gamma, anchor }
4. typed film positive:      FilmRgbImage
5. NC film RGB v1 mapping:   linear Rec.709/D65 → linear ACEScg/D60
6. print/display controls:   white balance, exposure, black/range placement
```

Steps 1–2 are density reconstruction. The curve owns the positive mapping: it renders
`10^(gamma·(D'−A))` with a scalar anchor `A`, and returns the typed film RGB boundary
before the shared working-space transform. Step 6 runs after the ACEScg
boundary, in the shared display stage (`render_split`).

**Polarity.** With `D = -log10(scan / Dmin)` the density is `≥ 0` and *grows* with
the film's optical density — the unexposed base (scene black) sits at `D = 0`, a
dense negative area (a scene highlight) at large `D`. A positive must brighten as
`D` grows, so step 3 uses `10^(+gamma·D')`, **not** `10^(−gamma·D')` (which would
reproduce the negative). This matches darktable `negadoctor` (denser negative →
brighter print).

**Curve-stage anchor.** The exponential's anchor `A` is the corrected density that
renders to `1.0`; the base (`D′ = 0`) renders to `10^(−gamma·A)`. `curve.anchor` holds
the placement rule, and one rule ships:

| `curve.anchor` | anchor `A` | meaning |
|---|---|---|
| `{"mid-at-base-offset": d}` (default `d = 0.62`) | `d + 0.745/contrast` | mid-grey (18%) renders at density `d` *above the film base*; display white falls `0.745/contrast` above it |

`0.745 = −log10(0.18)` is mid-grey's fixed distance below white on the *output* axis.
At the defaults (contrast 2.0) `A ≈ 0.99`; the new chain's decode runs at its
linearization alone (1.8, `A ≈ 1.03`) and leaves the rest of the contrast to the look
(§9, `reconstruction.linearization` and `look.contrast`). The rule is **reference-free**: it reads only
the film base, which step 1 divides out, so the base *is* `D′ = 0` by construction
(modulo `density.offset`). No leader or reference density
enters the render, so a leader's roll-to-roll error cannot reach it, and nothing is
measured per frame — the rule is a roll-level placement, never derived from frame
content. The other three placements — `white-at-dmax` and `mid-at-dmax-fraction`, which
read a roll reference density, and `black-at-base`, which pinned the base to an output
floor — retired with that reference in `nf-retire/dmax-machinery`; a recipe naming one is refused naming `mid-at-base-offset`.
`AnchorPlacement` stays a tagged enum, the form a content-referenced placement would
need. `nf-calibration/anchor-comparison` placed the roll's white through `look.contrast`
instead, leaving the anchor base-referenced, so no such placement is planned.

A recipe curve without `gamma` or `anchor` warns that it will pick up this build's
(moved) defaults: this build always writes both keys, so their absence marks a file
some other build wrote.

**The `characteristic` curve — retired** in `nf-retire/characteristic`. It inverted a
named stock's published per-channel curve (`--film-stock`), reading its slope and
mid-grey placement off the sheet, with an identity `density.scale` of its own. The fixed
decode is stock-agnostic by design (`docs/design-update.md` Part 1); the digitized tables
stay, test-only, as the evidence for its constants (`film_stock/`), and per-stock
normalization would return as an optional look control with its own flag. A recipe naming
the curve or a `stock` is refused, naming the `density.scale` its sidecars carry at
`[1, 1, 1]`.

**Regional (shadow/highlight) balance — retired** in `nf-retire/regional-balance`. Step 2
used to add per-channel density offsets ramped by tone between a shadow and a highlight
density (`reconstruction.density.shadow_balance` / `highlight_balance`), with the ramp's
range measured per frame (`balance_range`, default `auto`). It was per-channel adjustment
by tone region acting on film density before the 3×3, unbounded (a large difference
between its ends folded two densities onto one), and its `auto` range was a per-frame
measurement a roll stayed consistent under only by measure-once-replay. A crossover is now
the look's per-channel grade (§9, `look.channel_grade`), pivoted at a fixed mid-grey. The
flags and non-neutral keys are migration errors; the neutral keys every earlier sidecar
carries are dropped on load.

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

`--highlight-compress` retired in `nf-retire/display-tones` with the `shoulder` tone
whose knee it placed (its other meaning, an above-`1.0` soft clip on film RGB, had
retired with the `legacy` preset); highlight roll-off is the display tone's, sized by
`--display-tone-headroom` (§6).

### 7.3 `sigmoid` — density-domain S-curve (H&D / paper response) (retired)

> **Retired** in `nf-retire/sigmoid-and-simple` (`pipeline_version` 6): its shoulder is a
> rendering fused into the decode. The default became the exponential at the fixed
> decode's configuration — the sigmoid with both knees off, at a base-derived anchor.
> `curve.type = "sigmoid"`, the `--sigmoid-*` flags and the `sigmoid-*` presets are
> migration errors; the text below records what it was. The base-offset anchor it
> introduced is the exponential's one placement (§7.2).

An S-shaped tone curve in density space, giving the shoulder/toe control of a
photographic H&D / print-paper characteristic instead of the `density`
algorithm's straight `10^(gamma·(D'−A))` line. It shares density correction
(steps 1–2 and their parameters, §9) and the later print/display render
(`print.*`) with `density`; only step 3 — the density → positive curve — is
replaced:

```
A = anchor(contrast)                           anchor density: the D' rendering to 1.0 (§7.2)
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
density, for any valid params, so under neutral print params the default u16 encode **reports no clipped
highlights** (the later print/display render — exposure/gains — can still lift samples
above `1.0`).

With `shoulder = 0` there is no roll-off and
highlights follow the (toe-shaped) line, which can exceed `1.0` like `density`.
The toe holds shadows to the paper-black floor `≈ 10^(−contrast·A)` (exact
when `shoulder = 0`; the shoulder nudges it imperceptibly lower otherwise).
`toe`/`shoulder` are knee widths in log10 density units; `contrast` is the
mid-density slope in log-output space. With `toe = shoulder = 0` both knees are
skipped and it reduces to the exponential's straight line — which is how the fixed
decode's default was derived from it.

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

// Working-space boundary (film-rgb-working-space, `pipeline::working_space`):
// implemented; every preset crosses it. Named output code
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

A single binary (`hanten`) with subcommands. The agent-facing surface is
optimized for scripting: flags for everything, JSON in/out, stable exit codes,
no interactive prompts.

### Subcommands

| Command | Purpose |
|---|---|
| `hanten convert` | The main pipeline: negative file → positive image in the resolved preset's container (a gain-map JPEG by default; a TIFF or AVIF under the presets that say so). |
| `hanten roll` | Convert a batch of frames from one shared, frozen recipe (the batch-**apply** scaffold). Per-frame outputs into `--out-dir` + a roll-level JSON report. Single-frame `convert` is unchanged; roll is additive. |
| `hanten inspect` | Read a scan and emit a JSON report of format, channels, bit depth, candidate rebate regions (coordinates + spread, ready for `--base-region`), suggested `Dmin`. No output image. |
| `hanten estimate` | Run only film-base/`Dmin` estimation; emit JSON with a reuse-ready `--film-base` flag and a `calibration` object in recipe shape. `--grid` adds 5-cell agreement-checked sampling for blank reference frames. |
| `hanten measure-roll` | Measure a roll's white balance once, for its new-chain recipe (`nf-scene-correction/roll-white-balance`): decode every picture frame with the roll's explicit film base, pool the effective areas' pixels, and report the green-anchored gains that equalize their per-channel p99 — as a reuse-ready `--white-balance` flag and a `scene_correction` recipe fragment. `--leader` leaves out any pixel within 0.1 density of the leader, so a fully exposed frame cannot become the roll's white; without it the run warns. |
| `hanten params`  | Print the full default/effective parameter set as JSON (for discovery and recipe scaffolding). The scaffold is a **template to edit, not a runnable recipe**: `calibration.film_base` has no default, so it prints as `null` and `convert`/`roll` reject it until you state a base. |

### Recipes (JSON in/out)

- `--params recipe.json` — load a full parameter set from JSON.
- `--dump-params out.json` — write the effective parameters (defaults + overrides)
  to JSON. Individual `--flag` overrides take precedence over the loaded recipe,
  so an agent can load a roll recipe and tweak one value per frame.

The shipped recipe is grouped into `reconstruction`, `input`, `calibration`,
`measure`, `print`, and `output`. The roll's measured values live in
`calibration` — see "The calibration section" below. The reconstruction is exactly
one `reconstruction` object; the removed legacy forms (top-level `algorithm`, the
sibling `density`/`sigmoid`/`simple` sections, the top-level `film_base` section,
`reconstruction.curve.dmax`, a `"sigmoid"` or `"characteristic"` curve, a
`curve.stock` and `"type": "simple"`) are rejected at recipe load with a migration
error — they are not aliases. Both `type` selectors retired with what they selected
between — `reconstruction.type` with `simple`, `reconstruction.curve.type` with
`characteristic` — so neither is written, and the old values every earlier sidecar
carries (`"density"`, `"exponential"`) are accepted and dropped. This is the complete
reconstruction shape (other stage objects are omitted here):

```json
{
  "reconstruction": {
    "schema_version": 1,
    "density": {
      "scale": [1.0, 0.84, 0.73],
      "offset": [0.0, 0.0, 0.0]
    },
    "curve": {
      "gamma": 2.0,
      "anchor": {"mid-at-base-offset": 0.62}
    }
  }
}
```

That example is the **resolved default document** as of `pipeline_version` 6 —
copying it reproduces the shipped render, the fixed decode's configuration. The
curve's fields can all be restated (here with a lower mid-grey placement):

```json
{
  "reconstruction": {
    "schema_version": 1,
    "density": {
      "scale": [1.0, 0.84, 0.73],
      "offset": [0.0, 0.0, 0.0]
    },
    "curve": {
      "gamma": 2.0,
      "anchor": {"mid-at-base-offset": 0.5}
    }
  }
}
```

`reconstruction.schema_version` is exactly `1`. Partial input may omit it and
defaults to 1; resolved recipes always emit it. `curve.anchor`
accepts `{"mid-at-base-offset": <d>}` with `d > 0`, defaulting to
`{"mid-at-base-offset": 0.62}` — the fixed decode's rule (§7.2). A retired placement
(`"white-at-dmax"`, `mid-at-dmax-fraction`, `black-at-base`) is refused naming it. The
sigmoid's keys (`contrast`, `toe`, `shoulder`) are refused naming it. Omitted
density fields take the displayed defaults. Partial input may omit
`reconstruction.curve`, which takes its defaults; every resolved recipe/report emits
the curve. Partial objects are
otherwise permitted. Unknown fields are rejected at every level.

### The `calibration` section

**The roll's measured values live in their own top-level section**, separate from
the look, so the split is structural rather than a convention about which keys go
in which file:

```json
{
  "calibration": {
    "film_base": {"explicit": [0.163, 0.080, 0.0377]}
  }
}
```

`calibration.film_base` accepts `"auto"`, `{"region": [x, y, w, h]}` or
`{"explicit": [r, g, b]}` and has **no default** — `convert` and `roll` refuse an
unstated one (§9). It is the section's only member today. A pipeline profile is then
"a recipe with no `calibration` section", and a roll calibration is "a recipe with
nothing else".

**A key belongs here when it is (a) measured from the film, (b) fixed across the
roll, and (c) consumed by a rule that lives elsewhere.** That is why `curve.anchor`
stays in the curve: it is a rule, part of the look, and reads only the film base. The section is deliberately
**open**, not a fixed pair: `nf-calibration/anchor-comparison` chose a content-referenced
roll white, and `nf-calibration/roll-white-rule` decides whether it joins this section or
only the contrast solved from it is carried. Each member carries its own
optionality and its own default.

Producing a calibration is not "one frame in, one calibration out": `film_base`
comes from a single reference frame, but a roll content white is read from every frame:
the brightest frame's own white under a cap, never a percentile across frames
(`nf-calibration/roll-white-rule`). The acquisition cascade is
`core/base-acquisition-planner`.

**`calibration.dmax` retired** with the roll reference density
(`nf-retire/dmax-machinery`). Because every earlier sidecar and `--dump-params` document
serializes it at its old default, `"dmax": "fixed"` is accepted and dropped on load;
any other value is refused (exit 2) naming the key. The older paths
(`reconstruction.curve.dmax`, a top-level `film_base`) are likewise rejected with a
migration error.

The retired `simple.invert_white_balance`, `simple.clip_low`, and `simple.clip_high`
keys (and flags) are rejected with a migration error naming their replacements under
`print` (`print.white_balance`, `print.linear_range`).

### The new chain's recipe (`--new-flow`)

The new chain (`docs/design-update.md`) reads its own document, declared by a
top-level **document version** rather than per-object ones:

```json
{
  "recipe_version": 2,
  "input": { "…": "as above" },
  "calibration": { "film_base": {"explicit": [0.163, 0.080, 0.0377]} },
  "measure": { "inset": 0.05 },
  "reconstruction": {
    "scale": [1.0, 0.84, 0.73],
    "offset": [0.0, 0.0, 0.0],
    "linearization": 1.8,
    "anchor": {"mid-at-base-offset": 0.62}
  },
  "scene_correction": {
    "white_balance": {"explicit": [1.0, 1.0, 1.0]},
    "exposure": 0.0
  },
  "look": {
    "contrast": 1.1111112,
    "channel_grade": [1.0, 1.0],
    "highlight_desaturation": {"strength": 0.8, "start_stops": -1.0, "band": [0.015, 0.025]}
  },
  "fit_range": {"headroom_stops": 6.0},
  "fit_gamut": {}
}
```

- **One section per stage, in chain order.** `input` and `measure` are shared with
  the current chain; `calibration` holds the film base alone, since the fixed
  decode reads no reference density. `reconstruction` is the fixed decode's
  parameters (`--density-scale`, `--density-offset`, `--density-gamma`,
  `--anchor-mid-offset`); its slope is `linearization`, the calibrated half of the
  single `gamma` the current chain ships — print contrast is the look's
  (`nf-reconstruction/gamma-split`). Only the products `linearization · scale_c` enter
  the curve, pinned by the convention `scale_r = 1`. The pre-split
  `reconstruction.contrast` is refused by name at every value, with the `look.contrast`
  that would keep it as the whole contrast. `scene_correction` is white balance and
  exposure and `fit_range` the operator's headroom, and `look` contrast, the
  per-channel grade and highlight desaturation (all below).
  `fit_gamut` is empty and has no knob: it changes primaries into the destination's
  gamut and maps out-of-gamut colour radially toward neutral at constant luminance,
  against the cube `[0, max(peak, Y)]` — the peak is fit range's, and content above
  it renders neutral at its own luminance and clips, counted, at the encode. A pixel
  whose destination luminance is `≤ 0` renders black. The report names it
  `acescg-to-display-p3-matrix+neutral-axis-radial-boundary-v2`. There is no
  `output` section while the new chain writes one fixed destination.
- **The version is the chain declaration.** `recipe_version` is required and is
  exactly `2`. Under `--new-flow` a recipe without it is refused; without the flag,
  a recipe stating it is refused. The current chain's `print`/`output` sections, its
  `reconstruction`/`calibration` keys, and the keys both chains retired (top-level
  `algorithm`/`density`/`film_base`, `input.color`, …) are refused in a v2 document by
  name, each with where its knobs went — a migration error, no aliases.
- `hanten params --new-flow` writes the default document, and `--dump-params` under
  `--new-flow` writes the resolved one; either reloads under the flag unchanged.
  `recipe_version`, like `params`, is reserved and never a key of the current
  chain's recipe.

- **`scene_correction`** (`nf-scene-correction/stage`): per-channel gains on linear
  ACEScg, after the NC film RGB v1 3×3 and before the look. `white_balance` is
  `{"explicit": [r, g, b]}` (`--white-balance`; finite and positive) and nothing
  else: a roll's gains are measured once by `hanten measure-roll` and stated here, so
  every frame applies the same ones. The per-frame `"gray-world"` / `"percentile"`
  modes and `--auto-wb` retired (`nf-scene-correction/roll-white-balance`) — a frame's
  own statistics read a sunset as the cast — and are refused by name. `exposure`
  is in stops (`--exposure`, the new chain's spelling of `--print-exposure`), applied
  as `2^EV`; each gain times it must be a normal `f32`. The report's
  `new_flow.scene_correction` states the gains and the exposure applied.
- **`look`** (`nf-look`): the creative stage, scene-referred and linear, between scene
  correction and fit range. Each control is **its own key**, added by its own task — not
  one CDL-style object, whose slope and offset would restate white balance and the
  flare subtraction. At every control's identity the stage is a bit-exact identity and
  the report's `new_flow.stages` lists it as `"applied": "identity"`. Controls run in
  the order the section lists them.
  `contrast` (`nf-reconstruction/gamma-split`, `--contrast`, new-flow only) — print
  contrast pivoted at mid-grey, `out_c = 0.18 · (in_c / 0.18)^contrast` on each ACEScg
  channel; non-positive and non-finite samples pass through. Finite and positive; `1`
  is the identity. The default is `2.0 / 1.8`, so with the decode's linearization a
  neutral renders where the single-slope decode rendered it (within a few f32 ULP);
  saturated colour differs slightly, because the power acts after the NC film RGB v1
  3×3 and the decode's slope before it. Scene correction runs first, so an exposure
  of `e` stops leaves the look as `e · contrast` stops.
  `channel_grade` (`nf-look/per-channel-grade`, `--channel-grade R,B`, new-flow only)
  = `[r, b]` — red and blue exponents of a power pivoted at mid-grey,
  `p_c = 0.18 · (x_c / 0.18)^g_c` with `g = [r, 1, b]` (green fixed at 1: a common
  exponent under the restore would be a saturation knob), then the ACEScg luminance
  restored, `out = p · Y(x) / Y(p)`, so it never moves neutral contrast and a neutral
  mid-grey stays exactly neutral while a cast grows away from it. Both exponents
  finite and positive and the spread over `[r, 1, b]` under 1, which keeps it monotone
  in exposure; `[1, 1]` is the identity. A pixel is graded only when all three
  channels and both luminances are finite and positive and the result is finite; any
  other pixel passes through whole, bit for bit. It replaces the retired regional balance
  (§7.2), which the current chain no longer has.
  `highlight_desaturation`
  (`nf-look/path-to-white`) = `{strength, start_stops, band: [s0, s1]}` —
  `--highlight-desaturation`, `--highlight-desaturation-start`,
  `--highlight-desaturation-band`, new-flow only. Per pixel on scene-referred ACEScg:
  `rgb ← rgb + strength · b · w · (Y − rgb)`, where `b` is a smoothstep in stops from
  `start_stops` (default −1) up to diffuse white, held above it, and `w` a linear band
  over `s = log10(max/min) / (reconstruction.linearization · look.contrast)` — the
  negative's density spread, whichever stage carries the contrast — full pull at `s ≤ s0`, none at
  `s ≥ s1` (default `0.015, 0.025`). Luminance is kept. `strength` is in `[0, 1]`,
  default `0.8`; `0` is off, a bit-exact identity. It assumes a roll-level white
  balance ahead of it. The report's `new_flow.look` echoes the section.
- **`fit_range`** (`nf-display-stages/fit-range`): fits the scene's range into the
  display's, with the display's **peak** as the operator's one per-destination
  argument — the destination states it, never the recipe (`1.0` for the SDR TIFF).
  One operator, reinhard: `Y′ = r(Y)·(1 + (P − 1)·s(Y))` on ACEScg luminance, all
  three channels scaled by `Y′/Y`, where `r` is the mid-grey-preserving extended
  reinhard at `W = 2^headroom_stops` and `s` a smoothstep in stops from diffuse white
  (`1.0` on the fixed decode) to `W`. So `P = 1` is exactly reinhard, and every peak
  agrees bit for bit below diffuse white. `headroom_stops` (`--display-tone-headroom`)
  is finite, `0`–`24`, default `6`; `0` is the identity. Content above `W` exceeds
  the peak on every branch and is clamped and counted at the encode. A non-finite
  sample is refused, naming the pixel; a pixel with luminance ≤ 0 is scaled by the
  curve's limit at black (the mid-grey gain), so the scale is continuous there.
  The report's `new_flow.fit_range` names the operator (`reinhard-peak-lifted-v1`, or
  `identity` at zero headroom) with its headroom, white point and display peak.

This section states the shape. Each stage's keys are specified by the task that
ships the knob, not written here ahead of the code.

### Target: recipe composition and the calibrate/profile split

> **This subsection describes the target, not the shipped surface** — like the
> "Target replacement architecture" block in `TASKS.md`. Owned by
> `core/recipe-composition`, `core/profile-authoring`,
> `core/base-acquisition-planner` and `core/value-domain-terminology`. Everything
> above this heading is what ships today.

**Two kinds of configuration, distinguished by lifetime:**

| | Scope | Origin | Reused |
|---|---|---|---|
| **pipeline profile** — reconstruction, curve shape, print controls, output policy | a look | chosen | across many rolls |
| **roll calibration** — `calibration.film_base` (and any later roll measurement) | one roll | measured from film | never |

**The structural half of this has shipped** (`core/calibration-recipe-section`):
the measurements live in their own `calibration` section, described above. What
remains below is the *workflow* built on that split.

**Composition is layered, over one schema.** `--params` is repeatable and accepts
`-` for stdin. Later layers win, and individual flags still win over all of them:

```text
defaults  <  --params A  <  --params B  <  …  <  individual flags
```

`roll` gains the same per-knob override flags `convert` has, so a one-off roll
needs no file at all. Per-frame overrides stay in the `--frames` manifest.

**The workflow.** Freezing no longer runs a conversion:

```sh
hanten inspect scan.tif                                       # optional: what is this file
hanten calibrate --unexposed blank.tif --leader exposed.tif              --out roll-cal.jsonc                          # measure the roll, once
hanten profile --density-gamma 2.4            --output-preset display-p3 --out my-look.jsonc  # author a look, no image
hanten roll frames/*.tif --out-dir positives/         --params my-look.jsonc --params roll-cal.jsonc     # apply
```

Both `--unexposed` and `--leader` are optional: `--unexposed` resolves the film base,
and `--leader` — which measured the retired reference density — would now only guard
a roll-white measurement, as `measure-roll`'s does. Agents can skip the
files entirely — the report stays on stdout, so
`hanten calibrate … | jq '{calibration}' | hanten roll … --params -` composes
(the report's `calibration` key is the section *body*, so the object form is what
`--params` takes).

**Authored files are JSONC** (JSON plus comments). It is a superset, so every
existing recipe, sidecar and `--params` file stays valid, the tagged enums the
schema leans on keep working, and the machine contracts — report on stdout, the
output sidecar — remain plain JSON. Comments are **generated from the schema, not
preserved**: serde round-trips discard them, so Hanten writes an annotated file once
and never rewrites a user's file in place.

**Renames and removals.** `hanten estimate` becomes **`hanten calibrate`** (it resolves a
roll, not one value — `hanten measure-roll`, the first command that measures across a
roll's frames, is the other half it would absorb) and `hanten params` becomes **`hanten profile`** (it authors a
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
  one label available to record them.) `2` moved the nominal anchor to `1.3`, the
  default curve to the mid-grey-anchored sigmoid, and the exponential's own gamma to
  `2.0` (measured in `docs/reports/render-defaults-v2.md`); `3`–`5` moved the default
  preset and the per-channel gain; `6` is current: the sigmoid retired and the default
  curve became the exponential at the fixed decode's configuration. `version.rs`
  carries the full table. This is the axis a version comparison is keyed on.
- **What the drift gate does and does not cover.** A golden drift test
  (`version::PIPELINE_FINGERPRINTS`) pairs each version with three fingerprints —
  the default **render** (the curated per-pixel vectors in
  `pipeline::stages::golden`), the default **film-base estimate** (stage 2, `auto`
  over the frozen scan in `pipeline::film_base::golden`, because the render
  fingerprint is handed a hardcoded base and the recipe fingerprint sees only
  `null` — `calibration.film_base` has no default, so the base fingerprint names `auto`
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
  (`hanten convert --dump-params f.json …` then hash `f.json`) and identical
  configurations are detectable across frames and versions. The sidecar's `params`
  body is the same **document** but not the same bytes — nesting it under `params`
  indents every line two extra spaces — so reproduce the hash from a
  `--dump-params` file, and compare the sidecar as parsed JSON. Omitted for
  `inspect`/`estimate`, which resolve no full recipe. `hanten roll` stamps one
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
identical pixels); an f32 preset (`film-master`, `hdr-linear-tiff`) reports the verbatim, **unclamped**
float mean over the *finite* samples, so it may exceed `1.0` and one `NaN` cannot
swallow the statistic (`loss.non_finite` is where that fault is reported). A u16
mean and an f32 mean are therefore not comparable, and `compare` refuses to subtract
them. Only the mean is recorded; ΔE2000 / SSIM need real pixel access and belong to
§12 item 7's QA harness.

`hanten --version` prints the same build identity (semver, `pipeline_version` with a
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
      "density": {
        "scale": [1.0, 0.84, 0.73],
        "offset": [0.0, 0.0, 0.0]
      },
      "curve": {
        "gamma": 2.0,
        "anchor": {"mid-at-base-offset": 0.62}
      }
    },
    "calibration": {
      "film_base": {"explicit": [0.163, 0.080, 0.0377]}
    }
  },
  "reconstruction_result": {
    "curve": {
      "anchor": {"mid-at-base-offset": 0.62},
      "anchor_value": 0.99236375
    }
  },
  "working_mapping": "nc-film-rgb-v1",
  "output_render": {
    "preset": "display-p3",
    "print_controls": true,
    "display_render": true,
    "display_tone": {
      "operator": "extended-reinhard-mid-preserving-v2",
      "headroom_stops": 6.0
    },
    "encoding": "display-p3-u16-tiff",
    "content": "single-rendition SDR: Display P3 primaries with the sRGB transfer, …",
    "working_mapping": "nc-film-rgb-v1",
    "reconstruction_schema_version": 1
  }
}
```

`output_render` (`convert` only) records **which branch out of the ACEScg boundary
ran and what it applied**, so a consumer never has to re-derive it from the recipe.
`preset` is the resolved `output.preset`; `print_controls` says whether the
shared print controls ran *at all* (not whether their values were non-default — it
is `false` only for `film-master`); `display_render` says whether any tone/gamut/transfer operation
ran; `display_tone` names the display branch's tone operator and its resolved
`fit_range.headroom_stops`; at `0` the operator reads `"identity"`, the new chain's rule,
since no pixel moved. It is **absent** on
`film-master`, which has no display tone stage at all. It rides in `output_render`, the one block *every* preset
emits, so the two SDR presets, which emit no per-preset block at all, still say which tone curve ran;
`content` therefore states what the branch does *besides* tone and never names a curve.
Note the AVIF pair *also* carries the renderer's pinned identifiers and luminance
anchors in `avif.rendering` (a nested block, because unlike the rest of `avif` it is
declared policy rather than facts read back out of the file); `output_render.display_tone`
states the same resolved operator in the block every preset emits. `encoding` is a stable identifier — one of
`unclamped-linear-acescg-float-tiff` | `legacy-ultra-hdr-v1-xmp-mpf-jpeg` |
`dual-dialect-gain-map-jpeg` |
`rec2100-pq-10bit-444-avif` | `rec2100-hlg-10bit-444-avif` |
`display-linear-bt2020-float-tiff` | `rec2100-pq-u16-tiff` |
`rec2100-hlg-u16-tiff` | `display-p3-u16-tiff` | `srgb-u16-tiff`. (The retired
`legacy` preset's `rendered-u16-tiff` and `transitional-rendered-float-tiff` are no
longer produced; older reports may still carry them.) The two float names are
mutually exclusive on purpose: `unclamped-linear-acescg-float-tiff` is the
pre-display master, `display-linear-bt2020-float-tiff` is display-rendered but
pre-transfer. `content` states what
the pixels contain; for `film-master` it names the intentional
film/lens/development/scanner/reconstruction/curve rendering and explicitly
disclaims physical scene recovery, naming the placement: "a film-base-derived anchor
placement". `working_mapping` is repeated inside the block
so a master's provenance is self-contained, and
`reconstruction_schema_version` mirrors `reconstruction.schema_version`. The
behavioral `pipeline_version` is a **separate** field owned by
`conversion-versioning`; this build stamps none, so it is absent rather than
guessed.

The `reconstruction_result.curve` block carries two placement fields: `anchor` (the resolved placement *rule*) and `anchor_value` (the
**derived** anchor — the corrected density this render mapped to `1.0`, hence the black
floor at `10^(−contrast·anchor_value)`). (The block's `dmax` {policy, value, provenance}
object retired with the reference in `nf-retire/dmax-machinery`.)
`reconstruction.schema_version = 1` versions the wire schema; it is
not the behavioral `pipeline_version`. The `conversion-versioning` task owns
stamping and bumping `pipeline_version`, and does so only when default pixels
change. Recipe/report
round trips, fixtures, and migration errors pin the reconstruction schema.

(Its `type`, and the `characteristic` curve's `stock` and `out_of_table` objects, retired
with that curve in `nf-retire/characteristic`; older reports may still carry them.)

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
**film-base** phase, not decode, is usually the peak. `hanten roll` reports the same
block **per frame** (frames may differ in dimensions, and the gate runs per
frame), not once for the roll — including for a frame that passed the gate and then
failed for another reason, whose entry carries both its `memory` block and its
`error`.

### Example invocations

```bash
# Default density conversion: base-derived anchor, gain-map JPEG, JSON report.
# The curve flag is optional (it is the default); the film-base flag
# is **not** — `calibration.film_base` has no default, so every `convert` must state
# one of `--film-base` / `--base-region` / `--auto-base`. The `.jpg` suffix *is*
# optional: `-o out` writes `out.jpg`, because the default preset is
# `gain-map-hdr`. Stating it is still checked — nc never renames a suffix you give
# it (add `--output-preset display-p3` for a 16-bit TIFF).
hanten convert in.tiff -o out.jpg \
  --auto-base --report json

# Rendered float TIFF: display-linear BT.2020 after the print controls and the HDR
# display render. This is NOT film-master.
hanten convert in.tiff -o out.tiff \
  --output-preset hdr-linear-tiff \
  --film-base 0.92,0.55,0.42 \
  --density-gamma 1.8 --print-exposure 0.0 --black-point 0.002 \
  --display-tone-headroom 4

# The film master: unclamped 32-bit float linear ACEScg straight out of the
# NC film RGB v1 mapping, ACEScg profile embedded, no print or display controls at
# all. Reconstruction + the density curve + its anchor placement ARE in the master
# (that is the intentional film rendering); WB/exposure/black/range and every
# display operation are not. A non-default print control alongside it is a usage
# error. Never silently dropped.
hanten convert frame12.tiff -o frame12_master.tiff \
  --output-preset film-master \
  --film-base 0.92,0.55,0.42
# → report.output_render = { "preset": "film-master", "print_controls": false,
#     "display_render": false, "encoding": "unclamped-linear-acescg-float-tiff",
#     "working_mapping": "nc-film-rgb-v1", … }
# Re-exporting a graded roll recipe as a master: reset its print controls on the
# command line (flags win, and the rejection is on the RESOLVED value).
hanten convert frame12.tiff -o frame12_master.tiff --params roll-A.json \
  --output-preset film-master --print-exposure 0 --white-balance 1,1,1

# Reuse a roll recipe but override one knob for this frame.
hanten convert frame12.tiff -o frame12_pos.jpg \
  --params roll-A.json --print-exposure 0.15

# Convert a whole roll from ONE shared, frozen recipe (batch-apply). The shared
# recipe config (the roll-fixed film base) lives in roll-A.json and appears
# once at the top of the roll report; each frame additionally echoes the resolved
# base it used. Per-frame outputs go to out/ as <stem>_positive.<ext>, the
# suffix following the resolved output preset.
hanten roll frame01.tiff frame02.tiff frame03.tiff --out-dir out/ --params roll-A.json
hanten roll scans/ --out-dir out/ --params roll-A.json   # a directory expands to its .tif/.tiff
# Per-frame overrides via a manifest: each frame may carry its own output path
# and a partial-recipe `params` deep-merged onto the shared recipe for that frame
# only (the "frame-local" knobs, e.g. print exposure). The manifest is the shape
# the base-acquisition-planner will emit.
#   frames.json: { "frames": [
#     { "input": "frame01.tiff" },
#     { "input": "frame02.tiff", "params": { "print": { "print_exposure": 0.15 } } } ] }
hanten roll --frames frames.json --out-dir out/ --params roll-A.json
# The roll report: { "command": "roll", "recipe": { …shared frozen recipe… },
#   "warnings": [ …roll-level, e.g. base-not-frozen… ],
#   "frames": [ { "input": …, "output": …, "status": "ok", "film_base": …,
#     "warnings": […], "overrides": … }, … ],
#   "summary": { "total": 3, "succeeded": 3, "failed": 0 } }
# (each frame's "film_base" is the *resolved* value it used, alongside the
#  shared recipe config above — not a second copy of a per-frame-varying knob)
# A frame's failure is recorded (status "failed" + error) and the roll continues;
# the process then exits non-zero. Determinism: same batch + same recipe ⇒
# byte-identical output per frame (each frame runs the same core as `convert`).

# Inspect only; let an agent read the JSON and decide parameters.
hanten inspect in.tiff --report json

# Calibrate once from an unexposed reference frame, then reuse for the roll.
# (Product tip: wind past the light-struck leader, shoot a lens-cap frame, and
# scan it — a full frame of clean base beats sampling the thin rebate. Don't use
# the auto-burned wind-on frames; they are fogged leader. See §9 film-base.)
# `estimate` measures Dmin from the sampled rectangle and reports it in
# directly reusable forms: a paste-ready --film-base flag string and a
# `calibration` object already in recipe shape (emitted only when the measurement
# is a valid explicit base — each channel in (0, 1] — else a warning explains why
# not).
hanten estimate reference.tiff --base-region 200,0,300,3600 --report json
# → { "film_base": { "r": 0.553, "g": 0.271, "b": 0.159 },
#     "film_base_source": { "region": [200, 0, 300, 3600] },
#     "film_base_flag": "--film-base 0.553,0.271,0.159",
#     "calibration": { "film_base": { "explicit": [0.553, 0.271, 0.159] } }, … }
hanten convert frame01.tiff -o frame01_pos.jpg --film-base 0.553,0.271,0.159
# …or write the calibration straight out and batch with it:
hanten estimate reference.tiff --base-region 200,0,300,3600 | jq '{calibration}' > roll-cal.json

# On a dedicated blank frame, `estimate --grid` samples a fixed 5-cell grid
# (corners + center) over the frame (or over --base-region) instead of a single
# measurement: the report gains a `grid` object (per-cell regions/values, the
# per-channel relative spread, the tolerance, and the agreement verdict), the
# combined base is the per-channel median across cells, and disagreement beyond
# the tolerance is a loud warning (--strict promotes it to a failing exit) —
# it diagnoses light leaks, scanner illumination falloff, or dust.
# A cells-disagree *warning* does NOT suppress the reuse-ready output: when the
# combined median base is in range it is still offered (film_base_flag and the
# `calibration` object), because the median resists a single bad cell. A consumer
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
hanten estimate blank.tiff --grid --report json

# Auto neutral white balance: estimate per-frame gains (percentile ≈ NLP
# Auto-Neutral; gray-world ≈ Auto-AVG), read the resolved gains back from the
# report, and freeze them into --white-balance / the roll recipe
# (print.white_balance = {"explicit": [...]}) — the reuse run is bit-identical.
hanten convert frame01.tiff -o frame01_pos.jpg --film-base 0.92,0.55,0.42 \
  --auto-wb percentile --report json
# → { "white_balance": [1.083, 1.0, 0.941], ... }
hanten convert frame02.tiff -o frame02_pos.jpg --film-base 0.92,0.55,0.42 \
  --white-balance 1.083,1.0,0.941

# The new chain measures white balance once per roll instead: pool every picture
# frame (the leader guards against a fully exposed one), then state the gains.
hanten measure-roll frames/*.tif --leader leader.tif --film-base 0.47,0.23,0.11
# → { "white_balance": { "gains": [1.002, 1.0, 1.277], "percentile": 0.99, ... },
#     "reuse": { "flag": "--white-balance 1.002,1,1.277",
#                "recipe": { "scene_correction": { "white_balance": { "explicit": [...] } } } } }
hanten roll --new-flow frames/*.tif --params roll.json -o out/
```

## 9. Parameter reference (grouped by stage)

Every conversion flag has a recipe key (for example, `--output-preset` ⇒
`output.preset`); reconstruction entries live under the tagged `reconstruction`
object (§8). Names are binding and unknown keys are rejected
(`deny_unknown_fields`).

The **operational** flags (`--report`, `--telemetry*`, `--max-memory`) are the
exception: they touch no parameter at all, so they have no recipe key. (`--preset`, a
named bundle with no recipe key of its own, retired with the `characteristic` curve.) A
second, **transitional** case sits outside that while the new-flow migration runs: `--new-flow` selects which *chain* — and so
which knobs — exist, so it is CLI-only yet does change the render
(`docs/nf-migration.md`). It is removed when the default flips and is deliberately
not specified here.

### Input / decode
- `--export-ir <path>` — write the IR plane to a separate TIFF (HDRi only).
  Recipe key `input.export_ir`. The **IR TIFF** follows the resolved output depth
  (`OutputParams::depth()`), which is *not* the primary container's depth for the
  display presets. In full:
  - 16-bit — `--output-preset hdr-pq-tiff` / `hdr-hlg-tiff`
    and `display-p3` / `compatibility` (whose primaries are themselves 16-bit);
    and the four presets whose primary is not a TIFF at all: `gain-map-hdr` and
    `ultra-hdr-v1` (fixed 8-bit JPEG) and `hdr-pq` / `hdr-hlg` (10-bit AVIF).
  - 32-bit float — `--output-preset film-master` and `--output-preset
    hdr-linear-tiff`.

  The IR *samples* never
  change: the plane is carried through the pipeline untouched (Step-1 rule: preserve,
  don't consume), so only the quantization headroom differs.
- `--film-type <silver|chromogenic|unknown>` ⇒ `input.film_type` (default
  `"unknown"`) — the declared film chemistry. **Provenance only: it gates nothing.**
  IR-assisted film-holder detection (§6.1) is enabled by *measuring* the IR plane,
  not by this declaration. Kept as a shared input-medium axis for the deferred IR
  dust-removal stage (§12 item 1) and `bw-support`; accepted on `convert`,
  `estimate`, and `inspect`, which echo it back as the report's `film_type` — those
  two resolve no recipe, so echoing is what keeps a declaration from being parsed
  and dropped. `hanten inspect` and `hanten estimate` report `ir_separability` (the measured
  interior IR transmission and the verdict) on any scan carrying an IR plane, and
  on a usable one additionally reports a `holder_mask`: the per-edge along-edge
  segments, each with its span `[start, end)`, holder/film class, and
  representative median IR transmission, so the occluded spans are inspectable.
  Where auto detection runs and the IR plane is present but unusable — shape-only
  provenance, or a frame whose own film is IR-opaque — the RGB-only fallback is a
  report warning promotable under `--strict`.
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
    Rec.709 for lacking an ICC. `hanten inspect` still reports the evidence so the
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
  `hanten inspect`, the `convert` report, **and each `hanten roll` frame report** expose
  the resolved `input_color`: both axes with per-axis evidence, whether an ICC is
  embedded plus the safe summary, and `transfer_decoded` (whether any
  inverse-transfer decoding was performed — always `false` in Step 1, which
  accepts only already-linear samples). `meaning` is always a flat string
  (`scanner-device` / `colorimetric` / `unknown`); the colorimetric detail rides
  in a sibling `meaning_reference` field so consumers can key `meaning` uniformly.
  In roll mode a shared recipe (or per-frame override) asserting the
  unconditionally-unsupported `input.meaning: colorimetric` is rejected up front
  (exit 4), before any frame is decoded.

### Measurement region (`measure`)
Every statistic nc reads off a frame — `Dmin`, a roll's white balance
(`hanten measure-roll`), content exposure and contrast, tiling uniformity — is read over
the **effective area**, not the whole scan. An uncropped scan carries an opaque film
holder that is maximum density, so a whole-frame statistic measures the holder rather
than the picture (a per-frame density percentile taken that way once rendered every
frame black).

The area is **two cuts, in order — never one or the other**:

1. **The film holder**, measured per edge from the IR plane by marching inward
   until IR reads film. Only where `film_base::ir_separability` measures the plane
   able to separate holder from film *on this frame*; declared chemistry takes no
   part in it. No IR plane, a plane identified by shape alone, or film too
   IR-opaque to separate (routine for exposed silver stock) all mean **not
   measured** — a different report from "measured, and there is no holder", which
   is what an already-cropped scan yields.
2. **A static inset** of what remains, recipe key `measure.inset` /
   `--measure-inset FRAC`, default `0.05` of the **original** frame's shorter
   dimension (so the pixel count does not move with the holder measurement).
   Accepted range `[0, 0.4]`; outside it is a usage error (exit 2) from every
   command, checked before the decode. Where cut 1 **ran**, the applied inset is
   floored at one holder-probe step (0.5% of the shorter edge): a march resolves a
   depth only to the start of the first band whose median reads film, so up to half
   a band of holder can sit inboard of any measured depth — including a measured
   zero. The floor absorbs that band, binds only near `0` (a 3600 px frame insets
   180 px against an 18 px step), and is reported, so a stated `0` on a measured
   frame is deliberately not honoured exactly. Where cut 1 did not run there is no
   measurement resolution to respect and the stated fraction is exact.

The inset is **not a fallback** for cut 1 — it runs either way, and where cut 1 did
not run it is simply the only cut. Because it is then sized for a rebate rather than
a holder, it may under-clear the holder and its rebate together: the only directly
measured holder depth is 2.5–4% of the shorter edge (the IR march across 31 real
frames). The 10–15% figure recorded in `analysis/conversion-metrics` is a holder
*occupancy* share of a rendered frame, not a depth, and bounds nothing here. That is deliberate: nc declines to guess a depth it could not measure, and
the user raises the fraction instead. The report records which case a run was in.

**nc never searches for a rebate**, for measurement or otherwise: the inset passes
over it blind. Where a measurement needs unexposed film, it is taken over the whole
effective area of a reference frame or over a region the user states.

**The image is never cropped.** The effective area changes only *which pixels a
statistic is computed over*; written dimensions, aspect ratio and pixel count are
exactly as decoded.

**Every command that decodes resolves the area and reports it**, under the report
key `effective_area` — the resolved rectangle, the per-edge holder depths (with a
per-edge `capped` and the frame-wide `converged`), `holder_applied`, and the applied
inset — for a `roll`, inside each frame's entry.
`convert` and every roll frame resolve it unconditionally so `--measure-inset` and
the `measure.inset` recipe key (including a per-frame override) are observable rather
than accepted-and-ignored. A `capped` edge or `converged: false` also emits a
`--strict`-promotable warning: both mean the reported rectangle is not a
measurement, and a capped edge truncates the cut its *perpendicular* edges are
measured over, so their depths may be artifacts rather than floors.

**Nothing in `convert` measures over the area today** — its one consumer there, the
per-frame reference density, retired with `nf-retire/dmax-machinery` — so an *empty*
region is always a warning on `convert` (with no reported area), never a refusal.
`measure.inset` stays live for `hanten measure-roll`, which samples each frame over the
area. For the same reason `holder_applied` does not suppress `convert`'s "IR preserved
but not used" warning: a marched holder moves the reported rectangle but no rendered
pixel, so only the film-base stage consuming the plane (`BaseEstimate::ir_mask_applied`)
counts as use. (`inspect`, which renders nothing, still counts the march.)

### Film base / Dmin (stage 2)
The base source is a single mutually-exclusive choice, recipe key
`calibration.film_base` — **required, with no default**. `convert` and `roll` reject a
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
   — measured with `hanten estimate` and frozen into the roll recipe (§8 example).
   The large area also enables multi-region sampling with an agreement check
   (`hanten estimate --grid`, §8), which doubles as a light-leak /
   illumination-falloff diagnostic.
2. **The rebate (the unexposed strip around each frame).** Reliable form: point
   `--base-region` at a visible rebate patch manually — `hanten inspect` reports the
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
   (`--base-content` / `calibration.film_base = "content"`) — it is **not** part of
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
density divide or be echoed back by `hanten estimate` as a trustworthy `Dmin`. This
holds for the `hanten estimate --grid` combined base too: it emits the diagnostic
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

### Named conversion presets — retired
- `--preset` retired with the `characteristic` curve (`nf-retire/characteristic`): its
  three names (`characteristic-generic`, `-stock`, `-aim`) each set that curve, its gain
  and an exposure calibrated to it, and nothing is coupled in the look that would earn a
  bundle again (`nf-look/look-presets`). A named look is a `--params` layer. The flag is
  hidden and exits 2 at every value on both chains, before any coarser rule; the report's
  `conversion_preset` block is gone. Old reports may still carry it.

### Density curve
- There is one curve, the exponential, so there is nothing to select.
- Recipe: `reconstruction.density` and `reconstruction.curve`, exactly as shown in
  §8. There are no sibling top-level density or curve sections.
- Removed, each a migration error rather than an alias (nc is unreleased): the
  `--algorithm` flag and old `algorithm` recipe form; the old top-level `density`,
  `sigmoid`, and `simple` forms; `--reconstruction` and `reconstruction.type =
  "simple"` (the old value `"density"` is accepted and dropped); `--density-curve
  sigmoid` and `curve.type = "sigmoid"`; the `--sigmoid-contrast`,
  `--sigmoid-toe`, `--sigmoid-shoulder`, `--sigmoid-mid-fraction` and
  `--sigmoid-white-at-d-max` flags, each naming its replacement; and, with the
  `characteristic` curve, `--density-curve` (at every value, `exponential` included),
  `--film-stock`, `--preset`, `curve.type = "characteristic"` and `curve.stock` (the old
  `curve.type = "exponential"` is accepted and dropped).

### Density stage (`reconstruction = density`)
- `--density-scale R,G,B` ⇒ `reconstruction.density.scale` — per-channel
  density gain. **Default `[1, 0.84, 0.73]` since `pipeline_version` 5**, not
  identity: green and blue density rise faster than red in a scan, so without a
  gain they drift against it across the tone scale. Calibrated from 31 hand-marked
  neutral patches over five rolls — each roll's median nulling scale, averaged with
  equal weight per roll. It replaced `[1, 0.90, 0.86]`, whose blue came from the
  manufacturers' published per-channel structure and overcorrects on this scanner;
  every roll measured wants blue 0.68–0.78. Green splits by scan date (July rolls
  0.86–0.90, September ~0.77), so `0.84` is a compromise rather than a fit. It is a
  **calibration**, so it is nulled deliberately in tests of the
  `D = −log10(scan / base)` definition. It is the fixed decode's
  `algo::fixed::DENSITY_SCALE`; the per-curve default it had while the `characteristic`
  curve existed (`[1, 1, 1]` there) went with that curve.
- `--density-offset R,G,B` ⇒ `reconstruction.density.offset` — per-channel
  density offset (orange-mask compensation).
- `--density-gamma <f>` ⇒ `reconstruction.curve.gamma` (default `2.0`).
- `--anchor-mid-offset <d>` ⇒ `reconstruction.curve.anchor = {"mid-at-base-offset": d}`
  (default `0.62`), strictly positive; it replaces a recipe's
  `anchor`. The placement divides by the slope, so validation **resolves the rule** and
  rejects a non-finite anchor, naming `--density-gamma` — and separately rejects a
  finite anchor whose product with the slope overflows (a silently black frame;
  `--anchor-mid-offset 2e38` reaches it at the default gamma). Slope positivity is
  diagnosed first.
- **Retired reference-density flags.** `--anchor-mid-fraction`,
  `--anchor-white-at-reference`, `--anchor-black-floor`, `--d-max`, `--fixed-d-max`,
  `--auto-d-max` and `--no-d-max` (and `estimate --d-max-region`) went with the roll
  reference density in `nf-retire/dmax-machinery`. They are hidden and exit 2 with a
  message naming `--anchor-mid-offset`, on both chains; the old behaviour lives only in
  the reference build.
- **Retired regional balance.** `--shadow-balance`, `--highlight-balance`,
  `--balance-range` and `--auto-balance-range` went with the regional balance in
  `nf-retire/regional-balance` (§7.2). They are hidden and exit 2 on both chains, naming
  the look's per-channel grade (`--channel-grade`, under `--new-flow`). The recipe keys
  `reconstruction.density.shadow_balance` / `highlight_balance` at `[0, 0, 0]` and
  `balance_range` at `"auto"` are dropped on load; any other value is refused, and an
  equal pair is pointed at `reconstruction.density.offset`, which it equalled; an
  explicit range beside equal balances, which the balance never consulted, is told to
  go, the render unchanged.
- **Retired curve selection.** `--density-curve`, `--film-stock` and `--preset` went
  with the `characteristic` curve in `nf-retire/characteristic`. They are hidden and exit
  2 on both chains at every value, telling the user to drop the flag.

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
  **Shipped state:** the shared display stage applies it on every display preset;
  `film-master` bypasses and rejects all print controls.
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

  Auto estimation runs under either curve.
- `--display-tone-headroom <stops>` / `fit_range.headroom_stops` (default `6`) — the
  display tone's one parameter: the specular headroom extended Reinhard compresses
  against, in **stops above reference white**, so its white point is `W = 2^stops` (the
  default `6` is `W = 64`). The same key the new chain's fit range reads. The knob sizes
  the curve; it is not the input that lands on reference white, since the operator
  preserves mid-grey instead (§6). Display-referred deliberately: a density spelling
  would make a display key read the reconstruction's anchor and contrast. `0` stops is
  `W = 1`, where the operator is exactly the identity and **self-policing**: a sample
  above the render's ceiling fails naming the pixel instead of clipping. The ceilings
  differ — reference white for SDR, the 1000-nit peak (`LINEAR_HEADROOM`) for HDR — so an
  overshoot that SDR refuses renders legitimately on an HDR preset. No shipped
  reconstruction is bounded at white, so on SDR lower `--print-exposure` until the frame
  fits (the refusal names that lever). At any other headroom the SDR overshoot is counted
  at the encode boundary (`.loss`, promotable by `--strict`). Bounded to `[0, 24]` stops
  as a *value* rule, so `roll` and per-frame overrides refuse it before any frame is
  decoded; beyond ~8 stops the operator converges on plain Reinhard and the extra
  headroom buys nothing. `film-master` has no display tone and refuses a non-default
  headroom by value, so the default is still the flags-win reset there.
- **Retired:** `--display-tone` / `print.display_tone` (`shoulder`, `none`, `reinhard`)
  and `--highlight-compress` / `print.highlight_compress`, in `nf-retire/display-tones`.
  Both tones existed for reconstructions bounded at white; the one operator left needs
  no selector. The flags and every `print.display_tone` value are migration errors on
  both chains — the old default `"shoulder"` included, since replaying it would render
  differently — while `print.highlight_compress` is stripped at its old default `0`.

### Removed `simple` controls
- `simple` reconstruction itself retired (§7.1). Its older controls were already gone:
  `--invert-white-balance R,G,B` and
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
  replacement names, and every preset applies them only after NC film RGB mapping.
  `film-master` rejects every final non-default range regardless of source;
  legacy flags may reset recipe endpoints to `[0,1]`. Aliases preserve parameter values, not bit-identical output through
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
- `--output-preset <gain-map-hdr|ultra-hdr-v1|display-p3|compatibility|film-master|hdr-pq|hdr-hlg|hdr-linear-tiff|hdr-pq-tiff|hdr-hlg-tiff>`
  — the atomic output **policy** choice;
  recipe key `output.preset` (**default `gain-map-hdr`**), the `output` section's
  only key. One mutually-exclusive enum field, never parallel bools: a preset
  resolves a whole coherent container/depth/profile policy plus which branch of the
  ACEScg boundary runs. The retired names `legacy` and `custom`, and the retired
  selectors `--out-depth` / `--output-profile` / `--bigtiff` (`output.depth` /
  `output.output_profile` / `output.bigtiff`), are removed-value usage errors (§5).
  - `film-master`: an unclamped 32-bit float linear ACEScg TIFF taken directly from
    the NC film RGB v1 mapping with the ACEScg profile embedded and no transform.
    After recipe/CLI merge it rejects every non-default print control (`print_exposure`, `black_point`, `white_balance`,
    `linear_range`) and a non-default display headroom (`fit_range.headroom_stops`)
    whatever their source. There is no ignore-conflicting-controls mode; the float export
    that applies those controls is `hdr-linear-tiff`.
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
    ACEScg → the shared print controls → `pipeline::sdr`, including its display
    tone and gamut mapping into the destination.
    They differ **only** in that destination: Display P3 for the first, sRGB for
    the second, which is the widest-support output nc writes.

    Making `display-p3` the default in place of the incumbent `gain-map-hdr` is
    `output/display-p3-default` — decided 2026-08-09 and reaffirmed 2026-09-13, not
    yet executed, because it is both a pixel change and a container change.
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
    the shared post-ACEScg print controls. Encoder settings (quality, speed, row
    multithreading with a pinned worker count of 8, no tiling) are pinned parts of
    the preset, not knobs: repeated encodes on one build are byte-identical. The
    worker count is a constant, never derived from the machine: libaom documents no
    thread-count independence, so it is measured (identical bytes for every count
    from 2 upward on libaom 3.11.0) and pinned by a test, while one thread disables
    row-mt and writes different bytes. No EXIF, XMP, ICC, timestamp or identifier is
    written.
  - `hdr-linear-tiff` is the display-linear HDR **interchange master**, accepted by
    requiring a `.tif`/`.tiff` output path. It writes the
    pre-transfer BT.2020/D65 samples of the same `pipeline::hdr` render verbatim as
    unclamped 32-bit float, with a synthesized linear-BT.2020 ICC profile —
    bit-exact, so an independent decoder recovers identical `f32` bits including the
    HDR values between the 203 cd/m² reference white (`1.0`) and the 1000 cd/m² peak
    (≈4.926108). It is named and therefore **atomic** on the same terms as
    `film-master`, and it consumes the shared post-ACEScg print controls (including
    a non-default `print.linear_range`). It is **not** `film-master` (that is linear
    ACEScg *before* any display rendering) and **not** `hdr-pq`/`hdr-hlg` (no
    transfer function has been applied). Because the ICC PCS stops at
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
- BigTIFF promotion is always automatic: a file too large for classic TIFF is
  written as BigTIFF, and the report says so.

`display-p3` and `compatibility` are 16-bit losslessly stored TIFF; `hdr-pq` and
`hdr-hlg` are AVIF, while the three shipped HDR TIFF policies provide
linear-float or losslessly stored PQ/HLG interchange. `film-master` encodes NC
film RGB v1 mapped unclamped linear ACEScg before print/display controls.
Named display presets use the SDR/HDR render
branches. The output path stays required; a suffix it states must match the
resolved container and is never rewritten silently, and one it omits is completed
from that container. After merge, `film-master` also rejects every
non-default effective WB, exposure, black, white, highlight, SDR/HDR tone, gamut, or
display-transfer control from recipe or CLI; it never ignores one. Flags may
explicitly reset recipe values to defaults, and the resolved report records the
effective values/provenance and that no display transfer ran. A selected
`correction.profile` is not a downstream creative/print/display control:
corrected output remains `film-master` and records mandatory profile
identity/hash/scope provenance. All ten presets are live and `gain-map-hdr` is the
default as of `pipeline_version` 3
(measured in [reports/render-defaults-v3.md](reports/render-defaults-v3.md)).
`hanten roll` migration is part of the preset task: automatic names use
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

**Roll (batch, `hanten roll` only — orchestration flags, NOT recipe keys).** `hanten roll`
converts many frames from one shared `--params` recipe; it reuses the exact recipe
shape above and adds no new conversion knobs. Its flags are operational (like
`--report`): `--out-dir <dir>` (per-frame outputs `<stem>_positive.<ext>`, the
suffix following each frame's resolved preset),
positional `inputs` (files and directories — a directory is expanded to its
`.tif`/`.tiff` files, sorted; shell globs are expanded by the shell, not by nc)
**or** `--frames <manifest.json>` (explicit per-frame `input`/`output`/partial-recipe
`params` overrides, deep-merged onto the shared recipe for that frame only).
**A `--params` recipe is effectively mandatory for `roll`**, because `roll`
converts and `calibration.film_base` has no default while `RollArgs` accepts none of
the three film-base flags — the recipe is the only place a roll can state its
base, and a roll with no recipe (or one omitting `calibration.film_base`) exits 2 with
a message that says so. That is the intended workflow rather than a limitation:
`Dmin` is measured once for the roll (`hanten estimate`) and frozen into the shared
recipe as `calibration.film_base.explicit`, which is also the only source that keeps
every frame on one base — see the roll-fixed invariant warnings below.
The shared recipe configuration appears once at the top of the roll report; each
frame additionally reports the *resolved* base it used — a redundant echo when the
recipe pins an explicit base, but meaningful under an `auto`/`region` base that
resolves per frame. Frame-local knobs are the per-frame `params` overrides. Roll-fixed
invariant violations are **loud, `--strict`-promotable warnings** rather than hard
errors, so a deliberate best-effort batch remains usable: (1) a shared
`calibration.film_base` other than `explicit` re-estimates Dmin per frame; a per-frame
override that sets (2) `calibration.film_base` changes that frame's Dmin, (3)
`reconstruction.curve.anchor` places that frame's mid-grey on a different rule, and
(4) `output.preset` gives it a different output **policy** — a different branch out of
the ACEScg boundary, so a different *image class* (unclamped linear master vs rendered
TIFF), not merely a different rendering. The override warnings key on the key's
presence, so they fire even when the override restates the shared value: `frames[]`
carries no `output_render` block (a `convert`-only field), leaving the
`frames[].overrides` echo as the only other trace. A per-frame `calibration.dmax`
follows the recipe rule (§8): dropped at `"fixed"`, refused otherwise (under
`--new-flow`, refused outright — no new-chain recipe ever carried it). `input.export_ir` is rejected in roll mode (one
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

**Telemetry record shape (`schema_version` 7, serialize-only JSON).** Designed for
a future background uploader (§12, `telemetry/upload`) to drain and ship:
```json
{
  "schema_version": 7,
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
    "preset": "display-p3",
    "params_hash": "92a827ffd2d0aebd",
    "film_base_source": { "explicit": [0.9, 0.55, 0.42] },
    "output_depth": "u16"
  },
  "outcome": { "warnings": 1, "clipped": 3419, "non_finite": 0 }
}
```
`timing_ms.ir_export` is present only when `--export-ir` ran (schema v2 replaced v1's
`conversion.algorithm` with the `reconstruction` + `curve` pair; v5 dropped
`reconstruction` with `simple` and made `curve` always present; v6 dropped
`conversion.dmax` with the roll reference density; v7 dropped `curve`, left one-valued
by the `characteristic` curve's retirement).
`conversion.preset` is the resolved `output.preset` — v3 added it, because without it
two f32 TIFFs (`film-master`, `hdr-linear-tiff`) are indistinguishable. Records made
before `nf-retire/legacy-custom` may carry the retired `legacy` / `custom`.
`conversion.output_depth` names the **primary** artifact's depth
(`OutputParams::primary_depth_label()`), not `OutputParams::depth()`, which for the
JPEG and AVIF presets is only the optional IR TIFF's depth — `u8`/`u10` are depths it
cannot spell at all.
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
    │   ├── display_tone.rs    # the display tone: extended Reinhard + its checked headroom
    │   ├── sdr.rs        # SDR display render (P3/sRGB, tone + gamut mapping)
    │   ├── hdr.rs        # Rec.2100 PQ/HLG display render
    │   ├── gain_map/     # SDR+HDR → canonical gain map; `iso.rs` owns the ISO 21496-1 bytes
    │   ├── pixels.rs     # the parallel per-pixel map drivers (byte-identical to a loop)
    │   ├── memory.rs     # peak-memory sizing model + budget preflight
    │   ├── stages.rs     # stage wiring as pure functions
    │   ├── white_balance.rs # white-balance statistics: the current chain's per-frame estimators, the roll's levels
    │   ├── roll_white.rs    # `measure-roll`: a roll's pooled white, leader-guarded (new flow)
    │   ├── chain.rs           # the --new-flow chain, composed
    │   ├── working_image.rs   # the buffer every new-flow stage boundary carries
    │   ├── scene_correction.rs # new flow stage 1: WB, exposure, flare (scene-referred)
    │   ├── look.rs            # new flow stage 2: contrast, grade, highlight desaturation
    │   ├── fit_range.rs       # new flow stage 3: scene range → display range
    │   └── fit_gamut.rs       # new flow stage 4: out-of-gamut colour → display boundary
    ├── algo/
    │   ├── mod.rs        # FilmRgbImage + reconstruct
    │   ├── density.rs    # density reconstruction + exponential curve
    │   └── fixed.rs      # the new flow's fixed, stock-agnostic decode
    ├── film_stock/       # test-only: the digitized per-stock curves, evidence for the decode's constants
    ├── flow.rs           # the transitional --new-flow selector (deleted by the flip)
    ├── recipe.rs         # the new chain's recipe (recipe_version 2), one section per stage
    ├── telemetry.rs      # opt-in JSONL perf/context record (never perturbs output)
    ├── version.rs        # build identity, pipeline_version, params hash
    └── types.rs          # LinearImage, FilmBase, Reconstruction, params, errors
```

The tree is the shipped module set, not a proposal — it had drifted by nine modules
and is worth re-checking whenever one is added. The six new-flow modules are the
migration's chain (`docs/design-update.md`, `docs/nf-migration.md`). `--new-flow`
runs them — the fixed decode, scene correction's white balance and exposure, the
identity look, fit range, fit gamut's radial map into Display P3, and one Display P3
16-bit TIFF destination — but nothing in this spec's pipeline runs through them yet.

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
2; `--input-profile` (reserved, not applied) is unsupported, exit 4. `hanten inspect`
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
path via `film_base::estimate`'s finite-and-positive guard, and `hanten estimate
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
   H&D-curve** model **shipped** as the tagged sigmoid density curve (§7.3, task
   `algo/sigmoid`) and retired in `nf-retire/sigmoid-and-simple`; still open: possibly a power-law/exponent model
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
   as `hanten roll` (task `roll-conversion`): convert N frames from one shared, frozen
   recipe (`--params`), with per-frame overrides via a `--frames` manifest and a
   roll-level JSON report (per-frame status + the shared recipe once). See §8.
   What remains: the auto-cascade that *generates* the shared recipe (detect the
   film base once for the roll and emit the frozen recipe roll applies) —
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
   silently), and `hanten inspect` reporting **candidate rebate regions**
   (coordinates + spread) so CLI users confirm instead of measuring — the same
   data a future UI would highlight. The opt-in **content-based source**
   (`calibration.film_base = "content"` / `--base-content`, §9 ladder tier 3) is
   **reassigned** to the dedicated `film-base/content-fallback` task (item 13)
   and is **not** implemented here — the auto-refusal message only *suggests* it.
   Remaining: threshold tuning against full-size scans rides
   `real-scan-verification`.
9. **Light film holders.** Auto/border logic assumes a dark holder surround; some
   holders are white. Add a `--holder white|black` control (recipe key
   `measure.holder`) so detection knows the surround polarity. **Not** under
   `calibration`: a holder-polarity declaration is a property of the scanner
   setup, not a measurement of the roll, so it fails that section's inclusion
   test (§8). The old `film_base.holder` spelling named a section that no longer
   exists.
10. **Reuse-ready `hanten estimate` output — shipped** (`estimate-reuse-output`).
    The estimate report now carries the measured base in directly reusable
    forms (`film_base_flag` and a recipe-shaped `calibration` object) and `--grid` provides the
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
    JSON record per `hanten convert` (image + per-stage timing + run context) written
    to a local JSONL log and/or one-off file (`--telemetry` / `--telemetry-file`,
    `NC_TELEMETRY_LOG`; see §9), best-effort and byte-identical-output-preserving.
    The `telemetry/strategy` spike is **complete**; its approved
    [design note](telemetry-strategy.md) fixes the remaining shape. The client
    keeps custom JSON (no embedded OTel SDK/Collector) and sends a separately
    versioned, allowlisted upload projection to an nc-owned Cloudflare Worker +
    D1 service. Persistent `hanten telemetry enable` consent opts into automatic
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
    deterministic **apply** half has shipped as `hanten roll`: it converts a batch
    from one shared recipe, supports per-frame manifest overrides, and emits one
    roll report while preserving the single-frame conversion core. Roll-fixed
    parameters (`Dmin`) versus frame-local print controls remain the
    model. The open `base-acquisition-planner` owns the automatic **plan** half:
    an acquisition cascade (unexposed reference → rebate region → `--auto-base`
    → cross-frame agreement → drop-to-single; content estimation only on explicit
    opt-in) emits the frozen recipe and provenance that `hanten roll` replays.
    Tracked: shipped `roll-conversion`; open `base-acquisition-planner` and
    `film-base/content-fallback`.
14. **Roll-fixed `Dmax` from a fully-exposed reference frame.** *(Shipped as
    `dmax-reference`, then retired by `nf-retire/dmax-machinery`.)* It made the display
    anchor a roll-fixed reference density measured from the light-struck leader
    (`pipeline_version` 1–2 record its nominal default). The base-derived
    `mid-at-base-offset` placement replaced it as the default in
    `nf-retire/sigmoid-and-simple`, and the reference, its flags and recipe key were
    then removed; the reference build keeps the old behaviour comparable.
15. **IR-assisted film-holder detection.** First consumer of the IR channel
    besides item 1. Chromogenic dyes are IR-transparent, so all such film (base,
    picture, even fully-exposed leader) is bright in IR while the opaque holder is
    dark — a content-independent holder mask that RGB can't produce (holder and
    dense film are both dark in RGB). The mask is classified in **sub-edge
    segments** (a holder may cover only part of an edge), and holder segments are
    excluded before the RGB rebate search. Gated by **measuring the IR plane**
    (§6.1, `ir-usability-detection`) — not by a declared film type, not by color
    model, and not by IR-plane presence. Also sidesteps holder *color* (item 9),
    since opacity, not color, is the IR signal. Tracked: `ir-holder-detection`.
16. **Conversion versioning & baseline comparison.** *(Shipped 2026-07-28 —
    `conversion-versioning`.)* Every report carries an `identity` block (§9):
    build identity (crate semver + git commit + dirty flag + target), a behavioral
    `pipeline_version` (bumps *only* on default-behavior changes, gated by a golden
    drift test over the default render, the default film-base estimate, and the
    default recipe values — see §9 for what that gate does **not** cover; `0` = the
    `v0` baseline, `4` = current as of 2026-09-09), and a resolved-params hash.
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
    (convert/inspect/estimate) and `hanten params` — uses `println!`, which
    panics on a closed pipe — the `hanten … | head` / `… | jq 'first'` case, where the
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
    binding 203-nit reference white, the display tone (a bounded Hermite shoulder
    as first shipped; extended Reinhard since `nf-retire/display-tones`), and
    same-luminance radial RGB-cube boundary
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
    display tone (a Hermite shoulder as first shipped; the lifted extended Reinhard
    since `nf-retire/display-tones`) and same-luminance radial gamut
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
    AVIF, and linear/PQ/HLG HDR TIFF interchange.
    `hanten roll` naming/manifests migrate with presets so suffixes derive from each
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
  **resolved**: **linear ACEScg**, shipped as `film-master`: it lands after NC film
  RGB v1 mapping and before print/display controls, and is not physical scene
  recovery or Rec.2100 display HDR. See §5.
- ~~Whether the embedded TIFF metadata should carry the full recipe~~ —
  **resolved**: the recipe lives in the sidecar JSON only (paired by name with
  the output); the TIFF embeds just the ICC profile. See §5.
