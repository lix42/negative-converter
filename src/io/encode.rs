//! [`LinearImage`] → 16-bit / 32-bit-float TIFF, embedded ICC, sidecar JSON,
//! optional IR export — plus the domain-typed HDR TIFF entry points.
//!
//! [`encode`] is the general path driven by [`OutputParams`]. The two HDR entry
//! points are separate on purpose, because each takes one of `pipeline::hdr`'s
//! opaque types rather than a bare [`LinearImage`], so an HDR domain cannot be
//! confused with the Rec.709 working space `encode`'s images live in:
//! [`encode_hdr_linear`] takes [`LinearBt2020Hdr`] (display-linear, written
//! verbatim as f32) and [`encode_hdr_coded`] takes
//! [`RenderedHdr`](crate::pipeline::hdr::RenderedHdr) (nonlinear Rec.2100 PQ/HLG,
//! quantized once to 16-bit codes). All three share this module's low-level writer,
//! BigTIFF sizing, and loss accounting — the split is in the *type*, not the
//! machinery.
//!
//! Pure-ish encode stage: the public `&Path` entry points wrap a thin
//! `*_to_writer` core generic over `Write + Seek`, so the unit tests can encode
//! into an in-memory `Cursor` and decode the bytes straight back — no temp files,
//! fully deterministic. Crate-specific `tiff` types stay confined to this module
//! (the neutral contract lives in [`crate::types`]).

use std::ffi::OsString;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use tiff::encoder::colortype::{ColorType, Gray16, Gray32Float, RGB16, RGB32Float};
use tiff::encoder::{TiffEncoder, TiffKind, TiffKindBig, TiffKindStandard, TiffValue};
use tiff::tags::Tag;

use crate::io::staged::{self, Staged};
use crate::pipeline::hdr::{ContentLightLevel, LinearBt2020Hdr, LinearHdrMetadata};
use crate::types::{
    BigTiff, EncodeOutcome, EncodeReport, LinearImage, NcError, OutDepth, OutputParams,
    OutputStats, Result,
};

/// Slack added to the raw sample-data size when deciding BigTIFF auto-promotion:
/// IFD entries and strip offset/bytecount tables live outside
/// `width*height*channels*bytes`. A conservative margin keeps a file that sits
/// just under the classic limit from overflowing its 32-bit offsets. (The
/// embedded ICC is counted explicitly via `extra_bytes`, not folded in here, so a
/// large custom profile can't slip past the margin.)
const BIGTIFF_MARGIN_BYTES: u64 = 1 << 20; // 1 MiB

/// Classic (non-Big) TIFF addresses file contents with 32-bit offsets, so the
/// whole file must stay within `u32::MAX` bytes (~4 GiB).
const CLASSIC_TIFF_LIMIT: u64 = u32::MAX as u64;

/// Encode `image` to a TIFF at `path` per `params` (depth, BigTIFF policy). `icc`
/// is the output-profile blob to embed — produced by `pipeline::color::to_output`,
/// so the encoder embeds exactly the profile the pixels were converted into rather
/// than re-resolving it. `None` embeds no profile.
///
/// Returns an [`EncodeOutcome`]: the [`EncodeReport`] recording any quantization
/// clipping so the caller can fold it into the JSON report (and `--strict` can
/// promote it to an error), plus the report-only [`OutputStats`] of the samples as
/// written (the cross-version comparison basis).
pub fn encode(
    image: &LinearImage,
    params: &OutputParams,
    icc: Option<&[u8]>,
    path: &Path,
) -> Result<(Staged, EncodeOutcome)> {
    // Staged: the bytes land on a same-directory temp and are fsynced, and `path`
    // does not exist (or still holds the previous output) until the caller commits.
    // Flushing is `stage`'s job now — a `BufWriter` dropped unflushed silently
    // truncates, which is why neither layer may leave it implicit.
    staged::stage(path, |writer| encode_to_writer(writer, image, params, icc))
}

/// Whether encoding `image` under `params` (with an `icc_len`-byte embedded
/// profile) will produce a BigTIFF. Reuses the same sizing logic `encode` runs
/// internally, so the orchestrator can report an `auto` promotion in the JSON
/// report without duplicating the threshold — and without re-deciding it
/// differently than the encoder does.
pub fn plans_bigtiff(params: &OutputParams, image: &LinearImage, icc_len: usize) -> bool {
    resolve_bigtiff(
        params.bigtiff,
        image.width,
        image.height,
        3,
        depth_bytes(params.depth()),
        icc_len as u64,
    )
}

/// TIFF `SampleFormat` for IEEE floating-point samples (TIFF 6.0 §19, tag 339,
/// value 3). Recorded in [`HdrLinearTiffSummary`] so the report names the storage
/// format instead of leaving a reader to infer it from the bit depth.
const SAMPLE_FORMAT_IEEE_FLOAT: u16 = 3;

/// What [`encode_hdr_linear`] resolved, for the JSON report.
///
/// The four storage fields are the contract this function *writes*, fixed by its
/// `RGB32Float` colour type rather than measured back out of the file — unlike
/// `io::avif`'s summary, which has to parse the codestream because libaom chooses
/// the level. Nothing here is negotiable at run time, so there is no encoder
/// decision to distrust; the round-trip tests are what prove the bytes match.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrLinearTiffSummary {
    /// Stable identifier of the pixel contract written to the file.
    pub pixel_contract: &'static str,
    /// Whether the file was written as BigTIFF.
    pub bigtiff: bool,
    /// Bits per sample as written.
    pub bits_per_sample: u16,
    /// TIFF `SampleFormat` as written.
    pub sample_format: u16,
    /// Size of the embedded ICC profile in bytes.
    pub icc_bytes: usize,
    /// The renderer's resolved linear policy, carried through for the report.
    pub linear: LinearHdrMetadata,
    /// This frame's measured light levels (taken while the samples were still
    /// linear luminance).
    pub content_light: ContentLightLevel,
}

/// Write the display-linear BT.2020 HDR rendition as a 32-bit float TIFF.
///
/// **The lossless contract:** every finite sample is written verbatim, so an
/// independent decoder recovers bit-identical `f32` values. Nothing is clamped,
/// normalized, transfer-encoded, or reinterpreted — values above the 203 cd/m²
/// reference white survive to the ≈4.926108 peak, which is the entire reason this
/// output exists next to the PQ/HLG ones. Non-finite samples are *counted* into the
/// [`EncodeReport`] so an upstream numerical fault stays visible, exactly as the
/// other `f32` path does; they are never laundered into a finite value.
///
/// Takes `render` **by value**: the samples are written straight out of its buffer,
/// so encoding from a borrow would put a second full-frame `f32` image on the heap
/// that `pipeline::memory`'s `HdrLinearTiff` profile does not account for.
///
/// `icc` is the linear-BT.2020 blob from `color::hdr_linear_bt2020_icc`. It is
/// passed in rather than built here so the encoder embeds exactly the profile the
/// orchestrator resolved — the same rule [`encode`] follows.
///
/// The destination is written through [`staged`], so a failure in sizing, in the
/// TIFF writer, or in the flush leaves no partial file at `path`.
pub fn encode_hdr_linear(
    render: LinearBt2020Hdr,
    params: &OutputParams,
    icc: &[u8],
    path: &Path,
) -> Result<(Staged, EncodeOutcome, HdrLinearTiffSummary)> {
    let (image, linear, content_light) = render.into_parts();
    let big = resolve_bigtiff(
        params.bigtiff,
        image.width,
        image.height,
        3,
        depth_bytes(OutDepth::F32),
        icc.len() as u64,
    );
    let summary = HdrLinearTiffSummary {
        pixel_contract: HDR_LINEAR_PIXEL_CONTRACT,
        bigtiff: big,
        bits_per_sample: 32,
        sample_format: SAMPLE_FORMAT_IEEE_FLOAT,
        icc_bytes: icc.len(),
        linear,
        content_light,
    };
    let (staged, outcome) = staged::stage(path, |writer| {
        encode_hdr_linear_to_writer(writer, &image, big, icc)
    })?;
    Ok((staged, outcome, summary))
}

