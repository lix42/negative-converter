# SDR Display Rendering

## Goal

Render characterized linear ACEScg into an independently valid rendered-linear
SDR rendition for Display P3 or sRGB. Own reference-white, tone, chromatic
adaptation, and gamut mapping; the destination-output stage owns transfer
encoding and signaling for standalone SDR and the base image of gain-map HDR.

## Design

Implement a pure deterministic display branch:

```text
characterized linear ACEScg
→ shared linear display adjustments (white balance/exposure/black/white placement)
→ SDR reference-white and tone mapping
→ chromatic adaptation + destination gamut mapping
→ rendered linear Display P3 or sRGB
```

Reference white, SDR highlight roll-off, black/white placement, and gamut behavior must
be explicit resolved parameters, not side effects of clipping or an ICC transform.
This task is the sole owner of ACEScg → rendered linear destination RGB,
including chromatic adaptation and gamut mapping. The Display P3 task consumes
the rendered linear P3 values, applies the standard transfer encoding, and
attaches profile/signaling metadata. The compatibility path uses
the same rendering model with sRGB as the smaller destination gamut.

Do not derive this base by clipping PQ/HLG pixels. It and the HDR rendition are
two intentional renders of the same characterized source, coordinated by the
reference-white/headroom decisions from the HDR spike.

## Implementation Suggestion

- Reuse `post-characterization-render-pipeline`'s shared linear display-adjustment
  stage; keep SDR highlight/tone policy separate from HDR highlight/tone policy.
  Gamut mapping remains in this task; destination transfer encoding does not.
- Return rendered-linear destination pixels plus resolved SDR metadata, including
  destination gamut, reference white, and the required transfer/profile
  identifier. Standalone output encoding consumes this result; gain-map
  construction consumes the pre-transfer pixels and must not infer reference
  white or color space from an encoded container.
- Make clipping a measured/reportable terminal condition, never the tone mapper.

## How to Verify

- Neutral ramps remain neutral and monotonic from linear ACEScg through both P3
  and sRGB outputs.
- Reference white and black land at their declared rendered-linear values;
  destination-output tests verify the corresponding encoded values. Highlights
  roll off without silent channel clipping.
- Out-of-gamut synthetic colors follow the documented mapping and remain finite.
- Golden tests pin the rendered-linear P3 and sRGB renditions for the same
  synthetic scene; destination-output tests separately pin transfer encoding.
- The P3 result embeds/signals the standardized Display P3 profile and the sRGB
  result embeds/signals sRGB.

## Dependencies

- [Post-characterization render pipeline](post-characterization-render-pipeline.md)
- [Display P3 output](display-p3-output.md)
- [HDR still-output spike](hdr-output-spike.md)
