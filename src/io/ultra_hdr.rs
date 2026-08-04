//! Legacy Ultra HDR v1 JPEG packaging.
//!
//! nc renders and compresses both JPEG images itself, then uses one small
//! libultrahdr API-4 boundary to attach the legacy XMP/MPF metadata. The native
//! library is pinned in `vendor/ultrahdr-sys`; ISO writing is deliberately off.

use std::ffi::{CStr, c_void};
use std::path::Path;
use std::ptr::NonNull;

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use ultrahdr_sys as uhdr;

use crate::io::staged::{self, Staged};
use crate::pipeline::{color, gain_map};
use crate::types::{EncodeOutcome, EncodeReport, NcError, OutputStats, Result};

const JPEG_QUALITY: u8 = 95;

/// Encode and package one explicit legacy Ultra HDR v1 output.
pub fn encode(render: gain_map::GainMapRender, path: &Path) -> Result<(Staged, EncodeOutcome)> {
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
    )?;
    let gain_jpeg = encode_jpeg(
        &gain.samples,
        gain.width,
        gain.height,
        None,
        "gain map",
        ColorType::Luma,
    )?;
    let packaged = package(&base_jpeg, &gain_jpeg, &gain.metadata)?;

    // Staged like the TIFF path: the whole package is built in memory first, so the
    // final path only ever sees a complete Ultra HDR file.
    let staged = staged::stage_bytes(path, &packaged)?;
    Ok((staged, EncodeOutcome { loss, stats }))
}

fn encode_jpeg(
    rgb: &[u8],
    width: u32,
    height: u32,
    icc: Option<&[u8]>,
    label: &str,
    color_type: ColorType,
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

    #[test]
    fn packaged_stream_round_trips_legacy_metadata_without_iso() {
        let base = encode_jpeg(
            &[64, 64, 64, 192, 192, 192, 64, 64, 64, 192, 192, 192],
            2,
            2,
            None,
            "test base",
            ColorType::Rgb,
        )
        .unwrap();
        let gain =
            encode_jpeg(&[0, 255, 0, 255], 2, 2, None, "test gain", ColorType::Luma).unwrap();
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
