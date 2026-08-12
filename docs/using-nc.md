# Using `nc`

A practical guide to converting film negative scans to positives with `nc`.

> **Scope.** This is the *user-facing* guide: what to run, in what order, and why.
> For the authoritative design rationale and the full parameter semantics, see
> [`design-spec.md`](design-spec.md). Where the two disagree, the spec wins on
> *intent* — but this document is verified against the binary, so it wins on
> *what the CLI currently accepts*.
>
> **Verified against:** `nc 0.1.0`, `pipeline_version 3`, built at commit
> `698337e62ef0`. The staleness signal is `pipeline_version`: if `nc --version`
> reports a different one, treat this document as suspect and re-verify.
>
> **Known issue:** under the default render the gain map is inert (no HDR
> headroom) — see the callout in §8.

---

## 1. The mental model

`nc` converts a **negative** scan into a **positive** image. Three properties
shape every workflow below:

- **Deterministic.** Same input + same parameters ⇒ byte-identical output (on one
  build and architecture). There is no hidden per-frame adaptation unless you
  explicitly ask for it.
- **Every knob is a CLI flag *and* a recipe key**, and nothing is reachable only
  from code. Passing a flag that doesn't apply to your selected *curve* or *preset*
  is a **loud error**, never a no-op. The exception is `--reconstruction simple`,
  which has no print stage: `--print-exposure`, `--black-point`,
  `--highlight-compress` and `--white-balance` are accepted there and silently do
  nothing (verified — the output is byte-identical).
- **Calibrate once, apply many.** The film base (`Dmin`) and the reference density
  (`Dmax`) are properties of the *roll* — film stock, development, scanner — not
  of an individual frame. You measure them once and reuse them, which is what
  keeps a whole roll color-consistent. **`Dmin` has no default: every `convert`
  must say where the film base comes from**, because it sets the black point and
  the colour balance together.

That last point is the whole workflow:

```
   plan                        freeze                    apply
┌──────────────┐          ┌───────────────┐        ┌──────────────────┐
│ nc inspect   │  ──────► │ recipe.json   │ ─────► │ nc convert       │
│ nc estimate  │          │ (Dmin, Dmax,  │        │ nc roll          │
│              │          │  print knobs) │        │                  │
└──────────────┘          └───────────────┘        └──────────────────┘
  measure from a            reuse-ready forms         one shared recipe
  reference frame           printed by estimate       across every frame
```

### Value terms (read this once)

A pixel lives in several **per-channel** domains, and they don't all run the same
direction. As scene luminance rises:

```
transmission ↓     density ↑     positive ↑     output ↑
```

"Bright" and "dark" in this document always describe the **scene**, never a raw
pixel value. The film base is the *highest* transmission on the negative yet
renders to *black* in the positive.

- **`Dmin`** — a per-channel **transmission**: the unexposed film base. This is
  the `--film-base R,G,B` value.
- **`Dmax`** — a **scalar** in **density** units: the roll's *reference* density.
  This is `--d-max D`.

They are not two ends of one scale; don't conflate them.

> **`Dmax` is a reference density, not automatically "display white".** Which tone
> the reference *places* is a separate control:
>
> - Under the **sigmoid** curve (the default), `Dmax` and the placement are
>   deliberately split. The default anchor puts **mid-grey at 0.5·Dmax** and lets
>   display white float above *that placement* rather than being pinned. See §6.
> - Under the **exponential** curve, `Dmax` does map to display white directly.
>
> Some CLI help text still calls `--d-max` a "display-white anchor" — that wording
> predates the split and is accurate only for the exponential curve.

---

## 2. Getting a binary

```sh
cargo build --release      # → target/release/nc
```

A fresh machine needs CMake, C and C++ compilers, libclang (bindgen), and NASM —
the build compiles pinned libultrahdr, libjpeg-turbo, and libaom from vendored
source. Only those **native** libraries are vendored: cargo still fetches the Rust
crates from crates.io, so the build needs network access (or a warm cargo cache).

```sh
target/release/nc --version
```

prints the version, the **`pipeline_version`** (the render-behavior identity), the
git commit, and the target triple. Quote it in bug reports — output is only
guaranteed byte-identical within one build and architecture.

> `cargo build` neither installs the binary nor changes `PATH`, and on most
> systems a bare **`nc` is netcat**. Examples below write `nc` for brevity; run
> `target/release/nc`, or put it on your `PATH` under a name you choose.

---

## 3. The five commands

