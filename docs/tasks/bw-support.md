# Black & White Negative Support (Mono Color Model)

## Goal

Convert B&W negatives to clean mono positives through the existing `density`
algorithm. B&W film is still a density medium, so no new render stage is needed
— the gap versus Negative Lab Pro's B&W mode
(<https://www.negativelabpro.com/guide/blackwhite/>) is their **"Color Model:
B+W"**: pooling R,G,B into a single gray so scanner channel mismatch and
residual casts cannot tint the output. `simple` stays a debug baseline. Tone /
toning effects (sepia, split-tone) are explicitly out of scope. 16-bit RAW scan
*input* is a separate concern that stays in roadmap item 3 (design-spec §12) —
do not pull input-format work into this task.

## Design

- **The density path already is the B&W renderer.** Mapping to NLP's B&W
  controls: `--density-gamma` ≈ paper grade (contrast), `--print-exposure` ≈
  brightness, the auto-`Dmax` percentile white anchor ≈ NLP's WhiteClip. The
  only missing piece is the mono color model; under mono, white balance can
  no longer tint the output (see below), matching NLP's "no white balance
  needed in B&W mode".
- **Knob shape — one enum** (house rule for mutually-exclusive knobs):
  `ColorModel { Color (default) | Mono([f32; 3] weights) }`. Recipe shape like
  `film_base.source`: `"color"` or `{ "mono": [r, g, b] }`. CLI:
  `--color-model color|mono` plus an optional `--mono-weights R,G,B`.
  Validation is against the **post-merge resolved model**, not flag presence:
  `--color-model mono --mono-weights …` sets the weights; `--mono-weights`
  alone over a recipe whose `color_model` already resolves to mono overrides
  just the weights; it is a usage error only when the resolved model is
  `color` (weights with nothing to apply to). `--color-model` by itself
  replaces a recipe's `color_model` wholesale (mono gets the documented
  default weights). Weights are normalized to sum 1; pick and document
  a default (equal thirds or Rec. 709 luma — implementer's call, the point of
  exposing weights is emulating a green- or blue-heavy channel mix when one
  scanner channel is noisy).
- **Placement: post-algorithm, pre-output-transform, in
  `pipeline::stages::render`** — after `convert_reported`, before
  `color::to_output`. That makes it work with both algorithms, keeps it in the
  linear working space, and keeps density conversion / print rendering
  untouched (core fidelity rule). Values pass through unclamped as always
  (clamping only at u16 encode).
- **Interaction with the auto-`Dmax` anchor.** The white anchor's percentile
  is measured *inside* the density render, pooled across all three channels —
  before post-algorithm pooling can exclude anything. So under `Mono`, a noisy
  channel the weights were chosen to avoid could still drive the anchor and
  skew the pooled exposure. Thread the resolved color model into the density
  algorithm's `Dmax` resolution: when `color_model = mono`, drop zero-weight
  channels from (or weight the sample stream by) the percentile sample.
  Deterministic either way; an explicit `--d-max` remains the escape hatch,
  and `color` mode must keep today's pooled sampling bit-exactly.
  Channel weighting alone still doesn't address **spatial** outliers: on an
  uncropped frame the dark holder or dust/scratches (all high-density —
  the thin rebate is Dmin and sits at the *low* end, so it can't drive the
  anchor) can occupy more than the top 0.5% of densities and become the
  anchor, dimming the render (NLP's guide likewise says to crop or buffer
  non-film area before evaluation).
  Spatially excluding border pixels from the anchor statistics (reusing the
  border/region conventions) benefits color conversions equally — treat it
  as a shared consideration and split it into its own task if it grows.
- **Recipe key: top-level `color_model`**, parallel to top-level `algorithm` —
  it is a cross-algorithm render selector, so it belongs to neither `density`
  nor `simple` nor `output` (§9 assigns keys by stage; this is a new small
  §9 section, "Color model (post-algorithm)"). Keep the recipe struct and §9
  in sync — `deny_unknown_fields` means a misplaced key makes a docs-shaped
  recipe fail to load (a loud `Usage` error, exit 2), so the key must land
  exactly where §9 assigns it. Update design-spec.md **and** .html together.
