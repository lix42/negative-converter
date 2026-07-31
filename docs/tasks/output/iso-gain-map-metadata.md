# Final ISO Gain-Map Metadata

## Goal

Extend NC's Ultra HDR v1 gain-map JPEG with final ISO 21496-1:2025 metadata
using the same SDR base, gain-map image, and canonical metadata model. Do not
claim ISO conformance until an authoritative final-standard checklist or
independent oracle verifies the serialized bytes and reconstruction behavior.

## Design

Keep `output/gain-map-hdr-output`'s public Ultra HDR v1 JPEG fully valid and add
the final ISO metadata dialect without generating a second gain map. Serialize
both dialects from the one canonical scale/offset/gamma/capacity model, with
explicit linear-versus-logarithmic unit conversions.

Use a permitted human-authored conformance checklist or equivalently
authoritative final-standard implementation to pin:

- the final identifier, version, fields, ranges, byte order, and JPEG placement;
- offset, gamma, capacity, and per-channel semantics;
- the legal mapping between ISO and Ultra HDR v1 metadata; and
- dual-aware decoder precedence when both dialects are present.

The ISO extension must remain separate from rendering, gain-map quantization,
and the Ultra HDR container implementation so a conformance correction changes
only the ISO serializer and its tests. The later `output/presets` task may make
the neutral `gain-map-hdr` dual-dialect output the default only after this task
is complete.

## How to Verify

- An independent ISO 21496-1 implementation and an independent Ultra HDR v1
  implementation reconstruct the canonical HDR rendition from the same file
  within pinned codec-aware bounds.
- Metadata inspection proves both dialects express the same
  scale/offset/gamma/capacity semantics after unit conversion.
- A deliberately conflicting fixture proves a dual-aware decoder selects the
  ISO metadata.
- The final identifier and every mandatory field match the approved
  final-standard checklist; no ISO/TS draft identifier remains.
- The unchanged primary JPEG opens as SDR in an ordinary JPEG reader.
- Android 15+ and target Apple software recognize the dual-dialect file as HDR.

## Dependencies

- [Ultra HDR v1 gain-map JPEG output](gain-map-hdr-output.md)
