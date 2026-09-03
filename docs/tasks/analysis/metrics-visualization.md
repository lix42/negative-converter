# Metrics Visualization

## Goal

Make the measurements readable as pictures, **inside the existing review app**,
so that numeric review sits beside visual review instead of in a separate tool.
Today `nctool metrics` emits JSON and a Markdown table; both answer "what
changed" well and "what does that look like" not at all.

## What exists

- **`nctool metrics {image,roll,table}`** (`analysis/conversion-metrics`, done
  2026-09-03) writes a per-image record — endpoint occupancy, tone in log2 stops,
  colour in CIELAB — and a per-roll rollup with a spread table. That is the data
  source; this task does not add measurements.
- **`tools/review-app`** (`analysis/comparison-review-tooling`, viewer half
  shipped on `main` 2026-09-02; Vite+ / Solid / StyleX) compares configs by
  toggling renditions **in place**: every rendition of an image shares one grid
  cell, so switching config cannot move the picture. Its `review.json` is
  `configs × images → renditions`, and `SCHEMA.md` already calls `images[].note`
  "the natural home for measured numbers" — a hook nobody has used yet.

Those two shapes line up: a metrics record is per (image, config), which is
exactly what `renditions` is keyed by.

## What the design discussion settled (2026-09-03)

Ranked by value, with the reasoning, because the ordering is the useful part:

1. **`percentiles_stops` as a curve** — the highest-value plot and the place to
   start. It contains what `bands` contains and more, and **two overlaid answer
   the question the app exists for**: vertical gap is exposure, relative slope is
   contrast, and where they diverge says whether shadows, midtones or highlights
   moved. Annotating the ends gives `toe_span` / `shoulder_span` for free — a
   curve going horizontal at the top *is* `shoulder_span: 0`.
2. **`cast_by_tone_band` as a path on the a\*/b\* plane**, one point per band
   joined in tone order. The shape of that path is crossover: a tight cluster is
   clean, a long diagonal sweep is not. As a bar chart the same data is nearly
   unreadable.
3. **`bands` as a stacked bar** — redundant with (1) for a single image, but the
   right way to show a whole roll at once, one bar per frame.
4. **`endpoints` as a small per-channel bar.** Unglamorous, but it is the closest
   thing here to a correctness signal, and on a real frame it showed a 22%
   top-code population that was *blue only* — which no other view made obvious.
5. **`hue_sectors`, last.** Six sectors is coarse and the natural polar rendering
   invites over-reading; two polar charts also compare poorly, which matters in a
   comparison tool.

Scalars (`key_stops`, `contrast.p95_minus_p5`, `mean_cast`, `b_over_g`) belong in
a readout strip, not a chart.

**The constraint that shapes all of it: design every chart to overlay two configs
from the start**, even if the first version renders one. That is what ranks the
curve and the a\*b\* path above the others, and it is the app's whole premise.

## Open questions

- How does a metrics record reach the app? A new key on `renditions[config]`, a
  sibling file the page fetches, or inlined axes in `review.json`? Inlining keeps
  a review set self-contained (the schema's stated virtue) but duplicates data
  that already exists; a reference keeps one copy but breaks "move the directory
  anywhere".
- Do the charts **toggle in place** like the image does, or sit beside it? In
  place is consistent with the app's core interaction, but an overlay of both
  configs at once is strictly more informative for a curve — those pull in
  opposite directions and the answer may differ per chart.
- What does a config with no measurement render as? The schema already says a
  missing *rendition* is fine and an unknown config id is an error; charts need
  their own answer.
- Per-frame and per-roll are different views. Does the app grow a roll view, or
  does the roll rollup stay in Markdown?
- Hand-rolled SVG or a charting library? The app has no chart dependency today
  and this repo has a habit of hand-rolled SVG/CSS, but overlays with axes and
  hit-testing are where that habit stops paying.
- Percentages and units: the JSON keeps fractions deliberately, and the Markdown
  report converts. A chart needs the same decision made once.

## Non-goals

Not a new application, not a replacement for the Markdown report (which stays the
committable, diffable artifact), and no verdicts — the same rule the measurement
side holds to: it describes, it does not rank.

## How to Verify

- A review set carrying measurements renders its charts, and one without them
  still loads — the app must not require the data it did not have yesterday.
- Switching config updates the charts and the picture together, without either
  moving on screen.
- A chart's numbers agree with the record it came from; a reader can go from a
  visible feature to the field that produced it.
- The app's own gates (`pnpm verify`) stay green, and a set with a partially
  measured config is handled rather than crashing.

## Dependencies

- [Conversion Metrics & Photographic Analysis](conversion-metrics.md)
- [Comparison review tooling](comparison-review-tooling.md)
