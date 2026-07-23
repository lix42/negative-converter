# ISO Gain-Map HDR Output

## Goal

Write a standards-based, backward-compatible HDR still image containing an SDR
base rendition plus an ISO 21496-1 gain map. Initially target HEIC, subject to
the encoder decision from the HDR spike.

## Design

Combine two independently valid renderings of the mapped film image:

```text
SDR Display P3 base + display-HDR rendition → gain map + ISO metadata → container
```

Both renditions must originate from the same ACEScg film-rendering pixels and identical
resolved shared linear adjustments (white balance, exposure, and black/white placement).
They diverge only in SDR-versus-HDR reference-white, tone, gamut, and transfer
rendering.

Before transfer encoding, convert both renditions into the exact common linear
color domain required by the selected ISO 21496-1 profile/metadata contract and
derive gain ratios there. Never divide encoded Display P3 and PQ/BT.2020 channel
values: their primaries and nonlinear transfer functions are different. Encode
the resulting scale/offset/gamma/headroom metadata and ensure the SDR base remains
the default representation for unaware readers. Use the neutral public
name `gain-map-hdr`; do not brand the format for one platform.

HEIC is the intended first container for the product default because it can
carry a compact modern rendition, but the implementation must follow the
spike's cross-platform/licensing decision. Keep container code separate enough
that an ISO-compatible JPEG gain-map encoder can be added without changing the
rendering model.

## How to Verify

- A standards-aware decoder reconstructs the expected HDR rendition from the SDR
  base and gain map within declared tolerance.
- Tests prove both renditions originate from the identical mapped/shared-
  adjusted source and that gain ratios are computed in the pinned common linear
  domain, not from encoded P3/PQ/HLG samples.
- A decoder that ignores the gain map displays the correct SDR Display P3 base.
- Metadata, orientation, dimensions, profile/CICP information, and content
  headroom survive round trip.
- Files render as HDR in target macOS/iOS software and in at least one supported
  non-Apple implementation; fallback is tested independently.
- Encoding is deterministic to the degree promised by the selected codec, with
  nondeterministic metadata removed or normalized.

## Dependencies

- [SDR display rendering](sdr-display-rendering.md)
- [Display-HDR rendering](hdr-display-rendering.md)