| Command | Purpose | Writes an image? |
|---|---|---|
| `nc inspect` | **"What is this file?"** — format, dimensions, IR presence, scanner metadata, resolved input semantics, candidate rebate regions. | No |
| `nc estimate` | **"What number do I freeze?"** — measure the film base (`Dmin`), and optionally `Dmax`. Prints **reuse-ready** flag and recipe forms. | No |
| `nc params` | Print the full default recipe as JSON — the scaffolding starting point. | No |
| `nc convert` | Convert one frame. The full parameter surface. | Yes |
| `nc roll` | Convert many frames from **one shared frozen recipe**. | Yes |

Every command except `params` emits a **JSON report on stdout** on success
(`--report none` to suppress, `--report-file PATH` to redirect); `params` takes no
flags at all and just prints the default recipe. Logs and warnings go to
**stderr**, so stdout stays clean for piping into `jq`.

> **A hard failure emits no report at all** — stdout is empty. A decode error, a
> memory refusal, or a measurement that fails (`estimate` finding no rebate band)
> exits non-zero before the report is written. Only `roll` is different: it
> aggregates per-frame failures into its report and still emits it. So a script
> must check the exit code, not just parse stdout.

---

## 4. The core workflow

### Step 1 — Inspect the scan

```sh
nc inspect scan.tif | jq '{decode, input_color, warnings}'
```

Tells you what you actually have: dimensions, bit depth, whether an **IR plane** is
present (HDRi 64-bit input), the scanner make/model/software, the SilverFast XMP
mode metadata, and — importantly — how `nc` **resolved the input semantics**
(`transfer` and `meaning`) with the evidence behind each.

`inspect` is non-fatal by design: if a rebate band is detectable it suggests a
`Dmin`, and if selection *refuses* it still reports the candidate rectangles it
found:

```sh
nc inspect scan.tif | jq '.base_candidates'
```

Confirm one of those rectangles and pass it to step 2 as `--base-region` — that
saves measuring coordinates by hand on a scan where auto-detection won't commit.

### Step 2 — Measure the film base

**This step is mandatory.** `convert` and `roll` refuse to run without a stated
film base — there is no default, because `Dmin` is the divisor of the density
conversion and sets the black point and the colour balance together:

```
usage: no film base selected: pass --film-base R,G,B (a Dmin measured once per
       roll, e.g. with `nc estimate`), --base-region X,Y,W,H to sample an
       unexposed border, or --auto-base to detect the rebate band …
```

`estimate` and `inspect` are the deliberate exceptions — `estimate` exists to
*produce* a base, so it still resolves an unstated source to `auto`, and `inspect`
always runs the detector. Requiring a base there would make the measure-once
workflow circular.

Three sources, in descending order of reliability:

**(a) An unexposed reference frame** — the best option. Use `--grid` to sample five
cells (corners + center) and cross-check them:

```sh
nc estimate unexposed-leader.tif --grid
```

Disagreement between cells warns loudly — that diagnoses light leaks, illumination
falloff, or dust *before* it silently poisons a whole roll.

**(b) A known border region** on a normal frame:

```sh
nc estimate scan.tif --base-region 0,0,24,24
```

`nc` checks the rectangle for uniformity and warns if it looks like it mixes rebate
with image content.

**(c) Auto-detection** — scans inward for the unexposed rebate band behind the
film holder. Still one flag; what's gone is arriving there by omission:

```sh
nc estimate scan.tif --auto-base
```

> **Real scans are laid out `dark holder → thin inset rebate → picture`** — the
> rebate is *not* the outer margin. Auto-detection is therefore best-effort and
> **fails loudly** rather than guessing. Prefer (a) or (b) for production work.

Either way, `estimate` hands you the result in **reuse-ready form**:

```json
{
  "film_base": { "r": 0.16311894, "g": 0.080109864, "b": 0.037720304 },
  "film_base_flag": "--film-base 0.16311894,0.080109864,0.037720304",
  "film_base_recipe": {
    "source": { "explicit": [0.16311894, 0.080109864, 0.037720304] }
  }
}
```

Copy `film_base_flag` straight onto a command line, or splice `film_base_recipe`
into a recipe under the `film_base` key. That is the intended handoff — no manual
transcription of floats.

Add `--strict` when scripting: it turns "plausible-looking but bad" into a hard
failure instead of a value your pipeline silently bakes in.

### Step 3 — (Optional) Measure `Dmax`

The reference density defaults to a **fixed nominal** value (1.3 density),
scene-independent and reused across the roll. You can instead measure it from a
**fully-exposed** reference frame — the light-struck roll leader — though see the
reliability caveat in §6 before relying on the result:

```sh
nc estimate leader.tif \
  --film-base 0.163,0.080,0.0377 \
  --d-max-region 100,100,80,80
```

which reports:

```json
{ "dmax": 0.39084455,
  "d_max_flag": "--d-max 0.39084455",
  "d_max_recipe": { "dmax": { "explicit": 0.39084455 } } }
```

