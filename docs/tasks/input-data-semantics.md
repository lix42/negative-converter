# Input Data Semantics and Validation

## Goal

Make nc resolve two independent facts before negative conversion: the transfer
encoding, and whether pixel axes are scanner-device measurements, colorimetric
RGB, or unknown. Report the evidence for each and fail loudly on ambiguity rather
than silently assuming linear Rec.709 or automatically applying an embedded ICC
profile before Dmin and density calculations.

## Design

Introduce an explicit decoded-input description shared by decode, inspect, and
convert. The exact Rust names may change, but measurement meaning and transfer
encoding must remain independent axes:

```rust
enum MeasurementMeaning {
    ScannerDevice,
    Colorimetric { space_or_profile: ColorReference },
    Unknown,
}

struct InputColorMetadata {
    meaning: MeasurementMeaning,
    transfer: TransferDescription,
    embedded_icc: Option<Vec<u8>>,
    evidence: Vec<InputEvidence>,
}
```

For the currently supported SilverFast HDR/HDRi TIFF path:

- A linear transfer (`Gamma=1`) and scanner-device measurement meaning are
  resolved from separate evidence. Only inputs with both a supported linear
  transfer and positive SilverFast HDR/HDRi raw-mode evidence enter Dmin/density
  without a source-to-working-space transform. Gamma 1 by itself proves neither
  raw-mode provenance nor device dependence.
- An embedded scanner input profile is retained and reported as device
  characterization metadata. Its presence does not, by itself, make the pixels
  rendered color data and does not authorize applying it before density.
- Gamma metadata that contradicts HDR/HDRi raw semantics, an unknown transfer,
  missing raw-mode evidence, or metadata that cannot establish measurement
  meaning is a loud
  unsupported/ambiguous-input error in `convert`. `inspect` must still report
  the evidence so the user can diagnose the file.
- IR remains measurement data and is never color transformed.

Already color-encoded negatives are a distinct future input path. Supporting
one requires a documented inverse transfer function and evidence that upstream
processing did not clip or otherwise destroy the transmission relationship.
Do not treat an ICC profile as sufficient evidence. DNG support should populate
the same semantic model when its decoder lands, while keeping raw image IFDs
distinct from rendered previews.

Revise the current `input.color` contract as part of implementation. Remove the
promise that `Auto` applies an embedded profile before film-base estimation;
keep `--input-profile` rejected for normal conversion unless and until the
before-density experiment establishes a supported placement. Because nc is
unreleased, prefer an honest schema change over preserving misleading behavior.

Replace/deprecate the combined `--assume-linear` / `input.color = "linear"`
promise with two independent assertions, exposed in both CLI and recipe shape:
one for transfer (for example `input.transfer = auto|linear|...`) and one for
meaning (`input.meaning = auto|scanner-device|colorimetric`). Define the allowed
combinations explicitly:

- scanner-device + supported linear transfer may enter Dmin/density;
- colorimetric or encoded negatives are rejected until a separately specified
  inverse-transfer/reconstruction path exists;
- unknown on either axis is rejected by `convert` but remains inspectable.

Evidence precedence is deterministic: explicit user assertions override
descriptive parsed metadata and record the displaced evidence; authoritative
raw-mode/container structure outranks descriptive tags. An explicit assertion
that contradicts a structural impossibility fails rather than overriding it, and
conflicting assertions at the same precedence are never silently reconciled.
Gamma 1 establishes only the transfer axis. Every explicit override is recorded
with its CLI/recipe provenance; it does not make an otherwise unsupported
colorimetric/encoded negative supported.

`nc inspect` and conversion reports should expose, without dumping profile
bytes:

- resolved measurement meaning and transfer function, with evidence for each;
- whether an ICC profile is embedded, plus safe summary fields such as class,
  color space, PCS, and description when parsable;
- the metadata/evidence that led to the resolution;
- whether any transfer decoding was performed.

## Implementation Suggestion

- Keep container parsing in `io/decode.rs`, but resolve semantics in a small
  pure helper that can be table-tested with synthetic metadata.
- Start with the SilverFast TIFF facts nc can validate. Do not expand this task
  into a DNG decoder.
- Spike TIFF tag 34675 access early; profile extraction is useful for inspection
  even though normal negative conversion does not apply it.
- Update `design-spec.md`, CLI help, recipes, reports, and exit-code
  documentation together so no surface continues to promise automatic input
  ICC conversion.

## How to Verify

- The known SilverFast HDR/HDRi samples independently resolve a linear transfer
  and scanner-device meaning, then reach Dmin/density without an RGB color
  transform.
- A synthetic Gamma-1 TIFF without supported raw-mode evidence remains `Unknown`
  rather than being treated as scanner measurements.
- Embedded-profile and non-embedded variants with the same raw semantics choose
  the same conversion path; inspection reports the profile difference.
- Synthetic contradictory gamma/HDR metadata and unknown encodings are rejected
  loudly by `convert`, while `inspect` reports why they are ambiguous.
- No input is silently labelled linear Rec.709 merely because it lacks an ICC
  profile.
- IR is bit-identical before and after input resolution.
- CLI/recipe merge and conflict tests cover the revised input controls, and the
  design specification no longer promises pre-density ICC application.
- Table tests cover every allowed/forbidden transfer × meaning combination,
  evidence precedence, contradictory explicit assertions, and provenance in the
  resolved report. The old combined assertion is rejected or emits a pinned
  deprecation migration error; it never silently asserts both axes.

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
