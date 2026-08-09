//! Gain-map JPEG packaging.
//!
//! nc renders and compresses both JPEG images itself, then uses one small
//! libultrahdr API-4 boundary to attach the legacy XMP/MPF metadata. The native
//! library is pinned in `vendor/ultrahdr-sys`.
//!
//! **libultrahdr's own ISO writing stays off, deliberately and permanently:** it
//! emits a common-denominator compact layout the normative structure has no flag
//! for (see `pipeline::gain_map::iso`). ISO segments here are serialized by nc
//! and attached by this module — the gain map's before packaging, the baseline's
//! after, because libultrahdr rewrites the baseline's segments and appends the
//! gain-map image verbatim.

use std::ffi::{CStr, c_void};
use std::path::Path;
use std::ptr::NonNull;

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use ultrahdr_sys as uhdr;

use crate::io::staged::{self, Staged};
use crate::pipeline::gain_map::iso;
use crate::pipeline::{color, gain_map};
use crate::types::{EncodeOutcome, EncodeReport, NcError, OutputStats, Result};

const JPEG_QUALITY: u8 = 95;

/// Which metadata dialects the packaged JPEG carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialects {
    /// Only the public Android/Adobe XMP + MPF form. Makes no ISO claim, and
    /// writes no ISO or draft-ISO marker.
    LegacyUltraHdrV1,
    /// The same legacy metadata plus ISO 21496-1 segments in both images,
    /// describing the one shared gain map.
    // No CLI caller yet by design: `output/presets` owns the neutral
    // `gain-map-hdr` name and its default activation, and the shipped
    // `ultra-hdr-v1` preset is contractually ISO-free (a test asserts its bytes
    // contain no "21496"). Inventing a preset name here would hand that task a
    // migration instead of a capability. Remove this allowance when presets wires
    // it up. Scoped to the variant, which is what the lint names — an allowance on
    // the enum would also hide a genuinely dead future variant.
    #[allow(dead_code)]
    LegacyPlusIso,
}

/// Encode and package one explicit legacy Ultra HDR v1 output.
pub fn encode(render: gain_map::GainMapRender, path: &Path) -> Result<(Staged, EncodeOutcome)> {
    encode_with(render, path, Dialects::LegacyUltraHdrV1)
}

/// Encode and package a gain-map JPEG carrying the selected metadata dialects.
///
/// Both dialects describe **one** gain-map image. That image is the achromatic
/// luminance map, because the legacy XMP dialect cannot signal a multichannel
/// one — so a dual-dialect file necessarily shares the legacy form. The ISO
/// fields are projected from that map's own encoded metadata, which is what
/// makes the two dialects agree by construction rather than by coincidence.
pub fn encode_with(
    render: gain_map::GainMapRender,
    path: &Path,
    dialects: Dialects,
) -> Result<(Staged, EncodeOutcome)> {
    let (images, outcome) = compress_images(render)?;
    // The ISO fields describe the very map just encoded, so both dialects
    // report the same normalization window.
    let iso = match dialects {
        Dialects::LegacyUltraHdrV1 => None,
        Dialects::LegacyPlusIso => Some(iso::project(&images.gain.metadata)?),
    };
    let staged = package_images(&images, iso.as_ref(), path)?;
    Ok((staged, outcome))
}

/// The two compressed images plus the gain map's own metadata — everything the
/// container needs, and nothing that depends on which dialects it will carry.
struct CompressedImages {
    base_jpeg: Vec<u8>,
    gain: gain_map::EncodedGainMap,
}

/// Render half of [`encode_with`]: consume the render into the two JPEGs.
fn compress_images(render: gain_map::GainMapRender) -> Result<(CompressedImages, EncodeOutcome)> {
    let gain = gain_map::encode_legacy_gain_map(&render)?;
    let (base, icc, _) = color::encode_rendered_sdr(render.into_sdr())?;
    let (base_rgb, loss, stats) = quantize_base(&base.rgb);
    let base_jpeg = encode_jpeg(
        &base_rgb,
        base.width,
        base.height,
        Some(&icc),
        "SDR base",
        ColorType::Rgb,
        None,
    )?;
    Ok((
        CompressedImages { base_jpeg, gain },
        EncodeOutcome { loss, stats },
    ))
}

/// Container half of [`encode_with`]: the **one** path that assembles a finished
/// gain-map JPEG, so nothing — not the sample writers, not the marker-order
/// tests — can assert against or ship a container that differs from the
/// product's. Anything added here (an Exif APP1, an MPEntry patch) is therefore
/// seen by every caller at once. The single exception is
/// `baseline_insertion_keeps_every_mpf_offset_resolvable`, which calls `package`
/// directly because it needs the *pre*-insertion package as its "before".
///
/// `iso` is already resolved rather than derived from [`Dialects`] here, which
/// is what lets `iso_oracle_samples` emit its deliberately conflicting file —
/// legacy metadata from `images.gain`, ISO fields from a divergent copy —
/// through this same code.
///
/// Returns the bytes; [`package_images`] is the same thing written to a path.
fn assemble(images: &CompressedImages, iso: Option<&iso::IsoGainMapFields>) -> Result<Vec<u8>> {
    let gain = &images.gain;
    // The gain map's segment goes in before packaging: libultrahdr appends this
    // image verbatim, so the segment survives. The baseline's cannot — see
    // `insert_baseline_iso_segment`.
    let gain_app2 = iso
        .map(|fields| iso::serialize_metadata(fields).map(|payload| iso::segment_content(&payload)))
        .transpose()?;
    let gain_jpeg = encode_jpeg(
        &gain.samples,
        gain.width,
        gain.height,
        None,
        "gain map",
        ColorType::Luma,
        gain_app2,
    )?;
    let mut packaged = package(&images.base_jpeg, &gain_jpeg, &gain.metadata)?;
    if let Some(fields) = iso {
        let version = iso::app2_segment(&iso::serialize_version(fields))?;
        packaged = insert_baseline_iso_segment(&packaged, &version)?;
    }
    Ok(packaged)
}

/// [`assemble`] written to `path`, staged like the TIFF path: the whole package
/// is built in memory first, so the final path only ever sees a complete Ultra
/// HDR file.
fn package_images(
    images: &CompressedImages,
    iso: Option<&iso::IsoGainMapFields>,
    path: &Path,
) -> Result<Staged> {
    staged::stage_bytes(path, &assemble(images, iso)?)
}