The `d_max_recipe` fragment nests under **`reconstruction.curve`**.

### Step 4 — Write the recipe

`roll` is configured **only** by recipe — it has no `--film-base` — so a roll needs
a recipe file. Splice `estimate`'s reuse-ready fragments together with your
parameter choices:

```jsonc
{
  "film_base": { "source": { "explicit": [0.163, 0.080, 0.0377] } },
  "reconstruction": { "curve": { "type": "sigmoid",
                                 "dmax": { "explicit": 0.391 } } }
}
```

Omitted sections take their defaults, so a recipe only needs to carry what you
decided. `nc params` prints the full default document if you want a scaffold to
edit.

> **`--dump-params` does not freeze your measurements.** It writes the resolved
> *config*, which for a measured value is the **mode**, not the number — a run
> with `--auto-base --auto-wb percentile` dumps `"auto"` and `"percentile"`, and
> the report's measured values are nowhere in it. Two different scans with the
> same flags produce identical dumps. So a recipe dumped from an auto run
> **re-measures on every frame of the roll**, which is exactly what `roll` exists
> to prevent. Only explicit values freeze. (The automatic `<output>.json` sidecar
> has the same content, and reloads through `--params` unchanged.)

### Step 5 — Apply to the whole roll

```sh
nc roll frames/*.tif --out-dir positives/ --params roll-recipe.json
```

Every frame gets the identical film base, `Dmax`, and print controls, so the roll
is color-consistent. Outputs are named `<input-stem>_positive.<ext>`, the suffix
coming from the resolved preset; a roll-level
JSON report lands on stdout.

---

## 5. Recipes

### Shape

The recipe *is* the resolved config. Get the current default with:

```sh
nc params
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
    "curve": { "type": "sigmoid", "contrast": 2.0686874, "toe": 0.2,
               "shoulder": 0.6, "dmax": "fixed",
               "anchor": { "mid-at-dmax-fraction": 0.5 } }
  },
  "input":     { "transfer": "auto", "meaning": "auto",
                 "film_type": "unknown", "export_ir": null },
  "film_base": { "source": null },
  "print":     { "print_exposure": 0.0, "black_point": 0.0,
                 "white_balance": { "explicit": [1.0, 1.0, 1.0] },
                 "highlight_compress": 0.0, "linear_range": [0.0, 1.0] },
  "output":    { "preset": "gain-map-hdr", "depth": "u16",
                 "output_profile": null, "bigtiff": "auto" }
}
```

`film_base.source` prints as `null` because it has **no default** — this document
is a template to edit, not a runnable recipe. `convert` and `roll` reject an
unstated base.

### Partial recipes are fine

Omit any section and serde defaults fill the gap. This minimal recipe produces a
**byte-identical** result to the full flag form:

```json
{
  "film_base": { "source": { "explicit": [0.163, 0.080, 0.0377] } },
  "reconstruction": { "curve": { "type": "sigmoid",
                                 "dmax": { "explicit": 0.391 } } }
}
```

> **Gotcha:** the tagged objects need their tag. If you include
> `reconstruction.curve` at all, you **must** include its `"type"` — omitting it
> fails with `reconstruction.curve is missing 'type'`. Omitting the whole `curve`
> object is fine.

The `curve` object above is the **sigmoid** shape. Selecting the exponential curve
resolves a different, smaller set of keys:

```json
{ "type": "exponential", "gamma": 2.0, "dmax": "fixed" }
```

### Strictness

Every recipe struct uses `deny_unknown_fields`, so a typo is rejected rather than
ignored:

```
usage: invalid recipe t.json: unknown field `exposure`,
       expected one of `print_exposure`, `black_point`, `white_balance`,
       `highlight_compress`, `linear_range`
```

This means a **misplaced** key fails too — a key must live under the stage section
that owns it (`--export-ir` ⇒ `input.export_ir`, not top level).

Removed legacy forms (a top-level `algorithm`, or sibling
`density`/`sigmoid`/`simple` sections) produce a **migration error** explaining the
replacement. They are not accepted as aliases.

### Precedence

**Flags always win over the recipe.** Precedence is by *source*, not value — an
explicit `--white-balance 1,1,1` over a recipe's `auto` mode means neutral gains,
not re-estimation.

```sh
nc convert scan.tif -o out.jpg --params roll-recipe.json --d-max 0.5
#                                                          ^ overrides the recipe
```

### Sidecars: every conversion is reproducible

**Every** written image gets a `<output>.json` sidecar automatically — no flag
needed:

