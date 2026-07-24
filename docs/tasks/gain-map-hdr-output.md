# ISO Gain-Map HDR Output

## Goal

Write a standards-based, backward-compatible HDR still image containing an SDR
base rendition plus an ISO 21496-1 gain map. Initially target JPEG with both
final ISO and Android Ultra HDR v1 compatibility metadata; defer HEIC until a
portable encoder and licensing policy satisfy the HDR spike.

## Design

Combine two independently valid renderings of the mapped film image:

```text
SDR Display P3 base + display-HDR rendition → gain map + ISO metadata → container
```

Both renditions must originate from the same ACEScg film-rendering pixels and identical
resolved shared linear adjustments (white balance, exposure, and black/range placement).
They diverge only in SDR-versus-HDR reference-white, tone, gamut, and transfer
rendering.

Before transfer encoding, convert both renditions into linear Display P3 and
derive an RGB gain map there. Never divide encoded Display P3 and PQ/BT.2020
channel values: their primaries and nonlinear transfer functions are different.
Encode the resulting scale/offset/gamma/headroom metadata and ensure the SDR base
remains the default representation for unaware readers. The initial target is
an 8-bit Display P3 JPEG base, 4:4:4 at quality 95, plus a half-resolution RGB
gain map. These codec settings are provisional until the acceptance task freezes
measured codec-aware bounds. Use the neutral public name `gain-map-hdr`; do not
brand the format for one platform.

Keep display/content headroom distinct from serialized gain-map parameters. For
the initial 203/1000 policy, linear display headroom is
`1000 / 203 = 4.926108...` and its logarithmic capacity is
`log2(1000 / 203) = 2.300448...`. `libultrahdr`'s C API exposes capacity in
linear scale, while Ultra HDR XMP serializes `HDRCapacityMax`, `GainMapMax`, and
the corresponding minima in log2 units. Convert deliberately at that boundary;
never serialize `4.926108` as an Ultra HDR XMP logarithmic capacity.

Before selecting offsets or computing gain, express both linear Display P3
renderings in reference-white-relative units. SDR/reference white is `1.0`.
Divide HDR absolute luminance by the pinned 203 cd/m² reference white, so
203 nits is `1.0` and 1000 nits is `4.926108...`. Express both offsets in this
same normalized domain. Mixing absolute-nit HDR values with `[0,1]` or
reference-white-relative SDR values is a unit error and must fail with a
diagnostic before gain-map generation. Preserve unit/domain tags at the renderer
boundary (or use distinct typed buffers) so this rejection is enforceable rather
than inferred from sample magnitude.

After selecting and pinning per-channel positive finite offsets, derive every
gain sample in that common normalized linear Display P3 domain from exactly:

```text
gain_c = (HDR_c + offset_hdr,c) / (SDR_c + offset_sdr,c)
```

Ultra HDR guidance suggests `1/64` offsets, but do not assume that value has
final ISO-equivalent semantics until the licensed normative review confirms the
mapping. Require every rendered HDR/SDR sample to be finite and nonnegative.
Require every offset, adjusted denominator, and resulting gain to be finite and
positive before taking a logarithm. Any violation fails loudly; do not inject an
arbitrary epsilon, silently clamp a sample, or define `0/0`.

Derive per-channel gain-map minima and maxima from that exact offset-adjusted
formula. They need not equal the global display-headroom ratio because the
independent SDR and HDR tone maps differ. Serialize each metadata dialect from
this one canonical scale/offset/gamma/capacity model, with explicit unit
conversions, and require the ISO 21496-1 and Ultra HDR v1 interpretations to
agree semantically.

Use a narrow, audited Rust FFI around a corrected `libultrahdr` release or
reviewed patch. Stable 1.4.0 is not acceptable unchanged: its APP2-before-JFIF
marker ordering failed macOS ImageIO in the spike. Verify final ISO 21496-1:2025
serialization rather than the earlier ISO/TS draft identifier, include Ultra
HDR v1 compatibility metadata in the same file, and add the required Adobe
gain-map license notice if that patent grant is used. Keep container code
separate enough that final-standard HEIF/AVIF gain-map encoders can be added
without changing the rendering model. See
[the decision note](../hdr-output-spike.md).

## How to Verify

- An independent ISO 21496-1 implementation and an independent Ultra HDR v1
  implementation each reconstruct the canonical HDR/headroom from the same file
  within the pinned bounds.
- Independent metadata inspection proves both dialects express the same
  scale/offset/gamma/capacity semantics after their specified unit conversions,
  including log2 Ultra HDR XMP values; a deliberately conflicting dual-metadata
  fixture proves a dual-aware decoder selects ISO 21496-1.
- Tests prove both renditions originate from the identical mapped/shared-
  adjusted source and that gains use the pinned offset-adjusted formula in the
  common reference-white-relative linear domain, not encoded P3/PQ/HLG samples
  or mixed absolute/normalized units.
- Equal SDR/HDR reference-white samples (`1.0`) with equal offsets yield gain
  exactly 1. A peak fixture converts 1000 nits to `4.926108...` before the
  formula, then computes expected gain from the actual independently tone-mapped
  SDR sample and both offsets; it must not assert gain `4.926108...` merely from
  display headroom.
- Black, near-black, and zero-channel fixtures prove pinned positive offsets
  yield finite positive gains without arbitrary epsilon handling. Negative or
  nonfinite samples, nonpositive/nonfinite offsets or denominators, overflow,
  and nonpositive/nonfinite gains fail before logarithm/serialization.
- A mixed-units fixture passes absolute-nit HDR with normalized SDR and verifies
  a stable unit/domain diagnostic with no partial output.
- A decoder that ignores the gain map displays the correct SDR Display P3 base.
- Metadata, orientation, dimensions, profile/CICP information, and content
  headroom survive round trip.
- Files render as HDR in target macOS/iOS software and in at least one supported
  non-Apple implementation; Android 14 Ultra HDR v1, Android 15+ ISO metadata,
  current Chromium/Safari, Firefox fallback, and an ordinary JPEG reader are
  tested independently.
- JFIF APP0 precedes ISO/MPF APP2 metadata, final ISO and compatibility metadata
  independently decode through the same gain map to equivalent HDR results, and
  the default JPEG image opens without repair.
- Encoding is deterministic to the degree promised by the selected codec, with
  nondeterministic metadata removed or normalized.

## Dependencies

- [SDR display rendering](sdr-display-rendering.md)
- [Display-HDR rendering](hdr-display-rendering.md)