fn encode_jpeg(
    rgb: &[u8],
    width: u32,
    height: u32,
    icc: Option<&[u8]>,
    label: &str,
    color_type: ColorType,
    app2: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let width = u16::try_from(width).map_err(|_| {
        NcError::Unsupported(format!(
            "{label} width exceeds the JPEG limit (got {width})"
        ))
    })?;
    let height = u16::try_from(height).map_err(|_| {
        NcError::Unsupported(format!(
            "{label} height exceeds the JPEG limit (got {height})"
        ))
    })?;
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, JPEG_QUALITY);
    encoder.set_sampling_factor(SamplingFactor::R_4_4_4);
    if let Some(profile) = icc {
        encoder
            .add_icc_profile(profile)
            .map_err(|e| NcError::Write(format!("embedding {label} ICC profile: {e}")))?;
    }
    if let Some(segment) = app2 {
        encoder
            .add_app_segment(2, segment)
            .map_err(|e| NcError::Write(format!("embedding {label} APP2 segment: {e}")))?;
    }
    encoder
        .encode(rgb, width, height, color_type)
        .map_err(|e| NcError::Write(format!("encoding {label} JPEG: {e}")))?;
    Ok(bytes)
}

fn quantize_base(rgb: &[f32]) -> (Vec<u8>, EncodeReport, OutputStats) {
    let mut bytes = Vec::with_capacity(rgb.len());
    let mut loss = EncodeReport {
        total_samples: rgb.len() as u64,
        ..EncodeReport::default()
    };
    let mut sums = [0_u64; 3];
    let mut pixels = 0_u64;
    for (index, value) in rgb.iter().copied().enumerate() {
        let byte = if !value.is_finite() {
            loss.non_finite += 1;
            0
        } else if value < 0.0 {
            loss.clipped_low += 1;
            0
        } else if value > 1.0 {
            loss.clipped_high += 1;
            u8::MAX
        } else {
            (value * u8::MAX as f32).round() as u8
        };
        sums[index % 3] += u64::from(byte);
        bytes.push(byte);
        if index % 3 == 2 {
            pixels += 1;
        }
    }
    let mean = if pixels == 0 {
        [0.0; 3]
    } else {
        [
            sums[0] as f64 / pixels as f64 / u8::MAX as f64,
            sums[1] as f64 / pixels as f64 / u8::MAX as f64,
            sums[2] as f64 / pixels as f64 / u8::MAX as f64,
        ]
    };
    (bytes, loss, OutputStats { mean })
}

struct EncoderHandle(NonNull<uhdr::uhdr_codec_private_t>);

impl EncoderHandle {
    fn new() -> Result<Self> {
        // SAFETY: creation has no preconditions. A non-null result is uniquely
        // owned by this guard and released exactly once in `Drop`.
        NonNull::new(unsafe { uhdr::uhdr_create_encoder() })
            .map(Self)
            .ok_or_else(|| NcError::Other("libultrahdr failed to allocate an encoder".into()))
    }
}

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `uhdr_create_encoder`, remains live for the
        // guard's lifetime, and has not been released elsewhere.
        unsafe { uhdr::uhdr_release_encoder(self.0.as_ptr()) };
    }
}

fn package(
    base_jpeg: &[u8],
    gain_jpeg: &[u8],
    metadata: &gain_map::GainMapMetadata,
) -> Result<Vec<u8>> {
    let encoder = EncoderHandle::new()?;
    let mut base = compressed(base_jpeg, uhdr::uhdr_color_gamut_t::UHDR_CG_DISPLAY_P3);
    let mut gain = compressed(gain_jpeg, uhdr::uhdr_color_gamut_t::UHDR_CG_UNSPECIFIED);
    let mut native_metadata = uhdr::uhdr_gainmap_metadata_t {
        max_content_boost: metadata.gain_max,
        min_content_boost: metadata.gain_min,
        gamma: metadata.gain_gamma,
        offset_sdr: metadata.offset_sdr,
        offset_hdr: metadata.offset_hdr,
        hdr_capacity_min: 1.0,
        hdr_capacity_max: metadata.display_headroom_linear,
        use_base_cg: 1,
    };

    // SAFETY: both compressed descriptors point into `base_jpeg`/`gain_jpeg`,
    // which remain live and immovable through `uhdr_encode`. libultrahdr copies
    // their bytes when the descriptors are set; metadata is a live, writable C
    // descriptor for the duration of the call.
    check(
        unsafe {
            uhdr::uhdr_enc_set_compressed_image(
                encoder.0.as_ptr(),
                &mut base,
                uhdr::uhdr_img_label_t::UHDR_BASE_IMG,
            )
        },
        "setting the Ultra HDR base image",
    )?;
    check(
        unsafe {
            uhdr::uhdr_enc_set_gainmap_image(encoder.0.as_ptr(), &mut gain, &mut native_metadata)
        },
        "setting the Ultra HDR gain map",
    )?;
    check(
        unsafe { uhdr::uhdr_encode(encoder.0.as_ptr()) },
        "packaging legacy Ultra HDR v1 metadata",
    )?;

    // SAFETY: the encoder is live and `uhdr_encode` succeeded. The returned
    // descriptor and its byte storage are owned by the encoder.
    let stream = NonNull::new(unsafe { uhdr::uhdr_get_encoded_stream(encoder.0.as_ptr()) })
        .ok_or_else(|| NcError::Other("libultrahdr returned no encoded stream".into()))?;
    // SAFETY: the non-null descriptor is owned by the still-live encoder.
    let stream = unsafe { stream.as_ref() };
    if stream.data.is_null() || stream.data_sz == 0 || stream.data_sz > stream.capacity {
        return Err(NcError::Other(
            "libultrahdr returned an invalid encoded stream descriptor".into(),
        ));
    }
    // SAFETY: the descriptor was validated non-null with `data_sz <= capacity`;
    // copy it before `encoder` drops and invalidates the native storage.
    Ok(unsafe { std::slice::from_raw_parts(stream.data.cast::<u8>(), stream.data_sz) }.to_vec())
}

