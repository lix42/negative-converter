//! SilverFast HDR (48-bit RGB) / HDRi (64-bit RGB+IR) → [`LinearImage`].
//!
//! On-disk layout, reverse-engineered from real sample scans (no published spec —
//! see `docs/tasks/silverfast-decode.md`):
//!
//! - **HDR**: a single TIFF IFD — 3-sample chunky RGB, 16-bit unsigned, no IR.
//! - **HDRi**: **two** IFDs — IFD0 is the RGB image (as HDR); IFD1 is the IR plane
//!   (1-sample grayscale, 16-bit, same dimensions, `NewSubfileType=4`).
//!
//! HDR vs HDRi is detected **structurally** (the presence of the second image),
//! never from metadata — the `Silverfast:HDRScan` XMP flag is `"Yes"` on both.
//! The IR plane is preserved into [`LinearImage::ir`], never consumed in Step 1
//! (design-spec §6.1).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::Serialize;
use tiff::ColorType;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::types::{LinearImage, NcError, Result};

/// Which SilverFast variant a file turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SilverFastFormat {
    /// 48-bit RGB, no IR plane (single IFD).
    Hdr,
    /// 64-bit RGB + IR (two IFDs; IFD1 = IR).
    Hdri,
}

/// What the decoder found in the file — surfaced by `inspect` / the JSON report
/// so inspection doesn't have to re-parse the TIFF. Carries the original on-disk
/// facts (format, channels, bit depth) that are lost once samples are normalized
/// to linear `f32`, plus any non-fatal warnings.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecodeInfo {
    pub format: SilverFastFormat,
    pub width: u32,
    pub height: u32,
    /// RGB channels in the primary image (always 3 for the formats we accept).
    pub channels: u16,
    /// Bits per sample of the primary image (always 16 for accepted files).
    pub bits_per_sample: u8,
    pub ir_present: bool,
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: Option<String>,
    /// Non-fatal notes (e.g. extra IFDs beyond the IR plane that we ignored).
    pub warnings: Vec<String>,
}

/// Decode a SilverFast HDR/HDRi TIFF at `path` into a linear `f32` image,
/// returning a [`DecodeInfo`] describing what was found alongside it.
pub fn decode(path: &Path) -> Result<(LinearImage, DecodeInfo)> {
    let file = File::open(path)
        .map_err(|e| NcError::Decode(format!("cannot open {}: {e}", path.display())))?;
    let mut dec = Decoder::new(BufReader::new(file))
        .map_err(|e| decode_err(path, "not a readable TIFF", e))?;

    // --- IFD0: the RGB image -------------------------------------------------
    let (width, height) = dec
        .dimensions()
        .map_err(|e| decode_err(path, "reading image dimensions", e))?;
    let color = dec
        .colortype()
        .map_err(|e| decode_err(path, "reading color type", e))?;
    if color != ColorType::RGB(16) {
        return Err(NcError::Unsupported(format!(
            "{}: expected 3-channel 16-bit RGB in the primary image, found {color:?}; \
             only SilverFast HDR/HDRi 16-bit scans are supported",
            path.display()
        )));
    }
    // `read_image` only returns the first sample plane under PlanarConfiguration=2
    // (a known `tiff`-crate limitation); RGB has 3 samples, so reject planar to
    // avoid silently dropping G and B. SilverFast scans are always chunky (=1).
    let planar = dec
        .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
        .ok()
        .flatten()
        .unwrap_or(1);
    if planar != 1 {
        return Err(NcError::Unsupported(format!(
            "{}: PlanarConfiguration={planar} (planar) is not supported; expected chunky (1)",
            path.display()
        )));
    }
    let rgb = read_plane_u16(&mut dec, path, "RGB image")?;

    // Read IFD0 metadata *before* advancing to the IR IFD (it shifts the cursor).
    let make = dec.get_tag_ascii_string(Tag::Make).ok();
    let model = dec.get_tag_ascii_string(Tag::Model).ok();
    let software = dec.get_tag_ascii_string(Tag::Software).ok();

    let mut warnings = Vec::new();

    // --- IFD1 (if present): the IR plane ------------------------------------
    let ir = if dec.more_images() {
        dec.next_image()
            .map_err(|e| decode_err(path, "advancing to the IR image", e))?;
        let (iw, ih) = dec
            .dimensions()
            .map_err(|e| decode_err(path, "reading IR dimensions", e))?;
        if (iw, ih) != (width, height) {
            return Err(NcError::Unsupported(format!(
                "{}: IR plane is {iw}x{ih} but RGB image is {width}x{height}; \
                 mismatched dimensions",
                path.display()
            )));
        }
        let ir_color = dec
            .colortype()
            .map_err(|e| decode_err(path, "reading IR color type", e))?;
        if ir_color != ColorType::Gray(16) {
            return Err(NcError::Unsupported(format!(
                "{}: expected a 1-channel 16-bit grayscale IR plane, found {ir_color:?}",
                path.display()
            )));
        }
        // The real IR plane is marked `NewSubfileType=4`. We still accept a
        // matching-dimension 16-bit grayscale IFD without it (the layout is
        // reverse-engineered, and the IR plane is only carried, not consumed in
        // Step 1), but record a warning so an incidental second page isn't
        // reported as IR provenance with no trace.
        let subfile = dec
            .find_tag_unsigned::<u32>(Tag::NewSubfileType)
            .ok()
            .flatten();
        if subfile != Some(4) {
            warnings.push(format!(
                "second IFD has NewSubfileType={subfile:?} (expected 4 for an IR plane); \
                 identified as IR by its 16-bit grayscale shape alone"
            ));
        }
        let ir = read_plane_u16(&mut dec, path, "IR plane")?;
        // Anything past the IR plane is unexpected; carry it through as a note.
        if dec.more_images() {
            warnings.push("file has additional IFDs beyond the IR plane; ignored".into());
        }
        Some(ir)
    } else {
        None
    };

    let format = if ir.is_some() {
        SilverFastFormat::Hdri
    } else {
        SilverFastFormat::Hdr
    };
    let info = DecodeInfo {
        format,
        width,
        height,
        channels: 3,
        bits_per_sample: 16,
        ir_present: ir.is_some(),
        make,
        model,
        software,
        warnings,
    };

    // Validated constructor enforces the buffer-length invariants at the boundary.
    let image = LinearImage::new(width, height, rgb, ir)?;
    Ok((image, info))
}

