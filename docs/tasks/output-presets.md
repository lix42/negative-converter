# Output Presets and Guidance

## Goal

Expose coherent output choices that resolve format, color space, transfer,
bit-depth, rendering, and metadata together. Make standards-based gain-map HDR
the product default while preserving explicit compatibility and scene-master
outputs.

## Design

Define stable presets approximately as follows; exact names/formats follow the
HDR spike and encoder implementation:

| Preset | Purpose |
|---|---|
| `gain-map-hdr` | **Default:** backward-compatible SDR base plus ISO gain-map HDR |
| `display-p3` | wide-gamut SDR display output |
| `compatibility` | sRGB SDR for broad legacy/web support |
| `scene-master` | unclamped 32-bit float linear ACEScg TIFF |
| `hdr-pq` | single-rendition BT.2020/Rec.2100 PQ |
| `hdr-hlg` | explicit HLG/broadcast-oriented output |
| `custom` | advanced explicit profile/format configuration |

A preset is an atomic policy choice, not a nickname for one ICC profile. It
resolves container, depth, primaries/profile, transfer function, tone/gamut
mapping, and required metadata.

`scene-master` branches directly from characterized linear ACEScg and bypasses
white balance, exposure, black/white placement, highlight compression, SDR/HDR tone
mapping, and display gamut mapping. This makes it a real unclamped scene-linear
master rather than a display-rendered float file. Named display presets use the
SDR or HDR rendering branches. Any explicitly adjusted linear master belongs to
`custom` and records every adjustment.

The bypass is strict, not silent: after recipe/CLI merge, `scene-master` rejects
any non-default white balance, exposure, black/white point, highlight, SDR/HDR tone,
gamut, or display-transfer control from either source. There is no flag to ignore
conflicting controls. A CLI override that resolves a recipe value back to the
documented default is allowed under flags-win semantics and the resolved report
records the final default value and its provenance.

For the simple algorithm, named presets use raw unclamped `1 - scan/Dmin` as the
characterization input. Target presets use `print.white_balance` and a new
`print.linear_range = [low, high]` / `--linear-range LOW,HIGH` (default `[0,1]`)
for the exact affine black/white remap. The current
`--invert-white-balance`, `--clip-low`, and `--clip-high` controls (and
`simple.*` recipe keys) are legacy render controls, not reconstruction
coordinates. During migration they are accepted as warned aliases to the new
fields and are never emitted by new recipes/reports. Range resolution starts
from `print.linear_range` in the recipe or default `[0,1]`. Atomic
`--linear-range` replaces the pair and conflicts with either `--clip-low` or
`--clip-high`; without it, each legacy flag independently overrides only its
endpoint, so one or both are valid. Validate finite `low < high` after merge,
record provenance per endpoint, and emit a legacy warning. Legacy simple recipe
endpoint keys construct the baseline only when `print.linear_range` is absent;
coexistence is a usage error. Legacy no-preset TIFF
calls retain current ordering until migration. Named presets apply resolved
aliases only after characterization; `scene-master` rejects every final
non-default range regardless of source, while flags may reset recipe endpoints
to `[0,1]`.
The shared order is WB → exposure → existing black point → `linear_range`
affine placement; range endpoints are finite with `low < high`.
Because per-channel WB generally does not commute with characterization, an
alias preserves requested numbers but not legacy pixels. Reports/help say so,
and activating this simple ordering bumps `pipeline_version`.

To preserve exposure across frames, `scene-master` rejects frame-local automatic
Dmax. Density accepts `none` (unity fused-runtime placement) or a fixed/
roll-calibrated scalar applied in ACEScg and records that already-applied
placement policy/value in output provenance. This is exposure placement, not a
guarantee that any sample maps to display white. Sigmoid v1 requires the exact fixed Dmax
declared by its characterization artifact because that value shapes its nonlinear
reconstruction; simple has no Dmax. The
current `--output-hdr` float TIFF is already print-rendered and must be documented
as a transitional rendered float TIFF, never as an alias for `scene-master`.

