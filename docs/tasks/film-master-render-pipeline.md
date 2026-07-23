# Film-Master and Shared Display Pipeline

## Goal

Route the intentional linear ACEScg film rendering to a true film master and to
one shared adjustment stage feeding SDR and HDR:

```text
linear ACEScg film rendering
  ├→ film-master
  └→ shared adjustments → SDR/HDR renderers
```

`film-master` replaces the planned `scene-master` name. It is not a physical
scene-linear recovery.

## Design

The split accepts typed `AcesCgImage` regardless of which upstream stage
produced it; producer provenance is not a type invariant. This mandatory task
constructs and tests that input only through the direct, uncorrected NC film RGB
v1 mapper path. Define `film-master` as an unclamped 32-bit float linear ACEScg
TIFF containing the intentional film, lens, development, scanner,
reconstruction, and density-curve rendering.

The master includes Dmin normalization, negative reconstruction, the exponential
or sigmoid density curve, and supported fixed/roll Dmax placement. It bypasses
all later white balance, exposure, black/white placement, highlight compression,
display tone mapping, gamut mapping, and transfer encoding.

After recipe/CLI merge, `film-master` rejects frame-local auto Dmax and every
non-default downstream control, regardless of whether the value came from a
recipe, a flag, or a migrated simple-control alias. It never silently ignores a
requested adjustment. A linear export with a creative, print, or display
adjustment is an explicitly reported `custom` workflow.

Move shared print controls after the ACEScg boundary for named display outputs
and resolve them once for both branches. Pin the order:

```text
white balance → exposure → black/range placement → branch-specific rendering
```

SDR and HDR receive the identical adjusted source and then independently own
reference white, highlight/tone behavior, destination gamut mapping, and transfer
encoding. Preserve legacy no-preset TIFF ordering until output-preset migration.

For simple reconstruction, migrate existing inversion-WB and clip endpoints to
the shared white-balance/range controls for named presets. Preserve their
requested numeric values but do not promise identical pixels after the
working-space transform, since channel gains do not generally commute with a
matrix. Pin merge, conflict, provenance, warning, and pipeline-version behavior.

The current `--output-hdr` remains a transitional rendered float TIFF and is
never an alias for `film-master`.

This task implements no correction-profile selection, correction stage, or
correction provenance. Its fixtures cover the direct uncorrected mapper path;
the stable public split remains producer-agnostic over `AcesCgImage`.

## How to Verify

- Type/API tests allow only `AcesCgImage` into the split and prevent film RGB or
  device RGB from reaching a named output directly; this task's fixtures feed
  the uncorrected mapper output directly.
- `film-master` round-trips unclamped finite ACEScg through float TIFF and
  records the reconstruction, curve, Dmax, working-mapping, and pipeline
  versions without claiming physical scene recovery.
- Master validation rejects auto Dmax and every non-default downstream control;
  fixed/roll Dmax and supported `none` behavior are pinned by curve type.
- Ordering fixtures prove WB → exposure → black/range placement and prove SDR
  and HDR receive identical adjusted source buffers before diverging.
- Simple migration tests cover new controls, old-key rejection or migration
  diagnostics, merge conflicts, resolved provenance, and the legacy no-preset
  pixel-preservation boundary.
- SDR/HDR branch fixtures prove no display tone, gamut, or transfer operation
  runs on `film-master`.
- Reports and help consistently use `film-master`; `scene-master` is rejected as
  an unreleased-schema break.

## Dependencies

- [NC Film RGB working-space mapping](film-rgb-working-space.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
