# TIFF Encode and Output

## Goal

Write a rendered positive `LinearImage` to a TIFF file as **16-bit integer** or
**32-bit float**, embed the chosen ICC profile, auto-promote to BigTIFF when
needed, optionally export the IR plane, and write the sidecar JSON recipe.

## Design

`io/encode.rs`:

```rust
pub struct EncodeOptions<'a> {
    pub depth: OutDepth,            // U16 | F32
    pub icc_profile: Option<&'a [u8]>, // ICC blob to embed
    pub bigtiff: BigTiff,          // Auto | On | Off
}

pub fn encode_tiff(path: &Path, img: &LinearImage, opts: &EncodeOptions) -> Result<(), NcError>;
pub fn export_ir(path: &Path, img: &LinearImage) -> Result<(), NcError>;
pub fn write_sidecar(path: &Path, recipe_json: &str) -> Result<(), NcError>;
```

- **u16:** clamp `f32` `[0,1]` → `u16` `[0,65535]`. **f32:** write float samples
  directly (no clamp; preserve extended range for HDR).
- Embed the ICC profile via the TIFF `ICCProfile` tag (34675).
- **BigTIFF auto:** estimate output size; if it would exceed the ~4 GB classic
  TIFF limit, write BigTIFF (64-bit offsets). `On`/`Off` force the choice.
- `export_ir` writes the IR plane as a single-channel 16-bit (or f32) TIFF.
- `write_sidecar` writes the effective recipe JSON next to the output
  (`<output>.json`).

This task takes the ICC blob as bytes — it does **not** select or build the
profile (that's `color-management`). It just embeds what it's given.

## Implementation Suggestion

- Use the `tiff` crate's encoder; check its f32 sample and BigTIFF support and, if
  a gap exists, note it in `progress.md` and write the tag(s) manually.
- Compute the BigTIFF size estimate from `width*height*channels*bytes_per_sample`
  plus a margin for tags/strips.
- Round-half-to-even or simple round for u16 quantization — pick one and document
  it; keep it deterministic.

## How to Verify

- Encoding a known `LinearImage` at `u16` and reading it back yields matching
  pixels (within quantization).
- `f32` output round-trips exactly for representative values, including >1.0.
- A synthetic large image triggers BigTIFF under `Auto`; `Off` keeps classic
  TIFF; `On` forces BigTIFF.
- Embedded ICC bytes are present in the output and re-readable.
- `export_ir` produces a valid single-channel file; `write_sidecar` writes valid
  JSON.

## Dependencies

- [Project foundation and core types](project-foundation.md)
