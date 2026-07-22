# HDR Still-Output Spike

## Goal

Resolve the standards, container, encoder, metadata, tone-reference, and
cross-platform decisions needed for display HDR before production code is
committed. Produce a decision note and concretely scoped implementation inputs.

## Design

Evaluate two distinct deliverables:

- single-rendition ISO HDR: BT.2020 with Rec.2100 PQ or HLG, at least 10 bits;
- backward-compatible gain-map HDR: an SDR base plus ISO 21496-1 gain map and
  metadata, initially targeting HEIC while evaluating JPEG interoperability.

Decide:

- cross-platform Rust encoder/library versus platform APIs;
- HEIF/HEIC patent, licensing, packaging, and static-link implications;
- ICC versus CICP/nclx signaling and all required HDR/gain-map metadata;
- reference white, content headroom, peak policy, tone mapping, and gamut mapping;
- SDR base space (target: Display P3) and whether the gain map is luminance-only
  or RGB;
- deterministic encoding expectations and metadata normalization;
- support and fallback behavior in macOS/iOS, Android, browsers, and ordinary SDR
  readers.

Use standards-neutral product terminology: `gain-map-hdr`, not Apple HDR or
Ultra HDR. ISO 21496-1 defines the gain-map model; container conformance must be
specified separately.

## How to Verify

- The decision note names exact standards/versions, container brands, metadata,
  encoder API, licensing constraints, and supported-platform matrix.
- Small reference files render as intended in target HDR software and degrade to
  the declared SDR representation in non-HDR readers.
- Pixel/headroom measurements verify that software is showing HDR rather than a
  visually similar SDR tone map.
- Follow-up rendering and encoder tasks can implement the decision without
  reopening fundamental format choices.

## Dependencies

- [Color management](color-management.md)
