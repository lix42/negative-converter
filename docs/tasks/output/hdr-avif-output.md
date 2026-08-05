# HDR AVIF Output

## Goal

Encode the Display-HDR renderer's 10-bit BT.2020 PQ and HLG signals as portable,
standards-signaled AVIF files with pinned profiles, metadata, packaging,
determinism, and decoded-error contracts.

## Design

Keep rendered-pixel production in `hdr-display-rendering`; this task owns AV1
encoding, the AVIF container, dependency builds, static packaging, codec
licensing notices/review inputs, and error translation at the Rust boundary.

**Encoder/container decision (2026-08-05, supersedes the original "wrap
`libavif` 1.4.2 or newer behind a narrow Rust FFI" plan and the matching
paragraph in `docs/hdr-output-spike.md`).** Use the published `libaom-sys` crate
for the AV1 codestream and an **nc-owned Rust MIAF/AVIF container writer**. Two
measured facts forced this: no published crate ships libavif ≥ 1.4.2
(`libavif-sys` 0.17 is libavif **1.0.4**, predating `MA1A`/Advanced Profile
brand writing), and `avif-serialize` 0.8.9 hardcodes
`compatible_brands: [mif1, miaf]` with no setter, so it cannot emit the brands
below. `libaom-sys` vendors libaom inside the crate and links it statically with
no network access and no in-repo snapshot — the dependency shape
`output/ultrahdr-dependency-externalization` names as the target. The spike's
binding numbers (203 cd/m² reference white, 1000 cd/m² peak, gain-map domain,
RGB-map decision) are unaffected.

Because the container is authored rather than delegated, `av1C` **must** be
populated by parsing the encoded codestream's own sequence-header OBU.
`AV1E_GET_SEQ_LEVEL_IDX` reports the encoder's *target* level (31 = unset), so
trusting it writes a bogus level into the file.

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

Statically package the selected `libaom` configuration on every supported
target. Record exact source versions, build flags, enabled codecs, license
files, and the AOM patent-license review outcome. Mastering-display
colour-volume metadata stays unwritten under the 203/1000 policy; since the
container is nc-owned, that is now a deliberate product choice to record rather
than a library limitation to retest.

Before `hdr-pq` or `hdr-hlg` becomes CLI-reachable, extend and calibrate
`pipeline::memory` for the shared adjusted ACEScg source overlapping the HDR
renderer’s 12 B/px output and the selected AVIF encoder's staging buffers. The
current `RunProfile::Convert` is deliberately the shipped legacy model and is
not sufficient evidence for this new allocation graph.

## Boundary with `output/presets` (recorded 2026-08-05)

Both this task and `output/presets` claimed the memory-calibration gate, which
read as either duplicated work or a gap where each assumed the other did it. The
`ultra-hdr-v1` precedent settles it, and the split is now explicit:

**This task owns** the AVIF encoder plus **explicit, `convert`-only `hdr-pq` and
`hdr-hlg` presets** — exactly how `output/gain-map-hdr-output` shipped
`ultra-hdr-v1`. That includes accepting the two names, requiring an `.avif`
suffix *for them*, their preset atomicity, their entry in the resolved report,
and **adding and calibrating their own `RunProfile`** against measured peak RSS.

**`output/presets` owns** the product surface and must not be pre-empted here:
making `gain-map-hdr` the default, replacing `--output-hdr`, the suffix table for
*every other* preset, `custom`, `nc roll` naming/manifest integration, and the
`conversion-versioning`-owned `pipeline_version` bump. On memory it *verifies
selection* — that a resolved preset picks its calibrated profile — rather than
re-deriving this task's model.

Activating here is also the only sequencing that works: `output/presets`
additionally depends on `output/iso-gain-map-metadata`, which is hard-blocked on
the paywalled ISO 21496-1:2025 text. Deferring activation would leave a complete,
tested AVIF encoder unreachable behind an unrelated standard.

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
- Static builds pass on macOS and Linux with dependency versions, build flags,
  licenses, patent-review inputs, and binary-size impact recorded. **Windows is
  deferred** (2026-08-05 decision): CI has no Windows runner, so the platform is
  gated by a separate follow-up task rather than claimed untested here.
- Encoder and allocation failures cross the FFI boundary as stable nc errors
  without leaks, panics, undefined behavior, or partial destination files.
- Memory-preflight tests and measured calibration cover the named HDR render and
  AVIF staging overlap before preset activation.

## Dependencies

- [Display-HDR rendering](hdr-display-rendering.md)
