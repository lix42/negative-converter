# NC Film RGB Working-Space Mapping

## Goal

Define one normal, deterministic working-space mapping shared by simple
reconstruction and every density curve:

```text
FilmRgbImage → NC film RGB v1 interpretation → linear ACEScg/D60
```

This mapping expresses NC's film-rendering intent. It is not a claim that the
result recovers physically neutral scene color.

## Design

Define **NC film RGB v1** as the existing intentional interpretation of the
reconstructed film RGB values as linear Rec.709 primaries at D65, followed by
the standard primary transform and chromatic adaptation into linear ACEScg/AP1
at D60. Pin the matrices, adaptation method, white points, precision, and
operation order as a versioned mapping.

Use private-field typed boundaries:

- `FilmRgbImage` contains the intentional positive film rendering produced by
  reconstruction and a density curve.
- `AcesCgImage` contains that rendering after NC film RGB v1 interpretation.

Named color outputs accept only `AcesCgImage`; they cannot attach an ACEScg,
Display P3, sRGB, or HDR profile directly to `FilmRgbImage`. The mapping is the
same for simple, density/exponential, and density/sigmoid. It adds no fitted
curves or matrices and deliberately preserves differences caused by film stock,
lens, development, scanner, and the selected density curve.

Record `working_mapping = "nc-film-rgb-v1"` (or an equivalent pinned tagged
form) in resolved recipes and reports. A future mapping change requires a new
identifier and a behavioral-version decision by `conversion-versioning` rather
than silently altering v1.

Keep legacy no-preset TIFF behavior and ordering during migration. Named outputs
use the typed mapping when output presets activate. Preserve unclamped finite
floating-point values through the transform; output rendering, not this stage,
owns tone and gamut limiting.

## How to Verify

- Pinned primary, neutral, saturated, negative, and above-one vectors verify the
  exact Rec.709/D65 → ACEScg/D60 transform and adaptation.
- All reconstruction/curve combinations use the same mapper and produce
  deterministic, byte-identical float buffers on repeated runs.
- Direct matrix/adaptation fixtures compare the mapped ACEScg values with
  independently calculated binary64 reference vectors at maximum absolute error
  ≤ 2×10⁻⁶ per `f32` channel; they do not invoke named output renderers or
  profiles. Cross-encoding and display decode-back verification belongs to
  `display-output-acceptance`.
- Unclamped finite values survive the mapping without hidden clipping; non-finite
  handling follows the pipeline's explicit error/report policy.
- Compile-time/API tests prevent direct `FilmRgbImage` → named-profile tagging
  and prevent construction of typed values without the owning stage.
- Recipe/report fixtures pin the mapping identifier. Behavioral version stamping
  remains owned by `conversion-versioning`.
- Legacy no-preset TIFF regression fixtures retain their current pixels until
  the preset migration activates the typed path.

## Dependencies

- [Negative reconstruction and density curves](negative-reconstruction-density-curves.md)
- [Color management](color-management.md)
