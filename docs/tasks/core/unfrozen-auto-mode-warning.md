# Warn when a recipe's auto modes silently defeat a roll

## Goal

Say something when a recipe applied to a roll still re-measures per frame. A
recipe carrying `dmax: "auto"`, an auto `white_balance`, or an `auto`
`balance_range` with a non-neutral balance produces a *different* calibration on
every frame — the exact inconsistency `roll` exists to prevent — and today nothing
warns.

## The gap, measured

2026-08-11, one run with `--base-region … --auto-wb percentile --auto-d-max`:

```text
report:   film_base {0.16312, 0.08011, 0.03772}   dmax 0.58147   wb [1.22832, 1.0, 0.72115]
recipe:   film_base.source {"region": […]}        dmax "auto"    white_balance "percentile"
```

The report holds measurements; the recipe holds *modes*. Applying that recipe to a
roll re-derives all three per frame, and the only warning emitted was an
incidental region-uniformity note.

`roll` already warns when the film base is not `explicit`, so the precedent and
the plumbing exist — `dmax` and white balance are the same hazard and are silent.

## Open questions

1. **Warn at apply time, author time, or both?** It bites on `roll`. But
   `core/profile-authoring` validates without an image and could flag it earlier,
   where it is cheaper to fix.
2. **Is `convert` affected?** A single frame has no consistency to break, so
   probably not — which means this is a roll-scoped rule, unlike most of
   `validate`.
3. **Which modes qualify?** `dmax: "auto"` and the auto white-balance modes
   clearly. `balance_range: "auto"` only matters when a balance is non-zero
   (`density::consults_balance_range`). A `{"region": …}` film base is subtler:
   it re-samples the *same rectangle* on every frame, which is stable only if
   that rectangle is rebate on every frame.
4. **Warning or `--strict`-only?** These are legitimate deliberate choices for
   someone grading rather than converting, so a hard error is wrong.

## How to Verify

- A roll whose recipe carries `dmax: "auto"` warns, naming the field and what it
  means for consistency.
- The same recipe under `convert` does whatever question 2 decides — asserted
  either way, not incidental.
- A fully explicit recipe emits nothing (the falsifiable control).
- `balance_range: "auto"` with neutral balances emits nothing; with a
  non-neutral balance it warns.
- `--strict` promotes it, and the existing not-explicit-base warning is not
  duplicated for the same frame.

## Dependencies

- [Roll conversion](roll-conversion.md)

Related: [Layered recipe composition](recipe-composition.md) makes it easy to
compose a calibration layer that *removes* the hazard, and
[profile authoring](profile-authoring.md) is the natural earlier place to surface
it. Neither is required to ship this.
