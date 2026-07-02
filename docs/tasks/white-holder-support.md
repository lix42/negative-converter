# Light film holder support

## Goal

Let film-base auto/border detection work when the film holder is **white** (light)
rather than the assumed dark surround, via an explicit CLI/recipe control.

## Background

Auto detection assumes the frame is surrounded by a near-black holder and that the
unexposed rebate is the bright band just inside it (see `auto-base-redesign`). Some
holders are white/light, which inverts that assumption — a bright surround would be
mistaken for the rebate. The polarity can't always be inferred reliably, so make
it an explicit knob.

## Design

Add a single mutually-exclusive knob (default `black`):

- CLI: `--holder white|black`
- Recipe key: `film_base.holder` (extends the `film_base` section; keep the
  `deny_unknown_fields` contract and the flag↔recipe↔merge↔validate wiring noted
  in `cli-framework`).

Thread it into the auto detector so "holder" is classified by the configured
polarity (dark vs light surround) while the rebate remains the bright, uniform,
inset band. Only affects `auto`; `region`/`explicit` are unaffected.

## How to Verify

- `--holder white` on a synthetic light-holder `holder → rebate → picture` image
  finds the rebate; the default (`black`) would misfire on it.
- Round-trips through the recipe (`film_base.holder`) and rejects unknown values.
- Default behavior is unchanged for dark-holder scans.

## Dependencies

- [Robust auto film-base detection](auto-base-redesign.md)
