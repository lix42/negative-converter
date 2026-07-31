# Lossless HDR TIFF Outputs

## Goal

Provide two deterministic HDR TIFF deliverables for different uses:

- a 32-bit float, display-linear BT.2020 interchange TIFF that preserves the
  HDR renderer's pre-transfer samples exactly; and
- display-referred Rec.2100 PQ or HLG TIFFs whose quantized code values are
  stored losslessly.

Keep both distinct from `film-master`, which is linear ACEScg before display
rendering, and from AVIF/gain-map outputs, which target consumer distribution.

## Design

Consume the typed outputs already owned by `pipeline::hdr` without weakening
their domain boundaries:

```text
shared adjusted ACEScg
→ HDR tone/gamut rendering
→ display-linear BT.2020 ─────────→ 32-bit float linear BT.2020 TIFF
                          └→ PQ/HLG transfer → 16-bit integer PQ/HLG TIFF
```

Do not represent nonlinear PQ/HLG samples as an ordinary `LinearImage`. Add
domain-specific TIFF encoder entry points that accept the opaque linear-BT.2020
and encoded-PQ/HLG types, while reusing the existing low-level ClassicTIFF /
BigTIFF writer and transactional-output behavior where appropriate.

Define “lossless” separately for the two products:

- **Linear HDR TIFF:** write each finite `f32` BT.2020 sample verbatim and prove
  bit-exact decode round-trip. Preserve values above reference white; do not
  clamp, normalize, transfer-encode, or reinterpret them.
- **PQ/HLG TIFF:** quantize the renderer's normalized full-range signal once to
  unsigned 16-bit code values using one pinned rounding rule. TIFF storage must
  reproduce every resulting code value exactly. This is lossless relative to
  the quantized signal, not mathematically lossless relative to the source
  `f32`; report maximum/RMS quantization error and reject non-finite or
  out-of-domain samples rather than silently clipping them.

Uncompressed TIFF satisfies the storage contract. A deterministic TIFF
compression option may use only a lossless codec supported consistently on all
targets; never use TIFF JPEG compression. BigTIFF promotion and the memory
preflight must account for each new depth/domain path.

The linear variant uses BT.2020/D65 primaries with a linear transfer curve and
must embed a deterministic, independently inspected linear-BT.2020 ICC profile.
Its report and sidecar record that samples are reference-white-relative
display-linear values, the fixed 203 cd/m² reference white, the 1000 cd/m²
initial target peak, gamut/tone policy, and pipeline/colorimetry versions. The
ICC profile alone must not be claimed to communicate all HDR luminance
semantics to arbitrary viewers.

Before implementing the PQ/HLG variant, pin a standards-valid TIFF signaling
contract from current TIFF, ICC, CICP, Rec.2100, and relevant profile
specifications. Record the exact standard revisions and independently inspect
the emitted file. Do not invent private tags, copy metadata conventions from
another container, or claim that a viewer will enter HDR mode without evidence.
If TIFF cannot portably signal automatic HDR presentation, still support the
encoded-signal interchange TIFF with complete in-file standard metadata where
available plus the authoritative sidecar, but name and document it truthfully
as limited-interoperability rather than “display-ready.”

Expose three atomic planned policies through `output/presets`:

| Policy | Pixel contract |
|---|---|
| `hdr-linear-tiff` | 32-bit float, display-linear BT.2020/D65 |
| `hdr-pq-tiff` | 16-bit integer, full-range BT.2020 Rec.2100 PQ |
| `hdr-hlg-tiff` | 16-bit integer, full-range BT.2020 Rec.2100 HLG |

All accept only `.tif`/`.tiff`. They resolve their own depth, transfer, profile,
metadata, BigTIFF, and renderer policy and reject contradictory legacy output
flags. Do not reuse the ambiguous legacy `--output-hdr`: that remains a
transitional rendered float TIFF and is neither the linear-BT.2020 output nor a
PQ/HLG signal.

The source color-space definitions and pinned derived artifacts come from
`pipeline/colorimetry.rs`; do not introduce another copy of BT.2020 primaries,
luma weights, matrices, or transfer constants while adding the profiles and
encoder adapters.

This task owns the TIFF encoding/profile/signaling contracts and their
round-trip verification. `output/presets` owns final CLI/recipe activation,
suffix validation, roll naming, reports, and user guidance.

## How to Verify

- An independent TIFF decoder recovers every finite linear-BT.2020 `f32` sample
  with identical `to_bits()` values, including black, reference white, values
  between reference white and peak, and values near the supported maximum.
- Linear-output inspection proves RGB 32-bit IEEE float samples, correct
  ClassicTIFF/BigTIFF selection, a deterministic linear-BT.2020 ICC profile, and
  no accidental PQ/HLG transfer encoding.
- PQ and HLG tests quantize standard transfer vectors, neutral ramps, saturated
  colors, reference white, and peak; independent decode recovers every stored
  16-bit code exactly and the reported max/RMS quantization errors match an
  independent calculation.
- Invalid dimensions, non-finite samples, out-of-domain PQ/HLG samples,
  unsupported compression, profile construction failures, and write/flush
  failures are loud errors and leave no partial destination.
- Independent metadata inspection verifies the pinned standards contract for
  BT.2020 primaries, PQ or HLG transfer, full range, reference white, target
  peak, and HLG display/system assumptions. Manual application checks record
  which viewers recognize the files and never broaden the documented
  compatibility beyond the evidence.
- The three policies reject mismatched extensions and conflicting legacy flags;
  reports and sidecars distinguish `film-master`, `hdr-linear-tiff`,
  `hdr-pq-tiff`, `hdr-hlg-tiff`, `hdr-pq`/`hdr-hlg` AVIF, and gain-map HDR.
- Memory-model tests and measured peak-RSS calibration cover the HDR renderer
  overlapping the float or quantization/TIFF staging buffers before activation.
- Same-build repeated encodes are byte-identical after deterministic metadata
  normalization.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build`, and `cargo test` pass.

## Dependencies

- [Display-HDR rendering](hdr-display-rendering.md)
- [Colorimetry source of truth and update workflow](../color/colorimetry-source-of-truth.md)
- [Transactional output writes](../io/transactional-output-writes.md)
