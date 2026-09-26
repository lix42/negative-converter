# Design update

**Status:** agreed direction from a design discussion, 2026-09-16/17. **Not yet
applied** to `design-spec.md`, `TASKS.md`, any task file or CLAUDE.md. Where
this document contradicts them, it describes the intended design and they
describe the current one. Folding it in (spec revision, task changes) is
follow-up work and has not been planned. Git history keeps earlier versions of
this document. Part 1's first version set a different goal ("estimate scene
exposure, per stock"); it was replaced on 2026-09-17 for the reasons recorded
below. The 2026-09-17 revision also folds in two independent reviews (a repo
fact-check and an adversarial read) and the offset test they prompted —
corrections are marked where they overturn an earlier claim rather than quietly
rewritten.

Parts are appended as each discussion settles. **Part 1** covers reconstruction,
**Part 2** rendering and output, **Part 3** how the decode is evaluated and
tuned, and **Part 4** is a placeholder for the same question about rendering.

# Part 1: Reconstruction

## Why this was needed

The split (`algo/reconstruction-render-curve-split`, verdict 2026-09-02) was
justified by its results: better highlights and a working HDR rendition at
matched lightness. It never stated what reconstruction's output **is**. Without
that goal there is no principled way to decide which methods belong in
reconstruction or which knobs belong in rendering, and no target to measure a
reconstruction against. The current contract makes the gap explicit: NC film RGB
v1 is "film-rendering intent, not physical scene recovery". A stage that
promises nothing physical cannot be measured.

## The goal

> **By default nc shows what the negative really holds; it does not optimize
> the photograph. Reconstruction is a fixed, stock-agnostic decode of the
> negative, the way a traditional print sees it. It loses nothing it can avoid
> losing and reports what it could not recover. Every choice about how the
> picture should look, including per-stock normalization, belongs to
> rendering.**

### The model: a traditional colour print

- **C-41 stocks are designed to a common printing-density aim**, so one paper
  prints them all. Papers do come in contrast families and photofinishing
  printers keep per-film-type channels, so "one curve for every negative" is too
  strong — but the per-stock compensation a lab applies is **filtration and
  exposure, a gain**, not a per-channel slope and not a curve inversion. That is
  the invariance the decode copies.
- **The lab adjusts two things per roll or frame:** colour filtration and
  exposure time. Those are rendering's scene correction (white balance,
  exposure).
- **Stock differences reach the print.** Ektar's negative is steeper than Portra
  400's — the registry's own γ column reads Ektar 0.608/0.589/0.656 against
  Portra 400 0.531/0.555/0.633, so ≈14 % in red, about half a stop of extra
  rendered contrast over a four-stop scene. Real but modest: most of Ektar's
  "punch" is dye set and saturation, not negative slope. The paper does not undo
  it, so neither does the decode.
- **The negative's toe is never inverted.** The paper prints the compressed
  shadows as they are.
- **Cinema is the nearest precedent, and it is split** (unverified in detail):
  the classic Cineon → linear is a straight line, but **ADX is not** — it is
  per-channel density → a cross-channel matrix → a per-channel curve inverse,
  with the look in a separate print-film emulation. So the precedent for a
  stock-agnostic decode is a *generic curve plus a matrix*, which is this doc's
  alternative candidate rather than its default one.

### Why not per-stock inversion as the default

Inverting each stock's own datasheet curve (`characteristic`) was the first goal
written here. It was dropped as the **default** because:

- **It normalizes tone character.** A perfect per-stock inversion returns every
  stock to the same scene contrast, erasing the difference a print shows between
  Ektar and Portra. That is optimizing, not showing.
- **It inverts the toe.** The datasheet toe is nearly flat: slope ≈0.02–0.03
  against ≈0.6 mid-scale, so inverting it amplifies noise and film-base error
  **20–35×** where the film recorded almost no information.
- **It needs per-stock data** that cannot exist for every stock, developer and
  scanner. The fixed decode needs none.

Per-stock inversion still has a place: as an **optional normalization in
rendering** ("show this frame as if every stock had the same response"). Because
the fixed decode is invertible, that step can move there without losing anything
(see the key argument).

### What reconstruction does not decide

