# Post-Reconstruction Characterization Runtime

**Status:** closed—superseded (2026-07-23)

This proposal is retained as historical decision context and was not
implemented. NC does not aim to recover a physically neutral scene: film stock,
lens, development, and scanner character are intentional parts of the default
rendering. Any future measured neutralization must be an explicitly selected
correction profile, not a dependency of P3, HDR, output presets, or display
acceptance. Replacement film-preserving pipeline tasks are defined separately.

## Goal

Establish the runtime boundary that maps a reconstructed scanner/film-dependent
positive into linear ACEScg using a deterministic, versioned characterization
artifact. Keep an explicit provisional fallback so pipeline integration can be
implemented and tested before controlled calibration artifacts are available.

## Design

Keep Dmin and film-domain math in linear scanner coordinates. Stop negative
reconstruction at the algorithm's canonical unclamped characterization-input
domain, before white balance, exposure, black/white placement, highlight
compression, or any other creative/display render. Apply the
calibrated mapping at that boundary:

```text
canonical scanner/film positive → characterization → linear ACEScg at canonical scale
```

Pin the canonical input separately for every v1 algorithm:

- **Density:** after Dmin normalization, density scale/offset, regional balance,
  and density gamma, form the **Dmax-neutral** positive
  `U = 10^(gamma * D')`. Apply the nonlinear input curves and matrix to `U`.
  Do not subtract Dmax before the curves. In this same runtime operation, apply
  scalar roll exposure-placement gain `G = 10^(-gamma * Dmax)` to every
  characterized ACEScg channel (`G = 1` for `none`), then return ordinary typed
  `f32` linear ACEScg to the render pipeline. This deliberately refactors current
  `10^(gamma * (D' - Dmax))`; density artifacts can then span numeric Dmax values
  without moving a scale through nonlinear artifact curves.
  Because removing the anchor from the exponent also removes current `f32`
  overflow cancellation, evaluate `U → curves → matrix → G` with `f64` or a
  documented equivalent extended-range representation and narrow to `f32` only
  after the fused placement. No intermediate extended-range buffer crosses the
  runtime API. An unrepresentable final result fails/reports loudly; no
  intermediate overflow or clamp may silently change it.
  With an arbitrary nonlinear artifact, this post-artifact gain is exposure
  placement, not a guarantee that `D' = Dmax` maps to `1.0`. That equality is a
  property of the current pre-artifact renderer (and suitable identity/
  homogeneous mappings); SDR/HDR rendering owns display reference white.
- **Sigmoid:** v1 retains `U = S(D'; Dmax, contrast, toe, shoulder)`. Dmax changes
  nonlinear curve shape and cannot be factored into a downstream scalar. A
  measured sigmoid artifact therefore declares the exact numeric fixed Dmax and
  every shape setting as compatibility/scope constraints; runtime rejects any
  different value or auto-Dmax result. A future reusable sigmoid artifact needs
  a separately versioned, genuinely Dmax-neutral curve definition.
- **Simple:** v1 canonical input is the raw unclamped inversion immediately after
  Dmin normalization: `U_c = 1 - scan_c / Dmin_c`, evaluated per channel without
  clipping. The exact normalization/inversion equation and operation order define
  artifact compatibility; measured Dmin remains runtime provenance. The currently
  shipped simple renderer additionally applies `invert_white_balance` and the
  `clip_low`/`clip_high` affine remap before its output transform. The target
  refactor moves those user adjustments after characterization, so they must not
  identify or alter a simple characterization artifact. `simple` has no Dmax.

The runtime supports a deliberately small, pinned artifact contract. Version 1's
model discriminator is `matrix3x3-with-input-curves`; its fixed operation order
is reconstructed linear RGB → three monotone per-channel input curves → 3x3
matrix → linear ACEScg, with identity curves represented explicitly. It fixes
array lengths and finite coefficient encoding. The content hash is lowercase-hex
SHA-256 over the RFC 8785 JSON Canonicalization Scheme serialization of the
artifact object with its `content_sha256` member omitted entirely. This makes the
hash non-self-referential and pins key ordering, whitespace, Unicode, and number
serialization for interoperable implementations. Unknown
schema versions/models, malformed or non-monotone curves,
wrong array lengths/order, hash mismatches, and non-finite coefficients or
results fail loudly. A future 3D LUT requires a new declared model/version, not
an ambiguous optional field. Artifact production and measured model selection
belong to `color-characterization-calibration`, not this task.

Every artifact declares a standardized reconstruction-domain contract. It binds
to the reconstruction algorithm, pipeline/model versions, operation order, Dmin
normalization equation/units, density scale/offset/gamma settings, regional-
balance semantics/settings, each algorithm's canonical rule above, and sigmoid
parameters when applicable. Those definitions determine the
coordinates on which the fitted model operates and therefore invalidate reuse
when changed.

