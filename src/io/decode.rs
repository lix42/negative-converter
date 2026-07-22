//! SilverFast HDR (48-bit RGB) / HDRi (64-bit RGB+IR) → [`LinearImage`].
//!
//! On-disk layout, reverse-engineered from real sample scans (no published spec —
//! see `docs/tasks/silverfast-decode.md`):
//!
//! - **HDR**: a single TIFF IFD — 3-sample chunky RGB, 16-bit unsigned, no IR.
//! - **HDRi**: the RGB image in IFD0 (as HDR) plus a full-resolution IR plane
//!   (1-sample grayscale, 16-bit, same dimensions, `NewSubfileType=4`) in a later
//!   IFD. High-resolution scans also embed a reduced-resolution RGB **preview**
//!   (`NewSubfileType` bit 0) between the two, so the IR plane is not always the
//!   second IFD; the decoder skips previews and scans for the IR plane by shape.
//!
//! HDR vs HDRi is detected **structurally** (the presence of an IR plane),
//! never from metadata — the `Silverfast:HDRScan` XMP flag is `"Yes"` on both.
//! The IR plane is preserved into [`LinearImage::ir`], never consumed in Step 1
//! (design-spec §6.1).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde::Serialize;
use tiff::ColorType;
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

use crate::types::{GammaFact, LinearImage, NcError, Result};

/// Which SilverFast variant a file turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SilverFastFormat {
    /// 48-bit RGB, no IR plane (single IFD).
    Hdr,
    /// 64-bit RGB + IR (two IFDs; IFD1 = IR).
    Hdri,
}

/// Namespace URI of the `Silverfast:` RDF attributes in the XMP packet — the
/// authoritative source of SilverFast mode metadata (verified against the real
/// samples, 2026-07). The XML declares `xmlns:Silverfast="LSI/"`.
const SILVERFAST_XMP_NS: &str = "LSI/";

/// SilverFast mode metadata parsed from the XMP packet (TIFF tag 700). The
/// authoritative, mode-specific provenance the input semantic resolver keys on —
/// not the (spoofable, export-surviving) `Software` string or IR-plane presence.
/// Every genuine SilverFast scan carries these `Silverfast:` RDF attributes;
/// negatives have `Negative=Yes`, positive-mode scans `Negative=No`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SilverfastXmp {
    /// `Silverfast:Company` — `"LaserSoft Imaging"` on genuine SilverFast output.
    pub company: Option<String>,
    /// `Silverfast:HDRScan` — `Yes` on a raw HDR/HDRi scan (the raw-mode marker).
    pub hdr_scan: Option<bool>,
    /// `Silverfast:Gamma` — the scan's transfer gamma ([`GammaFact`]; `1` = linear
    /// on raw scans). A present-but-uninterpretable value is `Malformed`, not
    /// absent, so a malformed non-linear gamma can't silently resolve to linear.
    pub gamma: GammaFact,
    /// `Silverfast:Negative` — `Yes` for a negative scan, `No` for positive mode.
    pub negative: Option<bool>,
}

impl SilverfastXmp {
    /// Whether this is genuine SilverFast raw-mode provenance: company is
    /// LaserSoft Imaging AND the scan is flagged `HDRScan=Yes`. This — not the
    /// `Software` string or an IR plane — is what makes an input scanner-device.
    fn is_raw_mode(&self) -> bool {
        self.company.as_deref() == Some("LaserSoft Imaging") && self.hdr_scan == Some(true)
    }
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
    /// SilverFast mode metadata parsed from the XMP packet (tag 700), when
    /// present. The authoritative provenance the resolver keys on; `None` for a
    /// file with no (or unparsable) SilverFast XMP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silverfast_xmp: Option<SilverfastXmp>,
    /// Embedded ICC profile bytes (TIFF tag 34675 `InterColorProfile`), when the
    /// primary IFD carries one. Retained verbatim for the input-semantic resolver
    /// (device-characterization metadata; reported as a safe summary, never
    /// applied before density). **Not serialized** — the report emits a summary
    /// via `pipeline::input_semantics`, not the raw bytes.
    #[serde(skip)]
    pub embedded_icc: Option<Vec<u8>>,
    /// Non-fatal notes (e.g. extra IFDs beyond the IR plane that we ignored).
    pub warnings: Vec<String>,
}

