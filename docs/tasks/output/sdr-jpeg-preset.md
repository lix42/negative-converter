# A plain SDR JPEG output

## Goal

Add an SDR JPEG output with no gain map: the same Display P3 (or sRGB) rendition the
`display-p3` / `compatibility` TIFF presets produce, encoded as an 8-bit JPEG for
sharing. Today nc's only JPEG outputs are `gain-map-hdr` and `ultra-hdr-v1`, both of
which package a gain map.

## Why now

On 2026-09-13 the user set the product shape: **SDR lossless is the default**
(`output/display-p3-default`), HDR lossless is supported (`hdr-linear-tiff`,
`hdr-pq-tiff`, `hdr-hlg-tiff`, all shipped), **SDR JPEG is supported**, and HDR JPEG
(`gain-map-hdr`) is good to have. The third of those does not exist.

## What is known

- The pixels exist: `pipeline::sdr` already renders the SDR rendition and
  `color::encode_rendered_sdr` derives its transfer and profile. The gain-map presets'
  *base* image is exactly this rendition as an 8-bit JPEG, produced by the pure-Rust
  `jpeg_encoder` at a pinned quality of 95 with the Display P3 ICC embedded, so the
  encode path exists too; what is missing is a preset that writes it alone.
- Encoder settings are **pinned parts of a preset, not knobs** (the `JPEG_QUALITY`
  and AVIF `cq_level` precedent), so repeated encodes on one build are byte-identical.
- Every preset states its suffix (`cli::required_extensions`) and its canonical derived
  spelling (`cli::derived_extension`, `jpg`), and must add and calibrate its own
  `memory::RunProfile` on two frame sizes before activation. The JPEG-only profile is
  smaller than `UltraHdrV1`'s four display buffers; measure, do not inherit.
- The telemetry `conversion.preset` enum grows by one more member; the wire-shape
  question is parked in `output/sdr-preset-followups`.

## Open questions

1. One preset per gamut (`display-p3-jpeg`, `compatibility-jpeg`) mirroring the TIFF
   pair, or one JPEG preset with the gamut as a selector? The atomic-preset rule is
   simpler with names.
2. Chroma subsampling: 4:4:4 as the gain-map base uses, or 4:2:0 for size.
3. Whether sRGB output embeds an ICC at all (JPEG readers assume sRGB) while Display P3
   must.
4. The telemetry `output_depth` label for an 8-bit primary already exists (`u8`);
   confirm the report's other depth claims.

- **2026-09-25 (`nf-destinations/preset-set`):** on the new chain an SDR JPEG is a
  knob combination (`--range sdr --container jpeg`, a row refused as "not yet"), so
  open question 1 (one preset per gamut) is moot there; it stands only for the legacy
  chain, which retires at `nf-core/default-flip`.

## How to Verify

- `hanten convert --output-preset <name> -o out.jpg` writes an 8-bit JPEG whose decoded
  pixels match the `display-p3` TIFF's within the codec's pinned error bounds, with
  the Display P3 ICC embedded and no MPF, XMP gain-map or ISO 21496-1 segment present.
- `hanten roll` derives `<stem>_positive.jpg`; a mismatched suffix is refused.
- `RunProfile` calibrated on two frame sizes; `docs/using-nc.md` updated by running the
  binary.

## Dependencies

- [Output presets and guidance](presets.md)
- [SDR display rendering](sdr-display-rendering.md)
