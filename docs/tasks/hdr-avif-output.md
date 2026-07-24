# HDR AVIF Output

## Goal

Encode the Display-HDR renderer's 10-bit BT.2020 PQ and HLG signals as portable,
standards-signaled AVIF files with pinned profiles, metadata, packaging,
determinism, and decoded-error contracts.

## Design

Wrap `libavif` 1.4.2 or newer and `libaom` behind a narrow, audited Rust FFI.
Keep rendered-pixel production in `hdr-display-rendering`; this task owns AV1
encoding, the AVIF container, dependency builds, static packaging, codec
licensing notices/review inputs, and error translation at the Rust boundary.

Encode 10-bit full-range YUV 4:4:4. Because 4:4:4 AV1 uses High Profile, conform
to the AVIF v1.2 Advanced Profile with High Profile level no greater than 6.0.
For images within those profile limits, write major brand `avif` and compatible
brands `avif`, `mif1`, `miaf`, and `MA1A`. Independently inspect the resulting
AV1 sequence header, item properties, brands, and dimensions rather than
assuming encoder defaults establish conformance.

If an image exceeds the Advanced Profile's permitted coded-image dimensions,
tile dimensions, or other limits, either encode a standards-conforming AVIF grid
whose coded items and aggregate canvas meet the applicable limits, or omit
`MA1A` and explicitly report a general-brand-only AVIF. Never advertise `MA1A`
for a file outside the profile. Pin maximum supported dimensions, grid
construction, tile ordering, edge-tile behavior, and rejection limits before
enabling oversized output.

Write CICP/nclx `9/16/9` for BT.2020 PQ and `9/18/9` for BT.2020 HLG, full
range, plus content-light-level metadata where supported. Preserve the
renderer-provided 203 cd/m² reference white, 1000 cd/m² initial peak, and HLG
system/display assumptions in the resolved report. Normalize orientation into
pixels and omit timestamps, random identifiers, and unrequested EXIF/XMP.

Use one encoder job/thread and pinned quality/speed settings for the initial
determinism contract. Establish codec-specific max/RMS code error,
structural/perceptual, neutral-ramp, saturated-patch, edge, and gradient bounds
with an independent decoder. Byte identity is required only for repeated runs
using the same pinned encoder build, settings, target architecture, and thread
count; cross-build output must preserve semantic metadata and decoded pixels
within the pinned codec bounds.

Statically package the selected `libavif`/`libaom` configuration on every
supported target. Record exact source versions, build flags, enabled codecs,
license files, and the AOM patent-license review outcome. Retest the lack of
mastering-display color-volume writer support when upgrading `libavif`; do not
invent metadata the library cannot serialize.

## How to Verify

- Independent inspection proves AVIF v1.2 conformance, High Profile level
  ≤ 6.0, 10-bit 4:4:4 full-range coding, the correct CICP values, content-light
  metadata, and `avif`/`mif1`/`miaf`/`MA1A` brands for Advanced Profile files.
- Boundary fixtures at and beyond every pinned profile/dimension limit prove
  that grids are conforming and deterministic or that oversized files omit
  `MA1A` and are explicitly reported as general-brand-only; unsupported sizes
  fail before partial output is committed.
- Independent AVIF decode passes the task-pinned PQ/HLG code-error,
  ramp/neutral/saturated-patch, edge, gradient, and perceptual bounds against
  the canonical pre-encode buffers.
- Repeated same-build encodes are byte-identical; normalized metadata and
  decoded-pixel tests enforce the weaker documented cross-build contract.
- Static builds pass on macOS, Linux, and Windows with dependency versions,
  build flags, licenses, patent-review inputs, and binary-size impact recorded.
- Encoder and allocation failures cross the FFI boundary as stable nc errors
  without leaks, panics, undefined behavior, or partial destination files.

## Dependencies

- [Display-HDR rendering](hdr-display-rendering.md)