/// Insert the ISO 21496-1 C.4.3 `GainMapVersion` segment into the packaged
/// baseline image's leading application-segment block.
///
/// libultrahdr rewrites the baseline image's marker segments during packaging
/// and drops unknown APP2 segments, so the baseline's segment cannot be added
/// before packaging the way the gain map's can — it has to go in afterwards.
///
/// Placement is load-bearing, not cosmetic, and **two** constraints bind it.
///
/// 1. **Before `SOF0`.** A JPEG reader scans for `APPn` markers only in the
///    header block; once it reaches the frame header it stops looking. An
///    earlier version of this function inserted immediately before the MPF
///    segment, which libultrahdr emits *after* `SOF0` and the tables — so the
///    segment was well-formed, correctly sized, and simply never parsed. Apple
///    ImageIO reported no gain map at all and decoded the file as plain SDR;
///    moving the same bytes into the header block flipped it to `PRESENT`, with
///    every ISO field reading back as written. (The decoder's reported headroom
///    is *not* the evidence — it echoes nc's declared `AlternateHeadroom`, so it
///    reads 4.93 even on a flat gain map.) Verified 2026-08-06 against ImageIO
///    on macOS 26.5 — see `docs/progress/output.md`.
/// 2. **Before the `MPF\0` label.** MPF individual-image offsets are measured
///    from the byte after that label, so inserting before it moves the reference
///    point and the appended gain map by the same amount and leaves every stored
///    offset correct. Only the first image's recorded size grows, which this
///    function patches. Inserting *after* the MPF segment would invalidate every
///    offset.
///
/// The insertion point is therefore the end of the leading `APPn` run, clamped
/// to the MPF segment start in case a future libultrahdr emits MPF earlier. That
/// also keeps the JFIF `APP0` first, which
/// `dual_dialect_baseline_keeps_jfif_first_and_iso_before_mpf` pins.
fn insert_baseline_iso_segment(packaged: &[u8], segment: &[u8]) -> Result<Vec<u8>> {
    let malformed = |what: &str| NcError::Write(format!("ISO gain-map MPF repair: {what}"));

    let label_at = packaged
        .windows(4)
        .position(|window| window == b"MPF\0")
        .ok_or_else(|| malformed("the packaged baseline image has no MPF segment"))?;
    // The APP2 segment header (marker + length) precedes the label.
    let mpf_start = label_at
        .checked_sub(4)
        .ok_or_else(|| malformed("the MPF label is not preceded by an APP2 header"))?;
    if packaged[mpf_start..mpf_start + 2] != iso::APP2_MARKER {
        return Err(malformed("the MPF label is not inside an APP2 segment"));
    }
    let segment_start = leading_app_segment_end(packaged)?.min(mpf_start);

    // MPF stores a TIFF structure whose offsets are relative to its own start.
    let tiff = label_at + 4;
    let endian = packaged
        .get(tiff..tiff + 2)
        .ok_or_else(|| malformed("truncated MPF byte-order mark"))?;
    let big_endian = match endian {
        b"MM" => true,
        b"II" => false,
        _ => return Err(malformed("unrecognized MPF byte-order mark")),
    };
    let read_u16 = |at: usize| -> Result<u16> {
        let raw = packaged
            .get(at..at + 2)
            .ok_or_else(|| malformed("truncated MPF field"))?;
        let raw = [raw[0], raw[1]];
        Ok(if big_endian {
            u16::from_be_bytes(raw)
        } else {
            u16::from_le_bytes(raw)
        })
    };
    let read_u32 = |at: usize| -> Result<u32> {
        let raw = packaged
            .get(at..at + 4)
            .ok_or_else(|| malformed("truncated MPF field"))?;
        let raw = [raw[0], raw[1], raw[2], raw[3]];
        Ok(if big_endian {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        })
    };

    let ifd = tiff + read_u32(tiff + 4)? as usize;
    let entries = read_u16(ifd)?;
    // Tag 0xB002 is the MP Entry array; each entry is 16 bytes of
    // attribute/size/offset/dependencies, and the first entry is the baseline.
    let mut size_field = None;
    for entry in 0..usize::from(entries) {
        let at = ifd + 2 + entry * 12;
        if read_u16(at)? == 0xB002 {
            let count = read_u32(at + 4)? as usize;
            if count < 16 {
                return Err(malformed("the MP Entry array has no baseline entry"));
            }
            // Offset 4 within the first entry is its individual image size.
            size_field = Some(tiff + read_u32(at + 8)? as usize + 4);
            break;
        }
    }
    let size_field = size_field.ok_or_else(|| malformed("no MP Entry array (tag 0xB002)"))?;
    if size_field < segment_start {
        return Err(malformed(
            "the MP Entry array precedes the insertion point, so inserting would move it",
        ));
    }
    let recorded = read_u32(size_field)?;

    let grown = recorded
        .checked_add(u32::try_from(segment.len()).map_err(|_| malformed("segment too large"))?)
        .ok_or_else(|| malformed("baseline image size overflows its MPF field"))?;

    let mut output = Vec::with_capacity(packaged.len() + segment.len());
    output.extend_from_slice(&packaged[..segment_start]);
    output.extend_from_slice(segment);
    output.extend_from_slice(&packaged[segment_start..]);

    // The size field sits after the insertion point, so it moved with it.
    let moved = size_field + segment.len();
    let bytes = if big_endian {
        grown.to_be_bytes()
    } else {
        grown.to_le_bytes()
    };
    output
        .get_mut(moved..moved + 4)
        .ok_or_else(|| malformed("patched size field is out of range"))?
        .copy_from_slice(&bytes);

    Ok(output)
}

/// Offset just past the last `APPn` segment of the leading header block.
///
/// Walks from the `SOI`, following each segment's own length. Stops at the first
/// marker that is not `APP0..APP15` — in a libultrahdr package that is `SOF0`,
/// which is exactly the boundary a decoder stops scanning at.
fn leading_app_segment_end(packaged: &[u8]) -> Result<usize> {
    let malformed = |what: &str| NcError::Write(format!("ISO gain-map MPF repair: {what}"));

    if packaged.get(..2) != Some(&[0xFF, 0xD8][..]) {
        return Err(malformed(
            "the packaged baseline image does not start with SOI",
        ));
    }
    let mut at = 2;
    loop {
        let marker = packaged
            .get(at..at + 2)
            .ok_or_else(|| malformed("ran off the end looking for the frame header"))?;
        if marker[0] != 0xFF {
            return Err(malformed("expected a marker in the header block"));
        }
        // APP0..APP15 are 0xE0..=0xEF; anything else ends the header block.
        if !(0xE0..=0xEF).contains(&marker[1]) {
            return Ok(at);
        }
        let length = packaged
            .get(at + 2..at + 4)
            .ok_or_else(|| malformed("truncated application-segment length"))?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        if length < 2 {
            return Err(malformed(
                "application segment declares an impossible length",
            ));
        }
        at += 2 + length;
    }
}

fn compressed(bytes: &[u8], gamut: uhdr::uhdr_color_gamut_t) -> uhdr::uhdr_compressed_image_t {
    uhdr::uhdr_compressed_image_t {
        // The C API's descriptor predates const-correctness. Packaging treats
        // compressed inputs as read-only; the borrow remains live through every
        // native call that receives this pointer.
        data: bytes.as_ptr().cast::<c_void>().cast_mut(),
        data_sz: bytes.len(),
        capacity: bytes.len(),
        cg: gamut,
        ct: uhdr::uhdr_color_transfer_t::UHDR_CT_UNSPECIFIED,
        range: uhdr::uhdr_color_range_t::UHDR_CR_UNSPECIFIED,
    }
}

