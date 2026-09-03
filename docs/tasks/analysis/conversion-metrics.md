# Conversion Metrics & Photographic Analysis

## Background

`nctool roll analyze` normalizes conversion provenance and the aggregate
statistics already present in `nc roll` reports. Comparing the Portra 400 Dmax
1.2 and 1.9 runs showed that this is not enough to understand the result as an
image: channel means reveal a broad brightness change, but not how color and tone
are distributed, whether highlights or shadows occupy useful range, how much of
the frame lies in those regions, or how closely the result approaches the black
and white limits.

The deeper gap: **nothing in the repo reads pixels out of an output image.** Every
number the analysis tooling reports today is derived from `nc`'s own JSON report,
so it exists only for nc outputs. NLP conversions, SmartConvert conversions, and
hand-tweaked exports are invisible to all of it — which is what blocks
[`nlp-comparison`](nlp-comparison.md).

## Goal

A deterministic, pixel-derived metric artifact for *any* output image regardless
of which tool produced it, describing tone, color, range use, and endpoint
behavior well enough that two renderings can be compared numerically. Keep
objective measurement separate from aesthetic judgment: the artifact describes,
it does not rank.

## Decided (2026-09-02)

- **`numpy` + `tifffile` in a venv**, with `requirements.txt` under
  `scripts/analysis/`. The metric math is unit-tested on small synthetic arrays so
  the suite stays hermetic, but the Python CI gate has to install the dependencies
  on Linux and macOS — `nctool` stops being stdlib-only, and `scripts/analysis/README.md`,
  `scripts/README.md` and CLAUDE.md's gate command all say otherwise today.
- **Every input's color space is declared, never guessed.** A small named
  allowlist (sRGB, Display P3, Adobe RGB, ProPhoto, BT.2020, each linear or
  encoded) resolved from the manifest or the invocation; an unstated or
  unrecognized space is a loud refusal, not a default. Reading it from an embedded
  ICC profile is a possible later convenience, not the contract — and would be a
  second colorimetry source of truth beside `pipeline/colorimetry/`, so it needs
  its own argument.
- **A space the tool supports is defined in `pipeline/colorimetry/definitions.rs`
  first**, even when nc renders to nothing like it (`ADOBE_RGB` is the first such
  entry). Primaries living only in the Python would be a second source of truth by
  construction; this way the analysis tests re-read the Rust and a one-sided edit
  fails.
- **Measure in linear light in one common space.** Decode transfer, convert
  primaries, then derive luminance and a perceptual (Lab-like) representation.
  Metrics computed across mixed encodings — nc's transfer-encoded u16 against
  NLP's linear f32 — are noise, and this is the single easiest way to publish a
  wrong table.
- **Tone lives in log2 stops relative to 0.18**, because that is the domain where
  exposure is an offset and contrast is a slope. The *geometric* mean is the
  exposure statistic; the arithmetic mean of display values is not.
- **Regions are fractions, not pixels** (see below), and the region actually used
  is recorded in the artifact.
- **Derived numbers only.** The tool reads full-resolution pixels and emits
  statistics; sample pixels never reach a report, a committed artifact, or an
  agent context (CLAUDE.md). Downscaled previews for visual review belong to
  [`comparison-review-tooling`](comparison-review-tooling.md).

## Metric families

Definitions, thresholds and the exact common space get settled while
implementing — against real converted rolls, not on paper. The families:

- **Frame facts** — dimensions, declared space, region used, sample count,
  non-finite count, per-channel fraction at or beyond each endpoint.
- **Tone** — log2 geometric mean (the key); a luminance percentile vector in
  stops; contrast as robust percentile spreads; area shares by tone band; how
  compressed each end is (toe and shoulder spans).
- **Color** — per-channel geometric means and the channel balance in stops; mean
  and median chroma; **cast measured separately per tone band**, which is the
  crossover detector and the most diagnostic color number for a negative
  conversion; per-hue-sector chroma and hue placement; neutral share.

Both shipped (tone 2026-09-02, color 2026-09-02). CIELAB's reference white is
derived from this module's own D65 rather than the tabulated `(0.95047, 1,
1.08883)`, so an RGB-neutral frame measures `a* = b* = 0` exactly — a cast metric
whose zero is not zero reports a fault the render does not have. That is a
different choice from the one `display-output-acceptance` pins for its
cross-encoding oracle, which compares absolute colorimetry rather than relative
cast; the two coexist deliberately.

Deliberately **out of scope**: sharpness, micro-contrast and noise (NLP sharpens,
nc does not, and resolutions differ — the metric would always report a difference
nobody intends to close), and any composite quality score.

## Region selection and the film holder

Until [`film-base/ir-holder-detection`](../film-base/ir-holder-detection.md)
lands, uncropped outputs still contain the dark holder and the rebate, which would
dominate the shadow statistics. The tool takes a symmetric inset, an explicit
fractional rectangle, or a per-image region recorded once in a spec — fractions,
not pixels, because the images being compared have different dimensions.

Two traps: a 5% inset often does not clear a real holder (the scans run dark
holder → thin inset rebate → picture, and the holder can occupy 10–15% of one
edge), and excluding near-black pixels as a *proxy* for the holder biases exactly
the shadow metrics the artifact exists to report. Region only. When the IR mask
exists it can feed the region automatically; this task does not wait for it.

## Open questions

- Which common space, and which perceptual representation, keep results
  meaningful across SDR, HDR, integer and float outputs?
- What should shadow, highlight, near-black and near-white mean here, and which
  definitions stay comparable across configurations?
- Full-resolution percentiles, or a deterministic decimation recorded in the
  artifact? (The earlier draft of this task insisted on full-res; with `numpy`
  that is affordable, so the burden is on decimation to justify itself.)
- Which summaries distinguish healthy range use from clipping, lifted blacks,
  empty highlights, or a narrowly clustered image?
- What belongs per frame, what belongs in a roll summary, and which differences
  should an ordinary text diff make obvious?
- How should the artifact state its limits when two outputs do not share a
  comparable encoding or display intent?

## How to Verify

- On nc outputs, the tool's endpoint-clipping fractions agree with the `loss.*`
  counters already in nc's report. This is the reader's falsifiability check — the
  one number that has an independent source of truth.
- The Portra 400 Dmax 1.2 and 1.9 runs come out visibly different in tone, color
  and range terms, not merely in global means, and the differences agree with
  targeted visual inspection.
- Repeated runs on identical inputs produce identical artifacts; ordinary `diff`
  is useful on them; JSON reaches stdout clean enough to pipe through `jq`.
- An image with no declared color space is refused, loudly.

## Dependencies

- [Asset Manifest](asset-manifest.md)