```json
{
  "meta":   { "nc_version": "0.1.0", "git_commit": "e4a56bb2540d",
              "pipeline_version": 2, "target": "aarch64-apple-darwin",
              "params_hash": "18b95264170ab67a" },
  "params": { ...the exact recipe... }
}
```

`params` is the recipe body. `meta` is provenance and no part of it changes a
pixel — but it is not entirely ignored either: the loader parses
`meta.pipeline_version`, rejects a malformed one (exit 2), and emits a
`--strict`-promotable warning when it differs from the running build, so an
archived replay tells you the render may have moved.
Feed the sidecar straight back to reproduce the conversion exactly:

```sh
nc convert scan.tif -o repro.jpg --params out.jpg.json
# → byte-identical to out.jpg
```

### Per-frame overrides in a roll

Use a `--frames` manifest instead of positional inputs when individual frames need
a tweak on top of the shared recipe:

```json
{ "frames": [
    { "input": "a.tif" },
    { "input": "b.tif", "output": "b-brighter.jpg",
      "params": { "print": { "print_exposure": 1.0 } } }
] }
```

```sh
nc roll --frames frames.json --out-dir positives/ --params roll-recipe.json
```

An explicit manifest `output` goes through the same suffix rule as `convert`, so
its extension must match the resolved preset's container — `.jpg` under the
default, `.tiff` under `legacy`/`display-p3`/`film-master`, `.avif` under
`hdr-pq`/`hdr-hlg`.

---

## 6. Reconstruction and curves

Two reconstruction types, selected with `--reconstruction`:

| Type | What it does |
|---|---|
| `density` *(default)* | Density-domain inversion (Cineon / negadoctor lineage). What you want for colour negative. |
| `simple` | Direct channel inversion `1 − scan/Dmin` — **a debugging baseline**, not a production path. No density correction, curve, or `Dmax`, so it isolates decode plus film base. |

> **`simple` is not the B&W path**, despite what `--reconstruction`'s help text
> says. B&W film is still a density medium with its own characteristic curve, so
> B&W support (`algo/bw-support`) runs through `density` too — what it adds is a
> *mono colour model* that pools R,G,B into one gray, not a different
> reconstruction.
>
> The difference between the two is which domain the inversion happens in.
> `simple` is affine in **transmission** (`1 − t/Dmin`); `density` goes through
> the log domain and, with neutral correction, reduces to a **power law**
> (`positive ∝ (t/Dmin)^(−gamma)`). Film density is logarithmic in exposure — that
> is what a characteristic curve *is* — so the power law is the inversion that
> corresponds to something physical. `1 − t/Dmin` corresponds to none: it
> saturates toward 1 as the negative gets denser, compressing highlights by
> accident rather than by a tone decision.

Under `density`, two curves, selected with `--density-curve`:

| Curve | Knobs | Defaults |
|---|---|---|
| `sigmoid` *(default)* | `--sigmoid-contrast` (mid-density slope), `--sigmoid-toe` / `--sigmoid-shoulder` (knee widths in log10 density; `0` disables), plus the anchor flags below | `contrast 2.0686874`, `toe 0.2`, `shoulder 0.6` |
| `exponential` | `--density-gamma` — the straight line's slope | `gamma 2.0` |

The two curves take **different recipe keys**, and mixing them is rejected —
`anchor` on an exponential curve fails with *"`anchor` is a sigmoid-curve key, but
the curve type is "exponential" (its knobs are `gamma` and `dmax`)"*.

### Sigmoid anchoring — where the curve pins a tone

The sigmoid curve separates **which density is the reference** (`dmax`) from
**which tone that reference places** (`anchor`). This split is the substantive
outcome of [`reports/sigmoid-reference-baseline.md`](reports/sigmoid-reference-baseline.md),
and it exists because the two knobs otherwise fight: raising contrast pivots the
line *about the pinned point*, so pinning white necessarily drags everything below
it down.

| Anchor | Flag | Recipe (`reconstruction.curve.anchor`) |
|---|---|---|
| Mid-grey at a fraction of the reference *(default, F = 0.5)* | `--sigmoid-mid-fraction F` | `{"mid-at-dmax-fraction": 0.5}` |
| Display white *at* the reference — the pre-2026-08 rule | `--sigmoid-white-at-d-max` | `"white-at-dmax"` |

Raising `F` renders the roll **darker**; lowering it renders **brighter**.

`--sigmoid-white-at-d-max` is retained as an explicit **diagnostic**, not a
recommended setting: at a photographic contrast it renders midtones roughly 2.5–3.6
stops dark. It is kept reachable so the original defect can be reproduced on demand.

