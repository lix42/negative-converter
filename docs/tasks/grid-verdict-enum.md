# Grid agreement verdict enum

## Goal

Replace `GridEstimate.agreement: bool` (plus the overloaded `spread` sentinel)
with a self-describing verdict enum, so `nc estimate --grid` reports *which* of
the mutually-exclusive grid outcomes occurred and the CLI stops re-deriving it
from the combined base.

## Background

`GridEstimate` (`src/pipeline/film_base.rs`) currently reports a boolean
`agreement`. A `false` verdict conflates two distinct conditions:

- the cells genuinely **disagree** — light leak, scanner illumination falloff, or
  dust (a warning; `--strict`-promotable), and
- the sample is **degenerate** — all-zero / dark, e.g. a `--grid --base-region`
  on the holder (a hard error, per the `estimate-reuse-output` fix).

The `spread` value can't disambiguate them: a degenerate all-zero channel yields
the `1.0` spread sentinel (`(max - min) / max` with `max <= 0`), and a genuine
full-range disagreement also reads ~`1.0`. So `cli::run_estimate` re-inspects the
combined `base` (`channel <= 0` ⇒ degenerate ⇒ hard error; otherwise ⇒
disagreement warning) to recover the case the estimate already knew but threw
away. That re-derivation is the smell: the estimate should name its own verdict.

## Design

Carry a verdict enum on `GridEstimate` instead of the bool:

```rust
enum GridVerdict { Uniform, Disagree, Degenerate }
```

- `estimate_grid` decides the verdict once, at the point it has the per-cell
  values: `Degenerate` when the combined base has any non-finite / `<= 0`
  channel, `Disagree` when any channel's spread exceeds `GRID_MAX_RELATIVE_SPREAD`
  (and not degenerate), else `Uniform`.
- `cli::run_estimate` matches the verdict directly — `Degenerate` ⇒ the
  hard-error path, `Disagree` ⇒ the warning path, `Uniform` ⇒ clean — deleting
  the "re-inspect the base" logic.
- Decide the JSON wire treatment deliberately: either keep an `agreement` boolean
  **derived** from the verdict for backward compatibility and add the verdict
  alongside, or replace it. This is a report-shape change — update design-spec §8
  (grid) and §9 in **both** `.md` and `.html`, and any snapshot/round-trip tests
  that pin the grid report keys. The `spread` array stays (it is the evidence);
  only its use as a *degeneracy signal* goes away.
- Model the verdict as one enum (the "one enum, not parallel flags" convention in
  `CLAUDE.md`), serialized lowercase like the other report enums.

## How to Verify

- Unit tests on `estimate_grid`: a uniform frame ⇒ `Uniform`; a single
  disagreeing channel ⇒ `Disagree`; an all-black / degenerate frame ⇒
  `Degenerate` (no longer indistinguishable from `Disagree`).
- `cli::run_estimate` no longer reads the combined base to classify the failure;
  the e2e exit-code behavior is unchanged (disagreement ⇒ warning / `--strict`
  fails; degenerate ⇒ exit 1 regardless of `--strict`).
- The grid report round-trips / snapshots pass with the documented wire shape.

## Dependencies

- [Reuse-ready `nc estimate` output](estimate-reuse-output.md)
- [Film-base / Dmin estimation](film-base-estimation.md)
