# Re-express the `--preset` bundles — retired, not rebuilt

## Goal

Decide what `--preset` means under the new flow. The five shipped bundles each
name a reconstruction curve that is retiring and carry an exposure calibrated to
the old chain, so none of them survives unchanged; the likely answer is that
`--preset` becomes a *look* preset.

## Design

- Today a preset sets four knobs — curve, `density.scale`, `print_exposure`,
  `display_tone` (`cli::ConversionPreset`, five names). Under the new flow the
  decode is fixed and stock-agnostic, so three of those either no longer vary or
  belong to another stage. What carries over is the *mechanism*: a CLI-only
  expansion with no recipe key, sitting `defaults < params < preset < flags`.
- **The numbers do not carry.** Each preset's exposure was solved so its frames
  land at a common mid-grey under the old chain; with a new anchor convention and
  a new fit range those solves are meaningless, and re-deriving them is
  calibration work.
- **A look-only preset is atomic in a way the old bundles were not**: it cannot
  reach into the decode, which removes their special-case handling around
  `film-master` (which still refuses a look at all — see the stage task).
- **The `characteristic-*` names have no referent** once `characteristic` leaves
  reconstruction. [stock-data-home](stock-data-home.md) answered what returns: an
  optional per-stock normalization in the look, planned but unscheduled, with its own
  spelling — not the preset names.
- **A preset does not set `look.contrast`** (`nf-look/contrast`, 2026-09-24).
  `nf-calibration/anchor-comparison` chose a per-roll contrast (2026-09-25), which
  `nf-calibration/roll-white-rule` has `measure-roll` compute: that value is the roll's,
  carried in its recipe, and a preset setting the knob would silently overwrite it.
- **It must work on a roll.** `docs/tasks/core/recipe-composition.md` adds
  `--preset` to `roll`'s override surface — the only way to apply a look to a roll
  without authoring a file. Whatever this becomes keeps that precedence position.

## Open questions

- How many presets, what they are named after, and whether the default is "no
  look" or a named one.
- Whether a preset name is provenance in the report only, or a recipe key that
  re-expands — the old task chose CLI-only; re-check the reasoning, don't inherit
  it.

## Outcome (2026-09-25): `--preset` retires

Decided with the user and carried out by `nf-retire/characteristic`, whose deletion of
the last three names left the flag with nothing to name. Both open questions resolve to
"no preset":

- **Nothing is coupled any more.** The bundles existed because their numbers were
  meaningless apart (an exposure solved per reconstruction). With a fixed decode the look
  has three knobs and none bundles: `look.contrast` is the roll's (a preset may not set
  it), `look.channel_grade` corrects the roll's own crossover, and
  `look.highlight_desaturation` is one number, on by default. A look preset would be one
  or two independent flags under a name.
- **Layered `--params` already names a look** (`core/recipe-composition`): a partial
  recipe holding `look` is a user-owned, versioned bundle of ordinary recipe keys that
  works on `roll`, with no calibration constants in code. That also removes the reason
  `recipe-composition` had for carrying `--preset` onto `roll`.
- Per-stock normalization brings its own flag (`stock-data-home`); the "direct"
  rendering is a destination's (`nf-destinations/direct-preset`).

`--preset` is a hidden migration error at every value, on both chains; the expansion
layer, the report's `conversion_preset` block and the `defaults < params < preset <
flags` layer are gone. Curated looks, if ever wanted, can ship as example `--params`
files in `docs/using-nc.md` with no CLI surface. Do not reuse the name `--preset` for
something else: an old command line should keep meeting its migration message.

## How to Verify

- Every shipped name either resolves to look knobs only, or errors as a removed
  flag value naming what replaced it — no name silently changes meaning.
- The expansion is visible to the user (whatever succeeds `--dump-params`), and a
  recipe naming a preset is still rejected if the CLI-only rule is kept.
- A preset applied to a roll reaches every frame, and a per-frame flag still wins.

## Dependencies

- [The print-contrast knob](contrast.md)
- [A per-channel grade with a mid-grey pivot](per-channel-grade.md)
