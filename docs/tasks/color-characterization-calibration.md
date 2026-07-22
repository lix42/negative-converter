# Color-Characterization Calibration

## Goal

Produce and validate real scanner/film characterization artifacts for the runtime
mapping from reconstructed scanner/film RGB into linear ACEScg. Choose model
complexity from controlled measurements rather than assumptions.

## Design

Build offline tooling around the versioned artifact contract established by the
runtime task. Fit against a photographed target with recorded illuminant, capture,
film stock/development, scanner settings, and trusted reference values. Pin the
target reference coordinates and reference illuminant. Declare whether reference
values are already in ACEScg D60 or the exact chromatic-adaptation method used to
map them there; do not silently reinterpret white points. Start with
a 3x3 matrix, add per-channel curves only when residual structure justifies them,
and propose a separate 3D-LUT runtime extension only if measured errors still
require it.

Report median and tail Delta E, neutral-axis error, and held-out-patch behavior.
Predeclare the acceptance criterion and keep fitting and validation sets separate.
Record whether an artifact is global, scanner-specific, or coupled to film stock
and development. The produced artifact carries identity, version, provenance, the
compatible reconstruction-domain contract/hash, and a reproducible content hash.
Calibration normalization must not bake in creative/scene white balance that the
display pipeline applies again. Any target-capture normalization is explicit
provenance and mathematically separated from user WB.

This task owns calibration/model selection only. Runtime types, stage ordering,
fallback behavior, and artifact loading remain in
`post-reconstruction-color-characterization`.

## Implementation Suggestion

- Keep fitting tooling outside the conversion hot path; nc runtime consumes only
  the compact deterministic artifact.
- Preserve raw patch measurements and fitting configuration in small reproducible
  fixtures without committing full-size scans.
- Compare every fitted model with the explicit assumed linear Rec.709/D65 →
  ACEScg/D60 provisional fallback.

## How to Verify

- Repeated fitting from identical measurements produces the same artifact bytes
  or canonical content hash.
- Held-out target validation improves over the provisional fallback under the
  predeclared median/tail Delta E criteria.
- Increasing model complexity is rejected when it does not materially improve
  held-out results.
- Artifact provenance identifies scanner, film/development scope, illuminant,
  reference data, fitting configuration, model type, and version.
- Reference-coordinate and illuminant tests pin the declared adaptation into
  ACEScg D60; neutral patches remain neutral without applying creative WB twice.
- An artifact identifies the exact compatible reconstruction-domain contract and
  is rejected after any coordinate-defining policy/setting change. New measured
  Dmin values remain compatible. Density calibration uses Dmax-neutral
  `10^(gamma*D')`, so new density Dmax values remain compatible and are applied
  later as a scalar ACEScg gain. Sigmoid v1 artifacts pin numeric Dmax because it
  changes curve shape; simple artifacts fit and pin the raw unclamped
  `1 - scan/Dmin` canonical inversion and remain independent of downstream white
  balance and black/white placement.
- The generated artifact loads and reproduces expected patch transforms through
  the runtime implementation.

## Dependencies

- [Post-reconstruction characterization runtime](post-reconstruction-color-characterization.md)
