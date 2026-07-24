# Display-Output Acceptance

## Goal

Verify the shipped display-output policies against the user's full-size real
scans after the gain-map encoder and output presets land. This is the final
product-quality and interoperability gate; it does not block earlier core
pipeline resource measurements.

## Design

Reuse the asset classes and resolved Dmin/Dmax inputs established by
`real-scan-verification`. Check in a small `display-acceptance-manifest.json`
that pins, for every case, the source asset ID/hash (never the large asset),
resolved recipe/hash, pipeline and working-mapping versions, preset, expected
container/signaling, canonical pre-encode float-buffer hash, golden metadata
dump, independent decoder/version, and applicable numeric tolerances. Golden
buffers live as compact deterministic fixtures or content-addressed harness
artifacts; changing one requires an explicit reviewed golden update with the old
and new metrics recorded.

For representative color and HDR frames, execute this matrix:

1. **Default `gain-map-hdr`** — the HDR rendition uses declared headroom and the
   SDR Display P3 base independently passes its decode-back oracle.
2. **Explicit presets** — `display-p3` and `compatibility` render correctly;
   `film-master` preserves unclamped linear ACEScg film rendering and cross-frame
   exposure under fixed/roll-calibrated Dmax; `hdr-pq` and `hdr-hlg` carry
   their declared color signaling and metadata.
3. **Container/profile metadata** — independent inspection confirms the resolved
   container, ICC/CICP signaling, gain-map metadata, reference white, and
   headroom. Output suffixes agree with their containers.
4. **Interoperability** — run the named manual viewer rubric below on target
   macOS/iPhone software, at least one non-Apple gain-map-aware reader, and one
   SDR-only fallback reader.
5. **Determinism** — repeated runs meet each encoder's documented contract:
   byte-identical where promised, otherwise decoded pixels within the applicable
   pinned codec bounds and identical semantic metadata.
6. **Film-rendering fidelity** — representative stocks, lenses, development
   processes, exponential/sigmoid curves, and scanners retain their intended
   differences through NC film RGB v1 and across named encodings. Acceptance
   compares those encodings with the same NC rendering, not with a physically
   neutral scene. Optional correction profiles are outside the required matrix.
7. **Cross-encoding consistency** — matched SDR, HDR, gain-map, and film-master
   outputs preserve hue and relative exposure within each renderer's declared
   tone/gamut policy; clipping and gamut compression are measured and reported.
8. **Simple boundary and migration** — simple reconstruction maps raw
   unclamped `1 - scan/Dmin`; named display output applies resolved
   `print.white_balance`/`print.linear_range` afterward, while film master
   rejects non-default values and legacy aliases follow their warned migration
   contract.

### Automated oracles

All automated comparisons start from the manifest's canonical buffers and use a
decoder independent of nc:

- `film-master`: decoded float ACEScg must match the canonical ACEScg buffer with
  per-sample maximum absolute error ≤ 2×10⁻⁶ and RMS error ≤ 5×10⁻⁷; metadata
  matches the golden semantic dump exactly.
- 16-bit standalone SDR (`display-p3`/`compatibility`): after
  independent ICC/transfer decode to the renderer's canonical linear destination
  RGB, each channel differs by at most 1 code value when re-quantized to 16-bit.
- Lossy 8-bit JPEG gain-map base: compare the independent decode with the
  canonical encoded-Display-P3 base using encoder-task-pinned max/RMS error,
  structural/perceptual, neutral-ramp, and saturated-patch bounds. A universal
  one-code-value bound is not valid for JPEG. Record the codec version, quality,
  chroma mode, and every measured threshold in the manifest.
- PQ/HLG AVIF: independently apply the pinned transfer to the canonical
  absolute-linear BT.2020 buffer in binary64, quantize to the 10-bit 4:4:4
  reference, then compare an independent AVIF decode using encoder-task-pinned
  max/RMS code error plus ramp, neutral, and saturated-patch thresholds. A
  universal one-code codec allowance is not valid for lossy AV1. PQ uses
  Rec.2100/ST 2084 with 203 cd/m² reference white and 1000 cd/m² target peak;
  the HLG preset must pin its OETF/OOTF/system gamma/reference display in its
  manifest row.
