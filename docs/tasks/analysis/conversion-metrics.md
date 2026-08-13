# Conversion Metrics & Photographic Analysis

## Background

The first `nctool roll analyze` artifact normalizes conversion provenance and the
limited aggregate statistics already present in `nc roll` reports. Comparing the
Portra 400 Dmax 1.2 and 1.9 runs showed that this is not enough to understand the
result as an image: channel means reveal a broad brightness change, but not how
color and tone are distributed, whether highlights or shadows occupy useful
range, how much of the frame lies in those regions, or how closely the result
approaches the black and white limits.

## Goal

Formalize the ad-hoc image-library analysis from real-scan verification into the
reusable Python toolkit that is the toolkit's single documented entry point.
Make converted-roll analysis describe color, tone, highlights, shadows, range
use, black/white-limit behavior, and the amount of image area represented by
those conditions through deterministic per-image and per-roll artifacts, while
keeping objective measurement separate from aesthetic judgment. Retain the
original pipeable JSON, Markdown summary, and downscaled-thumbnail goals, always
respecting the "no full-res pixels in agent context" invariant.

## Opening Questions

- Which color and tone domains make the results meaningful across the supported
  SDR, HDR, integer, and floating-point outputs?
- What should “shadow,” “highlight,” “near black,” and “near white” mean for this
  purpose, and which definitions remain comparable across configurations?
- Which summaries best distinguish a healthy use of range from clipping,
  compression, lifted blacks, empty highlights, or a narrowly clustered image?
- How should per-channel behavior, perceptual brightness, chroma, neutrality,
  and color casts be represented without implying a subjective quality score?
- What belongs in each frame's analysis, what belongs in the roll summary, and
  which differences should a normal text diff make easy to see?
- How should analysis state its limits when two outputs do not share a directly
  comparable encoding or display intent?

## Suggested Direction

Begin with the photographic questions the artifact must answer, then select and
validate measurements against representative converted rolls and deliberately
different configurations. Retain the existing deterministic, diff-friendly
provenance from roll analysis. Keep the result factual and inspectable: derived
measurements can support visual review, but should not be presented as an
automatic verdict on which rendering looks better.

## Design

A Python package `scripts/analysis/nctool/` with an isolated venv +
`requirements.txt` (numpy, tifffile, Pillow — none are installed system-wide;
system Python is 3.14). It **subsumes** the `real-scan-verify` role: `nctool` is
the single entry point (`python -m nctool …`) and can drive `nc` for
conversion/verification too (determinism byte-compare via `hashlib`, peak RSS via
`/usr/bin/time` or `resource`). The existing frozen `recipes/` stay as-is;
`harness.sh` is retired or reduced to a thin shim that calls `nctool`.

Package layout:

```
scripts/analysis/
  README.md            # setup (venv), entry point, invariants
  requirements.txt
  manifest.sample.json # committed schema sample (live manifest.json lives at the assets root, not here)
  generate_manifest.py # precursor manifest generator (asset-manifest task)
  nctool/
    __init__.py
    manifest.py        # load/generate/validate (asset-manifest task)
    metrics.py         # per-image metrics from a tifffile array
    thumbs.py          # downscaled preview + contact-sheet generation
    cli.py             # `python -m nctool {manifest,metrics,thumbs,...}`
```

Metric set (per image, one image in memory at a time):
- Per-channel percentiles p0.1/1/5/50/95/99/99.9, plus min/max.
- Black point / white point (robust near-min / near-max).
- Contrast: log-domain dynamic range and p99−p1 spread.
- Saturation: median chroma in a defined space (document which).
- Clip %: fraction at/above white and at/below black, per channel + overall.
- Non-finite (NaN/Inf) count — meaningful for the float HDR outputs.
- dtype, bit depth, channel count, dimensions.

Output: metrics → JSON on stdout (clean, pipeable; logs to stderr, mirroring the
`nc` report contract) + a Markdown table generator for `docs/reports/`.
Thumbnails: ≤512 px long-edge PNG per image, written to an output dir; these
downscaled previews are the **only** pixel-derived artifacts the tool emits, and
are never read back into an agent context.

## Implementation Suggestion

- Read TIFFs with `tifffile`; handle both 16-bit integer and 32-bit float
  outputs and the multi-IFD HDRi source layout (analyze the RGB IFD; the IR plane
  is out of scope here).
- Process strictly one image at a time and free arrays between images — the
  target machine is an 8 GB Air; a single 18 MP float frame is ~224 MB.
- Keep metric definitions in one place with docstrings stating the value domain
  (see design-spec §4 terminology) so the numbers are interpretable later.
- Percentiles are cheap on the downscale for thumbnails but must be computed on
  **full-res** pixels for the metric JSON — don't silently sample.

## How to Verify

- Use known same-roll configuration pairs, including the Portra 400 Dmax 1.2 and
  1.9 runs, and confirm the analysis exposes meaningful color, tone, range,
  shadow, highlight, and endpoint differences rather than only a change in
  global means.
- Check representative SDR and HDR outputs, limit-reaching and non-clipped
  cases, and repeated analysis of identical inputs. The artifacts should remain
  deterministic and useful with ordinary diff tools, and their claims should
  agree with targeted visual inspection.
- `python -m nctool metrics <converted-frame>` emits valid JSON whose clip % on
  the `2026-07-22` outputs matches the per-frame clip % already recorded in
  `docs/reports/real-scan-verification.md` (u16 clips 4.8–10.3 %, float 0).
- Float HDR outputs report 0 non-finite; 16-bit outputs report the expected
  high-clip fraction.
- Thumbnails are generated at the target size for every manifest frame; a
  contact sheet renders for a roll.
- Metric JSON round-trips through `jq` cleanly (stdout uncontaminated by logs).

## Dependencies

- [Asset Manifest](asset-manifest.md)