/// Read the current IFD's image as 16-bit unsigned samples and normalize to
/// linear `f32` in `[0, 1]`. Fails loudly if the samples aren't 16-bit unsigned.
fn read_plane_u16(dec: &mut Decoder<BufReader<File>>, path: &Path, what: &str) -> Result<Vec<f32>> {
    match dec
        .read_image()
        .map_err(|e| decode_err(path, &format!("reading {what} pixels"), e))?
    {
        DecodingResult::U16(samples) => Ok(normalize_u16(&samples)),
        other => Err(NcError::Unsupported(format!(
            "{}: {what} is not 16-bit unsigned ({}); unsupported sample format",
            path.display(),
            decoding_result_kind(&other)
        ))),
    }
}

/// Map 16-bit unsigned samples to linear `f32` in `[0, 1]` (divide by 65535).
/// Data is treated as linear scanner values — no gamma is applied here.
fn normalize_u16(samples: &[u16]) -> Vec<f32> {
    const MAX: f32 = u16::MAX as f32;
    samples.iter().map(|&s| s as f32 / MAX).collect()
}

/// Wrap a `tiff` error as a [`NcError::Decode`] with file + operation context.
fn decode_err(path: &Path, while_doing: &str, err: tiff::TiffError) -> NcError {
    NcError::Decode(format!("{}: {while_doing}: {err}", path.display()))
}

