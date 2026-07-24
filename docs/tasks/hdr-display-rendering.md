# Display-HDR Rendering

## Goal

Render NC's intentional linear ACEScg film values into a standards-defined HDR
display signal with controlled reference white, highlight behavior, gamut, and
headroom. Produce BT.2020 PQ first and retain a defined HLG option where useful.

## Design

Implement a pure, deterministic stage before container encoding:

```text
linear ACEScg film rendering
→ shared linear display adjustments (same WB/exposure/black/white as the SDR branch)
→ HDR reference-white placement
→ highlight tone mapping
→ gamut mapping to BT.2020
→ Rec.2100 PQ or HLG encoding
→ encoded HDR image + headroom/luminance metadata
```

PQ is the primary still-image path. HLG is an explicit interoperability/broadcast
choice, not an internal working space. Parameters must name physical/display
meaning (for example diffuse/reference white and target peak) rather than a
generic gamma. The separately tracked `sdr-display-rendering` task owns the SDR
rendition; do not derive that base by blindly clipping the HDR signal.

The HDR spike pins diffuse/reference white at 203 cd/m² and the initial target
peak at 1000 cd/m², giving linear headroom `1000 / 203 = 4.926108...`
(`log2(1000 / 203) = 2.300448...` stops).
The output stage uses current ISO 22028-5:2026 and Rec. BT.2100-3. The
single-rendition container task consumes 10-bit full-range BT.2020 4:4:4 pixels
and writes AVIF with CICP `9/16/9` for PQ or `9/18/9` for HLG. HLG must pin its
OETF/OOTF/system-gamma and reference-display assumptions rather than inheriting
an ambient default. See [the spike decision](../hdr-output-spike.md).

The stage returns pixels and metadata but does not own HEIC/gain-map packaging.
The separately tracked `hdr-avif-output` task owns AV1 encoding and AVIF
container/profile conformance for the PQ/HLG signals.
For a paired gain-map render, it must consume the same resolved shared-adjustment
parameters as the SDR branch; only display-specific tone/gamut/transfer policy
diverges after that common source.

## How to Verify

- Standard transfer-function vectors for PQ/HLG and BT.2020 conversion pass.
- Neutral ramps remain neutral; output is monotonic and finite over supported
  scene ranges.
- Reference white, target peak, and content headroom land at declared encoded
  and measured luminance values.
- A monotonic luminance-preserving highlight shoulder reaches the declared peak;
  out-of-gamut colors use documented hue-preserving compression without silent
  per-channel clipping.
- Golden tests pin deterministic PQ and HLG renditions for synthetic scenes.

## Dependencies

- [Film-master and shared display pipeline](film-master-render-pipeline.md)
- [HDR still-output spike](hdr-output-spike.md)
