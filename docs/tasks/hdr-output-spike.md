# HDR Still-Output Spike

## Goal

Resolve the standards, container, encoder, metadata, tone-reference, and
cross-platform decisions needed for display HDR before production code is
committed. Produce a decision note and concretely scoped implementation inputs.

## Design

Evaluate two distinct deliverables:

- single-rendition ISO HDR: BT.2020 with Rec.2100 PQ or HLG, at least 10 bits,
  targeting AVIF;
- backward-compatible gain-map HDR: an SDR base plus ISO 21496-1 gain map and
  metadata, targeting JPEG first and deferring HEIC pending portable final-
  standard encoder support and licensing approval.

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

The provisional decision and prototype evidence are recorded in
[the HDR still-output decision note](../hdr-output-spike.md). The spike
remains open only for the normative-text review listed there. Encoder
conformance, physical-device interoperability, and final codec thresholds are
pre-shipping gates owned by the downstream encoder and acceptance tasks, not
prerequisites for completing this spike.

## How to Verify

- The decision note names exact standards/versions, container brands, metadata,
  encoder API, licensing constraints, and supported-platform matrix.
- Licensed normative review pins the exact mandatory fields, serialization,
  offset semantics, and dual-metadata mapping needed by the implementation
  tasks, or records a required decision revision.
- Prototype evidence and the target platform matrix are sufficient to scope the
  follow-up work; actual encoder conformance, device rendering, fallback, and
  pixel/headroom results are explicitly assigned to the encoder and acceptance
  tasks rather than required to close this spike.
- Follow-up rendering and encoder tasks can implement the decision without
  reopening fundamental format choices.

## Dependencies

- [Color management](color-management.md)
