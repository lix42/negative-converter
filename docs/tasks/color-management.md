# Color Management

## Goal

Transform pixels from the linear working space into the selected output color
space using ICC profiles, and choose the output profile with a depth-aware
default (sRGB for u16, a wide-gamut space for f32). Provide the ICC blob that
`tiff-encode` embeds.

## Design

`pipeline/color.rs` using `lcms2`:

```rust
pub enum OutputSpace { SRgb, ProPhoto, AcesCg, Custom(PathBuf) }

/// Resolve the effective output space from an explicit choice + output depth.
pub fn resolve_output_space(explicit: Option<OutputSpace>, depth: OutDepth) -> OutputSpace;

/// Apply working-space -> output-space transform in place (or to a new buffer).
pub fn to_output(img: &mut LinearImage, space: &OutputSpace) -> Result<(), NcError>;

/// The ICC bytes to embed for a given space.
pub fn icc_profile(space: &OutputSpace) -> Result<Vec<u8>, NcError>;
```

- **Depth-aware default:** if no explicit profile is given, `u16 → sRGB`,
  `f32 → wide-gamut` (default value still open — ProPhoto vs linear ACEScg vs
  Rec.2020; pick one and record it in `progress.md`).
- Working space is linear; build an lcms2 transform from the working profile to
  the chosen output profile. For sRGB output, apply the sRGB tone curve; for
  linear/scene-referred wide-gamut output, keep it linear.
- `Custom(path)` loads a user-supplied ICC file.

## Implementation Suggestion

- Resolve `lcms2` API via Context7. Use built-in profile constructors where
  available (sRGB), and bundle/standard profiles for ProPhoto/ACEScg, or
  synthesize them from primaries + TRC.
- Keep the transform operating on the `f32` RGB buffer directly to avoid an extra
  copy; respect channel order.
- Decide and document whether the output is display-referred (tone-curved) or
  scene-referred (linear) per space — this affects how `tiff-encode` data looks.

## How to Verify

- `resolve_output_space(None, U16)` → sRGB; `resolve_output_space(None, F32)` →
  the chosen wide-gamut default; explicit choice overrides both.
- A neutral linear gray transforms to the expected sRGB-encoded value.
- `icc_profile` returns non-empty, valid ICC bytes for each built-in space and
  loads a `Custom` profile from disk.
- Round-trip / known-value tests on at least sRGB pass within tolerance.

## Dependencies

- [Project foundation and core types](project-foundation.md)
