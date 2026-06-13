# SilverFast HDR/HDRi Decode

## Goal

Read a SilverFast scan file and produce a `LinearImage` in linear `f32`: handle
both **HDR (48-bit RGB)** and **HDRi (64-bit RGB + IR)** variants, preserving the
IR plane when present. This is the pipeline's input contract — nothing downstream
needs to know the on-disk format.

## Design

`io/decode.rs`:

```rust
pub fn decode(path: &Path) -> Result<LinearImage, NcError>;
```

Steps:
1. Open as a TIFF (SilverFast HDR/HDRi are TIFF-family). Use the `tiff` crate;
   fall back to manual IFD inspection for scanner-specific extras.
2. Read `SamplesPerPixel` / `BitsPerSample`: expect 16-bit samples, 3 channels
   (HDR) or 4 channels (HDRi, 4th = IR).
3. Normalize 16-bit integer samples to `f32` in `[0,1]` (divide by 65535).
   Treat data as **linear** (raw-ish scanner data); do not apply any gamma here.
4. Split into `rgb: Vec<f32>` and, if a 4th channel exists, `ir: Some(Vec<f32>)`.
5. Populate width/height; return `LinearImage`.

Emit a structured note (channel count, bit depth, any unrecognized tags) for the
JSON report — `inspect` will surface this. Return `NcError::Unsupported` for layouts
we can't yet handle (e.g. non-16-bit, >4 channels) rather than guessing.

## Implementation Suggestion

- There is **no published spec** for the exact HDRi tag/IFD layout — validate
  against the user's real sample files. Build defensively: detect channel count
  from the TIFF tags rather than assuming.
- Keep a small `DecodeInfo` struct (format variant, channels, bps, warnings) and
  return it alongside (or log it) so `inspect`/reports can show what was found.
- Watch for planar vs. chunky (`PlanarConfiguration`) layout and strip vs. tile
  organization — the `tiff` crate exposes these; handle both or fail clearly.
- Don't act on the IR channel here; just carry it. (Dust removal is a roadmap item.)

## How to Verify

- Decodes a real SilverFast HDR file → `LinearImage` with `ir == None`, correct
  dimensions, values in `[0,1]`.
- Decodes a real SilverFast HDRi file → `ir == Some(..)` of length `w*h`.
- A non-16-bit or unsupported-channel file returns `NcError::Unsupported` with a
  clear message (no panic).
- Unit test on a small synthetic 16-bit TIFF (3- and 4-channel) confirms correct
  normalization and channel split.

## Dependencies

- [Project foundation and core types](project-foundation.md)