- **Scene illuminant / white balance** (the lab's filtration).
- **Absolute exposure** (the lab's exposure time). Which value "should" be
  mid-grey needs a light-meter reading the negative does not carry.
- **Look.** Contrast, toe and shoulder shaping, paper or print emulation,
  per-stock normalization.

### Known limits

- **Saturated regions carry no information.** Near the base and on the film
  shoulder the negative recorded little; the decode must stay monotonic, must
  not invent data, and must report the problem.
- **The decode needs a density calibration, and its target is Status M.** A
  print system is balanced in *printing density* — how a particular paper sees
  the dyes — which is not one canonical space and would leave the calibration's
  reference undefined. Status M is the standardized densitometry chosen to track
  printing density for camera negatives, it is what the digitized datasheet
  corpus is in, and it is what `io/scanner-density-calibration`'s
  3×3-plus-offset targets. So: **scanner → Status M**, and the gap from there to
  any paper is small beside it. That calibration is a measurement and stays in
  reconstruction.
- **What the calibration must explain is green, not blue.** Blue's
  exposure-dependent cast is the film's own and the datasheets predict it almost
  exactly; Ektar's green is roughly six times what its sheet predicts. The film
  half is known; the scanner/developer half is green. (Appendix B.)

## Key argument: pick reconstruction by what its output means

Any **invertible** reconstruction can be compensated by some rendering to give
the same final image. For example, a straight-line decode followed by a newly
designed rendering transform can reproduce characteristic + reinhard exactly:

- Both reconstructions are strictly increasing per-channel functions of density,
  so each can be converted into the other.
- The converting transform is generally **not a 1D curve**. The NC film RGB →
  ACEScg 3×3 matrix sits between the stages, and a per-channel nonlinearity
  doesn't commute with a matrix that mixes channels. So it must undo the matrix
  first or be a 3D transform. White balance and exposure would also have to move
  after it.
- This holds only while nothing between the stages loses information (no
  clamping).

Consequence: **the final image cannot justify a choice of reconstruction while
rendering is free to be redesigned** — and neither can information content,
since every invertible reconstruction keeps the same information. (With
rendering *held fixed*, the final image is the only thing that can: that is what
makes a review set evidence. See Part 3.) Reconstruction is chosen for **what
its intermediate image means**, and rendering for the result. The fixed decode
is chosen because it means one thing for every stock: the negative, as a print
sees it.

## The decode's parameters, and which of them are really rendering

```text
D_c   = −log10(scan_c / base_c)            measurement
D′_c  = scale_c · D_c + offset_c           calibration
out_c = 10^(gamma · (D′_c − A))            the curve (A = anchor)
```

**Nothing is lost, by construction.** Every step above is strictly increasing,
unclamped, in f32, so no choice of `scale`, `offset`, `gamma` or the anchor
destroys information — each is undoable downstream. That is why these settings
are conventions and opinions rather than accuracy questions, and why they cannot
be judged by "how much survived". Three exceptions, none of them about those
four: a dead-pixel floor on the scan; non-finite samples passed through as NaN
for the encoder to count; and the **regional balance** (`shadow_balance` /
`highlight_balance`), which was omitted from the chain above and *could* be
non-monotone — its weights varied with the scalar tone, nothing bounded their
magnitude, and a large enough shadow-minus-highlight difference mapped two scene
densities onto one. It retired for that reason and others
(`nf-retire/regional-balance`); rendering's grade replaces it.

Expanding the curve shows what each knob really is:

```text
out_c = 10^(−gamma·A) × 10^(gamma·offset_c) × (10^(D_c))^(gamma · scale_c)
        └─ scalar gain ─┘ └ per-channel gain ┘ └── per-channel exponent ──┘
```

| Knob | Effect | Rendering equivalent |
|---|---|---|
| **anchor `A`** | one gain on all channels | **Exactly exposure** — a scalar commutes with the 3×3. |
| **`offset [3]`** | a per-channel gain, constant at every brightness | **White balance** — but in film-layer space, *before* the 3×3, so it is not the same operator as rendering's white balance after it (≈2.6 % apart on a neutral, more on saturated colour). |
| **`scale [3]`** | a per-channel *exponent*: the rate each channel grows with exposure | The pivoted per-channel grade (Part 2) — same symptom, different basis, not the same correction. |
| **`gamma`** | overall contrast | The contrast knob (Part 2) — the same for neutrals, different for saturated colour. |
| `shadow_balance` / `highlight_balance` (retired) | per-channel offsets by tone region | A grade — the look's `channel_grade`, which replaced them. |

So `scale` and `gamma`'s calibration half are the decode's own. Rendering has a
counterpart for every knob here — exposure, white balance, contrast, and the
pivoted per-channel grade — but a counterpart acts **after** the 3×3, in a
different basis, so it addresses the symptom rather than the error. The anchor is
an exact duplicate and belongs there; `offset` duplicates white balance only
approximately, since it acts before the 3×3, and stays a calibration term (see
Decisions). Tuning either here while "holding rendering fixed" is tuning the
final image and calling it reconstruction.

Note also that `gamma` and `scale` are over-parameterized: only the products
`gamma · scale_c` enter, pinned by the convention `scale_r = 1`. "Measure
`scale` from neutrals, `gamma` from a bracket" is one measurement split by a
convention, not two independent ones.

**The anchor behaves differently here than under the sigmoid.** On the straight
line the anchor factors out as a pure gain: all four placement rules give the
same shape and differ only in brightness. Under the knee'd sigmoid the knees sit
relative to the anchor against a fixed 0–1 range, so moving it changes the
*shape*, which is why the anchor is the lever `sigmoid-knees` has. (Its
*refusal* of `--print-exposure` is a different mechanism: that bundle resolved
the since-retired `display_tone: none`, and a scalar gain after a bounded curve pushes past
reference white, where the renderer's range check rejects the frame.)

### Decisions

**What is decided is the *rule*; the numbers filling it are current picks.** One
anchor rule, `gamma` split into a calibrated half and a look half, `Dmax` out of
the default path — those are the design, and moving one is a design change. The
values in them — `d ≈ 0.62`, the linearization ≈1.8, `density.scale`, and
`offset = [0, 0, 0]` — are today's best estimates and are **expected to move**: by
visual review now (Part 3), and by the bracketed calibration frames later. That
is not free (every default pixel moves, so it costs a `pipeline_version` bump
and a drift-gate row) but it is planned, not a regression. Whether a value
should also stay reachable by the end user is a separate question this doc does
not settle; today all of them are flags and recipe keys.

- **One anchor rule: `mid-at-base-offset(d)`**, with `d` the film's
  mid-above-base density (≈0.62; stocks measure 0.54–0.70). Mid-grey is what
  "exposed correctly" means, it is reference-free (no leader, no roll-to-roll
  error), and pinning mid makes contrast and exposure independent: changing
  `gamma` pivots about mid instead of moving the whole image. Pinning black
  instead would leave midtone brightness depending on contrast, and the base is
  not scene black anyway — it is fog, with real shadows above it.
- **`d` is a calibration, not a brightness knob.** It is a pure gain (at gamma
  2.0, 0.1 density ≈ 0.66 stop, 1 stop ≈ 0.15 density), i.e. the same lever as
  `--print-exposure`. Brightness is set in rendering.
- **`Dmax` leaves the default path.** This rule never reads the reference, so
  `--d-max`, `--auto-d-max` and `estimate --d-max-region` stop mattering for the
  decode, and the four `--anchor-*` flags collapse to one number. The reference and
  the other three placements retired in `nf-retire/dmax-machinery`; the reference
  build keeps them for comparison.
- **`offset` defaults to `[0, 0, 0]` for now — a pick, not a closed question.**
  It is not a duplicate of the measured base: it is the gap between density
  measured from the rebate and the density where the three layers correspond to
  equal exposure, and the datasheets carry that term. A 2026-09-17 review
  rejected **two candidate values** for it, not the term itself (Appendix E);
  the data that could identify one — density varied at a single illuminant —
  does not exist yet.
- **`density.scale` is one global value.** It is the decode's calibration of the
  common per-channel slope error, never varied per stock, roll or frame. What one
  value cannot reach is a rendering correction, not a second decode.
- **`gamma` is two things and splits.** Linearizing the film (≈1/0.55 ≈ 1.8) is
  calibration and stays; print contrast is a look and moves to rendering.
  Today's single 2.0 bundles both — roughly linearization plus ≈1.10× print
  contrast.
- **`d` and the linearization are fixed nominal values, not per-stock ones.**
  Both are per stock in the registry (`d` 0.542–0.699, i.e. **1.06 stops** at
  gamma 2; red film gamma 0.53–0.61), and choosing either per stock would be
  per-stock exposure and contrast normalization inside a decode declared
  stock-agnostic — the thing this design demotes `characteristic` for. Fixed
  values let film speed and stock contrast show through, which is the faithful
  behaviour.

## Methods under this goal

| Method | Role |
|---|---|
| exponential (≡ sigmoid with `toe = shoulder = 0` at the same anchor, bit-exact) | **Default candidate.** A straight line in density against log exposure, with the toe passed through as recorded. |
| `generic-c41` characteristic | **Alternative candidate** for the fixed decode. It is stock-agnostic but inverts an averaged toe. Needs a comparison against the exponential, or a toe-limited form. (The curve retired in `nf-retire/characteristic`; the reference build still renders it.) |
| per-stock `characteristic` | **Leaves reconstruction** and becomes an optional per-stock normalization in rendering. **Left** in `nf-retire/characteristic`; the normalization is planned, not built. |
| sigmoid with toe/shoulder | **Leaves reconstruction:** a decode with a rendering fused on top. Kept for now as the **visual reference** for the migration (see below). Retired from the product later. |
| `simple` | **Remove.** `1 − T/T_base` is an affine inversion, not a decode of anything a print sees. |

### Calibration: the part that is a measurement

- **Film base** (Dmin) per roll.
- **Toward Status M — and what can actually be fitted.** The *target* is a common
  densitometry, so datasheet numbers mean something here; what an achievable
  measurement produces is a fit of the **whole chain** — film × development ×
  scanner — not a scanner profile. A chart shot on film cannot separate them, and
  a transmission step wedge, which would isolate the scanner, is blind to dye
  cross-talk. Separating the two needs the developed negative read on a real
  densitometer. `io/scanner-density-calibration` owns settling which instrument
  and which model it delivers. Target form: a 3×3 + offset; today's crude form is
  the per-channel `density.scale`, re-calibrated on 2026-09-16
  (`pipeline_version` 5) to **`[1, 0.84, 0.73]`** from 31 hand-marked neutral
  patches over five rolls.
- **It depends on development, not only on stock or scanner.** Green splits by
  scan date on one scanner, and the split follows the developer, so no single
  value fits every roll. The decode ships the common-ground value anyway — a
  per-roll default would stop being fixed — and the roll-level remainder is
  rendering's. (Appendix C.)
- **We cannot measure a user's chain, only our own.** Their scanner, developer and
  stock differ, so whatever we fit ships as a **default prior**, not as their
  calibration. Two consequences: the default should be the most transferable value
  we can justify rather than the best fit to our rolls, and a user who wants better
  needs a calibration *procedure* — shoot a target, fit, freeze it into the roll
  recipe — which is a product feature, not a default.
- **Why a per-channel gain cannot be the end state:** interimage effects and DIR
  couplers make each layer's slope depend on the *other* layers' exposure. That
  is not a per-channel quantity at all, which is why the standard model is a
  matrix in density — and it is per stock, so it also breaks "one calibration
  per scanner and developer".
- **The marked patches can anchor a level, not a slope.** Their density range
  comes from *illumination* rather than exposure, so a fitted slope absorbs the
  illuminant. The bracketed calibration frames
  (`analysis/calibration-frame-capture`) vary density at one illuminant, which
  is the whole point of them. (Appendix C.)

### Why dividing by the base is not enough

Dividing the scan by the film base fixes the **offset**, not the **slope**. It
makes unexposed film neutral (`D = 0` in every channel), which removes the
mask's constant part. What remains is that each channel's density **rises at a
different rate** with exposure — an error that is zero where you normalized and
worst in the highlights. It has four contributors (film layer gammas, interimage
effects, the scanner's channels, development), and our shipped gain sits ≈10 %
from what the datasheets imply once their own offset term is respected.
**Appendix B** carries both, and the reason the earlier "factor of two" claim
was wrong.

**The decode corrects the common part, and only that.** One global
`density.scale` is the best single value across stocks, developers and scanners;
it cannot fit every scan, because the residual varies by stock, by developer and
— through the illuminant — by frame. Fixing it perfectly would mean measuring
each scan, which nothing short of the calibration frames provides. So the decode
aims at **acceptable on any stock**, not neutral on every frame, and the
remainder is closed in rendering, per stock, roll or frame, with the per-channel
grade (Part 2). A per-roll or per-frame value in the decode would make it stop
being fixed, which is the property the whole design rests on.

### What NLP's white-balance step does, and why ours differs

Negative Lab Pro white-balances on the film base in Lightroom, which is **the
same operation** as nc's base division in another domain — and so it also fixes
only the constant and leaves the slope. It then appears to fit each channel per
frame, which nc's roll-consistency principle rules out. The real difference is
not "datasheet vs content" but **fitted per frame vs measured per roll**.
(Appendix D; `algo/contrast-latitude-spike` owns what NLP actually does.)

### Knobs currently in reconstruction, sorted

- **Measurement, keep:** film base (per roll, from that roll's own rebate);
  `density.scale` — but see below: it is a fit of *our* chain, shipped as a prior
  for everyone else's.
- **Convention, frozen:** the anchor rule (`mid-at-base-offset`), its `d`, and
  the linearization half of `gamma`. `d` and the linearization are *per stock*
  in the registry, so fixing them is what keeps the decode stock-agnostic.
- **Default pending evidence:** `offset = [0, 0, 0]` — a real term with no
  identifiable value yet.
- **Rendering, move out:** sigmoid `toe` / `shoulder`; per-stock curves;
  `gamma`'s print- contrast half; `shadow_balance` / `highlight_balance` (a
  grade — see Part 2's per-channel control, which subsumes them; retired in
  `nf-retire/regional-balance`).
- **Tuned by review:** `scale` and `gamma` are the two knobs a visual review can
  settle. `#124` (the `scale` recalibration) was the first such round; `#120`
  moved the shared *brightness* target, which this design puts in rendering, so
  it is not a precedent for the loop.

### Removal constraints

- The default is still the knee'd sigmoid, and `legacy` depends on it, so
  removal comes **after** `algo/split-default-migration`.
- nc is unreleased, so removal is cheap. Follow the precedent of the removed
  `algorithm` key: old recipes get a migration error, with no aliases.

## Reference for the migration

As of `pipeline_version` 5 and the 2026-09-15 brightness target, **`--preset
sigmoid-knees` is the best result reviewed so far** — on whites decisively, on
midtones not. It stays available as the reference the fixed decode plus
rendering is judged against, even though the knee'd sigmoid leaves the product
later.

**Measured 2026-09-17: `sigmoid-knees` is not more neutral, it desaturates
highlights.** Its channels converge as lightness rises, so every white surface
reads white under it; in the midtones they do not, and on asian skin the user
ranked it worst of three. A luminance-preserving operator like reinhard cannot
reproduce that by construction. **What causes the convergence is not
established** — see Appendix F.

Two consequences. **The decode is not what makes whites white here**, so a cast
measured on white surfaces is partly a statement about the display operator. And
**the behaviour the user prefers is a real print behaviour** — paper applies
per-channel curves, which desaturate highlights — so it belongs in rendering as
an explicit control (Part 2), not in the decode. The cast's *direction* is per
roll, which fits the developer split. (Appendix E.)

## Colour: what "character" is, and what the decode must keep

What the datasheet curves actually show — the orange mask, the film base, the
exposure axis — is in **Appendix A**; this section is only about which
differences between stocks the decode keeps.

### Kinds of character

| Kind | Example | Where it belongs |
|---|---|---|
| **Neutral balance** | grey prints grey | Not character. Every stock is designed neutral under its rated light, and the lab keeps greys neutral. |
| **Illuminant cast** | tungsten light on daylight film | A scene fact. Kept by the decode; white balance decides. |
| **Colour rendering** | Ektar's saturation and hue shifts | Real character, from spectral sensitivities and dye interactions. Kept automatically: the layers saw the scene through the stock's sensitivities. |
| **Tone character** | Ektar prints punchier than Portra | Real character. Kept by the fixed decode; removed only by the optional per-stock normalization. |

Greys staying grey while other colours change per stock is possible
mathematically (any 3×3 whose rows each sum to 1 preserves the neutral axis) and
is how colour film is designed.

### Dye layers and the NC film RGB v1 3×3

Colour negative film has three emulsion layers sensitive to blue, green and red
light. Each forms the complementary dye (yellow, magenta, cyan). After decoding,
"R" means how much light the red-sensitive layer received. Those channels are
defined by the film's own sensitivities, not by any standard colour space.

Converting one RGB space into another is a 3×3 linear mix, and it can only be
computed from known primaries. **NC film RGB v1 declares the film's channels to
be Rec.709 primaries with D65 white**, then applies the standard matrix into
ACEScg. Nothing measured supports that declaration. Neutrals are unaffected
(white maps to white); saturation and hue of colours depend on it. A real
characterization would come from spectral data or a ColorChecker.

## The sigmoid shoulder is a contract violation, not a rendering option

The sigmoid's shoulder runs **in reconstruction** and compresses everything
above diffuse white before either display branch sees it. SDR and HDR therefore
receive identical input, and the default gain map decodes as 1.0x. With the
shoulder off, the same frame reaches 4.87x.

CLAUDE.md currently calls this "not a blocker … a rendering-intent option".
Under this goal it is a violation of the stage boundary: reconstruction
discarded range that rendering was entitled to. The cause is historical: the
curve was designed when nc had one SDR output and reconstruct-plus-print was a
single step. The fix is the migration already planned
(`algo/split-default-migration`). What changes is the framing, not the plan.

## Handed to rendering

- **Exposure and white balance:** the anchor gain and any per-channel constant,
  which the decode deliberately leaves at its conventions.
- **Contrast:** `gamma`'s print-contrast half.
- **A per-channel grade** for a cast that varies with brightness — the tunable
  counterpart of `scale`, which cannot be tuned per stock in the decode (Part
  2).
- **Highlight desaturation (path to white)** — what a paper's per-channel curves
  do, and measurably what makes `sigmoid-knees` look clean on whites (Part 2).
- **Per-stock normalization** (optional): the per-stock characteristic
  inversion, applied on top of the fixed decode.
- **Print or paper emulation** (look): a fixed paper curve applied forward, as a
  print does.
- **Reinhard vs sigmoid is not a family choice.** Classic Reinhard `x/(1+x)` is
  exactly a sigmoid: logistic in log exposure, and nc's own shoulder formula
  with width 1. The extended Reinhard nc ships (`v(1 + v/W²)/(1+v)`) has no
  upper limit, so it is not strictly a sigmoid. The characteristic presets use
  reinhard because it was the **only display tone that works** on unbounded
  input (`none` refuses it, `shoulder` plateaus it). A sigmoid display operator
  was never built, so it was never compared.
- **`shoulder` and `none` exist for bounded reconstructions.** `shoulder` is
  linear to 0.75, cubic to 1.0, and flat above. Once reconstruction is
  unbounded, both lose their job.

## Open questions

- **Exponential vs `generic-c41`** as the fixed decode, or a toe-limited form
  (invert only where the slope carries information). Note the two target
  different densitometries — the shipped characteristic curves are Status M — so
  a clean comparison needs the calibration that is deferred. The ADX precedent
  (a generic curve plus a matrix) argues for `generic-c41`.
- **How to get to Status M:** the 3×3 + offset, fitted per roll or per
  developer, and from which frames. Interimage effects are per stock and
  cross-channel, so a per-scanner matrix cannot be the whole answer.
- **Whether nc ships a user-facing calibration workflow**, and in which stage its
  result lands (`io/scanner-density-calibration`,
  `color/optional-color-correction-profiles`). Without one, every user inherits a
  prior fitted on one scanner and two developers.
- **Whether `offset` earns a non-zero default.** The term is real and the
  datasheets carry it; two candidate values lost a review, and identifying one
  needs density varied at a single illuminant (`analysis/calibration-frame-capture`,
  or the within-frame two-density test in Part 3).
- **Whether the green residual is film, scanner or developer.** Blue's drift
  matches the sheets (+1.26 measured, +1.29 predicted); Ektar's green does not
  (+1.26 against +0.22). Green is the unexplained term, and the shipped `scale`
  is carrying it.
- **Where the NC film RGB v1 matrix belongs:** reconstruction output in
  film-layer space, with the conversion into a working colour space moving to
  scene correction (a generic assumption or a per-stock characterization)? That
  would change what `film-master` holds.
- **How to evaluate a reconstruction** under this goal: a bracketed neutral
  series works for any stock (greys are designed neutral), with slope equal to
  the stock's own contrast; colour patches measure consistency, not true colour.
  Depends on `analysis/calibration-frame-capture`.
- **The design-spec revision:** principle 2 in §3, the NC film RGB v1 contract
  in §4/§6, and §7's framing of the curves as alternatives.
- **Task impact:** which existing tasks this re-scopes
  (`split-default-migration` targets `characteristic-generic`; retiring the
  sigmoid knees and `simple`; the `shadow_balance` / `highlight_balance`
  decision; CLAUDE.md's HDR framing). To be planned, not assumed.

# Part 2: Rendering and output

## The stages

Rendering is a chain of named stages, each with one job, followed by two output
stages:

```text
reconstruction (the fixed decode)
  → scene correction → look → fit range → fit gamut    rendering
  → encode → package                                   output
```

| Stage | Job | Today |
|---|---|---|
| **scene correction** | Photographic corrections toward what the scene was: white balance, exposure, flare/fog removal. Scene-referred, linear. | `render_split::display_source` (`apply_shared_controls`): WB → exposure → black point → `linear_range` |
| **look** | Creative and optional: contrast, per-channel colour grading, saturation, print or paper emulation, per-stock normalization. Scene-referred. | Doesn't exist as a stage |
| **fit range** | Fit the scene's dynamic range into the display's range, with parameters from the display's peak (SDR vs HDR). The industry term is *tone mapping*: "tone" means brightness levels, not colour. | `fit_range.headroom_stops` (`display_tone::Headroom`) in `pipeline::sdr` / `pipeline::hdr`; `pipeline::fit_range` under `--new-flow` |
| **fit gamut** | Move out-of-gamut colour to the display's boundary, keeping hue. | `fit_gamut::radial_to_boundary`, shared by both chains (the legacy metadata names it `neutral-axis-radial-boundary-v1`, with a `bt2020-` / `display-p3-` prefix on the HDR and gain-map renditions) |
| **encode** | Transfer function (sRGB, PQ or HLG), quantization, counting clipped samples | `color`, `io::encode`, `io::avif` |
| **package** | Container (TIFF / AVIF / gain-map JPEG), ICC profile or CICP, metadata | `io::*` |

Constraints the order carries:

- **Look comes before the SDR/HDR branch.** A gain map requires the two
  renditions to agree below diffuse white. If contrast or character lived in fit
  range, whose parameters depend on the display's peak, the midtones would
  disagree. Only fit range and later stages may differ per branch, and only in the
  display's peak: fit range's headroom shapes the midtones too, so it is shared.
  Below diffuse white the renditions are bit-identical except where the SDR cube
  binds a saturated colour that HDR can show, which the gain map carries per
  channel (`pipeline::chain`'s branch contract). A single-rendition destination
  renders one branch through the same function.
- **Fit range and fit gamut are separate but coupled.** The gamut ceiling
  follows the luminance fit range produced (`fit_gamut::apply`'s
  `max(peak, Y)`), so they stay adjacent.
- **Encode and package are separate.** `hdr-pq` and `hdr-pq-tiff` write the same
  encoded signal in two containers. The gain map is the one thing spanning the
  boundary: it needs both renditions, then gets built into the package.
- **Print emulation is a look; per-stock normalization is optional.** The fixed
  decode keeps each stock's tone character, as a print does. Paper and
  print-film curves are published datasheets too.
- **Contrast lives here.** The decode keeps only the calibrated linearization of
  the film; print contrast is a look knob (Part 1).

## Decisions

- **Rename to match the stages.** Stage names replace mechanical ones
  (`apply_shared_controls`). The `print.*` recipe prefix is a leftover from the
  legacy print path and is renamed. `print.linear_range` is an affine levels
  remap, not fit range, so it needs a new name and a stage to live in.
- **Fit range uses reinhard.** `shoulder` and `none` are retired: both exist for
  reconstructions already bounded at white, and `shoulder` flattens everything
  above 1.0. Their knob `highlight_compress` goes with them. (Done in
  `nf-retire/display-tones`, `pipeline_version` 7.) "Fit range" rather than "highlight compression", because reinhard holds
  mid-grey and reshapes everything above it, costing ≈0.86 stop at diffuse white
  — but note what it does *not* do: below mid it is nearly a gain (see "The shadow
  end" below).
- **Retire `legacy` and `custom`.** This removes the second implementation of
  the print controls: `density::render_print` runs them on film RGB before the
  NC film RGB v1 mapping, while the display presets run them on ACEScg.
  - **Features that exist only on legacy move into the new pipeline:** Adobe RGB
    output is a **must have** (it was reachable only via an ICC path on legacy;
    the new chain's gamut, profile and encode are `output/adobe-rgb-gamut`, a
    destination selecting it `nf-destinations/direct-preset`), and a rendered
    float TIFF (`--out-depth f32`) is good to have.
  - **The legacy-measuring coverage is not preserved across the redesign** — the
    golden vectors, the ~87 legacy-injected integration tests and the
    benchmark's legacy cases. A regression gate measuring legacy would only pin
    the design being replaced; build new gates on the new stages once the work
    is done. **Scope the drift gate carefully, though:** its `render` row hashes
    `reconstruct_and_print`, whose `reconstruct` half *is* the decode being
    kept, and its `base` and `recipe` rows (film-base estimation, the default
    recipe document) have nothing to do with the print path. Retire the print
    half, not the row.

## Three controls the look stage owes

### A per-channel grade with a pivot

Reconstruction's `scale` corrects a cast that grows with brightness, but it acts
on the film layers *before* the 3×3, so changing one channel there moves all
three output channels. That is accurate and unpredictable to tune by hand. The
look stage should carry the photographer-facing counterpart, acting on
working-space channels directly:

```text
out_c = mid · (in_c / mid)^k_c        mid = 0.18
```

As built (`nf-look/per-channel-grade`), green is fixed at 1 and the ACEScg luminance
is restored after the power, so the grade is a colour operator that never moves
neutral contrast; the task's decision records why.

- **The pivot matters.** Without it a per-channel power moves neutral
  everywhere; pivoted at mid-grey, a neutral mid stays neutral and the cast
  grows away from mid in both directions — which is the thing white balance
  cannot do.
- **Scene-referred, before the SDR/HDR branch**, or the two renditions disagree
  in the midtones and the gain map breaks. It needs a guard for values at or
  below zero, which a wide-gamut linear space contains.
- **It subsumes `shadow_balance` / `highlight_balance`** (retired in
  `nf-retire/regional-balance`). Per-channel adjustment by tone region is the same
  family, and the grade needs no per-frame measured range; the standard forms are ASC-CDL (slope,
  offset, power per channel) and per-channel tone curves. One control, not
  three.
- **It does not replace the calibration.** It is a grade on top; without a
  measured per-roll `scale` every roll needs hand-grading.

### Highlight desaturation (path to white)

Measured 2026-09-17: `sigmoid-knees` reads clean on every white surface, and its
channels converge as lightness rises (B/R 1.65 → 1.27 across one frame's deciles,
against 1.76 → 1.48 for the shoulder-less render). **The cause is not established.**
The two presets differ in four ways at once — the per-channel shoulder, the anchor
(mid-fraction 0.28 vs 0.5), `print_exposure` and `display_tone` (`none` vs reinhard)
— so the comparison cannot attribute the whites to the shoulder alone; and the
verdict that ranked them was made by eye, on an axis the reviewer reports being
insensitive to. `nf-calibration/scale-ladder` separates these before this stage is
built. What *is* settled: a luminance-preserving operator cannot converge channels at
all — it scales all three by one factor, so a cast survives to display white.

This is a real print behaviour — paper applies per-channel curves — so the look
stage needs it explicitly rather than inheriting it from a reconstruction curve:

- **A controlled path to white**: chroma goes to zero as a pixel approaches
  white. The SDR renderer's gamut map already does something like this at the
  cube boundary (`sdr.rs:249-266`); this would make it a deliberate,
  parameterised part of the look instead of a side effect at the boundary.
  (Measured 2026-09-23, `docs/reports/gamut-map-share.md`: under `none` and `reinhard`
  in SDR it moves no marked white, though a few bright frames lose some top-end chroma;
  it is heavy only under the `shoulder` tone. Today's clean whites are the sigmoid
  shoulder's.)
- **Parameterised, because it is a look.** How early it starts and how hard it
  pulls are choices, and "off" must stay available: it hides residual cast,
  which is useful for a print and wrong for a diagnostic.
- **Anchored to diffuse white, not to a branch's display white.** "Display white"
  differs between SDR and HDR, so a single pre-branch operator cannot be defined
  against it. Diffuse white is scene-referred and common to both, and a gain map
  only requires the renditions to agree *below* it — which is the same crossover
  the HDR highlight lift already uses. The whole look sits above the SDR/HDR split
  (`nf-display-stages/branch-contract`), so the pull is shared as well as its trigger.

### The shadow end: reinhard compresses upward only

Measured on the shipped operator at the default 6 stops of headroom (input gain
1.2194): the local slope is **1.00 at 0.002 and 0.99 at 0.01**, 0.82 at mid-grey,
0.45 at diffuse white and 0.10 an octave above. So below mid-grey reinhard is
nearly a gain — 0.94 near 0.05, costing 1–3% of shadow slope
(`chain::tests::contrast_not_fit_range_decides_shadow_separation`) — and almost all
of its compression is above.

That leaves the dark end to whatever else is in the chain:

- **Nothing clips at the bottom** — zero maps to zero — but where black lands is
  decided by the decode's contrast, not by the operator. At gamma 2 with
  `d ≈ 0.62` the film base renders near 0.01, about code 28 in sRGB: the pale
  blacks that opened `algo/reference-anchored-sigmoid`.
- **So the black point is doing the job today, by subtraction**, which is the
  wrong instrument: 0.019 crushed 0.69–8.66 % of frames to code 0. Reaching black
  by subtracting is exactly what a toe exists to avoid.
- **The principled fix is a toe in the operator**, which is one of the reasons to
  prefer a parametric member of the sigmoid family over Reinhard. Note the
  measured caution points the other way only for *reconstruction*: a toe there
  bought nothing and cost 1–2 code values of black depth. A toe belongs where the
  display range is known, which is here.

Two open questions below are the same question seen from each end: the parametric
operator (does it earn its keep?) and the black point split (flare removal versus
display black).

### A "direct" preset for external editing

A render that does as little as possible, for a workflow that continues in
Lightroom or Photoshop: identity scene correction, empty look, Adobe RGB, and
only the fit range needed to land in the container. Two properties it must have,
and both come from reconstruction's conventions rather than from rendering:

- **Mid-grey lands mid** when the frame was exposed correctly — the
  `mid-at-base-offset` anchor.
- **The cast stays within an acceptable range on any stock** — the `scale`
  calibration. Acceptable, not perfect: one global value cannot neutralize every
  stock, developer and light, and closing the remainder is a grade, which this
  preset deliberately does not apply.

"Minimal" cannot mean "no tone": Adobe RGB ends at 1.0 and a real decode exceeds
it, so a gentle compression is still a choice, just a fixed and documented one.
This is the preset that makes Adobe RGB a must-have output (Part 2 decisions).

## `film-master` is the reconstruction output

`film-master` is reconstruction → NC film RGB v1 (a 3×3 to ACEScg) → unclamped
f32 TIFF with an ACEScg profile. It runs no rendering stage at all, so it is the
artifact on which a reconstruction is measured. Caveats:

- **It contains whatever reconstruction was configured.** Since `pipeline_version` 6
  the default is the exponential at the fixed decode's configuration, and since
  `nf-retire/characteristic` it is the only curve.
- **The 3×3 treats the dye-layer channels as Rec.709.** Neutrality checks
  survive, since white maps to white. Per-layer slope measurements get slightly
  mixed. The cleanest measurement point is `FilmRgbImage`, before the matrix,
  which nc can't export today.
- **Numbers can be read from it directly. Visual review can't:** it's linear,
  exceeds 1.0 and has no white balance. Comparing reconstructions by eye needs
  one fixed reference rendering held constant across them.

## Open questions

- **A parametric fit-range operator.** Classic Reinhard is one member of the
  sigmoid family. An operator with a toe, contrast, shoulder and display-peak
  parameters could hold both mid-grey and diffuse white, which reinhard can't,
  and could shape the approach to black instead of leaving it to a subtraction.
  Add it only if it beats reinhard at matched lightness.
- **How contrast and the per-channel grade are spelled** — *settled*: separate keys
  under `look` (`look.contrast`, `look.channel_grade`), not one CDL-style object.
  Still open: whether the "direct" preset is a named output preset or a rendering
  profile.
- **The black point is two jobs:** a small flare/fog subtraction (scene
  correction) and display black / toe (fit range). Today it's one linear
  subtraction, and 0.019 crushed 0.69–8.66% of frames to code 0.
- **Other legacy-only outputs** (ProPhoto, arbitrary ICC paths): keep or drop,
  undecided.
- **Where per-stock normalization lives** (scene correction or look). Part 1
  settled that it is optional and in rendering, not which stage.

# Part 3: Evaluating and tuning the decode

Because the decode is invertible, "how much information survived" cannot grade
it. The final image can — but only with **rendering held fixed**, which is what
makes a review set evidence about the decode rather than about a redesigned
rendering. What is left to judge is the two knobs that are the decode's own
rather than duplicates of rendering: `scale` (a cast that grows with brightness)
and `gamma`'s calibration half. Both must be right before the 3×3, where a
rendering grade cannot reach them. Everything else — exposure, white balance,
the anchor — is a convention here and a control there.

**And the rendering that is held fixed shapes what the eye can see.** Measured
2026-09-17: a per-channel highlight compression desaturates whites toward
neutral, so judging cast on white surfaces structurally favours any config that
has one. With a luminance-preserving operator the same decode shows its cast.
Compare decodes under one operator, and prefer midtone neutrals for accuracy.

## What is measurable, and what is opinion

| Question | How it settles |
|---|---|
| `scale` vs `offset` | **Measurable.** A neutral surface must give `D′_r = D′_g = D′_b`. Across several densities, the residual's **slope** is `scale` and its **level** is the offset (equivalently the base). |
| `gamma`'s linearization half | **Measurable, but only with a bracket:** if exposure doubles, the reconstructed value must double. That measures the film's own slope. |
| `gamma`'s print-contrast half | **Opinion.** No neutral reference constrains it, and it belongs to rendering anyway. |
| Whether a per-channel gain suffices | **Measurable:** whatever is left after the best `scale` is the evidence for how much of a 3×3 is needed. |
| Model stability | **Needs a reference after all.** Fitting `scale` per frame and comparing the spread looks reference-free, but a per-frame fit absorbs scene colour and illuminant, so the spread partly measures subject matter — and a model that suppresses real between-frame differences scores *better*. Usable only on known-neutral or bracketed captures under one illuminant. |

## The limits of the data we have

- **The bracketed grey card is postponed, deliberately.** It needs a card
  bought, frames shot on several stocks, developed and scanned.
  `analysis/calibration-frame-capture` holds the protocol and is a **release
  gate, not a blocker**: work continues on visual review until the frames exist.
- **The 31 marked patches are white surfaces, and only approximately neutral.**
  In ordinary photographs the only findable neutral is white; the eye cannot
  tell a slightly tinted grey from a pure one, and cloud and snow carry the
  sky's blue.
- **Their density range is real but confounded with illumination**, so they can
  anchor a level, not a slope — and the 2026-09-17 offset test is what that
  confound predicted: neither offset candidate improved the render. (Appendix
  C.)

## The interim method: compare against other converters

Four producers on the same frames: **NLP**, **SilverFast with CCR**,
**SilverFast without CCR**, and nc's tuned `sigmoid-knees` as the in-house
reference. It is not ground truth, but it makes nc comparable to its
competitors, and three independent converters disagreeing with nc *in the same
direction* is evidence where one disagreeing is not.

- **What the SilverFast pair actually is.** CCR off still applies a per-stock
  NegaFix profile, i.e. the *per-stock inversion Part 1 demotes* — so agreement
  with it is evidence for that design, not for ours, and run across stocks it is
  the sharpest available test of Part 1's central claim. CCR on adds per-frame
  cast removal, so their difference is **SilverFast's estimate of the cast**,
  not a measurement of it: it embeds the same grey-world prior as NLP's
  per-frame fit, and it contains the real scene illuminant, which is exactly
  what must be kept separate. Useful as a reference, not as a measurement.
- **Do not chase adaptation.** NLP and CCR-on both neutralize per frame, which
  nc rejects by principle (and which is why NLP collapses on a frame filled by
  one surface). The useful reading: where the references agree with each other,
  treat it as evidence about the scene; where they differ from nc **the same way
  on every frame**, that is a fixed error in the decode, i.e. `scale` /
  `offset`; where they differ **per frame in different directions**, that is
  their adaptation. A table across many frames separates those; the eye on one
  frame cannot.
- **A consensus reference is cheap, for the slope only.** The references are
  images, so `nctool metrics` reads them. But NLP and CCR-on share a grey-world
  prior, so averaging them shrinks the apparent spread without cancelling the
  bias: two families, not three votes. That shared bias is approximately a
  per-frame per-channel **gain**, i.e. a level, so a consensus is defensible for
  the **slope** and not for the level — which is the half we most need.
- **Estimate the slope within a frame, never by pooling frames.** Every
  reference except CCR-off re-balances per frame, and a per-frame gain is a
  per-frame density offset, so pooling patches across frames confounds the slope
  with each frame's own offset. One surface at two densities *in the same frame*
  is the measurement that identifies it.

### What to hold fixed, or the comparison means nothing

- Match brightness before judging colour (the +1 stop review showed preferences
  move with it).
- One nc rendering config across every nc variant in the set.
- Same output space, same viewer.
- **Judge colour more than tone.** NLP and SilverFast bake their own looks, so a
  contrast comparison mostly compares looks, while a cast comparison transfers.

## Tuning order

The loop tunes **decode** knobs — `scale` and `gamma` — while rendering is held
fixed, and the "direct" preset (Part 2) is the rendering to hold: with scene
correction identity and the look empty, what the eye judges is the decode. The
2026-09-17 caution still applies in that setup: a defect seen there may belong
to the fixed rendering rather than to the decode, so a candidate that loses
should be re-checked under a second rendering before the decode is blamed.

The loop settles the **common-ground** values, not per-frame neutrality: one
`scale` cannot fit every scan, and the residual is rendering's to grade. `scale`
first, then `gamma`: the cast is the open question, and contrast is easier to
judge once the cast is settled. Two or three candidates per review set
keeps a frame's toggle manageable. `#124` and the 2026-09-17 offset test are the
rounds so far.

**Note what the offset test showed about the loop itself.** The candidates were
built from patch arithmetic and the arithmetic preferred them; the eye rejected
both. The patches cannot see a per-frame illuminant, and they cannot see what
the display operator does to a white. So a candidate that wins on patches still
has to win on the picture, and when it does not, the disagreement is information
about the *measurement*, not only about the candidate.

# Part 4: Evaluating and tuning the rendering — not yet planned

Deliberately empty. The same questions (what is measurable, what is opinion,
what to compare against) apply to the rendering stages, but they cannot be
answered before the decode is settled: every rendering judgement made on top of
a moving decode has to be redone. Recorded here so the gap is visible rather
than forgotten.

# Appendices

The body states what a decision rests on; these carry the measurements behind
it. The permanent record is `docs/progress/algo.md` and `docs/progress/io.md` —
where an appendix and a log disagree, the log wins.

## Appendix A — The datasheet and the film base

- **The flat left end of a curve is base + fog, i.e. Dmin**: film that got no
  image exposure, matching the unexposed rebate in a scan. nc's tables store
  density above Dmin.
- **B > G > R density is the orange mask.** Density measures how much light is
  blocked; blocking blue most and red least looks orange. Scan and sheet agree
  in order: an Ektar scan base of `[0.53, 0.26, 0.16]` transmission (design-spec
  §4) is D `[0.28, 0.59, 0.80]`, against the sheet's Dmin `[0.21, 0.63, 0.84]`.
  Measured bases move with development — this roll's own values live in the
  review notes — so only the ordering transfers.
- **The x-axis position encodes film speed only** (absolute lux-seconds). nc
  discards it and places each stock by its published grey-card aim density.
- **Curves are measured on a neutral exposure under the rated light**, so all
  three layers received the same exposure at every point.

## Appendix B — The slope the base division leaves, and the gap to the sheets

Dividing the scan by the film base fixes the **offset**, not the **slope**. It
makes unexposed film neutral (`D = 0` in every channel), which removes the
mask's constant part. What remains is that each channel's density **rises at a
different rate** with exposure: an error that is zero where you normalized and
grows with density, so it is worst in the highlights. Contributors: the film's
own layer gammas differ (the registry's corpus rule is blue 12–19 % steeper than
red, green 2–5 %; they look parallel on the page only because the plot is
logarithmic); interimage effects, which are cross-channel and per stock; the
scanner's channels each integrate a band overlapping more than one dye, so what
they report is not the dye's own density, and that cross term depends on the dye
set; and development, which splits green by date on one scanner.

**How far our fit is from the sheets, stated carefully.** The sheets'
per-channel structure was fitted as a **(gain, offset) pair** — generic green
0.977/−0.036, blue 0.860/−0.057 — while our `[1, 0.84, 0.73]` is a gain alone,
so comparing the two numbers directly is not like for like. Collapsed onto a
zero-offset gain at our own patch densities the sheets imply ≈0.94 green and
≈0.80 blue, so the real disagreement is ≈10 %, not the factor of two an earlier
draft claimed. Blue is close to the sheets once the offset is respected; **green
is the term neither the sheets nor a per-channel model explain**, and closing it
is `io/scanner-density-calibration`.

`docs/progress/algo.md` (2026-09-04, 2026-09-06) carries the per-stock tables
and the drift measurements.

## Appendix C — The marked patches: what they are, and what they can measure

31 hand-marked patches over five rolls, in `../temp/neutral-patches/`
(uncommitted — the frames are the user's photographs). They are the data behind
`density.scale = [1, 0.84, 0.73]` (`pipeline_version` 5) and behind the
2026-09-17 offset test.

- **What they are:** 29 `white` and 2 `grey` — cloud x10, walls, cloth, cars,
  snow, a flag, a boat, a lily, a door, a curtain. No grey cards. White is the
  only neutral findable in ordinary photographs, which is the limitation, not a
  choice.
- **Density range is real but confounded.** Red density spans 0.28–1.33, and
  sorted by density the patches sort by *illumination*: the dark end is shade,
  interior, sunrise and sunset, the bright end is sun. Shade is bluer, so its
  blue layer genuinely received more exposure.
- **Which is why a two-parameter fit is unstable.** Per roll, adding an offset
  cuts the blue residual sharply (Ektar 09-09 rms 0.087 → 0.020; Portra 160
  0.081 → 0.052) — the term is real — but the fitted values scatter far beyond
  anything physical: blue gains 0.63–1.18 with offsets −0.50…+0.07, against the
  datasheets' −0.002…−0.101. Pooled over all 31 the fit is green 0.902/−0.073,
  blue 0.790/−0.075.
- **The development split.** Green wants ≈0.86–0.90 on the July rolls and ≈0.77
  on the September ones, on the same scanner and with the same stock; blue wants
  0.68–0.78 everywhere. The July rolls were developed in CineStill by the user's
  recollection, and the unexposed frames agree something changed (the September
  base is ~0.15–0.17 density denser). Recorded in `docs/progress/io.md`,
  2026-09-16.

## Appendix D — What NLP appears to do

Negative Lab Pro's workflow starts by white-balancing on the film base in
Lightroom. That is **the same operation** as nc's base division, in another
domain: a per-channel gain on the negative is a per-channel offset in density.
(Not exactly — Lightroom's white balance runs through the camera profile and its
matrix — but close.) Crucially it also fixes only the constant and leaves the
slope.

NLP's next step **appears** to fit each channel's range onto the output range
per frame, which would be a per-channel slope + offset derived from the frame's
own content. Treat that as a hypothesis, not a finding:
`algo/contrast-latitude-spike` records that the measured spreads are *compatible
with* the model rather than evidence for it, and that the single-surface
collapse (`ektar0909-1612`) is an observation to explain — a monotone stretch
destroys nothing by itself, so the mechanism that loses the detail is
unidentified. Whatever it is, nc's roll-consistency principle rules out fitting
per frame, so nc's equivalent is the same two numbers **measured per roll** and
frozen into the recipe: not "datasheet vs content", but "fitted per frame vs
measured per roll".

## Appendix E — The 2026-09-17 offset test

Set: `../temp/offset-test/`, six frames x four configs, reviewed with the gain
map stripped so every cell is plain SDR. Configs: `sigmoid-flat` with the
shipped gain `[1, 0.84, 0.73]`; the same with the datasheet-fitted pair (`[1,
0.977, 0.860]` + `[0, −0.036, −0.057]`); the same pair re-fitted to our own
patches (`[1, 0.902, 0.790]` + `[0, −0.073, −0.075]`); and `sigmoid-knees`.

**What the test rejected.** Two *values* for `offset`, not the term: the
datasheet-fitted pair and one re-fitted to our own patches. The term stays open
(Part 1's open questions); what is missing is data that can identify a value.

**Why the offset was a candidate.** It is physically distinct from the film base
even though the two share an axis: the base is *measured* from the rebate, so
the offset is the residual between that and the density where the three layers
correspond to equal exposure — the layers' toes start at different exposures.
The datasheet fits carry exactly that term (blue −0.002…−0.101 by stock). With
the base left free the two are degenerate; with it measured, the offset is
identifiable in principle, which is why it was worth a render rather than an
argument.

**Verdict (user, all six frames):** `sigmoid-knees` is white on every white
surface; the datasheet pair is worst, strongly blue; the shipped gain and the
re-fitted pair sit between, and the re-fitted pair is not an improvement —
pinker on the July Ektar and Portra frames, slightly bluer on `ektar0909-1632`.
On asian skin the ranking reverses: the re-fitted pair is reddest, then the
shipped gain, then `sigmoid-knees`, which reads slightly green and worst.

**Measured on the same renders:**

| | white flag, G/R · B/R | deciles L1 → L9, B/R |
|---|---|---|
| `flat`, shipped gain | 1.084 · 1.089 | 1.76 → 1.48 |
| `flat`, re-fitted pair | 1.069 · 1.086 | 1.70 → 1.47 |
| `flat`, datasheet pair | 1.273 · 1.263 | — |
| `sigmoid-knees` | 1.024 · 1.027 | **1.65 → 1.27** |

(Encoded 8-bit, 18 % inset; the decile columns are one frame's own scene colour,
so only the *trend* across them is comparable.) The knee'd render's much steeper
fall was read at the time as its per-channel shoulder pulling the channels
together; that attribution did not survive (Appendix F). The cast's direction is per roll: blue on 2026-09-09-Ektar100, pink on
2026-07-15-Ektar100 and 2026-09-11-Portra400.

Frame-by-frame notes are in `../temp/notes/observations.md`.

## Appendix F — Superseded claims

Kept so a reader who meets the old number elsewhere knows it was retired, and
why.

- **"`sigmoid-knees` reads clean on every white surface *because* its per-channel
  shoulder pulls the channels together."** The convergence is measured; the cause
  was not. `sigmoid-flat` and `sigmoid-knees` differ in **four** ways at once —
  the shoulder, the anchor (`mid-at-dmax-fraction` 0.28 against 0.5),
  `print_exposure` (0 against 2.17) and `display_tone` (`none` against reinhard) —
  so nothing in that round isolates the shoulder, and the ranking that anointed
  the knee'd render was made by eye on an axis the reviewer reports being
  insensitive to.

  **Narrowed to two candidates on 2026-09-20, by structure rather than by a
  render.** Of the four differences, three cannot move a channel ratio at all:
  `print_exposure` is a scalar gain after the curve; the anchor factors out of
  `out_c = 10^(gamma·(scale_c·D_c − A))` as `10^(−gamma·A)`, identical on every
  channel; and **every nc display tone is luminance-preserving** — `sdr.rs:244-247`
  and `hdr.rs:550` curve one luminance and multiply all three channels by the
  resulting ratio, so `shoulder`, `reinhard` and `none` alike leave every ratio
  invariant. What remains is the **per-channel shoulder** and the **gamut map**,
  which converges radially near luminance 1.0 (`sdr.rs:249-266`) with a ceiling
  that follows the rendered luminance — so the tone choice reaches it indirectly
  even though the tone itself cannot. Separating those two is
  `nf-display-stages/gamut-map-share`'s (filed 2026-09-22). It was pointed at
  `nf-reconstruction/anchor-spike`, which is done and did not separate them — nothing
  turns the gamut map off by flag, so every desaturation measurement so far reads
  shoulder-plus-gamut-map jointly.

  **Separated on 2026-09-23: it is the per-channel shoulder.** Read on both sides of the
  map in float, `sigmoid-knees` has the map touch 0.00% of top-end pixels on all four
  measured rolls, and no marked white (`docs/reports/gamut-map-share.md`). The shoulder
  is the cause by elimination rather than by a matched render: removing it also lifts
  the whites by up to 60 L\*, so its convergence is not measured apart from its
  luminance compression.
- **"The Ektar green cast is a drift, not a hue."** The argument used
  `curve_probe::channel_drift`, which groups **ordinary picture pixels** by red
  density and reads the green/red ratio across the groups, with no grey patch. A
  constant scene colour cancels, but scene colour that correlates with
  brightness does not — and Hawaii frames (bright blue sky and sea over mid-tone
  foliage) are exactly that case, so the result is suggestive, not shown. Two
  points still stand: the Ektar sheet predicts +0.22 green drift where scans
  show +1.26, and `generic-c41` renders Ektar better than Ektar's own sheet.

- **"The sheet and the scanner disagree by a factor of two."** Written in the
  first draft of Part 1 and wrong: it compared the datasheets' **(gain,
  offset)** fit against our **gain-only** fit. Collapsed onto a zero-offset gain
  at our own patch densities the sheets imply ≈0.94 green and ≈0.80 blue, so the
  disagreement is ≈10 %.
- **The mid-slope figures "Ektar 0.64/0.60/0.76, Portra 400 0.52/0.57/0.63."**
  Computed by sampling each channel's table at its own index midpoint — but the
  channels have different point counts, so those are three different densities.
  The registry's own γ column is the number to use, and it puts every stock's
  green *above* red, not below.
- **Reconstruction as "the best estimate of relative per-channel exposure, per
  stock."** Part 1's first goal, replaced 2026-09-17 by the fixed stock-agnostic
  decode, because per-stock inversion normalizes tone character, inverts a toe
  that carries no information, and needs data that cannot exist for every stock,
  developer and scanner.
