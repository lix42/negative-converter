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
`pipeline/colorimetry/`; do not introduce another copy of BT.2020 primaries,
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

## Status

**Done (2026-08-06)**, delivered in two chunks — A `hdr-linear-tiff`, B
`hdr-pq-tiff` / `hdr-hlg-tiff` — following this file's own instruction to pin the
PQ/HLG signaling contract before implementing that variant. See
[progress/output.md](../../progress/output.md) for the execution record.

Three decisions worth carrying forward, all recorded there in full:

- The PQ profile is an **extended-range A2B** (`lutAtoBType`), PCS `Y = L / 203`
  unclipped to ≈49.26, matching Adobe's reference BT.2100 profiles. A matrix-shaper
  profile cannot express it — a TRC output is confined to `[0, 1]` — so it would
  have to clip at reference white or render everything near-black.
- The HLG profile is **scene-referred**, because HLG's OOTF is not per-channel
  separable and no 1D curve set can carry it. A display-referred one needs a 3D
  CLUT (Adobe's are ~66 KB for exactly this reason); nc does not generate one.
- Both are documented as **limited-interoperability interchange, never
  "display-ready"**: TIFF has no CICP tag of its own, so the signalling lives in the
  ICC `cicpTag` and only a CICP-aware reader honours it. macOS ColorSync accepts and
  names the profiles (`sips`), which is evidence of parsing, **not** of HDR
  presentation — that remains a manual visual gate.

The manual viewer gate **ran on 2026-08-06** and came back "valid and correct, but
not discriminating": every TIFF and AVIF renders correctly and looks good, with
little visible difference between them. That confirms the files are well-formed and
that ColorSync accepts the profiles; it does **not** confirm visible HDR
presentation, so the documented compatibility stays where it is. A discriminating
retest needs a specular-highlight scene and an explicit **sigmoid** reconstruction
(the default is still exponential), comparing the PQ TIFF against the PQ AVIF rather
than against the legacy SDR baseline. See the progress log.

Two ICC conformance gaps are documented on `color::synth_coded_hdr` and
**deferred to `output/presets`** (decision 2026-08-06; the closing recipe lives in
[that task file](presets.md)). Both verified against ICC.1:2022: §8.4.2 requires
`BToA0Tag` (only `AToB0Tag` is written, so these are valid *source* profiles but not
conformant Display-class ones) and §8.2 requires `chromaticAdaptationTag` (missing,
so a consumer cannot recover that the encoding white is D65). Neither affects the
stored code values and the `cicp` tag remains authoritative. Deferred because
closing them needs two more pinned colorimetry artifacts and **changes the profile
bytes**, which wants a re-review alongside preset activation. The existing
dependency edge `output/lossless-hdr-tiff --> output/presets` already carries it, so
no graph change was needed.

**No paywalled standard blocks this task**, correcting the 2026-08-04 assumption in
`output/iso-gain-map-metadata`'s log that ISO 22028-5:2026 would have to be bought
"since `hdr-avif-output` and `lossless-hdr-tiff` hit the same gate". The 203-nit
reference white and 1000-nit peak were pinned by the *closed* HDR spike and are
only recorded here; the signaling contract comes from ICC.1:2022, ITU-T H.273,
BT.2100-3 and TIFF 6.0, all obtainable.

The **PQ/HLG signaling contract is already researched** (do not re-derive it):
ICC.1:2022 §9.2.17 `cicpTag` / §10.3 `cicpType` — a 12-byte tag (`'cicp'`, four
reserved zero bytes, then ColourPrimaries / TransferCharacteristics /
MatrixCoefficients / VideoFullRangeFlag as `uInt8`, encoded per ITU-T H.273),
permitted only for an RGB/YCbCr/XYZ data space in an Input or Display profile —
which nc's synthesized profiles satisfy (verified: `exiftool` reports
`ColorSpaceData: RGB`, `ProfileClass: Display Device Profile`). The specification's
own examples name the exact code points: `9-16-0-1` is "PQ R'G'B' full range
representation specified in Recommendation ITU-R BT.2100-2, Table 9" and
`9-18-0-1` the HLG equivalent. **MatrixCoefficients must be 0**, because §10.3
requires it for an RGB data space — the AVIF path's `9` reflects AVIF storing
Y'CbCr, so `HdrRenderMetadata::cicp_matrix_coefficients` must not be copied into a
TIFF profile. `cicpTag` *supplements* the transform tags rather than replacing them
("the colour encoding specified by the CICP tag content shall be equivalent to the
data colour space encoding represented by this ICC profile"), so a real TRC is
still required beside it. `lcms2` 6.1.1 can write the `cicpTag` itself in safe Rust —
`Profile::write_tag(TagSignature::CicpTag, Tag::VideoSignal(&VideoSignalType{…}))`
against Little CMS 2.19. **The surrounding profile, however, is built entirely
through `lcms2-sys` FFI**: the safe crate cannot insert stages into a `Pipeline`
(only `cat`) and does not expose a profile's raw handle, so an A2B profile is
unreachable through it.

Chunk B's fallback TRC was **decided by probe** (see the progress log): neither
candidate above was chosen. Adobe's reference BT.2100 profiles place a 203-nit
diffuse white at PCS `1.0` *without clipping*, carrying extended range to ≈49.26,
and that is what shipped — it is both colorimetrically equivalent to the declared
encoding and correct-looking in a naive CMM, which neither original candidate was. `github.com/digitaltvguy/ICC-v4.4-Profiles-with-CICP-Tags-for-HDR-and-SDR-Broadcast-Applications`
is prior art worth reading in a browser (a scripted README fetch failed).

Also note for Chunk B: 16-bit is **not** one of BT.2100's specified depths (it
specifies 10 and 12), so those files carry BT.2100's *transfer* at TIFF's
quantization and must be described that way.

## Dependencies

- [Display-HDR rendering](hdr-display-rendering.md)
- [Colorimetry source of truth and update workflow](../color/colorimetry-source-of-truth.md)
- [Transactional output writes](../io/transactional-output-writes.md)