The output path remains required and is never silently renamed. Its extension
must match the preset's resolved container (for example, the spike will pin the
accepted `.heic`/`.heif` spelling for `gain-map-hdr`); a mismatch is a usage
error that reports the expected extensions. Named presets other than `custom`
are atomic: legacy depth/profile/container controls such as `--output-hdr`,
`--output-sdr`, `--output-profile`, and `--bigtiff` cannot accompany them, even
when they appear equivalent. Existing legacy flags without `--output-preset`
continue to resolve the current TIFF policy during migration. Advanced explicit
combinations use `--output-preset custom`, are fully validated, and are recorded
in the resolved recipe/report.

This task extends the shipped `nc roll` batch-apply scaffold. Today, automatic
names are `<stem>_positive.tiff`, manifest entries may provide explicit outputs,
per-frame partial recipes deep-merge onto the shared recipe, sidecars derive from
each final image path, and exactly one roll report uses stdout or
`--report-file`; the implementation collision-checks all of those targets before
writing. Preset migration replaces only the hard-coded TIFF/container
assumption: automatic names derive their suffix from each resolved preset,
manifest paths and per-frame preset overrides validate container/suffix
compatibility independently, and the existing single-report and collision
guarantees remain intact. Define which output policy is roll-shared versus
per-frame/custom without duplicating the shipped roll orchestration.

Replace or deprecate the ambiguous current `--output-hdr` meaning. The target
unrendered 32-bit float linear ACEScg branch is `scene-master`, whereas PQ/HLG/
gain-map outputs are display HDR; the current rendered float path aliases neither.
Because nc is unreleased, prefer a clear schema over compatibility aliases that
preserve misleading terminology.

## How to Verify

- With an output path but no output-selection options, resolution selects
  `gain-map-hdr` and records every effective setting.
- Each preset resolves to the documented container, depth, color encoding,
  rendering path, and metadata; explicit conflicts fail loudly.
- Path-extension tests cover every container, including a mismatched
  `gain-map-hdr`/`.tiff` path; nc rejects mismatches and never rewrites a path.
- CLI tests reject legacy output flags combined with a named non-`custom` preset,
  while legacy flag-only invocations retain their documented transitional TIFF
  behavior.
- Recipe/CLI merge tests prove flags win and unknown preset names fail.
- Help and documentation explain which output to choose without requiring color
  management knowledge.
- `scene-master` tests prove print/display controls are bypassed and unclamped
  characterized linear ACEScg round-trips through float TIFF; auto Dmax is
  rejected; density fixed/roll-calibrated or `none` preserves exposure, sigmoid
  enforces its exact artifact constraint, simple exposes no Dmax, and the report
  records the applied placement without claiming a display-white mapping.
- Merge/conflict tests cover every downstream control from recipe and CLI,
  flags-win resets to defaults, complete resolved-report provenance, and the
  absence of a silent-ignore option.
- Simple migration tests prove named display presets characterize raw inversion
  before applying resolved WB/black/white placement, while `scene-master`
  rejects non-default new controls and legacy aliases. Help, recipes, and reports
  use the replacement names and emit the pinned warned-alias behavior for the old
  names.
- Range merge tests cover replacement/legacy recipe baselines and their conflict,
  default baseline, atomic replacement, each
  legacy endpoint alone, both together, atomic/legacy conflicts, post-merge
  validation, per-endpoint provenance/warning, scene-master rejection from every
  source, and flags resetting a recipe pair to `[0,1]`.
- A channel-mixing artifact fixture proves the warned alias runs after
  characterization and may differ from legacy simple output; version/report
  tests pin the required pipeline-version bump and migration diagnostic.
- `nc roll` tests cover auto naming for every resolved container, explicit
  manifest outputs, per-frame/custom overrides, mismatch failures, shared-policy
  resolution, sidecars derived per final image, exactly one roll report on stdout
  or `--report-file`, and report collision rejection against all inputs, outputs,
  and sidecars.

## Dependencies

- [ISO gain-map HDR output](gain-map-hdr-output.md)
- [Roll conversion](roll-conversion.md)
