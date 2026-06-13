# Density-Domain Algorithm

## Goal

Implement the `density` converter — the default for color negatives — following
Kodak Cineon / darktable `negadoctor` ideas: convert transmission to density,
correct in density space (orange-mask compensation), then render to a positive
through a print-like tone curve. Density conversion and print rendering are kept
as separate sub-stages.

## Design

`algo/density.rs`, implementing `Converter`. Sub-stages:

```text
1. transmission → density:  D = -log10(scan / base)       (per channel, base = FilmBase)
2. density correction:      D' = D * density_scale + density_offset   (orange-mask comp)
3. map density → positive:  linear = 10^(-D' * density_gamma)   (print back-transform)
4. print render:            apply print_exposure, black_point, white_balance, highlight_compress
```

```rust
pub struct Density { pub density: DensityParams, pub print: PrintParams }
// DensityParams: scale:[f32;3], offset:[f32;3], gamma:f32
// PrintParams:   exposure:f32, black_point:f32, white_balance:[f32;3], highlight_compress:f32
```

- Stage 1 anchors on the passed-in `FilmBase` (the `Dmin` estimate). Guard against
  division by zero / log of non-positive (clamp scan to a small epsilon).
- Stages 1–2 (density) and 3–4 (print) must be cleanly separable functions so each
  is independently testable — this is the core fidelity rule from the design.
- Highlight compression is a soft roll-off near the top of the range.
- Operate in `f32` linear; preserve the IR plane untouched.

## Implementation Suggestion

- Implement `to_density(img, base) -> DensityImage` and
  `render(density, params) -> LinearImage` as separate pure fns; the `Converter`
  impl just composes them.
- Pick concrete, documented formulas for orange-mask offset and highlight
  compression; record the exact equations chosen in `progress.md` so they're
  reproducible and tunable.
- Default params should yield a reasonable neutral conversion on a typical color
  negative; expose every constant as a param (no magic numbers baked in).
- `rayon` for the per-pixel passes once correct.

## How to Verify

- `to_density` on a known transmission/base yields the expected `-log10` density
  per channel; epsilon clamp prevents NaN/Inf on zero/negative input.
- `render` maps a known density back through the curve to the expected linear
  value for given gamma/exposure/black.
- End-to-end on a real color negative (via orchestration later) produces a
  plausible positive with neutral grays and no channel blow-outs.
- Sub-stage unit tests pass; the two stages are callable independently.

## Dependencies

- [Algorithm interface](algo-interface.md)
