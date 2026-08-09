# Mid-Grey Anchor for the Exponential Curve

## Goal

Give the exponential density curve an anchor-placement rule, as the sigmoid has, so
its contrast knob stops fighting its white placement.

Today the exponential renders `10^(γ·(D′ − Dmax))`: display white is pinned at
`Dmax` and there is **no** placement rule, so raising `γ` pivots the line *around
white* and drags everything below it down. `docs/reports/sigmoid-reference-baseline.md`
measured that trade on user-confirmed shadow patches: `γ = 2.0` takes the black floor
from 72/255 to 12/255 — the "pale, not dark" defect it was raised for — while costing
**2.75 EV of midtone placement**. Both effects come from the same pivot; one knob
cannot deliver the first without the second.

The sigmoid does not have this problem because `AnchorPlacement` lets it pin
**mid-grey** instead of white (`types::AnchorPlacement::MidAtDmaxFraction`, default
`0.5`), so contrast rotates the curve about a point inside the tonal range. That is
the piece the exponential is missing.

`ExponentialParams::gamma` shipped as `2.0` on 2026-08-08 (`pipeline_version` 2)
because the floor fix is worth more than the midtone offset costs, and the docstring
on `ExponentialParams::default` says plainly that it is "the better of two imperfect
slopes, not a calibrated value". This task removes the trade rather than re-picking a
point on it.

## Design

Not predetermined. The obvious shape is to reuse the sigmoid's vocabulary rather than
invent a second one:

- Add an `anchor` field to `ExponentialParams` carrying the existing
  `AnchorPlacement` enum (`"white-at-dmax"` — today's behaviour — or
  `{"mid-at-dmax-fraction": <f>}`), so both curves answer the same question with the
  same recipe spelling and one CLI flag family covers both.
- Render `10^(γ·(D′ − A))` where `A` is the resolved anchor, exactly as the sigmoid
  derives its `A` from the reference density and the placement rule. With
  `anchor = "white-at-dmax"`, `A = Dmax` and the arithmetic is bit-identical to
  today's — that equivalence is what keeps the straight line usable as the debuggable
  reference, and it should be pinned by a golden.
- Decide the **default** placement deliberately and separately from the mechanism. A
  mid-grey default is a default-render change: it needs a `pipeline_version` bump, a
  `PIPELINE_FINGERPRINTS` row, and a measured report, the same way the sigmoid
  default did. Shipping the mechanism with `white-at-dmax` as the default and moving
  the default in a second step is a legitimate way to keep those reviewable.

Things to keep straight while doing it:

- Per CLAUDE.md, a new knob spans four coupled spots — the CLI `*Overrides` field,
  the recipe `*Params` field, a `merge` arm and a `validate` check — plus a merge
  test. `--sigmoid-mid-fraction` / `--sigmoid-white-at-d-max` currently reject a
  resolved *exponential* curve as a usage error; whatever flag surface this adds has
  to leave those cross-curve rejections coherent rather than half-lifted.
- The exponential's `dmax = "none"` case has no anchor at all, and must keep having
  none — a placement rule cannot be resolved without a reference.
- design-spec §7.3 and §9 describe `anchor` as sigmoid-only; both need updating, and
  §9's `deny_unknown_fields` structs must move with them.

## How to Verify

- With `anchor = "white-at-dmax"` the rendered pixels are **bit-identical** to the
  current exponential, pinned by a `pipeline::stages::golden` vector.
- Raising `γ` under a mid-grey anchor lowers the black floor **without** the 2.75 EV
  midtone shift, measured on the same frozen shadow patches
  `docs/reports/sigmoid-reference-baseline.md` used — the report is the acceptance
  metric, not a visual preference.
- Any change to the *default* placement carries a `pipeline_version` bump, a new
  `PIPELINE_FINGERPRINTS` row, and a measured report; a neutral-default opt-in
  refreshes only the current row's `recipe` hash.
- Full CI gate passes.

## Dependencies

- [Negative reconstruction and density curves](negative-reconstruction-density-curves.md)

Blocks nothing: the exponential is the explicit diagnostic straight line since the
sigmoid became the default curve, so this improves a non-default path.
