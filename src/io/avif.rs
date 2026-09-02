//! Deterministic 10-bit 4:4:4 AVIF encoding for the Rec.2100 PQ/HLG renditions.
//!
//! nc owns the container. libaom (via `libaom-sys`, whose vendored source is
//! pinned by `Cargo.lock`) produces the AV1 codestream; every ISOBMFF/MIAF box
//! around it is written here. That split is deliberate and is recorded in
//! `docs/tasks/output/hdr-avif-output.md`: no published crate ships libavif
//! ≥ 1.4.2, and `avif-serialize` cannot emit the `MA1A` brand the AVIF v1.2
//! Advanced Profile requires. Authoring the boxes also means the brands, item
//! properties and level are *stated* by nc rather than inherited from an
//! encoder's defaults, which is what the task's conformance clause asks for.
//!
//! Three invariants hold everywhere below:
//!
//! * **`av1C` is filled from the codestream, never from the encoder config.**
//!   `AV1E_GET_SEQ_LEVEL_IDX` reports the encoder's *target* level (31 = unset),
//!   so trusting it writes a bogus level into the file. [`parse_sequence_header`]
//!   reads back what libaom actually wrote, and [`encode`] then checks that it
//!   agrees with the renderer's declared signalling before packaging anything.
//! * **`MA1A` is advertised only inside Advanced Profile limits.** See
//!   [`ADVANCED_MAX_PIXELS`] and friends; outside them the file is still a valid
//!   general-brand AVIF, and the omission is reported rather than hidden.
//! * **Nothing time-varying or random is written.** No timestamps, no UUIDs, no
//!   EXIF/XMP unless requested, and one encoder thread — so repeated encodes of
//!   the same input on the same build are byte-identical.

use std::ffi::{CStr, c_int, c_uint};
use std::path::Path;
use std::ptr::NonNull;

use libaom_sys as aom;

use crate::io::staged::{self, Staged};
use crate::pipeline::colorimetry::pinned::BT2020_NCL_RGB_TO_YCBCR;
use crate::pipeline::hdr::{self, RenderedHdr};
use crate::types::{EncodeOutcome, EncodeReport, NcError, OutputStats, Result};

/// Coded bit depth. 10-bit is the task's pinned contract for both PQ and HLG.
const BIT_DEPTH: u8 = 10;
/// Largest 10-bit code value, and the full-range scale factor (BT.2100 Table 9).
const MAX_CODE: f32 = 1023.0;
/// Full-range achromatic chroma level, `2^(n-1)`.
const CHROMA_ZERO: f32 = 512.0;

/// AV1 `seq_level_idx` for level 6.0, the Advanced Profile ceiling.
///
/// The index encodes `(major - 2) * 4 + minor`, so 6.0 is 16 and 6.1/6.2/6.3 are
/// 17/18/19 — i.e. the bound is `<= 16`, not "any index that looks like a 6".
const ADVANCED_MAX_SEQ_LEVEL_IDX: u8 = 16;
/// AV1 `seq_profile` 1 = High Profile, which 4:4:4 requires.
const SEQ_PROFILE_HIGH: u8 = 1;

/// `seq_level_idx` 31 is AV1's "maximum parameters" sentinel, **not** a level.
///
/// Real and reachable: libaom emits it for an image too large for any defined
/// level, so a 74.6 MP scan produces it. Formatting it as a level would print
/// "9.3", which is not a thing the specification defines — hence [`level_name`].
const SEQ_LEVEL_IDX_MAX_PARAMETERS: u8 = 31;

/// Human-readable name for a `seq_level_idx`.
///
/// The index encodes `(major - 2) * 4 + minor` for defined levels; 31 is the
/// maximum-parameters sentinel and 24..=30 are reserved, so neither is rendered as
/// a version number.
pub fn level_name(seq_level_idx: u8) -> String {
    match seq_level_idx {
        SEQ_LEVEL_IDX_MAX_PARAMETERS => "maximum-parameters".to_string(),
        24..=30 => format!("reserved({seq_level_idx})"),
        idx => format!("{}.{}", 2 + (idx >> 2), idx & 3),
    }
}

// Advanced Profile coded-image limits, quoted from the AVIF v1.2 specification
// (§ "AVIF Advanced Profile"): "coded image items compliant to the AVIF Advanced
// profile may not have a number of pixels greater than 35651584, a width greater
// than 16384 or a height greater than 8704."
/// Maximum coded pixel count for the Advanced Profile.
const ADVANCED_MAX_PIXELS: u64 = 35_651_584;
/// Maximum coded width for the Advanced Profile.
const ADVANCED_MAX_WIDTH: u32 = 16_384;
/// Maximum coded height for the Advanced Profile.
const ADVANCED_MAX_HEIGHT: u32 = 8_704;

/// Hard per-axis ceiling for the AV1 **encoder**, 65,536.
///
/// This is a format limit, not an implementation one: the sequence header codes
/// `frame_width_bits` as `f(4)`, so a dimension gets at most 16 bits. libaom
/// enforces it in `validate_config` (vendored `av1/av1_cx_iface.c:646-647`,
/// libaom 3.11.0) — `RANGE_CHECK(cfg, g_w, 1, 65536); // 16 bits available`,
/// and the same for `g_h` — and rejects anything larger at encoder init. Do not
/// substitute `aom_img_alloc`'s documented `2^27`: that bounds the image
/// *allocator*, so using it would let a full quantization pass run and three
/// planes be allocated before init failed with a generic error.
const AV1_MAX_DIMENSION: u32 = 65_536;

/// Pinned encoder speed. Part of the byte-determinism contract, like the single
/// thread and the disabled tiling — not a knob.
const CPU_USED: c_int = 6;

/// Pinned constant-quality level, chosen by measurement.
///
/// A **fixed part of the preset's definition**, not a conversion knob — the same
/// shape as `io::ultra_hdr`'s `JPEG_QUALITY`. A user-facing quality control would
/// need a recipe key, a merge arm and a `pipeline_version` story; the task pins
/// settings for the initial determinism contract instead.
///
/// Measured on an 18.66 MP real scan (file size) and a 256x64 four-class test
/// field decoded by `avifdec`/dav1d (worst per-plane code error out of 1023):
///
/// | `cq_level` | real-scan file | max err | RMS |
/// |---|---|---|---|
/// | 0 | 20.38 MiB | **0** | **0.000** |
/// | 8 | 0.99 MiB | 10 | 0.85 |
/// | 12 | 0.35 MiB | 14 | 1.24 |
/// | 20 | 0.07 MiB | 20 | 1.91 |
///
/// 8 is the choice: under 1% of the code range at ~1 MiB for an 18.7 MP frame.
/// AVIF is nc's *delivery* HDR container, so some loss is appropriate — the
/// archival paths are `film-master` and the planned lossless HDR TIFFs. Note the
/// first row: `cq_level = 0` is **mathematically lossless**, so AV1 could carry a
/// bit-exact HDR still if a preset ever wants one, at ~20x the size.
const CQ_LEVEL: c_uint = 8;

/// Which AVIF profile the produced file may honestly advertise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AvifProfile {
    /// Within every Advanced Profile limit — `MA1A` is advertised.
    Advanced,
    /// Outside at least one limit. Still a valid AVIF, but `MA1A` is omitted and
    /// the reason travels back to the caller for the report.
    GeneralOnly {
        /// Human-readable statement of which limit was exceeded.
        reason: String,
    },
}

impl AvifProfile {
    /// Whether the `MA1A` compatible brand may be written.
    fn advertises_advanced(&self) -> bool {
        matches!(self, AvifProfile::Advanced)
    }
}

/// What [`encode`] resolved, for the JSON report.
#[derive(Clone, Debug, PartialEq)]
pub struct AvifSummary {
    pub profile: AvifProfile,
    pub bit_depth: u8,
    pub seq_profile: u8,
    pub seq_level_idx: u8,
    pub cicp: (u8, u8, u8),
    pub full_range: bool,
    pub codestream_bytes: usize,
}

/// Encode one rendered Rec.2100 HDR image as a 10-bit 4:4:4 AVIF file.
///
/// The destination is written through [`staged`], so a failure anywhere — in
/// quantization, in libaom, or in packaging — leaves no partial file at `path`.
pub fn encode(
    render: RenderedHdr,
    path: &Path,
) -> Result<(Staged, EncodeOutcome, Box<AvifSummary>)> {
    let (image, metadata) = render.into_parts();
    let (width, height) = (image.width(), image.height());
    check_encodable_dimensions(width, height)?;

    let (planes, loss, stats) = quantize_to_ycbcr(image.rgb(), width, height)?;
    let codestream = encode_codestream(&planes, width, height, &metadata)?;

    // Read back what libaom actually wrote and refuse to package a file whose
    // codestream disagrees with the signalling the renderer declared.
    let header = parse_sequence_header(&codestream)?;
    verify_codestream(&header, width, height, &metadata)?;

    let profile = resolve_profile(&header, width, height);
    let bytes = write_container(&codestream, width, height, &header, &metadata, &profile)?;
    let staged = staged::stage_bytes(path, &bytes)?;

    let summary = AvifSummary {
        profile,
        bit_depth: BIT_DEPTH,
        seq_profile: header.seq_profile,
        seq_level_idx: header.seq_level_idx_0,
        cicp: (
            metadata.cicp_color_primaries,
            metadata.cicp_transfer,
            metadata.cicp_matrix_coefficients,
        ),
        full_range: metadata.full_range,
        codestream_bytes: codestream.len(),
    };
    Ok((staged, EncodeOutcome { loss, stats }, Box::new(summary)))
}

