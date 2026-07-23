# Optional Color-Correction Profiles

## Goal

Provide an explicitly selected, CCR-like correction feature for users who want
to neutralize some scanner, film, development, or lens behavior. Correction is
optional and is never part of NC's default film-preserving pipeline.

## Design

Selection is explicit:

- CLI: `--correction-profile <path>`;
- recipe: `correction.profile = {"file": "<path>"}`;
- default/absence: `correction.profile = null`, with no profile load, correction
  math, warning, or pixel change.

The input recipe records the file reference. The resolved report and sidecar
add the loaded profile's declared identifier/version and SHA-256, compatible
working-mapping/pipeline versions, correction scope, and full provenance listed
below. A CLI path replaces the recipe selection under flags-win semantics.
Unknown fields, an invalid selection structure, an unreadable file, a hash
mismatch, or an incompatible version are usage/input errors; none fall back to the
uncorrected path.

Apply the selected profile **immediately after** NC film RGB v1 constructs
`AcesCgImage` and **before** the film-master/display split:

```text
FilmRgbImage → NC film RGB v1 → AcesCgImage
                              → optional selected correction
                                ├→ film-master
                                └→ shared display adjustments
```

With no profile selected, this stage is an identity and the normal path is
bit-identical. An explicitly corrected master is still the `film-master`
preset: it remains unclamped linear ACEScg and bypasses display/print controls,
but its metadata must say `correction_applied = true` and identify exactly what
was corrected. This is not the default film-preserving master and must never be
indistinguishable from one in reports or sidecars.

Every profile must identify:

- the scanner, film stock, and development process it was fitted for;
- whether the capture lens is included and, if so, its identity/settings;
- exactly which behavior the profile intends to correct;
- capture target, illuminant, reference data, fitting configuration, model, and
  version;
- its compatible working-mapping and pipeline versions.

Keep chart capture, profile fitting, curves/matrices or later model extensions,
and Delta E validation in offline tooling. Runtime loading is deterministic,
strictly versioned, and fail-loud for incompatible or malformed profiles. Do not
bake creative white balance or display rendering into a correction.

This task owns the runtime selection field, loader, correction stage, corrected
film-master integration, profile tooling, and provenance. It depends on the
working-space definition (so correction coordinates are well defined) and the
completed film-master/display split (so the insertion point and
base contract are stable). It feeds corrected `AcesCgImage` into that
same producer-agnostic typed split without weakening or changing the mandatory
task's public contract or verification.
It has **no downstream dependency
edges**: P3, SDR, HDR, gain maps, presets, film master, and display acceptance
work with the default film-preserving rendering and cannot wait for a measured
profile.

## How to Verify

- With `correction.profile = null` and with the key omitted, outputs are
  bit-identical to the normal pipeline and no artifact is opened.
- CLI/recipe merge tests pin `--correction-profile` →
  `correction.profile.file`, flags-win behavior, resolved identity/hash, and
  fail-loud malformed/incompatible/missing artifacts.
- Explicit selection records profile identity, scope, provenance, corrected
  behaviors, lens inclusion, and compatibility versions in recipes/reports.
- Mismatched or malformed profiles fail loudly; unknown models/versions are not
  guessed.
- Repeated fitting from identical controlled measurements produces a stable
  artifact/hash, and held-out Delta E plus neutral-axis results are reported
  against predeclared criteria.
- Model complexity is rejected when held-out measurements do not justify it.
- Tests prove profile selection is opt-in and that no output preset or acceptance
  path silently activates or requires one.
- Stage-order tests prove correction runs after NC film RGB v1 and before the
  split; corrected `film-master` remains unclamped and records its correction,
  while the uncorrected master remains byte-identical.

## Dependencies

- [NC Film RGB working-space mapping](film-rgb-working-space.md)
- [Film-master and shared display pipeline](film-master-render-pipeline.md)
