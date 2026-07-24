# Conversion Metrics & Thumbnails

## Goal

Formalize the ad-hoc image-library analysis from real-scan verification into the
reusable Python toolkit that is the toolkit's single documented entry point.
Produce a per-image metric set (percentiles, black/white points, contrast,
saturation, clip %) as pipeable JSON + a Markdown summary, plus downscaled
thumbnails — computed over the manifest's frames and converted outputs, always
respecting the "no full-res pixels in agent context" invariant.

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
