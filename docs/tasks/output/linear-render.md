# Linear Display Render

## Goal

Let a display render skip its tone curve, so a reconstruction that already places
every tone is not compressed a second time on the way out.

Today both display renderers apply a fixed Hermite shoulder and neither can turn it
off — `highlight_compress` only moves the knee (`0.5 + 0.25/(1+hc)`, so 0.75 at the
default and never later). Under the shipped sigmoid that means SDR is shouldered
**twice**: once in density space by the curve, once again in linear display space
from 0.75 upward.

## Why

`algo/sigmoid` guarantees that with `shoulder > 0` its stage-3 output is **≤ 1.0 by
construction** for any finite density. When that holds, the display shoulder has
nothing left to protect against — it is compressing values that were already rolled
off, in exactly the highlight region a user notices. Visual review on 2026-08-27
found highlight separation the weakest axis of the shipped default.

Measured context from `algo/exponential-anchor-placement`: `highlight_compress` made
every config tested slightly *worse* (shipped default 6.45% → 6.64% blown at `hc=1`),
and could not rescue a shoulder-less reconstruction at all (21.4% → 21.6%). A knob
that only costs is a sign the stage should be skippable rather than tuned.

## Design

Not predetermined. What is known:

- **No curve-type gate is needed, and one would be wrong.** The ≤1.0 guarantee is a
  property of `shoulder > 0` plus neutral print gains, not of "the curve is sigmoid" —
  `shoulder = 0` reduces to the straight line and `print_exposure` can lift samples
  back above 1.0. `sdr::render` already **errors** on any sample outside `[0, 1]`, so
  a linear render is self-policing: misuse fails loudly instead of clipping quietly.
  Test the real condition, not a proxy for it.
- It is a **render**-stage choice, so it belongs with the display presets and must not
  reach `film-master`, which bypasses the stage entirely.
- HDR's shoulder starts at 3.94x and never fires under a bounded reconstruction, so
  the SDR side is where this changes anything today.

## Open questions

- **Surface.** A new preset, a `print` key, or an extension of `highlight_compress`
  (e.g. an explicit "off")? The last keeps one concept in one place, but
  `highlight_compress` is a *width* knob and "off" is not a width.
- **Does it actually improve the picture?** The double-compression argument predicts
  more highlight separation; that is a prediction, not a measurement. Verify on the
  `scripts/sigmoid-baseline` fixtures with the `blown%` / `code sep` pair — and note
  `sat%` is unusable for this comparison (see that harness's cautions).
- Whether the gamut mapping and transfer encode, which stay in either case, make
  "linear render" a misleading name for what this actually is.

## How to Verify

- With a bounded reconstruction the output differs from the shouldered render only
  above the knee, and highlight separation measurably improves.
- Pairing it with a reconstruction that exceeds 1.0 fails loudly rather than clipping.
- `film-master` is unaffected.
- Full CI gate passes; any default change carries a `pipeline_version` bump.

## Dependencies

- [SDR display rendering](sdr-display-rendering.md)