> **Provisional values.** The measurement behind these defaults filters *methods*
> rather than tuning parameters, so the numbers are not final. The mid fraction
> `F = 0.5` rests on a chart read that is not a true Status M density (measured
> α ≈ 0.48–0.57 across three stocks). A per-stock datasheet anchor — the better
> form on the evidence — is not shipped; it awaits the `algo/film-stock-profiles`
> registry. Expect movement, with a `pipeline_version` bump when it happens.

### Density correction (before the curve)

| Flag | Effect |
|---|---|
| `--density-scale R,G,B` | Per-channel density gain |
| `--density-offset R,G,B` | Per-channel density offset — **orange-mask compensation** |
| `--shadow-balance R,G,B` | Per-channel offset applied to the positive's **shadows** |
| `--highlight-balance R,G,B` | Per-channel offset applied to the positive's **highlights** |
| `--balance-range LO,HI` | Fix the regional-balance tone anchors (default: measured per frame) |

A positive balance value brightens that channel in that region. `0,0,0` (default)
skips the regional pass entirely and is bit-exact with the unbalanced output.

Negative values are common for the balance flags and a leading `-` is accepted.

For **roll consistency**, measure the range once and freeze it. The range is only
*measured* when the regional pass runs — that is, when the two balances **differ**
— so set them first:

```sh
nc convert scan.tif -o out.jpg --film-base … \
  --shadow-balance=-0.05,0,0 --highlight-balance 0.05,0,0
```

Read the reported top-level `balance_range` (e.g. `[-0.368, 0.494]`), then pass it
as `--balance-range LO,HI` on the rest. With the neutral default the pass
short-circuits and no range is reported at all.

### The `Dmax` reference density — four mutually exclusive choices

Where the reference density comes from. (What it *places* is the anchor, above.)

| Flag | Behavior |
|---|---|
| *(none)* / `--fixed-d-max` | Fixed nominal reference (1.3 density), reused across the roll. **Default.** Darker frames render darker — faithful relative exposure. |
| `--d-max D` | Explicit roll-fixed reference — your measured calibration. |
| `--auto-d-max` | Measure per frame. **Per-frame exposure normalization**: brightens underexposed frames and breaks roll consistency. Grading, not conversion. |
| `--no-d-max` | No reference — scene-referred output (base → 1.0, detail above). **Exponential only**: the sigmoid needs an anchor and rejects it (exit 2), so pair it with `--density-curve exponential`. |

> **A caveat on measuring `Dmax` from a leader** (§4 step 3). The baseline report
> found leader-measured `Dmax` untrustworthy on three independent counts:
> same-stock rolls differed by a full stop (0.295 density) while their bases agreed
> to 0.0005; real frame content measured *above* the leader value; and grain
> sensitivity makes "fully exposed" ill-posed, so a leader is a uniform field at an
> *uncontrolled* level. Blue is the least uniform channel in every leader measured.
> Treat a measured `Dmax` as better than nothing, not as a calibration you can
> trust across rolls.

### Nothing is silently ignored

Cross-curve and cross-type flags are **usage errors**:

```sh
nc convert … --density-curve sigmoid --density-gamma 1.8
# usage: --density-gamma sets the exponential curve's gamma, but the resolved
#        curve is sigmoid — its mid-density slope is --sigmoid-contrast

nc convert … --reconstruction simple --density-gamma 1.8
# usage: --density-gamma configures density reconstruction, but the resolved
#        reconstruction is `simple`
```

This is deliberate: a flag that quietly did nothing would be worse than a failure.

---

## 7. Print / tone controls

| Flag | Effect |
|---|---|
| `--print-exposure F` | Overall positive exposure |
| `--black-point F` | Paper black / shadow floor |
| `--white-balance R,G,B` | Explicit highlight / neutral gains |
| `--auto-wb MODE` | Estimate gains per frame — `gray-world` (≈ NLP Auto-AVG) or `percentile` (≈ NLP Auto-Neutral, more robust to a dominant scene colour) |
| `--highlight-compress F` | Highlight roll-off |
| `--linear-range LOW,HIGH` | Affine black/white placement, applied last — **display presets only**, which includes the default (see §8) |

`--white-balance` and `--auto-wb` are the two faces of one setting and are mutually
exclusive. The report tells you what was actually used:

```sh
nc convert scan.tif -o out.jpg --film-base … --auto-wb percentile \
  | jq '.white_balance'
# [1.2501934, 1.0, 0.6589681]
```

Feed that back as an explicit `--white-balance` to freeze it across a roll.

---

## 8. Output presets

