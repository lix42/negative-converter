# HDR Still-Output Spike Decision Note

**Status:** provisional implementation decision; normative-text review remains
open

**Investigated:** 2026-07-23

## Decision

NC should ship two different HDR still-image products:

| Preset | Initial container and encoding | Purpose |
|---|---|---|
| `gain-map-hdr` | JPEG with an 8-bit Display P3 SDR base and ISO 21496-1 gain map | Default, backward-compatible display output |
| `hdr-pq` | 10-bit 4:4:4 AVIF, BT.2020 / Rec.2100 PQ | Explicit single-rendition HDR |
| `hdr-hlg` | 10-bit 4:4:4 AVIF, BT.2020 / Rec.2100 HLG | Explicit broadcast/interchange-oriented HDR |

Do not use HEIC for the first implementation. HEIC has attractive Apple support,
but its HEVC encoder patent/licensing and x265 GPL/commercial obligations are a
poor fit for NC's portable static binary. The available cross-platform HEIF
library also cannot yet write the final standardized gain-map container contract.

Do not ship the current stable `libultrahdr` output unchanged. It is the best
available cross-platform starting point for JPEG gain maps, but version 1.4.0
writes ISO/MPF APP2 segments before JFIF APP0 and emits an ISO/TS-era identifier.
A generated reference file decoded with `libultrahdr`, ExifTool, and ImageMagick,
but macOS 26.5 ImageIO rejected it. Upstream
[pull request 394](https://github.com/google/libultrahdr/pull/394) corrects the
ordering and remained open when checked. Locally moving APP0 first and correcting
the MPF offset did not make ImageIO accept the file, so marker ordering is not
the only proven compatibility gate. NC's encoder task must consume a released
or reviewed ordering fix, verify final-standard serialization, and then pass the
platform matrix below.

The final implementation decisions are:

- Use the current [ISO 22028-5:2026](https://www.iso.org/standard/87778.html)
  HDR/WCG encoding standard, not the withdrawn ISO/TS 22028-5:2023.
- Use [ISO 21496-1:2025](https://www.iso.org/standard/86775.html) for gain-map
  mathematics and metadata.
- Treat the container contract separately. JPEG uses the ISO gain-map APP2
  representation plus MPF indexing. A later HEIF implementation must target
  [ISO/IEC 23008-12:2025](https://www.iso.org/standard/89035.html) and
  [Amendment 1:2025](https://www.iso.org/standard/89044.html), including the
  standardized tone-map-derived-image relationship.
- Use [Rec. BT.2100-3](https://www.itu.int/rec/R-REC-BT.2100-3-202502-I/en)
  transfer and colorimetry definitions.
- Set diffuse/reference white to 203 cd/m² and the initial target peak to
  1000 cd/m². The resulting linear display/content headroom is
  `1000 / 203 = 4.926108...`; its logarithmic capacity is
  `log2(1000 / 203) = 2.300448...` stops.
- Make PQ the primary single-rendition HDR transfer. HLG remains an explicit
  option and must record the chosen HLG system/display assumptions.
- Use CICP/nclx signaling for AVIF: color primaries 9 (BT.2020), matrix 9
  (BT.2020 non-constant luminance), full range, and transfer 16 (PQ) or 18
  (HLG). Write content-light-level information where supported.
- Use an embedded Display P3 ICC profile for the gain-map JPEG base. Normalize
  orientation into pixels and omit or fix volatile EXIF/XMP fields.
- Derive a half-resolution RGB gain map in common linear Display P3 expressed
  relative to the pinned reference white: SDR/reference white is `1.0`, and HDR
  absolute luminance is divided by 203 cd/m² so 203 nits is `1.0` and 1000 nits
  is `4.926108...`. This keeps the initial file compatible with older Android
  gain-map math, preserves chromatic highlight differences better than a
  single-channel map, and avoids dividing nonlinear, differently primaried, or
  differently normalized samples. HDR colors outside Display P3 are
  gamut-mapped for this compatibility preset.
- Encode both Android Ultra HDR v1 compatibility metadata and final ISO 21496-1
  metadata in the JPEG, using one gain-map image. A decoder that understands
  both must prefer ISO metadata. Each dialect must independently reconstruct the
  canonical HDR/headroom within the same pinned bounds and express equivalent
  scale/offset/gamma/capacity semantics after unit conversion.
- Keep public terminology standards-neutral: `gain-map-hdr`.

The exact ISO 22028-5:2026 mandatory-field table and the final ISO 21496-1 JPEG
URN/serialization must be checked against licensed copies before code is merged.
The accessible summaries and current libraries are not sufficient normative
sources for those byte-level requirements.

## Rendering contract

The rendering tasks should implement this numeric policy:

1. Start both display branches from the same mapped ACEScg film pixels and
   resolved shared linear adjustments.
2. Place adjusted linear value `1.0` at 203 cd/m².
3. Render the SDR base independently into linear Display P3, applying the SDR
   tone and gamut policy before the Display P3 transfer.
4. Render HDR independently, with a 1000 cd/m² peak, a monotonic
   luminance-preserving highlight shoulder, and hue-preserving gamut mapping.
   Per-channel hard clipping is not an acceptable gamut policy.
5. For `hdr-pq`, gamut-map into BT.2020, scale to absolute luminance, and apply
   ST 2084/PQ. For `hdr-hlg`, use the explicitly pinned BT.2100 HLG
   OETF/OOTF/system-gamma contract.
6. For `gain-map-hdr`, transform both rendered results to common linear Display
   P3 and normalize both to reference-white-relative units before gain math:
   SDR/reference white is `1.0`; divide HDR absolute luminance by 203 cd/m², so
   203 nits is `1.0` and 1000 nits enters as `4.926108...`. Express offsets in
   this same normalized domain and, after pinning positive finite offsets,
   compute each channel's gain as
   `gain_c = (HDR_c + offset_hdr,c) / (SDR_c + offset_sdr,c)`. Never combine
   absolute-nit HDR with `[0,1]`/reference-white-relative SDR, or compute ratios
   from encoded Display P3 and PQ values. Rendered samples must be finite and
   nonnegative; offsets, the resulting denominator, and the resulting gain must
   be finite and positive. Fail loudly on unit mismatch or domain violations
   before taking a logarithm—never inject an arbitrary epsilon or define `0/0`.
   Derive gain-map minima/maxima from this exact formula. They may differ from
   the global display-headroom ratio because SDR and HDR use different tone maps.

The rendering tasks still own the exact shoulder and gamut-compression curve,
but they may not change the reference white, target peak, common gain-map domain,
or RGB-map decision without reopening this spike.

## Encoder and packaging choice

### Single-rendition AVIF

Use `libavif` 1.4.2 or newer through a narrow Rust FFI, initially with `libaom`.
Both projects use permissive licenses and support static builds. The AOM patent
license grants participating licensors' royalty-free rights, but normal legal
review remains appropriate.

The dedicated [`hdr-avif-output`](tasks/hdr-avif-output.md) task owns this
encoder/container boundary. Pin these encoder settings for the first
implementation:

- 10-bit YUV 4:4:4, full range;
- AVIF v1.2 Advanced Profile, using AV1 High Profile level no greater than 6.0;
- fixed quality/speed settings chosen by the encoder task after numeric testing;
- one encoder job/thread for reproducibility;
- major brand `avif`, with compatible brands `avif`, `mif1`, `miaf`, and `MA1A`
  whenever the file remains within Advanced Profile limits;
- no timestamps, random identifiers, or unrequested EXIF/XMP;
- CICP values and content-light metadata as specified above.

For images beyond the Advanced Profile's coded-image or aggregate limits, the
encoder task must either use a conforming AVIF grid with pinned tile/canvas
limits or omit `MA1A` and report a general-brand-only AVIF. It must never
advertise Advanced Profile conformance outside its limits.

`libavif` 1.4.2 does not expose a writer for mastering-display color-volume
metadata. That metadata is optional for the proposed 203/1000 policy, but the
limitation must be recorded and retested when the dependency changes.

Do not use the current pure-Rust `ravif` raw 10-bit API for this path: its
high-bit-depth raw configuration currently fixes sRGB primaries and transfer
rather than allowing PQ/HLG signaling.

### Gain-map JPEG

Use a small audited Rust FFI around `libultrahdr`, statically linked with
libjpeg-turbo, after the following gates pass:

- JFIF APP0 precedes ISO/MPF APP2 metadata;
- the serialized identifier and fields conform to final ISO 21496-1:2025, not
  only the earlier ISO/TS draft;
- the file also contains valid Ultra HDR v1 compatibility metadata;
- the default image is an ordinary 8-bit Display P3 JPEG;
- the ISO-aware decoder reconstructs the RGB gain map and declared headroom;
- independent ISO and Ultra HDR v1 decoders each reconstruct the canonical
  HDR/headroom within pinned bounds, their metadata meanings agree after unit
  conversion, and a dual-aware preference test selects ISO when both are present;
- reference-white fixtures prove equal normalized SDR/HDR samples with equal
  offsets produce gain 1; a 1000-nit HDR sample enters as `4.926108...`, but its
  expected gain is computed with the actual independently tone-mapped SDR sample
  and pinned offsets rather than assumed to equal `4.926108...`;
- mixed absolute-nit HDR and reference-white-relative SDR is rejected with a
  unit/domain diagnostic before serialization;
- Apple ImageIO, Android, and Chromium all select HDR on supported hardware;
- Firefox and an ordinary JPEG reader open the SDR base without repair.

The initial settings to validate are JPEG quality 95, 4:4:4 base chroma,
half-resolution RGB gain map, gamma 1, and normalized orientation. Quality is
not a conformance property and may change only after the acceptance metrics are
measured.

Do not confuse linear API values with serialized logarithmic values:
`libultrahdr` exposes capacity in linear scale, but Ultra HDR XMP writes
`HDRCapacityMax`, `GainMapMax`, and corresponding minima in log2 units. The
203/1000 display ratio is therefore linear `4.926108...` and logarithmic
`2.300448...`; actual per-pixel gain extrema come from the offset-adjusted
formula over the independently tone-mapped HDR/SDR renderings and can be
different. Ultra HDR guidance suggests `1/64` offsets, but NC must not assume
that value is equivalent to the final ISO contract until licensed normative
text is checked. The gain-map task must pin positive finite offsets and feed the
same canonical gain calculation to both dialect serializers.

`libultrahdr` is Apache-2.0 and libjpeg-turbo is BSD-style. If NC implements the
Adobe gain-map specification or uses Adobe's relevant patent grant, distributed
documentation must include Adobe's required prominent notice:
“This product includes Gain Map technology under license by Adobe.”

### Deferred containers

- **HEIC gain map:** defer until a portable library writes the final
  ISO/IEC 23008-12:2025/Amd 1 container and the HEVC licensing/packaging policy
  is explicitly approved.
- **AVIF gain map:** defer until Android and the selected encoder expose a
  stable final-standard path. AVIF remains the single-rendition HDR container.
- **Platform-only encoders:** do not make Apple ImageIO or Android framework
  APIs the primary encoder because NC's CLI must behave consistently across
  macOS, Linux, and Windows.

## Prototype evidence

The spike generated small synthetic PQ, HLG, and gain-map files outside the
repository.

| Probe | Result |
|---|---|
| 10-bit PQ HEIC via libheif | ImageIO decoded `ITU-R 2100 PQ`; measured content headroom 4.92611 |
| 10-bit HLG HEIC via libheif | ImageIO decoded `ITU-R 2100 HLG` |
| 10-bit PQ/HLG AVIF via libavif/libheif | Correct 9/16/9 and 9/18/9 signaling; ImageIO decoded both; PQ headroom 4.92611 |
| Repeated fixed AVIF/HEIC encodes | Byte-identical on the same toolchain and architecture |
| ISO/Ultra HDR JPEG via libultrahdr 1.4.0 | Its C API decoded linear HDR capacity 4.92611; ExifTool/ImageMagick opened the file, but serialized log2 fields still require independent conformance checks |
| Same JPEG via macOS 26.5 ImageIO | Decode failed; segment order was SOI, ISO APP2, MPF APP2, JFIF APP0 |
| APP0-first JPEG with corrected MPF offset | ImageIO still rejected it, so ordering alone is insufficient |
| Static libultrahdr probe | Linked successfully; approximately 878 KiB executable with only system dynamic dependencies |

These are format probes, not product-quality fixtures. The downstream tasks
must replace them with checked-in, license-safe synthetic vectors and an
independent decoder oracle.

## Platform and fallback matrix

This matrix is the implementation target, not yet a completed acceptance result.
Exact versions must be pinned in the final acceptance manifest.

| Platform/reader | `gain-map-hdr` JPEG | PQ/HLG AVIF | Expected fallback |
|---|---|---|---|
| macOS 15+/iOS 18+ ImageIO/Photos-class software | ISO gain-map APIs exist; repaired reference file still needs device/viewer verification | Current ImageIO probe decodes PQ/HLG AVIF | Display P3 SDR base for unaware JPEG readers |
| Android 14 | Ultra HDR v1 JPEG path | Device/app dependent | SDR JPEG base |
| Android 15 | ISO 21496-1 JPEG encode/decode support | Device/app dependent | SDR JPEG base |
| Android 16+ | Expanded Ultra HDR and HEIC support | Device/app dependent | SDR JPEG base |
| Chromium-based browser | ISO/Ultra HDR JPEG decoding exists in current source/tests; hardware validation required | Browser/OS dependent | SDR JPEG base |
| Safari 26+ | HDR image rendering is supported; gain-map selection requires explicit test | HDR image support advertised | SDR JPEG base |
| Firefox/current ordinary JPEG readers | Do not rely on HDR gain-map selection | HDR AVIF support varies | SDR JPEG base opens normally |

Android's current guidance explicitly recommends writing both Ultra HDR v1 and
ISO metadata for maximum compatibility. Apple exposes ISO HDR and ISO gain-map
ImageIO keys beginning with macOS 15/iOS 18. These APIs establish plausible
support, but only the manual viewer rubric can establish NC interoperability.

## Determinism and acceptance

NC should promise byte-identical output only for the same pinned encoder build,
settings, architecture, and thread count. Across dependency or architecture
changes, promise identical semantic metadata and decoded pixels within the
codec-specific bounds.

The existing one-code-value acceptance bounds are incompatible with the chosen
lossy JPEG and AVIF encodings. The acceptance task must use:

- exact comparisons for pre-encode canonical buffers and metadata;
- codec-aware decoded error metrics for AVIF and the JPEG base;
- the existing absolute/relative luminance test for reconstructed gain-map HDR,
  adjusted only if measured independent-decoder behavior proves it unrealistic;
- exact normalized semantic metadata comparisons;
- byte identity as an additional same-build regression check, not the
  cross-platform product contract.

The encoder tasks must establish max/RMS error and perceptual thresholds from
synthetic ramps, saturated patches, edges, neutral gradients, and representative
full-resolution scans before those bounds are frozen.

## Spike completion gates

The spike itself is complete when the pre-implementation decision inputs are
closed:

1. licensed ISO 22028-5:2026 and ISO 21496-1:2025 text is checked for the exact
   mandatory metadata, JPEG serialization, offset semantics, and permitted
   ISO/Ultra HDR dual-metadata mapping;
2. the note records any resulting changes to the already selected containers,
   profiles, reference white, gain formula, or encoder boundaries;
3. the implementation tasks have sufficient normative inputs to proceed without
   reopening fundamental format choices.

These gates do not require downstream encoder implementations or physical-device
results. Pre-shipping checks remain owned by the follow-up tasks:

- `gain-map-hdr-output` owns corrected JPEG serialization, independent
  dual-dialect reconstruction, and the initial platform-reader matrix;
- `hdr-avif-output` owns Advanced Profile brands/limits, oversized grid or
  general-brand-only behavior, libavif/libaom packaging, and AVIF codec bounds;
- `display-output-acceptance` owns final device/viewer evidence, proof that aware
  viewers selected HDR, and the frozen cross-encoding acceptance matrix;
- the encoder/release tasks own project-appropriate Adobe/AOM/HEVC licensing and
  distribution checks.

## Primary references

- [ISO 22028-5:2026](https://www.iso.org/standard/87778.html)
- [ISO 21496-1:2025](https://www.iso.org/standard/86775.html)
- [ISO/IEC 23008-12:2025](https://www.iso.org/standard/89035.html) and
  [Amendment 1:2025](https://www.iso.org/standard/89044.html)
- [ITU-R BT.2100-3](https://www.itu.int/rec/R-REC-BT.2100-3-202502-I/en) and
  [ITU-R BT.2408](https://www.itu.int/pub/R-REP-BT.2408)
- [Apple: Explore HDR rendering, part 2](https://developer.apple.com/videos/play/wwdc2023/10181/)
- [Apple: Use HDR for dynamic image experiences](https://developer.apple.com/videos/play/wwdc2024/10177/)
- [Android Ultra HDR format](https://developer.android.com/media/platform/hdr-image-format)
- [Android 16 media features](https://developer.android.com/about/versions/16/features)
- [WebKit features in Safari 26](https://webkit.org/blog/17333/webkit-features-in-safari-26-0/)
- [libavif](https://github.com/AOMediaCodec/libavif),
  [libultrahdr](https://github.com/google/libultrahdr), and
  [libheif](https://github.com/strukturag/libheif)
- [AOMedia Patent License 1.0](https://aomedia.org/license/patent-license/)
- [AOMedia AV1 Image File Format specification](https://aomediacodec.github.io/av1-avif/)
- [Adobe gain-map specification and patent notice](https://helpx.adobe.com/camera-raw/using/gain-map.html)