/// Stable identifier for the `hdr-linear-tiff` pixel contract, shared by the
/// encoder summary and the report so the two cannot drift.
pub const HDR_LINEAR_PIXEL_CONTRACT: &str =
    "rgb-f32-display-linear-bt2020-d65-relative-to-203-nit-reference-white";

fn encode_hdr_linear_to_writer<W: Write + Seek>(
    writer: W,
    image: &LinearImage,
    big: bool,
    icc: &[u8],
) -> Result<EncodeOutcome> {
    // Same accounting as the `f32` arm of `encode_to_writer`: verbatim samples, so
    // no `clipped_*` tally is meaningful, but a non-finite sample is still a fault.
    let loss = scan_non_finite(&image.rgb);
    let stats = channel_means_f32(&image.rgb);
    if big {
        encode_planar::<_, TiffKindBig, RGB32Float>(
            TiffEncoder::new_big(writer)?,
            image.width,
            image.height,
            &image.rgb,
            Some(icc),
        )?;
    } else {
        encode_planar::<_, TiffKindStandard, RGB32Float>(
            TiffEncoder::new(writer)?,
            image.width,
            image.height,
            &image.rgb,
            Some(icc),
        )?;
    }
    Ok(EncodeOutcome { loss, stats })
}

/// Full-scale 16-bit code value, and the full-range quantization scale.
///
/// BT.2100 specifies 10- and 12-bit systems, not 16 — so this is TIFF's
/// quantization applied to BT.2100's *transfer function*, which is exactly how the
/// report and docs describe it. Full range means code = `round(v · 65535)` with no
/// footroom or headroom reserved, matching the `VideoFullRangeFlag = 1` the
/// embedded profile's `cicp` tag declares.
const MAX_CODE_16: f32 = 65535.0;

/// What [`encode_hdr_coded`] resolved, for the JSON report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrCodedTiffSummary {
    /// Stable identifier of the pixel contract written to the file.
    pub pixel_contract: &'static str,
    /// Whether the file was written as BigTIFF.
    pub bigtiff: bool,
    /// Bits per sample as written.
    pub bits_per_sample: u16,
    /// TIFF `SampleFormat` as written (1 = unsigned integer).
    pub sample_format: u16,
    /// Size of the embedded ICC profile in bytes.
    pub icc_bytes: usize,
    /// Largest quantization error over the frame, in code units — at most `0.5` by
    /// construction, since rounding cannot be worse than half a step.
    pub max_quantization_error_codes: f32,
    /// Root-mean-square quantization error over the frame, in code units.
    pub rms_quantization_error_codes: f32,
    /// The renderer's resolved transfer and signalling contract.
    pub metadata: crate::pipeline::hdr::HdrRenderMetadata,
}

/// Write a Rec.2100 PQ/HLG rendition as a 16-bit integer TIFF.
///
/// **What "lossless" means here, precisely.** The renderer's normalized signal is
/// quantized to unsigned 16-bit code values **once**, with one pinned rounding rule
/// (`round`, half away from zero — the same rule [`quantize_u16`] uses), and TIFF
/// then stores every resulting code exactly. It is lossless *relative to the
/// quantized signal*, not relative to the source `f32`; the quantization step it
/// costs is measured and reported as
/// [`max_quantization_error_codes`](HdrCodedTiffSummary::max_quantization_error_codes)
/// and its RMS companion rather than left for the caller to assume.
///
/// Out-of-domain samples are **rejected, not clipped**. `pipeline::hdr` already
/// guarantees finite samples in `[0, 1]`, so this cannot fire today and is a
/// tripwire for a future path that reaches the encoder with a numerical fault —
/// which is why it names the offending pixel instead of quietly clamping. That is
/// the opposite of the legacy `encode` path, where clipping is an expected outcome
/// of an unclamped render and is *counted*; here a sample outside the domain means
/// the transfer stage is broken.
pub fn encode_hdr_coded(
    render: crate::pipeline::hdr::RenderedHdr,
    params: &OutputParams,
    icc: &[u8],
    path: &Path,
) -> Result<(Staged, EncodeOutcome, HdrCodedTiffSummary)> {
    let (image, metadata) = render.into_parts();
    let (width, height) = (image.width(), image.height());
    let big = resolve_bigtiff(
        params.bigtiff,
        width,
        height,
        3,
        depth_bytes(OutDepth::U16),
        icc.len() as u64,
    );
    let (data, loss, stats, error) = quantize_coded_u16(image.rgb())?;
    let summary = HdrCodedTiffSummary {
        pixel_contract: match metadata.transfer {
            crate::pipeline::hdr::HdrTransfer::Pq => HDR_PQ_PIXEL_CONTRACT,
            crate::pipeline::hdr::HdrTransfer::Hlg => HDR_HLG_PIXEL_CONTRACT,
        },
        bigtiff: big,
        bits_per_sample: 16,
        sample_format: SAMPLE_FORMAT_UNSIGNED,
        icc_bytes: icc.len(),
        max_quantization_error_codes: error.max,
        rms_quantization_error_codes: error.rms,
        metadata,
    };
    let (staged, outcome) = staged::stage(path, |writer| {
        if big {
            encode_planar::<_, TiffKindBig, RGB16>(
                TiffEncoder::new_big(writer)?,
                width,
                height,
                &data,
                Some(icc),
            )?;
        } else {
            encode_planar::<_, TiffKindStandard, RGB16>(
                TiffEncoder::new(writer)?,
                width,
                height,
                &data,
                Some(icc),
            )?;
        }
        Ok(EncodeOutcome { loss, stats })
    })?;
    Ok((staged, outcome, summary))
}

/// TIFF `SampleFormat` for unsigned integer samples (TIFF 6.0 tag 339, value 1).
const SAMPLE_FORMAT_UNSIGNED: u16 = 1;

/// Stable identifiers for the coded-HDR pixel contracts. They name the transfer
/// *and* the fact that 16-bit is TIFF's quantization rather than one of BT.2100's
/// own bit depths, so a consumer cannot read them as a Rec.2100 system claim.
pub const HDR_PQ_PIXEL_CONTRACT: &str = "rgb-u16-full-range-bt2020-rec2100-pq-tiff-quantized";
pub const HDR_HLG_PIXEL_CONTRACT: &str = "rgb-u16-full-range-bt2020-rec2100-hlg-tiff-quantized";

/// Measured cost of the one quantization step, in code units.
#[derive(Debug)]
struct QuantizationError {
    max: f32,
    rms: f32,
}

