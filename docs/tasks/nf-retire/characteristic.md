# Retire the `characteristic` curve path

## Goal

Remove the per-stock characteristic curve from reconstruction, so that the claim
the other retirements make — the fixed decode is the only reconstruction — is
actually true.

## Design

- **What survives unowned today.** [Retire the sigmoid and `simple`](sigmoid-and-simple.md)
  removes two of the three `DensityCurve` members; the third, with
  `CharacteristicParams`, the curve-inversion path in `src/algo/characteristic.rs`,
  `--film-stock`, three `ConversionPreset` names (`characteristic-generic`,
  `-stock`, `-aim`) and `DensityParams::default_scale_for`'s `[1, 1, 1]` arm, is
  nobody's. This task is that third member.
- **Collapsing the curve set collapses `default_scale_for`.** `DensityParams::default_scale_for` records its
  per-curve default being resolved in **three** places — the recipe's `Deserialize`
  (off raw-JSON key *presence*), the `--density-curve` merge arm (which must stay
  before the `--density-scale` arm), and the `roll` planner by hand, because the
  overlay is merged onto the *serialized* shared config. With one curve there is no
  per-curve answer left: decide whether `density.scale` keeps a plain default and
  delete the machinery, rather than leaving a one-armed match and a roll warning
  about a reset that can no longer happen.
- **The stock *data* is not this task's to delete.**
  [A home for the film-stock data](../nf-look/stock-data-home.md) settled it
  (2026-09-24): the tables, registry and `docs/datasheets/` stay as the evidence for
  the decode's constants, in `src/film_stock/`. The inversion was split out for this
  task into `src/algo/characteristic.rs` (`invert`, `apply_curve`, `OutOfTable`,
  `check_tables`, `aim_red_scale`, and the tests of each), so it goes **whole** —
  with its test-only readers, `algo::curve_probe` and `pipeline::stages`' characteristic
  tests, which import `invert` directly. What
  is left then: make `film_stock` `#[cfg(test)]` (its tests read the tables forward,
  so they survive), drop `types`' re-export of `FilmStock` with its recipe use, and
  drop the `{film_stock}` placeholder from `nctool review` and the preset-review
  matrix.
- **`--film-stock` leaves with the curve** — decided there, not kept as provenance
  (`--film-type` already records chemistry). Per-stock normalization will bring its
  own flag; update the `--new-flow` refusal row in `flow.rs` to match whatever this
  task makes of the flag.
- **Retiring a curve is mostly deleting conditions.** `DensityCurve::consumes_reference`,
  `cli::unconsumed_dmax_warning` and the `--film-stock` required-here/refused-there pair
  all exist to tell this curve from the parametric ones; each deletion is a rule that can
  no longer be ordered wrongly. (`takes_dmax()` and `cli::preset_curve`'s
  carry-`dmax`-across rule were the same shape and went in
  `core/calibration-recipe-section`, which moved the reference out of the curve.)
- Removed preset names and the removed curve value get migration errors on the
  `--algorithm` precedent — no aliases. Note `characteristic-generic` was
  `algo/split-default-migration`'s target, so any doc calling it the next default is
  stale.

## Open questions

- Does the decode keep a `density.scale` default at all once it is not per-curve?

## Outcome (2026-09-26, done)

Scope grew by three decisions taken with the user before starting (progress log):
`--preset` retires outright rather than losing only its names (`nf-look/look-presets`
closed as "retired, not rebuilt"); `DensityCurve` collapses to `ExponentialParams`, the
wire's `curve.type = "exponential"` dropped on load; and the one-valued surfaces go —
`--density-curve` at every value, telemetry `conversion.curve` (schema 7), the report's
curve `type`, `stock` and `out_of_table`.

- **The open question:** `density.scale` keeps one plain default, `fixed::DENSITY_SCALE`.
  All three per-curve resolution sites are gone, and with them roll's curve-switch reset
  and its warnings.
- **No pixel moved.** `render` and `base` reproduced; `recipe` refreshed in place on the
  v7 row. `render`/`base` held because the default was already the exponential.
- **Deleted beyond the file:** `algo/curve_probe.rs` whole, per the plan — including four
  scalar-path probes (`channel_drift`, `sigmoid_scale`, `whole_roll_*`) that the open
  `io/scanner-density-calibration` cites; it now points at them in git.
  `scripts/preset-review/` and `nctool review`'s `{film_stock}` placeholder and `rolls`
  block went too.

## How to Verify

- `reconstruction.curve` accepts only the fixed decode; a recipe naming
  `characteristic`, and each of the three preset names, fails with a message naming
  the replacement.
- Nothing resolves a per-curve `density.scale`: all three sites are gone or reduced
  to one unconditional default, proven through the real path (`merge`, the binary, a
  roll with a per-frame override), not by calling the resolver.
- No error message or help text recommends a flag or preset that no longer exists.
- The four CI gates pass.

## Dependencies

- [Retire the sigmoid and `simple`](sigmoid-and-simple.md)
- [A home for the film-stock data](../nf-look/stock-data-home.md)
