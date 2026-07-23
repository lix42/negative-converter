# Negative Reconstruction and Density Curves

## Goal

Separate negative reconstruction from the density-to-positive curve, while
preserving the shipped exponential-density pixels and the exact shipped sigmoid
equation. Produce one typed film-rendering RGB boundary from every reconstruction
path:

```text
scan → Dmin normalization → corrected density D′
     → exponential or sigmoid density curve
     → FilmRgbImage
```

Simple reconstruction reaches the same boundary through its direct inversion
path. This task defines film-domain math only; it does not neutralize film,
scanner, lens, or development character.

## Design

Replace `Algorithm::{Simple,Density,Sigmoid}` with a tagged reconstruction
configuration:

```text
reconstruction:
  schema_version: 1
  type: simple

reconstruction:
  schema_version: 1
  type: density
  density:
    scale: [1, 1, 1]
    offset: [0, 0, 0]
    shadow_balance: [0, 0, 0]
    highlight_balance: [0, 0, 0]
    balance_range: auto
  curve:
    type: exponential
    gamma: 1.0
    dmax: fixed
```

The public CLI is `--reconstruction simple|density`. Density reconstruction also
accepts `--density-curve exponential|sigmoid`; it defaults to `exponential`.
The exact target recipe contract is:

- `reconstruction.schema_version = 1` in every resolved recipe; partial input
  may omit it and defaults to 1;
- `reconstruction.type = "simple"` with no density or curve fields; or
- `reconstruction.type = "density"` with
  `reconstruction.density.{scale,offset,shadow_balance,highlight_balance,balance_range}`
  and exactly one tagged `reconstruction.curve`;
- exponential curve fields are `{type, gamma, dmax}`; sigmoid curve fields are
  `{type, contrast, toe, shoulder, dmax}`;
- `dmax` accepts `"fixed"`, `"auto"`, `"none"`, or
  `{"explicit": <density>}`, with `"none"` valid only for exponential.

Partial input may omit `reconstruction.curve`; that selects exponential with
all exponential defaults. Resolved recipes and reports always emit exactly one
tagged curve, so omission never survives normalization.

CLI mapping is exact: `--density-scale`/`--density-offset` map to
`reconstruction.density.scale`/`.offset`; regional-balance flags map to the
same-named density fields; `--density-gamma` maps to
`reconstruction.curve.gamma`; sigmoid flags map to
`reconstruction.curve.{contrast,toe,shoulder}`; and
`--fixed-d-max`/`--d-max`/`--auto-d-max`/`--no-d-max` replace
`reconstruction.curve.dmax`.
Because nc is unreleased, reject `--algorithm` and the old `algorithm` recipe
forms, including the old sibling `density`, `sigmoid`, and `simple` objects,
with a clear migration error instead of maintaining aliases. Reject
`--density-curve` or curve/Dmax flags with `simple`, sigmoid-only settings with
`exponential`, `--density-gamma` with sigmoid, and every other invalid tagged
combination after recipe/CLI merge. A customized gamma under sigmoid is a usage
error, never ignored or downgraded to a warning.

Density reconstruction owns Dmin normalization, density scale/offset, regional
balance, and corrected density `D′`. The selected curve then maps `D′` into
positive film RGB:

- `exponential { gamma, dmax }` preserves the current density equation and
  default pixels exactly.
- `sigmoid { contrast, toe, shoulder, dmax }` preserves the current sigmoid
  equation exactly; this refactor changes ownership and schema, not its numeric
  behavior.

Move Dmax ownership to the density-curve stage. For exponential it is the scalar
placement currently represented inside the exponent. For sigmoid it remains a
curve-shaping input. Fixed/roll Dmax and `none` retain their supported meanings;
frame-local auto Dmax remains available for display-oriented conversions but is
not suitable for a film master.

Both density curves and simple reconstruction return a private-field
`FilmRgbImage`. Move simple's inversion white balance and clip-range controls out
of reconstruction so the typed value contains the direct unclamped positive
after Dmin normalization. Preserve the current no-preset TIFF pixel ordering
until output-preset activation; only named presets use the new stage ordering.

Introduce `reconstruction.schema_version = 1` for the tagged recipe/report wire
schema. This is separate from behavioral `pipeline_version`. The
`conversion-versioning` task solely owns stamping and bumping
`pipeline_version`, and only default pixel changes trigger a bump. This
bit-identical refactor preserves legacy no-preset TIFF pixels and therefore does
not claim a bump. Named-preset activation and the new simple ordering do change
pixels and must cross a prospective golden-tested `pipeline_version` boundary
when they activate.

The report's `recipe.reconstruction` uses the exact nested target schema. Its
`reconstruction_result` is either `{"type":"simple"}` or a density
object with curve type and resolved
`dmax = {policy,value,provenance}`; policy is
`fixed|explicit|auto|none`, and provenance is `default|recipe|cli|auto-frame`.
Reference-measured scalars frozen into a recipe report `explicit`/`recipe`;
capture provenance remains in the estimate record. Removed schema
forms fail loudly rather than silently changing meaning.

## How to Verify

- Numeric fixtures prove default density/exponential output is bit-identical to
  the current density path and the sigmoid equation is unchanged over shadows,
  midtones, highlights, and out-of-range finite values.
- Fixtures pin Dmin normalization, corrected density `D′`, exponential
  placement, sigmoid shaping, simple inversion, and IR-plane preservation.
- Type/API tests prove every supported path produces `FilmRgbImage` and no raw
  scan/density buffer can cross into the working-space mapper.
- CLI and recipe tests cover both tagged forms, exponential defaulting, clean
  rejection of `--algorithm`/old recipes, invalid combinations, flags-win merge,
  round trips, reports, and unknown fields.
- Dmax tests pin fixed, roll-calibrated, none, and auto behavior separately for
  exponential and sigmoid, including the master-incompatible auto case.
- Legacy no-preset TIFF regression fixtures remain unchanged until preset
  activation; named-preset fixtures pin the new simple control ordering.
- Schema fixtures pin `reconstruction.schema_version = 1`, omitted-curve
  normalization, and resolved tagged-curve emission. Golden no-preset fixtures
  prove this task causes no behavioral `pipeline_version` bump; preset
  activation tests own the later prospective boundary.

## Dependencies

- [Input data semantics and validation](input-data-semantics.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
