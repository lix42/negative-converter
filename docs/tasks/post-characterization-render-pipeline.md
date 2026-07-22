# Post-Characterization Render Pipeline

## Goal

Refactor the current combined reconstruction/render path so characterized linear
ACEScg is the sole source for named scene-master and display outputs, with a
typed direct master branch and one shared display-adjustment API.

## Design

Negative reconstruction ends at scanner/film-dependent positive RGB. The runtime
characterization task maps that into linear ACEScg. This task then establishes:

```text
placed f32 linear ACEScg ──┬→ scene-master encode (no print/display adjustments)
                          └→ shared WB/exposure/black/white placement
                             ├→ SDR display renderer
                             └→ HDR display renderer
```

Move white balance, exposure, black/white placement, and highlight compression out of
the current algorithm renderer. Film-domain Dmin normalization, density
scale/offset/gamma, regional balance, and inversion/film curve remain before
characterization. The characterization runtime owns density's fused extended-
range artifact evaluation plus scalar Dmax exposure placement and returns only
placed `f32` ACEScg. For sigmoid, Dmax remains inside its nonlinear canonical
reconstruction and is constrained by the v1 artifact; simple has no Dmax. This is
an explicit pipeline refactor from current code. This task starts at that typed
placed boundary. For simple, the current `simple.invert_white_balance` and
`simple.clip_low`/`simple.clip_high` operations must also move out of canonical
reconstruction. Target presets use existing `print.white_balance` and add
`print.linear_range = [low, high]` (CLI `--linear-range LOW,HIGH`, default
`[0,1]`) for the exact affine `(x-low)/(high-low)` placement. The old simple
flags/keys are warned migration aliases: inversion WB maps to explicit
`print.white_balance`, and clip endpoints map to `print.linear_range`. Range
merge starts from the recipe pair (or `[0,1]`). Atomic `--linear-range` replaces
that pair and conflicts with either legacy range flag. Without the atomic flag,
`--clip-low` and `--clip-high` independently replace only their respective
endpoint, so either or both may be supplied; validate finite `low < high` only
after merge. Legacy recipe endpoint keys construct the recipe baseline only when
`print.linear_range` is absent; coexistence is a usage error. Reports record
per-endpoint provenance and warn when a legacy range
flag or recipe alias was consumed. New recipe/report output writes only the
replacement names. Legacy no-preset TIFF calls keep current ordering until
preset migration. Named target presets never retain the old pre-characterization
meaning. Shared WB/exposure/black/white parameters resolve
once and feed both display branches; SDR and
HDR own separate highlight/tone, reference-white, gamut, and transfer policies.
The aliases preserve requested values, not legacy pixels: post-artifact ACEScg
gains generally do not commute with a channel-mixing characterization. Report
the order migration visibly and bump `pipeline_version` when the new simple
default/order becomes active.
Pin the shared order as explicit WB → exposure → existing black-point operation
→ `linear_range` affine placement → branch-specific highlight/tone work.
`linear_range` endpoints must be finite with `low < high`; defaults are identity.

The target `scene-master` preserves cross-frame scene exposure. It rejects
frame-local automatic Dmax because that is exposure normalization. It accepts
`density.dmax = none` where the selected reconstruction algorithm supports it,
or an explicit fixed/roll-calibrated Dmax from `dmax-reference`; sigmoid's
existing requirement for a fixed Dmax still applies. The master bypasses every
later WB, exposure, black, white, highlight, tone, gamut, and display-transfer control.
After recipe/CLI merge, selecting the named `scene-master` preset fails loudly if
any of those controls has a non-default effective value, regardless of whether it
came from the recipe, a flag, or a deprecated simple-control alias. It never
silently ignores a requested adjustment and has no "ignore display controls"
escape hatch. An adjusted linear export is
an explicitly reported `custom` workflow, not `scene-master`.
Today’s `--output-hdr` float TIFF has already passed the current print renderer
and is therefore only a transitional rendered float output, not this target
scene-master.

## How to Verify

- Type/API tests prevent display renderers from consuming device RGB and prevent
  named outputs from bypassing the explicit characterization/fallback decision.
- Ordering tests prove characterization precedes shared WB/exposure/black/white and all
  nonlinear highlight/tone/gamut work.
- Boundary/type tests prove this task accepts only placed `f32` linear ACEScg and
  cannot observe or re-run density artifact/Dmax intermediates. Sigmoid/simple
  arrive at the same typed boundary under their respective contracts.
- SDR and HDR receive byte-identical characterized/shared-adjusted source buffers
  and identical resolved shared parameters before their branches diverge.
- Ordering tests pin WB → exposure → existing black point → linear-range affine
  placement, including finite/ordered range validation and identity defaults.
- Range merge tests cover replacement/legacy recipe baselines and their conflict,
  default baseline, atomic replacement, low-only,
  high-only, both legacy endpoint overrides, atomic/legacy conflicts, post-merge
  validation, endpoint provenance, and the legacy warning. Scene-master tests
  reject every final non-default range regardless of source and allow legacy
  flags to reset recipe endpoints to `[0,1]`.
- `scene-master` with default later controls remains unclamped linear ACEScg and
  round-trips through float TIFF. Every non-default WB, exposure, black,
  white, highlight, tone, gamut, or display-transfer value from recipe or CLI is
  a usage error after merge; tests include legacy simple aliases, flags-win
  resets to defaults, recipe-only and CLI-only conflicts, and prove there is no
  ignore mode.
- The resolved report records the selected master branch, default effective
  downstream controls, the already-applied Dmax placement policy/value and its
  provenance, and that no display transfer ran.
- `scene-master` rejects frame-local auto Dmax, accepts density `none` or fixed
  roll-calibrated Dmax, and preserves known cross-frame exposure ratios from the
  runtime's fused scalar placement. Sigmoid requires the artifact's exact
  fixed Dmax; simple requires none.
- Simple-path tests prove the runtime characterizes raw unclamped
  `1 - scan/Dmin`; explicit inversion WB and clip/black/white remapping occur
  afterward through the shared render contract. Migration tests pin the fate of
  `--invert-white-balance`, `--clip-low`, `--clip-high`, and their recipe keys:
  warned aliases to `print.white_balance` / `print.linear_range`, with the pinned
  endpoint merge/conflict rules and new recipes/reports emitting only replacement names. A
  channel-mixing fixture proves migrated WB is post-characterization and is not
  falsely promised bit-identical to legacy ordering.
- Regression tests retain the explicitly named transitional behavior of
  `--output-hdr` until output-preset migration removes or replaces it.

## Dependencies

- [Post-reconstruction characterization runtime](post-reconstruction-color-characterization.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
