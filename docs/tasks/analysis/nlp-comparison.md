# Reference Comparison: nc vs NLP and Hand-Tweaked Targets

## Goal

Make "is our render closer to what I want than NLP's?" a question with a
reproducible numeric answer. Compare an nc configuration against reference
conversions of the same source frame — Negative Lab Pro, SmartConvert, and the
user's own hand-tweaked exports — on the metric set from
[`conversion-metrics`](conversion-metrics.md), pairing by identity through the
manifest rather than by pixel registration.

## Two kinds of reference, and why it matters

NLP is **not** ground truth — the user is often not happy with its result and
edits it afterwards. So a reference carries a role:

- **`reference`** — another tool's output as it came out (NLP, SmartConvert).
- **`target`** — an image the user has edited to what they actually wanted.

Every axis then yields three deltas: nc→target, NLP→target, and nc→NLP. The
second one is the useful one twice over: it says which axes NLP systematically
gets wrong, and it supplies the *scale* for what counts as a meaningful
difference on that axis. The acceptance question becomes
`|nc − target| < |NLP − target|` per axis — we landed closer to the intent than
NLP did — which is a claim a number can carry and a side-by-side cannot.

## Concrete inputs (verified 2026-09-02)

- NLP outputs are **32-bit float TIFF carrying a linear sRGB profile**. The
  manifest records `encoding: "f32"`, which names the depth but *not* the
  transfer — the color-space declaration
  [`conversion-metrics`](conversion-metrics.md) requires is not in the manifest
  yet.
- Framing differs per set: `nlp/2026-07-23` is 4406×2930 against a 5184×3600
  source — cropped, and to a **different aspect ratio** (1.504 vs 1.44), so it is
  not a uniform inset and cannot be undone by arithmetic. `nlp/2026-08-04` is
  5184×3600, i.e. **full frame**.
- `converted/SmartConvert/TIFF` exists (u16, LZW, **no ICC profile at all**) and
  its manifest entries have `source_frame: null`, so it is unpaired today.
- Two Portra160 source frames (1096, 1097) have no NLP output; the manifest
  already records these in `coverage_gaps`.
- nc's default output is a **gain-map JPEG**. Its SDR base is readable
  (`metrics --jpeg-image sdr`, the default), so a default roll can be compared —
  but that base is not the rendition an HDR-aware viewer shows, and reconstructing
  the HDR one is not implemented. A comparison that wants the HDR signal renders
  through `hdr-linear-tiff` instead.

## Decided (2026-09-02)

- **Pairing is by `source_frame`, never by registration.** Differing crop,
  aspect and color space make per-pixel diffing meaningless in general.
- **Pixel-wise comparison is an opt-in bonus, gated on exact dimension equality**
  after region selection — never by resampling one side to match the other. The
  `2026-08-04` set will actually qualify; most sets will not, and the artifact
  says which mode it used.
- **Crop-tolerant pair statistics** carry the comparison: matched-percentile
  regression across the two tone distributions decomposes a difference into
  exposure offset, contrast ratio and residual curve shape without any alignment,
  and a distribution distance gives one headline number. Exact estimators are
  settled while implementing.
- **A missing side is reported, not skipped**, and every artifact states the
  no-registration caveat and the regions it used.

## What `conversion-metrics` settled (2026-09-03)

It shipped as `nctool metrics {image,roll,table}`, so this task consumes rather
than builds the measurement:

- **Only the reference side needs a declared color space.** `metrics roll`
  resolves nc's own space from the run's frozen recipe and refuses an
  under-determined one, so the manifest work below shrinks to the reference and
  target images.
- **Reuse `frame_axes` and `spread`, do not re-derive them.** The scalar axis list
  and the crossover terms already exist; a comparison is a delta over the same
  axes, and a second list would drift from the first.
- **A per-roll rollup does mean something, as spread rather than mean** — but it
  is **not attributable**: one frozen recipe serves every frame, so variation
  combines scene content with calibration fit. A comparison rollup inherits that
  caveat, with one improvement: `|nc − target|` per frame is already
  content-normalized, so *its* spread is more nearly about the render than the
  absolute spread is.
- **A default (gain-map JPEG) roll is measurable as its SDR base**, so no
  re-render is needed to compare one — but the record's `gain_map_present` flag
  has to reach the comparison output, or a P3 SDR base gets compared against an
  NLP export as though it were the whole rendition.

## Open questions

- Where do the reference role and the color-space declaration live — additive
  manifest fields (`manifest generate` preserves human fields, so this is cheap),
  or a separate comparison-set spec beside the run? Hand-tweaked targets need a
  home either way, and they are not regenerable.
- SmartConvert has no `source_frame` and no profile. Worth pairing at all, or
  left out until someone declares both by hand?
- Which axes deserve a stated tolerance, and is that tolerance derived from the
  NLP→target spread rather than picked?
- The side-by-side visual sheet: does it belong here, or does this task feed
  pairs to [`comparison-review-tooling`](comparison-review-tooling.md) rather
  than growing a second page builder? Visual review remains a required step —
  the numbers support it, they do not replace it.
- Which of `metrics`' axes belong in a comparison table at all? Thirteen plus two
  crossover terms is right for one roll's absolute numbers; a three-way delta over
  all of them may be more than a reader can use.

## How to Verify

- For a roll with both sides present, the tool emits a per-frame delta table
  (JSON + Markdown) and names the frames missing a side rather than dropping them.
- On `nlp/2026-08-04` the pixel-wise section engages; on `nlp/2026-07-23` it is
  absent with the dimension mismatch stated.
- Re-running produces identical artifacts; the region used is recorded, so a
  result can be reproduced from the artifact alone.
- A comparison whose conclusion contradicts a side-by-side visual review is a
  finding about the metrics, not about the render — at least one such check
  is made before any conclusion is published to `docs/reports/`.

## Dependencies

- [Conversion Metrics & Photographic Analysis](conversion-metrics.md)