impl DecodeInfo {
    /// Whether the file carries genuine SilverFast HDR/HDRi raw-mode provenance —
    /// the evidence the input semantic resolver keys `raw_mode` on.
    ///
    /// `decode` accepts *any* 3-channel 16-bit chunky RGB TIFF, so this must be a
    /// real provenance check. It is keyed on the SilverFast **XMP mode metadata**
    /// (`Company=LaserSoft Imaging` + `HDRScan=Yes`), not the `Software` string (a
    /// processed export keeps it) or IR-plane presence (a generic RGB16 + Gray16
    /// multipage forges it) — both of which the adversarial review showed
    /// misclassify. A file without that XMP resolves to `Unknown` meaning and is
    /// rejected by `convert` unless the user explicitly asserts the axes.
    pub fn is_silverfast_raw_mode(&self) -> bool {
        self.silverfast_xmp
            .as_ref()
            .is_some_and(SilverfastXmp::is_raw_mode)
    }

    /// Whether the scan is a SilverFast **positive-mode** scan (`Negative=No`).
    /// Such a scan is still raw linear — it passes the transfer/meaning gate — but
    /// converting it as a negative is silently wrong, so `convert` rejects it (a
    /// separate, clearly-scoped check; positive-mode support is a follow-up).
    pub fn is_silverfast_positive_mode(&self) -> bool {
        self.silverfast_xmp.as_ref().and_then(|x| x.negative) == Some(false)
    }
}

/// Parse the SilverFast `Silverfast:` RDF attributes out of an XMP packet.
///
/// The packet is a standard XMP/RDF document (`<rdf:Description … Silverfast:*=…>`);
/// we read the mode attributes off whichever element carries the `Silverfast`
/// namespace. Read-only, deterministic (`roxmltree`). Returns `None` when the XML
/// doesn't parse or carries no `Silverfast` namespace — callers treat that as "no
/// SilverFast provenance", which the resolver rejects.
fn parse_silverfast_xmp(xml: &str) -> Option<SilverfastXmp> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    // The `Silverfast:` attributes all sit on one `rdf:Description` element; find
    // it by any of its namespaced attributes rather than assuming the RDF layout.
    let node = doc.descendants().find(|n| {
        n.attribute((SILVERFAST_XMP_NS, "Company")).is_some()
            || n.attribute((SILVERFAST_XMP_NS, "HDRScan")).is_some()
    })?;
    // Yes/No flag: `Some(true)`/`Some(false)` for an explicit yes/no, `None` for a
    // missing or *unrecognized* value — an unrecognized value must NOT masquerade
    // as an explicit "No" (that would fail a genuine negative scan as positive-mode
    // or a raw scan as non-HDR).
    let yes_no = |name| match node.attribute((SILVERFAST_XMP_NS, name)) {
        Some(v) if v.trim().eq_ignore_ascii_case("yes") => Some(true),
        Some(v) if v.trim().eq_ignore_ascii_case("no") => Some(false),
        _ => None,
    };
    // Gamma: distinguish absent from present-but-uninterpretable (a locale-formatted
    // "2,2" must be `Malformed`, never dropped to "linear").
    let gamma = match node.attribute((SILVERFAST_XMP_NS, "Gamma")) {
        None => GammaFact::Absent,
        Some(v) => match v.trim().parse::<f64>() {
            Ok(g) => GammaFact::Value(g),
            Err(_) => GammaFact::Malformed(v.trim().to_owned()),
        },
    };
    Some(SilverfastXmp {
        company: node
            .attribute((SILVERFAST_XMP_NS, "Company"))
            .map(str::to_owned),
        hdr_scan: yes_no("HDRScan"),
        gamma,
        negative: yes_no("Negative"),
    })
}