> ### ⚠️ Known issue: the default gain map is inert
>
> Under the **default sigmoid**, the HDR rendition peaks at *exactly* the 203-nit
> reference white, so `gain-map-hdr` writes a structurally valid gain-map JPEG
> whose `GainMapMax` decodes as **1.0x** — no HDR headroom. Viewers show it as a
> normal SDR image.
>
> This is a **rendering** property, not a container defect. The
> reference-anchored sigmoid pins mid-grey at half the reference density and rolls
> its shoulder so diffuse white lands *at* reference white, so by construction
> nothing exceeds it. The `exponential` curve on the same frame reaches 4.87x,
> because it pins white at `Dmax` with no placement rule and lets contrast push
> values past reference white. The film is not the limitation — negative stock has
> wide latitude; the *print rendering* decides whether output exceeds diffuse
> white, and today's default declines to.
>
> So HDR is a rendering-intent choice rather than a correctness gap, and it is
> deliberately deprioritised. **Changing container does not help**: `hdr-pq` and
> `hdr-hlg` consume the same shared display source and the same HDR renderer, so
> under the default curve they too report their brightest pixel at or below 203
> nits with none of the 1000-nit headroom used. Real headroom needs a *render*
> change — the `exponential` curve, or `Dmax`/print controls that push values past
> reference white.

`--output-preset` (recipe key `output.preset`) is an **atomic** policy choice: a
named preset resolves container, bit depth, and colour profile itself. `custom` is
the deliberate exception — see below.

Twelve names are accepted, **every one pins a required suffix**, and there is no
planned-but-unaccepted tier left, so an unknown name always means a typo:

| Preset | Container | Suffix | Depth | Contents |
|---|---|---|---|---|
| `gain-map-hdr` *(default)* | JPEG | `.jpg` / `.jpeg` | u8 base | SDR base + gain map, packaged **dual-dialect**: ISO 21496-1 segments *and* the legacy Ultra HDR v1 XMP/MPF. |
| `ultra-hdr-v1` | JPEG | `.jpg` / `.jpeg` | u8 base | The **same pixels** as `gain-map-hdr`, legacy XMP/MPF only — no ISO claim. |
| `legacy` | TIFF | `.tif` / `.tiff` | u16 / f32 | The transitional path: print controls run before the output ICC transform. |
| `custom` | TIFF | `.tif` / `.tiff` | u16 / f32 | Same bytes as `legacy`; the difference is **provenance** — it says the combination was chosen. |
| `film-master` | TIFF | `.tif` / `.tiff` | f32 | Unclamped **linear ACEScg**, straight from the NC film RGB v1 mapping. Bypasses every print/display control. |
| `display-p3` | TIFF | `.tif` / `.tiff` | u16 | Modern-pipeline SDR render, losslessly stored in **Display P3**. |
| `compatibility` | TIFF | `.tif` / `.tiff` | u16 | The same SDR render in **sRGB**, for broad compatibility. |
| `hdr-pq` | AVIF | `.avif` | 10-bit | 4:4:4 Rec.2100 **PQ**. |
| `hdr-hlg` | AVIF | `.avif` | 10-bit | 4:4:4 Rec.2100 **HLG**. |
| `hdr-linear-tiff` | TIFF | `.tif` / `.tiff` | f32 | Display-linear **BT.2020**, no transfer applied — HDR interchange. |
| `hdr-pq-tiff` | TIFF | `.tif` / `.tiff` | u16 | The same signal as `hdr-pq`, as losslessly stored Rec.2100 **PQ** codes. |
| `hdr-hlg-tiff` | TIFF | `.tif` / `.tiff` | u16 | The same signal as `hdr-hlg`, as losslessly stored Rec.2100 **HLG** codes. |

> **The default writes a JPEG.** `nc convert scan.tif -o out.tiff` now *fails* —
> with no `--output-preset`, nc resolves `gain-map-hdr` and wants `.jpg`. For a
> TIFF, name the preset: `--output-preset legacy` (or `display-p3`,
> `compatibility`, `film-master`, …).

`gain-map-hdr` and `ultra-hdr-v1` are **one render packaged twice** — identical
pixels, differing only in metadata dialect. Only the dual-dialect default decodes
as HDR on Apple platforms; `ultra-hdr-v1` exists for readers that predate ISO
21496-1.

The wrong suffix is a usage error, not a silent rename:

```
usage: output preset `hdr-pq` requires an output path ending in .avif
```

### Preset interaction rules

- An **atomic** preset rejects `--out-depth` / `--output-profile` / `--bigtiff`
  (from a flag *or* the recipe), because it resolves those itself. For
  `--output-profile` and `--bigtiff`, a value equal to the documented default —
  like `--bigtiff auto` — is accepted. **`--out-depth` is rejected by flag
  presence**, so even `--out-depth u16`, which *is* the default, errors alongside
  a named preset.