fn check_encodable_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(NcError::Other(format!(
            "AVIF encoding received an empty image ({width}x{height})"
        )));
    }
    if width > AV1_MAX_DIMENSION || height > AV1_MAX_DIMENSION {
        return Err(NcError::Unsupported(format!(
            "image is {width}x{height}, beyond the AV1 encoder's {AV1_MAX_DIMENSION}-pixel \
             per-axis limit"
        )));
    }
    Ok(())
}

/// Classify the produced codestream against the Advanced Profile's limits.
///
/// Every clause is checked against the file that exists, not the request: the
/// level comes from the parsed sequence header, so an encoder that chose a higher
/// level than expected downgrades the brand instead of being mis-advertised.
fn resolve_profile(header: &SequenceHeader, width: u32, height: u32) -> AvifProfile {
    let pixels = u64::from(width) * u64::from(height);
    let reason = if header.seq_profile != SEQ_PROFILE_HIGH {
        Some(format!(
            "AV1 seq_profile is {} but the Advanced Profile requires High Profile (1)",
            header.seq_profile
        ))
    } else if header.seq_level_idx_0 > ADVANCED_MAX_SEQ_LEVEL_IDX {
        Some(format!(
            "AV1 level {} exceeds the Advanced Profile ceiling of 6.0",
            level_name(header.seq_level_idx_0)
        ))
    } else if pixels > ADVANCED_MAX_PIXELS {
        Some(format!(
            "{pixels} coded pixels exceed the Advanced Profile limit of {ADVANCED_MAX_PIXELS}"
        ))
    } else if width > ADVANCED_MAX_WIDTH {
        Some(format!(
            "coded width {width} exceeds the Advanced Profile limit of {ADVANCED_MAX_WIDTH}"
        ))
    } else if height > ADVANCED_MAX_HEIGHT {
        Some(format!(
            "coded height {height} exceeds the Advanced Profile limit of {ADVANCED_MAX_HEIGHT}"
        ))
    } else {
        None
    };
    match reason {
        Some(reason) => AvifProfile::GeneralOnly { reason },
        None => AvifProfile::Advanced,
    }
}

/// Cross-check the encoded codestream against the renderer's declared contract.
fn verify_codestream(
    header: &SequenceHeader,
    width: u32,
    height: u32,
    metadata: &hdr::HdrRenderMetadata,
) -> Result<()> {
    let mismatch = |what: &str, got: String, want: String| {
        Err(NcError::Other(format!(
            "AVIF codestream {what} is {got} but the render declared {want}; refusing to \
             package a file whose signalling does not match its pixels"
        )))
    };
    if header.seq_profile != SEQ_PROFILE_HIGH {
        return mismatch(
            "seq_profile",
            header.seq_profile.to_string(),
            format!("{SEQ_PROFILE_HIGH} (High, required for 4:4:4)"),
        );
    }
    if !header.still_picture {
        return mismatch("still_picture", "0".into(), "1".into());
    }
    if header.subsampling_x || header.subsampling_y {
        return mismatch(
            "chroma subsampling",
            format!("({}, {})", header.subsampling_x, header.subsampling_y),
            "4:4:4 (false, false)".into(),
        );
    }
    if !header.high_bitdepth || header.twelve_bit {
        return mismatch(
            "bit depth",
            format!(
                "high_bitdepth={} twelve_bit={}",
                header.high_bitdepth, header.twelve_bit
            ),
            format!("{BIT_DEPTH}-bit"),
        );
    }
    if header.mono_chrome {
        return mismatch("mono_chrome", "1".into(), "0".into());
    }
    let declared = (
        metadata.cicp_color_primaries,
        metadata.cicp_transfer,
        metadata.cicp_matrix_coefficients,
    );
    let coded = (
        header.color_primaries,
        header.transfer_characteristics,
        header.matrix_coefficients,
    );
    if coded != declared {
        return mismatch(
            "CICP",
            format!("{}/{}/{}", coded.0, coded.1, coded.2),
            format!("{}/{}/{}", declared.0, declared.1, declared.2),
        );
    }
    if header.color_range != metadata.full_range {
        return mismatch(
            "colour range",
            header.color_range.to_string(),
            metadata.full_range.to_string(),
        );
    }
    if header.max_frame_width != width || header.max_frame_height != height {
        return mismatch(
            "coded size",
            format!("{}x{}", header.max_frame_width, header.max_frame_height),
            format!("{width}x{height}"),
        );
    }
    Ok(())
}

// -- quantization -------------------------------------------------------------

/// 10-bit full-range Y'/Cb/Cr planes, each `width * height` samples.
#[derive(Debug)]
struct Planes {
    y: Vec<u16>,
    cb: Vec<u16>,
    cr: Vec<u16>,
}

/// Convert nonlinear R'G'B' to full-range 10-bit Y'CbCr, counting every loss.
///
/// The matrix is the pinned BT.2020 non-constant-luminance one (AVIF
/// `matrix_coefficients = 9`); it is imported, never restated here, per the
/// colorimetry rule in CLAUDE.md.
///
/// Quantization follows BT.2100-2 Table 9's full-range rows. Clipping is real and
/// reachable rather than defensive: a fully saturated primary lands on
/// `±0.5 · 1023 + 512`, i.e. half a code outside the range at each end, so those
/// samples are counted into [`EncodeReport`] instead of being silently clamped.
fn quantize_to_ycbcr(
    rgb: &[f32],
    width: u32,
    height: u32,
) -> Result<(Planes, EncodeReport, OutputStats)> {
    let pixels = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| NcError::Unsupported(format!("image {width}x{height} is too large")))?;
    let (chunks, remainder) = rgb.as_chunks::<3>();
    if !remainder.is_empty() || chunks.len() != pixels {
        return Err(NcError::Other(format!(
            "AVIF encoding received {} samples for a {width}x{height} image",
            rgb.len()
        )));
    }

    let mut planes = Planes {
        y: Vec::with_capacity(pixels),
        cb: Vec::with_capacity(pixels),
        cr: Vec::with_capacity(pixels),
    };
    let mut loss = EncodeReport {
        total_samples: rgb.len() as u64,
        ..EncodeReport::default()
    };
    let mut sums = [0_f64; 3];

    for px in chunks {
        // Statistics are taken on the encoded R'G'B' signal, not on the written
        // Y'CbCr codes: `OutputStats` is defined per R/G/B channel, and Y'CbCr has
        // no such channels, so reporting chroma under `mean[1]` would mislead.
        for (sum, &channel) in sums.iter_mut().zip(px.iter()) {
            if channel.is_finite() {
                *sum += f64::from(channel);
            }
        }
        let ycbcr = apply_matrix(BT2020_NCL_RGB_TO_YCBCR, *px);
        planes
            .y
            .push(quantize_code(ycbcr[0] * MAX_CODE, 0, &mut loss));
        planes.cb.push(quantize_code(
            ycbcr[1] * MAX_CODE + CHROMA_ZERO,
            CHROMA_ZERO as u16,
            &mut loss,
        ));
        planes.cr.push(quantize_code(
            ycbcr[2] * MAX_CODE + CHROMA_ZERO,
            CHROMA_ZERO as u16,
            &mut loss,
        ));
    }

    let divisor = pixels as f64;
    let stats = OutputStats {
        mean: sums.map(|sum| sum / divisor),
    };
    Ok((planes, loss, stats))
}

/// Round one code-domain value into `[0, MAX_CODE]`, recording any loss.
///
/// `neutral` is the substitute for a non-finite input, and differs per component:
/// zero for luma but the achromatic level for chroma, so a numerical fault cannot
/// turn into a saturated colour.
fn quantize_code(value: f32, neutral: u16, loss: &mut EncodeReport) -> u16 {
    if !value.is_finite() {
        loss.non_finite += 1;
        return neutral;
    }
    let rounded = value.round();
    if rounded < 0.0 {
        loss.clipped_low += 1;
        0
    } else if rounded > MAX_CODE {
        loss.clipped_high += 1;
        MAX_CODE as u16
    } else {
        rounded as u16
    }
}