/// Quantize the renderer's normalized `[0, 1]` signal to full-range 16-bit codes,
/// measuring what the step cost.
///
/// The error is accumulated in `f64` and reported in **code units**, where the
/// theoretical maximum is exactly `0.5`: that makes "did rounding behave?" a
/// checkable claim rather than a scale the reader has to reconstruct.
fn quantize_coded_u16(
    samples: &[f32],
) -> Result<(Vec<u16>, EncodeReport, OutputStats, QuantizationError)> {
    let mut data = Vec::with_capacity(samples.len());
    let mut worst = 0.0_f64;
    let mut squared = 0.0_f64;
    for (index, &value) in samples.iter().enumerate() {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(NcError::Other(format!(
                "coded HDR sample {index} is outside the encodable domain ({value}); the \
                 transfer stage must deliver finite values in [0, 1] — refusing to clip it \
                 into range"
            )));
        }
        // **Scaled and measured in binary64, and that is load-bearing.** Doing
        // `value * MAX_CODE_16` in `f32` rounds *twice* — once into the product,
        // then again in `round()` — so a sample whose exact product sits just below
        // a half-code boundary can be pushed onto it and rounded away from the
        // nearest code. Measured: `0.996_498_05_f32 · 65535` evaluates to exactly
        // `65305.5_f32` and stores 65306, where the exact product is 65305.4995957
        // and the nearest code is 65305 — and the `f32` residual then *reports*
        // 0.5 while the stored code is really 0.5004 away, understating the error
        // and breaking the "at most half a code" contract this function advertises.
        // 271 of the 167,772 `f32` values in `[0.99, 1.0)` disagree on the nearest
        // code that way. `f64` has ample headroom over a 16-bit target, so one
        // rounding is all that happens.
        let scaled = f64::from(value) * f64::from(MAX_CODE_16);
        let code = scaled.round();
        let error = (scaled - code).abs();
        worst = worst.max(error);
        squared += error * error;
        data.push(code as u16);
    }
    let rms = if samples.is_empty() {
        0.0
    } else {
        (squared / samples.len() as f64).sqrt() as f32
    };
    let worst = worst as f32;
    // The samples were verified in range above, so nothing was clipped and nothing
    // was non-finite; the report is provably all-zero apart from the total, and
    // saying so keeps `--strict` from being handed a meaningless warning.
    let report = EncodeReport {
        total_samples: samples.len() as u64,
        ..EncodeReport::default()
    };
    let stats = channel_means_u16(&data);
    Ok((data, report, stats, QuantizationError { max: worst, rms }))
}

/// Write the IR plane as a single-channel TIFF at `depth`. Errors loudly when the
/// image carries no IR plane rather than writing an empty/placeholder file — the
/// caller asked for IR export, so a missing plane is a real failure.
pub fn export_ir(image: &LinearImage, depth: OutDepth, path: &Path) -> Result<Staged> {
    // Check for the IR plane before staging anything. Staging alone would no longer
    // clobber the target — that is the point of the temp — but there is no reason to
    // create and immediately discard a file for a request that cannot succeed.
    if image.ir.is_none() {
        return Err(no_ir_error());
    }
    let (staged, ()) = staged::stage(path, |writer| export_ir_to_writer(writer, image, depth))?;
    Ok(staged)
}

/// Write the sidecar JSON next to the output. The sidecar path is `<output>.json`
/// (e.g. `out.tiff` → `out.tiff.json`), so an output and its recipe stay paired by
/// name.
///
/// The caller composes the document: `cli` writes the
/// `{ "meta": {…identity…}, "params": {…recipe…} }` envelope
/// (`core/conversion-versioning`), keeping run identity out of the recipe body so
/// the sidecar stays reloadable through `--params`. This function only owns the
/// path and the write error.
pub fn write_sidecar(output_path: &Path, sidecar_json: &str) -> Result<Staged> {
    let sidecar = sidecar_path(output_path);
    staged::stage_bytes(&sidecar, sidecar_json.as_bytes())
}

/// The sidecar path for an output: `<output>.json` (extension appended, not
/// replaced, so `a.tiff` → `a.tiff.json` and output/sidecar stay paired by name).
/// Exposed so the CLI can include the sidecar in write-target collision checks.
pub fn sidecar_path(output_path: &Path) -> PathBuf {
    let mut name = OsString::from(output_path.as_os_str());
    name.push(".json");
    PathBuf::from(name)
}

// ---------------------------------------------------------------------------
// Writer-generic core (the testable seam)
// ---------------------------------------------------------------------------

fn encode_to_writer<W: Write + Seek>(
    writer: W,
    image: &LinearImage,
    params: &OutputParams,
    icc: Option<&[u8]>,
) -> Result<EncodeOutcome> {
    let (w, h) = (image.width, image.height);
    let bytes_per_sample = depth_bytes(params.depth());
    let icc_bytes = icc.map_or(0, |b| b.len() as u64);
    let big = resolve_bigtiff(params.bigtiff, w, h, 3, bytes_per_sample, icc_bytes);

    // Only the u16 path quantizes and can clamp out-of-range samples. f32 is
    // written verbatim (HDR-preserving, no clamp), but we still scan it for
    // non-finite samples so a NaN/inf numerical fault surfaces at either depth.
    match (params.depth(), big) {
        (OutDepth::U16, false) => {
            let (data, report) = quantize_u16(&image.rgb);
            let stats = channel_means_u16(&data);
            encode_planar::<_, TiffKindStandard, RGB16>(
                TiffEncoder::new(writer)?,
                w,
                h,
                &data,
                icc,
            )?;
            Ok(EncodeOutcome {
                loss: report,
                stats,
            })
        }
        (OutDepth::U16, true) => {
            let (data, report) = quantize_u16(&image.rgb);
            let stats = channel_means_u16(&data);
            encode_planar::<_, TiffKindBig, RGB16>(
                TiffEncoder::new_big(writer)?,
                w,
                h,
                &data,
                icc,
            )?;
            Ok(EncodeOutcome {
                loss: report,
                stats,
            })
        }
        (OutDepth::F32, false) => {
            let report = scan_non_finite(&image.rgb);
            let stats = channel_means_f32(&image.rgb);
            encode_planar::<_, TiffKindStandard, RGB32Float>(
                TiffEncoder::new(writer)?,
                w,
                h,
                &image.rgb,
                icc,
            )?;
            Ok(EncodeOutcome {
                loss: report,
                stats,
            })
        }
        (OutDepth::F32, true) => {
            let report = scan_non_finite(&image.rgb);
            let stats = channel_means_f32(&image.rgb);
            encode_planar::<_, TiffKindBig, RGB32Float>(
                TiffEncoder::new_big(writer)?,
                w,
                h,
                &image.rgb,
                icc,
            )?;
            Ok(EncodeOutcome {
                loss: report,
                stats,
            })
        }
    }
}

fn export_ir_to_writer<W: Write + Seek>(
    writer: W,
    image: &LinearImage,
    depth: OutDepth,
) -> Result<()> {
    let ir = image.ir.as_deref().ok_or_else(no_ir_error)?;
    let (w, h) = (image.width, image.height);
    let big = resolve_bigtiff(BigTiff::Auto, w, h, 1, depth_bytes(depth), 0);

    match (depth, big) {
        (OutDepth::U16, false) => {
            // IR is normalized to [0,1] at decode and carried through untouched,
            // so quantization cannot clip it — the report is provably all-zero
            // and safe to drop. Revisit if IR-processing stages ever land.
            let (data, report) = quantize_u16(ir);
            debug_assert!(!report.any_loss(), "IR plane unexpectedly clipped");
            encode_planar::<_, TiffKindStandard, Gray16>(
                TiffEncoder::new(writer)?,
                w,
                h,
                &data,
                None,
            )
        }
        (OutDepth::U16, true) => {
            let (data, report) = quantize_u16(ir);
            debug_assert!(!report.any_loss(), "IR plane unexpectedly clipped");
            encode_planar::<_, TiffKindBig, Gray16>(
                TiffEncoder::new_big(writer)?,
                w,
                h,
                &data,
                None,
            )
        }
        (OutDepth::F32, false) => encode_planar::<_, TiffKindStandard, Gray32Float>(
            TiffEncoder::new(writer)?,
            w,
            h,
            ir,
            None,
        ),
        (OutDepth::F32, true) => encode_planar::<_, TiffKindBig, Gray32Float>(
            TiffEncoder::new_big(writer)?,
            w,
            h,
            ir,
            None,
        ),
    }
}