- **`custom` is the one named preset that is not atomic.** It accepts the
  depth/profile/container selectors, resolving the same branch and the same bytes
  as `legacy`. Use it to record that the combination was a decision.
- **`--out-depth` replaces the old `--output-hdr` / `--output-sdr` pair**:
  `u16` (default, archival) or `f32`. Only `legacy` and `custom` consult it.
- `f32` there is the **transitional print-rendered** float TIFF in the selected
  output space. It is not `film-master` (unclamped linear ACEScg, no print
  controls) and not `hdr-linear-tiff` (display-linear BT.2020) — three different
  f32 TIFFs.
- `film-master` additionally rejects `--auto-d-max` / `--auto-balance-range` and
  every non-default downstream control — it bypasses them, so accepting them
  would be a lie.
- `--linear-range` is consumed **only** by a display preset — which the default
  now is, so it works out of the box. On `legacy` / `custom` it stays a loud error
  rather than a silently ignored knob.

### `roll` takes every preset

The old `convert`-only restriction is gone. `roll` derives
`<stem>_positive.<ext>` from each frame's own resolved preset, so a
`gain-map-hdr` roll writes `_positive.jpg` and an `hdr-pq` roll writes
`_positive.avif`. An explicit manifest `output` path goes through the same
suffix-mismatch rule `convert` uses.

### Depth, profile and container knobs

These are consulted by `legacy` and `custom` only.

| Flag | Values |
|---|---|
| `--out-depth` | `u16` (default, archival) or `f32` (written verbatim, values above 1.0 preserved) |
| `--output-profile` | `sRGB`, `prophoto`, `acescg`, `display-p3`, or a path to an ICC file |
| `--bigtiff` | `auto` (default — promote only when needed), `on`, `off` |

---

## 9. Input semantics and the IR channel

`nc` resolves two **independent** axes from the container's evidence, and you can
assert either one:

| Flag | Values | Meaning |
|---|---|---|
| `--input-transfer` | `auto`, `linear` | How samples are **encoded** |
| `--input-meaning` | `auto`, `scanner-device`, `colorimetric` | What they **measure** |

Only `scanner-device` + a linear transfer enters the density path. `colorimetric`
is recognized but unsupported — `convert` rejects it even when explicitly asserted.
`inspect` shows the resolution and the evidence chain under `input_color`.

`--input-profile` is reserved and currently rejected: input-side ICC application
has no validated placement in the pipeline yet.

### IR (HDRi 64-bit input)

The IR plane is decoded and **preserved but not acted on** by default. Two things
you can do with it:

- `--export-ir PATH` writes the decoded plane out. **`convert` only** — `roll`
  rejects `input.export_ir`, because one path cannot serve every frame, so IR
  planes have to be exported frame by frame.
- `--film-type chromogenic` enables **IR-assisted film-holder detection**:
  chromogenic dyes (C-41 colour and C-41-process B&W) are IR-transparent, so the
  holder can be masked off before the rebate search. `silver` (IR-opaque) and the
  `unknown` default keep the IR path off.

IR-based dust removal is not implemented.

> Any scan carrying an IR plane emits an "IR preserved but not used" warning, so
> **`--strict` will always fail on an IR scan** unless the IR path is engaged.

---

## 10. Reports, warnings, and exit codes

The JSON report on stdout carries the run identity, the effective recipe, the
resolved film base and `Dmax`, the white balance actually used, encode loss
statistics, and warnings:

```sh
nc convert scan.tif -o out.jpg --film-base … | jq '.loss, .warnings'
```

Clipping is reported, never silent:

```json
["output lost 126296 clipped and 0 non-finite of 695772 samples (18.15%)"]
```

Every encoder counts this, not just the TIFF ones — the gain-map JPEG and the
AVIF paths build the same report when they quantize.

Under the **default sigmoid the curve alone cannot clip**: its shoulder approaches
display white asymptotically and never reaches it. So a clip warning means
something downstream pushed samples past 1.0 — most often a **print control**
(`--print-exposure 12` clips 100% of a default-curve frame), or the `exponential`
curve, which hits white exactly at `Dmax` and exceeds it above. Check the print
controls before reaching for the curve or `Dmax`.

`--strict` promotes warnings **that reach the JSON report** to a hard error
(exit 1), after the report is emitted — the right default for scripts and CI.

> One deliberate exception: a failure to write an opted-in **telemetry**
> destination prints `nc: warning:` on stderr but is kept out of the report set, so
> it stays fail-soft even under `--strict`. Telemetry must never change a
> conversion's outcome. A script that needs to know telemetry landed has to check
> the file, not the exit code.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic / unexpected error — **including `--strict` with warnings present** |
| 2 | Invalid CLI usage or parameters (bad flag value, unsupported preset, wrong suffix, bad recipe) |
| 3 | Input read/decode error |
| 4 | Unsupported variant (e.g. a channel layout not handled yet) |
| 5 | Output write error |
| 6 | Resource limit — estimated peak memory exceeds the budget |

