# Reference-Anchored Sigmoid Calibration and Redesign

## Goal

Fix a measured tonal defect in the shipped sigmoid rather than re-implementing
properties it already has. With one frozen real-roll Dmin/Dmax pair and recipe,
the current `D' = 0`-anchored sigmoid/defaults leave correctly exposed
photographic shadow content crowded into a narrow raised interval, producing a
pale, compressed tonal spread in both film-master-derived and display outputs.

The existing design-spec §7.3 equation is the baseline. First make that failure
reproducible and quantitative; only then decide whether calibration of defaults,
clearer parameter semantics, or an equation change is warranted.

## Design

Freeze a representative real-roll fixture set, one roll-derived Dmin/Dmax pair,
and one complete recipe. It must include correctly exposed frames with textured
deep shadows plus under- and overexposed controls. Record the exact asset
manifest identities/checksums and keep every frame on the same references and
recipe—no per-frame histogram fitting.

Measure the shipped sigmoid before proposing a replacement:

- corrected-density and film-master luminance distributions for declared shadow
  patches, including low-percentile interval width and ordering;
- normalized film-master → SDR and film-master → HDR shadow spreads, black-floor
  distance, clipping/non-finite counts, and representative patch deltas;
- relative exposure spacing across the correctly exposed, underexposed, and
  overexposed frames.

Define the acceptance bounds from those frozen patches and record them before
tuning. The completion report must show both the baseline failure and the chosen
candidate against the same metrics. A visual preference without the frozen
numbers is not completion.

Preserve these constraints regardless of the chosen remedy:

- Dmin remains the film-base/density origin; do not replace it with the darkest
  sampled photograph pixel.
- The roll-fixed scalar Dmax remains the normalization reference.
- The mapping stays finite, continuous, monotone, and has no hard shadow cutoff;
  below-toe values remain ordered and recoverable in the float film master.
- The product default remains reference-driven, never content-aware or
  frame-normalized. Content-aware toe placement stays a separate optional task.
- SDR/HDR rendering remains downstream output adaptation, not a hidden second
  reconstruction grade.

After the baseline is pinned, evaluate in this order: recalibrate existing
defaults; clarify/reparameterize current toe/contrast/shoulder semantics; change
the equation only if neither can satisfy the frozen acceptance bounds. Keep
exponential and simple as explicit diagnostic references. `output/presets` owns
activation of any new default and the associated conversion-version migration.

## Implementation Suggestion

- Add a checked-in derived-metrics harness or test fixture that consumes the
  frozen recipe/references and reports the named patch metrics without embedding
  real scan pixels in source.
- Capture the current §7.3 equation/default output as the baseline before editing
  curve code.
- If controls change, update CLI overrides, the tagged sigmoid recipe, merge,
  validation, resolved reporting, help, and design-spec §9 together.
- If pixels/defaults change, add the new pipeline fingerprint/version row and
  same-machine before/after report; never edit a historical fingerprint.

## How to Verify

- The frozen baseline test/report reproduces the narrow raised shadow interval
  on the correctly exposed real-roll patches and records all fixture, reference,
  and recipe identities.
- Candidate output meets the predeclared shadow-spread/black-floor bounds in
  film-master, normalized SDR, and normalized HDR while retaining patch ordering
  and finite samples.
- One frozen recipe preserves the measured relative exposure spacing of the
  correctly exposed, underexposed, and overexposed frames; no frame statistics
  enter default resolution.
- Property tests retain finiteness, monotonicity, continuity, endpoint behavior,
  and below-toe ordering across the allowed parameter domain.
- The final report states which remedy was selected—defaults, parameter
  semantics, or equation—and why the less invasive choices failed or sufficed.
- If the result becomes the product default, `output/presets` and conversion
  versioning land together; merely restating the already-shipped §7.3 properties
  cannot mark this task complete.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, and `cargo test` pass.

## Dependencies

- [Negative reconstruction and density curves](negative-reconstruction-density-curves.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](../film-base/dmax-reference.md)

Downstream activation: [Output presets and guidance](../output/presets.md).