/// Decode a SilverFast HDR/HDRi TIFF at `path` into a linear `f32` image,
/// returning a [`DecodeInfo`] describing what was found alongside it.
pub fn decode(path: &Path) -> Result<(LinearImage, DecodeInfo)> {
    let file = File::open(path)
        .map_err(|e| NcError::Decode(format!("cannot open {}: {e}", path.display())))?;
    let mut dec = Decoder::new(BufReader::new(file))
        .map_err(|e| tiff_err(path, "not a readable TIFF", e))?
        .with_limits(decode_limits());

    // --- IFD0: the RGB image -------------------------------------------------
    let (width, height) = dec
        .dimensions()
        .map_err(|e| tiff_err(path, "reading image dimensions", e))?;
    let color = dec
        .colortype()
        .map_err(|e| tiff_err(path, "reading color type", e))?;
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
    // Absent PlanarConfiguration ⇒ chunky (=1) per the TIFF spec; but a *read
    // error* on the tag is corruption, not absence — surface it as Decode rather
    // than defaulting to chunky and risking a silently channel-dropped image.
    let planar = dec
        .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
        .map_err(|e| tiff_err(path, "reading PlanarConfiguration", e))?
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

    // Embedded ICC profile (tag 34675), if present — retained for the input
    // semantic resolver (reported as a summary, never applied before density).
    // ICC extraction is inspection-only, so a malformed tag is a non-fatal
    // *warning*, not a decode error — but it must not be swallowed silently
    // (fail-loud): distinguish tag ABSENCE (`Ok(None)` → no note) from a genuine
    // tag READ ERROR or a non-byte value type (surfaced as a warning), so an
    // unreadable profile isn't reported as "no profile".
    let embedded_icc = match dec.find_tag(Tag::IccProfile) {
        Ok(None) => None,
        Ok(Some(value)) => match value.into_u8_vec() {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            Ok(_) => None, // present but empty — treat as no profile
            Err(e) => {
                warnings.push(format!(
                    "embedded ICC profile tag (34675) is present but not a byte array \
                     ({e}); ignored for inspection"
                ));
                None
            }
        },
        Err(e) => {
            warnings.push(format!(
                "embedded ICC profile tag (34675) could not be read ({e}); ignored for \
                 inspection"
            ));
            None
        }
    };

    // SilverFast XMP mode metadata (tag 700) — the authoritative provenance the
    // input semantic resolver keys on. Same loud-vs-silent contract as the ICC
    // tag: absence is silent (a non-SilverFast file simply has no XMP), but a
    // genuine read error / non-UTF-8 packet is a non-fatal warning, not swallowed.
    let silverfast_xmp = match dec.find_tag(Tag::Unknown(700)) {
        Ok(None) => None,
        Ok(Some(value)) => match value.into_u8_vec() {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(xml) => {
                    let parsed = parse_silverfast_xmp(&xml);
                    // A present, non-empty packet that yields no recognizable
                    // SilverFast metadata (malformed XML, or a namespace/layout that
                    // isn't the reverse-engineered `LSI/` shape — e.g. a future
                    // scanner) must leave a breadcrumb, not silently lose provenance.
                    if parsed.is_none() && !xml.trim().is_empty() {
                        warnings.push(
                            "XMP packet (tag 700) present but no recognizable SilverFast \
                             metadata (namespace/layout unrecognized); provenance not \
                             established"
                                .into(),
                        );
                    }
                    // A present-but-uninterpretable gamma is ambiguous (see F1): warn
                    // naming the value so the loud transfer=Unknown has a breadcrumb.
                    if let Some(SilverfastXmp {
                        gamma: GammaFact::Malformed(raw),
                        ..
                    }) = &parsed
                    {
                        warnings.push(format!(
                            "Silverfast:Gamma value {raw:?} is not an interpretable number; \
                             the transfer is treated as ambiguous (not linear)"
                        ));
                    }
                    parsed
                }
                Err(_) => {
                    warnings.push(
                        "XMP packet (tag 700) is not valid UTF-8; SilverFast metadata \
                         ignored"
                            .into(),
                    );
                    None
                }
            },
            Err(e) => {
                warnings.push(format!(
                    "XMP packet (tag 700) is present but not a byte array ({e}); \
                     SilverFast metadata ignored"
                ));
                None
            }
        },
        Err(e) => {
            warnings.push(format!(
                "XMP packet (tag 700) could not be read ({e}); SilverFast metadata ignored"
            ));
            None
        }
    };

    // --- Remaining IFDs: locate the IR plane, skipping preview thumbnails ----
    // High-resolution HDRi scans place a reduced-resolution RGB preview
    // (`NewSubfileType` bit 0) between the RGB image and the full-res IR plane
    // (`NewSubfileType=4`, 16-bit grayscale), so the IR plane is not always the
    // second IFD. Scan every remaining page: skip previews, validate the first
    // non-preview page as the IR plane (strictly, as before), and note extras.
    let mut ir = None;
    let mut warned_extra = false;
    while dec.more_images() {
        dec.next_image()
            .map_err(|e| tiff_err(path, "advancing to the next IFD", e))?;

        let subfile = dec
            .find_tag_unsigned::<u32>(Tag::NewSubfileType)
            .ok()
            .flatten();
        let (iw, ih) = dec
            .dimensions()
            .map_err(|e| tiff_err(path, "reading IFD dimensions", e))?;

        // `NewSubfileType` bit 0 marks a reduced-resolution preview. These are
        // normal in high-res scans — skip them (without reading their strips)
        // rather than mistaking one for the IR. Gate on the reduced *dimensions*
        // too, not the bit alone: the IR plane could carry a stray bit 0 (e.g.
        // `5` = reduced|transparency-mask), and a full-resolution page must still
        // reach IR validation instead of being silently dropped.
        let is_preview = subfile.is_some_and(|s| s & 0x1 != 0) && (iw, ih) != (width, height);
        if is_preview {
            continue;
        }

        // A non-preview page after the IR plane is already accounted for is
        // unexpected; carry it through as a note (matches the prior contract).
        // Warn once, however many extra IFDs there are, to avoid report spam.
        if ir.is_some() {
            if !warned_extra {
                warnings.push("file has additional IFDs beyond the IR plane; ignored".into());
                warned_extra = true;
            }
            continue;
        }

        if (iw, ih) != (width, height) {
            return Err(NcError::Unsupported(format!(
                "{}: IR plane is {iw}x{ih} but RGB image is {width}x{height}; \
                 mismatched dimensions",
                path.display()
            )));
        }
        let ir_color = dec
            .colortype()
            .map_err(|e| tiff_err(path, "reading IR color type", e))?;
        if ir_color != ColorType::Gray(16) {
            return Err(NcError::Unsupported(format!(
                "{}: expected a 1-channel 16-bit grayscale IR plane, found {ir_color:?}",
                path.display()
            )));
        }
        // `colortype()` reports both BlackIsZero and WhiteIsZero as Gray(16), and
        // the crate *inverts* WhiteIsZero samples while decoding — so a WhiteIsZero
        // page would be silently kept as IR with transformed values. The verified
        // IR layout is BlackIsZero (PhotometricInterpretation=1); reject anything
        // else rather than preserve a possibly-inverted plane.
        let ir_photometric = dec
            .find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)
            .ok()
            .flatten();
        if ir_photometric != Some(1) {
            return Err(NcError::Unsupported(format!(
                "{}: IR plane PhotometricInterpretation={ir_photometric:?} (expected 1 = BlackIsZero)",
                path.display()
            )));
        }
        // The real IR plane is marked `NewSubfileType=4`. We still accept a
        // matching-dimension 16-bit grayscale IFD without it (the layout is
        // reverse-engineered, and the IR plane is only carried, not consumed in
        // Step 1), but record a warning so an incidental page isn't reported as IR
        // provenance with no trace.
        if subfile != Some(4) {
            warnings.push(format!(
                "IR plane has NewSubfileType={subfile:?} (expected 4); \
                 identified as IR by its full-res 16-bit grayscale shape alone"
            ));
        }
        ir = Some(read_plane_u16(&mut dec, path, "IR plane")?);
    }

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
        silverfast_xmp,
        embedded_icc,
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
        .map_err(|e| tiff_err(path, &format!("reading {what} pixels"), e))?
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