---

## 11. Operational flags

These are **not** conversion knobs. They never appear in a recipe and can never
perturb a pixel.

| Flag | Purpose |
|---|---|
| `--max-memory BYTES` | Peak-memory budget, checked **before decode**. Accepts `8GiB`, `4096MB`, or raw bytes. Default 6 GiB — a fixed value, so the pass/fail decision is machine-independent. Over budget ⇒ **exit 6**. |
| `--report` / `--report-file` | Report format (`json`, `none`) and destination |
| `-v` / `-vv` / `--quiet` | stderr verbosity — never pollutes stdout |
| `--strict` | Promote warnings to errors |

Two more are **`convert` only** — `roll`, `estimate` and `inspect` do not accept
them and exit 2 if given one:

| Flag | Purpose |
|---|---|
| `--telemetry` / `--telemetry-file` | Opt-in, fail-soft performance record (JSONL). Also `NC_TELEMETRY_LOG`. |
| `--seed N` | Reserved; nothing is stochastic today |

> **Caveat on `--max-memory`:** the budget also caps the TIFF read buffers, so a
> small-but-passing budget can turn a decodable file into an exit-3 decode failure.
> There is also a warning tier above ~70% of detected RAM — the one documented
> exception to machine-independence, since with `--strict` the same run can exit 0
> on a large machine and non-zero on a small one. The *image* is still identical;
> only the exit code differs.

On `roll`, the gate runs **per frame**: a rejected frame is recorded in the report,
its siblings are still written, and the roll exits **1**, not 6.

---

## 12. Troubleshooting

**"no film base selected"**
Neither `convert` nor `roll` has a default film base — but they take it from
different places. On **`convert`**, pass `--film-base R,G,B` (measured once per
roll), `--base-region X,Y,W,H`, or `--auto-base`. **`roll` accepts none of those
flags**: set `film_base.source` in the shared `--params` recipe instead.
`estimate` still defaults to auto, so `nc estimate scan.tif` remains the way to
get a value in the first place.

**"auto film-base detection found no uniform unexposed rebate band"**
The scan has no detectable rebate — it's cropped, or the holder covers it. Measure
the base from a reference frame and pass `--film-base`, or point at a known region
with `--base-region`. Content-based estimation is planned but not shipped.

**"base-region … is not uniform (worst per-channel relative spread …)"**
Your rectangle mixes rebate with image content. Check the coordinates against
`nc inspect`, or use `estimate --grid` on a genuinely unexposed frame.

**Heavy clipping in the report**
You are on the `exponential` curve — the default sigmoid cannot clip highlights. The
reference density is probably too low, pushing content past display white. Raise
`--d-max`, switch to the default sigmoid, or use `--no-d-max` with an f32 output for a
scene-referred float result.

**"reconstruction.curve is missing `type`"**
A partial recipe that includes the `curve` object must include its tag. Add the
type you actually intended — normally `"sigmoid"`, the default. Adding
`"type": "exponential"` also makes it parse, but it silently switches you to the
other curve and changes your pixels.

**`--strict` fails on every frame of an IR scan**
Expected — see §9: an unconsumed IR plane warns, and `--strict` promotes it.
`--film-type chromogenic` alone does **not** clear it: the plane is consumed only
when the base source is `auto` *and* the plane is marker-verified, so a frozen
explicit `--film-base` — the recommended roll workflow — still warns. Either drop
`--strict` for those runs, or use `--export-ir` so the plane is consumed.

**Output differs between two machines**
Determinism is scoped to one build and architecture. Transcendental FP and the
lcms2 colour transform differ by ~1 ULP across platforms. Compare
`nc --version` output — `pipeline_version`, commit, and target must all match.

---

## 13. Not yet available

So you don't go looking:

| Missing | Owning task |
|---|---|
| **A bare `-o out`** — the suffix must currently match the preset's container; deriving it is proposed | [`output/output-path-suffix`](tasks/output/output-path-suffix.md) |
| **Auto-cascade recipe generation** — a planner that produces a roll recipe for you, instead of you measuring and freezing it by hand | [`core/base-acquisition-planner`](tasks/core/base-acquisition-planner.md) |
| **Content-based film-base fallback** (`--base-content`) for cropped scans with no visible rebate | [`film-base/content-fallback`](tasks/film-base/content-fallback.md) |
| **IR dust removal** | roadmap follow-up, no task file yet |

[`docs/TASKS.md`](TASKS.md) is the authoritative status for all of it.
