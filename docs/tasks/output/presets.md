# Output Presets and Guidance

## Goal

Expose coherent output choices that resolve format, color space, transfer,
bit-depth, rendering, and metadata together. Make standards-based gain-map HDR
the product default while preserving explicit compatibility and film-master
outputs.

## Design

Define stable presets approximately as follows; exact names/formats follow the
HDR spike and encoder implementation:

| Preset | Purpose |
|---|---|
| `gain-map-hdr` | **Default:** backward-compatible SDR base plus ISO gain-map HDR |
| `ultra-hdr-v1` | Explicit pre-ISO compatibility output using public Ultra HDR v1 JPEG metadata |
| `display-p3` | 16-bit losslessly stored Display P3 SDR TIFF |
| `compatibility` | 16-bit losslessly stored sRGB SDR TIFF for broad compatibility |
| `film-master` | unclamped 32-bit float linear ACEScg TIFF preserving NC's film rendering |
| `hdr-pq` | single-rendition BT.2020/Rec.2100 PQ |
| `hdr-hlg` | explicit HLG/broadcast-oriented output |
| `hdr-linear-tiff` | 32-bit float display-linear BT.2020 HDR interchange TIFF |
| `hdr-pq-tiff` | losslessly stored 16-bit BT.2020/Rec.2100 PQ TIFF |
| `hdr-hlg-tiff` | losslessly stored 16-bit BT.2020/Rec.2100 HLG TIFF |
| `custom` | advanced explicit profile/format configuration |

A preset is an atomic policy choice, not a nickname for one ICC profile. It
resolves container, depth, primaries/profile, transfer function, tone/gamut
mapping, and required metadata.

The product-default recipe uses the reference-anchored sigmoid reconstruction.
Its toe establishes the common shadow-floor placement before the film-master /
display split. Output presets do not silently replace an explicit reconstruction
choice, but exponential and simple are advanced/custom diagnostic paths rather
than members of the normal product guidance. Do not remove those paths in this
task; public-surface retirement requires separate acceptance evidence and a
migration decision.

`film-master` branches directly from NC film RGB v1 mapped linear ACEScg and bypasses
white balance, exposure, black/range placement, highlight compression, SDR/HDR tone
mapping, and display gamut mapping. It preserves the intentional film, lens,
development, scanner, reconstruction, and curve rendering; it is not a physical
scene-linear recovery. Named display presets use the SDR or HDR rendering
branches. Their neutral defaults preserve the sigmoid's black and midtone
placement: display rendering may perform the declared transfer, reference-white,
highlight-headroom, and gamut adaptation, but must not compensate for a raised
reconstruction floor or introduce a second large creative grade. A linear
master with creative, print, or display adjustments belongs to `custom` and
records every adjustment. The mandatory preset implementation covers the
uncorrected path and does not depend on correction profiles. The later optional
correction task may produce the same accepted `AcesCgImage` type and feed it into
the unchanged split; that task owns the rule that corrected output remains
`film-master`, its identity/hash/scope provenance, and rejection of bypassed
print/display controls.

The bypass is strict, not silent: after recipe/CLI merge, `film-master` rejects
any non-default white balance, exposure, black/white point, highlight, SDR/HDR tone,
gamut, or display-transfer control from either source. There is no flag to ignore
conflicting controls. A CLI override that resolves a recipe value back to the
documented default is allowed under flags-win semantics and the resolved report
records the final default value and its provenance.

For simple reconstruction, named presets map raw unclamped `1 - scan/Dmin`
through NC film RGB v1. Target presets use `print.white_balance` and a new
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
aliases only after the ACEScg boundary; `film-master` rejects every final
non-default range regardless of source, while flags may reset recipe endpoints
to `[0,1]`.
The shared order is WB → exposure → existing black point → `linear_range`
affine placement; range endpoints are finite with `low < high`.
Because per-channel WB generally does not commute with the working-space matrix, an
alias preserves requested numbers but not legacy pixels. Reports/help say so,
and `conversion-versioning` owns the golden-tested `pipeline_version` bump when
this new preset/default ordering and the named SDR/HDR interpretations of
`highlight_compress` activate. The earlier bit-identical tagged reconstruction
refactor does not cause that bump.

Preset activation is also gated on extending and measuring
`pipeline::memory`. Its current `RunProfile::Convert` represents the shipped
legacy render/encode allocation graph; named display paths keep the shared
adjusted ACEScg buffer live while allocating a 12 B/px branch output, and
gain-map HDR may keep both renditions plus gain-map/codec staging live. Add
profile-specific accounting and calibrate it against measured peak RSS before
any display preset becomes CLI-reachable. Do not silently apply the legacy
estimate to these new paths.