/// Decode limits raised above the `tiff` crate's 256 MiB default so full-size
/// archival scans (a single uncompressed RGB16 IFD can exceed 256 MiB) decode in
/// one read. Capped at the classic-TIFF ceiling rather than unlimited so a corrupt
/// oversized header still trips the limit and fails loudly instead of OOMing.
fn decode_limits() -> Limits {
    // SilverFast HDR/HDRi are classic TIFFs (< 4 GiB); size both the whole-image
    // and per-segment buffers to that ceiling.
    const MAX_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4 GiB
    let mut limits = Limits::default();
    limits.decoding_buffer_size = MAX_BYTES;
    limits.intermediate_buffer_size = MAX_BYTES;
    limits
}

/// Wrap a `tiff` error with file + operation context, mapping a
/// readable-but-unsupported layout (`TiffError::UnsupportedError`) to
/// [`NcError::Unsupported`] (exit 4) and IO/parse/corruption to
/// [`NcError::Decode`] (exit 3), per the documented exit-code contract (§11).
fn tiff_err(path: &Path, while_doing: &str, err: tiff::TiffError) -> NcError {
    let msg = format!("{}: {while_doing}: {err}", path.display());
    match err {
        tiff::TiffError::UnsupportedError(_) => NcError::Unsupported(msg),
        _ => NcError::Decode(msg),
    }
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
        // The `tiff` encoder writes no NewSubfileType, so the page is accepted as
        // IR by shape alone — that must be surfaced as a warning, not silent.
        assert!(
            info.warnings.iter().any(|w| w.contains("NewSubfileType")),
            "expected an accepted-by-shape warning, got {:?}",
            info.warnings
        );
    }

    #[test]
    fn preview_without_ir_decodes_as_hdr() {
        // IFD0 RGB + a reduced-resolution RGB preview (NewSubfileType=1), but no IR
        // plane: the preview is skipped and the file classifies as HDR, ir absent.
        let tmp = temp_path("preview-noir");
        let rgb: [u16; 6] = [1000, 2000, 3000, 4000, 5000, 6000];
        let thumb: [u16; 3] = [1000, 2000, 3000];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        let mut preview = enc.new_image::<RGB16>(1, 1).unwrap();
        preview
            .encoder()
            .write_tag(Tag::NewSubfileType, 1u32)
            .unwrap();
        preview.write_data(&thumb).unwrap();

        let (img, info) = decode(&tmp.0).unwrap();
        assert_eq!(info.format, SilverFastFormat::Hdr);
        assert!(!info.ir_present);
        assert!(img.ir.is_none());
        assert!(
            info.warnings.is_empty(),
            "a skipped preview should not warn, got {:?}",
            info.warnings
        );
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
    fn rejects_white_is_zero_ir_plane() {
        // IFD1 is Gray16 but WhiteIsZero (Photometric=0): the tiff crate reports it
        // as Gray(16) and would invert it on read, so it must be rejected rather
        // than preserved as an inverted IR plane.
        let tmp = temp_path("wiz");
        let rgb: [u16; 6] = [0; 6];
        let ir: [u16; 2] = [0, 65535];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        let mut ir_img = enc.new_image::<Gray16>(2, 1).unwrap();
        // Override the BlackIsZero default the Gray16 colortype would write.
        ir_img
            .encoder()
            .write_tag(Tag::PhotometricInterpretation, 0u16)
            .unwrap();
        ir_img.write_data(&ir).unwrap();

        match decode(&tmp.0) {
            Err(NcError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn skips_reduced_resolution_preview_before_ir() {
        // Real high-res HDRi layout: IFD0 RGB, IFD1 a reduced-resolution RGB
        // preview (NewSubfileType=1), IFD2 the full-res Gray16 IR (NewSubfileType
        // =4). The preview must be skipped and the IR read from the third IFD.
        let tmp = temp_path("preview");
        let rgb: [u16; 6] = [1000, 2000, 3000, 4000, 5000, 6000];
        let thumb: [u16; 3] = [1000, 2000, 3000];
        let ir: [u16; 2] = [0, 65535];
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        // Reduced-resolution preview (1x1 RGB) tagged NewSubfileType=1.
        let mut preview = enc.new_image::<RGB16>(1, 1).unwrap();
        preview
            .encoder()
            .write_tag(Tag::NewSubfileType, 1u32)
            .unwrap();
        preview.write_data(&thumb).unwrap();
        // Full-res IR (2x1 Gray16) tagged NewSubfileType=4 (transparency mask).
        let mut ir_img = enc.new_image::<Gray16>(2, 1).unwrap();
        ir_img
            .encoder()
            .write_tag(Tag::NewSubfileType, 4u32)
            .unwrap();
        ir_img.write_data(&ir).unwrap();

        let (img, info) = decode(&tmp.0).unwrap();
        assert_eq!(info.format, SilverFastFormat::Hdri);
        assert!(info.ir_present);
        let ir_plane = img.ir.expect("IR plane present");
        assert_eq!(ir_plane.len(), 2);
        assert_eq!(ir_plane[0], 0.0);
        assert_eq!(ir_plane[1], 1.0);
        // The preview is expected, not an anomaly — no warnings for this layout.
        assert!(
            info.warnings.is_empty(),
            "expected no warnings, got {:?}",
            info.warnings
        );
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

    #[test]
    fn extracts_embedded_icc_profile_bytes() {
        // An ICC profile in tag 34675 on IFD0 must be extracted verbatim into
        // `embedded_icc` (retained for the input semantic resolver); a file
        // without the tag leaves it `None`.
        let tmp = temp_path("icc");
        let rgb: [u16; 6] = [0; 6];
        let icc: Vec<u8> = (0..128u16).map(|b| b as u8).collect();
        let mut enc = TiffEncoder::new(std::fs::File::create(&tmp.0).unwrap()).unwrap();
        let mut image = enc.new_image::<RGB16>(2, 1).unwrap();
        image
            .encoder()
            .write_tag(Tag::IccProfile, &icc[..])
            .unwrap();
        image.write_data(&rgb).unwrap();

        let (_img, info) = decode(&tmp.0).unwrap();
        assert_eq!(info.embedded_icc.as_deref(), Some(&icc[..]));

        let noicc = temp_path("noicc");
        let mut enc = TiffEncoder::new(std::fs::File::create(&noicc.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        let (_img, info) = decode(&noicc.0).unwrap();
        assert!(info.embedded_icc.is_none());
    }

    /// Minimal synthetic SilverFast XMP packet (the real one is ~150 KB; only the
    /// `Silverfast:` mode attributes matter). `attrs` is the attribute list on the
    /// `rdf:Description` element.
    fn silverfast_xmp_packet(attrs: &str) -> String {
        format!(
            "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"\" xmlns:Silverfast=\"LSI/\" {attrs}/>\
             </rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>"
        )
    }

    /// Write a 2x1 RGB16 TIFF carrying the given XMP packet in tag 700.
    fn write_rgb16_with_xmp(path: &Path, xmp: &str) {
        let rgb: [u16; 6] = [0; 6];
        let mut enc = TiffEncoder::new(std::fs::File::create(path).unwrap()).unwrap();
        let mut image = enc.new_image::<RGB16>(2, 1).unwrap();
        image
            .encoder()
            .write_tag(Tag::Unknown(700), xmp.as_bytes())
            .unwrap();
        image.write_data(&rgb).unwrap();
    }

    #[test]
    fn parse_silverfast_xmp_extracts_mode_fields() {
        let neg = silverfast_xmp_packet(
            r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="Yes""#,
        );
        let x = parse_silverfast_xmp(&neg).expect("negative XMP parses");
        assert_eq!(x.company.as_deref(), Some("LaserSoft Imaging"));
        assert_eq!(x.hdr_scan, Some(true));
        assert_eq!(x.gamma, GammaFact::Value(1.0));
        assert_eq!(x.negative, Some(true));
        assert!(x.is_raw_mode());

        // Positive mode: same raw-mode markers, Negative=No.
        let pos = silverfast_xmp_packet(
            r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="No""#,
        );
        let x = parse_silverfast_xmp(&pos).unwrap();
        assert_eq!(x.negative, Some(false));
        assert!(x.is_raw_mode());

        // A non-linear gamma is surfaced (drives the contradiction path upstream).
        let g = silverfast_xmp_packet(
            r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="2.2""#,
        );
        assert_eq!(
            parse_silverfast_xmp(&g).unwrap().gamma,
            GammaFact::Value(2.2)
        );

        // A present-but-uninterpretable gamma (locale comma) is Malformed, NOT
        // absent — so it can't silently resolve to linear (F1).
        let bad = silverfast_xmp_packet(
            r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="2,2""#,
        );
        assert_eq!(
            parse_silverfast_xmp(&bad).unwrap().gamma,
            GammaFact::Malformed("2,2".into())
        );

        // An unrecognized yes/no value is `None`, not a masquerading explicit "No"
        // (F3): a `Negative="y"` must not read as positive-mode.
        let weird = silverfast_xmp_packet(
            r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Negative="y""#,
        );
        assert_eq!(parse_silverfast_xmp(&weird).unwrap().negative, None);

        // No `Silverfast` namespace, and malformed XML, both yield None.
        assert!(parse_silverfast_xmp("<x><y/></x>").is_none());
        assert!(parse_silverfast_xmp("<not valid xml").is_none());
    }

    #[test]
    fn malformed_gamma_warns_and_is_not_absent() {
        // F1 end of decode: a raw scan with a locale-formatted gamma keeps the
        // Malformed fact AND pushes a decode warning naming the value.
        let tmp = temp_path("badgamma");
        write_rgb16_with_xmp(
            &tmp.0,
            &silverfast_xmp_packet(
                r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="2,2" Silverfast:Negative="Yes""#,
            ),
        );
        let (_img, info) = decode(&tmp.0).unwrap();
        assert_eq!(
            info.silverfast_xmp.unwrap().gamma,
            GammaFact::Malformed("2,2".into())
        );
        assert!(
            info.warnings
                .iter()
                .any(|w| w.contains("2,2") && w.contains("ambiguous")),
            "expected a malformed-gamma warning, got {:?}",
            info.warnings
        );
    }

    #[test]
    fn unrecognized_xmp_warns_and_yields_no_metadata() {
        // F2: a present, valid-UTF-8 XMP packet with no recognizable SilverFast
        // namespace leaves a breadcrumb rather than silently losing provenance.
        let tmp = temp_path("foreignxmp");
        write_rgb16_with_xmp(
            &tmp.0,
            r#"<?xpacket begin=""?><x:xmpmeta xmlns:x="adobe:ns:meta/"><other>data</other></x:xmpmeta>"#,
        );
        let (_img, info) = decode(&tmp.0).unwrap();
        assert!(info.silverfast_xmp.is_none());
        assert!(
            info.warnings
                .iter()
                .any(|w| w.contains("no recognizable SilverFast metadata")),
            "expected an unrecognized-XMP warning, got {:?}",
            info.warnings
        );
    }

    #[test]
    fn raw_mode_provenance_comes_from_xmp_not_software_or_ir() {
        // A bare RGB16 TIFF (no XMP) is NOT raw mode.
        let generic = temp_path("generic");
        let rgb: [u16; 6] = [0; 6];
        let mut enc = TiffEncoder::new(std::fs::File::create(&generic.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        let (_img, info) = decode(&generic.0).unwrap();
        assert!(info.silverfast_xmp.is_none());
        assert!(!info.is_silverfast_raw_mode());

        // A `Software="SilverFast …"` tag but NO XMP is NOT sufficient (the old
        // substring heuristic misclassified processed exports that keep it).
        let sw = temp_path("sfsoftware");
        let mut enc = TiffEncoder::new(std::fs::File::create(&sw.0).unwrap()).unwrap();
        let mut image = enc.new_image::<RGB16>(2, 1).unwrap();
        image
            .encoder()
            .write_tag(Tag::Software, "SilverFast 9.2.8 (Dec 29 2025)")
            .unwrap();
        image.write_data(&rgb).unwrap();
        let (_img, info) = decode(&sw.0).unwrap();
        assert!(!info.is_silverfast_raw_mode());

        // A validated IR plane but NO XMP is NOT sufficient either (a generic
        // RGB16 + Gray16 multipage would otherwise forge provenance).
        let ir: [u16; 2] = [0, 65535];
        let hdri = temp_path("irnoxmp");
        let mut enc = TiffEncoder::new(std::fs::File::create(&hdri.0).unwrap()).unwrap();
        enc.write_image::<RGB16>(2, 1, &rgb).unwrap();
        enc.write_image::<Gray16>(2, 1, &ir).unwrap();
        let (_img, info) = decode(&hdri.0).unwrap();
        assert!(info.ir_present && !info.is_silverfast_raw_mode());

        // A SilverFast negative XMP IS raw mode, and not positive-mode.
        let neg = temp_path("xmpneg");
        write_rgb16_with_xmp(
            &neg.0,
            &silverfast_xmp_packet(
                r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="Yes""#,
            ),
        );
        let (_img, info) = decode(&neg.0).unwrap();
        assert!(info.is_silverfast_raw_mode());
        assert!(!info.is_silverfast_positive_mode());

        // A SilverFast positive XMP is raw mode but flagged positive.
        let pos = temp_path("xmppos");
        write_rgb16_with_xmp(
            &pos.0,
            &silverfast_xmp_packet(
                r#"Silverfast:Company="LaserSoft Imaging" Silverfast:HDRScan="Yes" Silverfast:Gamma="1" Silverfast:Negative="No""#,
            ),
        );
        let (_img, info) = decode(&pos.0).unwrap();
        assert!(info.is_silverfast_raw_mode());
        assert!(info.is_silverfast_positive_mode());
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