/// Human-readable name of a `DecodingResult` variant, for error messages.
fn decoding_result_kind(r: &DecodingResult) -> &'static str {
    match r {
        DecodingResult::U8(_) => "8-bit unsigned",
        DecodingResult::U16(_) => "16-bit unsigned",
        DecodingResult::U32(_) => "32-bit unsigned",
        DecodingResult::U64(_) => "64-bit unsigned",
        DecodingResult::F16(_) => "16-bit float",
        DecodingResult::F32(_) => "32-bit float",
        DecodingResult::F64(_) => "64-bit float",
        DecodingResult::I8(_) => "8-bit signed",
        DecodingResult::I16(_) => "16-bit signed",
        DecodingResult::I32(_) => "32-bit signed",
        DecodingResult::I64(_) => "64-bit signed",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use tiff::encoder::TiffEncoder;
    use tiff::encoder::colortype::{Gray16, RGB8, RGB16};

    use super::*;

    /// A temp TIFF that deletes itself on drop, so a failing test can't leak it.
    struct TempTiff(PathBuf);
    impl Drop for TempTiff {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_path(name: &str) -> TempTiff {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempTiff(std::env::temp_dir().join(format!("nc-decode-{name}-{nanos}.tif")))
    }

    /// Path to a committed real-scan fixture (`tests/fixtures/<name>`).
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn assert_unit_range(values: &[f32]) {
        assert!(
            values.iter().all(|&v| (0.0..=1.0).contains(&v)),
            "all samples must normalize into [0, 1]"
        );
    }

    // --- synthetic-file tests (run in CI; no real assets needed) -------------

    #[test]
    fn normalize_u16_maps_full_range_to_unit_interval() {
        let out = normalize_u16(&[0, 32768, u16::MAX]);
        assert_eq!(out[0], 0.0);
        assert!((out[1] - 0.5).abs() < 1e-4);
        assert_eq!(out[2], 1.0);
    }

    #[test]
    fn decodes_single_ifd_rgb_as_hdr() {
        // 2x1 RGB, 16-bit: pixel0 = full white, pixel1 = mid/zero/full.
        let tmp = temp_path("hdr");
        let rgb: [u16; 6] = [65535, 65535, 65535, 32768, 0, 65535];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();

        let (img, info) = decode(&tmp.0).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(info.format, SilverFastFormat::Hdr);
        assert!(img.ir.is_none());
        assert!(!info.ir_present);
        assert_eq!(img.rgb.len(), 6);
        assert_eq!(img.rgb[0], 1.0);
        assert_eq!(img.rgb[4], 0.0);
        assert_unit_range(&img.rgb);
    }

    #[test]
    fn decodes_two_ifds_rgb_plus_ir_as_hdri() {
        // IFD0 = 2x1 RGB, IFD1 = 2x1 grayscale IR.
        let tmp = temp_path("hdri");
        let rgb: [u16; 6] = [1000, 2000, 3000, 4000, 5000, 6000];
        let ir: [u16; 2] = [0, 65535];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        enc.write_image::<Gray16>(2, 1, &ir).unwrap();

        let (img, info) = decode(&tmp.0).unwrap();
        assert_eq!(info.format, SilverFastFormat::Hdri);
        assert!(info.ir_present);
        let ir_plane = img.ir.expect("IR plane present");
        assert_eq!(ir_plane.len(), 2);
        assert_eq!(ir_plane[0], 0.0);
        assert_eq!(ir_plane[1], 1.0);
        assert_unit_range(&img.rgb);
    }

    #[test]
    fn rejects_non_16bit_as_unsupported() {
        // An 8-bit RGB image is not a SilverFast 16-bit scan.
        let tmp = temp_path("rgb8");
        let rgb: [u8; 6] = [255, 128, 0, 0, 128, 255];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB8>(2, 1, &rgb).unwrap();

        match decode(&tmp.0) {
            Err(NcError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn rejects_mismatched_ir_dimensions() {
        // IFD0 = 2x1 RGB, IFD1 = 1x1 grayscale — a dimension mismatch.
        let tmp = temp_path("badir");
        let rgb: [u16; 6] = [0; 6];
        let ir: [u16; 1] = [0];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        enc.write_image::<Gray16>(1, 1, &ir).unwrap();

        match decode(&tmp.0) {
            Err(NcError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_decode_error() {
        match decode(Path::new("/no/such/nc-decode-missing.tif")) {
            Err(NcError::Decode(_)) => {}
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_grayscale_ir_plane() {
        // IFD0 = 2x1 RGB16, IFD1 = 2x1 RGB16: matching dimensions but not a
        // 1-channel plane, so the second IFD can't be the IR plane.
        let tmp = temp_path("rgbir");
        let rgb: [u16; 6] = [0; 6];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();

        match decode(&tmp.0) {
            Err(NcError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn extra_ifds_beyond_ir_are_warned() {
        // IFD0 RGB, IFD1 grayscale IR, IFD2 a stray extra image — the third IFD
        // is ignored but must be surfaced as a warning, not dropped silently.
        let tmp = temp_path("extra");
        let rgb: [u16; 6] = [0; 6];
        let ir: [u16; 2] = [0, 65535];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        enc.write_image::<Gray16>(2, 1, &ir).unwrap();
        enc.write_image::<Gray16>(2, 1, &ir).unwrap();

        let (_img, info) = decode(&tmp.0).unwrap();
        assert_eq!(info.format, SilverFastFormat::Hdri);
        assert!(
            info.warnings.iter().any(|w| w.contains("additional IFDs")),
            "expected an extra-IFD warning, got {:?}",
            info.warnings
        );
    }

    #[test]
    fn reads_scanner_metadata_from_primary_ifd() {
        // Software is read from IFD0 *before* advancing to the IR plane; assert it
        // round-trips so a future reordering past `next_image()` is caught.
        let tmp = temp_path("meta");
        let rgb: [u16; 6] = [0; 6];
        let ir: [u16; 2] = [0, 0];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        let mut image = enc.new_image::<RGB16>(2, 1).unwrap();
        image
            .encoder()
            .write_tag(Tag::Software, "nc-ship-test")
            .unwrap();
        image.write_data(&rgb).unwrap();
        enc.write_image::<Gray16>(2, 1, &ir).unwrap();

        let (_img, info) = decode(&tmp.0).unwrap();
        assert_eq!(info.software.as_deref(), Some("nc-ship-test"));
    }

    // --- real-scan fixture tests (committed under tests/fixtures) -------------

    #[test]
    fn decodes_real_hdr_fixture() {
        let (img, info) = decode(&fixture("hdr-48bit.tif")).unwrap();
        assert_eq!(info.format, SilverFastFormat::Hdr);
        assert_eq!((img.width, img.height), (502, 462));
        assert!(img.ir.is_none());
        assert_eq!(img.rgb.len(), 502 * 462 * 3);
        assert_unit_range(&img.rgb);
    }

    #[test]
    fn decodes_real_hdri_fixture() {
        let (img, info) = decode(&fixture("hdri-64bit.tif")).unwrap();
        assert_eq!(info.format, SilverFastFormat::Hdri);
        assert_eq!((img.width, img.height), (502, 462));
        let ir = img.ir.expect("HDRi fixture must carry an IR plane");
        assert_eq!(ir.len(), 502 * 462);
        assert_unit_range(&img.rgb);
        assert_unit_range(&ir);
    }
}