fn check(status: uhdr::uhdr_error_info_t, action: &str) -> Result<()> {
    if status.error_code == uhdr::uhdr_codec_err_t::UHDR_CODEC_OK {
        return Ok(());
    }
    let detail = if status.has_detail != 0 {
        // SAFETY: libultrahdr guarantees `detail` is NUL-terminated whenever
        // `has_detail` is set; the status value owns the inline array here.
        unsafe { CStr::from_ptr(status.detail.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        "no detail".into()
    };
    Err(NcError::Write(format!(
        "{action} failed in libultrahdr ({:?}): {detail}",
        status.error_code
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_quantization_counts_every_loss() {
        let (_, loss, stats) = quantize_base(&[-1.0, 0.5, f32::NAN, 2.0, 1.0, 0.0]);
        assert_eq!(loss.total_samples, 6);
        assert_eq!(loss.clipped_low, 1);
        assert_eq!(loss.clipped_high, 1);
        assert_eq!(loss.non_finite, 1);
        assert_eq!(stats.mean[0], 0.5);
    }

    /// List the APPn/SOI/SOS marker sequence of a JPEG stream, for probes.
    fn marker_sequence(bytes: &[u8]) -> Vec<String> {
        let mut markers = Vec::new();
        let mut position = 0;
        while position + 1 < bytes.len() {
            if bytes[position] != 0xFF {
                position += 1;
                continue;
            }
            let marker = bytes[position + 1];
            match marker {
                0xD8 => {
                    markers.push("SOI".to_string());
                    position += 2;
                }
                0xD9 => {
                    markers.push("EOI".to_string());
                    position += 2;
                }
                0xDA => {
                    markers.push("SOS".to_string());
                    // Entropy-coded data follows; stop structural parsing here.
                    break;
                }
                0xE0..=0xEF => {
                    // A truncated APPn header stops the walk instead of panicking;
                    // the loop guard only proves the marker's own two bytes exist.
                    let Some(raw) = bytes.get(position + 2..position + 4) else {
                        break;
                    };
                    let length = u16::from_be_bytes([raw[0], raw[1]]) as usize;
                    // Keep the label inside this segment: a short one would
                    // otherwise borrow the next segment's bytes into the name
                    // printed by a failing assertion.
                    let label_end = (position + 2 + length.max(2))
                        .min(position + 4 + 32)
                        .min(bytes.len());
                    let label = String::from_utf8_lossy(&bytes[position + 4..label_end])
                        .chars()
                        .take_while(|c| c.is_ascii_graphic())
                        .collect::<String>();
                    markers.push(format!("APP{} ({label})", marker - 0xE0));
                    // Bounded for the same reason as the frame-header arm below:
                    // a garbage length must end the walk, not skip past markers
                    // a test is about to assert on.
                    if length < 2 || position + 2 + length > bytes.len() {
                        break;
                    }
                    position += 2 + length;
                }
                // Frame headers: 0xC0..=0xCF except DHT (0xC4), JPG (0xC8) and
                // DAC (0xCC). This is the boundary a reader stops scanning for
                // APPn at, so `baseline_iso_segment_precedes_the_frame_header`
                // needs it in the sequence.
                0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                    markers.push(format!("SOF{}", marker - 0xC0));
                    let Some(raw) = bytes.get(position + 2..position + 4) else {
                        break;
                    };
                    let length = u16::from_be_bytes([raw[0], raw[1]]) as usize;
                    // A spurious `FF Cx` pair inside another segment's payload
                    // would yield a garbage length here; stop the walk rather
                    // than skipping past real markers (MPF, say) and turning a
                    // clean assertion failure into a confusing one.
                    if length < 2 || position + 2 + length > bytes.len() {
                        break;
                    }
                    position += 2 + length;
                }
                _ => position += 2,
            }
        }
        markers
    }

    /// Parse the MPF index of a packaged file into
    /// `(mpf_tiff_start, [(size, offset)])`, mirroring what a reader does.
    fn mpf_entries(bytes: &[u8]) -> (usize, Vec<(u32, u32)>) {
        let label = bytes
            .windows(4)
            .position(|window| window == b"MPF\0")
            .expect("packaged file has an MPF segment");
        let tiff = label + 4;
        let big_endian = &bytes[tiff..tiff + 2] == b"MM";
        let read16 = |at: usize| {
            let raw = [bytes[at], bytes[at + 1]];
            if big_endian {
                u16::from_be_bytes(raw)
            } else {
                u16::from_le_bytes(raw)
            }
        };
        let read32 = |at: usize| {
            let raw = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
            if big_endian {
                u32::from_be_bytes(raw)
            } else {
                u32::from_le_bytes(raw)
            }
        };
        let ifd = tiff + read32(tiff + 4) as usize;
        let mut images = Vec::new();
        for entry in 0..usize::from(read16(ifd)) {
            let at = ifd + 2 + entry * 12;
            if read16(at) == 0xB002 {
                let entries = tiff + read32(at + 8) as usize;
                for image in 0..read32(at + 4) as usize / 16 {
                    let base = entries + image * 16;
                    images.push((read32(base + 4), read32(base + 8)));
                }
            }
        }
        (tiff, images)
    }

    /// Build a small dual-dialect package through the product's own
    /// [`assemble`], without touching the filesystem.
    ///
    /// Going through `assemble` rather than re-listing its steps is what makes
    /// the marker-order tests below binding: a future container change (the Exif
    /// APP1 or MPEntry patch `output/mp-container-conformance` owns) moves these
    /// fixtures with the product, instead of leaving them green while the
    /// shipped layout drifts. Only the two tiny JPEGs are hand-built here.
    fn dual_dialect_package() -> (Vec<u8>, iso::IsoGainMapFields) {
        let metadata = probe_metadata();
        let fields = iso::project(&metadata).unwrap();
        let base_jpeg = encode_jpeg(
            &[64, 64, 64, 192, 192, 192, 64, 64, 64, 192, 192, 192],
            2,
            2,
            None,
            "test base",
            ColorType::Rgb,
            None,
        )
        .unwrap();
        let images = CompressedImages {
            base_jpeg,
            gain: gain_map::EncodedGainMap {
                width: 2,
                height: 2,
                samples: vec![0, 255, 0, 255],
                metadata,
            },
        };
        (assemble(&images, Some(&fields)).unwrap(), fields)
    }

    #[test]
    fn dual_dialect_package_carries_iso_segments_in_both_images() {
        let (bytes, fields) = dual_dialect_package();
        let label = b"urn:iso:std:iso:ts:21496:-1\0";
        let occurrences = bytes
            .windows(label.len())
            .filter(|window| *window == label)
            .count();
        // C.4.3 puts a version-only segment in the baseline; C.4.6 the full
        // structure in the gain map. Exactly two, never one.
        assert_eq!(occurrences, 2, "expected a segment in each image");

        let base_end = bytes
            .windows(2)
            .position(|window| window == [0xFF, 0xD9])
            .unwrap()
            + 2;
        let positions = bytes
            .windows(label.len())
            .enumerate()
            .filter(|(_, window)| *window == label)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(positions[0] < base_end, "baseline segment must precede EOI");
        assert!(
            positions[1] >= base_end,
            "gain-map segment follows the base"
        );

        // The baseline carries only the 4-byte GainMapVersion, the gain map the
        // full structure — so their payload lengths must differ.
        let version = iso::serialize_version(&fields);
        let full = iso::serialize_metadata(&fields).unwrap();
        assert_eq!(version.len(), 4);
        assert_eq!(
            &bytes[positions[0] + label.len()..positions[0] + label.len() + 4],
            &version[..]
        );
        assert_eq!(
            &bytes[positions[1] + label.len()..positions[1] + label.len() + full.len()],
            &full[..]
        );
    }

    #[test]
    fn baseline_insertion_keeps_every_mpf_offset_resolvable() {
        // The whole reason the segment goes in *before* the MPF label: offsets
        // are relative to it, so they must survive untouched while the recorded
        // baseline size grows.
        let metadata = probe_metadata();
        let fields = iso::project(&metadata).unwrap();
        let base = encode_jpeg(
            &[64, 64, 64, 192, 192, 192, 64, 64, 64, 192, 192, 192],
            2,
            2,
            None,
            "test base",
            ColorType::Rgb,
            None,
        )
        .unwrap();
        let gain = encode_jpeg(
            &[0, 255, 0, 255],
            2,
            2,
            None,
            "test gain",
            ColorType::Luma,
            None,
        )
        .unwrap();
        let before = package(&base, &gain, &metadata).unwrap();
        let segment = iso::app2_segment(&iso::serialize_version(&fields)).unwrap();
        let after = insert_baseline_iso_segment(&before, &segment).unwrap();

        assert_eq!(after.len(), before.len() + segment.len());
        let (_, entries_before) = mpf_entries(&before);
        let (tiff_after, entries_after) = mpf_entries(&after);

        // The baseline's recorded size grew by exactly the inserted bytes.
        assert_eq!(
            entries_after[0].0,
            entries_before[0].0 + segment.len() as u32
        );
        // Every stored offset is unchanged, and the gain map's still resolves to
        // a real SOI — the check that would fail if we had inserted after MPF.
        for (before, after) in entries_before.iter().zip(&entries_after) {
            assert_eq!(before.1, after.1, "MPF offsets must not move");
        }
        let gain_start = tiff_after + entries_after[1].1 as usize;
        assert_eq!(&after[gain_start..gain_start + 2], &[0xFF, 0xD8]);
        // The gain map's recorded size still reaches exactly the file end.
        assert_eq!(gain_start + entries_after[1].0 as usize, after.len());
    }

    #[test]
    fn dual_dialect_baseline_keeps_jfif_first_and_iso_before_mpf() {
        // Marker order is the one thing that already cost this epic a failed
        // ImageIO decode, so pin it. JFIF APP0 must stay first, and our ISO
        // segment must sit before the MPF segment — the placement that keeps
        // MPF's relative offsets valid.
        let (bytes, _) = dual_dialect_package();
        let markers = marker_sequence(&bytes);
        assert_eq!(markers[0], "SOI");
        assert!(markers[1].starts_with("APP0 (JFIF"), "{markers:?}");

        let position = |needle: &str| {
            markers
                .iter()
                .position(|marker| marker.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing from {markers:?}"))
        };
        assert!(
            position("urn:iso:std:iso:ts:21496:-1") < position("MPF"),
            "{markers:?}"
        );
        assert!(position("JFIF") < position("urn:iso"), "{markers:?}");
    }

    #[test]
    fn baseline_iso_segment_precedes_the_frame_header() {
        // The defect the 2026-08-06 ImageIO oracle caught. Placing the segment
        // "immediately before MPF" satisfied every MPF invariant, but libultrahdr
        // emits MPF *after* SOF0 — and a JPEG reader stops scanning for APPn at
        // the frame header. The segment was well-formed and simply never parsed:
        // ImageIO reported no gain map and decoded plain SDR. Ordering against
        // MPF alone cannot catch this, because both markers were on the wrong
        // side of SOF0 together; only the frame header is the real boundary.
        let (bytes, _) = dual_dialect_package();
        let markers = marker_sequence(&bytes);
        let position = |needle: &str| {
            markers
                .iter()
                .position(|marker| marker.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing from {markers:?}"))
        };
        let frame = markers
            .iter()
            .position(|marker| marker.starts_with("SOF"))
            .unwrap_or_else(|| panic!("no frame header in {markers:?}"));
        assert!(
            position("urn:iso:std:iso:ts:21496:-1") < frame,
            "the ISO segment must sit in the header block, before the frame \
             header, or no decoder will parse it: {markers:?}"
        );
    }

    #[test]
    fn baseline_carries_no_exif_colorspace_claim() {
        // C.4.4 branches on Exif ColorSpace: a value of 1 *forces* the baseline
        // to be read as sRGB, which would misidentify our Display P3 base. With
        // no Exif present, the second branch applies and the embedded ICC
        // identifies the space — which is what we rely on. If Exif is ever added
        // here (C.4.3 wants a CIPA DC-007 baseline), it must use Uncalibrated,
        // never 1. This test is the tripwire for that.
        let (bytes, _) = dual_dialect_package();
        let markers = marker_sequence(&bytes);
        assert!(
            !markers.iter().any(|marker| marker.contains("Exif")),
            "an Exif block appeared; check its ColorSpace tag is not 1: {markers:?}"
        );
    }

    #[test]
    fn conflicting_dialect_fixture_really_disagrees() {
        // The task requires a deliberately conflicting fixture to test dual-aware
        // decoder precedence. Precedence itself can only be observed with an
        // external ISO-aware decoder — libultrahdr reads the legacy dialect only,
        // and the standard says nothing about coexistence. What is provable here
        // is that the fixture is genuinely in conflict, so an external test run
        // against it is meaningful rather than vacuous.
        let legacy = probe_metadata();
        let mut divergent = legacy;
        divergent.gain_max = [8.0; 3]; // legacy says 4.0; ISO will say 8.0
        let fields = iso::project(&divergent).unwrap();

        let base_jpeg = encode_jpeg(
            &[64, 64, 64, 192, 192, 192, 64, 64, 64, 192, 192, 192],
            2,
            2,
            None,
            "test base",
            ColorType::Rgb,
            None,
        )
        .unwrap();
        // Assembled the product's way, with the *legacy* metadata in the
        // container and divergent ISO fields beside it — the same construction
        // `iso_oracle_samples` ships to the external decoder.
        let images = CompressedImages {
            base_jpeg,
            gain: gain_map::EncodedGainMap {
                width: 2,
                height: 2,
                samples: vec![0, 255, 0, 255],
                metadata: legacy,
            },
        };
        let bytes = assemble(&images, Some(&fields)).unwrap();

        // The legacy dialect reports log2(4.0) = 2.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("hdrgm:GainMapMax"), "no legacy GainMapMax");

        // The ISO payload reports log2(8.0) = 3. Read channel 0's max out of the
        // **second** label occurrence: the first is the baseline's version-only
        // C.4.3 segment, the second the gain map's full C.4.6 structure. Then
        // 4 version + 1 flags + 16 headroom, then min pair, then max pair.
        let label = b"urn:iso:std:iso:ts:21496:-1\0";
        let at = bytes
            .windows(label.len())
            .enumerate()
            .filter(|(_, window)| *window == label)
            .map(|(index, _)| index)
            .nth(1)
            .expect("full ISO structure present in the gain-map image")
            + label.len();
        let max_numerator = i32::from_be_bytes([
            bytes[at + 29],
            bytes[at + 30],
            bytes[at + 31],
            bytes[at + 32],
        ]);
        let max_denominator = u32::from_be_bytes([
            bytes[at + 33],
            bytes[at + 34],
            bytes[at + 35],
            bytes[at + 36],
        ]);
        let iso_max = f64::from(max_numerator) / f64::from(max_denominator);
        assert!((iso_max - 3.0).abs() < 1e-6, "ISO max(G) was {iso_max}");
        // And it differs from the legacy dialect's log2(4.0) = 2.
        assert!(
            (iso_max - 2.0).abs() > 0.5,
            "fixture does not actually conflict"
        );
    }

    #[test]
    fn dual_dialect_package_still_decodes_through_libultrahdr() {
        // Adding our segments must not disturb the legacy reconstruction path.
        let (bytes, _) = dual_dialect_package();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("hdrgm:Version=\"1.0\""));

        let decoder = NonNull::new(unsafe { uhdr::uhdr_create_decoder() }).unwrap();
        let mut image = compressed(&bytes, uhdr::uhdr_color_gamut_t::UHDR_CG_UNSPECIFIED);
        check(
            unsafe { uhdr::uhdr_dec_set_image(decoder.as_ptr(), &mut image) },
            "setting dual-dialect decoder image",
        )
        .unwrap();
        check(
            unsafe { uhdr::uhdr_dec_probe(decoder.as_ptr()) },
            "probing the dual-dialect package",
        )
        .unwrap();
        // SAFETY: the decoder is live and the probe succeeded.
        unsafe { uhdr::uhdr_release_decoder(decoder.as_ptr()) };
    }

    #[test]
    fn baseline_insertion_fails_loudly_without_a_usable_mpf() {
        let error = insert_baseline_iso_segment(&[0xFF, 0xD8, 0xFF, 0xD9], &[0; 8]).unwrap_err();
        assert!(error.to_string().contains("no MPF segment"), "{error}");

        // An `MPF\0` label that is not preceded by an APP2 header must not be
        // mistaken for a real segment.
        let mut fake = vec![0u8; 16];
        fake[8..12].copy_from_slice(b"MPF\0");
        let error = insert_baseline_iso_segment(&fake, &[0; 8]).unwrap_err();
        assert!(error.to_string().contains("not inside an APP2"), "{error}");
    }

    /// A self-cleaning temp directory, following `io::staged`'s test pattern
    /// (the crate deliberately has no `tempfile` dependency).
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("nc-uhdr-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build a real `GainMapRender` through the production stages, so the
    /// container tests exercise the actual SDR base, ICC, and gain map rather
    /// than hand-rolled JPEGs.
    fn real_render() -> gain_map::GainMapRender {
        use crate::algo::reconstruct;
        use crate::pipeline::render_split::display_source;
        use crate::pipeline::working_space::map_nc_film_rgb_v1;
        use crate::types::{FilmBase, LinearImage, PrintParams, Reconstruction};

        let film_rgb = [
            0.05_f32, 0.4, 2.5, 2.5, 0.4, 0.05, 0.18, 0.3, 0.5, 1.0, 1.0, 1.0,
        ];
        let scan = film_rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new(4, 1, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        let print = PrintParams::default();
        let shared = display_source(map_nc_film_rgb_v1(film), &print).unwrap();
        gain_map::render(
            &shared,
            gain_map::GainMapConfig::ultra_hdr_v1(print.highlight_compress),
        )
        .unwrap()
    }

    #[test]
    fn encode_with_writes_both_dialects_end_to_end() {
        let tmp = TempDir::new("dual");
        let iso_path = tmp.path("dual.jpg");
        let legacy_path = tmp.path("legacy.jpg");

        let (staged, _) = encode_with(real_render(), &iso_path, Dialects::LegacyPlusIso).unwrap();
        staged.commit().unwrap();
        let (staged, _) =
            encode_with(real_render(), &legacy_path, Dialects::LegacyUltraHdrV1).unwrap();
        staged.commit().unwrap();

        let dual = std::fs::read(&iso_path).unwrap();
        let legacy = std::fs::read(&legacy_path).unwrap();
        let label = b"urn:iso:std:iso:ts:21496:-1\0";
        let count = |bytes: &[u8]| {
            bytes
                .windows(label.len())
                .filter(|window| *window == label)
                .count()
        };
        // One segment per image in the dual file; none at all in the legacy one,
        // which is the shipped preset's contract.
        assert_eq!(count(&dual), 2);
        assert_eq!(count(&legacy), 0);
        assert!(!legacy.windows(5).any(|window| window == b"21496"));

        // Both still carry the legacy dialect and a Display P3 ICC.
        for bytes in [&dual, &legacy] {
            let text = String::from_utf8_lossy(bytes);
            assert!(text.contains("hdrgm:Version=\"1.0\""));
            assert!(bytes.windows(12).any(|window| window == b"ICC_PROFILE\0"));
        }
        // The ISO segments are the only difference in size.
        assert!(dual.len() > legacy.len());
    }

    /// The render behind [`iso_oracle_samples`]: the toy fixture by default, or
    /// a real scan when `NC_ISO_SAMPLE_INPUT` names one. Reads only derived
    /// numbers out — it writes JPEGs and prints statistics, never pixels.
    fn oracle_render() -> gain_map::GainMapRender {
        use crate::pipeline::stages;
        use crate::types::{FilmBase, PrintParams, Reconstruction};

        let Ok(input) = std::env::var("NC_ISO_SAMPLE_INPUT") else {
            return real_render();
        };

        let base: Vec<f32> = std::env::var("NC_ISO_SAMPLE_BASE")
            .expect("NC_ISO_SAMPLE_BASE=r,g,b is required with NC_ISO_SAMPLE_INPUT")
            .split(',')
            .map(|part| part.trim().parse().expect("film base component"))
            .collect();
        // Arity is checked here rather than by indexing, so a two-component
        // value reports what was wrong instead of panicking on `base[2]`.
        let base: [f32; 3] = base.as_slice().try_into().unwrap_or_else(|_| {
            panic!(
                "NC_ISO_SAMPLE_BASE needs exactly three components (r,g,b); got {}",
                base.len()
            )
        });
        let film_base = FilmBase::from(base);
        let dmax: f32 = std::env::var("NC_ISO_SAMPLE_DMAX")
            .expect("NC_ISO_SAMPLE_DMAX is required with NC_ISO_SAMPLE_INPUT")
            .parse()
            .expect("dmax");
        let ev: f32 = std::env::var("NC_ISO_SAMPLE_EV")
            .map(|value| value.parse().expect("ev"))
            .unwrap_or(0.0);

        // The stage-0 memory gate is deliberately bypassed (`u64::MAX`): this is
        // an opt-in sample writer run by hand on a known scan, not a conversion
        // path, and the budget would otherwise also cap the TIFF read buffers.
        let (image, _) =
            crate::io::decode::decode_within(std::path::Path::new(&input), u64::MAX).unwrap();
        let reconstruction = serde_json::from_value(serde_json::json!({
            "type": "density",
            "curve": { "type": "exponential", "dmax": { "explicit": dmax } },
        }))
        .expect("reconstruction recipe");
        let reconstruction: Reconstruction = reconstruction;
        let print = PrintParams {
            print_exposure: ev,
            ..PrintParams::default()
        };
        let source = stages::render_display_source(&image, &film_base, &reconstruction, &print)
            .expect("display source");
        println!("oracle render: {input} at {ev:+} EV, dmax {dmax}");
        gain_map::render(
            &source.shared,
            gain_map::GainMapConfig::ultra_hdr_v1(print.highlight_compress),
        )
        .unwrap()
    }

    /// Emit the full three-file set the decoder-oracle gate compares:
    /// legacy-only, dual-dialect, and a dual file whose two dialects
    /// deliberately disagree. All three are packaged from **one** render and
    /// one pair of compressed images, so any difference an external decoder
    /// reports is attributable to the metadata alone — and a real scan is
    /// decoded and rendered once, not three times.
    ///
    /// There is deliberately no CLI path to a dual-dialect file, so this is the
    /// only way to produce one; it superseded the narrower
    /// `iso_sample_for_external_decoder` on 2026-08-06. The reader that consumes
    /// these files is `scripts/iso-decoder-oracle/`.
    ///
    /// Note this calls [`compress_images`] and [`package_images`] directly, one
    /// step short of the product: [`encode_with`]'s [`Dialects`] → `iso::project`
    /// dispatch is *not* exercised by anything the oracle reads, and is covered
    /// instead by `encode_with_writes_both_dialects_end_to_end`.
    ///
    /// `NC_ISO_SAMPLE_DIR=/some/dir cargo test --bin nc -- --ignored iso_oracle_samples`
    ///
    /// Set `NC_ISO_SAMPLE_INPUT` to a real scan to render *that* instead of the
    /// toy fixture, with `NC_ISO_SAMPLE_BASE` (`r,g,b`), `NC_ISO_SAMPLE_DMAX`,
    /// and `NC_ISO_SAMPLE_EV`. The toy fixture and a default render both produce a
    /// **flat** gain map (measured `GainMapMax` 0.0039 log2 = 1.003x — under the
    /// exponential curve that was default when this was written, and re-measured at
    /// ≈1.0027x under the sigmoid default that replaced it on 2026-08-08), which
    /// cannot discriminate an HDR reconstruction — the oracle needs content driven
    /// above the SDR shoulder knee, hence the EV.
    #[test]
    #[ignore = "writes sample files for external decoder verification"]
    fn iso_oracle_samples() {
        let dir = std::env::var("NC_ISO_SAMPLE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());

        let (images, _) = compress_images(oracle_render()).unwrap();
        let agreeing = iso::project(&images.gain.metadata).unwrap();

        // The conflicting file: the same container, packaged with the true
        // legacy metadata but carrying ISO fields projected from a copy whose
        // `gain_max` is doubled — exactly one stop apart, whatever the render
        // measured, which any decoder that actually reads its chosen dialect
        // must report differently.
        let mut divergent = images.gain.metadata;
        divergent.gain_max = images.gain.metadata.gain_max.map(|max| max * 2.0);
        let conflicting = iso::project(&divergent).unwrap();

        for (name, fields) in [
            ("oracle-legacy-only.jpg", None),
            ("oracle-dual-dialect.jpg", Some(&agreeing)),
            ("oracle-conflicting.jpg", Some(&conflicting)),
        ] {
            let path = dir.join(name);
            package_images(&images, fields, &path)
                .unwrap()
                .commit()
                .unwrap();
            println!("wrote {}", path.display());
            println!("  inspect with: exiftool -a -G1 '{}'", path.display());
            println!("  and with:     sips -g all '{}'", path.display());
        }
        println!(
            "conflicting: legacy gain_max {:?} vs ISO gain_max {:?}",
            images.gain.metadata.gain_max, divergent.gain_max
        );
        println!("read back with: scripts/iso-decoder-oracle/oracle <files>");
    }

    fn probe_metadata() -> gain_map::GainMapMetadata {
        gain_map::GainMapMetadata {
            offset_sdr: [1.0 / 64.0; 3],
            offset_hdr: [1.0 / 64.0; 3],
            gain_gamma: [1.0; 3],
            gain_min: [1.0; 3],
            gain_max: [4.0; 3],
            display_headroom_linear: 1000.0 / 203.0,
            display_headroom_log2: (1000.0_f32 / 203.0).log2(),
            reference_white_nits: 203.0,
            common_primaries: "display-p3-d65",
            common_domain: "linear-relative-to-203-nit-reference-white",
            hdr_gamut_mapping: "test",
            gain_formula: "test",
        }
    }

    #[test]
    fn packaged_stream_round_trips_legacy_metadata_without_iso() {
        let base = encode_jpeg(
            &[64, 64, 64, 192, 192, 192, 64, 64, 64, 192, 192, 192],
            2,
            2,
            None,
            "test base",
            ColorType::Rgb,
            None,
        )
        .unwrap();
        let gain = encode_jpeg(
            &[0, 255, 0, 255],
            2,
            2,
            None,
            "test gain",
            ColorType::Luma,
            None,
        )
        .unwrap();
        let metadata = gain_map::GainMapMetadata {
            offset_sdr: [1.0 / 64.0; 3],
            offset_hdr: [1.0 / 64.0; 3],
            gain_gamma: [1.0; 3],
            gain_min: [1.0; 3],
            gain_max: [4.0; 3],
            display_headroom_linear: 1000.0 / 203.0,
            display_headroom_log2: (1000.0_f32 / 203.0).log2(),
            reference_white_nits: 203.0,
            common_primaries: "display-p3-d65",
            common_domain: "linear-relative-to-203-nit-reference-white",
            hdr_gamut_mapping: "test",
            gain_formula: "test",
        };
        let bytes = package(&base, &gain, &metadata).unwrap();

        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("hdrgm:Version=\"1.0\""));
        assert!(text.contains("Item:Semantic=\"GainMap\""));
        assert!(!text.contains("21496"));

        let decoder = NonNull::new(unsafe { uhdr::uhdr_create_decoder() }).unwrap();
        let mut image = compressed(&bytes, uhdr::uhdr_color_gamut_t::UHDR_CG_UNSPECIFIED);
        check(
            unsafe { uhdr::uhdr_dec_set_image(decoder.as_ptr(), &mut image) },
            "setting test decoder image",
        )
        .unwrap();
        check(
            unsafe { uhdr::uhdr_dec_probe(decoder.as_ptr()) },
            "probing test Ultra HDR",
        )
        .unwrap();
        assert_eq!(
            unsafe { uhdr::uhdr_dec_get_image_width(decoder.as_ptr()) },
            2
        );
        assert_eq!(
            unsafe { uhdr::uhdr_dec_get_gainmap_width(decoder.as_ptr()) },
            2
        );
        let decoded =
            NonNull::new(unsafe { uhdr::uhdr_dec_get_gainmap_metadata(decoder.as_ptr()) }).unwrap();
        let decoded = unsafe { decoded.as_ref() };
        assert_eq!(decoded.min_content_boost, [1.0; 3]);
        assert_eq!(decoded.max_content_boost, [4.0; 3]);
        assert_eq!(decoded.offset_sdr, [1.0 / 64.0; 3]);
        let gain_block =
            NonNull::new(unsafe { uhdr::uhdr_dec_get_gainmap_image(decoder.as_ptr()) }).unwrap();
        let gain_block = unsafe { gain_block.as_ref() };
        let gain_bytes =
            unsafe { std::slice::from_raw_parts(gain_block.data.cast::<u8>(), gain_block.data_sz) };
        assert_eq!(
            image::load_from_memory(gain_bytes)
                .unwrap()
                .color()
                .channel_count(),
            1
        );
        // Gain-map extraction advances this libultrahdr decoder to its terminal
        // state. Use an independent context for final-rendition reconstruction
        // so the test proves both public paths without relying on reset state.
        unsafe { uhdr::uhdr_release_decoder(decoder.as_ptr()) };
        let decoder = NonNull::new(unsafe { uhdr::uhdr_create_decoder() }).unwrap();
        let mut reconstruction_image =
            compressed(&bytes, uhdr::uhdr_color_gamut_t::UHDR_CG_UNSPECIFIED);
        check(
            unsafe { uhdr::uhdr_dec_set_image(decoder.as_ptr(), &mut reconstruction_image) },
            "setting reconstruction decoder image",
        )
        .unwrap();
        check(
            unsafe {
                uhdr::uhdr_dec_set_out_img_format(
                    decoder.as_ptr(),
                    uhdr::uhdr_img_fmt_t::UHDR_IMG_FMT_32bppRGBA1010102,
                )
            },
            "setting test decoder pixel format",
        )
        .unwrap();
        check(
            unsafe {
                uhdr::uhdr_dec_set_out_color_transfer(
                    decoder.as_ptr(),
                    uhdr::uhdr_color_transfer_t::UHDR_CT_PQ,
                )
            },
            "setting test decoder transfer",
        )
        .unwrap();
        check(
            unsafe { uhdr::uhdr_dec_set_out_max_display_boost(decoder.as_ptr(), 4.0) },
            "setting test decoder display boost",
        )
        .unwrap();
        check(
            unsafe { uhdr::uhdr_dec_probe(decoder.as_ptr()) },
            "probing reconstruction Ultra HDR",
        )
        .unwrap();
        check(
            unsafe { uhdr::uhdr_decode(decoder.as_ptr()) },
            "decoding test Ultra HDR",
        )
        .unwrap();
        let reconstructed =
            NonNull::new(unsafe { uhdr::uhdr_get_decoded_image(decoder.as_ptr()) }).unwrap();
        let reconstructed = unsafe { reconstructed.as_ref() };
        assert_eq!((reconstructed.w, reconstructed.h), (2, 2));
        let packed = reconstructed.planes[uhdr::UHDR_PLANE_PACKED as usize];
        assert!(!packed.is_null());
        assert_eq!(
            reconstructed.fmt,
            uhdr::uhdr_img_fmt_t::UHDR_IMG_FMT_32bppRGBA1010102
        );
        assert_eq!(reconstructed.ct, uhdr::uhdr_color_transfer_t::UHDR_CT_PQ);
        let pixels = unsafe {
            std::slice::from_raw_parts(
                packed.cast::<u32>(),
                (reconstructed.stride[0] * reconstructed.h) as usize,
            )
        };
        let channels = |pixel: u32| {
            [
                (pixel & 0x3ff) as i32,
                ((pixel >> 10) & 0x3ff) as i32,
                ((pixel >> 20) & 0x3ff) as i32,
            ]
        };
        let row_stride = reconstructed.stride[0] as usize;
        let decoded = [
            channels(pixels[0]),
            channels(pixels[1]),
            channels(pixels[row_stride]),
            channels(pixels[row_stride + 1]),
        ];
        for pixel in decoded {
            let neutral_error = pixel.iter().max().unwrap() - pixel.iter().min().unwrap();
            assert!(
                neutral_error <= 4,
                "grayscale reconstruction diverged by {neutral_error} codes: {pixel:?}"
            );
        }
        assert!(
            decoded[1][0] > decoded[0][0] + 100,
            "brighter base plus gain must reconstruct brighter: {decoded:?}"
        );
        for channel in 0..3 {
            assert!(
                (decoded[0][channel] - decoded[2][channel]).abs() <= 4,
                "repeated dark rows differ too much: {decoded:?}"
            );
            assert!(
                (decoded[1][channel] - decoded[3][channel]).abs() <= 4,
                "repeated bright rows differ too much: {decoded:?}"
            );
        }
        unsafe { uhdr::uhdr_release_decoder(decoder.as_ptr()) };
    }
}
