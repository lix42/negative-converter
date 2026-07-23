# Simple Inversion Algorithm

> **Target-pipeline note:** this completed task records the shipped renderer.
> The replacement film-preserving pipeline stops reconstruction at raw
> unclamped `1 - scan/Dmin`, maps the resulting `FilmRgbImage` through NC film
> RGB v1, and moves WB and clip-range controls to downstream shared
> `print.white_balance` and `print.linear_range` placement. See
> `negative-reconstruction-density-curves`, `film-rgb-working-space`, and
> `film-master-render-pipeline`.

## Goal

Implement the `simple` converter: channel inversion plus white balance / black-
white-point handling. It's the debugging reference and the reasonable path for
B&W edge cases — predictable and cheap, not a strong color endpoint.

## Design

`algo/simple.rs`, implementing `Converter`:

```rust
pub struct Simple { pub params: SimpleParams }

// SimpleParams: invert_white_balance: [f32;3], clip_low: f32, clip_high: f32
impl Converter for Simple {
    fn convert(&self, img: &LinearImage, base: &FilmBase) -> Result<LinearImage, NcError> {
        // 1. optional normalize against base (neutralize film base)
        // 2. positive = 1.0 - value   (per channel, in linear)
        // 3. apply per-channel white-balance gains
        // 4. apply clip_low / clip_high to set black/white points
    }
}
```

- Operate in the `f32` linear working space; preserve the IR plane untouched.
- White balance is a simple per-channel multiply.
- Black/white points (`clip_low`/`clip_high`) remap the range linearly.
- Keep it deterministic and allocation-light (process the RGB buffer in place or
  into one new buffer).

## Implementation Suggestion

- Parallelize the per-pixel loop with `rayon` if it helps, but correctness first.
- For B&W scans the three channels are ~equal; the same path works, white balance
  near 1.0.
- Don't fold density math in here — that's what distinguishes `density`. This task
  stays a literal inversion so it's a trustworthy reference.

## How to Verify

- A known linear input inverts correctly: pixel `v` → `1 - v` (before WB/clip).
- White-balance gains scale channels as expected.
- `clip_low`/`clip_high` map endpoints correctly (e.g. low→0, high→1).
- IR plane passes through unchanged.
- Unit tests on small synthetic images cover inversion, WB, and clipping.

## Dependencies

- [Algorithm interface](algo-interface.md)