To preserve exposure across frames, `film-master` rejects frame-local automatic
Dmax. The exponential density curve accepts supported `none` or fixed/
roll-calibrated scalar placement; the sigmoid curve uses fixed Dmax as a
curve-shaping input. Recipes and reports record the resolved policy/value without
claiming a display-white or physical-scene mapping. Simple has no Dmax. The
current `--output-hdr` float TIFF is already print-rendered and must be documented
as a transitional rendered float TIFF, never as an alias for `film-master`.

The output path remains required and is never silently renamed. Its extension
must match the preset's resolved container: `gain-map-hdr` and
`ultra-hdr-v1` accept `.jpg` and `.jpeg`; `hdr-pq` and `hdr-hlg` accept `.avif`; and `display-p3`,
`compatibility`, `film-master`, `hdr-linear-tiff`, `hdr-pq-tiff`, and
`hdr-hlg-tiff` accept `.tif`/`.tiff`. A mismatch is a usage error that reports
the expected extensions. Named presets other than `custom` are atomic: legacy
depth/profile/container controls such as `--output-hdr`, `--output-sdr`,
`--output-profile`, and `--bigtiff` cannot accompany them, even when they appear
equivalent. Existing legacy flags without `--output-preset` continue to resolve
the current TIFF policy during migration. Advanced explicit combinations use
`--output-preset custom`, are fully validated, and are recorded in the resolved
recipe/report.

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
unrendered 32-bit float linear ACEScg branch is `film-master`, whereas PQ/HLG/
gain-map outputs are display HDR; the current rendered float path aliases neither.
Because nc is unreleased, prefer a clear schema over compatibility aliases that
preserve misleading terminology.

## How to Verify

- With an output path but no output-selection options, resolution selects
  `gain-map-hdr` with reference-anchored sigmoid reconstruction and records every
  effective setting.
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
- `film-master` tests prove print/display controls are bypassed and unclamped
  NC film RGB v1 mapped linear ACEScg round-trips through float TIFF; auto Dmax
  is rejected; exponential fixed/roll-calibrated or supported `none` placement
  preserves exposure, sigmoid uses fixed Dmax for curve shaping, simple exposes
  no Dmax, and the report records the curve and placement without claiming a
  physical-scene or display-white mapping.
- A shared-source fixture proves film-master, SDR, and HDR start from the same
  reference-anchored sigmoid result. After declared output normalization, neutral
  display defaults do not re-establish black or substantially reshape midtones;
  unavoidable highlight/gamut adaptation is measured separately.
- Merge/conflict tests cover every downstream control from recipe and CLI,
  flags-win resets to defaults, complete resolved-report provenance, and the
  absence of a silent-ignore option.
- Simple migration tests prove named display presets map raw inversion before
  applying resolved WB/black/range placement, while `film-master`
  rejects non-default new controls and legacy aliases. Help, recipes, and reports
  use the replacement names and emit the pinned warned-alias behavior for the old
  names.
- Range merge tests cover replacement/legacy recipe baselines and their conflict,
  default baseline, atomic replacement, each
  legacy endpoint alone, both together, atomic/legacy conflicts, post-merge
  validation, per-endpoint provenance/warning, film-master rejection from every
  source, and flags resetting a recipe pair to `[0,1]`.
- A working-space matrix fixture proves the warned alias runs after
  NC film RGB mapping and may differ from legacy simple output; version/report
  tests pin the `conversion-versioning`-owned prospective pipeline-version
  boundary and migration diagnostic.
- `nc roll` tests cover auto naming for every resolved container, explicit
  manifest outputs, per-frame/custom overrides, mismatch failures, shared-policy
  resolution, sidecars derived per final image, exactly one roll report on stdout
  or `--report-file`, and report collision rejection against all inputs, outputs,
  and sidecars.
- Memory-preflight tests select the resolved preset's calibrated allocation
  profile and cover the shared-source/branch overlap; gain-map tests additionally
  cover simultaneous rendition and codec staging before default activation.

## Dependencies

- [Final ISO gain-map metadata](iso-gain-map-metadata.md)
- [HDR AVIF output](hdr-avif-output.md)
- [Lossless HDR TIFF outputs](lossless-hdr-tiff.md)
- [Reference-anchored sigmoid reconstruction](../algo/reference-anchored-sigmoid.md)
- [Roll conversion](../core/roll-conversion.md)
- [Conversion versioning and baseline comparison](../core/conversion-versioning.md)