- Gain-map reconstruction: independent ISO 21496-1 and Ultra HDR v1
  implementations must each reconstruct the canonical HDR rendition and declared
  headroom within max(0.02 nit, 0.5% relative) per channel. Their independently
  decoded SDR base must also pass the manifest row's lossy 8-bit JPEG gain-map
  base bounds. A semantic metadata oracle must prove their
  scale/offset/gamma/capacity meanings agree after dialect-specific unit
  conversion, and a deliberately conflicting dual-metadata fixture must prove a
  dual-aware decoder gives ISO 21496-1 precedence. The oracle independently
  converts both linear Display P3 renderings to reference-white-relative units:
  SDR/reference white is `1.0`, and HDR absolute luminance is divided by the
  pinned 203 cd/m² reference white. It then derives each canonical gain as
  `(HDR_c + offset_hdr,c) / (SDR_c + offset_sdr,c)` using the manifest-pinned
  positive finite offsets expressed in that same domain. Equal reference-white
  samples with equal offsets must yield gain 1. A peak row feeds 1000 nits into
  the formula as `4.926108...`, but computes expected gain from the actual
  independently tone-mapped SDR sample and offsets rather than equating gain
  with display headroom. Black, near-black, and zero-channel rows must remain
  finite without arbitrary epsilon or `0/0` behavior; negative/nonfinite samples
  and any nonpositive/nonfinite adjusted denominator or gain must fail loudly.
  Mixing absolute-nit HDR with normalized SDR must produce the pinned unit/domain
  diagnostic.
- Deterministic encoders must produce byte-identical files. If the format task
  documents unavoidable container variability, decoded pixels must meet the
  applicable bound and a normalized semantic metadata dump must match exactly;
  only a manifest-listed set of volatile fields may differ.
- Cross-encoding color comparisons operate only on manifest-listed,
  non-tone-mapped/shared-gamut patches: independent decodes to XYZ D65 must have
  ΔE2000 ≤ 0.5 and neutral Δu'v' ≤ 0.0001. Tone- or gamut-mapped patches instead
  compare against each renderer's own canonical buffer and report hue-angle,
  clipping, and compression deltas; no unbounded “looks consistent” pass is
  allowed.

For that cross-encoding oracle, the manifest pins each rendition's declared
reference-white luminance in nits and the shared source exposure. Decode every
patch to absolute XYZ D65, then divide X, Y, and Z by that rendition's declared
reference-white luminance; this makes reference white `Y = 1` without any
per-image or per-patch exposure fit. HDR and SDR are compared only after this
explicit normalization. Convert normalized XYZ to CIELAB using the D65
2° reference white `(Xn, Yn, Zn) = (0.95047, 1.00000, 1.08883)`, the standard
CIE 1976 piecewise `f(t)` with `δ = 6/29`, and CIEDE2000 as specified by
Sharma–Wu–Dalal (2005) with `kL = kC = kH = 1`. Compute neutral chromaticity as
CIE 1976 `u' = 4X/(X+15Y+3Z)` and `v' = 9Y/(X+15Y+3Z)` on the same normalized
XYZ; `Δu'v'` is Euclidean distance. Zero-denominator samples are invalid
fixtures, not automatic passes.

Any metric outside its bound fails the automated row. The harness writes a
machine-readable result containing measured maxima, RMS/percentile summaries,
metadata diffs, decoder identity, and pass/fail.

### Manual viewer rubric

Manual viewing is an interoperability check, not the numeric color oracle. For
each pinned viewer/OS version, record these binary observations:

1. file opens without repair/error;
2. the viewer reports or demonstrably selects the intended SDR/HDR rendition;
3. HDR enable/disable or SDR fallback changes rendition as the preset specifies;
4. orientation, dimensions, crop, and alpha/extra-channel handling are correct;
5. no gross channel swap, inversion, all-black/all-white render, or frame-edge
   artifact is visible.

Record the exact viewer/OS/display-HDR setting and evidence for every item in
`progress.md`. “Plausible,” “looks good,” and agreement with a remembered
physical scene are not pass criteria. File a follow-up task for every failure
instead of fixing it ad hoc inside acceptance.

## Implementation Suggestion

- Extend the reusable harness from `real-scan-verification`; do not duplicate or
  re-read large assets into agent context.
- Preserve small metadata dumps and derived measurements, not large generated
  image outputs.
- Pin viewer/OS versions because HDR and gain-map behavior can change outside nc.

## How to Verify

The manifest-driven harness passes every applicable numeric and metadata bound,
repeat runs satisfy the declared determinism class, and every named manual
viewer-rubric item passes. Results are recorded with tool/viewer versions, and
every failure has a tracked follow-up (or the log explicitly records none).

## Dependencies

- [Output presets and guidance](output-presets.md)
- [Real-scan core verification](real-scan-verification.md)
