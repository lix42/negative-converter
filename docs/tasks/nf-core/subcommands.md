# `roll`, `inspect` and `estimate` under the new chain

## Goal

Carry the three non-`convert` surfaces across the flow boundary. The migration is
sequenced around `convert`; these three reach the same stages by other paths, and
nothing in the plan owns them.

## Design

- **`roll` resolves defaults by hand, and a new decode reopens that trap.** Its
  planner merges the per-frame overlay onto the **serialized** shared config, so
  every key is present and deserialize-time resolution cannot fire — which is why
  `density.scale` used to be re-resolved there in code, per curve, until
  `nf-retire/characteristic` left one curve and one default. Any new-flow default that keys off a key's *presence* breaks there silently, on a
  whole roll rather than one frame.
- **Roll's per-frame recipe** (`nf-core/recipe-schema`, 2026-09-22): each `--new-flow`
  overlay is merged onto the serialized new `Recipe`, checked, and carried on
  `PlannedFrame` to that frame's render, so a per-frame decode override reaches the
  pixels (pinned by `roll_refuses_the_current_chains_keys_from_either_recipe_site`).
- **The new chain's per-frame overrides get no roll-consistency warning.** The current
  chain warns (`--strict`-promotable) when an override sets `calibration.*`,
  `reconstruction.curve.anchor`, `curve.stock` or `output.preset`. Their new-chain
  counterparts — `reconstruction.anchor` and `reconstruction.linearization`, a frame pinned
  on a different rule or slope than its roll — are probed by nothing, so such a frame
  would render differently from its siblings in silence once the seam opens. Decide
  which of the new sections are roll-fixed and warn on them.
- **`inspect` reports the resolved `dmax` and always runs the base detector.**
  [Retiring the anchor machinery](../nf-retire/dmax-machinery.md) takes the first;
  the rest of `inspect` — film base, effective area, IR separability, holder mask —
  is a pre-chain measurement and should stay one. Say which of its fields are
  chain-dependent and which are not.
- **`estimate` keeps its film-base half** while `--d-max-region` retires. What is
  this task's rather than the retirement's is that `estimate` plus `nctool roll` is
  the calibrate-once workflow the guide documents end to end, so a removed region
  changes a *procedure*, not just a flag.
- **`measure-roll` joins that workflow on the new chain**
  (`nf-scene-correction/roll-white-balance`): the roll's white balance is measured once,
  after the base, and frozen into the recipe as `scene_correction.white_balance`.
  `nctool roll` calibrates with `estimate` alone today and renders on the current chain;
  when it gains the new chain, its calibrate step runs `measure-roll` too.
- **The error surface is a contract.** Retirement adds a class of removed-flag and
  removed-value errors, and design-spec §11 says which exit code each is. A rejected
  frame inside `roll` follows roll's own handling — recorded in the report entry,
  siblings still written, roll exits 1, not the frame's code — which is how the
  memory gate's exit 6 already behaves.
- Every command here decodes, so every one runs `memory::preflight` against its own
  `RunProfile`; `inspect`/`estimate` peak at the film-base phase rather than encode.
  Which phase peaks is per profile and measured, never inherited.

## Open questions

- ~~Does `roll` accept `--new-flow` during the migration?~~ **Settled 2026-09-20:**
  it does ([the flag task](new-flow-flag.md)). Roll takes no conversion flags, so it
  reaches only the resolved-value half of the availability refusal, at its two
  validate sites (shared recipe, per-frame overlay). While the new chain has no
  stages, roll refuses once after the plan resolves rather than per frame.
- **Does `inspect` gain anything from the new chain** — a reported reconstruction
  fact — or stay strictly a pre-chain tool?

## How to Verify

- A roll under the new flow with a per-frame override resolves the same config the
  equivalent single `convert` does, tested through the real path rather than by
  calling the resolver.
- `inspect` and `estimate` run on a committed fixture with their film-base fields
  unchanged, and neither help nor report names a retired region.
- Every removed flag and value produces the §11 code the spec assigns; a roll frame
  refused for any reason still exits 1 with its siblings written.
- The four CI gates pass.

## Dependencies

- [A minimal end-to-end render](minimal-end-to-end.md)
