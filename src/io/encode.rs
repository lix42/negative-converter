//! [`LinearImage`] → 16-bit / 32-bit-float TIFF, embedded ICC, sidecar JSON,
//! optional IR export.
//!
//! Pure-ish encode stage: the public `&Path` entry points wrap a thin
//! `*_to_writer` core generic over `Write + Seek`, so the unit tests can encode
//! into an in-memory `Cursor` and decode the bytes straight back — no temp files,
//! fully deterministic. Crate-specific `tiff` types stay confined to this module
//! (the neutral contract lives in [`crate::types`]).

use std::ffi::OsString;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use tiff::encoder::colortype::{ColorType, Gray16, Gray32Float, RGB16, RGB32Float};
use tiff::encoder::{TiffEncoder, TiffKind, TiffKindBig, TiffKindStandard, TiffValue};
use tiff::tags::Tag;

use crate::types::{BigTiff, EncodeReport, LinearImage, NcError, OutDepth, OutputParams, Result};

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
/// Returns an [`EncodeReport`] recording any quantization clipping so the caller
/// can fold it into the JSON report (and `--strict` can promote it to an error).
pub fn encode(
    image: &LinearImage,
    params: &OutputParams,
    icc: Option<&[u8]>,
    path: &Path,
) -> Result<EncodeReport> {
    // Borrow the BufWriter into the encoder rather than moving it, so we still
    // own it afterward and can flush explicitly — see `flush_buf`.
    let mut writer = BufWriter::new(create(path)?);
    let report = encode_to_writer(&mut writer, image, params, icc)?;
    flush_buf(&mut writer, path)?;
    Ok(report)
}

/// Write the IR plane as a single-channel TIFF at `depth`. Errors loudly when the
/// image carries no IR plane rather than writing an empty/placeholder file — the
/// caller asked for IR export, so a missing plane is a real failure.
pub fn export_ir(image: &LinearImage, depth: OutDepth, path: &Path) -> Result<()> {
    // Check for the IR plane *before* creating the file: a no-IR error must not
    // truncate/clobber an existing target the user pointed `--export-ir` at.
    if image.ir.is_none() {
        return Err(no_ir_error());
    }
    let mut writer = BufWriter::new(create(path)?);
    export_ir_to_writer(&mut writer, image, depth)?;
    flush_buf(&mut writer, path)
}

/// Write the effective recipe JSON to the sidecar next to the output. The sidecar
/// path is `<output>.json` (e.g. `out.tiff` → `out.tiff.json`), so an output and
/// its recipe stay paired by name.
pub fn write_sidecar(output_path: &Path, recipe_json: &str) -> Result<()> {
    let mut name = OsString::from(output_path.as_os_str());
    name.push(".json");
    let sidecar = PathBuf::from(name);
    std::fs::write(&sidecar, recipe_json)
        .map_err(|e| NcError::Write(format!("writing sidecar {}: {e}", sidecar.display())))
}

// ---------------------------------------------------------------------------
// Writer-generic core (the testable seam)
// ---------------------------------------------------------------------------

fn encode_to_writer<W: Write + Seek>(
    writer: W,
    image: &LinearImage,
    params: &OutputParams,
    icc: Option<&[u8]>,
) -> Result<EncodeReport> {
    let (w, h) = (image.width, image.height);
    let bytes_per_sample = depth_bytes(params.out_depth);
    let icc_bytes = icc.map_or(0, |b| b.len() as u64);
    let big = resolve_bigtiff(params.bigtiff, w, h, 3, bytes_per_sample, icc_bytes);

    // Only the u16 path quantizes and can clamp out-of-range samples. f32 is
    // written verbatim (HDR-preserving, no clamp), but we still scan it for
    // non-finite samples so a NaN/inf numerical fault surfaces at either depth.
    match (params.out_depth, big) {
        (OutDepth::U16, false) => {
            let (data, report) = quantize_u16(&image.rgb);
            encode_planar::<_, TiffKindStandard, RGB16>(
                TiffEncoder::new(writer)?,
                w,
                h,
                &data,
                icc,
            )?;
            Ok(report)
        }
        (OutDepth::U16, true) => {
            let (data, report) = quantize_u16(&image.rgb);
            encode_planar::<_, TiffKindBig, RGB16>(
                TiffEncoder::new_big(writer)?,
                w,
                h,
                &data,
                icc,
            )?;
            Ok(report)
        }
        (OutDepth::F32, false) => {
            let report = scan_non_finite(&image.rgb);
            encode_planar::<_, TiffKindStandard, RGB32Float>(
                TiffEncoder::new(writer)?,
                w,
                h,
                &image.rgb,
                icc,
            )?;
            Ok(report)
        }
        (OutDepth::F32, true) => {
            let report = scan_non_finite(&image.rgb);
            encode_planar::<_, TiffKindBig, RGB32Float>(
                TiffEncoder::new_big(writer)?,
                w,
                h,
                &image.rgb,
                icc,
            )?;
            Ok(report)
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

fn create(path: &Path) -> Result<File> {
    File::create(path).map_err(|e| NcError::Write(format!("creating {}: {e}", path.display())))
}

fn no_ir_error() -> NcError {
    NcError::Unsupported("cannot export IR: image has no IR plane (HDRi input only)".into())
}

/// Flush buffered output explicitly, surfacing the error. `BufWriter`'s implicit
/// flush on drop discards any error (e.g. a full disk on the final block), so a
/// dropped-without-flush writer would silently truncate the TIFF — exactly the
/// "fail loudly" violation this stage must avoid. The `tiff` encoder never flushes
/// and gives no way to reclaim the moved writer, so callers own the BufWriter and
/// flush it here once encoding returns.
fn flush_buf<W: Write>(writer: &mut W, path: &Path) -> Result<()> {
    writer
        .flush()
        .map_err(|e| NcError::Write(format!("flushing {}: {e}", path.display())))
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
    use std::io::Cursor;
    use tiff::decoder::{Decoder, DecodingResult};

    fn img(width: u32, height: u32, rgb: Vec<f32>, ir: Option<Vec<f32>>) -> LinearImage {
        LinearImage::new(width, height, rgb, ir).unwrap()
    }

    fn out(depth: OutDepth, bigtiff: BigTiff) -> OutputParams {
        OutputParams {
            out_depth: depth,
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
    fn flush_error_is_surfaced_not_swallowed() {
        // A writer whose flush fails must produce an NcError::Write, not be
        // silently dropped (the BufWriter-drop-swallows-errors trap).
        struct FailFlush;
        impl Write for FailFlush {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
        }
        let mut w = FailFlush;
        let err = flush_buf(&mut w, Path::new("out.tiff")).unwrap_err();
        assert!(matches!(err, NcError::Write(msg) if msg.contains("disk full")));
    }

    #[test]
    fn sidecar_path_appends_json() {
        let dir = std::env::temp_dir();
        let output = dir.join(format!("nc_sidecar_test_{}.tiff", std::process::id()));
        let json = r#"{"algorithm":"density"}"#;
        write_sidecar(&output, json).unwrap();

        let sidecar = PathBuf::from(format!("{}.json", output.display()));
        let read = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(read, json);
        // Valid JSON.
        let _: serde_json::Value = serde_json::from_str(&read).unwrap();
        let _ = std::fs::remove_file(&sidecar);
    }
}
