# Ultra HDR v1 Gain-Map JPEG Output

## Goal

Write a backward-compatible HDR JPEG containing an ordinary Display P3 SDR base
plus a public Ultra HDR v1 luminance gain map. Expose it explicitly as
`--output-preset ultra-hdr-v1`; do not label it ISO-conformant or make it the
default before the downstream final-ISO and preset tasks are complete.

## Design

Combine two independently valid renderings of the same mapped film image:

```text
SDR Display P3 base + display-HDR rendition
  → common linear Display P3
  → Ultra HDR v1 gain map + XMP/MPF/GContainer
  → JPEG
```

Both renditions originate from one ACEScg film-rendering image and identical
resolved shared linear adjustments. They diverge only in SDR-versus-HDR tone
and gamut rendering. Before transfer encoding, convert both to linear Display
P3 relative to the fixed 203 cd/m² reference white. Never divide encoded
Display P3 and PQ/BT.2020 values.

Keep the canonical full-resolution RGB ratio model for the future ISO
serializer. Legacy XMP mode cannot signal a multi-channel gain map, so derive
this preset's one supported gain channel from Display P3 luminance:

```text
gain = (luma(HDR) + offset_hdr) / (luma(SDR) + offset_sdr)
```

Pin both offsets to `1/64` and gain gamma to `1`. Require every input, adjusted
denominator/numerator, gain, and logarithm to be finite and valid; do not inject
an epsilon or silently define `0/0`. Derive gain minima/maxima from
the actual pixels, express them as log2 metadata, encode each gain into the
public normalized logarithmic recovery representation, then downsample that
floating recovery image to half width and half height with bilinear-or-better
filtering. Quantize once to 8-bit grayscale using the documented nearest-integer rule.

Encode an 8-bit Display P3 primary JPEG at quality 95 with 4:4:4 chroma and an
8-bit grayscale secondary gain-map JPEG. Embed the synthesized Display P3 ICC profile
in the primary image. Preserve normalized orientation and deterministic
metadata; never add timestamps, random identifiers, or draft/final ISO markers.

Use a narrow audited Rust boundary around google/libultrahdr pinned at or after
approved marker-order merge commit
`11ac0c325bbf56ecf8be8704ff0f79fc9e1aac77`. Statically package the required
native code and the pinned libjpeg-turbo 3.1.0 source so macOS and Linux builds
do not depend on a machine-installed library. Keep the binding private and translate every native
error into `NcError`; document every unsafe pointer/lifetime invariant.

The explicit preset owns its complete policy and accepts only `.jpg`/`.jpeg`.
It rejects legacy TIFF depth/profile controls rather than silently ignoring
them. Recipe/CLI merge, resolved reports, transactional writes, roll naming,
and making the neutral dual-dialect `gain-map-hdr` output the default remain
owned by `output/presets`; this task activates only the explicit single-file
`convert` path needed to use and verify Ultra HDR v1 now.

Before activation, add a gain-map-specific memory profile. It must count the
shared adjusted ACEScg source, SDR rendition, converted HDR rendition,
full-resolution gain ratios/recovery values, half-resolution map, quantized
codec inputs, and native output staging that are simultaneously live. Calibrate
the conservative model against measured peak RSS without weakening the existing
legacy profile.

Distribution documentation must include the prominent Adobe notice:
“This product includes Gain Map technology under license by Adobe.”

## How to Verify

- An ordinary JPEG reader displays the intended Display P3 SDR primary image.
- libultrahdr reconstructs the HDR rendition and reports the intended
  identical-channel offsets, gamma, gain extrema, and 203/1000 headroom.
- Independent metadata inspection finds valid Ultra HDR v1 XMP plus MPF and
  GContainer linkage, with no ISO or ISO/TS marker.
- Equal SDR/HDR reference-white samples produce gain exactly 1; black,
  near-black, saturated, odd-dimension, and independently tone-mapped peak
  fixtures reconstruct within measured codec-aware bounds.
- Tests pin log2 normalization, gamma, half-resolution bilinear sampling,
  8-bit rounding, grayscale component count, ICC embedding, orientation, dimensions,
  and JFIF/APP/MPF marker ordering.
- Negative/nonfinite samples, invalid offsets or gamma, invalid extrema,
  overflow, mixed units, mismatched dimensions, unsupported suffixes, native
  errors, and partial writes fail loudly.
- `nc convert input.tif -o output.jpg --output-preset ultra-hdr-v1` succeeds and
  records the explicit non-ISO format. The same preset with `.tiff` fails
  without creating output.
- Memory-preflight tests and measured RSS cover the actual overlapping buffers.
- Fixed same-build inputs and parameters produce byte-identical files.
- The corrected file is exercised with current macOS ImageIO and an Android
  Ultra HDR implementation when those environments are available; unsupported
  readers retain the SDR fallback.

## Dependencies

- [SDR display rendering](sdr-display-rendering.md)
- [Display-HDR rendering](hdr-display-rendering.md)
