# Validate reference frames by tiling, for both Dmin and Dmax

## Goal

Replace the 5-cell grid with a **coarse tiling** computed in the same pass as the
estimate, reporting *within-tile* and *between-tile* variation separately — so
"uniformly grainy" and "sloped across the frame" stop looking alike. Apply it to
`Dmax` as well as `Dmin`, which has no such check today.

This is diagnostics only: it changes warnings and the report, **not** the
estimate, so it owes no `pipeline_version` bump.

## Why the current check cannot answer the question

`--grid` compares five fixed patches by `(max − min)`, which is driven entirely by
the two extreme cells. It cannot distinguish a smooth gradient from one bad patch,
and it says nothing about how much of the spread is simply grain.

Decomposing does answer it. Measured on two leaders (2026-08-11, 4x4 tiles over
the interior):

| Roll | between-tile spread | within-tile p05–p95 | ratio |
|---|---|---|---|
| Gold 200 | **0.0081** (0.027 stops) | 0.0830 | 10 : 1 |
| Portra 160 | **0.0390** (0.13 stops) | 0.0887 | 2.3 : 1 |

Same scanner, ~5× difference in genuine spatial structure — and it independently
reproduces the baseline report's finding that Portra 160's leader is the least
uniform of the set, with blue drifting 0.048 left-to-right. A pooled percentile
cannot see this, and five patches cannot characterise it. That the leader analysis
in `reports/sigmoid-reference-baseline.md` had to be done by a bespoke script is
the evidence this belongs in `estimate`.

## What this absorbs and retires

- **`--grid` should be retired.** Once masking and a central estimator are
  unconditional, grid no longer selects an estimator, and the tiling runs for free
  in the pass the estimate already makes. A flag that no longer changes the
  estimate would be exactly the silently-ignored knob the project forbids.
- **`film-base/grid-verdict-enum` is removed with it** — that task exists to give
  `GridEstimate.agreement` a self-describing verdict. Its *intent* carries over
  here: report a verdict, not a bool plus an overloaded spread sentinel.

## Open questions

1. **Tile count.** Enough tiles to localise a defect, few enough that each keeps a
   usable sample. 4x4 was adequate to separate the two rolls above; that is not
   the same as being right.
2. **Thresholds.** 0.0081 and 0.0390 bracket "fine" and "suspect" on two rolls,
   with one scanner, over interior boxes only. Setting a number needs more rolls,
   and `film-base/dmax-anchor-reliability` already owns the leader's
   trustworthiness — coordinate rather than deciding twice.
3. **What the verdict enumerates.** At least: uniform; grain-dominated but sloped;
   localised outlier tile. Say which are actionable and which are informational.
4. **Does it warn, or can it refuse under `--strict`?** `estimate --strict` already
   exists to stop a bad base being baked into a recipe.
5. **Same shape for `Dmin` and `Dmax`?** Probably, but their populations differ —
   the base frame should be flat, a leader is allowed some falloff.

## How to Verify

- The two rolls above reproduce their between/within figures, and a threshold
  placed between them flags Portra 160 and not Gold 200.
- A synthetic frame with pure per-pixel noise and no gradient reports large
  within-tile and near-zero between-tile — the decomposition's whole point.
- A synthetic frame with a smooth ramp reports the inverse.
- A single bad tile is localised in the report, not just summed into a spread.
- `--grid` is gone: the flag, its clap conflicts, `GridEstimate`, and the tests
  that pinned it. No path silently accepts it.
- Running on a masked region, frame corners no longer raise a false "light leak"
  on a holder-mounted scan — the failure mode the old full-frame grid had.

## Dependencies

- [Mask the holder, then estimate from a single population](holder-masked-measurement.md)

Coordinate with [Dmax anchor reliability](dmax-anchor-reliability.md), which owns
whether a leader-measured anchor can be trusted at all and is the right place for
the thresholds this task surfaces.
