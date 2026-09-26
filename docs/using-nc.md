# Using Hanten

A practical guide to converting film negative scans to positives with `hanten`.

> **Scope.** This is the *user-facing* guide: what to run, in what order, and why.
> For the authoritative design rationale and the full parameter semantics, see
> [`design-spec.md`](design-spec.md). Where the two disagree, the spec wins on
> *intent* — but this document is verified against the binary, so it wins on
> *what the CLI currently accepts*.
>
> **Verified against:** `hanten 0.1.0`, `pipeline_version 7`, built at commit
> `b36ca64` (under `--new-flow` the decode's slope is `reconstruction.linearization`,
> print contrast is `look.contrast` / `--contrast`, and the look's grade is
> `--channel-grade`, §11) plus `nf-retire/regional-balance` (the balance flags and keys
> retired, §5–§6) and `nf-destinations/preset-set` (the `--new-flow` destination
> flags). The staleness
> signal is `pipeline_version`: if
> `hanten --version` reports a different one, treat this document as suspect and
> re-verify.
>
> **Retired flags and presets** — the regional balance (`--shadow-balance`,
> `--highlight-balance`, `--balance-range`, `--auto-balance-range`),
> `--display-tone` and `--highlight-compress`,
> `--reconstruction`, `--density-curve sigmoid`, the `--sigmoid-*` flags, the
> `sigmoid-knees` / `sigmoid-flat` presets, and before them `legacy` / `custom` — are
> documented in the reference build's own guide
> (`git show origin/reserve:docs/using-nc.md`; `scripts/reference-snapshot/README.md`
> builds that binary). Here they are refused with a message naming the replacement.

---

## 1. The mental model

Hanten converts a **negative** scan into a **positive** image. Three properties
shape every workflow below:

- **Deterministic.** Same input + same parameters ⇒ byte-identical output (on one
  build and architecture). There is no hidden per-frame adaptation unless you
  explicitly ask for it. (The transitional `--new-flow` selector in §11 is the one
  flag outside that promise: it chooses a whole rendering chain.)
- **Every knob is a CLI flag *and* a recipe key**, and nothing is reachable only
  from code. Passing a flag that doesn't apply to your selected *curve* or *preset*
  is a **loud error**, never a no-op. The print controls reach every display
  preset whatever the curve: white balance, exposure and the black point run in the
  shared display stage, and `--display-tone-headroom` sizes the display tone inside
  each display renderer.
- **Calibrate once, apply many.** The film base (`Dmin`) is a property of the
  *roll* — film stock, development, scanner — not of an individual frame. You
  measure it once and reuse it, which is what keeps a whole roll color-consistent.
  **`Dmin` has no default: every `convert`
  must say where the film base comes from**, because it sets the black point and
  the colour balance together.

That last point is the whole workflow:

```
   plan                            freeze                    apply
┌──────────────────┐          ┌───────────────┐        ┌──────────────────────┐
│ hanten inspect   │  ──────► │ recipe.json   │ ─────► │ hanten convert       │
│ hanten estimate  │          │ (Dmin,        │        │ hanten roll          │
│                  │          │  print knobs) │        │                      │
└──────────────────┘          └───────────────┘        └──────────────────────┘
  measure from a                reuse-ready forms         one shared recipe
  reference frame               printed by estimate       across every frame
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
- **The anchor** — a **scalar** in **density** units: the corrected density that
  renders to display white. It is placed from the film base: mid-grey sits
  `--anchor-mid-offset` (default 0.62) density above it (§6).

They are not two ends of one scale; don't conflate them. (Earlier builds also read a
roll *reference* density, `Dmax`, measured off a light-struck leader; it retired with
the placements that read it — see §6.)

---

## 2. Getting a binary

```sh
cargo build --release      # → target/release/hanten
```

A fresh machine needs CMake, C and C++ compilers, libclang (bindgen), and NASM —
the build compiles pinned libultrahdr, libjpeg-turbo, and libaom from vendored
source. Only those **native** libraries are vendored: cargo still fetches the Rust
crates from crates.io, so the build needs network access (or a warm cargo cache).

```sh
target/release/hanten --version
```

prints the version, the **`pipeline_version`** (the render-behavior identity), the
git commit, and the target triple. Quote it in bug reports — output is only
guaranteed byte-identical within one build and architecture.

> `cargo build` neither installs the binary nor changes `PATH`. Examples below
> write `hanten` for brevity; run `target/release/hanten`, or put it on your
> `PATH`. (The binary was called `nc` until 2026-09-21, which collided with
> netcat; `hanten` does not.)

---

## 3. The six commands

| Command | Purpose | Writes an image? |
|---|---|---|
| `hanten inspect` | **"What is this file?"** — format, dimensions, IR presence, scanner metadata, resolved input semantics, candidate rebate regions. | No |
| `hanten estimate` | **"What number do I freeze?"** — measure the film base (`Dmin`). Prints **reuse-ready** flag and recipe forms. | No |
| `hanten params` | Print the full default recipe as JSON — the scaffolding starting point. | No |
| `hanten convert` | Convert one frame. The full parameter surface. | Yes |
| `hanten roll` | Convert many frames from **one shared frozen recipe**. | Yes |
| `hanten measure-roll` | **"What white balance does this roll need?"** — measured once over the roll's frames, for the new chain (§11). Prints reuse-ready flag and recipe forms. | No |

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
hanten inspect scan.tif | jq '{decode, input_color, warnings}'
```

Tells you what you actually have: dimensions, bit depth, whether an **IR plane** is
present (HDRi 64-bit input), the scanner make/model/software, the SilverFast XMP
mode metadata, and — importantly — how `hanten` **resolved the input semantics**
(`transfer` and `meaning`) with the evidence behind each.

`inspect` is non-fatal by design: if a rebate band is detectable it suggests a
`Dmin`, and if selection *refuses* it still reports the candidate rectangles it
found:

```sh
hanten inspect scan.tif | jq '.base_candidates'
```

Confirm one of those rectangles and pass it to step 2 as `--base-region` — that
saves measuring coordinates by hand on a scan where auto-detection won't commit.