Do **not** bind a reusable density/simple scanner/film/development artifact to
incidental resolved measurements. Measured Dmin RGB, density's numeric Dmax,
source rectangles/files, confidence statistics, and other image-specific
estimates are reported runtime inputs, not artifact identity, provided they are
consumed by the contracted canonical rule. Sigmoid v1 is the explicit exception:
its numeric Dmax is a required scale-sensitive scope constraint. Any other
deliberately roll-specific artifact may declare narrower fixed measurement
constraints. The contract schema explicitly
classifies fields as coordinate-defining, runtime-measured, downstream-invariant,
or optional scope constraints.

The contract hash is lowercase-hex SHA-256 over RFC 8785 canonical JSON with the
`contract_sha256` member omitted. Runtime recomputes the semantic contract and
fails loudly on a true algorithm/model/coordinate-policy mismatch or violated
explicit scope constraint, while accepting new measured Dmin values and, for
density, new numeric Dmax values under an otherwise compatible reusable contract.

Until a calibrated artifact is supplied, a named color-defined output uses one
explicit, versioned assumed-source fallback: interpret the reconstructed values
as linear Rec.709/sRGB primaries at D65, chromatically adapt to ACEScg's D60
white, and transform into linear ACEScg. This creates internally valid ACEScg but
does **not** add measured scanner/film accuracy; every recipe/report and visible
warning labels it provisional and records the fallback version. Never pass
identity scanner-device RGB into an ACEScg-typed value or attach an ACEScg/P3/
sRGB profile to it. Raw identity device RGB may be preserved only through an
explicit untagged `custom` diagnostic output, and that domain cannot enter any
named color-defined preset.

This task ends at ordinary, placed `f32` linear ACEScg (measured artifact or
explicit assumed-source fallback) and an explicitly typed device-RGB diagnostic domain. The
separate `post-characterization-render-pipeline` task owns print/display stage
refactoring and scene-master/display routing.

## Implementation Suggestion

- Introduce a distinct type/boundary for reconstructed device RGB versus
  characterized working RGB so output transforms cannot accept the wrong domain.
- Parse and validate the versioned runtime artifact without depending on fitting
  or Delta E tooling.
- Preserve unclamped float values through characterization and report gamut or
  non-finite problems without silently clipping them.

## How to Verify

- Type/API tests prevent reconstructed scanner/film RGB from reaching the output
  ICC transform without an explicit characterization/fallback decision.
- Synthetic matrices prove channel mixing, identity behavior, determinism, and
  unclamped float handling.
- A boundary test uses canonical density/artifact intermediates above `f32::MAX`
  whose fused Dmax placement produces a representable result; no intermediate is
  exposed or narrowed, and the returned typed ACEScg is finite `f32`.
- Recipe/report round trips pin characterization identity, schema/model version,
  non-self-referential artifact/contract hashes, runtime measurement provenance,
  declared artifact scope, and fallback version.
- Loader negative tests reject unknown schema/model values, bad operation order
  or array lengths, malformed/non-monotone curves, hash mismatch, and non-finite
  coefficients; execution rejects non-finite results.
- Changing an algorithm/model, operation order, or algorithm-specific canonical
  coordinate setting causes a compatibility error. Density tests reuse one
  nonlinear-curves artifact across two numeric Dmax values and prove final
  ACEScg differs by exactly `10^(-gamma * (Dmax_2 - Dmax_1))`; changing Dmin
  changes only runtime provenance under the same normalization policy. Sigmoid
  v1 rejects any numeric Dmax different from its artifact constraint and rejects
  auto as an artifact-compatible source. Simple artifacts remain compatible when
  downstream WB or black/white placement changes, but reject a changed canonical
  Dmin-normalization/inversion definition. Other scoped artifacts reject values
  outside scope.
- A simple ordering fixture proves characterization receives exactly unclamped
  `1 - scan/Dmin`, independent of legacy `invert_white_balance` and
  `clip_low`/`clip_high` values; those controls are absent from the artifact
  contract and operate only through the downstream render path.
- Extreme-density tests prove the stable extended-range density path avoids the
  intermediate overflow that direct `f32` `10^(gamma*D')` would create and fails
  loudly only when the final placed value is not representable.
- A nonlinear density fixture proves the runtime does not assert or normalize
  `D' = Dmax` to `1.0`; it returns `artifact(U) * G` and leaves display reference
  white to the selected SDR/HDR renderer.
- The assumed linear Rec.709/D65 fallback produces the pinned chromatically
  adapted ACEScg/D60 vectors, warns visibly, and is never reported as measured.
- Identity device RGB remains explicitly untagged/custom and type/API tests prove
  it cannot reach named color-defined presets.
- Output profile assignment alone is not counted as characterization.

## Dependencies

- [Input data semantics and validation](input-data-semantics.md)
- [Color management](color-management.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
