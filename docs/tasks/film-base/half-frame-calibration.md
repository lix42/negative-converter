# Calibrate from a single part-exposed frame

## Goal

Let one frame that is *partly* unexposed and *partly* fully exposed serve as both
calibration references, instead of requiring two separate frames.

**Deferred — blocks nothing.** `core/base-acquisition-planner` handles the normal
case of one frame per reference; this is a convenience for the roll where the
photographer produced a single transitional frame instead.

## Why it comes up

Real rolls carry them. `20260808-film-1330` in the Ilford HP5 set is half leader,
half unexposed, and its IR statistics show the split plainly — the interior median
sits at 0.4620 (transparent, unexposed film) while the border p05 is 0.0194 and
the interior p05 is 0.0202, i.e. an opaque population coexisting with a
transparent one in the same frame.

Today that frame is usable for neither measurement without hand-picked regions,
because `calibrate` resolves one reference per input file.

## Open questions

1. **How are the two zones found?** Hand-specified regions are the obvious first
   answer and probably sufficient. Automatic segmentation is tempting on an IR
   scan — the two populations are ~20:1 apart on chromogenic and on unexposed
   silver — but the boundary is a gradient on a real transitional frame, and a
   misplaced boundary contaminates *both* measurements at once.
2. **Is a transitional frame trustworthy for `Dmax` at all?** The exposed half of
   a transition is where exposure was still ramping, so it may not be the film's
   maximum. `film-base/dmax-anchor-reliability` already questions whether a
   *dedicated* leader can be trusted; a half-leader is strictly weaker evidence.
   That may be the real answer here: support it, and say loudly that it is the
   least reliable tier.
3. **Does this deserve its own CLI shape** (`--unexposed f.tif:REGION`), or is it
   just the existing region flags pointed at one file twice?

## How to Verify

- Frame 1330 yields both a film base and a `dmax`, and the base agrees with the
  one measured from the dedicated unexposed frame 1364 to within the estimator's
  reproducibility.
- The `dmax` from 1330's exposed half is reported with whatever reliability
  signal question 2 settles on — it must not be presented as equivalent to a
  dedicated leader.
- Pointing both references at the *same* region fails loudly rather than
  producing a base and a `dmax` from identical pixels.

## Dependencies

- [Base-acquisition planner](../core/base-acquisition-planner.md) — owns the
  one-reference-per-frame path this extends

Coordinate with [Dmax anchor reliability](dmax-anchor-reliability.md), which owns
how far any leader-derived anchor can be trusted.