/// The one place pixels actually hit the `tiff` encoder. Generic over the file
/// kind (classic vs BigTIFF) and the color type (u16/f32 × RGB/Gray) so the four
/// depth×size combinations share a single body. The ICC blob, when present, is
/// written as the `ICCProfile` tag (34675) before the sample data.
fn encode_planar<W, K, C>(
    encoder: TiffEncoder<W, K>,
    width: u32,
    height: u32,
    data: &[C::Inner],
    icc: Option<&[u8]>,
) -> Result<()>
where
    W: Write + Seek,
    K: TiffKind,
    C: ColorType,
    [C::Inner]: TiffValue,
{
    let mut encoder = encoder;
    let mut image = encoder
        .new_image::<C>(width, height)
        .map_err(|e| NcError::Write(format!("starting TIFF image: {e}")))?;
    if let Some(blob) = icc {
        image
            .encoder()
            .write_tag(Tag::IccProfile, blob)
            .map_err(|e| NcError::Write(format!("writing ICC profile tag: {e}")))?;
    }
    image
        .write_data(data)
        .map_err(|e| NcError::Write(format!("writing TIFF sample data: {e}")))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn no_ir_error() -> NcError {
    NcError::Unsupported("cannot export IR: image has no IR plane (HDRi input only)".into())
}

fn depth_bytes(depth: OutDepth) -> u64 {
    match depth {
        OutDepth::U16 => 2,
        OutDepth::F32 => 4,
    }
}

/// Decide whether to emit BigTIFF. `On`/`Off` force the choice; `Auto` estimates
/// the file size (`width*height*channels*bytes`, plus `extra_bytes` for the
/// embedded ICC, plus a margin for tags/strips) and promotes once it would exceed
/// the classic 32-bit-offset limit.
fn resolve_bigtiff(
    policy: BigTiff,
    width: u32,
    height: u32,
    channels: u64,
    bytes: u64,
    extra_bytes: u64,
) -> bool {
    match policy {
        BigTiff::On => true,
        BigTiff::Off => false,
        BigTiff::Auto => {
            let sample_bytes = (width as u64)
                .saturating_mul(height as u64)
                .saturating_mul(channels)
                .saturating_mul(bytes);
            sample_bytes
                .saturating_add(extra_bytes)
                .saturating_add(BIGTIFF_MARGIN_BYTES)
                > CLASSIC_TIFF_LIMIT
        }
    }
}

/// Quantize linear `f32` samples in `[0, 1]` to `u16` `[0, 65535]`, returning the
/// quantized data alongside an [`EncodeReport`] counting the samples that lost
/// information. Out-of-range values are clamped rather than wrapped (a quietly
/// wrapped pixel would violate "fail loudly") *and* counted, so the caller can
/// surface the loss as a report warning. Rounding is round-half-away-from-zero via
/// `f32::round` — chosen for determinism and simplicity.
///
/// Non-finite samples get their own branch: `NaN` is neither `< 0.0` nor `> 1.0`
/// so the range comparisons miss it, yet `NaN as u16` saturates to 0 — a pixel
/// silently turned black. `±inf` would clamp sanely but is a numerical fault, not
/// an in-gamut value. Both are a live possibility (the density algorithm's
/// log/division math), so any non-finite sample is counted as `non_finite` (kept
/// out of the `clipped_*` finite-clamp tallies) to keep the fault visible.
fn quantize_u16(samples: &[f32]) -> (Vec<u16>, EncodeReport) {
    let mut report = EncodeReport {
        total_samples: samples.len() as u64,
        ..EncodeReport::default()
    };
    let data = samples
        .iter()
        .map(|&v| {
            if !v.is_finite() {
                report.non_finite += 1;
            } else if v < 0.0 {
                report.clipped_low += 1;
            } else if v > 1.0 {
                report.clipped_high += 1;
            }
            (v.clamp(0.0, 1.0) * 65535.0).round() as u16
        })
        .collect();
    (data, report)
}

/// Per-channel mean of quantized u16 samples, normalized back to `[0, 1]` — the
/// statistic of what the u16 file actually contains.
///
/// The accumulation is **integer** (`u64` sums of the written u16 values), so the
/// result is exactly reproducible on every target given identical pixels: no
/// floating-point summation order or rounding enters it. Interleaved RGB, so
/// channel = `index % 3`; a sample count that isn't a multiple of 3 can't occur for
/// an RGB image (`LinearImage` enforces it), and a partial tail would simply
/// contribute to the channels it covers.
fn channel_means_u16(data: &[u16]) -> OutputStats {
    let mut sums = [0u64; 3];
    let mut counts = [0u64; 3];
    for (i, &v) in data.iter().enumerate() {
        let c = i % 3;
        sums[c] += v as u64;
        counts[c] += 1;
    }
    OutputStats {
        mean: std::array::from_fn(|c| {
            if counts[c] == 0 {
                0.0
            } else {
                sums[c] as f64 / counts[c] as f64 / 65535.0
            }
        }),
    }
}

/// Per-channel mean of verbatim-written f32 samples (HDR: unclamped, so a mean may
/// exceed 1.0).
///
/// Non-finite samples are excluded from both the sum and the count — a single `NaN`
/// would otherwise poison the whole statistic, hiding the very comparison the mean
/// exists for; the fault itself is reported by `EncodeReport::non_finite`. Summation
/// is sequential in `f64`, so it is deterministic for a given build.
fn channel_means_f32(data: &[f32]) -> OutputStats {
    let mut sums = [0f64; 3];
    let mut counts = [0u64; 3];
    for (i, &v) in data.iter().enumerate() {
        if v.is_finite() {
            let c = i % 3;
            sums[c] += v as f64;
            counts[c] += 1;
        }
    }
    OutputStats {
        mean: std::array::from_fn(|c| {
            if counts[c] == 0 {
                0.0
            } else {
                sums[c] / counts[c] as f64
            }
        }),
    }
}

/// Scan verbatim-written f32 samples for non-finite values. f32 output is not
/// clamped (HDR is preserved), so there is no `clipped_*` accounting — but a
/// `NaN`/`inf` still signals a pipeline numerical fault that must surface, so it
/// is counted here just as the u16 path counts it.
fn scan_non_finite(samples: &[f32]) -> EncodeReport {
    EncodeReport {
        total_samples: samples.len() as u64,
        non_finite: samples.iter().filter(|v| !v.is_finite()).count() as u64,
        ..EncodeReport::default()
    }
}

// `tiff`'s encoder errors surface as `NcError::Write` — a TIFF that won't start is
// an output-write failure (design-spec §11, exit 5).
impl From<tiff::TiffError> for NcError {
    fn from(e: tiff::TiffError) -> Self {
        NcError::Write(format!("tiff: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutputPreset;
    use std::io::Cursor;
    use tiff::decoder::{Decoder, DecodingResult};

    fn img(width: u32, height: u32, rgb: Vec<f32>, ir: Option<Vec<f32>>) -> LinearImage {
        LinearImage::new(width, height, rgb, ir).unwrap()
    }

    fn out(depth: OutDepth, bigtiff: BigTiff) -> OutputParams {
        OutputParams {
            // Stated, not defaulted: `output.depth` is consulted only by `legacy` /
            // `custom`, and the default preset (`gain-map-hdr`) pins u16 — so these
            // depth tests would silently all become u16 tests without it.
            preset: OutputPreset::Legacy,
            depth,
            output_profile: None,
            bigtiff,
        }
    }

    /// Classic TIFF carries magic 42, BigTIFF carries 43, in the file's byte order
    /// (the `tiff` crate writes little-endian "II").
    fn is_bigtiff(bytes: &[u8]) -> bool {
        assert_eq!(&bytes[0..2], b"II", "expected little-endian TIFF");
        let magic = u16::from_le_bytes([bytes[2], bytes[3]]);
        match magic {
            42 => false,
            43 => true,
            other => panic!("not a TIFF magic: {other}"),
        }
    }

    fn encode_bytes(image: &LinearImage, params: &OutputParams, icc: Option<&[u8]>) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let _ = encode_to_writer(&mut buf, image, params, icc).unwrap();
        buf.into_inner()
    }

    fn encode_report(image: &LinearImage, params: &OutputParams) -> EncodeReport {
        encode_outcome(image, params).loss
    }

    fn encode_outcome(image: &LinearImage, params: &OutputParams) -> EncodeOutcome {
        let mut buf = Cursor::new(Vec::new());
        encode_to_writer(&mut buf, image, params, None).unwrap()
    }

    #[test]
    fn u16_round_trips_within_quantization() {
        // Values chosen so the expected u16 is exact, plus an out-of-range value
        // that must clamp rather than wrap.
        let image = img(2, 1, vec![0.0, 1.0, 0.5, 0.25, 2.0, -1.0], None);
        let bytes = encode_bytes(&image, &out(OutDepth::U16, BigTiff::Off), None);

        let mut dec = Decoder::new(Cursor::new(bytes)).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (2, 1));
        let DecodingResult::U16(pixels) = dec.read_image().unwrap() else {
            panic!("expected u16 image");
        };
        // 0→0, 1→65535, 0.5→32768 (round half up), 0.25→16384, 2.0 clamps→65535,
        // -1.0 clamps→0.
        assert_eq!(pixels, vec![0, 65535, 32768, 16384, 65535, 0]);
    }

    #[test]
    fn u16_reports_clipping_counts() {
        // Two samples below 0 and one above 1; the rest in range. The encoder
        // must count each clamp so the caller can warn (color-management does
        // not clamp — that job is delegated here).
        let image = img(2, 1, vec![-0.5, -2.0, 0.5, 0.25, 1.0, 3.0], None);
        let report = encode_report(&image, &out(OutDepth::U16, BigTiff::Off));
        assert_eq!(report.total_samples, 6);
        assert_eq!(report.clipped_low, 2);
        assert_eq!(report.clipped_high, 1);
        assert_eq!(report.non_finite, 0);
        assert_eq!(report.clipped_total(), 3);
        assert!(report.any_loss());
        assert_eq!(report.loss_fraction(), 0.5);
    }

    #[test]
    fn u16_reports_non_finite_samples() {
        // Non-finite pixels (e.g. from density-domain log/division math) must be
        // counted, not silently turned black — that is the "fail loudly" rule.
        // NaN and ±inf all count as non_finite, kept out of the finite clip tally.
        let image = img(
            2,
            1,
            vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5, 0.5, 0.5],
            None,
        );
        let report = encode_report(&image, &out(OutDepth::U16, BigTiff::Off));
        assert_eq!(report.non_finite, 3); // NaN, +inf, -inf
        assert_eq!(report.clipped_high, 0);
        assert_eq!(report.clipped_low, 0);
        assert!(report.any_loss());
    }

    #[test]
    fn f32_writes_verbatim_but_counts_non_finite() {
        // HDR values > 1.0 are written verbatim with no clip, so finite
        // out-of-[0,1] samples produce no loss...
        let clean = img(2, 1, vec![-0.5, 0.5, 1.0, 1.5, 7.25, 42.0], None);
        let report = encode_report(&clean, &out(OutDepth::F32, BigTiff::Off));
        assert_eq!(report.total_samples, 6);
        assert_eq!(report.clipped_total(), 0);
        assert_eq!(report.non_finite, 0);
        assert!(!report.any_loss());

        // ...but a NaN/inf is still a numerical fault and must surface even
        // though f32 writes it verbatim.
        let faulty = img(
            2,
            1,
            vec![f32::NAN, 0.5, f32::INFINITY, 0.5, 0.5, 0.5],
            None,
        );
        let report = encode_report(&faulty, &out(OutDepth::F32, BigTiff::Off));
        assert_eq!(report.non_finite, 2);
        assert!(report.any_loss());
    }

    #[test]
    fn u16_stats_are_the_means_of_the_written_clamped_samples() {
        // The mean is of what the *file* holds: the clamped, quantized values
        // (2.0 → 65535 → 1.0; -1.0 → 0), not the incoming floats — that is what
        // makes two builds' means directly comparable.
        let image = img(2, 1, vec![0.0, 1.0, 0.5, 2.0, -1.0, 0.25], None);
        let stats = encode_outcome(&image, &out(OutDepth::U16, BigTiff::Off)).stats;
        assert_eq!(stats.mean[0], (0.0 + 1.0) / 2.0);
        assert_eq!(stats.mean[1], (1.0 + 0.0) / 2.0);
        // 0.5 → 32768/65535, 0.25 → 16384/65535: exact integer arithmetic.
        assert_eq!(stats.mean[2], (32768.0 / 65535.0 + 16384.0 / 65535.0) / 2.0);
    }

    #[test]
    fn f32_stats_keep_hdr_values_and_skip_non_finite() {
        // f32 is written verbatim, so the mean may exceed 1.0...
        let clean = img(2, 1, vec![0.5, 0.0, 0.0, 1.5, 0.0, 0.0], None);
        let stats = encode_outcome(&clean, &out(OutDepth::F32, BigTiff::Off)).stats;
        assert_eq!(stats.mean[0], 1.0); // (0.5 + 1.5) / 2

        // ...and a NaN is excluded rather than poisoning the whole channel mean
        // (the fault is reported by `EncodeReport::non_finite`).
        let faulty = img(2, 1, vec![f32::NAN, 0.0, 0.0, 0.75, 0.0, 0.0], None);
        let outcome = encode_outcome(&faulty, &out(OutDepth::F32, BigTiff::Off));
        assert_eq!(outcome.loss.non_finite, 1);
        assert_eq!(outcome.stats.mean[0], 0.75);
    }

    #[test]
    fn stats_of_an_empty_channel_are_zero_not_nan() {
        // Defensive: a zero-sample image must not divide by zero into a NaN that
        // would then serialize as `null` in the report.
        assert_eq!(channel_means_u16(&[]).mean, [0.0, 0.0, 0.0]);
        assert_eq!(channel_means_f32(&[]).mean, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn f32_round_trips_exactly_including_hdr() {
        // f32 must preserve values > 1.0 (HDR) with no clamp and no precision loss.
        let rgb = vec![0.0, 0.5, 1.0, 1.5, 7.25, 42.0];
        let image = img(2, 1, rgb.clone(), None);
        let bytes = encode_bytes(&image, &out(OutDepth::F32, BigTiff::Off), None);

        let mut dec = Decoder::new(Cursor::new(bytes)).unwrap();
        let DecodingResult::F32(pixels) = dec.read_image().unwrap() else {
            panic!("expected f32 image");
        };
        assert_eq!(pixels, rgb);
    }

    #[test]
    fn bigtiff_policy_controls_header() {
        let image = img(2, 1, vec![0.0; 6], None);
        // Off → classic, On → big, regardless of (tiny) size.
        assert!(!is_bigtiff(&encode_bytes(
            &image,
            &out(OutDepth::U16, BigTiff::Off),
            None
        )));
        assert!(is_bigtiff(&encode_bytes(
            &image,
            &out(OutDepth::U16, BigTiff::On),
            None
        )));
        // Auto stays classic for a small image.
        assert!(!is_bigtiff(&encode_bytes(
            &image,
            &out(OutDepth::U16, BigTiff::Auto),
            None
        )));
    }

    #[test]
    fn auto_promotes_past_classic_limit() {
        // Estimate-only (no allocation): a synthetic large image must trip Auto.
        // ~1.5 GiB at f32×3ch exceeds 4 GiB? No — pick dims whose sample bytes
        // exceed u32::MAX: 40000 * 40000 * 3 * 4 ≈ 19.2 GB.
        assert!(resolve_bigtiff(BigTiff::Auto, 40_000, 40_000, 3, 4, 0));
        // Just under the limit stays classic.
        assert!(!resolve_bigtiff(BigTiff::Auto, 1000, 1000, 3, 2, 0));
        // On/Off ignore size.
        assert!(resolve_bigtiff(BigTiff::On, 1, 1, 1, 1, 0));
        assert!(!resolve_bigtiff(BigTiff::Off, 40_000, 40_000, 3, 4, 0));
    }

    #[test]
    fn auto_counts_icc_bytes_in_sizing() {
        // Sample data sits just under the classic limit; a large ICC pushes the
        // total over, so Auto must promote (ignoring the ICC would wrongly stay
        // classic and fail at encode time).
        let bytes = CLASSIC_TIFF_LIMIT - (8 << 20); // 8 MiB of headroom
        let (w, h) = (bytes / 3 / 2, 1); // u16 RGB sample bytes ≈ `bytes`
        assert!(!resolve_bigtiff(BigTiff::Auto, w as u32, h, 3, 2, 0));
        // A 16 MiB ICC blob exceeds the headroom + margin → promote.
        assert!(resolve_bigtiff(BigTiff::Auto, w as u32, h, 3, 2, 16 << 20));
    }

    #[test]
    fn embedded_icc_is_present_and_readable() {
        let icc = b"fake-icc-profile-bytes".to_vec();
        let image = img(2, 1, vec![0.0; 6], None);
        let bytes = encode_bytes(&image, &out(OutDepth::U16, BigTiff::Off), Some(&icc));

        let mut dec = Decoder::new(Cursor::new(bytes)).unwrap();
        let read = dec.get_tag_u8_vec(Tag::IccProfile).unwrap();
        assert_eq!(read, icc);
    }

    #[test]
    fn export_ir_writes_single_channel() {
        let image = img(2, 1, vec![0.0; 6], Some(vec![0.25, 0.75]));
        let mut buf = Cursor::new(Vec::new());
        export_ir_to_writer(&mut buf, &image, OutDepth::U16).unwrap();

        let mut dec = Decoder::new(Cursor::new(buf.into_inner())).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (2, 1));
        let DecodingResult::U16(pixels) = dec.read_image().unwrap() else {
            panic!("expected u16 IR image");
        };
        assert_eq!(pixels, vec![16384, 49151]);
    }

    #[test]
    fn export_ir_errors_without_ir_plane() {
        let image = img(2, 1, vec![0.0; 6], None);
        let mut buf = Cursor::new(Vec::new());
        let err = export_ir_to_writer(&mut buf, &image, OutDepth::U16).unwrap_err();
        assert!(matches!(err, NcError::Unsupported(_)));
    }

    #[test]
    fn export_ir_without_plane_does_not_create_file() {
        // The no-IR error must fire before the file is created, so an existing
        // target the user pointed --export-ir at is never clobbered.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("nc_no_ir_test_{}.tiff", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let image = img(2, 1, vec![0.0; 6], None);
        let err = export_ir(&image, OutDepth::U16, &path).unwrap_err();
        assert!(matches!(err, NcError::Unsupported(_)));
        assert!(!path.exists(), "no-IR export must not create the file");
    }

    #[test]
    fn sidecar_path_appends_json() {
        let dir = std::env::temp_dir();
        let output = dir.join(format!("nc_sidecar_test_{}.tiff", std::process::id()));
        let json = r#"{"algorithm":"density"}"#;
        let staged = write_sidecar(&output, json).unwrap();
        let sidecar = PathBuf::from(format!("{}.json", output.display()));
        // Staged, so nothing is at the sidecar path yet — the commit is what puts it
        // there. This test now covers the path derivation *and* that ordering.
        assert!(!sidecar.exists(), "the sidecar appears only on commit");
        staged.commit().unwrap();

        let read = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(read, json);
        // Valid JSON.
        let _: serde_json::Value = serde_json::from_str(&read).unwrap();
        let _ = std::fs::remove_file(&sidecar);
    }

    // -----------------------------------------------------------------------
    // hdr-linear-tiff
    // -----------------------------------------------------------------------

    /// Render a tiny real image through the production stages, so these tests
    /// exercise genuine renderer output and metadata rather than a hand-built
    /// struct that could drift from the renderer's contract (the `io::avif` tests'
    /// `render_tiny` precedent).
    ///
    /// `print_exposure` is the lever that reaches the HDR headroom: `simple`
    /// reconstruction of a `1.0 - v` scan yields a positive in `[0, 1]`, so without
    /// exposure nothing would ever exceed reference white and a
    /// "highlights survive" assertion would pass vacuously. At 2.5 stops the
    /// samples span ≈1.13 up to exactly `LINEAR_HEADROOM`.
    fn render_linear_tiny(rgb: &[f32], w: u32, h: u32, print_exposure: f32) -> LinearBt2020Hdr {
        use crate::algo::reconstruct;
        use crate::pipeline::render_split::display_source;
        use crate::pipeline::working_space::map_nc_film_rgb_v1;
        use crate::types::{FilmBase, PrintParams, Reconstruction};

        let scan = rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new(w, h, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        let print = PrintParams {
            print_exposure,
            ..PrintParams::default()
        };
        let shared = display_source(map_nc_film_rgb_v1(film), &print).unwrap();
        crate::pipeline::hdr::render_linear(&shared, 0.75).unwrap()
    }

    fn hdr_linear_params() -> OutputParams {
        OutputParams {
            preset: crate::types::OutputPreset::HdrLinearTiff,
            ..OutputParams::default()
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nc-hdr-linear-{tag}-{}.tiff", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Decode an RGB f32 TIFF back to its samples.
    fn decode_f32(bytes: &[u8]) -> (u32, u32, Vec<f32>) {
        let mut decoder = Decoder::new(Cursor::new(bytes)).unwrap();
        let (w, h) = decoder.dimensions().unwrap();
        match decoder.read_image().unwrap() {
            DecodingResult::F32(data) => (w, h, data),
            other => panic!("expected f32 samples, got {other:?}"),
        }
    }

    #[test]
    fn hdr_linear_tiff_round_trips_every_sample_bit_exactly() {
        // The lossless contract: an independent decode recovers identical `to_bits()`
        // for every sample, including the HDR values above reference white that no
        // integer TIFF could hold.
        let render = render_linear_tiny(
            &[0.0, 0.0, 0.0, 0.2, 0.2, 0.2, 0.5, 0.5, 0.5, 0.9, 0.9, 0.9],
            4,
            1,
            2.5,
        );
        let expected = render.image().rgb.clone();

        // Teeth: the fixture must actually exercise the headroom, or "highlights
        // survive" would be proven by an image that has none.
        assert!(
            expected.iter().any(|v| *v > 1.0),
            "fixture has no values above reference white: {expected:?}"
        );
        assert!(
            expected.contains(&crate::pipeline::hdr::LINEAR_HEADROOM),
            "fixture never reaches the 1000-nit peak: {expected:?}"
        );
        assert!(expected.contains(&0.0), "no black sample");

        let path = temp_path("roundtrip");
        let icc = crate::pipeline::color::hdr_linear_bt2020_icc().unwrap();
        let (staged, outcome, summary) =
            encode_hdr_linear(render, &hdr_linear_params(), &icc, &path).unwrap();
        staged.commit().unwrap();

        let (w, h, decoded) = decode_f32(&std::fs::read(&path).unwrap());
        assert_eq!((w, h), (4, 1));
        assert_eq!(decoded.len(), expected.len());
        for (i, (got, want)) in decoded.iter().zip(&expected).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "sample {i}: {got} != {want} (bit-exact round trip)"
            );
        }
        assert!(!outcome.loss.any_loss(), "clean render reported loss");
        assert_eq!(summary.bits_per_sample, 32);
        assert_eq!(summary.sample_format, SAMPLE_FORMAT_IEEE_FLOAT);
        assert_eq!(summary.pixel_contract, HDR_LINEAR_PIXEL_CONTRACT);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hdr_linear_tiff_applies_no_transfer_function() {
        // The falsifiable form of "linear": a sample at the 203-nit reference white
        // must be exactly 1.0 in the file. PQ would store ≈0.58 for the same
        // luminance and HLG ≈0.75, so this fails loudly if a transfer ever leaks in.
        //
        // 2.0 stops is exactly 4x, so a 0.25 positive lands on 1.0 by construction
        // rather than by a tolerance.
        let render = render_linear_tiny(&[0.25, 0.25, 0.25], 1, 1, 2.0);
        let path = temp_path("linear");
        let icc = crate::pipeline::color::hdr_linear_bt2020_icc().unwrap();
        let (staged, _, _) = encode_hdr_linear(render, &hdr_linear_params(), &icc, &path).unwrap();
        staged.commit().unwrap();

        let (_, _, decoded) = decode_f32(&std::fs::read(&path).unwrap());
        for (channel, value) in decoded.iter().enumerate() {
            assert!(
                (value - 1.0).abs() < 1e-6,
                "channel {channel}: reference white stored as {value}, expected 1.0 \
                 (PQ would be ≈0.58, HLG ≈0.75 — a transfer function leaked in)"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hdr_linear_tiff_embeds_the_linear_bt2020_profile_verbatim() {
        let render = render_linear_tiny(&[0.4, 0.3, 0.2], 1, 1, 1.0);
        let path = temp_path("icc");
        let icc = crate::pipeline::color::hdr_linear_bt2020_icc().unwrap();
        let (staged, _, summary) =
            encode_hdr_linear(render, &hdr_linear_params(), &icc, &path).unwrap();
        staged.commit().unwrap();
        assert_eq!(summary.icc_bytes, icc.len());

        let bytes = std::fs::read(&path).unwrap();
        let mut decoder = Decoder::new(Cursor::new(&bytes)).unwrap();
        let embedded = decoder
            .get_tag_u8_vec(Tag::IccProfile)
            .expect("no ICC profile tag in the written TIFF");
        assert_eq!(
            embedded, icc,
            "the embedded profile is not the one the caller resolved"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hdr_linear_tiff_honours_the_bigtiff_policy_and_reports_it() {
        for (policy, want_big) in [(BigTiff::Off, false), (BigTiff::On, true)] {
            let render = render_linear_tiny(&[0.5, 0.5, 0.5], 1, 1, 1.0);
            let params = OutputParams {
                bigtiff: policy,
                ..hdr_linear_params()
            };
            let path = temp_path(&format!("big-{policy:?}"));
            let icc = crate::pipeline::color::hdr_linear_bt2020_icc().unwrap();
            let (staged, _, summary) = encode_hdr_linear(render, &params, &icc, &path).unwrap();
            staged.commit().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(summary.bigtiff, want_big, "{policy:?}: summary disagrees");
            assert_eq!(
                is_bigtiff(&bytes),
                want_big,
                "{policy:?}: file magic disagrees with the policy"
            );
            // Either way the samples must still decode as f32.
            let (_, _, decoded) = decode_f32(&bytes);
            assert_eq!(decoded.len(), 3);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn hdr_linear_tiff_counts_non_finite_without_laundering_it() {
        // The renderer cannot emit a non-finite sample — `render_linear` fails first —
        // so this exercises the accounting through the writer-generic seam rather
        // than through the opaque type. It is a tripwire for a future path that
        // reaches the encoder with a numerical fault: the count must surface, and the
        // value must still be written verbatim rather than quietly turned into 0.
        let image = img(
            2,
            1,
            vec![f32::NAN, 0.5, f32::INFINITY, 0.5, 0.5, 0.5],
            None,
        );
        let mut buf = Cursor::new(Vec::new());
        let icc = crate::pipeline::color::hdr_linear_bt2020_icc().unwrap();
        let outcome = encode_hdr_linear_to_writer(&mut buf, &image, false, &icc).unwrap();
        assert_eq!(outcome.loss.non_finite, 2);
        assert_eq!(outcome.loss.clipped_low, 0, "f32 must not clamp");
        assert_eq!(outcome.loss.clipped_high, 0, "f32 must not clamp");
        assert_eq!(outcome.loss.total_samples, 6);

        let (_, _, decoded) = decode_f32(buf.get_ref());
        assert!(decoded[0].is_nan(), "NaN was laundered into {}", decoded[0]);
        assert_eq!(decoded[2], f32::INFINITY);
    }

    // -----------------------------------------------------------------------
    // hdr-pq-tiff / hdr-hlg-tiff
    // -----------------------------------------------------------------------

    fn render_coded_tiny(
        transfer: crate::pipeline::hdr::HdrTransfer,
        rgb: &[f32],
        w: u32,
        h: u32,
    ) -> crate::pipeline::hdr::RenderedHdr {
        use crate::algo::reconstruct;
        use crate::pipeline::render_split::display_source;
        use crate::pipeline::working_space::map_nc_film_rgb_v1;
        use crate::types::{FilmBase, PrintParams, Reconstruction};

        let scan = rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new(w, h, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        let shared = display_source(map_nc_film_rgb_v1(film), &PrintParams::default()).unwrap();
        crate::pipeline::hdr::render(&shared, transfer, 0.75).unwrap()
    }

    fn coded_params(preset: crate::types::OutputPreset) -> OutputParams {
        OutputParams {
            preset,
            ..OutputParams::default()
        }
    }

    fn decode_u16(bytes: &[u8]) -> (u32, u32, Vec<u16>) {
        let mut decoder = Decoder::new(Cursor::new(bytes)).unwrap();
        let (w, h) = decoder.dimensions().unwrap();
        match decoder.read_image().unwrap() {
            DecodingResult::U16(data) => (w, h, data),
            other => panic!("expected u16 samples, got {other:?}"),
        }
    }

    #[test]
    fn coded_hdr_tiff_stores_every_code_value_exactly() {
        // "Lossless relative to the quantized signal": whatever the single
        // quantization step produces, TIFF must give back bit-identically.
        use crate::pipeline::hdr::HdrTransfer;
        for (transfer, preset) in [
            (HdrTransfer::Pq, crate::types::OutputPreset::HdrPqTiff),
            (HdrTransfer::Hlg, crate::types::OutputPreset::HdrHlgTiff),
        ] {
            let render = render_coded_tiny(
                transfer,
                &[0.0, 0.0, 0.0, 0.2, 0.5, 0.8, 0.5, 0.5, 0.5, 1.0, 1.0, 1.0],
                4,
                1,
            );
            // Recompute the expected codes from the renderer's own samples with the
            // documented rule, independently of the encoder's loop.
            // Binary64, matching the encoder: an `f32` oracle would disagree with it
            // on samples whose exact product sits near a half-code boundary.
            let expected: Vec<u16> = render
                .image()
                .rgb()
                .iter()
                .map(|v| (f64::from(*v) * 65535.0).round() as u16)
                .collect();
            let icc = if transfer == HdrTransfer::Pq {
                crate::pipeline::color::hdr_pq_tiff_icc().unwrap()
            } else {
                crate::pipeline::color::hdr_hlg_tiff_icc().unwrap()
            };
            let path = temp_path(&format!("coded-{transfer:?}"));
            let (staged, outcome, summary) =
                encode_hdr_coded(render, &coded_params(preset), &icc, &path).unwrap();
            staged.commit().unwrap();

            let (_, _, decoded) = decode_u16(&std::fs::read(&path).unwrap());
            assert_eq!(
                decoded, expected,
                "{transfer:?}: stored codes are not exact"
            );
            assert_eq!(summary.bits_per_sample, 16);
            assert_eq!(summary.sample_format, SAMPLE_FORMAT_UNSIGNED);
            // Nothing clipped and nothing non-finite: the domain was verified, so a
            // `--strict` run must not be handed a warning here.
            assert!(!outcome.loss.any_loss(), "{transfer:?}: reported loss");
            // Rounding cannot be worse than half a code.
            assert!(
                summary.max_quantization_error_codes <= 0.5,
                "{transfer:?}: max error {} exceeds half a code",
                summary.max_quantization_error_codes
            );
            assert!(summary.rms_quantization_error_codes <= summary.max_quantization_error_codes);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn coded_hdr_tiff_reports_the_quantization_error_it_actually_made() {
        // The reported max/RMS must match an independent calculation over the same
        // samples — the task requires the numbers be checkable, not decorative.
        let samples = [0.0_f32, 0.5, 1.0, 0.123_456_7, 0.999_999, 1.0 / 3.0];
        let (_, _, _, error) = quantize_coded_u16(&samples).unwrap();
        // The oracle must use binary64 like the implementation — computing it in
        // `f32` would just re-derive the double-rounding defect and "confirm" it.
        let mut worst = 0.0_f64;
        let mut squared = 0.0_f64;
        for value in samples {
            let scaled = f64::from(value) * 65535.0;
            let residual = (scaled - scaled.round()).abs();
            worst = worst.max(residual);
            squared += residual * residual;
        }
        let rms = (squared / samples.len() as f64).sqrt() as f32;
        let worst = worst as f32;
        assert_eq!(error.max, worst);
        assert!((error.rms - rms).abs() < 1e-6);
        // A value exactly on a code boundary costs nothing; 1/3 is the worst here.
        assert_eq!(quantize_coded_u16(&[0.0, 1.0]).unwrap().3.max, 0.0);
    }

    #[test]
    fn coded_hdr_tiff_picks_the_nearest_code_even_at_a_half_code_boundary() {
        // Regression for a double-rounding defect: scaling in `f32` rounds into the
        // product *and* in `round()`, so this sample's exact product (65305.4995957,
        // nearest code 65305) evaluated to exactly `65305.5_f32` and stored 65306 —
        // a non-nearest code whose real error is 0.5004, reported as 0.5. Both the
        // stored code and the reported error are checked, because the `f32` path got
        // the second one *looking* correct while being wrong.
        let value = 0.996_498_05_f32;
        let exact = f64::from(value) * 65535.0;
        let nearest = exact.round() as u16;
        assert_eq!(
            nearest, 65305,
            "the independently computed nearest code moved"
        );
        assert_eq!(
            (value * 65535.0_f32).round() as u16,
            65306,
            "the f32 path no longer reproduces the defect — pick a fresh witness"
        );

        let (codes, _, _, error) = quantize_coded_u16(&[value]).unwrap();
        assert_eq!(codes, vec![nearest], "stored a non-nearest code");
        // The advertised bound now actually holds for the code that was stored.
        let true_error = (exact - f64::from(nearest)).abs();
        assert!(
            f64::from(error.max) >= true_error - 1e-6 && error.max <= 0.5,
            "reported max {} must not understate the true error {true_error}",
            error.max
        );
    }

    #[test]
    fn coded_hdr_tiff_rejects_an_out_of_domain_sample_instead_of_clipping() {
        // The renderer cannot produce these — `encode_transfer` fails first — so this
        // is a tripwire. It must *refuse*, not clamp: a clamped code would be a
        // silently wrong pixel, and this path has no legitimate clipping to count
        // (unlike the unclamped legacy `encode`).
        for bad in [1.000_001_f32, -0.000_001, f32::NAN, f32::INFINITY] {
            let err = quantize_coded_u16(&[0.5, bad, 0.5]).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("outside the encodable domain"),
                "{bad}: {message}"
            );
            // Pin the *rendered index*, not just a stray `1` — the message already
            // contains one via "finite values in [0, 1]", so a laxer assertion would
            // stay green with the index dropped, and naming the offending pixel is
            // the whole reason this path refuses instead of clipping.
            assert!(
                message.contains("sample 1 is outside"),
                "{bad}: error must name the offending sample index: {message}"
            );
        }
        // And the in-domain boundaries are accepted, so the guard is not overzealous.
        let (codes, ..) = quantize_coded_u16(&[0.0, 1.0, 0.5]).unwrap();
        assert_eq!(codes, vec![0, 65535, 32768]);
    }

    #[test]
    fn hdr_linear_tiff_is_staged_and_deterministic() {
        // Two facts in one fixture: the destination does not exist until the caller
        // commits (so a later failure in the run leaves nothing behind), and two
        // encodes of the same render are byte-identical on this build.
        let path = temp_path("staged");
        let icc = crate::pipeline::color::hdr_linear_bt2020_icc().unwrap();
        let first = {
            let render = render_linear_tiny(&[0.3, 0.6, 0.9], 1, 1, 1.5);
            let (staged, _, _) =
                encode_hdr_linear(render, &hdr_linear_params(), &icc, &path).unwrap();
            assert!(
                !path.exists(),
                "the output must appear only when the caller commits"
            );
            staged.commit().unwrap();
            std::fs::read(&path).unwrap()
        };

        let second_path = temp_path("staged-2");
        let render = render_linear_tiny(&[0.3, 0.6, 0.9], 1, 1, 1.5);
        let (staged, _, _) =
            encode_hdr_linear(render, &hdr_linear_params(), &icc, &second_path).unwrap();
        staged.commit().unwrap();
        let second = std::fs::read(&second_path).unwrap();

        assert_eq!(
            first, second,
            "repeated encodes of the same render must be byte-identical"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&second_path);
    }
}
