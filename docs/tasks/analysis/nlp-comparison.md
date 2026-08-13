# NLP vs nc Comparison

## Goal

Ingest Negative Lab Pro (NLP) conversion outputs (the user adds them to
`nc-assets`) and compare them against nc's outputs: global per-image metrics side
by side, plus side-by-side downscaled thumbnails. Make judging conversion quality
against the NLP reference repeatable — without per-pixel registration, which is a
separate, harder problem.

## Design

Builds on the manifest and the metrics toolkit. NLP outputs land in a manifest
`converted` bucket with `producer: "nlp"` and, crucially, a `source_frame` link
back to the original roll frame — that linkage is how nc↔NLP↔source are aligned
(by identity, **not** by pixel registration).

`python -m nctool compare` then:
- Runs the [`conversion-metrics`](conversion-metrics.md) metric set on the nc
  output, the NLP output, and (optionally) the source, aligned via `source_frame`.
- Emits a **diff table** (JSON + Markdown) of the global metrics: percentiles,
  black/white points, contrast, saturation, clip %.
- Renders a **side-by-side contact sheet** (source | nc | NLP) from downscaled
  thumbnails for visual judgment.

**No registration / alignment.** NLP and nc differ in color space, encoding, and
framing (crop/rotation), so per-pixel diffs are not meaningful without warping.
The comparison is global-statistics + visual, and every output prints that caveat
explicitly. If per-pixel comparison is later wanted, it becomes its own task
(color-space normalization + registration).

## Concrete inputs (as of 2026-07-24)

The first NLP set is in the manifest under `converted → nlp/2026-07-23`: four
outputs for the `Portra160-2026-07-22` roll (frames 1102, 1111, 1121, 1127).
Observed facts that validate the no-registration design:

- NLP outputs are **32-bit float TIFF, 4406×2930** — **cropped** relative to the
  5184×3600 source (≈15 % smaller), so per-pixel alignment is impossible without
  warping.
- **2 of 6 source frames (1096, 1097) have no NLP output** — recorded in the
  manifest's `coverage_gaps`; `compare` must report the missing side, not skip it.

## Implementation Suggestion

- NLP export format is 32-bit float TIFF (above); `metrics.py` must handle float
  as well as 16-bit. The manifest's `source_frame` link is the alignment key.
- Match nc↔NLP by `source_frame`; if NLP framing crops differently, the metrics
  are still comparable as global tone/color summaries — note framing differences
  rather than trying to correct them.
- Keep the diff table's column order stable (source, nc, NLP) so reports are
  diffable across runs.

## How to Verify

- With NLP outputs registered in the manifest, `python -m nctool compare
  <roll>` emits a metrics diff table and a contact sheet for each frame that has
  both an nc and an NLP output.
- Frames missing one side are reported (not silently skipped).
- The caveat about no registration / differing color space is present in the
  output.
- A written comparison lands in `docs/reports/` for at least one roll.

## Dependencies

- [Conversion Metrics & Photographic Analysis](conversion-metrics.md)
