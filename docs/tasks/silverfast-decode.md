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

**On-disk layout (verified against the real sample files, 2026-06):** SilverFast
HDR/HDRi are **uncompressed little-endian ClassicTIFFs**, `PlanarConfiguration=1`
(chunky), 16-bit **unsigned** samples (no `SampleFormat` tag). Critically, the IR
channel is **not** a 4th interleaved sample — HDRi stores it as a **second IFD**:

- **HDR**: a single IFD — `SamplesPerPixel=3`, `BitsPerSample=16/16/16`,
  `Photometric=RGB`, `NewSubfileType=0`. No IR.
- **HDRi**: **two IFDs**. IFD0 is identical to the HDR image; IFD1 is the IR plane
  — `SamplesPerPixel=1`, `BitsPerSample=16`, `Photometric=BlackIsZero`,
  `NewSubfileType=4`, **same width/height** as IFD0.

So HDR vs HDRi is detected **structurally** — by the presence of the second image
(`decoder.more_images()`) — *not* from metadata. The `Silverfast:HDRScan="Yes"`
XMP flag is present on **both** variants and must not be used to detect IR.

Steps:
1. Open as a TIFF with the `tiff` crate (`Decoder::new`).
2. Read IFD0: require `ColorType::RGB(16)` (3 samples, 16-bit), chunky. Reject
   anything else (planar multi-sample, non-16-bit, wrong channel count) with
   `NcError::Unsupported` — don't guess. `read_image()` → `DecodingResult::U16`.
3. Normalize 16-bit integer samples to `f32` in `[0,1]` (divide by 65535). Treat
   data as **linear** (raw-ish scanner data); do not apply any gamma here.
4. If `decoder.more_images()`, advance (`next_image()`) and read IFD1 as the IR
   plane: require `ColorType::Gray(16)`, 1 sample, **dimensions equal to IFD0**;
   normalize to `ir: Some(Vec<f32>)`. Mismatched dims / unexpected layout →
   `Unsupported`. Any further IFDs beyond IFD1 → record a warning and ignore.
5. Build via `LinearImage::new(w, h, rgb, ir)` (validated constructor); return it
   alongside a `DecodeInfo` (below). Map `tiff` parse/IO errors → `NcError::Decode`.

Emit a structured `DecodeInfo` (format variant, channel count, bit depth, IR
presence, scanner make/model/software, unrecognized-tag warnings) for the JSON
report — `inspect` will surface this. Return `NcError::Unsupported` for layouts we
can't yet handle (non-16-bit, unexpected channel/plane counts) rather than guessing.

## Implementation Suggestion

- There is **no published spec** for the exact HDRi tag/IFD layout — the layout
  above is reverse-engineered from the user's real sample files. Build defensively:
  detect the variant from structure (channel count + presence of a second IFD)
  rather than from XMP/metadata.
- Return a small `DecodeInfo` struct (format variant, channels, bps, IR presence,
  scanner make/model/software, warnings) **alongside** the `LinearImage` (change
  the stub signature to `decode(&Path) -> Result<(LinearImage, DecodeInfo)>`) so
  `inspect`/reports can show what was found without re-parsing the file.
- The `tiff` crate's `read_image()` only returns the first sample plane under
  `PlanarConfiguration=2`; all known samples are chunky (`=1`), so guard against
  planar-multi-sample with an `Unsupported` error rather than silently dropping
  channels. (Strip vs. tile is handled transparently by the crate.)
- Don't act on the IR channel here; just carry it. (Dust removal is a roadmap item.)

## How to Verify

- Decodes a real SilverFast HDR file → `LinearImage` with `ir == None`, correct
  dimensions, values in `[0,1]`; `DecodeInfo.format == Hdr`.
- Decodes a real SilverFast HDRi file → `ir == Some(..)` of length `w*h`;
  `DecodeInfo.format == Hdri`.
- Real-file tests run against committed fixtures (`tests/fixtures/` — one
  `48bit-small` HDR + one `64bit-small` HDRi sample) so they also run in CI.
- A non-16-bit or unsupported-layout file returns `NcError::Unsupported` with a
  clear message (no panic).
- Unit test on a small synthetic 16-bit TIFF — single-IFD 3-channel (HDR) and
  two-IFD RGB + grayscale-IR (HDRi) — confirms correct normalization, IR split,
  and structural HDR/HDRi detection.

## Dependencies

- [Project foundation and core types](project-foundation.md)