- **White balance under mono is tint-free but NOT a no-op.** The gains apply
  inside the render, *before* the black-point subtraction / highlight
  soft-clip (`density`) or the clip remap (`simple`), and pooling happens
  after those — so white balance still shifts the pooled gray values
  (tonality); it just cannot tint. Nor is it equivalent to `--mono-weights`,
  which mixes *after* the non-linear print steps. Do not reject it; document
  the distinction so a mono recipe isn't "simplified" by folding WB gains
  into the weights.
- **Output stays 3-channel RGB with R==G==B.** True grayscale TIFF encode is a
  possible follow-up, not this task — keeping the encode/ICC path untouched is
  what makes this change small.
- **IR note.** Silver B&W film blocks IR, so the HDRi IR channel is useless for
  dust removal on traditional B&W negatives. Nothing to do now (Step 1
  preserves but never consumes IR) — but the future IR dust-removal task
  (roadmap item 1) must key its guard on the **film type** (a knob that task
  introduces), *not* on `color_model = mono`: color film rendered mono and
  chromogenic (C-41) B&W both keep a usable IR plane, while a silver B&W scan
  rendered in color mode still doesn't.
- **Stretch item (do not let it block shipping): auto black-clip percentile** —
  the shadow-side twin of the auto-`Dmax` white anchor (≈ NLP's BlackClip);
  today `--black-point` is manual only. Sketch: grow `print.black_point` into a
  source enum (`Explicit(f32)` default vs `Auto(percentile)`), resolved value
  in the JSON report. The recipe-shape change for `black_point` is acceptable
  pre-release (same policy as the `output.hdr` rename): follow the house
  tagged-enum wire form (`FilmBaseSource`/`DmaxSource` convention) and reject
  old plain-float recipes loudly — no untagged back-compat shim, which would
  also blur serde's error messages. Marked stretch rather than core because
  it is orthogonal to the mono model (it benefits color conversions equally),
  reshapes an
  existing print knob, and interacts with the `Dmax` auto anchor — if it grows,
  split it into its own task instead.

## Implementation Suggestion

- A small pure helper (e.g. `pool_mono(&mut LinearImage, [f32; 3])`) called
  from `stages::render`; IR plane untouched. Compute the gray value **once**
  per pixel and copy it to all three channels — three separate dot products
  could differ in the last ulp and break the R==G==B invariant.
- Normalize weights at validate/build time; reject degenerate weights loudly
  (non-finite, negative, or non-positive sum) — fail-loudly rule.
- Every new knob spans the four coupled spots: CLI `*Overrides` field
  (`cli.rs`), recipe struct (`types.rs`), `merge` arm, `validate` check — a
  forgotten `merge` arm silently makes the flag a no-op, so add a merge test.
- Report the resolved color model (and normalized weights) in the convert JSON
  report so a recipe can be frozen from it.

## How to Verify

- Unit: after pooling (pre-output-transform), every pixel has R==G==B exactly;
  end-to-end output is neutral (channels equal within transform tolerance).
- Unit: weights normalize (`[2,1,1]` ≡ `[0.5,0.25,0.25]`); degenerate weights
  (zero sum, negative, NaN) fail loudly.
- Unit: `Color` (the default) is bit-exact with today's output.
- Merge tests: `--color-model mono` replaces a recipe's `"color"`;
  `--mono-weights` without mono mode is a usage error; recipe keys land under
  the §9-assigned (top-level) location.
- Determinism: same input + params ⇒ identical output, twice.
- Real-scan spot check per CLAUDE.md rules (never read sample scans into
  context): convert a real B&W scan — or a color scan as a stand-in — with
  `--color-model mono` via a throwaway `#[ignore]` test or the CLI, and check
  only derived numbers (per-channel equality stats, clipped-sample counts).

## Dependencies

- [Density-domain algorithm](algo-density.md)
- [Pipeline orchestration](pipeline-orchestration.md)
- [Display-range white anchor (Dmax)](dmax-white-anchor.md) — the design
  changes the auto-`Dmax` measurement path that task introduced.

All are complete — this task is immediately unblocked.
