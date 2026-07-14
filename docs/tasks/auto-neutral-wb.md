# Auto Neutral White Balance

## Goal

Deterministically estimate per-channel white-balance gains from image statistics
— the equivalent of NLP's Auto-Neutral / Auto-AVG — so a default conversion of an
arbitrary frame comes out near-neutral without hand-tuned `--white-balance`.
Estimated gains are reported and reusable, following the same
measure-once-reuse-for-the-roll pattern as Dmin/Dmax.

## Design

An analysis pass over the **rendered positive** (post-algorithm, pre-output
transform) that computes gains applied via the existing `print.white_balance`
mechanism:

```text
convert:  decode → film base → algorithm → [auto-wb analysis → re-render stage 4 with gains] → color → encode
```

- **Modes as one enum** (recipe key under the `print` section per §9):
  `AutoWb { Off (default) | GrayWorld | Percentile }` or similar —
  - `GrayWorld` (≈ NLP Auto-AVG): equalize channel means; simple, vulnerable to
    dominant colors — document that.
  - `Percentile`/neutral-patch (≈ NLP Auto-Neutral): equalize channels over
    low-saturation pixels or at matched luminance percentiles; more robust.
  Both are pure statistics — deterministic, no ML (per the project's
  "AI-friendly ≠ ML" rule).
- **Explicit gains always win — by *source*, not by value.** Do not detect "user
  set gains" via `gains == default`: an explicit `--white-balance 1,1,1` must
  still beat auto, and after the merge a default-valued field is
  indistinguishable from an omitted one. Model the choice as one source enum
  (e.g. `WbSource { Auto(mode) | Explicit(gains) }`) or carry presence through
  the merge — the house one-enum rule for mutually-exclusive knobs.
- The resolved gains go in the **convert JSON report** so an agent can freeze
  them into a roll recipe. They are **not** added to `nc estimate`: its contract
  is film-base/Dmin-only and it carries no algorithm/print params, so it cannot
  render the positive these statistics are computed from. Extending `estimate`
  is a separate redesign (`estimate-reuse-output` territory), out of scope here.
- Spec: §8/§9 gain the mode + document the workflow; design-spec.md and .html
  together.

## Implementation Suggestion

- The *estimation* pass reads the rendered positive, but the *application* must
  go through the standard stage-4 `print.white_balance` slot — NOT a post-hoc
  multiply on the final output. Stage 4 applies `white_balance` *before* the
  `black_point` subtraction and the `highlight_compress` soft-clip, so a
  post-hoc multiply would differ from a later run reusing the same gains via
  explicit `--white-balance`, breaking measure-once-reuse-for-the-roll. Re-run
  the print render with the estimated gains (stage 3's density→linear output
  can be cached to keep the second pass cheap).
- Ignore non-finite samples and clipped extremes in the statistics; consider
  reusing the border/region conventions so rebate pixels don't skew the estimate.
- Watch the four coupled knob spots + merge tests; `deny_unknown_fields` means
  the recipe key must land in the right section.

## How to Verify

- Unit: synthetic image with a known cast → computed gains neutralize it (channel
  means/percentiles equalize); `Off` is bit-exact with today's output; explicit
  `--white-balance` beats auto in the merge test.
- Determinism: same input + params ⇒ identical gains and output.
- Report contains the resolved gains in a form that round-trips into a recipe.
- Real-scan spot check: a frame with the typical post-inversion blue/cyan cast
  converts to plausibly neutral grays.

## Dependencies

- [Density-domain algorithm](algo-density.md)
- [Pipeline orchestration](pipeline-orchestration.md)