`inspect` also reports the **effective area** — the region `hanten` reads measurements
over, after the film holder and a border inset are removed. Worth a look on any
uncropped scan; see [The measurement region](#the-measurement-region-the-effective-area).

### Step 2 — Measure the film base

**This step is mandatory.** `convert` and `roll` refuse to run without a stated
film base — there is no default, because `Dmin` is the divisor of the density
conversion and sets the black point and the colour balance together:

```
usage: no film base selected: pass --film-base R,G,B (a Dmin measured once per
       roll, e.g. with `hanten estimate`), --base-region X,Y,W,H to sample an
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
hanten estimate unexposed-leader.tif --grid
```

Disagreement between cells warns loudly — that diagnoses light leaks, illumination
falloff, or dust *before* it silently poisons a whole roll.

**(b) A known border region** on a normal frame:

```sh
hanten estimate scan.tif --base-region 0,0,24,24
```

`hanten` checks the rectangle for uniformity and warns if it looks like it mixes rebate
with image content.

**(c) Auto-detection** — scans inward for the unexposed rebate band behind the
film holder. Still one flag; what's gone is arriving there by omission:

```sh
hanten estimate scan.tif --auto-base
```

> **Real scans are laid out `dark holder → thin inset rebate → picture`** — the
> rebate is *not* the outer margin. Auto-detection is therefore best-effort and
> **fails loudly** rather than guessing. Prefer (a) or (b) for production work.

Either way, `estimate` hands you the result in **reuse-ready form**:

```json
{
  "film_base": { "r": 0.16311894, "g": 0.080109864, "b": 0.037720304 },
  "film_base_flag": "--film-base 0.16311894,0.080109864,0.037720304",
  "calibration": {
    "film_base": { "explicit": [0.16311894, 0.080109864, 0.037720304] }
  }
}
```

Copy `film_base_flag` straight onto a command line, or take the whole
`calibration` object as a recipe — `jq '{calibration}'` writes one directly. That
is the intended handoff — no manual transcription of floats.

Add `--strict` when scripting: it turns "plausible-looking but bad" into a hard
failure instead of a value your pipeline silently bakes in.

> **No `Dmax` step.** Earlier builds measured a roll reference density off the
> light-struck leader (`estimate --d-max-region`); it retired with the placements that
> read it, and the flag now exits 2. The anchor is placed from the film base (§6). The
> reference build still documents the old step (`git show origin/reserve:docs/using-nc.md`).

### Step 3 — Write the recipe

`roll` is configured **only** by recipe — it has no `--film-base` — so a roll needs
a recipe file. `estimate`'s `calibration` object *is* the measured half; add your
parameter choices beside it:

```jsonc
{
  "calibration": { "film_base": { "explicit": [0.163, 0.080, 0.0377] } },
  "reconstruction": { "curve": { "type": "exponential", "gamma": 2.0,
                                 "anchor": { "mid-at-base-offset": 0.62 } },
                      "density": { "scale": [1.0, 0.84, 0.73] } }
}
```

**The two halves have different lifetimes, and the schema keeps them apart.**
`calibration` is what you measured off *this* roll; everything else is the look,
which you reuse across rolls. So a roll calibration is a recipe with nothing but
`calibration`, and a look is a recipe with no `calibration` at all — the second
needs a base from a flag, since `calibration.film_base` has no default.

Omitted sections take their defaults, so a recipe only needs to carry what you
decided. `hanten params` prints the full default document if you want a scaffold to
edit.

> **`--dump-params` does not freeze your measurements.** It writes the resolved
> *config*, which for a measured value is the **mode**, not the number — a run
> with `--auto-base --auto-wb percentile` dumps `"auto"` and `"percentile"`, and
> the report's measured values are nowhere in it. Two different scans with the
> same flags produce identical dumps. So a recipe dumped from an auto run
> **re-measures on every frame of the roll**, which is exactly what `roll` exists
> to prevent. Only explicit values freeze. (The automatic `<output>.json` sidecar
> has the same content, and reloads through `--params` unchanged.)

### Step 4 — Apply to the whole roll

```sh
hanten roll frames/*.tif --out-dir positives/ --params roll-recipe.json
```

Every frame gets the identical film base and print controls, so the roll
is color-consistent. Outputs are named `<input-stem>_positive.<ext>`, the suffix
coming from the resolved preset; a roll-level
JSON report lands on stdout.

---

## 5. Recipes

### Shape

The recipe *is* the resolved config. Get the current default with:

```sh
hanten params
```

```json
{
  "reconstruction": {
    "schema_version": 1,
    "density": {
      "scale": [1.0, 0.84, 0.73],
      "offset": [0.0, 0.0, 0.0]
    },
    "curve": { "type": "exponential", "gamma": 2.0,
               "anchor": { "mid-at-base-offset": 0.62 } }
  },
  "input":       { "transfer": "auto", "meaning": "auto",
                   "film_type": "unknown", "export_ir": null },
  "calibration": { "film_base": null },
  "measure":     { "inset": 0.05 },
  "print":     { "print_exposure": 0.0, "black_point": 0.0,
                 "white_balance": { "explicit": [1.0, 1.0, 1.0] },
                 "linear_range": [0.0, 1.0] },
  "fit_range": { "headroom_stops": 6.0 },
  "output":    { "preset": "gain-map-hdr" }
}
```

`calibration.film_base` prints as `null` because it has **no default** — this
document is a template to edit, not a runnable recipe. `convert` and `roll` reject
an unstated base.

### Partial recipes are fine

Omit any section and serde defaults fill the gap. This minimal recipe produces a
**byte-identical** result to `--film-base 0.163,0.080,0.0377`:

```json
{
  "calibration": { "film_base": { "explicit": [0.163, 0.080, 0.0377] } }
}
```

Naming a curve by `"type"` alone also resolves to the default, but warns that its
`gamma` and `anchor` were left to this build — both defaults have moved before.

> **Gotcha:** the tagged objects need their tag. If you include
> `reconstruction.curve` at all, you **must** include its `"type"` — omitting it
> fails with `reconstruction.curve is missing 'type'`. Omitting the whole `curve`
> object is fine.

The `curve` object above is the default **exponential** shape. The `characteristic`
curve carries only its stock:

```json
{ "type": "characteristic", "stock": "portra-400" }
```

A recipe from an earlier build still carries `"type": "density"` inside
`reconstruction`; that old value is accepted and dropped. `"type": "simple"` and a
`"sigmoid"` curve are refused with a migration error — including every sidecar and
`--dump-params` written while the sigmoid was the default, because replaying one on
another curve would render a different picture. Render those with the reference build.

The roll reference density `calibration.dmax` is retired. A recipe from an earlier
build carries it at its old default, `"fixed"`, which is **dropped on load** so the
recipe replays unchanged; any other value (`"auto"`, `"none"`, `{"explicit": <d>}`)
asked for a reference this build no longer reads, and is refused (the reference-density and retired anchor flags are removed on both chains,
§6):

```
usage: recipe roll.json: `calibration.dmax` ({"explicit":1.5}) was removed: the roll
       reference density retired with the placements that read it, and the anchor is
       now placed from the film base (`reconstruction.curve.anchor =
       {"mid-at-base-offset": <d>}`). Remove the key — its old default `"fixed"` is
       still accepted, so a sidecar written before the retirement replays.
```

An older spelling, `reconstruction.curve.dmax`, is refused the same way.

The regional balance is retired too (§6). Every earlier sidecar carries
`reconstruction.density.shadow_balance` / `highlight_balance` at `[0, 0, 0]` and
`balance_range` at `"auto"`; those are **dropped on load** and replay unchanged. Any
other value is refused, naming the key. One case has an exact replacement: an equal
shadow and highlight value was a tone-independent offset, and the message says to set
`reconstruction.density.offset` to the offset the run resolves plus the pair:

```
usage: recipe eq.json: recipe key `reconstruction.density.shadow_balance`
       ([0.05,0,-0.02]) was removed with the regional balance: … This recipe's equal
       pair [0.05, 0.0, -0.02] was a tone-independent offset: remove the balance keys
       and set `reconstruction.density.offset` to the offset this run resolves plus the
       pair (the shared recipe's, for a roll per-frame override), which replays the
       render (exactly over a zero offset, otherwise to float rounding). …
```

A differing pair has no counterpart on this chain; render it with the reference build.
An explicit `balance_range` beside equal (or absent) balances is refused too, but the
range was consulted only when the two balances differed, so its remedy is to remove the
key — the render is unchanged:

```
usage: recipe range.json: recipe key `reconstruction.density.balance_range`
       ({"explicit":[0.2,1.6]}) was removed with the regional balance: … Remove the
       key; the render is unchanged — the range was consulted only when the two
       balances differed. …
```

### Strictness

Every recipe struct uses `deny_unknown_fields`, so a typo is rejected rather than
ignored:

```
usage: invalid recipe t.json: unknown field `exposure`,
       expected one of `print_exposure`, `black_point`, `white_balance`,
       `linear_range`
```

This means a **misplaced** key fails too — a key must live under the stage section
that owns it (`--export-ir` ⇒ `input.export_ir`, not top level).

Removed legacy forms produce a **migration error** explaining the replacement, never
a silent alias. Those are a top-level `algorithm` or sibling
`density`/`sigmoid`/`simple` sections, the retired `simple` reconstruction and
`sigmoid` curve, a retired anchor placement, and the two paths the roll measurements
moved from — a top-level `film_base` section, and `reconstruction.curve.dmax`:

```
usage: recipe roll.json: top-level `film_base` is no longer supported — the roll's
       measured values moved into their own `calibration` section, and the `source`
       wrapper went with them. Replace `"film_base": {"source": {"explicit": [r, g, b]}}`
       with `"calibration": {"film_base": {"explicit": [r, g, b]}}` …
```

### Precedence

**Flags always win over the recipe.** Precedence is by *source*, not value — an
explicit `--white-balance 1,1,1` over a recipe's `auto` mode means neutral gains,
not re-estimation. With a [`--preset`](#-preset--pick-a-look-by-name) the full chain is
`defaults < --params recipe < --preset < flags`.

```sh
hanten convert scan.tif -o out.jpg --params roll-recipe.json --print-exposure 0.5
#                                                          ^ overrides the recipe
```

### Sidecars: every conversion is reproducible

**Every** written image gets a `<output>.json` sidecar automatically — no flag
needed:

```json
{
  "meta":   { "nc_version": "0.1.0", "git_commit": "e4a56bb2540d",
              "pipeline_version": 7, "target": "aarch64-apple-darwin",
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
hanten convert scan.tif -o repro.jpg --params out.jpg.json
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
hanten roll --frames frames.json --out-dir positives/ --params roll-recipe.json
```

An explicit manifest `output` goes through the same suffix rule as `convert`: an
extension it states must match the resolved preset's container — `.jpg` under the
default, `.tiff` under `display-p3`/`film-master`, `.avif` under
`hdr-pq`/`hdr-hlg` — and one it omits is completed from that container, so
`"output": "chosen"` writes `chosen.jpg` on a default roll.

Some keys describe the *roll*, not the frame: the whole `calibration` section, the
anchor placement, `curve.stock` and `output.preset`. Overriding one per frame is applied
but warns loudly (and `--strict` turns the warning into a failing exit), because the
frame then renders on a different rule from its siblings — a roll is one piece of film
through one process.

An override that changes `curve.type` **re-resolves the two knobs whose right value is
per-curve**: the anchor placement and the per-channel `density.scale` both take the new
curve's default. The roll's measured `calibration` is untouched by a curve switch, in
either direction — it is a separate section, not a curve knob. Each reset warns if it
discarded a value the recipe had stated; restate it inside the override to keep it.

---

## 6. Reconstruction and curves

### `--preset` — pick a look by name

The three settings below (curve, per-channel gain, print exposure) only
mean anything **together**: the exposure that lands one brightness runs from 1.59 to
1.91 across the presets, because they place mid-grey differently.
`--preset` names a bundle so you don't have to carry three coupled numbers. It leaves the
display tone alone: a recipe's `fit_range.headroom_stops` survives it.

Each bundle's exposure is solved so that **switching preset changes the look, not the
brightness** — what you compare is then the reconstruction. That
calibration is done on `portra-400`; on another stock the generic profile drifts with
how closely it models that film. Presets are not meant to render
alike, so a brightness difference between two of them on your film is part of what they
offer, not something to correct.

| `--preset` | Reconstruction | Display tone | Needs |
|---|---|---|---|
| `characteristic-generic` | `characteristic`, the averaged generic C-41 profile | `reinhard` | — |
| `characteristic-stock` | `characteristic`, the roll's own published response | `reinhard` | `--film-stock` |
| `characteristic-aim` | `characteristic-stock` + the aim-matched red density scale | `reinhard` | `--film-stock` |

The `sigmoid-knees` and `sigmoid-flat` presets retired with the sigmoid curve and are
refused by name; the reference build still renders them.

```sh
hanten convert scan.tif -o out.jpg --film-base 0.9,0.55,0.42 --preset characteristic-generic
hanten convert scan.tif -o out.jpg --film-base 0.9,0.55,0.42 --preset characteristic-stock --film-stock portra-400
```

All three render scene mid-grey at the same brightness, so what you are comparing
between them is the reconstruction and the tone, not "one is brighter".

**A preset is a set of starting values, and individual flags still win over it.** The
precedence chain is `defaults < --params recipe < --preset < flags` — the preset sits
*above* the recipe, because `hanten params` writes every key explicitly and a preset
underneath one would have nothing left to set. The report separates the two directions:

```json
"conversion_preset": {
  "name": "characteristic-generic",
  "replaced":   ["reconstruction.curve"],      // the preset won over the recipe
  "overridden": ["print.print_exposure"]       // a flag won over the preset
}
```

**A preset never touches the roll's measured `calibration`.** A preset names a *look*,
and writes no `calibration` key at all, so a measured film base survives it
untouched and never appear in `replaced`. Everything in `curve` — slope, anchor,
stock — is the look, and the preset does replace it.

**It is a command-line shorthand, not a recipe key.** `--dump-params` writes the
*expanded* values, so a recipe replays identically on any build — including one whose
preset definitions have since moved. A recipe that names a preset is rejected as an
unknown field, and `hanten roll` takes the dumped recipe rather than a preset name:

```sh
hanten convert scan.tif -o out.jpg --film-base 0.9,0.55,0.42 \
   --preset characteristic-stock --film-stock ektar-100 --dump-params roll.json
hanten roll frames/ --out-dir out/ --params roll.json
```

Four combinations are refused rather than quietly doing something else:

- **`--film-stock` beside `characteristic-generic`**, which has no stock — otherwise it
  would silently render a different bundle at the wrong exposure. Use
  `characteristic-stock`.
- **`characteristic-stock` / `-aim` with `--film-stock generic-c41`** — that profile is
  an average of nine sheets, not one film's response. Use `characteristic-generic`.
- **`characteristic-aim` with `portra-800` or `ultramax-800`** — their datasheets
  tabulate an aim delta their own curves contradict, so there is no correction to
  derive. Use `characteristic-stock` for those.
- **A preset with `--output-preset film-master`** — see below.

A preset never *sets* `--output-preset`, but the two are not freely combinable: a
conversion preset is a reconstruction **and display** bundle, so it needs an output preset
that renders a display image. `display-p3`, `compatibility`, `gain-map-hdr`,
`ultra-hdr-v1`, `hdr-pq`, `hdr-hlg`, `hdr-linear-tiff`, `hdr-pq-tiff` and `hdr-hlg-tiff`
all work. `film-master`, which runs no display stage, refuses any `--preset` with a single
message. For a film master, set the reconstruction knobs
directly instead:

```sh
hanten convert scan.tif -o master.tif --film-base 0.9,0.55,0.42 \
   --output-preset film-master --density-curve characteristic --film-stock ektar-100
```

Reconstruction is density-domain inversion (Cineon / negadoctor lineage), with two
curves selected by `--density-curve`:

| Curve | Knobs | Defaults |
|---|---|---|
| `exponential` *(default)* | `--density-gamma` — the straight line's slope, plus the anchor flags below | `gamma 2.0`, `anchor {"mid-at-base-offset": 0.62}` |
| `characteristic` | `--film-stock` — and nothing else | `generic-c41` |

The default is the fixed decode's configuration: the same render `--new-flow` decodes
with (§11). Each curve takes **different recipe keys**, and mixing them is rejected —
*"`gamma` is a parametric-curve key, but the curve type is "characteristic", which
reads its slope and its mid-grey placement off the stock's published curve. Its only
key is `stock`"*.

`simple` reconstruction and the `sigmoid` curve retired. `--reconstruction`,
`--density-curve sigmoid` and the `--sigmoid-*` flags are refused, each naming what
replaces it:

```
usage: --sigmoid-toe was removed with the sigmoid curve: the exponential has no knees,
       and highlight roll-off belongs to the display tone (`--display-tone-headroom`).
```

### `characteristic` — invert the film's own published curve

The exponential *models* the film with a slope and an anchor. This one
**reads** it: each dye layer's measured density-to-log-exposure relation, digitized from
the manufacturer's characteristic curve, inverted per channel. Mid-grey lands at 0.18 by
construction, so there is nothing to anchor and no contrast to pick.

```sh
hanten convert scan.tif -o out.tif --output-preset display-p3 \
  --film-base 0.5,0.25,0.15 --density-curve characteristic --film-stock portra-400
```

Ten stocks ship, all digitized from Kodak publications kept in
[`docs/datasheets/`](datasheets/): `generic-c41` *(the default)*, `ektar-100`,
`portra-160`, `portra-160vc`, `portra-400`, `portra-400vc`, `portra-800`, `gold-200`,
`ultramax-400`, `ultramax-800`. Naming a stock is a **refinement, never a requirement** —
omit it and you get `generic-c41`, the average of the nine measured stocks, which renders
correctly on any C-41 film. A stock that is named but unknown is a loud error listing the
accepted spellings, because a silent fallback would hide a typo behind a plausible render.

The report says which publication the numbers came from, so a datasheet value is
checkable:

```json
"curve": {
  "type": "characteristic",
  "out_of_table": { "below": [0.00012215113, 8.5596905e-05, 1.6293963e-05],
                    "above": [0.058473945, 0.051922057, 0.0] },
  "stock": { "name": "portra-400", "publication": "E-4050", "revision": "2025-01",
             "aims": [0.82, 1.18], "d_min": [0.2192, 0.646, 0.8665] },
  "anchor": null, "anchor_value": null
}
```

`anchor` is `null` on purpose: this curve follows no placement rule, and reporting one
would name a knob the render never read.
`aims` are the sheet's published *Judging Negative Exposures* densities, `[grey card,
paper white]` (Status M, red channel) — the most directly checkable numbers on it if you
own a densitometer. `out_of_table` is the fraction of the frame that fell past either end
of the published curve, per channel; see the extrapolation note at the end of this
section. The `d_min` is **diagnostic only** — your measured `--film-base` is what the
render divides by. Its usefulness is as a check: the *differences* between those three
numbers are the stock's orange-mask signature, so comparing them against your own base's
differences tells you whether the stock you declared is the film you scanned.

Why it exists: every C-41 stock measured has a blue layer 12–19 % steeper than its red
one, so one contrast applied to all three channels leaves a colour cast that **grows with
density** — measured at +1.26 stops per unit corrected density across 21 real frames,
against +1.29 predicted by the datasheets. Inverting each channel's own curve removes it
(residual +0.09). It also inverts the film's toe rather than adding a second one.

**Known issue — a residual green cast.** The blue cast a single scalar contrast leaves is
removed (measured residual +0.09 stops per unit density, against +1.26 before), but a green
one remains, and its size depends on the stock: +0.08 for `gold-200`, +0.18 `portra-400`,
+0.48 `portra-160`, +1.00 `ektar-100`. On the badly-affected stocks `--film-stock
generic-c41` currently looks *better* than naming the stock, because averaging nine curves
dilutes any one sheet's error. The cause is most likely a missing cross-channel term (ACES
applies a 3×3 before its curves; Hanten does not yet) and it is tracked by
`io/scanner-density-calibration`. Ektar's own sheet also disagrees with itself by 11 %
between its aim table and its curve, which is a second, smaller factor for that stock.

**It needs the display tone.** Like the exponential, this curve does not bound itself at
the render's ceiling — it hands the display scene-referred exposure, and measured picture
content reaches p99.99 **+3.64 stops** over diffuse white. So `--display-tone-headroom 0`
(the identity) in practice *refuses* a render of ordinary picture content: the per-pixel
range check rejects the frame (verified: "pixel 13 sits above reference white"), while any
real headroom renders. It is not a rejected *combination* — nothing validates the pair —
so content dark enough to stay inside the ceiling still renders (`--print-exposure=-3` on
the test fixture exits 0). See §7.

Two further limits. The published curves are for *typical* processing, not your
roll, so a heavily pushed or badly stored film will not match. And densities outside the
published range are **extrapolated** along its end slope, not read off it; `convert`'s
report gives the per-channel fractions in `reconstruction_result.curve.out_of_table`. A
`roll` frame entry carries no `reconstruction_result` block, so on a roll only the
above-20 % warning surfaces — measure a representative frame with `convert` if you want
the numbers.

Expect a few per cent there on a full-frame scan and ignore it: the holder and rebate
around the picture are denser than any exposed frame, so they sit past the end of every
curve. Measured across twelve frames, that border is 5–7 % of the frame and **none of it is
inside the picture area**. The figure is a poor check on the stock you declared, too —
rendering one frame under every profile moved it only between 5.75 % and 6.55 %. Only a
much larger fraction (the warning fires above 20 %) means the image itself is being
extrapolated.

### Anchoring — where the curve pins a tone

The exponential pins **mid-grey** a stated density `D` above the film base and lets
white fall where the slope puts it. Pinning mid rather than white is the substantive
outcome of [`reports/sigmoid-reference-baseline.md`](reports/sigmoid-reference-baseline.md):
raising contrast pivots the line *about the pinned point*, so pinning white necessarily
drags everything below it down.

| Anchor | Flag | Recipe (`reconstruction.curve.anchor`) |
|---|---|---|
| Mid-grey at density D **above the base** *(default, D = 0.62)* | `--anchor-mid-offset D` | `{"mid-at-base-offset": 0.62}` |

A larger `D` renders the roll **darker**; a smaller one renders **brighter**.

**The rule reads the film base and nothing else.** Earlier builds could place the
anchor against a roll *reference* density measured from a light-struck leader — film
saturation, not a diffuse white, and far more variable between rolls of one stock than
the base. That reference and the three placements built on it or on the base floor
(`--anchor-white-at-reference`, `--anchor-mid-fraction`, `--anchor-black-floor`;
recipe `"white-at-dmax"`, `"mid-at-dmax-fraction"`, `"black-at-base"`) retired in
`nf-retire/dmax-machinery`. Each is refused, and the remedy is to drop it:

```
usage: --anchor-mid-fraction was removed: it pinned mid-grey at a fraction of the
       reference density. The roll reference density and the placements that read it
       are gone. Drop the flag: on the exponential curve the anchor is placed from the
       film base, mid-grey `--anchor-mid-offset D` density above it (default 0.62,
       slope `--density-gamma`); the characteristic curve places mid-grey from the
       stock's published response and takes neither.
```

The reference build keeps them, if you need to reproduce an old render.

**Switching to the characteristic curve drops the placement**, since that curve reads
its placement off the film. The roll's `calibration` is untouched — it is a separate
section. If your recipe pinned a non-default placement, that is a loud,
`--strict`-promotable warning naming what was dropped.

> **Provisional values.** `D = 0.62` is the generic C-41 profile's mid-grey aim above
> the base, rounded and frozen. The anchor stays referenced to the base: a roll's own
> white is planned to act through a per-roll contrast measured by `measure-roll`
> (`nf-calibration/roll-white-rule`), not by moving `D`. The per-channel density gain beside it
> (`--density-scale`) has moved twice (`pipeline_version` 4 and 5 — see below). Expect
> further movement, with a `pipeline_version` bump when it happens.

### Density correction (before the curve)

| Flag | Effect |
|---|---|
| `--density-scale R,G,B` | Per-channel density gain — **default `1,0.84,0.73`**, see below |
| `--density-offset R,G,B` | Per-channel density offset — **orange-mask compensation** |

**`--density-scale` does not default to `1,1,1`.** It is `1,0.84,0.73` — a
calibration, not an identity. Green and blue density rise faster than red in a scan,
so with no gain they drift against it across the tone scale, which shows up as a
tone-dependent cast rather than an overall one.

The values come from **31 hand-marked neutral patches** across five rolls: per patch,
the gain that renders it neutral; per roll, the median; the default is the mean of the
five roll medians (green `0.837`, blue `0.733`). Rolls are weighted equally on purpose
— two thirds of the patches come from one scan date, and weighting by patch would let
that date set the default on its own. It replaced `1,0.90,0.86` at `pipeline_version` 5.

Two things to know before relying on it:

- **The default is per-curve, and you do not have to manage it.** The `exponential`
  applies one scalar contrast to every channel, so it has no per-channel film model of
  its own and takes `1,0.84,0.73`. The `characteristic` curve carries each
  stock's published per-channel structure already, so the same gain would correct it twice
  — on ten reference frames that moves the channel means *away* from neutral
  (`|G/R − 1| + |B/R − 1|` rises from 0.04 to 0.19) — and it therefore defaults to
  `1,1,1`. Selecting a curve re-resolves the gain unless you state one: `--density-scale`
  always wins, and switching away from a gain you had stated warns rather than dropping it
  in silence.
- **It balances five rolls; it does not fit yours.** Blue is the steady half — every
  roll measured wants 0.68–0.78. Green is not: it splits by **scan date** (one group
  0.86–0.90, another ~0.77, which tracks a change of developer rather than of film), so
  the shipped `0.84` is a compromise that fits neither group exactly and can push a roll
  from the higher group slightly green-yellow. The value is also calibrated on one
  scanner. A roll that still shows a cast wants its own `--density-scale`;
  `io/scanner-density-calibration` is the task that should remove the need to guess.

### The regional balance — retired

`--shadow-balance`, `--highlight-balance`, `--balance-range` and `--auto-balance-range`
exit 2 on both chains, at every value (`0,0,0` included), and the report no longer has
a `balance_range` field:

```
usage: --shadow-balance was removed with the regional balance: per-channel density
       offsets ramped between a shadow and a highlight density, whose `auto` range was
       measured on every frame (…), and which nothing bounded, so a large enough
       difference between its ends folded two densities onto one. Its successor is the
       look's per-channel grade, `--channel-grade R,B` (recipe `look.channel_grade`)
       under `--new-flow`: pivoted at a fixed mid-grey, so it measures nothing, and
       bounded so it stays monotone. … the current chain has no counterpart. Drop the
       flag. (…)
```

The grade is not a rename: it acts on the working space's channels after the 3×3, not
on film density before it, so old balance values do not translate — match it by eye.
The current chain has no tone-dependent per-channel control until the new chain
becomes the default. A recipe's balance keys are covered in §5.

### The `Dmax` reference density — retired

`--d-max`, `--fixed-d-max`, `--auto-d-max`, `--no-d-max` and `estimate --d-max-region`
exit 2 with the anchor section's migration message, on both chains. A recipe's `calibration.dmax`
is dropped at its old default `"fixed"` and refused otherwise (§5).

### Nothing is silently ignored

Cross-curve flags are **usage errors**:

```sh
hanten convert … --density-curve characteristic --density-gamma 1.8
# usage: --density-gamma (1.8) sets a curve slope, but the resolved curve is
#        characteristic — its slope is the film's own, read off the stock's published
#        response. Pass --density-curve exponential to set a slope by hand

hanten convert … --film-stock ektar-100
# usage: --film-stock ektar-100 selects a published film response, but the resolved
#        curve is exponential — a stock has nothing to configure there.
#        Pass --density-curve characteristic
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
| `--display-tone-headroom STOPS` | The display tone's headroom above reference white (default `6` = a white point of 64; `0` is the identity) — see below. **Display presets only** |
| `--linear-range LOW,HIGH` | Affine black/white placement, applied last — **display presets only**, which includes the default (see §8) |

(Under `--new-flow` exposure is spelled `--exposure` and white balance is scene
correction's; `--display-tone-headroom` means the same on both chains — §11.)

`--white-balance` and `--auto-wb` are the two faces of one setting and are mutually
exclusive. The report tells you what was actually used:

```sh
hanten convert scan.tif -o out.jpg --film-base … --auto-wb percentile \
  | jq '.white_balance'
# [1.2501934, 1.0, 0.6589681]
```

Feed that back as an explicit `--white-balance` to freeze it across a roll.

### The display tone and `--display-tone-headroom`

Every display preset applies one tone: **extended Reinhard**, compressing the whole curve
against a white point `W = 2^stops`, where `--display-tone-headroom` (recipe
`fit_range.headroom_stops`, default `6`) is how many stops above reference white content
may sit and still be told apart. It holds scene mid-grey (`0.18`) where it is at every
headroom, so raising the headroom changes the highlights without darkening the midtones.

```sh
hanten convert scan.tif -o out.tiff --output-preset display-p3 \
  --film-base … --display-tone-headroom 4
```

The report says what ran, in a block every preset emits —
`.output_render.display_tone` is `{"operator": "extended-reinhard-mid-preserving-v2",
"headroom_stops": 6.0}` on a display preset — `"operator": "identity"` at zero headroom,
where no pixel moved, matching the new chain — and absent on `film-master`, which has no
display tone stage. On the HDR presets the per-preset block also states what the renderer
applied, next to the luminance anchors no container can carry — `.avif.rendering` for
`hdr-pq`/`hdr-hlg`, `.hdr_coded_tiff` and `.hdr_linear_tiff` for the TIFF pair:

```console
$ hanten convert scan.tif -o out.avif --output-preset hdr-pq --film-base … \
    --report json | jq .avif.rendering
{
  "reference_white_nits": 203.0,
  "target_peak_nits": 1000.0,
  "linear_headroom": 4.9261084,
  "tone_curve": "extended-reinhard-mid-preserving-v2",
  "gamut_mapping": "bt2020-neutral-axis-radial-boundary-v1",
  "linear_domain": "bt2020-linear-relative-to-203-nit-reference-white"
}
```

What to know:

- **On SDR it is not bounded, by design — so the headroom has to be sized to the
  reconstruction.** Content above the white point still exceeds display white; that loss
  is *counted* at the encode step and reported in `.loss` rather than refused. Counted is
  not free: the loss raises a warning, and under `--strict` that warning is a **failure**
  (exit 1). Read `.loss.clipped_high` against `.loss.total_samples` on a representative
  frame and raise `--display-tone-headroom` until the fraction is what you intend; the
  default `6` was sized against the retired sigmoid, not against your curve. **This is a
  change for `--strict` at defaults since `pipeline_version` 7:** the retired `shoulder`
  plateaued at display white and never clipped, so a frame with speculars far beyond the
  headroom that passed `--strict` before can now exit 1. Neither test fixture clips at
  defaults.
- **The HDR presets apply a different shape, not the same curve at a bigger ceiling.**
  They lift highlights toward the 1000-nit peak over an *asymptotic* base, which keeps the
  result **strictly inside** that peak, so nothing clips on the way out.
- **`0` stops is the exact identity, and it polices itself.** `W = 1` makes the operator
  `v`, so nothing rolls an overshoot off — and anything above the render's own ceiling
  **fails** naming the pixel rather than clipping quietly. The ceilings differ: the SDR
  presets stop at reference white, the HDR ones at the 1000-nit mastering peak (≈4.93x
  reference white), so the same overshoot is refused on `display-p3` and renders cleanly
  on `hdr-pq-tiff` (verified at `--print-exposure=-0.2` on the test fixture). No shipped
  reconstruction is bounded at white, so on SDR you pull the frame under reference white
  yourself:

  ```
  error: SDR display rendering ran at zero display-tone headroom (the identity), but
  pixel 16 sits above reference white (luminance 1.0626428), which the identity has no
  curve to roll off. … So: lower the print exposure (--print-exposure /
  print.print_exposure) until the frame fits, or raise the headroom above 0
  (--display-tone-headroom / fit_range.headroom_stops) to roll the highlights off.
  ```

  **The check is late.** The shared print controls run *before* the display render, so
  white balance, `--auto-wb` and `--linear-range` can lift samples past reference white
  too, and `hanten` renders the whole frame before it exits 1 (writing no file). Prove
  zero headroom out on a small frame first.
- **Mid-grey is preserved; diffuse white still costs about 0.86 stop.** At the default
  6 stops reference white `1.0` renders `0.550`, and raising the headroom does not recover
  it (0.858 stop at `W = 16`, 0.864 at `W = 64`) — the compression is what buys the
  headroom. It relaxes only at the very bottom of the range: 0.54 stop at 1 stop of
  headroom and **0.00 at `W = 1`**. Below mid-grey the curve *lifts* slightly (0.09
  renders 0.099).
- **The gain-map presets ratio against the base as stored.** A gain map stores the ratio
  between the HDR rendition and the SDR base, and the encode clamps the base at white.
  Because the SDR half runs past white, ratioing against the *rendered* base would store a
  gain short by whatever was clamped — highlights reconstructed dark, or, from one
  extreme sample, a whole map with no precision left — in a file that looks structurally
  perfect. Both the per-channel gains and the legacy luminance map ratio against
  `min(sdr, 1)`.
- **`film-master` applies no display tone**, so a non-default headroom there is a usage
  error, whether it came from a flag or a recipe. The default is accepted, so
  `--display-tone-headroom 6` resets a recipe's value on that preset.

**`--display-tone` and `--highlight-compress` are gone.** The `shoulder` and `none` tones
existed for reconstructions bounded at white, and none ships; with one operator left there
is nothing to select. Both flags, and the recipe key `print.display_tone` at every value,
are refused naming the replacement — `none`'s nearest is `--display-tone-headroom 0`:

```
usage: --display-tone was removed with the `shoulder` and `none` tones (recipe key
`print.display_tone`): the nearest to `none` on a display preset is the identity,
`--display-tone-headroom 0`.
The tone's one parameter is `--display-tone-headroom` (recipe `fit_range.headroom_stops`).
There is no alias.
```

A recipe's `print.display_tone` is refused even at the old default `"shoulder"`, because
replaying it would render differently; `print.highlight_compress` at its old default `0`
is dropped silently, so the rest of an old sidecar still loads.

---

## 8. Output presets

> ### The default gain map carries headroom — since `pipeline_version` 6
>
> Until the sigmoid retired, the HDR rendition peaked at *exactly* the 203-nit
> reference white — the sigmoid rolled its shoulder so diffuse white landed *at*
> reference white — and `gain-map-hdr` decoded at **1.0x**. The default reconstruction
> now has no shoulder, so highlights pass above reference white and the display tone
> decides what to do with them. Measured on `tests/fixtures/hdr-48bit.tif` with
> `--film-base 0.9,0.55,0.42`, `GainMapMax` (log2) is **0.93 (≈1.9x)** at defaults
> (`pipeline_version` 7; it read 1.88 under the retired `shoulder` tone, whose plateau
> held more content at the ceiling but none of it apart). The single-rendition HDR presets likewise reach past
> 203 nits; a frame that does not (a dark one, or a low `--print-exposure`) gets the
> `HDR output carries an SDR-range signal` warning.

`--output-preset` (recipe key `output.preset`) is an **atomic** policy choice: every
preset resolves container, bit depth, and colour profile itself, and no other knob
states them.

Ten names are accepted, **every one resolves a container**, and there is no
planned-but-unaccepted tier left, so an unknown name always means a typo. The
"Suffix" column is what a path may *state*; the **bold** spelling is the one
`hanten` writes when you leave the suffix off:

| Preset | Container | Suffix | Depth | Contents |
|---|---|---|---|---|
| `gain-map-hdr` *(default)* | JPEG | **`.jpg`** / `.jpeg` | u8 base | SDR base + gain map, packaged **dual-dialect**: ISO 21496-1 segments *and* the legacy Ultra HDR v1 XMP/MPF. |
| `ultra-hdr-v1` | JPEG | **`.jpg`** / `.jpeg` | u8 base | The **same pixels** as `gain-map-hdr`, legacy XMP/MPF only — no ISO claim. |
| `film-master` | TIFF | `.tif` / **`.tiff`** | f32 | Unclamped **linear ACEScg**, straight from the NC film RGB v1 mapping. Bypasses every print/display control. |
| `display-p3` | TIFF | `.tif` / **`.tiff`** | u16 | Modern-pipeline SDR render, losslessly stored in **Display P3**. |
| `compatibility` | TIFF | `.tif` / **`.tiff`** | u16 | The same SDR render in **sRGB**, for broad compatibility. |
| `hdr-pq` | AVIF | **`.avif`** | 10-bit | 4:4:4 Rec.2100 **PQ**. |
| `hdr-hlg` | AVIF | **`.avif`** | 10-bit | 4:4:4 Rec.2100 **HLG**. |
| `hdr-linear-tiff` | TIFF | `.tif` / **`.tiff`** | f32 | Display-linear **BT.2020**, no transfer applied — HDR interchange. |
| `hdr-pq-tiff` | TIFF | `.tif` / **`.tiff`** | u16 | The same signal as `hdr-pq`, as losslessly stored Rec.2100 **PQ** codes. |
| `hdr-hlg-tiff` | TIFF | `.tif` / **`.tiff`** | u16 | The same signal as `hdr-hlg`, as losslessly stored Rec.2100 **HLG** codes. |

> **The default writes a JPEG.** `hanten convert scan.tif -o out.tiff` *fails* —
> with no `--output-preset`, `hanten` resolves `gain-map-hdr` and wants `.jpg`. For a
> TIFF, name the preset: `--output-preset display-p3` (or `compatibility`,
> `film-master`, `hdr-linear-tiff`, …).

`gain-map-hdr` and `ultra-hdr-v1` are **one render packaged twice** — identical
pixels, differing only in metadata dialect. Only the dual-dialect default decodes
as HDR on Apple platforms; `ultra-hdr-v1` exists for readers that predate ISO
21496-1.

### You do not have to name the container

Leave the suffix off and `hanten` supplies it from the resolved preset — that is
what the preset is *for*:

```console
$ hanten convert scan.tif -o out --film-base 1,1,1 | jq -r .output
out.jpg

$ hanten convert scan.tif -o out --output-preset display-p3 --film-base 1,1,1 | jq -r .output
out.tiff
```

The report's `output` field always names what was actually written, so a script
never has to map a preset name to a container. The sidecar follows the completed
path too (`out.jpg.json`).

Four rules make this predictable:

- **A suffix you state is never rewritten.** `-o out.jpeg` writes `out.jpeg`, not
  `out.jpg`; case is preserved as typed.
- **A dot-segment is only a suffix if `hanten` recognises the container.**
  `-o out.v2` and `-o roll-1.2` are stems, so they get `out.v2.jpg` and
  `roll-1.2.jpg`. Only `.tif`, `.tiff`, `.jpg`, `.jpeg` and `.avif` are read as a
  container request.
- **A path that names a directory is refused.** There is nothing to append to, so
  `-o positives/` and `-o positives/.` both exit 2 rather than writing
  `positives.jpg` beside the directory. Name the file inside it
  (`-o positives/out`), or use `hanten roll --out-dir positives/` for a whole roll.
  In a `roll` manifest the same applies to an `"output"` of `"."` — drop the
  `output` key instead and the frame takes its derived name inside `--out-dir`.
- **A stated suffix the preset refuses is still an error**, never a silent rename:

```console
$ hanten convert scan.tif -o out.tiff --output-preset hdr-pq --film-base 1,1,1
usage: output preset `hdr-pq` requires an output path ending in .avif — or no suffix at all, which Hanten completes for you

$ hanten convert scan.tif -o out.tiff --film-base 1,1,1
usage: the output path out.tiff does not end in .jpg or .jpeg: with no --output-preset, Hanten writes `gain-map-hdr` (see --help for the other presets, e.g. `display-p3` for a 16-bit TIFF). Hanten never renames a suffix you state — drop it and the path is completed for you
```

With `-v`, `hanten` says on stderr when it completed a path.

### Preset interaction rules

- `film-master` rejects every non-default downstream control — it bypasses them, so
  accepting them would be a lie.
- Every other preset consumes the print controls, `--linear-range` included.
- The two f32 TIFFs are different images: `film-master` (unclamped linear ACEScg,
  no print controls) and `hdr-linear-tiff` (display-linear BT.2020, print controls
  and display rendering applied).

### Retired: `legacy`, `custom` and the depth/profile/container knobs

The `legacy` and `custom` presets — the older TIFF path, which ran the print controls
on film RGB before an output ICC transform — are gone, and with them `--out-depth`,
`--output-profile` and `--bigtiff` (recipe keys `output.depth`,
`output.output_profile`, `output.bigtiff`), which only those two read. Each name is a
usage error (exit 2) that says what replaced it:

```console
$ hanten convert scan.tif -o out.tiff --output-preset legacy --film-base 1,1,1
usage: output preset `legacy` was removed together with the legacy print path — for a 16-bit TIFF use `display-p3` (or `compatibility` for sRGB); for a float TIFF, `film-master` (linear ACEScg before display rendering) or `hdr-linear-tiff` (display-linear BT.2020). The old rendering is reproducible only from the reference build (`scripts/reference-snapshot/`). There is no alias.
```

A recipe or sidecar written before the retirement still loads: every earlier build
wrote those three keys at their defaults (`"depth": "u16"`, `"output_profile": null`,
`"bigtiff": "auto"`), and a key at that value asked for nothing, so it is dropped. A
non-default value is refused with the replacement and "Remove the key".

BigTIFF is now always decided automatically, and ProPhoto or a user-supplied ICC
profile has no replacement yet. To reproduce an old `legacy` render, build the
reference binary (`scripts/reference-snapshot/README.md`); its own guide is
`git show origin/reserve:docs/using-nc.md`.

### `roll` takes every preset

The old `convert`-only restriction is gone. `roll` derives
`<stem>_positive.<ext>` from each frame's own resolved preset, so a
`gain-map-hdr` roll writes `_positive.jpg` and an `hdr-pq` roll writes
`_positive.avif`. An explicit manifest `output` path goes through the same rule
`convert` uses — checked when it states a suffix, completed when it does not.

---

## 9. Input semantics and the IR channel

`hanten` resolves two **independent** axes from the container's evidence, and you can
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

The IR plane is decoded and **preserved but not acted on** by default, with one
exception that needs nothing from you:

- `--export-ir PATH` writes the decoded plane out. **`convert` only** — `roll`
  rejects `input.export_ir`, because one path cannot serve every frame, so IR
  planes have to be exported frame by frame.
- **IR-assisted film-holder detection** runs by itself when the plane can do the
  job. Hanten measures the interior IR transmission and, if the film reads
  IR-transparent, masks the opaque holder off before the auto rebate search.
  There is nothing to declare — `--film-type` does **not** gate it.

  `--film-type silver|chromogenic|unknown` still exists on `convert`, `estimate`
  and `inspect` (recipe key `input.film_type`), as a **provenance declaration**:
  it records what stock a run was made from, and nothing reads it. Set it if you
  want the film chemistry captured in the recipe or report you keep beside the
  output; leave it out otherwise. Planned IR dust removal will need the same
  declaration, which is why it stays.

  `hanten inspect` and `hanten estimate` report the verdict, and `inspect` adds the
  per-edge mask when it passes:

  ```sh
  hanten inspect scan.tif | jq -c '.ir_separability'
  ```
  ```json
  {"interior_median":0.67963684,"usable":true}
  ```

  ```sh
  hanten inspect scan.tif | jq -c '.holder_mask[0].segments[0]'
  ```
  ```json
  {"span":[0,20],"class":"film","ir":0.6301823}
  ```

  When the film itself is opaque to IR — a fully-exposed silver-halide frame, say
  — holder and film cannot be told apart, so detection falls back to RGB-only and
  says so, naming the measurement. The same happens for an IR page identified by
  shape alone (no `NewSubfileType=4` marker), which is never trusted for
  detection, and for a holder that wraps all four edges: masking it away would
  leave nothing to search, so the RGB-only search runs instead.

  Why measured and not declared: silver blocks IR *in proportion to accumulated
  density*, so an **unexposed** silver frame is IR-transparent against an opaque
  holder (~20:1) while its own **leader** is opaque throughout. Film chemistry
  mispredicts both — and on exactly the unexposed and leader frames of a roll.

IR-based dust removal is not implemented.

### The measurement region (the "effective area")

A region `hanten` resolves on every frame it decodes, so that a measurement reads the
picture rather than the film holder: on an uncropped scan the holder is maximum
density, so a whole-frame statistic measures the holder instead. Today only
`hanten measure-roll` (§11) measures over it; a `convert` resolves and reports it but
reads nothing over it. The area is two cuts, in order:

1. **The film holder**, measured per edge from the IR plane — the same separability
   verdict above. Nothing to configure.
2. **A static inset** of what is left, `--measure-inset FRAC` (recipe key
   `measure.inset`), default `0.05` of the shorter edge.

The inset is **not** a fallback for the first cut: it runs either way. Where the
holder could not be measured, it is simply the only cut.

Where the holder **was** measured, the applied inset is floored at one holder-probe
step — 0.5% of the shorter edge, which is the resolution the holder cut itself has.
The march stops at the start of the first band whose median reads film, and a film
median only means the holder covers less than half that band, so up to half a band
of it can sit inboard of any reported depth (a measured `0` included). The floor
absorbs that band. It binds only near zero — a 3600 px frame insets 180 px against
an 18 px step — and `inset` is the **applied** value, so `--measure-inset 0` on a
measured frame reads back as the step, not as 0:

```sh
hanten inspect --measure-inset 0 scan.tif | jq -c '.effective_area | {region, inset}'
```
```json
{"region":[90,108,5040,3402],"inset":18}
```

Where the holder was *not* measured there is no measurement resolution to respect,
and the fraction you state is exact.

Every command that decodes reports the result — `inspect`, `estimate`, `convert`,
and each frame of a `roll` (under its own `effective_area` key). `convert` and `roll` resolve it on every run, so `--measure-inset` and the
recipe key are never silently ignored:

```sh
hanten inspect scan.tif | jq -c '.effective_area'
```
```json
{"region":[306,270,4662,3078],"holder":{"top":90,"bottom":72,"left":126,"right":36,"capped":{"top":false,"bottom":false,"left":false,"right":false},"converged":true},"holder_applied":true,"inset":180}
```

`region` is `[x, y, w, h]`, in the same convention as `--base-region`.
`holder_applied` is the short answer to "did reading the IR plane change this
rectangle?" — true only when the holder was measured *and* some edge is non-zero.
Read `holder` itself carefully, because the three cases are different answers:

| `holder` | meaning |
|---|---|
| `{"top":90, …}` | measured, and the holder is that deep on each edge |
| `{"top":0,"bottom":0,"left":0,"right":0, …}` | measured, and there is **no** holder — an already-cropped scan |
| `null` | **not measured**: no IR plane, a plane identified by shape alone, or film too IR-opaque to separate |

That last row is the one that should change what you do. The default 5% is sized
for the **rebate**, on the assumption that cut 1 removed the holder first. Where cut
1 did not run, that same 5% has to clear the holder *and* its rebate together — so
on a `null` scan with a visible holder, raise the inset past holder-plus-rebate, not
past the holder alone. (For scale: the IR march measures holder depths of 2.5–4% of
the shorter edge on real scans, which is already most of the default on its own.)
`--measure-inset FRAC` is the flag; here it is on the *measured* frame above, so you
can see the arithmetic (the holder cut is unchanged and the inset goes from 180 px
to 432):

```sh
hanten inspect --measure-inset 0.12 scan.tif | jq -c '.effective_area'
```
```json
{"region":[558,522,4158,2574],"holder":{"top":90,"bottom":72,"left":126,"right":36,"capped":{"top":false,"bottom":false,"left":false,"right":false},"converged":true},"holder_applied":true,"inset":432}
```

Hanten will not guess that number for you — it reports which case the run was in and
leaves the blind cut to you. Values outside `[0, 0.4]` are a usage error (exit 2)
from every command, before the file is read.

Two fields on `holder` say the measurement is not what it looks like. **Both emit a
warning, which `--strict` promotes to a failure** — the fields alone are not the
channel, because a silent field is exactly what let a tenfold over-cut through at
exit 0 while it was being built.

- `capped` — one flag per edge. A capped edge marched as deep as `hanten` looks (25% of
  the shorter edge) without finding film, so its depth is a **floor**, not a
  measurement. The consequence does not stop at that edge: each edge is measured
  over what the *perpendicular* edges' cuts leave, so a truncated depth truncates
  that cut too, and the perpendicular edges then cap as well — at depths that are
  **artifacts of the cap, not floors on their own holder**. A 400×400 frame with a
  120 px top holder and 10 px sides reports `top: 100` (a floor, correctly) and
  `left`/`right` as 100 as well, a tenfold over-cut. So the reading that matters is
  whether a capped edge has a capped *perpendicular* neighbour: with one, treat no
  depth on the frame as measured; without one (a single edge exactly at the cap) the
  other three stand. The warning says which case you are in. No real scan has capped
  — 31 measured IR frames, zero caps, a 6–10× margin.
- `converged: false` — the per-edge march did not settle (the iteration above). Hanten
  then reports the deeper of the last two rounds, which over-cuts rather than leaving
  holder inside the region for a two-round oscillation or a run still settling
  downward; a longer cycle, or one settling upward, could still under-cut. A far
  enough over-cut leaves nothing to measure, which is refused outright on a run that
  measures over the region (exit 2, or a failed frame on a roll) and warned about
  otherwise. No real scan has produced this.

`converged` and `capped` are **not** independent, and `converged: true` is not a
quality verdict on its own: a cap *creates* a stable fixed point, so the worst
answer the march can produce — the tenfold over-cut above — settles and reports
`converged: true`. Read the two together.

Two things this does *not* do:

- **It never crops the image.** Written dimensions, aspect ratio and pixel count are
  exactly as decoded. The effective area changes only which pixels a statistic is
  computed over.
- **It never looks for the rebate.** The inset passes over it blind. Where a
  measurement needs unexposed film, give it a region (`--base-region`) or measure a
  reference frame.

Every command that decodes resolves the area and reports it; a conversion reads
nothing over it (its one consumer, the per-frame auto `Dmax`, retired). So if the two
cuts leave **nothing**, `convert` and `roll` warn rather than refuse, and the report
omits `effective_area` — there is no region to report, and `--measure-inset` has no
effect on that run.

> A scan carrying an IR plane that nothing consumes emits an "IR preserved but
> not used" warning, which **`--strict` promotes to a failure**. One thing in a
> conversion consumes the plane: **film-base holder detection**, when it actually
> masked something — the base source must be `auto`, the plane marker-verified and
> measured usable, *and* the resulting mask must leave some film to search (a holder
> wrapping all four edges falls back to RGB-only). The effective area's holder march
> reads the plane too (`holder_applied: true`), but no rendered pixel depends on it,
> so it does not count.
>
> So a frozen explicit `--film-base` with the default anchor — the recommended roll
> workflow — still warns. Either drop `--strict` for those runs, or use
> `--export-ir` so the plane is consumed.

---

## 10. Reports, warnings, and exit codes

The JSON report on stdout carries the run identity, the effective recipe, the
resolved film base, the white balance actually used, encode loss
statistics, and warnings:

```sh
hanten convert scan.tif -o out.jpg --film-base … | jq '.loss, .warnings'
```

Clipping is reported, never silent:

```json
["output lost 126296 clipped and 0 non-finite of 695772 samples (18.15%)"]
```

Every encoder counts this, not just the TIFF ones — the gain-map JPEG and the
AVIF paths build the same report when they quantize.

The default curve is unbounded, and the display tone overshoots display white only for
content beyond its headroom — on both test fixtures **a default render does not clip**. A
clip warning therefore means something pushed samples past the tone's reach — most often a
**print control** (`--print-exposure 12` clips 100% of a frame) or a
`--display-tone-headroom` too small for the content. Check those before anything else.

`--strict` promotes warnings **that reach the JSON report** to a hard error
(exit 1), after the report is emitted — the right default for scripts and CI.

> One deliberate exception: a failure to write an opted-in **telemetry**
> destination prints `hanten: warning:` on stderr but is kept out of the report set, so
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

The flags in the tables below are **not** conversion knobs: they never appear in a
recipe and can never perturb a pixel. (The transitional `--new-flow` selector at the
end of this section is CLI-only too, but for a different reason — it chooses a whole
rendering chain, so it *does* change the render.)

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

### `--new-flow` — the migration selector (transitional)

Hanten is migrating to the design in [`design-update.md`](design-update.md): a fixed
decode followed by named rendering stages. `--new-flow` (on `convert` and `roll`)
selects that chain. It is **scaffolding with an expiry** — when the new chain
becomes the default the flag is removed, and passing it will be a migration error.

It is CLI-only like the flags above — a recipe naming `new_flow` is rejected as an
unknown field — but **it is not in their "can never perturb a pixel" class**:
choosing a chain is a choice of pixels. That is precisely why it must stay out of
the recipe rather than merely out of the image.

**It renders a minimal picture, not a finished one.** The fixed decode feeds the new
chain. Scene correction applies white balance and exposure, the look desaturates
near-white highlights, fit range compresses the scene's range into the display's (all
below), and fit gamut maps colour outside the destination's gamut onto its boundary, keeping
hue. With no destination flag the result is a Display P3 16-bit TIFF:

```console
$ hanten convert scan.tif -o out --film-base 0.9,0.55,0.42 --new-flow
```

That writes `out.tiff`. Colour outside the gamut is never clipped channel by channel:
fit gamut moves it toward neutral at the same luminance until it fits. The one
exception is a colour whose luminance there is zero or below, which has no in-gamut
rendition and is written black. What can still clip is content brighter
than fit range's headroom, which reaches the encoder above display white as a neutral
(counted, and failed by `--strict`). Whether the picture *looks* right is not what
this flow promises yet.

**The destination is four separate knobs**, not a preset name — `--range`,
`--transfer`, `--gamut`, `--container` (recipe `output.display`), or `--film-master`
instead of all four. Only these combinations are written today:

| `--range` | `--transfer` | `--gamut` | `--container` | writes |
|---|---|---|---|---|
| `sdr` | `native` | `display-p3` / `adobe-rgb` | `tiff` | 16-bit TIFF in the gamut's own curve (sRGB curve / `563/256`) — the default is Display P3 |
| `hdr` | `linear` | `bt2020` | `tiff` | 32-bit float display-linear TIFF (1.0 = 203 cd/m²) |
| `hdr` | `pq` / `hlg` | `bt2020` | `tiff` | Rec.2100 signal as full-range 16-bit TIFF codes |
| `hdr` | `pq` / `hlg` | `bt2020` | `avif` | 10-bit 4:4:4 AVIF |

`--film-master` writes the fixed decode's linear ACEScg as an unclamped 32-bit float TIFF
with **no** rendering stage, so it refuses any stage you ask for — scene correction
(`--exposure`, `--white-balance`), the look, or fit range (`--display-tone-headroom`) —
naming the stage. Each stage's default and its identity are accepted, since neither
asks for anything: `--exposure 0 --white-balance 1,1,1`, the empty look
`--contrast 1 --highlight-desaturation 0`, and `--display-tone-headroom 0` (or its
default 6).
An HDR JPEG with a gain map (`--range hdr --container jpeg`) and an SDR JPEG are
planned, and refused as not written yet.

**Leave an axis unset and it is derived**, in the order range, transfer, gamut,
container: its default when a destination fits, else the one value left, else a
refusal listing the choices. So `--transfer pq` alone is an HDR BT.2020 TIFF and
`--gamut adobe-rgb` alone the Adobe RGB TIFF, while `--gamut bt2020` asks which
transfer. A value you **state** is never overridden — a combination the table lacks is
refused, naming the conflicting pair and a flag that fixes it:

```console
$ hanten convert … --new-flow --range hdr --gamut adobe-rgb
usage: no destination combines --range hdr and --gamut adobe-rgb (recipe keys
`output.display.range`, `.transfer`, `.gamut`, `.container`). Use --range sdr, or
--gamut bt2020 with --transfer linear|pq|hlg
```

The report records every resolved axis in `new_flow.destination`
(`{"display": {"range": "hdr", "transfer": "pq", "gamut": "bt2020", "container": "avif"}}`,
or `"film-master"`), which is exactly the recipe `output` that replays it. An HDR
destination clamps its rendition to the 1000 cd/m² peak and counts what that clamped
in `new_flow.peak_clamp` and in `loss`, where `--strict` sees it; one whose brightest
pixel stays at or below reference white is warned about, naming `--exposure` and
`--range sdr` as the remedies. On `roll`, each frame's derived name takes its
destination's container, and a per-frame `params.output` changes that frame's
destination (a per-frame `output.display` joins the shared recipe's axes, axis by axis)
and raises a roll warning (failed by `--strict`), even when it restates the
roll's.

- **The suffix is judged against that destination**, on `convert` and on a `roll`
  manifest's explicit `output`: a suffix the container accepts is kept as typed, a
  missing one is completed (`.tiff`, `.avif`), and anything else is refused:

  ```console
  $ hanten convert scan.tif -o out.jpg --film-base 0.9,0.55,0.42 --new-flow
  usage: the output path out.jpg does not end in .tif or .tiff: under --new-flow the
  destination is --range sdr --transfer native --gamut display-p3 --container tiff,
  which writes .tif or .tiff. Hanten never renames a suffix you state — drop .jpg and
  the path is completed for you
  ```

  When a destination written today has that container, the refusal offers it too, as
  the flags to add on top of what you stated — restating any axis you (or `--params`)
  stated that it needs changed. So `-o out.avif` adds `…, or state a destination that
  writes it: --transfer pq --container avif; --transfer hlg --container avif`, and with a
  recipe stating `"gamut": "adobe-rgb"` each offer also carries `--gamut bt2020`. With
  a typed `--film-master` the offer says to drop it first; a recipe's `"film-master"` is
  replaced by the offered flags themselves. A `roll` frame's refusal names the
  recipe keys (`output.display.…`) instead of flags.

- **No sidecar is written**, and the report carries no `recipe` echo and no
  `identity.params_hash`: all three were built around the *current* chain's config,
  which does not describe what ran. The new chain has its own recipe (below);
  carrying it in these is `nf-core/report-contract`'s to decide. A sidecar an earlier run left at
  the same path is **removed** — it describes the image just replaced — and the report
  names it in `new_flow.removed_sidecar`. A file there that is not one of Hanten's
  sidecars is left alone, and so is one this run read as its `--params` recipe (with a
  warning, since it still pairs by name with an image it no longer describes).
- **The report is provisional.** The current chain's sections (`reconstruction_result`,
  `output_render`, `white_balance`, …) are absent; a `new_flow` block states
  what ran instead — the decode's resolved `anchor`, `linearization`, `scale` and `offset`,
  each stage with what it `applied` (scene correction's from what it resolved —
  `"identity"`, `"white-balance"`, `"exposure"` or `"white-balance+exposure"`;
  for the look the controls that ran joined by `+` in the order they run —
  `"contrast"`, `"channel-grade"`, `"highlight-desaturation"` — so the default is
  `"contrast+highlight-desaturation"`, or `"identity"` when none ran; fit range's
  operator; `"acescg-to-display-p3-matrix+neutral-axis-radial-boundary-v2"` for fit
  gamut, named for the destination's gamut), scene correction's resolved values in
  `scene_correction`, fit range's in `fit_range` (below), the `destination` (above)
  and `"sidecar_written": false`. The film master runs no stage, so its `stages` is
  empty and those three blocks are absent. The HDR destinations also fill the same
  `hdr_linear_tiff` / `hdr_coded_tiff` / `avif` block the current chain's HDR presets
  do. Its final shape is
  `nf-core/report-contract`'s to decide.

What *is* live is the availability rule: a knob the new chain cannot honour is
refused (exit 2) rather than accepted and ignored, and the message says whether the
counterpart is missing **yet** or for good:

```console
$ hanten convert … --new-flow --output-preset hdr-pq
usage: --output-preset has no meaning under `--new-flow`: the new flow's counterpart
is --range/--transfer/--gamut/--container, or --film-master: … For `hdr-pq`, pass
--transfer pq --container avif.

$ hanten convert … --new-flow --density-curve characteristic
usage: --density-curve has no meaning under `--new-flow`: the new flow has no
counterpart for it, and will not gain one: … Use `--density-curve exponential`, …
```

A knob can be refused by the **flag** you typed, or — in a recipe — by the new
chain's recipe schema, which has no key for it (below).

**The reconstruction knobs are fully classified**, because the fixed decode that
strands them has landed. It is one decode for every negative: a straight line in
density against log exposure, with one reference-free anchor rule. So these are
refused:

| Refused | Why |
|---|---|
| `--density-curve characteristic` | the curve is no longer a choice the decode offers |
| `--film-stock` | per-stock normalization becomes an optional **rendering** step — planned, not scheduled, and it will bring its own flag; `--film-stock` leaves with `--density-curve characteristic` |
| `--preset` | a preset sets knobs on both sides of the decode/rendering boundary |

And these still work, because they *are* the fixed decode's own calibration and
anchor: `--density-scale`, `--density-offset`, `--density-gamma` (the decode's
linearization — see below) and `--anchor-mid-offset`. So does `--density-curve exponential`, which
names what the new flow already decodes with — an identity value asks for nothing this
flow cannot do. (It is *not* spared in order to let one recipe be re-used on either
chain: the new chain's recipe has no curve key, so there is no pinned value for a flag
to clear.) The retired sigmoid, `simple` and regional-balance flags are refused before
any of this, on either chain.

**A recipe for the new chain is its own document.** It states
`"recipe_version": 2` and has one section per stage; `hanten params --new-flow`
prints the defaults:

```console
$ hanten params --new-flow
{
  "recipe_version": 2,
  "input": { … },
  "calibration": { "film_base": null },
  "measure": { "inset": 0.05 },
  "reconstruction": {
    "scale": [1.0, 0.84, 0.73],
    "offset": [0.0, 0.0, 0.0],
    "linearization": 1.8,
    "anchor": { "mid-at-base-offset": 0.62 }
  },
  "scene_correction": {
    "white_balance": { "explicit": [1.0, 1.0, 1.0] },
    "exposure": 0.0
  },
  "look": {
    "contrast": 1.1111112,
    "channel_grade": [1.0, 1.0],
    "highlight_desaturation": { "strength": 0.8, "start_stops": -1.0, "band": [0.015, 0.025] }
  },
  "fit_range": { "headroom_stops": 6.0 },
  "fit_gamut": {},
  "output": { "display": {} }
}
```

(Abridged; the real output is one value per line.) `input` and `measure` are the
current chain's sections unchanged. `calibration` holds the film base only — the
fixed decode reads no reference density. `reconstruction` spells the four decode
knobs above (`--density-gamma` is `linearization` here). `scene_correction` holds white
balance and exposure, `look` contrast, the per-channel grade and highlight desaturation, `fit_range` its
headroom (all below); `fit_gamut` is empty for good — its ceiling comes from fit range and its gamut
from the destination. `output` is the destination (above), with nothing stated
by default — every axis derived. The current chain's `output.preset` is refused in
this document by name. `--dump-params` under `--new-flow` writes this
document with your values resolved, and it reloads under the flag unchanged.

The version is what tells the two chains' recipes apart, and each refuses the other's
by name rather than parsing it and reading nothing:

```console
$ hanten convert … --new-flow --params v1.json    # no "recipe_version"
usage: recipe v1.json: under `--new-flow` a recipe must state `"recipe_version": 2`
— without it the document describes the current chain, whose sections this chain
does not read. `hanten params --new-flow` writes the new layout; or run without
`--new-flow`, where a recipe with no `recipe_version` is read

$ hanten convert … --params v2.json               # "recipe_version": 2, no flag
usage: recipe v2.json: states `recipe_version`, so it describes the new rendering
chain, which only `--new-flow` reads. The current chain's recipe carries no version
```

Any other `recipe_version` reads on neither chain, and says so rather than sending
you to `--new-flow` for a second refusal (`states `recipe_version` 1, which no chain
reads — the current chain's recipe carries no `recipe_version` at all (remove it),
and the new chain (`--new-flow`) reads only 2`).

A versioned recipe that still carries one of the current chain's sections or keys is
refused naming it, and where its knobs went:

```console
$ hanten convert … --new-flow --params old.json   # "reconstruction": {"density": {…}}
usage: recipe old.json: `reconstruction.density` belongs to the current chain's
recipe, not the new one's: `density.scale` and `density.offset` are
`reconstruction.scale` and `reconstruction.offset`; the regional balances are
replaced by the look's per-channel grade, `look.channel_grade`. Drop it — the
current chain reads it only in a recipe with no `recipe_version`

$ hanten convert … --new-flow --params print.json # "print": {…}
usage: recipe print.json: `print` is a section of the current chain's recipe, not
the new one's: white balance and exposure are `scene_correction.white_balance` and
`scene_correction.exposure`; the display tone is fit range, whose one operator is
reinhard and whose headroom is `fit_range.headroom_stops`; the black point splits
between scene correction and fit range (`nf-scene-correction/flare-removal`), and
`linear_range` has no home yet (`nf-scene-correction/levels-knob`) — neither of those
two has a key yet. Drop it — the current chain reads it only in a recipe with no
`recipe_version`
```

The rest of the print and output **flags** are refused as well, each saying where
the knob went:

| Refused | Where it goes |
|---|---|
| `--print-exposure` | renamed: `--exposure` (below) |
| `--black-point` | split in two — flare/fog in scene correction, display black in fit range — which is why it is not a rename |
| `--linear-range` | an affine levels remap needing a stage and a name; retiring it outright is a listed outcome |
| `--output-preset` | the destination flags `--range`, `--transfer`, `--gamut`, `--container` or `--film-master` (above); the refusal names the preset's counterpart |
| `--telemetry`, `--telemetry-file` | the new chain's report and telemetry shape — the record would name the current chain's preset and timing buckets |

Unlike the decode's knees, **no value is spared here** — `--linear-range 0,1`
resolves the documented default and is still refused. An
identity value is normally left alone so a flag can clear what a recipe pinned, and
the new chain's recipe has no `print` section, so there is nothing to clear.

**Scene correction** is the first rendering stage. It applies white balance and
exposure as per-channel gains on linear ACEScg — after the decode's 3×3, before the
look — and clamps nothing:

| Flag | Recipe key | |
|---|---|---|
| `--white-balance R,G,B` | `scene_correction.white_balance` = `{"explicit": [r, g, b]}` | stated gains (default `[1, 1, 1]`) |
| `--exposure EV` | `scene_correction.exposure` | a gain of `2^EV` (default `0`) |

`--exposure` is the new chain's spelling: under `--new-flow`, `--print-exposure` is
refused with `Use --exposure`, and without the flag `--exposure` is refused naming
`--print-exposure`. `--white-balance` keeps its spelling on both chains. The recipe
takes only the tagged form — a bare `[r, g, b]` array, which the current chain still
accepts, is refused. The report states what was applied:

```console
$ hanten convert scan.tif -o out --film-base 0.9,0.55,0.42 --new-flow \
    --white-balance 1.2,1,0.8 --exposure 0.5 | jq -c '.new_flow.scene_correction'
{"white_balance":[1.2,1.0,0.8],"exposure":0.5}
```

**There is no per-frame auto white balance on this chain.** A frame's own statistics
read a sunset as the cast and remove it, so the gains are measured once per roll
with `hanten measure-roll` (below) and stated. `--auto-wb` is refused, and so is a
recipe naming a per-frame mode:

```console
$ hanten convert … --new-flow --auto-wb percentile
usage: --auto-wb has no meaning under `--new-flow`: the new flow has no counterpart
for it, and will not gain one: a per-frame estimate reads a sunset as the cast and
removes it before highlight desaturation can protect it, so white balance is
measured once per roll (…). Use `hanten measure-roll`, then its gains as
`--white-balance` (recipe `scene_correction.white_balance`), or run without
`--new-flow`, where this knob has a meaning. …

$ hanten convert … --new-flow --params wb.json   # "white_balance": "percentile"
usage: recipe wb.json: `scene_correction.white_balance` "percentile" was a per-frame
estimate, and the new chain has none: it read a sunset as the cast and removed it.
Drop it, then state the gains `hanten measure-roll` reports for the roll, as
`{"explicit": [r, g, b]}`
```

**The decode's slope and the picture's contrast are two knobs.** The current chain's
single `gamma` (2.0) bundled the film's **linearization** — undoing the negative's
≈0.55 density per decade, a calibration — with **print contrast**, a look. Under
`--new-flow` they are split: `--density-gamma` sets `reconstruction.linearization`
(default 1.8) and `--contrast` sets `look.contrast` (default 2.0/1.8 ≈ 1.11), so at
the defaults a neutral renders where the single 2.0 did. To change how contrasty a
picture is, change `--contrast`; `--density-gamma` is a calibration and moves with
`--density-scale`, never alone. A recipe still stating `reconstruction.contrast` is
refused, with the value that keeps it:

```console
$ hanten convert … --new-flow --params old.json   # "reconstruction": {"contrast": 2.0}
usage: recipe old.json: `reconstruction.contrast` split in two: the decode's slope is
now `reconstruction.linearization`, the film's linearization, and how contrasty the
picture is is the look's `look.contrast`. Drop the key; to keep a stated 2 as the whole
contrast, write `look.contrast`: 1.1111112 and leave `reconstruction.linearization` at
its default 1.8
```

**The look** is the stage between scene correction and fit range. It runs three
controls, in this order: **contrast**; the **per-channel grade**, which removes (or
adds) a cast that grows away from mid-grey; then **highlight desaturation**, which
pulls bright surfaces that are nearly neutral the rest of the way to neutral, so a
white that still carries a trace of cast after the roll's white balance reads clean.
Contrast and highlight desaturation are **on by default**; the grade is off.

| Flag | Recipe key | |
|---|---|---|
| `--contrast CONTRAST` | `look.contrast` | print contrast, pivoted at mid-grey; default `2.0/1.8`, `1` is the identity, must be positive |
| `--channel-grade R,B` | `look.channel_grade` | red and blue exponents pivoted at mid-grey, green fixed at 1; default `1,1` (off); both positive, with the spread over `R,1,B` under 1 |
| `--highlight-desaturation STRENGTH` | `look.highlight_desaturation.strength` | `0`–`1`; default `0.8`, `0` is off |
| `--highlight-desaturation-start STOPS` | `look.highlight_desaturation.start_stops` | where the pull begins, in stops below diffuse white (default `-1`) |
| `--highlight-desaturation-band S0,S1` | `look.highlight_desaturation.band` | the saturation band (default `0.015,0.025`) |

- **It only touches near-neutral highlights.** Its strength rises from `start_stops`
  up to diffuse white, and falls to nothing across the band: a pixel whose channels
  differ by more than `S1` (measured as `log10(max/min)` over the whole contrast,
  `--density-gamma` × `--contrast`, so the band means the same density spread
  whichever knob carries a roll's contrast) is left alone. So a sunset, sand or skin keeps its colour; a cast white does not.
- **It assumes the roll's white balance.** "Near-neutral" means near R = G = B, which
  is near white only after `measure-roll`'s gains have removed the roll's cast.
- **Contrast pivots at mid-grey**, on each ACEScg channel: mid-grey stays put and each
  stop away from it becomes `CONTRAST` stops. A neutral stays neutral; saturated colour
  shifts slightly against the pre-split single slope, which acted before the NC film
  RGB 3×3 rather than after it. It runs after scene correction, so `--exposure 1` at
  contrast 1.11 moves the picture 1.11 stops: exposure is in stops of the
  reconstructed scene.
- **The grade is for crossover** — a cast that differs between shadows and
  highlights, which one set of white-balance gains cannot remove. Each of red and blue
  becomes `0.18 · (v / 0.18)^R` (or `^B`), and the pixel's luminance is then put back,
  so mid-grey stays neutral, the cast it adds grows with distance from mid in both
  directions, and neutral contrast is untouched — that stays `--contrast`'s. Below 1
  a channel is pulled down in the highlights and up in the shadows; above 1 the
  reverse. It runs after contrast, so the same values act more strongly on a
  contrastier picture. It does not replace the roll's white balance, which should be
  set first, or the decode's calibrated `--density-scale`.
- **Luminance is kept** by the grade and by highlight desaturation; only chroma moves.
  `--highlight-desaturation 0` turns desaturation off — with `--contrast 1` and the
  grade at `1,1` the look is the exact identity, the way to see the roll's raw cast.
- The report says what ran:

  ```console
  $ hanten convert scan.tif -o out --film-base 0.9,0.55,0.42 --new-flow \
      | jq -c '{look: .new_flow.look, stage: .new_flow.stages[1]}'
  {"look":{"contrast":1.1111112,"channel_grade":[1.0,1.0],"highlight_desaturation":{"strength":0.8,"start_stops":-1.0,"band":[0.015,0.025]}},"stage":{"stage":"look","applied":"contrast+highlight-desaturation"}}
  ```

- The flags are new-flow only — the current chain has no look stage — and an
  out-of-range value is refused naming the flag and the key:

  ```console
  $ hanten convert … --new-flow --highlight-desaturation 1.5
  usage: --highlight-desaturation (recipe `look.highlight_desaturation.strength`) must be
  within [0, 1] (0 is off), got 1.5
  ```

**Fit range** fits the scene's range into the display's. It has one operator,
reinhard, applied to luminance so all three channels scale together and hue is kept;
mid-grey stays where the decode put it. Its one knob is how much range above diffuse
white it compresses:

| Flag | Recipe key | |
|---|---|---|
| `--display-tone-headroom STOPS` | `fit_range.headroom_stops` | `0`–`24`, default `6`; `0` is the identity |

The flag and the key are the same on both chains (`--display-tone` and
`--highlight-compress` are removed on both — §7). The display's peak
is the operator's other argument and belongs to the destination, not the recipe — `1`
for SDR, `1000/203 ≈ 4.93` for HDR. The report names what ran:

```console
$ hanten convert scan.tif -o out --film-base 0.9,0.55,0.42 --new-flow \
    | jq '.new_flow.fit_range'
{
  "operator": "reinhard-peak-lifted-v1",
  "headroom_stops": 6.0,
  "white_point": 64.0,
  "display_peak": 1.0
}
```

At `0` the operator reads `"identity"`: fit range passes the scene through unchanged,
and everything above display white clips at the encode (32% of the samples on
`tests/fixtures/hdr-48bit.tif`, against none at the default). A pixel with a
non-finite channel is refused (exit 1, naming the pixel) rather than passed to the
encoder.

#### `measure-roll` — a roll's white balance, measured once

It removes the cast a whole roll shares — the film, the development, the scanner —
and keeps the scene's light: one sunset frame barely moves a statistic taken over
every frame. Give it the roll's picture frames, its leader, and its explicit film base
(from `estimate`, §4):

```console
$ hanten measure-roll frames/*.tif --leader leader.tif --film-base 0.471,0.232,0.108
{
  "command": "measure-roll",
  "leader": { "median": [0.92152184, 0.7227872, 0.46504393], "guard_density": 0.1,
              "ceiling": [0.60884345, 0.4775408, 0.30725148], … },
  "frames": [ { "input": "frames/1774.tif", "region": [167, 167, 4579, 3009],
                "sampled": 131072, "kept": 131065, "guarded": 7, "unusable": 0, … }, … ],
  "white_balance": { "gains": [1.000904, 1.0, 1.2442316], "percentile": 0.99,
                     "pooled": 4542296 },
  "reuse": { "flag": "--white-balance 1.000904,1,1.2442316",
             "recipe": { "scene_correction": { "white_balance":
                         { "explicit": [1.000904, 1.0, 1.2442316] } } } },
  …
}
```

(Abridged; that is 35 frames of one roll.) Each frame is decoded under the new chain's
decode — at its linearization, before the look's contrast — and sampled over its **effective area** (§9); the gains equalize the pooled
pixels' per-channel 99th percentile, green-anchored. Freeze them by pasting `reuse.flag`
on `convert --new-flow`, or by merging `reuse.recipe` into the roll's recipe for
`roll --new-flow`.

- **Pass the leader.** Any pixel within 0.1 density of it is left out (`guarded`), so a
  fully exposed frame mixed into the inputs cannot become the roll's white — measured,
  it would move the gains 0.4–1.3 stops. Without `--leader` the run warns, and
  `--strict` refuses it before decoding anything (exit 2).
- **Only picture frames, each once.** Every input is pooled as picture; leave out the
  unexposed base, the leader and any calibration frame. A frame named twice is
  refused (exit 2) — it would weigh double — and so is the `--leader` file among the
  inputs. A frame that contributes nothing, or whose
  holder cut is not a measurement (the same warning `convert` gives), is warned about
  by name, so `--strict` catches both.
- **The base must be explicit** — `--film-base`, or `calibration.film_base` in a
  `--params` recipe (the new chain's, `"recipe_version": 2`, whose `reconstruction`
  it decodes under; its `scene_correction`, which this measures, and its `look` are
  not read). A base estimated per frame would decode every frame differently, so
  anything else is refused (exit 2) pointing at `estimate --grid`.

What survives untouched is everything before the seam: `--film-base`,
`--base-region`, `--auto-base`, `--measure-inset`, `--input-transfer`,
`--input-meaning` and `--film-type`. Decode, film base and the measurement region
are shared by both chains.

`--export-ir` works too: the IR plane is written from the decoded image at the
destination's depth — 32-bit float beside a float TIFF, 16-bit otherwise.

On `roll` the flag applies to every frame: each is written as
`<stem>_positive.<ext>`, the extension its destination's container's (`.tiff` by
default), with no sidecars and no `recipe` in the roll report. `roll`
takes no conversion flags, so its knobs come from the shared recipe and the
per-frame overrides. The shared recipe must be the new chain's document, and each
override is merged onto it and rendered with it, so an override uses the new
sections too (`{"reconstruction": {"linearization": 1.7}}`, `{"look": {"contrast": 1.3}}`,
`{"scene_correction": {"exposure": -1}}`, `{"fit_range": {"headroom_stops": 4}}`)
and one naming a
current-chain key is refused the same way — naming its frame — at exit 2.

---

## 12. Troubleshooting

**"no film base selected"**
Neither `convert` nor `roll` has a default film base — but they take it from
different places. On **`convert`**, pass `--film-base R,G,B` (measured once per
roll), `--base-region X,Y,W,H`, or `--auto-base`. **`roll` accepts none of those
flags**: set `calibration.film_base` in the shared `--params` recipe instead.
`estimate` still defaults to auto, so `hanten estimate scan.tif` remains the way to
get a value in the first place.

**"auto film-base detection found no uniform unexposed rebate band"**
The scan has no detectable rebate — it's cropped, or the holder covers it. Measure
the base from a reference frame and pass `--film-base`, or point at a known region
with `--base-region`. Content-based estimation is planned but not shipped.

**"base-region … is not uniform (worst per-channel relative spread …)"**
Your rectangle mixes rebate with image content. Check the coordinates against
`hanten inspect`, or use `estimate --grid` on a genuinely unexposed frame.

**Heavy clipping in the report**
The default display tone does not clip ordinary content, so something pushed content
past it: a positive `--print-exposure`, too little `--display-tone-headroom`, or a low
anchor (`--anchor-mid-offset` smaller than the default). The exponential has no
shoulder of its own, so a low anchor is severe. Lower the exposure, raise the headroom,
move the anchor up, or use an f32 output (`hdr-linear-tiff`, `film-master`) for an
unclamped result.

**"reconstruction.curve is missing `type`"**
A partial recipe that includes the `curve` object must include its tag. Add the
type you actually intended — normally `"exponential"`, the default. Adding
`"type": "characteristic"` also makes it parse, but it switches you to the other curve
and changes your pixels.

**`--strict` fails on every frame of an IR scan**
Expected — see §9: an unconsumed IR plane warns, and `--strict` promotes it. The
plane is consumed only when the base source is `auto` *and* the plane is
marker-verified *and* it measures able to separate holder from film, so a frozen
explicit `--film-base` — the recommended roll workflow — still warns. Passing
`--film-type` does not change this; it gates nothing. Either drop `--strict` for
those runs, or use `--export-ir` so the plane is consumed.

**Output differs between two machines**
Determinism is scoped to one build and architecture. Transcendental FP and the
lcms2 colour transform differ by ~1 ULP across platforms. Compare
`hanten --version` output — `pipeline_version`, commit, and target must all match.

---

## 13. Not yet available

So you don't go looking:

| Missing | Owning task |
|---|---|
| **Auto-cascade recipe generation** — a planner that produces a roll recipe for you, instead of you measuring and freezing it by hand | [`core/base-acquisition-planner`](tasks/core/base-acquisition-planner.md) |
| **Content-based film-base fallback** (`--base-content`) for cropped scans with no visible rebate | [`film-base/content-fallback`](tasks/film-base/content-fallback.md) |
| **Named conversion presets** (`--preset`) — selecting a whole reconstruction + display bundle by name instead of assembling the flags. Today each configuration is 3–5 coupled flags whose values only make sense together | [`algo/conversion-presets`](tasks/algo/conversion-presets.md) |
| **IR dust removal** | roadmap follow-up, no task file yet |

[`docs/TASKS.md`](TASKS.md) is the authoritative status for all of it.
