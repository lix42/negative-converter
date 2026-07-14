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
  only missing piece is the mono color model; white balance is a no-op under
  mono (see below), matching NLP's "no white balance needed in B&W mode".
- **Knob shape — one enum** (house rule for mutually-exclusive knobs):
  `ColorModel { Color (default) | Mono([f32; 3] weights) }`. Recipe shape like
  `film_base.source`: `"color"` or `{ "mono": [r, g, b] }`. CLI:
  `--color-model color|mono` plus an optional `--mono-weights R,G,B` (usage
  error without `mono`; whichever flags are given replace a recipe's
  `color_model` wholesale). Weights are normalized to sum 1; pick and document
  a default (equal thirds or Rec. 709 luma — implementer's call, the point of
  exposing weights is emulating a green- or blue-heavy channel mix when one
  scanner channel is noisy).
- **Placement: post-algorithm, pre-output-transform, in
  `pipeline::stages::render`** — after `convert_reported`, before
  `color::to_output`. That makes it work with both algorithms, keeps it in the
  linear working space, and keeps density conversion / print rendering
  untouched (core fidelity rule). Values pass through unclamped as always
  (clamping only at u16 encode).
- **Recipe key: top-level `color_model`**, parallel to top-level `algorithm` —
  it is a cross-algorithm render selector, so it belongs to neither `density`
  nor `simple` nor `output` (§9 assigns keys by stage; this is a new small
  §9 section, "Color model (post-algorithm)"). Keep the recipe struct and §9
  in sync — `deny_unknown_fields` means a misplaced key silently rejects
  docs-shaped recipes. Update design-spec.md **and** .html together.
- **White balance under mono.** `print.white_balance` applies per-channel gains
  *before* pooling, so under mono it only re-weights the channel mix — the
  output carries no tint regardless. Do not reject it; document that it is
  redundant in mono mode.
- **Output stays 3-channel RGB with R==G==B.** True grayscale TIFF encode is a
  possible follow-up, not this task — keeping the encode/ICC path untouched is
  what makes this change small.
- **IR note.** Silver B&W film blocks IR, so the HDRi IR channel is useless for
  dust removal on B&W. Nothing to do now (Step 1 preserves but never consumes
  IR), but the future IR dust-removal task (roadmap item 1) must be
  disabled/guarded when `color_model = mono`.
- **Stretch item (do not let it block shipping): auto black-clip percentile** —
  the shadow-side twin of the auto-`Dmax` white anchor (≈ NLP's BlackClip);
  today `--black-point` is manual only. Sketch: grow `print.black_point` into a
  source enum (`Explicit(f32)` default vs `Auto(percentile)`), resolved value
  in the JSON report. Marked stretch rather than core because it is orthogonal
  to the mono model (it benefits color conversions equally), reshapes an
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

Both are complete — this task is immediately unblocked.