fn apply_matrix(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

// -- libaom encoding ----------------------------------------------------------

/// Owns an `aom_codec_ctx_t` and destroys it exactly once.
///
/// The context is boxed because libaom stores interior pointers to it, so its
/// address must not change after `aom_codec_enc_init_ver`.
struct Encoder {
    ctx: Box<aom::aom_codec_ctx_t>,
}

impl Encoder {
    fn new(cfg: &aom::aom_codec_enc_cfg_t) -> Result<Self> {
        // SAFETY: `aom_codec_av1_cx` takes no arguments and returns a pointer to
        // a static interface descriptor.
        let iface = unsafe { aom::aom_codec_av1_cx() };
        if iface.is_null() {
            return Err(NcError::Other(
                "libaom returned no AV1 encoder interface".into(),
            ));
        }
        // `zeroed` is the documented pre-init state for the context.
        // SAFETY: `aom_codec_ctx_t` is a plain C struct whose all-zero value is
        // the valid uninitialized state libaom expects here.
        let mut ctx: Box<aom::aom_codec_ctx_t> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: `ctx` is a live, uniquely owned, stably addressed context;
        // `cfg` is a live configuration for the whole call; the ABI version is
        // the one these bindings were generated against.
        let status = unsafe {
            aom::aom_codec_enc_init_ver(
                &mut *ctx,
                iface,
                cfg,
                aom::AOM_CODEC_USE_HIGHBITDEPTH as aom::aom_codec_flags_t,
                aom::AOM_ENCODER_ABI_VERSION as c_int,
            )
        };
        if status != aom::AOM_CODEC_OK {
            // A failed init must not be destroyed, per libaom's contract, so the
            // guard is never constructed on this path.
            return Err(NcError::Other(format!(
                "initializing the AV1 encoder failed: {}",
                err_to_string(status)
            )));
        }
        Ok(Self { ctx })
    }

    /// `aom_codec_control` with an `int`-typed argument.
    fn control_int(&mut self, id: c_uint, value: c_int, what: &str) -> Result<()> {
        // SAFETY: the context is live and initialized. Every control id used here
        // is documented as taking a single `int`, matching this variadic call.
        let status = unsafe { aom::aom_codec_control(&mut *self.ctx, id as c_int, value) };
        self.check(status, what)
    }

    /// `aom_codec_control` with an `unsigned int`-typed argument.
    fn control_uint(&mut self, id: c_uint, value: c_uint, what: &str) -> Result<()> {
        // SAFETY: as `control_int`, for the controls documented as `unsigned int`.
        let status = unsafe { aom::aom_codec_control(&mut *self.ctx, id as c_int, value) };
        self.check(status, what)
    }

    fn check(&self, status: aom::aom_codec_err_t, what: &str) -> Result<()> {
        if status == aom::AOM_CODEC_OK {
            return Ok(());
        }
        // SAFETY: the context is live; both accessors return either NULL or a
        // NUL-terminated string owned by the context.
        let detail = unsafe {
            let ptr = aom::aom_codec_error_detail(&*self.ctx);
            if ptr.is_null() {
                String::new()
            } else {
                format!(": {}", CStr::from_ptr(ptr).to_string_lossy())
            }
        };
        Err(NcError::Write(format!(
            "{what} failed in libaom ({}){detail}",
            err_to_string(status)
        )))
    }

    /// Append every pending compressed packet to `out`.
    ///
    /// Must be called after *each* `aom_codec_encode`: the packet list belongs to
    /// the individual call, so draining only after the flush silently loses the
    /// frame libaom emitted during the first call and yields an empty codestream.
    fn drain(&mut self, out: &mut Vec<u8>) {
        let mut iter: aom::aom_codec_iter_t = std::ptr::null();
        loop {
            // SAFETY: the context is live and `iter` is the required cursor,
            // initialized to NULL and only passed back to this function.
            let pkt = unsafe { aom::aom_codec_get_cx_data(&mut *self.ctx, &mut iter) };
            let Some(pkt) = NonNull::new(pkt.cast_mut()) else {
                return;
            };
            // SAFETY: a non-null packet is valid until the next libaom call on
            // this context, which cannot happen before this borrow ends.
            let pkt = unsafe { pkt.as_ref() };
            if pkt.kind == aom::AOM_CODEC_CX_FRAME_PKT {
                // SAFETY: for a frame packet libaom guarantees the `frame` union
                // member is active, with `buf` valid for `sz` bytes.
                let frame = unsafe { pkt.data.frame };
                if !frame.buf.is_null() && frame.sz > 0 {
                    // SAFETY: validated non-null with a positive length, owned by
                    // the still-live context.
                    out.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(frame.buf.cast::<u8>(), frame.sz)
                    });
                }
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `ctx` was successfully initialized (the guard is not built
        // otherwise) and is destroyed exactly once, here.
        unsafe { aom::aom_codec_destroy(&mut *self.ctx) };
    }
}

/// Owns an `aom_image_t` allocated by libaom and frees it exactly once.
struct Image(NonNull<aom::aom_image_t>);

impl Image {
    fn alloc(width: u32, height: u32) -> Result<Self> {
        // SAFETY: a NULL descriptor asks libaom to heap-allocate both descriptor
        // and storage; the dimensions were bounds-checked by the caller.
        let ptr = unsafe {
            aom::aom_img_alloc(
                std::ptr::null_mut(),
                aom::AOM_IMG_FMT_I44416,
                width,
                height,
                1,
            )
        };
        NonNull::new(ptr).map(Self).ok_or_else(|| {
            NcError::Resource(format!(
                "libaom could not allocate a {width}x{height} 10-bit 4:4:4 frame"
            ))
        })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `aom_img_alloc` and is freed once, here.
        unsafe { aom::aom_img_free(self.0.as_ptr()) };
    }
}

/// Encode the planes into a single still-picture AV1 codestream.
fn encode_codestream(
    planes: &Planes,
    width: u32,
    height: u32,
    metadata: &hdr::HdrRenderMetadata,
) -> Result<Vec<u8>> {
    // SAFETY: `aom_codec_av1_cx` returns a static interface descriptor; `cfg` is
    // a live out-parameter for the duration of the call.
    let mut cfg: aom::aom_codec_enc_cfg_t = unsafe { std::mem::zeroed() };
    let status = unsafe {
        let iface = aom::aom_codec_av1_cx();
        if iface.is_null() {
            return Err(NcError::Other(
                "libaom returned no AV1 encoder interface".into(),
            ));
        }
        aom::aom_codec_enc_config_default(iface, &mut cfg, aom::AOM_USAGE_ALL_INTRA)
    };
    if status != aom::AOM_CODEC_OK {
        return Err(NcError::Other(format!(
            "reading libaom's default all-intra configuration failed: {}",
            err_to_string(status)
        )));
    }

    cfg.g_w = width;
    cfg.g_h = height;
    cfg.g_profile = u32::from(SEQ_PROFILE_HIGH);
    cfg.g_bit_depth = aom::AOM_BITS_10;
    cfg.g_input_bit_depth = u32::from(BIT_DEPTH);
    // One thread is part of the determinism contract, not a performance choice.
    cfg.g_threads = 1;
    // `g_limit = 1` makes libaom set `still_picture`, and leaving
    // `full_still_picture_hdr` at 0 makes it use the reduced header AVIF wants.
    cfg.g_limit = 1;
    cfg.full_still_picture_hdr = 0;
    cfg.monochrome = 0;
    cfg.rc_end_usage = aom::AOM_Q;

    let mut encoder = Encoder::new(&cfg)?;
    encoder.control_int(aom::AOME_SET_CPUUSED, CPU_USED, "setting the AV1 speed")?;
    encoder.control_uint(aom::AOME_SET_CQ_LEVEL, CQ_LEVEL, "setting the AV1 quality")?;
    encoder.control_int(
        aom::AV1E_SET_COLOR_PRIMARIES,
        c_int::from(metadata.cicp_color_primaries),
        "setting the AV1 colour primaries",
    )?;
    encoder.control_int(
        aom::AV1E_SET_TRANSFER_CHARACTERISTICS,
        c_int::from(metadata.cicp_transfer),
        "setting the AV1 transfer characteristics",
    )?;
    encoder.control_int(
        aom::AV1E_SET_MATRIX_COEFFICIENTS,
        c_int::from(metadata.cicp_matrix_coefficients),
        "setting the AV1 matrix coefficients",
    )?;
    encoder.control_int(
        aom::AV1E_SET_COLOR_RANGE,
        c_int::from(metadata.full_range),
        "setting the AV1 colour range",
    )?;
    // Tiling and row multithreading both perturb output bytes; pin them off so a
    // single-thread encode is reproducible.
    encoder.control_int(aom::AV1E_SET_ROW_MT, 0, "disabling AV1 row threading")?;
    encoder.control_int(aom::AV1E_SET_TILE_COLUMNS, 0, "pinning AV1 tile columns")?;
    encoder.control_int(aom::AV1E_SET_TILE_ROWS, 0, "pinning AV1 tile rows")?;

    let image = Image::alloc(width, height)?;
    fill_image(&image, planes, width, height);

    // SAFETY: the encoder and image are live; `pts`/`duration` are the
    // single-frame values libaom documents for a still picture.
    let status = unsafe { aom::aom_codec_encode(&mut *encoder.ctx, image.0.as_ptr(), 0, 1, 0) };
    encoder.check(status, "encoding the AVIF frame")?;
    let mut codestream = Vec::new();
    encoder.drain(&mut codestream);
    // SAFETY: a NULL image signals end-of-stream on a live context.
    let status = unsafe { aom::aom_codec_encode(&mut *encoder.ctx, std::ptr::null(), 0, 1, 0) };
    encoder.check(status, "flushing the AVIF encoder")?;
    encoder.drain(&mut codestream);

    if codestream.is_empty() {
        return Err(NcError::Write(
            "libaom produced no AV1 codestream for the AVIF image".into(),
        ));
    }
    Ok(codestream)
}

/// Copy the planes into libaom's strided buffers.
fn fill_image(image: &Image, planes: &Planes, width: u32, height: u32) {
    for (index, plane) in [&planes.y, &planes.cb, &planes.cr].into_iter().enumerate() {
        // SAFETY: the descriptor came from a successful `aom_img_alloc` for these
        // dimensions in `AOM_IMG_FMT_I44416`, so all three planes exist and each
        // row holds at least `width` 16-bit samples within `stride[index]` bytes.
        unsafe {
            let img = self_ref(image);
            let stride = img.stride[index] as usize;
            let base = img.planes[index];
            for row in 0..height as usize {
                let dst = base.add(row * stride).cast::<u16>();
                let src = &plane[row * width as usize..(row + 1) * width as usize];
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, width as usize);
            }
        }
    }
    // SAFETY: as above — a live, uniquely owned descriptor.
    unsafe {
        let img = &mut *image.0.as_ptr();
        img.bit_depth = u32::from(BIT_DEPTH);
        img.range = aom::AOM_CR_FULL_RANGE;
        img.cp = aom::AOM_CICP_CP_BT_2020;
        img.tc = aom::AOM_CICP_TC_SMPTE_2084;
        img.mc = aom::AOM_CICP_MC_BT_2020_NCL;
    }
}

/// SAFETY: caller must hold a live, uniquely owned [`Image`].
unsafe fn self_ref(image: &Image) -> &aom::aom_image_t {
    unsafe { image.0.as_ref() }
}

fn err_to_string(status: aom::aom_codec_err_t) -> String {
    // SAFETY: `aom_codec_err_to_string` maps any value to a static
    // NUL-terminated string, including unknown codes.
    let ptr = unsafe { aom::aom_codec_err_to_string(status) };
    if ptr.is_null() {
        return format!("error {status}");
    }
    // SAFETY: non-null result is a static NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

// -- sequence-header inspection -----------------------------------------------

/// The subset of the AV1 sequence header the container and its checks need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    pub seq_level_idx_0: u8,
    pub seq_tier_0: u8,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub mono_chrome: bool,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub color_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
}

/// Big-endian bit reader that reports exhaustion instead of panicking, because
/// its input is an encoder's output rather than a trusted literal.
struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn f(&mut self, n: u32) -> Result<u32> {
        let mut value = 0_u32;
        for _ in 0..n {
            let byte = *self
                .buf
                .get(self.pos >> 3)
                .ok_or_else(|| NcError::Other("AV1 sequence header ended mid-field".to_string()))?;
            let bit = (byte >> (7 - (self.pos & 7))) & 1;
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        Ok(value)
    }

    fn flag(&mut self) -> Result<bool> {
        Ok(self.f(1)? == 1)
    }
}

/// Read a low-overhead-bitstream `leb128` size field.
fn leb128(buf: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for i in 0..8 {
        let byte = *buf
            .get(*at)
            .ok_or_else(|| NcError::Other("AV1 OBU size field is truncated".to_string()))?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(NcError::Other(
        "AV1 OBU size field exceeds eight bytes".to_string(),
    ))
}

/// Find and parse the sequence-header OBU in a low-overhead AV1 bitstream.
pub fn parse_sequence_header(codestream: &[u8]) -> Result<SequenceHeader> {
    const OBU_SEQUENCE_HEADER: u8 = 1;
    let mut at = 0_usize;
    while at < codestream.len() {
        let header = codestream[at];
        let obu_type = (header >> 3) & 0x0F;
        let has_extension = (header >> 2) & 1 == 1;
        let has_size_field = (header >> 1) & 1 == 1;
        let mut cursor = at + 1;
        if has_extension {
            cursor += 1;
        }
        if !has_size_field {
            return Err(NcError::Other(
                "AV1 codestream is not in low-overhead bitstream format (no OBU size field)"
                    .to_string(),
            ));
        }
        let size = usize::try_from(leb128(codestream, &mut cursor)?)
            .map_err(|_| NcError::Other("AV1 OBU size does not fit in memory".to_string()))?;
        let end = cursor
            .checked_add(size)
            .filter(|end| *end <= codestream.len())
            .ok_or_else(|| {
                NcError::Other("AV1 OBU claims more payload than the codestream holds".to_string())
            })?;
        if obu_type == OBU_SEQUENCE_HEADER {
            return parse_sequence_header_payload(&codestream[cursor..end]);
        }
        at = end;
    }
    Err(NcError::Other(
        "AV1 codestream contains no sequence-header OBU".to_string(),
    ))
}

fn parse_sequence_header_payload(payload: &[u8]) -> Result<SequenceHeader> {
    let mut r = BitReader::new(payload);
    let seq_profile = r.f(3)? as u8;
    let still_picture = r.flag()?;
    let reduced_still_picture_header = r.flag()?;
    if !reduced_still_picture_header {
        // nc's encoder always produces the reduced header (`g_limit = 1` with
        // `full_still_picture_hdr = 0`). The full form carries timing,
        // decoder-model and frame-id syntax that would have to be parsed before
        // `color_config`, so rather than half-parse it, this is a loud refusal.
        return Err(NcError::Other(
            "AV1 sequence header is not a reduced still-picture header; nc cannot verify its \
             signalling"
                .to_string(),
        ));
    }
    let seq_level_idx_0 = r.f(5)? as u8;
    // Implied zero by the reduced header: `seq_tier` is only coded in the full form.
    let seq_tier_0 = 0;

    let frame_width_bits = r.f(4)? + 1;
    let frame_height_bits = r.f(4)? + 1;
    let max_frame_width = r.f(frame_width_bits)? + 1;
    let max_frame_height = r.f(frame_height_bits)? + 1;

    let _use_128x128_superblock = r.flag()?;
    let _enable_filter_intra = r.flag()?;
    let _enable_intra_edge_filter = r.flag()?;
    // The reduced header omits every inter-coding tool flag.
    let _enable_superres = r.flag()?;
    let _enable_cdef = r.flag()?;
    let _enable_restoration = r.flag()?;

    // color_config()
    let high_bitdepth = r.flag()?;
    let twelve_bit = if seq_profile == 2 && high_bitdepth {
        r.flag()?
    } else {
        false
    };
    // Profile 1 (High) is always colour; only the other profiles code the flag.
    let mono_chrome = if seq_profile == 1 { false } else { r.flag()? };
    let color_description_present = r.flag()?;
    let (mut color_primaries, mut transfer_characteristics, mut matrix_coefficients) = (2, 2, 2);
    if color_description_present {
        color_primaries = r.f(8)? as u8;
        transfer_characteristics = r.f(8)? as u8;
        matrix_coefficients = r.f(8)? as u8;
    }
    let (color_range, subsampling_x, subsampling_y, chroma_sample_position);
    if mono_chrome {
        color_range = r.flag()?;
        (subsampling_x, subsampling_y, chroma_sample_position) = (true, true, 0);
    } else if color_primaries == 1 && transfer_characteristics == 13 && matrix_coefficients == 0 {
        // The sRGB special case implies full-range 4:4:4 with nothing coded.
        (
            color_range,
            subsampling_x,
            subsampling_y,
            chroma_sample_position,
        ) = (true, false, false, 0);
    } else {
        color_range = r.flag()?;
        let (sx, sy) = match seq_profile {
            0 => (true, true),
            1 => (false, false),
            _ if twelve_bit => {
                let sx = r.flag()?;
                (sx, if sx { r.flag()? } else { false })
            }
            _ => (true, false),
        };
        (subsampling_x, subsampling_y) = (sx, sy);
        chroma_sample_position = if sx && sy { r.f(2)? as u8 } else { 0 };
    }

    Ok(SequenceHeader {
        seq_profile,
        still_picture,
        reduced_still_picture_header,
        seq_level_idx_0,
        seq_tier_0,
        max_frame_width,
        max_frame_height,
        high_bitdepth,
        twelve_bit,
        mono_chrome,
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        color_range,
        subsampling_x,
        subsampling_y,
        chroma_sample_position,
    })
}

// -- container writing --------------------------------------------------------

/// `ipco` property indices are 1-based and must line up with the `ipma`
/// associations below, so they are named once here.
const PROP_ISPE: u8 = 1;
const PROP_PIXI: u8 = 2;
const PROP_AV1C: u8 = 3;
const PROP_COLR: u8 = 4;
const PROP_CLLI: u8 = 5;
/// `ipma`'s per-association essential bit.
const ESSENTIAL: u8 = 0x80;
/// The single colour image item's id.
const COLOR_ITEM_ID: u16 = 1;

fn u16b(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn u32b(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Write a `Box` with a 32-bit size prefix, back-patching the length.
///
/// The size is a checked conversion, not a cast: a silently truncated `mdat`
/// length would be a malformed file written without an error.
/// [`check_codestream_addressable`] makes the failure unreachable, so a panic
/// here means that guard and this writer disagree — which must be loud.
fn bx(out: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    u32b(out, 0);
    out.extend_from_slice(kind);
    body(out);
    let size = u32::try_from(out.len() - start).unwrap_or_else(|_| {
        panic!(
            "AVIF `{}` box grew past the 32-bit box size this writer emits",
            String::from_utf8_lossy(kind)
        )
    });
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Write a `FullBox` — a `Box` whose body starts with version + 24-bit flags.
fn fullbx(
    out: &mut Vec<u8>,
    kind: &[u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>),
) {
    bx(out, kind, |o| {
        o.push(version);
        o.extend_from_slice(&flags.to_be_bytes()[1..]);
        body(o);
    });
}

/// Bytes of a 32-bit `Box` header: the size field plus the four-character type.
const BOX_HEADER_BYTES: usize = 8;

/// Largest codestream a 32-bit `mdat` size and `iloc` extent can address.
///
/// The header counts: `mdat`'s size field covers the box, not just its payload,
/// so a codestream in `u32::MAX - 7 ..= u32::MAX` would wrap to a size of 1..8
/// and produce a malformed file rather than an error.
const MAX_CODESTREAM_BYTES: usize = u32::MAX as usize - BOX_HEADER_BYTES;

fn check_codestream_addressable(len: usize) -> Result<()> {
    if len > MAX_CODESTREAM_BYTES {
        return Err(NcError::Unsupported(format!(
            "AV1 codestream is {len} bytes, beyond the {MAX_CODESTREAM_BYTES}-byte 32-bit \
             extent this writer emits"
        )));
    }
    Ok(())
}

/// Assemble the AVIF file around an AV1 codestream.
fn write_container(
    codestream: &[u8],
    width: u32,
    height: u32,
    header: &SequenceHeader,
    metadata: &hdr::HdrRenderMetadata,
    profile: &AvifProfile,
) -> Result<Vec<u8>> {
    // The `iloc` extent and the `mdat` box header are both 32-bit here, so a
    // codestream that cannot be addressed by them is refused rather than
    // truncated. Nothing in nc's range approaches this.
    check_codestream_addressable(codestream.len())?;

    let mut out = Vec::with_capacity(codestream.len() + 512);

    bx(&mut out, b"ftyp", |o| {
        o.extend_from_slice(b"avif"); // major_brand
        u32b(o, 0); // minor_version
        o.extend_from_slice(b"avif");
        o.extend_from_slice(b"mif1");
        o.extend_from_slice(b"miaf");
        if profile.advertises_advanced() {
            o.extend_from_slice(b"MA1A");
        }
    });

    // `iloc` holds an absolute file offset, which is only known once `meta` is
    // fully sized — so the field is written as zero and patched below.
    let mut extent_offset_at = 0_usize;
    let content_light = content_light_level(metadata);

    fullbx(&mut out, b"meta", 0, 0, |o| {
        fullbx(o, b"hdlr", 0, 0, |o| {
            u32b(o, 0); // pre_defined
            o.extend_from_slice(b"pict"); // handler_type
            u32b(o, 0); // reserved[0]
            u32b(o, 0); // reserved[1]
            u32b(o, 0); // reserved[2]
            o.push(0); // empty, NUL-terminated name
        });
        fullbx(o, b"pitm", 0, 0, |o| u16b(o, COLOR_ITEM_ID));
        fullbx(o, b"iloc", 0, 0, |o| {
            o.push(0x44); // offset_size = 4, length_size = 4
            o.push(0x00); // base_offset_size = 0, reserved
            u16b(o, 1); // item_count
            u16b(o, COLOR_ITEM_ID);
            u16b(o, 0); // data_reference_index (0 = this file)
            u16b(o, 1); // extent_count
            extent_offset_at = o.len();
            u32b(o, 0); // extent_offset, patched after `meta` is sized
            u32b(o, codestream.len() as u32); // extent_length
        });
        fullbx(o, b"iinf", 0, 0, |o| {
            u16b(o, 1); // entry_count
            fullbx(o, b"infe", 2, 0, |o| {
                u16b(o, COLOR_ITEM_ID);
                u16b(o, 0); // item_protection_index
                o.extend_from_slice(b"av01"); // item_type
                o.extend_from_slice(b"Color\0"); // item_name
            });
        });
        bx(o, b"iprp", |o| {
            bx(o, b"ipco", |o| {
                fullbx(o, b"ispe", 0, 0, |o| {
                    u32b(o, width);
                    u32b(o, height);
                });
                fullbx(o, b"pixi", 0, 0, |o| {
                    o.push(3); // num_channels
                    o.extend_from_slice(&[BIT_DEPTH; 3]);
                });
                bx(o, b"av1C", |o| write_av1c(o, header));
                bx(o, b"colr", |o| {
                    o.extend_from_slice(b"nclx");
                    u16b(o, u16::from(metadata.cicp_color_primaries));
                    u16b(o, u16::from(metadata.cicp_transfer));
                    u16b(o, u16::from(metadata.cicp_matrix_coefficients));
                    o.push(if metadata.full_range { 0x80 } else { 0x00 });
                });
                if let Some((max_cll, max_pall)) = content_light {
                    bx(o, b"clli", |o| {
                        u16b(o, max_cll);
                        u16b(o, max_pall);
                    });
                }
            });
            fullbx(o, b"ipma", 0, 0, |o| {
                u32b(o, 1); // entry_count
                u16b(o, COLOR_ITEM_ID);
                // Only `av1C` is essential: a reader that cannot understand it
                // must not render the item, whereas the descriptive properties
                // are safe to skip.
                let mut props = vec![PROP_ISPE, PROP_PIXI, PROP_AV1C | ESSENTIAL, PROP_COLR];
                if content_light.is_some() {
                    props.push(PROP_CLLI);
                }
                o.push(props.len() as u8);
                o.extend_from_slice(&props);
            });
        });
    });

    // The payload starts immediately after the `mdat` box header.
    let payload_offset = u32::try_from(out.len() + BOX_HEADER_BYTES).map_err(|_| {
        NcError::Other("AVIF metadata grew beyond a 32-bit file offset".to_string())
    })?;
    out[extent_offset_at..extent_offset_at + 4].copy_from_slice(&payload_offset.to_be_bytes());
    bx(&mut out, b"mdat", |o| o.extend_from_slice(codestream));

    Ok(out)
}

/// The `av1C` decoder-configuration record (AV1-ISOBMFF § 2.3.1).
///
/// Every field comes from `header`, i.e. from the codestream itself. `configOBUs`
/// is deliberately left empty: the codestream already carries its sequence
/// header, and libavif 1.4.2 writes it the same way.
fn write_av1c(out: &mut Vec<u8>, header: &SequenceHeader) {
    out.push(0x80 | 1); // marker = 1, version = 1
    out.push((header.seq_profile << 5) | (header.seq_level_idx_0 & 0x1F));
    out.push(
        (header.seq_tier_0 << 7)
            | (u8::from(header.high_bitdepth) << 6)
            | (u8::from(header.twelve_bit) << 5)
            | (u8::from(header.mono_chrome) << 4)
            | (u8::from(header.subsampling_x) << 3)
            | (u8::from(header.subsampling_y) << 2)
            | (header.chroma_sample_position & 0x03),
    );
    out.push(0); // reserved; initial_presentation_delay_present = 0
}

/// Content-light-level metadata for the `clli` box, as `(MaxCLL, MaxPALL)`.
///
/// Both numbers are **measured from this frame's pixels** by
/// `pipeline::hdr::render_linear`, which still holds display-linear luminance
/// relative to reference white: MaxCLL is the brightest pixel's luminance in
/// cd/m² and MaxPALL (MaxFALL) the frame average, exactly the CTA-861.3
/// semantics displays tone-map from. Neither is the renderer's 1000-nit peak or
/// 203-nit reference white — a dark frame must not claim a bright one's numbers.
///
/// Only PQ gets the box: PQ codes absolute luminance, whereas HLG is
/// display-referred, so absolute values there would be a false claim.
fn content_light_level(metadata: &hdr::HdrRenderMetadata) -> Option<(u16, u16)> {
    match metadata.transfer {
        hdr::HdrTransfer::Pq => Some((
            metadata.content_light.max_cll_nits,
            metadata.content_light.max_fall_nits,
        )),
        hdr::HdrTransfer::Hlg => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::display_tone::DisplayTone;

    /// Parse a minimal box tree into `(type, size, body offset)` triples.
    fn boxes(buf: &[u8]) -> Vec<(String, usize, usize)> {
        fn walk(buf: &[u8], start: usize, end: usize, out: &mut Vec<(String, usize, usize)>) {
            const CONTAINERS: [&[u8; 4]; 5] = [b"meta", b"iprp", b"ipco", b"iinf", b"iref"];
            let mut at = start;
            while at + 8 <= end {
                let size = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
                let kind: [u8; 4] = buf[at + 4..at + 8].try_into().unwrap();
                out.push((String::from_utf8_lossy(&kind).into_owned(), size, at + 8));
                if CONTAINERS.contains(&&kind) {
                    // `meta`/`iinf` are FullBoxes; `iinf` also has a count.
                    let skip = match &kind {
                        b"meta" => 4,
                        b"iinf" => 6,
                        _ => 0,
                    };
                    walk(buf, at + 8 + skip, at + size, out);
                }
                if size == 0 {
                    return;
                }
                at += size;
            }
        }
        let mut out = Vec::new();
        walk(buf, 0, buf.len(), &mut out);
        out
    }

    fn find<'a>(tree: &'a [(String, usize, usize)], kind: &str) -> &'a (String, usize, usize) {
        tree.iter()
            .find(|(name, _, _)| name == kind)
            .unwrap_or_else(|| panic!("no `{kind}` box in {tree:?}"))
    }

    /// Render a tiny real image so tests use genuine renderer metadata rather
    /// than a hand-built struct that could drift from the renderer's contract.
    fn render_tiny(transfer: hdr::HdrTransfer, rgb: &[f32], w: u32, h: u32) -> RenderedHdr {
        use crate::algo::reconstruct;
        use crate::pipeline::render_split::display_source;
        use crate::pipeline::working_space::map_nc_film_rgb_v1;
        use crate::types::{FilmBase, LinearImage, PrintParams, Reconstruction};

        let scan = rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new(w, h, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        let shared = display_source(map_nc_film_rgb_v1(film), &PrintParams::default()).unwrap();
        hdr::render(&shared, transfer, DisplayTone::shoulder(0.75).unwrap()).unwrap()
    }

    fn pq_metadata() -> hdr::HdrRenderMetadata {
        *render_tiny(hdr::HdrTransfer::Pq, &[0.2, 0.4, 0.6, 0.5, 0.5, 0.5], 2, 1).metadata()
    }

    /// The `clli` box body as `(MaxCLL, MaxPALL)`, or `None` when it is absent.
    fn clli_of(bytes: &[u8]) -> Option<(u16, u16)> {
        let tree = boxes(bytes);
        let (_, _, at) = *tree.iter().find(|(name, _, _)| name == "clli")?;
        Some((
            u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap()),
            u16::from_be_bytes(bytes[at + 2..at + 4].try_into().unwrap()),
        ))
    }

    #[test]
    fn quantization_counts_every_loss_and_neutralizes_faults() {
        // One pixel per case: black, reference-ish grey, a NaN, and a saturated
        // primary whose chroma lands half a code outside the range.
        let rgb = vec![
            0.0,
            0.0,
            0.0, //
            0.5,
            0.5,
            0.5, //
            f32::NAN,
            0.5,
            0.5, //
            1.0,
            0.0,
            0.0,
        ];
        let (planes, loss, stats) = quantize_to_ycbcr(&rgb, 4, 1).unwrap();
        assert_eq!(loss.total_samples, 12);
        // The NaN reaches all three outputs of its pixel, so three samples are
        // non-finite, and each falls back to its own neutral level.
        assert_eq!(loss.non_finite, 3);
        assert_eq!(planes.y[2], 0);
        assert_eq!(planes.cb[2], CHROMA_ZERO as u16);
        assert_eq!(planes.cr[2], CHROMA_ZERO as u16);
        // Full red: Cr = +0.5 exactly, i.e. 1023.5 → clipped to 1023.
        assert_eq!(loss.clipped_high, 1);
        assert_eq!(planes.cr[3], MAX_CODE as u16);
        // Black is exactly achromatic.
        assert_eq!(planes.y[0], 0);
        assert_eq!(planes.cb[0], CHROMA_ZERO as u16);
        assert_eq!(planes.cr[0], CHROMA_ZERO as u16);
        // Means are the R'G'B' signal's, and skip the non-finite sample.
        assert!((stats.mean[0] - 0.375).abs() < 1e-12, "{:?}", stats.mean);
    }

    #[test]
    fn neutral_input_quantizes_to_flat_chroma_across_the_code_ladder() {
        // The encoder must not tint greys: every achromatic input has to land on
        // the exact achromatic chroma level, not merely near it.
        let mut rgb = Vec::new();
        for code in 0..1024 {
            let v = code as f32 / 1023.0;
            rgb.extend_from_slice(&[v, v, v]);
        }
        let (planes, loss, _) = quantize_to_ycbcr(&rgb, 1024, 1).unwrap();
        assert!(!loss.any_loss(), "{loss:?}");
        assert!(planes.cb.iter().all(|&c| c == CHROMA_ZERO as u16));
        assert!(planes.cr.iter().all(|&c| c == CHROMA_ZERO as u16));
        // Luma is monotonic and spans the full range.
        assert_eq!(planes.y[0], 0);
        assert_eq!(*planes.y.last().unwrap(), MAX_CODE as u16);
        assert!(planes.y.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn quantization_rejects_a_sample_count_that_disagrees_with_the_dimensions() {
        let err = quantize_to_ycbcr(&[0.0; 9], 2, 2).unwrap_err();
        assert!(matches!(err, NcError::Other(_)), "{err:?}");
    }

    #[test]
    fn empty_and_oversized_images_fail_before_any_encoding() {
        assert!(check_encodable_dimensions(0, 4).is_err());
        assert!(check_encodable_dimensions(4, 0).is_err());
        assert!(check_encodable_dimensions(4, 4).is_ok());
        // The bound is libaom's encoder `RANGE_CHECK` (16 bits of frame size), so
        // exactly 65,536 is encodable and one past it is refused *here* — before
        // quantization allocates three full planes for an image libaom will reject.
        assert_eq!(AV1_MAX_DIMENSION, 65_536);
        assert!(check_encodable_dimensions(AV1_MAX_DIMENSION, AV1_MAX_DIMENSION).is_ok());
        for (w, h) in [
            (AV1_MAX_DIMENSION + 1, 4),
            (4, AV1_MAX_DIMENSION + 1),
            // The old bound was `aom_img_alloc`'s 2^27, which let these through.
            (1 << 27, 4),
        ] {
            assert!(
                matches!(
                    check_encodable_dimensions(w, h),
                    Err(NcError::Unsupported(_))
                ),
                "{w}x{h} must be refused as unsupported"
            );
        }
    }

    #[test]
    fn a_codestream_too_large_for_a_32_bit_box_is_refused_including_the_header() {
        // Arithmetic only — the sizes here are far past anything allocatable.
        assert!(check_codestream_addressable(0).is_ok());
        assert!(check_codestream_addressable(MAX_CODESTREAM_BYTES).is_ok());
        // `mdat`'s 32-bit size covers its 8-byte header, so the last addressable
        // codestream is `u32::MAX - 8`; one more would have wrapped the box size to
        // 0 and written a malformed file with no error.
        assert_eq!(MAX_CODESTREAM_BYTES, u32::MAX as usize - 8);
        assert!(matches!(
            check_codestream_addressable(MAX_CODESTREAM_BYTES + 1),
            Err(NcError::Unsupported(_))
        ));
        assert!(check_codestream_addressable(u32::MAX as usize).is_err());
    }

    #[test]
    fn content_light_level_measures_the_frame_instead_of_restating_policy() {
        // The regression that matters: a dark frame must not claim a bright one's
        // peak. Two uniform fields, one 20x darker than the other.
        let bright = render_tiny(hdr::HdrTransfer::Pq, &[1.0; 12], 4, 1);
        let dark = render_tiny(hdr::HdrTransfer::Pq, &[0.05; 12], 4, 1);
        let (bright_cll, dark_cll) = (
            bright.metadata().content_light.max_cll_nits,
            dark.metadata().content_light.max_cll_nits,
        );
        assert!(
            dark_cll < bright_cll,
            "dark frame reported MaxCLL {dark_cll} against the bright frame's {bright_cll}"
        );
        // A uniform field's brightest pixel *is* its average.
        for render in [&bright, &dark] {
            let measured = render.metadata().content_light;
            assert_eq!(
                measured.max_fall_nits, measured.max_cll_nits,
                "{measured:?}"
            );
        }

        // And the measurement is what reaches the file.
        let dir = tempdir();
        let mut written = Vec::new();
        for (name, render) in [("bright", bright), ("dark", dark)] {
            let path = dir.join(format!("clli-{name}.avif"));
            let expected = render.metadata().content_light;
            let (staged, _, _) = encode(render, &path).unwrap();
            staged.commit().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(
                clli_of(&bytes),
                Some((expected.max_cll_nits, expected.max_fall_nits)),
                "{name}"
            );
            written.push(clli_of(&bytes).unwrap());
        }
        assert!(
            written[1].0 < written[0].0,
            "the written clli must differ between a dark and a bright frame: {written:?}"
        );
    }

    #[test]
    fn advanced_profile_is_claimed_only_inside_every_published_limit() {
        let base = SequenceHeader {
            seq_profile: SEQ_PROFILE_HIGH,
            still_picture: true,
            reduced_still_picture_header: true,
            seq_level_idx_0: 0,
            seq_tier_0: 0,
            max_frame_width: 64,
            max_frame_height: 64,
            high_bitdepth: true,
            twelve_bit: false,
            mono_chrome: false,
            color_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 9,
            color_range: true,
            subsampling_x: false,
            subsampling_y: false,
            chroma_sample_position: 0,
        };
        assert_eq!(resolve_profile(&base, 64, 64), AvifProfile::Advanced);
        // Exactly on each boundary is still conforming.
        assert_eq!(
            resolve_profile(&base, ADVANCED_MAX_WIDTH, 2176),
            AvifProfile::Advanced
        );
        // Level 6.1 (index 17) is over the ceiling even though it "looks like 6".
        let over_level = SequenceHeader {
            seq_level_idx_0: ADVANCED_MAX_SEQ_LEVEL_IDX + 1,
            ..base
        };
        assert!(!resolve_profile(&over_level, 64, 64).advertises_advanced());
        // Main Profile cannot carry the Advanced brand.
        let main_profile = SequenceHeader {
            seq_profile: 0,
            ..base
        };
        assert!(!resolve_profile(&main_profile, 64, 64).advertises_advanced());
        // One pixel past each dimension limit.
        assert!(!resolve_profile(&base, ADVANCED_MAX_WIDTH + 1, 64).advertises_advanced());
        assert!(!resolve_profile(&base, 64, ADVANCED_MAX_HEIGHT + 1).advertises_advanced());
        // Within both axes but past the pixel-count limit.
        assert!(!resolve_profile(&base, 16_000, 8_000).advertises_advanced());
    }

    /// Decode an AVIF's codestream with libaom and return its 10-bit planes.
    ///
    /// This shares an implementation with the encoder, so it proves
    /// self-consistency and container navigability — *not* conformance. The
    /// independent dav1d decode the task requires is the codec-bounds step.
    fn decode_codestream(codestream: &[u8], width: u32, height: u32) -> Vec<[u16; 3]> {
        let mut ctx: Box<aom::aom_codec_ctx_t> = Box::new(unsafe { std::mem::zeroed() });
        let cfg = aom::aom_codec_dec_cfg {
            threads: 1,
            w: width,
            h: height,
            allow_lowbitdepth: 0,
        };
        assert_eq!(
            unsafe {
                aom::aom_codec_dec_init_ver(
                    &mut *ctx,
                    aom::aom_codec_av1_dx(),
                    &cfg,
                    0,
                    aom::AOM_DECODER_ABI_VERSION as c_int,
                )
            },
            aom::AOM_CODEC_OK
        );
        assert_eq!(
            unsafe {
                aom::aom_codec_decode(
                    &mut *ctx,
                    codestream.as_ptr(),
                    codestream.len(),
                    std::ptr::null_mut(),
                )
            },
            aom::AOM_CODEC_OK,
            "libaom rejected the codestream"
        );
        let mut iter: aom::aom_codec_iter_t = std::ptr::null();
        let img = unsafe { aom::aom_codec_get_frame(&mut *ctx, &mut iter) };
        assert!(!img.is_null(), "no decoded frame");
        let img = unsafe { &*img };
        assert_eq!((img.d_w, img.d_h), (width, height));
        assert_eq!(img.bit_depth, u32::from(BIT_DEPTH));
        assert_eq!(img.fmt, aom::AOM_IMG_FMT_I44416);

        let mut out = Vec::with_capacity((width * height) as usize);
        for row in 0..height as usize {
            for col in 0..width as usize {
                let sample = |plane: usize| unsafe {
                    let base = img.planes[plane].add(row * img.stride[plane] as usize);
                    *base.cast::<u16>().add(col)
                };
                out.push([sample(0), sample(1), sample(2)]);
            }
        }
        unsafe { aom::aom_codec_destroy(&mut *ctx) };
        out
    }

    #[test]
    fn a_pq_encode_round_trips_through_the_container_and_a_decoder() {
        let dir = tempdir();
        let path = dir.join("pq.avif");
        // A 4x1 strip: black, dark grey, mid grey, near-white. Neutral input, so
        // the decoded chroma must stay at the achromatic level.
        let ramp: Vec<f32> = [0.0_f32, 0.25, 0.5, 0.9]
            .iter()
            .flat_map(|&v| [v, v, v])
            .collect();
        let render = render_tiny(hdr::HdrTransfer::Pq, &ramp, (ramp.len() / 3) as u32, 1);
        let expected = quantize_to_ycbcr(render.image().rgb(), 4, 1).unwrap().0;
        let content_light = render.metadata().content_light;

        let (staged, outcome, summary) = encode(render, &path).unwrap();
        staged.commit().unwrap();
        assert!(!outcome.loss.any_loss(), "{:?}", outcome.loss);

        // The summary reports what was actually coded.
        assert_eq!(summary.profile, AvifProfile::Advanced);
        assert_eq!(summary.seq_profile, SEQ_PROFILE_HIGH);
        assert!(summary.seq_level_idx <= ADVANCED_MAX_SEQ_LEVEL_IDX);
        assert_eq!(summary.cicp, (9, 16, 9));
        assert!(summary.full_range);
        assert_eq!(summary.bit_depth, BIT_DEPTH);

        let bytes = std::fs::read(&path).unwrap();
        let tree = boxes(&bytes);

        // Brands, in the order the specification lists them.
        assert_eq!(&bytes[8..12], b"avif");
        for brand in [b"avif", b"mif1", b"miaf", b"MA1A"] {
            assert!(
                bytes[..32].windows(4).any(|w| w == brand),
                "missing brand {}",
                String::from_utf8_lossy(brand)
            );
        }

        // `ispe` states the real size; `pixi` states three 10-bit channels.
        let (_, _, ispe) = *find(&tree, "ispe");
        assert_eq!(&bytes[ispe + 4..ispe + 12], &[0, 0, 0, 4, 0, 0, 0, 1]);
        let (_, _, pixi) = *find(&tree, "pixi");
        assert_eq!(&bytes[pixi + 4..pixi + 8], &[3, 10, 10, 10]);

        // `colr` carries the renderer's CICP with the full-range flag set.
        let (_, _, colr) = *find(&tree, "colr");
        assert_eq!(&bytes[colr..colr + 4], b"nclx");
        assert_eq!(&bytes[colr + 4..colr + 10], &[0, 9, 0, 16, 0, 9]);
        assert_eq!(bytes[colr + 10], 0x80);

        // `clli` reports what this frame measured, not the renderer's policy: the
        // ramp tops out at an adjusted 0.9, so its peak is well under the 1000-nit
        // mastering ceiling, and its average is lower still.
        assert_eq!(
            clli_of(&bytes),
            Some((content_light.max_cll_nits, content_light.max_fall_nits))
        );
        assert!(
            content_light.max_fall_nits <= content_light.max_cll_nits,
            "{content_light:?}"
        );
        assert!(
            (1..hdr::TARGET_PEAK_NITS as u16).contains(&content_light.max_cll_nits),
            "a ramp under reference white must not claim the mastering peak: \
             {content_light:?}"
        );

        // `av1C` agrees with the codestream: High Profile, 10-bit, 4:4:4.
        let (_, _, av1c) = *find(&tree, "av1C");
        assert_eq!(bytes[av1c], 0x81, "av1C marker/version");
        assert_eq!(bytes[av1c + 1] >> 5, SEQ_PROFILE_HIGH);
        assert_eq!(bytes[av1c + 1] & 0x1F, summary.seq_level_idx);
        assert_eq!((bytes[av1c + 2] >> 6) & 1, 1, "high_bitdepth");
        assert_eq!((bytes[av1c + 2] >> 5) & 1, 0, "twelve_bit");
        assert_eq!(bytes[av1c + 2] & 0x0F, 0, "mono + 4:4:4 subsampling");

        // `iloc` must point at the real `mdat` payload — the one field a
        // hand-written container is most likely to get wrong.
        let (_, mdat_size, mdat_body) = *find(&tree, "mdat");
        let (_, _, iloc) = *find(&tree, "iloc");
        // Layout from the `iloc` body: version+flags (4), offset/length sizes (1),
        // base_offset_size (1), item_count (2), item_ID (2), data_ref_index (2),
        // extent_count (2) — so the extent's offset and length start at +14/+18.
        let extent_offset =
            u32::from_be_bytes(bytes[iloc + 14..iloc + 18].try_into().unwrap()) as usize;
        let extent_length =
            u32::from_be_bytes(bytes[iloc + 18..iloc + 22].try_into().unwrap()) as usize;
        assert_eq!(extent_offset, mdat_body);
        assert_eq!(extent_length, mdat_size - 8);
        assert_eq!(extent_offset + extent_length, bytes.len());

        // The codestream the container points at decodes to the pixels we fed in.
        let decoded = decode_codestream(&bytes[extent_offset..], 4, 1);
        assert_eq!(decoded.len(), 4);
        for (index, [y, cb, cr]) in decoded.iter().copied().enumerate() {
            assert!(
                cb.abs_diff(CHROMA_ZERO as u16) <= 2 && cr.abs_diff(CHROMA_ZERO as u16) <= 2,
                "neutral pixel {index} decoded with chroma ({cb}, {cr})",
            );
            let want = expected.y[index];
            assert!(
                y.abs_diff(want) <= 8,
                "pixel {index} luma decoded as {y}, expected about {want}",
            );
        }
        // Luma order is preserved across the ramp.
        assert!(
            decoded.windows(2).all(|w| w[0][0] <= w[1][0]),
            "decoded ramp is not monotonic: {decoded:?}"
        );
    }

    #[test]
    fn repeated_encodes_of_the_same_input_are_byte_identical() {
        let dir = tempdir();
        let ramp: Vec<f32> = (0..8).flat_map(|i| [i as f32 / 8.0; 3]).collect();
        let mut digests = Vec::new();
        for run in 0..3 {
            let path = dir.join(format!("run{run}.avif"));
            let render = render_tiny(hdr::HdrTransfer::Pq, &ramp, (ramp.len() / 3) as u32, 1);
            let (staged, _, _) = encode(render, &path).unwrap();
            staged.commit().unwrap();
            digests.push(std::fs::read(&path).unwrap());
        }
        assert_eq!(digests[0], digests[1], "run 0 and 1 differ");
        assert_eq!(digests[1], digests[2], "run 1 and 2 differ");
    }

    #[test]
    fn hlg_signals_its_own_cicp_and_omits_content_light_level() {
        let dir = tempdir();
        let path = dir.join("hlg.avif");
        let render = render_tiny(hdr::HdrTransfer::Hlg, &[0.5, 0.5, 0.5, 0.2, 0.2, 0.2], 2, 1);
        let (staged, _, summary) = encode(render, &path).unwrap();
        staged.commit().unwrap();
        // 9/18/9 for HLG, versus PQ's 9/16/9.
        assert_eq!(summary.cicp, (9, 18, 9));
        let bytes = std::fs::read(&path).unwrap();
        let tree = boxes(&bytes);
        let (_, _, colr) = *find(&tree, "colr");
        assert_eq!(&bytes[colr + 4..colr + 10], &[0, 9, 0, 18, 0, 9]);
        // HLG is display-referred, so absolute content-light metadata is omitted
        // rather than invented.
        assert!(
            !tree.iter().any(|(name, _, _)| name == "clli"),
            "HLG must not carry a clli box"
        );
    }

    #[test]
    fn an_uncommitted_staged_avif_never_appears_at_the_destination() {
        // This encode *succeeds*; what is under test is the staging boundary, not a
        // failure path. Dropping the `Staged` stands in for any failure between
        // encoding and commit — none of which can leave bytes at the destination,
        // because nothing is written there until `commit`.
        let dir = tempdir();
        let path = dir.join("dropped.avif");
        let render = render_tiny(hdr::HdrTransfer::Pq, &[0.5, 0.5, 0.5], 1, 1);
        let (staged, _, _) = encode(render, &path).unwrap();
        drop(staged);
        assert!(
            !path.exists(),
            "an uncommitted AVIF must not appear at the final path"
        );
    }

    /// The four fixture classes the task's codec-bounds clause names, as one
    /// 256x64 field: neutral ramp, saturated primary, a two-channel gradient, and a
    /// hard edge.
    fn bounds_field() -> (u32, u32, Vec<f32>) {
        let (w, h) = (256_u32, 64_u32);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let t = x as f32 / (w - 1) as f32;
                let px = match y / 16 {
                    0 => [t, t, t],
                    1 => [t, 0.0, 0.0],
                    2 => [0.0, t, 1.0 - t],
                    _ => {
                        if x < w / 2 {
                            [0.05; 3]
                        } else {
                            [0.95; 3]
                        }
                    }
                };
                rgb.extend_from_slice(&px);
            }
        }
        (w, h, rgb)
    }

    #[test]
    fn decoded_code_error_stays_within_the_pinned_codec_bounds() {
        // AV1 reconstruction is normatively specified and bit-exact, so the decoder
        // cannot change these numbers — libaom here reproduces `avifdec`/dav1d's
        // measurements exactly (asserted below against the dav1d figures recorded
        // in `docs/progress/output.md`). That is what makes an in-repo, CI-runnable
        // decode a legitimate stand-in for the independent one.
        let (w, h, rgb) = bounds_field();
        for (transfer, expected) in [
            // (max, rms) per plane, measured with avifdec/dav1d at CQ_LEVEL = 8.
            (hdr::HdrTransfer::Pq, [(9, 0.702), (10, 0.849), (9, 0.591)]),
            (hdr::HdrTransfer::Hlg, [(8, 0.645), (8, 0.782), (7, 0.615)]),
        ] {
            let render = render_tiny(transfer, &rgb, w, h);
            let want = quantize_to_ycbcr(render.image().rgb(), w, h).unwrap().0;
            let dir = tempdir();
            let path = dir.join(format!("bounds-{transfer:?}.avif"));
            let (staged, _, _) = encode(render, &path).unwrap();
            staged.commit().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let tree = boxes(&bytes);
            let (_, _, iloc) = *find(&tree, "iloc");
            let offset =
                u32::from_be_bytes(bytes[iloc + 14..iloc + 18].try_into().unwrap()) as usize;
            let got = decode_codestream(&bytes[offset..], w, h);

            for (plane, (want_plane, (max_bound, rms_bound))) in [&want.y, &want.cb, &want.cr]
                .into_iter()
                .zip(expected)
                .enumerate()
            {
                let diffs: Vec<u32> = got
                    .iter()
                    .zip(want_plane)
                    .map(|(px, &w)| u32::from(px[plane].abs_diff(w)))
                    .collect();
                let max = *diffs.iter().max().unwrap();
                let rms = (diffs
                    .iter()
                    .map(|d| f64::from(*d) * f64::from(*d))
                    .sum::<f64>()
                    / diffs.len() as f64)
                    .sqrt();
                // Equality, not a loose bound: a normative decoder plus a pinned
                // encoder build makes this exact, so any movement is a real change
                // in what nc ships and should be reviewed, not absorbed.
                assert_eq!(
                    max, max_bound,
                    "{transfer:?} plane {plane}: max code error {max} != pinned {max_bound}",
                );
                assert!(
                    (rms - rms_bound).abs() < 5e-3,
                    "{transfer:?} plane {plane}: RMS {rms:.4} != pinned {rms_bound}",
                );
            }
        }
    }

    #[test]
    fn a_neutral_ramp_survives_encoding_without_acquiring_a_colour_cast() {
        // The perceptually important case for film: greys must stay grey through a
        // lossy chroma-carrying codec, not drift into a tint.
        let (w, h) = (256_u32, 16_u32);
        let rgb: Vec<f32> = (0..h)
            .flat_map(|_| (0..w).flat_map(|x| [x as f32 / (w - 1) as f32; 3]))
            .collect();
        let render = render_tiny(hdr::HdrTransfer::Pq, &rgb, w, h);
        let dir = tempdir();
        let path = dir.join("neutral.avif");
        let (staged, _, _) = encode(render, &path).unwrap();
        staged.commit().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let tree = boxes(&bytes);
        let (_, _, iloc) = *find(&tree, "iloc");
        let offset = u32::from_be_bytes(bytes[iloc + 14..iloc + 18].try_into().unwrap()) as usize;
        let decoded = decode_codestream(&bytes[offset..], w, h);
        let worst = decoded
            .iter()
            .map(|[_, cb, cr]| {
                cb.abs_diff(CHROMA_ZERO as u16)
                    .max(cr.abs_diff(CHROMA_ZERO as u16))
            })
            .max()
            .unwrap();
        // Measured: a pure neutral ramp comes back with chroma exactly at the
        // achromatic level, because flat chroma planes are free for the codec.
        assert_eq!(worst, 0, "neutral ramp picked up {worst} codes of chroma");
    }

    #[test]
    #[ignore = "throwaway: emits an AVIF plus its pre-encode planes for avifdec bounds"]
    fn emit_for_independent_bounds() {
        // Opt-in reproduction path for the independent (`avifdec`/dav1d) measurement
        // behind `decoded_code_error_stays_within_the_pinned_codec_bounds`. Skips
        // rather than fails when run without a destination, so a blanket
        // `cargo test -- --ignored` stays green.
        let Ok(dir) = std::env::var("NC_AVIF_OUT") else {
            eprintln!("set NC_AVIF_OUT=<dir> to emit the bounds fixtures");
            return;
        };
        let out = std::path::PathBuf::from(dir);
        let (w, h, rgb) = bounds_field();
        for (name, transfer) in [
            ("bounds-pq", hdr::HdrTransfer::Pq),
            ("bounds-hlg", hdr::HdrTransfer::Hlg),
        ] {
            let render = render_tiny(transfer, &rgb, w, h);
            let planes = quantize_to_ycbcr(render.image().rgb(), w, h).unwrap().0;
            // Reference y4m of exactly what nc handed the encoder.
            let mut y4m =
                format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A0:0 C444p10\nFRAME\n").into_bytes();
            for plane in [&planes.y, &planes.cb, &planes.cr] {
                for s in plane {
                    y4m.extend_from_slice(&s.to_le_bytes());
                }
            }
            std::fs::write(out.join(format!("{name}.ref.y4m")), &y4m).unwrap();
            let (staged, _, summary) = encode(render, &out.join(format!("{name}.avif"))).unwrap();
            staged.commit().unwrap();
            println!("{name}: {summary:?}");
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "nc-avif-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn general_brand_only_files_omit_ma1a_and_say_why() {
        let profile = AvifProfile::GeneralOnly {
            reason: "test".into(),
        };
        let header = SequenceHeader {
            seq_profile: SEQ_PROFILE_HIGH,
            still_picture: true,
            reduced_still_picture_header: true,
            seq_level_idx_0: 0,
            seq_tier_0: 0,
            max_frame_width: 2,
            max_frame_height: 2,
            high_bitdepth: true,
            twelve_bit: false,
            mono_chrome: false,
            color_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 9,
            color_range: true,
            subsampling_x: false,
            subsampling_y: false,
            chroma_sample_position: 0,
        };
        let bytes = write_container(&[0u8; 8], 2, 2, &header, &pq_metadata(), &profile).unwrap();
        let ftyp_end = 8 + 8 + 4 * 3;
        assert_eq!(&bytes[8..12], b"avif", "major brand");
        assert!(
            !bytes[..ftyp_end].windows(4).any(|w| w == b"MA1A"),
            "MA1A must not appear outside Advanced Profile limits"
        );
        assert!(bytes[..ftyp_end].windows(4).any(|w| w == b"miaf"));
    }
}
