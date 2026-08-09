//! Deterministic Rec.2100 display-HDR rendering from the shared adjusted ACEScg
//! source.
//!
//! This stage owns ACEScg/D60 → BT.2020/D65 rendering, the 203-nit
//! reference-white / 1000-nit peak placement, a luminance-preserving highlight
//! shoulder, neutral-axis gamut mapping, and the Rec.2100 PQ or HLG transfer.
//! AVIF quantization, coding, and container metadata remain downstream.

use serde::Serialize;

use crate::pipeline::colorimetry::definitions::transfer;
use crate::pipeline::colorimetry::pinned::{ACESCG_TO_BT2020, BT2020_LUMA};
use crate::pipeline::render_split::SharedDisplaySource;
use crate::types::{LinearImage, NcError, OutputPreset, Result};

/// Binding display reference white for every named HDR rendition.
pub const REFERENCE_WHITE_NITS: f32 = 203.0;
/// Initial mastering-display peak selected by the HDR output spike.
pub const TARGET_PEAK_NITS: f32 = 1000.0;
/// Linear capacity above reference white: `1000 / 203`.
pub const LINEAR_HEADROOM: f32 = TARGET_PEAK_NITS / REFERENCE_WHITE_NITS;

// The renderer works in f32; every constant below (and the HLG OETF's `a`, in
// `hlg_oetf`) is narrowed from the single f64 definition in `colorimetry`. The PQ
// constants are ratios of small integers and so are exactly representable in both
// widths, 1.2 narrows to the same f32 either way, and the OETF constant's f64 and
// f32 spellings share a bit pattern — the casts cannot perturb a rendered value.
const HLG_SYSTEM_GAMMA: f32 = transfer::HLG_SYSTEM_GAMMA as f32;

/// Rec.2100 transfer function selected for the encoded HDR pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HdrTransfer {
    /// ST 2084 / perceptual quantizer; the primary still-image path.
    Pq,
    /// Hybrid log-gamma with the reference 1000-nit display OOTF.
    Hlg,
}

/// The Rec.2100 transfer a single-rendition display-HDR preset renders.
///
/// `None` for every preset that renders no Rec.2100 signal. This lives here rather
/// than on [`OutputPreset`] deliberately: `types` is the shared-types leaf and must
/// not depend on a pipeline module, while this module already depends on `types`.
///
/// **It answers "which transfer", never "which container".** Two presets share each
/// answer — `hdr-pq` writes AVIF and `hdr-pq-tiff` writes TIFF from the identical
/// rendition — so an orchestrator must not use a `Some(_)` here to pick an encoder.
/// `convert_frame` matches on the preset itself for that, exhaustively, so a new
/// preset cannot silently inherit another's container.
pub fn transfer_for(preset: OutputPreset) -> Option<HdrTransfer> {
    match preset {
        OutputPreset::HdrPq | OutputPreset::HdrPqTiff => Some(HdrTransfer::Pq),
        OutputPreset::HdrHlg | OutputPreset::HdrHlgTiff => Some(HdrTransfer::Hlg),
        // `hdr-linear-tiff` is `None` because it applies **no** transfer at all —
        // it stops at [`render_linear`]. That is a different thing from the presets
        // below, which are not HDR renditions in the first place; both answers are
        // "no transfer", for opposite reasons.
        OutputPreset::HdrLinearTiff
        | OutputPreset::Legacy
        | OutputPreset::FilmMaster
        | OutputPreset::UltraHdrV1 => None,
    }
}

/// Measured content-light levels for one rendered frame, in cd/m².
///
/// **Measured, not policy.** CTA-861.3 — and therefore AVIF's `clli` box —
/// defines these as properties of *this content*: MaxCLL is the brightest pixel
/// and MaxFALL the frame average. Displays tone-map from them, so a dark frame
/// has to report a low peak; nothing here may be derived from the renderer's
/// 1000-nit mastering ceiling or its 203-nit reference white.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ContentLightLevel {
    /// Brightest per-pixel luminance in the frame, as whole cd/m².
    pub max_cll_nits: u16,
    /// Frame-average per-pixel luminance, as whole cd/m².
    pub max_fall_nits: u16,
}

/// Warn when a single-rendition HDR container would carry a signal that never rises
/// above SDR reference white.
///
/// Every HDR preset advertises `target_peak_nits: 1000` in its report, and the
/// container's own signalling (CICP transfer 16/18, the PQ `clli` box) says "HDR".
/// If the rendered frame's brightest pixel measures at or below the 203-nit
/// reference white, all of that is true of the *file* and none of it is true of the
/// *picture*: the result is an HDR wrapper around an SDR-range signal, which costs
/// bit depth and compatibility and buys nothing. CLAUDE.md's fail-loudly rule puts
/// that in the report rather than leaving it to be discovered with `exiftool`.
///
/// The measurement is [`ContentLightLevel::max_cll_nits`], reused exactly as
/// measured for the `clli` box — the same number the artifact advertises, so the
/// warning and the file can never disagree, and no second pass over the frame is
/// needed. It is whole nits by the time it reaches here, which is why the
/// comparison is `<=` against a rounded 203 rather than a float epsilon dance: a
/// frame peaking at 203.4 nits is not meaningfully HDR either.
///
/// `None` for a frame with real highlights — that is the falsifiable half, and the
/// reason this takes the measurement rather than the preset.
pub fn sdr_range_warning(content_light: ContentLightLevel) -> Option<String> {
    let reference_white = REFERENCE_WHITE_NITS.round() as u16;
    (content_light.max_cll_nits <= reference_white).then(|| {
        format!(
            "HDR output carries an SDR-range signal: the brightest pixel measures {} nits, \
             at or below the {reference_white}-nit reference white, so nothing in this frame \
             uses the {:.0}-nit headroom the container and report advertise. Two common \
             causes: the resolved Dmax anchor is too high for this roll, which darkens the \
             whole render — measure the roll's own anchor with `nc estimate --d-max-region` \
             and pass it as `--d-max` — or the frame's content genuinely never reaches the \
             display shoulder, in which case an SDR preset delivers the same picture in a \
             more compatible container.",
            content_light.max_cll_nits, TARGET_PEAK_NITS,
        )
    })
}

/// Policy metadata that describes the pre-transfer linear BT.2020 rendition.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LinearHdrMetadata {
    pub reference_white_nits: f32,
    pub target_peak_nits: f32,
    pub linear_headroom: f32,
    pub highlight_compress: f32,
    pub shoulder_start: f32,
    pub tone_curve: &'static str,
    pub gamut_mapping: &'static str,
    pub linear_domain: &'static str,
}

/// Stable, reportable transfer and signaling contract for an HDR render.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HdrRenderMetadata {
    pub transfer: HdrTransfer,
    pub linear: LinearHdrMetadata,
    /// What this frame's pixels actually measured, for container metadata that
    /// describes content rather than policy (AVIF `clli`).
    pub content_light: ContentLightLevel,
    pub encoded_domain: &'static str,
    pub cicp_color_primaries: u8,
    pub cicp_transfer: u8,
    pub cicp_matrix_coefficients: u8,
    pub full_range: bool,
    pub hlg_system_gamma: Option<f32>,
    pub hlg_reference_display_peak_nits: Option<f32>,
    pub hlg_reference_display_black_nits: Option<f32>,
}

/// Pre-transfer, display-linear BT.2020 pixels paired with their resolved policy.
///
/// Single-rendition output passes this value to [`encode_transfer`] without
/// allocating another full-frame buffer. Gain-map construction may borrow it,
/// but must first transform these BT.2020 pixels into the common linear Display
/// P3 domain shared with the SDR rendition; ratios must never mix primaries.
#[derive(Debug)]
pub struct LinearBt2020Hdr {
    image: LinearImage,
    metadata: LinearHdrMetadata,
    /// Measured here, where the pixels are still reference-white-relative linear
    /// luminance, and carried forward — after the transfer they are nonlinear
    /// codes that no longer state cd/m² directly.
    content_light: ContentLightLevel,
}

impl LinearBt2020Hdr {
    /// Borrow the finite, non-negative, reference-white-relative BT.2020 pixels.
    pub fn image(&self) -> &LinearImage {
        &self.image
    }

    /// Borrow the fully resolved linear rendering policy.
    pub fn metadata(&self) -> &LinearHdrMetadata {
        &self.metadata
    }

    /// The measured content-light levels of these pixels.
    ///
    /// Read before the encode boundary so the orchestrator can check the rendition
    /// against reference white ([`sdr_range_warning`]) without keeping the buffer
    /// alive past [`into_parts`](Self::into_parts) — `hdr-linear-tiff` hands the
    /// image straight to the encoder, so there is no later moment to ask.
    pub fn content_light(&self) -> ContentLightLevel {
        self.content_light
    }

    /// Consume the typed value at the linear-TIFF encoding boundary.
    ///
    /// The mirror of [`RenderedHdr::into_parts`], and it exists for the same
    /// reason: `io::encode::encode_hdr_linear` writes these samples verbatim, so
    /// taking the buffer by value keeps the encode from staging a second
    /// full-frame `f32` image. Encoding from [`image`](Self::image)'s borrow would
    /// silently add ~12 B/px that `pipeline::memory` does not model.
    ///
    /// [`ContentLightLevel`] comes out too: it was measured while these pixels
    /// were still reference-white-relative *linear* luminance, and the linear TIFF
    /// reports it rather than re-deriving it.
    pub(crate) fn into_parts(self) -> (LinearImage, LinearHdrMetadata, ContentLightLevel) {
        (self.image, self.metadata, self.content_light)
    }
}

/// Opaque nonlinear Rec.2100 RGB signal ready for an HDR container encoder.
///
/// This is intentionally not a [`LinearImage`]: PQ/HLG samples are nonlinear,
/// and exposing them through the crate's linear-image type would erase the
/// transfer-domain invariant at the encoder boundary.
#[derive(Debug)]
pub struct EncodedHdrImage {
    width: u32,
    height: u32,
    rgb: Vec<f32>,
}

impl EncodedHdrImage {
    fn from_linear_storage(image: LinearImage) -> Result<Self> {
        if image.ir.is_some() {
            return Err(NcError::Other(
                "encoded HDR image unexpectedly retained an IR plane".to_string(),
            ));
        }
        Ok(Self {
            width: image.width,
            height: image.height,
            rgb: image.rgb,
        })
    }

    /// Encoded image width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Encoded image height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the full-range nonlinear PQ/HLG RGB samples.
    pub fn rgb(&self) -> &[f32] {
        &self.rgb
    }
}

/// Rec.2100-encoded HDR pixels paired inseparably with their signaling contract.
#[derive(Debug)]
pub struct RenderedHdr {
    image: EncodedHdrImage,
    metadata: HdrRenderMetadata,
}

impl RenderedHdr {
    /// Borrow the encoded full-range RGB signal.
    ///
    /// Tests only — this module's and `io::encode`'s, which check the encoded pixels
    /// before handing the pair to an encoder. Production takes the buffer apart with
    /// [`into_parts`](Self::into_parts) instead.
    #[allow(dead_code)]
    pub fn image(&self) -> &EncodedHdrImage {
        &self.image
    }

    /// Borrow the fully resolved rendering and signaling contract.
    ///
    /// **A production call site exists:** `cli::convert_frame` reads
    /// `metadata().transfer` to pick the coded-TIFF ICC profile, deliberately keying
    /// the profile off the transfer that produced the code values rather than off the
    /// preset. Hence no `dead_code` allow here.
    pub fn metadata(&self) -> &HdrRenderMetadata {
        &self.metadata
    }

    /// Consume the typed pair at the AVIF-encoding boundary.
    pub(crate) fn into_parts(self) -> (EncodedHdrImage, HdrRenderMetadata) {
        (self.image, self.metadata)
    }
}

/// Render and transfer-encode the shared adjusted source as Rec.2100 HDR.
pub fn render(
    shared: &SharedDisplaySource,
    transfer: HdrTransfer,
    highlight_compress: f32,
) -> Result<RenderedHdr> {
    encode_transfer(render_linear(shared, highlight_compress)?, transfer)
}

/// Render the shared adjusted source into display-linear BT.2020.
///
/// Adjusted `1.0` remains 203-nit reference white. A bounded Hermite shoulder
/// begins above reference white and reaches the 1000-nit peak with zero slope.
/// Out-of-gamut colour moves radially toward the same-luminance neutral axis,
/// preserving chroma direction instead of clipping channels independently.
///
/// This is also where [`ContentLightLevel`] is measured: the rendered values are
/// BT.2020 luminance relative to reference white, so `dot(rgb, BT2020_LUMA) *
/// REFERENCE_WHITE_NITS` is this pixel's luminance in cd/m².
pub fn render_linear(
    shared: &SharedDisplaySource,
    highlight_compress: f32,
) -> Result<LinearBt2020Hdr> {
    let shoulder_start = shoulder_start(highlight_compress)?;
    let source = shared.source.rgb().as_chunks::<3>().0;
    let mut rgb = Vec::with_capacity(shared.source.rgb().len());
    let mut peak_luminance = 0.0_f32;
    let mut luminance_sum = 0.0_f64;
    for (index, px) in source.iter().enumerate() {
        let rendered = render_pixel_checked(*px, index, shoulder_start)?;
        // Gamut mapping is luminance-preserving, so this is the rendered pixel's
        // luminance whether or not it was moved to the cube boundary.
        let luminance = dot(rendered, BT2020_LUMA).max(0.0);
        peak_luminance = peak_luminance.max(luminance);
        luminance_sum += f64::from(luminance);
        rgb.extend_from_slice(&rendered);
    }
    let content_light = ContentLightLevel {
        max_cll_nits: whole_nits(f64::from(peak_luminance)),
        max_fall_nits: match source.len() {
            0 => 0,
            pixels => whole_nits(luminance_sum / pixels as f64),
        },
    };
    let image = LinearImage::new(shared.source.width(), shared.source.height(), rgb, None)?;
    Ok(LinearBt2020Hdr {
        image,
        content_light,
        metadata: LinearHdrMetadata {
            reference_white_nits: REFERENCE_WHITE_NITS,
            target_peak_nits: TARGET_PEAK_NITS,
            linear_headroom: LINEAR_HEADROOM,
            highlight_compress,
            shoulder_start,
            tone_curve: "reference-white-preserving-hermite-shoulder-v1",
            gamut_mapping: "bt2020-neutral-axis-radial-boundary-v1",
            linear_domain: "bt2020-linear-relative-to-203-nit-reference-white",
        },
    })
}

/// Apply the selected Rec.2100 transfer in place.
///
/// PQ maps absolute display luminance through the inverse ST 2084 EOTF. HLG
/// first applies the inverse reference OOTF for a 1000-nit, zero-black display
/// (`gamma = 1.2`), then the reference OETF. HLG's inverse-OOTF result receives
/// one final neutral-axis boundary intersection in scene-linear BT.2020 so the
/// delivered full-range signal remains representable without channel clipping.
pub fn encode_transfer(mut linear: LinearBt2020Hdr, transfer: HdrTransfer) -> Result<RenderedHdr> {
    for (index, px) in linear
        .image
        .rgb
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .enumerate()
    {
        let encoded = match transfer {
            HdrTransfer::Pq => px.map(|channel| pq_encode_nits(channel * REFERENCE_WHITE_NITS)),
            HdrTransfer::Hlg => {
                let display = px.map(|channel| channel / LINEAR_HEADROOM);
                let scene = hlg_inverse_ootf(display);
                let scene_luminance = dot(scene, BT2020_LUMA);
                gamut_map(scene, scene_luminance, 1.0).map(hlg_oetf)
            }
        };
        if !encoded.iter().all(|value| value.is_finite()) {
            return Err(NcError::Other(format!(
                "HDR transfer encoding produced a non-finite sample at pixel {index}"
            )));
        }
        if !encoded.iter().all(|value| (0.0..=1.0).contains(value)) {
            return Err(NcError::Other(format!(
                "HDR transfer encoding produced an out-of-range sample at pixel {index}"
            )));
        }
        *px = encoded;
    }

    let metadata = HdrRenderMetadata {
        transfer,
        linear: linear.metadata,
        content_light: linear.content_light,
        encoded_domain: match transfer {
            HdrTransfer::Pq => "rec2100-pq-full-range",
            HdrTransfer::Hlg => "rec2100-hlg-full-range-reference-ootf",
        },
        cicp_color_primaries: 9,
        cicp_transfer: match transfer {
            HdrTransfer::Pq => 16,
            HdrTransfer::Hlg => 18,
        },
        cicp_matrix_coefficients: 9,
        full_range: true,
        hlg_system_gamma: (transfer == HdrTransfer::Hlg).then_some(HLG_SYSTEM_GAMMA),
        hlg_reference_display_peak_nits: (transfer == HdrTransfer::Hlg).then_some(TARGET_PEAK_NITS),
        hlg_reference_display_black_nits: (transfer == HdrTransfer::Hlg).then_some(0.0),
    };
    let image = EncodedHdrImage::from_linear_storage(linear.image)?;
    Ok(RenderedHdr { image, metadata })
}

/// Reference-white-relative luminance as whole cd/m², the unit `clli` codes.
///
/// Saturates at `u16::MAX` rather than wrapping. The renderer's own range bound
/// keeps every rendered value at or under the 1000-nit peak, so the saturation is
/// a guard on the type, not a reachable clamp.
fn whole_nits(relative_luminance: f64) -> u16 {
    let nits = relative_luminance * f64::from(REFERENCE_WHITE_NITS);
    if !nits.is_finite() {
        return 0;
    }
    nits.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

fn shoulder_start(highlight_compress: f32) -> Result<f32> {
    if !highlight_compress.is_finite() || highlight_compress < 0.0 {
        return Err(NcError::Usage(format!(
            "print.highlight_compress must be finite and non-negative (got \
             {highlight_compress})"
        )));
    }
    let position = 0.5 + 0.25 / (1.0 + highlight_compress);
    Ok(1.0 + (LINEAR_HEADROOM - 1.0) * position)
}

fn render_pixel_checked(aces: [f32; 3], index: usize, shoulder_start: f32) -> Result<[f32; 3]> {
    if !aces.iter().all(|value| value.is_finite()) {
        return Err(NcError::Other(format!(
            "HDR display rendering received a non-finite ACEScg sample at pixel {index}"
        )));
    }
    let bt2020 = mul(ACESCG_TO_BT2020, aces);
    let luminance = dot(bt2020, BT2020_LUMA);
    if !bt2020.iter().all(|value| value.is_finite()) || !luminance.is_finite() {
        return Err(NcError::Other(format!(
            "HDR display rendering produced a non-finite sample at pixel {index}"
        )));
    }
    let rendered = if luminance <= 0.0 {
        [0.0; 3]
    } else {
        let rendered_luminance = shoulder(luminance, shoulder_start, LINEAR_HEADROOM);
        let scale = rendered_luminance / luminance;
        gamut_map(
            bt2020.map(|channel| channel * scale),
            rendered_luminance,
            LINEAR_HEADROOM,
        )
    };
    if !rendered.iter().all(|value| value.is_finite()) {
        return Err(NcError::Other(format!(
            "HDR display rendering produced a non-finite sample at pixel {index}"
        )));
    }
    if !rendered
        .iter()
        .all(|value| (0.0..=LINEAR_HEADROOM).contains(value))
    {
        return Err(NcError::Other(format!(
            "HDR display rendering produced an out-of-range sample at pixel {index}"
        )));
    }
    Ok(rendered)
}

/// C¹-continuous cubic shoulder from `(start, start, slope=1)` to
/// `(peak, peak, slope=0)`, followed by the declared peak plateau.
fn shoulder(value: f32, start: f32, peak: f32) -> f32 {
    if value <= 0.0 {
        0.0
    } else if value <= start {
        value
    } else if value >= peak {
        peak
    } else {
        let span = peak - start;
        let t = (value - start) / span;
        let t2 = t * t;
        let t3 = t2 * t;
        (2.0 * t3 - 3.0 * t2 + 1.0) * start
            + (t3 - 2.0 * t2 + t) * span
            + (-2.0 * t3 + 3.0 * t2) * peak
    }
}

/// Same-luminance radial mapping to an RGB cube boundary. Every channel uses
/// one common chroma scale, preserving the neutral axis and chroma direction.
fn gamut_map(rgb: [f32; 3], luminance: f32, maximum: f32) -> [f32; 3] {
    let neutral = f64::from(luminance);
    let upper = f64::from(maximum);
    let delta = rgb.map(|channel| f64::from(channel) - neutral);
    let mut chroma_scale = 1.0_f64;
    let mut limiting_boundary = None;
    for (channel, d) in delta.into_iter().enumerate() {
        if d > 0.0 {
            let candidate = (upper - neutral) / d;
            if candidate < chroma_scale {
                chroma_scale = candidate;
                limiting_boundary = Some((channel, maximum));
            }
        } else if d < 0.0 {
            let candidate = -neutral / d;
            if candidate < chroma_scale {
                chroma_scale = candidate;
                limiting_boundary = Some((channel, 0.0));
            }
        }
    }
    let mut output = delta.map(|d| (neutral + chroma_scale * d) as f32);
    if let Some((channel, boundary)) = limiting_boundary {
        output[channel] = boundary;
    }
    output
}

/// ST 2084 inverse EOTF for absolute luminance in cd/m².
fn pq_encode_nits(nits: f32) -> f32 {
    if nits <= 0.0 {
        return 0.0;
    }
    let m1 = transfer::pq::M1 as f32;
    let m2 = transfer::pq::M2 as f32;
    let c1 = transfer::pq::C1 as f32;
    let c2 = transfer::pq::C2 as f32;
    let c3 = transfer::pq::C3 as f32;
    let power = (nits / transfer::pq::PEAK_NITS as f32).powf(m1);
    ((c1 + c2 * power) / (1.0 + c3 * power)).powf(m2)
}

/// PQ EOTF (ST 2084): a code value back to absolute display luminance in cd/m².
///
/// The exact inverse of [`pq_encode_nits`], in **binary64** — it exists to build
/// the tabulated tone curve of the `hdr-pq-tiff` ICC profile, where the values are
/// quantized to 16 bits once at the end, so carrying `f32` rounding through the
/// intermediate powers would waste precision for nothing. It is deliberately not
/// used on any pixel path: the renderer encodes, it never decodes.
pub fn pq_decode_nits(code: f64) -> f64 {
    if code <= 0.0 {
        return 0.0;
    }
    let power = code.powf(1.0 / transfer::pq::M2);
    // ST 2084's own `max(0, …)`, and it guards the **low** end, not the high one:
    // `power` falls below `C1` for codes under ≈7.31e-7, and without the clamp that
    // negative base to a fractional power would be NaN.
    //
    // Codes above 1.0 are **out of contract** and not made safe by this — the only
    // caller is the ICC table builder over `[0, 1]`. Above 1.0 the numerator is
    // positive so the clamp does nothing, the result is nonsense long before that
    // matters (code 1.5 decodes to ≈3.1e6 cd/m²), and the *denominator*
    // `C2 - C3·power` crosses zero at code ≈1.99206, past which this returns NaN.
    let numerator = (power - transfer::pq::C1).max(0.0);
    let denominator = transfer::pq::C2 - transfer::pq::C3 * power;
    transfer::pq::PEAK_NITS * (numerator / denominator).powf(1.0 / transfer::pq::M1)
}

/// Inverse of the BT.2100 HLG reference OETF: a code value back to normalized
/// scene linear.
///
/// Binary64 for the same reason as [`pq_decode_nits`], and used for the same one
/// thing: the `hdr-hlg-tiff` profile's tone curve. Note what it deliberately does
/// **not** include — the OOTF. That is why the HLG profile is scene-referred; see
/// `color::hdr_hlg_tiff_icc`.
pub fn hlg_decode_scene(code: f64) -> f64 {
    if code <= 0.0 {
        0.0
    } else if code <= 0.5 {
        code * code / 3.0
    } else {
        let a = transfer::hlg::OETF_A;
        let b = 1.0 - 4.0 * a;
        let c = 0.5 - a * (4.0 * a).ln();
        (((code - c) / a).exp() + b) / 12.0
    }
}

/// The HLG signal level this renderer produces for 203-nit reference white.
///
/// ≈0.75, which is BT.2100's nominal diffuse-white signal — but it is *computed*
/// from the shipped OOTF and OETF rather than asserted, so it cannot drift away
/// from what the renderer actually writes. Both the `hdr-hlg-tiff` profile (which
/// anchors its PCS here) and
/// `hlg_inverse_ootf_places_203_nit_reference_white_near_signal_075` read it, so
/// the profile's anchor and the renderer's output are the same number by
/// construction.
pub fn hlg_reference_white_signal() -> f32 {
    let display = [REFERENCE_WHITE_NITS / TARGET_PEAK_NITS; 3];
    hlg_oetf(hlg_inverse_ootf(display)[0])
}

/// Inverse of the BT.2100 HLG reference OOTF for a 1000-nit display with a
/// zero-black reference model.
fn hlg_inverse_ootf(display_linear: [f32; 3]) -> [f32; 3] {
    let display_luminance = dot(display_linear, BT2020_LUMA);
    if display_luminance <= 0.0 {
        return [0.0; 3];
    }
    let scale = display_luminance.powf((1.0 - HLG_SYSTEM_GAMMA) / HLG_SYSTEM_GAMMA);
    display_linear.map(|channel| channel * scale)
}

/// BT.2100 HLG reference OETF.
fn hlg_oetf(scene_linear: f32) -> f32 {
    if scene_linear <= 0.0 {
        0.0
    } else if scene_linear <= 1.0 / 12.0 {
        (3.0 * scene_linear).sqrt()
    } else {
        // `b` and `c` are defined by the standard in terms of `a`, so they are
        // derived here rather than recorded as separate definitions.
        const A: f32 = transfer::hlg::OETF_A as f32;
        let b = 1.0 - 4.0 * A;
        let c = 0.5 - A * (4.0 * A).ln();
        A * (12.0 * scene_linear - b).ln() + c
    }
}

fn mul(matrix: [[f32; 3]; 3], value: [f32; 3]) -> [f32; 3] {
    [
        dot(matrix[0], value),
        dot(matrix[1], value),
        dot(matrix[2], value),
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

// AP1/D60 → XYZ, Bradford D60→D65, then XYZ → BT.2020, plus the BT.2020 luma
// weights. Both are defined once in `colorimetry::pinned` (imported above) with
// their standards provenance; reviewed checked-in constants keep rendering
// independent of an installed ICC profile or CMM.
//
// Note the two have different provenance *kinds*: the matrix is derived from
// primaries, while the luma vector is transcribed from BT.2020's table and
// deliberately does not match a derivation.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct;
    use crate::pipeline::render_split::{SharedDisplaySource, display_source};
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::{FilmBase, LinearImage, PrintParams, Reconstruction};

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 2e-5, "{actual} != {expected}");
    }

    fn shared_from_film_rgb(rgb: &[f32]) -> SharedDisplaySource {
        let scan = rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new((rgb.len() / 3) as u32, 1, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        display_source(map_nc_film_rgb_v1(film), &PrintParams::default()).unwrap()
    }

    #[test]
    fn pq_vectors_match_bt2100_inverse_eotf() {
        for (nits, expected) in [
            (0.0, 0.0),
            (100.0, 0.508_078_4),
            (203.0, 0.580_688_9),
            (1000.0, 0.751_827_1),
            (10_000.0, 1.0),
        ] {
            close(pq_encode_nits(nits), expected);
        }
    }

    #[test]
    fn hlg_oetf_vectors_match_bt2100() {
        for (scene_linear, expected) in [(0.0, 0.0), (1.0 / 12.0, 0.5), (1.0, 1.0)] {
            close(hlg_oetf(scene_linear), expected);
        }
    }

    #[test]
    fn hlg_inverse_ootf_places_203_nit_reference_white_near_signal_075() {
        let display = [REFERENCE_WHITE_NITS / TARGET_PEAK_NITS; 3];
        let scene = hlg_inverse_ootf(display);
        let encoded = scene.map(hlg_oetf);
        close(encoded[0], 0.749_877_4);
        close(encoded[0], encoded[1]);
        close(encoded[1], encoded[2]);
        // The `hdr-hlg-tiff` profile anchors its PCS on this exact value, so the
        // accessor it reads must be the same number this test pins.
        close(hlg_reference_white_signal(), encoded[0]);
    }

    #[test]
    fn transfer_decoders_invert_their_encoders() {
        // The ICC tone curves are built from these inverses, so a sign or constant
        // error would silently mis-describe every coded TIFF. Checked against the
        // shipped *forward* functions rather than against restated constants.
        // 5e-5 relative, and the bound is the *forward* function's precision, not
        // the decoder's: `pq_encode_nits` computes in `f32`, and PQ's log-log slope
        // at the dark end amplifies its ~6e-8 relative rounding by ~190x, giving
        // ~1.1e-5 at 0.1 nits (measured). Tightening this would be asserting that
        // the shipped encoder has more precision than it does.
        for nits in [
            0.1_f32,
            1.0,
            10.0,
            100.0,
            REFERENCE_WHITE_NITS,
            500.0,
            TARGET_PEAK_NITS,
        ] {
            let code = pq_encode_nits(nits);
            let back = pq_decode_nits(f64::from(code));
            let error = (back - f64::from(nits)).abs() / f64::from(nits);
            assert!(
                error < 5e-5,
                "PQ round trip at {nits} nits: code {code} decoded to {back} (rel {error:.2e})"
            );
        }
        for scene in [0.0_f32, 1.0 / 12.0, 0.1, 0.26, 0.5, 1.0] {
            let code = hlg_oetf(scene);
            let back = hlg_decode_scene(f64::from(code)) as f32;
            assert!(
                (back - scene).abs() < 1e-5,
                "HLG round trip at scene {scene}: code {code} decoded to {back}"
            );
        }
        // The piecewise join: the standard splits at scene 1/12, i.e. code 0.5.
        assert!((hlg_decode_scene(0.5) - 1.0 / 12.0).abs() < 1e-12);
    }

    #[test]
    fn neutral_ramp_is_neutral_monotonic_and_pins_black_white_and_peak() {
        for transfer in [HdrTransfer::Pq, HdrTransfer::Hlg] {
            let shared = shared_from_film_rgb(&[0.0, 0.0, 0.0, 0.18, 0.18, 0.18, 1.0, 1.0, 1.0]);
            let rendered = render(&shared, transfer, 0.0).unwrap();
            let pixels = rendered.image().rgb().as_chunks::<3>().0;
            for px in pixels {
                close(px[0], px[1]);
                close(px[1], px[2]);
            }
            assert!(pixels.windows(2).all(|pair| pair[0][0] <= pair[1][0]));
            close(pixels[0][0], 0.0);
            close(
                pixels[2][0],
                match transfer {
                    HdrTransfer::Pq => 0.580_688_9,
                    HdrTransfer::Hlg => 0.749_877_4,
                },
            );
        }
    }

    #[test]
    fn highlight_shoulder_is_monotonic_and_reaches_declared_peak() {
        let peak = LINEAR_HEADROOM;
        let start = shoulder_start(0.0).unwrap();
        let samples = [
            shoulder(start, start, peak),
            shoulder((start + peak) * 0.5, start, peak),
            shoulder(peak - 0.01, start, peak),
            shoulder(peak, start, peak),
            shoulder(peak * 2.0, start, peak),
        ];
        assert!(samples.windows(2).all(|pair| pair[0] <= pair[1]));
        close(samples[0], start);
        close(samples[3], peak);
        close(samples[4], peak);
    }

    #[test]
    fn rendered_peak_lands_at_1000_nits_in_both_transfer_systems() {
        let shared = shared_from_film_rgb(&[LINEAR_HEADROOM; 3]);
        let pq = render(&shared, HdrTransfer::Pq, 0.0).unwrap();
        let hlg = render(&shared, HdrTransfer::Hlg, 0.0).unwrap();
        close(pq.image().rgb()[0], 0.751_827_1);
        close(hlg.image().rgb()[0], 1.0);
    }

    #[test]
    fn out_of_gamut_color_uses_one_chroma_scale_at_constant_luminance() {
        let input = [-0.4, 1.5, 0.3];
        let luminance = dot(input, BT2020_LUMA);
        let output = gamut_map(input, luminance, LINEAR_HEADROOM);
        close(dot(output, BT2020_LUMA), luminance);
        let before = input.map(|channel| channel - luminance);
        let after = output.map(|channel| channel - luminance);
        let scales = [
            after[0] / before[0],
            after[1] / before[1],
            after[2] / before[2],
        ];
        close(scales[0], scales[1]);
        close(scales[1], scales[2]);
        assert!(
            output
                .iter()
                .all(|value| (0.0..=LINEAR_HEADROOM).contains(value))
        );
    }

    #[test]
    fn golden_vectors_pin_pq_and_hlg_renditions() {
        let shared = shared_from_film_rgb(&[0.42, 0.18, 0.07]);
        let pq = render(&shared, HdrTransfer::Pq, 0.0).unwrap();
        let hlg = render(&shared, HdrTransfer::Hlg, 0.0).unwrap();
        for (actual, expected) in
            pq.image()
                .rgb()
                .iter()
                .copied()
                .zip([0.467_198_4, 0.418_472_56, 0.344_785_96])
        {
            close(actual, expected);
        }
        for (actual, expected) in
            hlg.image()
                .rgb()
                .iter()
                .copied()
                .zip([0.567_732_45, 0.446_373_76, 0.295_179_46])
        {
            close(actual, expected);
        }
    }

    #[test]
    fn public_renderer_is_deterministic_and_reports_transfer_contract() {
        let shared = shared_from_film_rgb(&[0.18, 0.18, 0.18, 3.0, 0.5, 0.1]);
        for (transfer, cicp_transfer) in [(HdrTransfer::Pq, 16), (HdrTransfer::Hlg, 18)] {
            let first = render(&shared, transfer, 0.4).unwrap();
            let second = render(&shared, transfer, 0.4).unwrap();
            assert_eq!(
                first
                    .image()
                    .rgb()
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                second
                    .image()
                    .rgb()
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            );
            assert_eq!(first.metadata(), second.metadata());
            assert_eq!(first.metadata().transfer, transfer);
            assert_eq!(first.metadata().cicp_color_primaries, 9);
            assert_eq!(first.metadata().cicp_transfer, cicp_transfer);
            assert_eq!(first.metadata().cicp_matrix_coefficients, 9);
            assert!(first.metadata().full_range);
        }
    }

    #[test]
    fn content_light_is_measured_from_the_frame_in_absolute_nits() {
        // Black, reference white, and the mastering peak in one row: rendered
        // luminance is 0, 1 and LINEAR_HEADROOM relative to reference white, so the
        // measurement must read 0, 203 and 1000 cd/m² — and the frame average is the
        // mean of those three, 401.
        let shared = shared_from_film_rgb(&[
            0.0,
            0.0,
            0.0,
            1.0,
            1.0,
            1.0,
            LINEAR_HEADROOM,
            LINEAR_HEADROOM,
            LINEAR_HEADROOM,
        ]);
        let measured = render_linear(&shared, 0.0).unwrap().content_light;
        assert_eq!(measured.max_cll_nits, TARGET_PEAK_NITS as u16);
        assert_eq!(measured.max_fall_nits, 401);
        assert!(measured.max_fall_nits <= measured.max_cll_nits);

        // A dark frame reports a dark frame's numbers. This is the whole point:
        // nothing may be inherited from the 1000-nit ceiling.
        let dark = render_linear(&shared_from_film_rgb(&[0.05; 3]), 0.0)
            .unwrap()
            .content_light;
        let bright = render_linear(&shared_from_film_rgb(&[1.0; 3]), 0.0)
            .unwrap()
            .content_light;
        assert_eq!(bright.max_cll_nits, REFERENCE_WHITE_NITS as u16);
        assert_eq!(dark.max_cll_nits, 10, "0.05 x 203 nits rounds to 10");
        assert!(dark.max_cll_nits < bright.max_cll_nits);
        // A uniform frame's peak is its average, in both transfer systems, and the
        // measurement survives the transfer encode unchanged.
        for transfer in [HdrTransfer::Pq, HdrTransfer::Hlg] {
            let rendered = render(&shared_from_film_rgb(&[0.05; 3]), transfer, 0.0).unwrap();
            assert_eq!(rendered.metadata().content_light, dark);
            assert_eq!(dark.max_fall_nits, dark.max_cll_nits);
        }
    }

    #[test]
    fn sdr_range_warning_fires_on_the_measurement_not_on_the_preset() {
        // A frame rendered exactly at reference white uses none of the headroom: the
        // container says 1000 nits, the picture says 203. `<=` is deliberate — a
        // signal that only *reaches* reference white has no HDR content either.
        let at_white = render_linear(&shared_from_film_rgb(&[1.0; 3]), 0.0)
            .unwrap()
            .content_light();
        assert_eq!(at_white.max_cll_nits, REFERENCE_WHITE_NITS as u16);
        let message = sdr_range_warning(at_white).expect("a 203-nit peak must warn");
        assert!(message.contains("203"), "{message}");
        assert!(message.contains("--d-max-region"), "{message}");

        // Darker still, obviously.
        let dark = render_linear(&shared_from_film_rgb(&[0.05; 3]), 0.0)
            .unwrap()
            .content_light();
        assert!(sdr_range_warning(dark).is_some());

        // The falsifiable half: one pixel above reference white silences it, so the
        // warning tracks the frame rather than the preset. `shoulder_start` for
        // `highlight_compress = 0` sits below the peak, so this renders above 1.0.
        let bright = render_linear(
            &shared_from_film_rgb(&[
                0.0,
                0.0,
                0.0,
                LINEAR_HEADROOM,
                LINEAR_HEADROOM,
                LINEAR_HEADROOM,
            ]),
            0.0,
        )
        .unwrap()
        .content_light();
        assert!(bright.max_cll_nits > REFERENCE_WHITE_NITS as u16);
        assert_eq!(sdr_range_warning(bright), None);

        // The transfer encode carries the same measurement, so PQ and HLG renditions
        // reach the identical verdict from the identical number.
        for transfer in [HdrTransfer::Pq, HdrTransfer::Hlg] {
            let rendered = render(&shared_from_film_rgb(&[0.05; 3]), transfer, 0.0).unwrap();
            assert!(sdr_range_warning(rendered.metadata().content_light).is_some());
        }
    }

    #[test]
    fn hlg_metadata_pins_reference_display_assumptions() {
        let shared = shared_from_film_rgb(&[1.0; 3]);
        let rendered = render(&shared, HdrTransfer::Hlg, 0.0).unwrap();
        assert_eq!(rendered.metadata().hlg_system_gamma, Some(1.2));
        assert_eq!(
            rendered.metadata().hlg_reference_display_peak_nits,
            Some(1000.0)
        );
        assert_eq!(
            rendered.metadata().hlg_reference_display_black_nits,
            Some(0.0)
        );
    }

    #[test]
    fn linear_seam_pins_domain_dimensions_reference_white_headroom_and_policy() {
        let shared = shared_from_film_rgb(&[
            0.0,
            0.0,
            0.0,
            1.0,
            1.0,
            1.0,
            LINEAR_HEADROOM,
            LINEAR_HEADROOM,
            LINEAR_HEADROOM,
        ]);
        let rendered = render_linear(&shared, 0.0).unwrap();
        assert_eq!(rendered.image().width, 3);
        assert_eq!(rendered.image().height, 1);
        assert_eq!(rendered.image().rgb.len(), 9);
        assert!(rendered.image().ir.is_none());
        assert!(
            rendered
                .image()
                .rgb
                .iter()
                .all(|value| value.is_finite() && (0.0..=LINEAR_HEADROOM).contains(value))
        );
        for channel in &rendered.image().rgb[3..6] {
            close(*channel, 1.0);
        }
        for channel in &rendered.image().rgb[6..9] {
            close(*channel, LINEAR_HEADROOM);
        }
        assert_eq!(rendered.metadata().reference_white_nits, 203.0);
        assert_eq!(rendered.metadata().target_peak_nits, 1000.0);
        close(rendered.metadata().linear_headroom, 1000.0 / 203.0);
        assert_eq!(
            rendered.metadata().linear_domain,
            "bt2020-linear-relative-to-203-nit-reference-white"
        );
        assert_eq!(
            rendered.metadata().tone_curve,
            "reference-white-preserving-hermite-shoulder-v1"
        );
        assert_eq!(
            rendered.metadata().gamut_mapping,
            "bt2020-neutral-axis-radial-boundary-v1"
        );
    }

    #[test]
    fn colored_vector_pins_independently_derived_acescg_to_bt2020_conversion() {
        // Expected BT.2020 values were independently derived in binary64 from
        // the published AP1/D60 and BT.2020/D65 chromaticities
        // with Bradford adaptation, rather than from ACESCG_TO_BT2020.
        let actual = mul(ACESCG_TO_BT2020, [0.18, 0.42, 0.73]);
        for (actual, expected) in actual
            .into_iter()
            .zip([0.172_012_9, 0.419_807_08, 0.740_597_3])
        {
            close(actual, expected);
        }
    }

    #[test]
    fn positive_highlight_compress_moves_hdr_knee_without_moving_white_or_peak() {
        let shared = shared_from_film_rgb(&[
            1.0,
            1.0,
            1.0,
            3.5,
            3.5,
            3.5,
            LINEAR_HEADROOM,
            LINEAR_HEADROOM,
            LINEAR_HEADROOM,
        ]);
        let baseline = render_linear(&shared, 0.0).unwrap();
        let compressed = render_linear(&shared, 3.0).unwrap();
        assert!(compressed.metadata().shoulder_start < baseline.metadata().shoulder_start);
        for rendered in [&baseline, &compressed] {
            assert_eq!(
                rendered.metadata().reference_white_nits,
                REFERENCE_WHITE_NITS
            );
            assert_eq!(rendered.metadata().target_peak_nits, TARGET_PEAK_NITS);
            for channel in &rendered.image().rgb[0..3] {
                close(*channel, 1.0);
            }
            for channel in &rendered.image().rgb[6..9] {
                close(*channel, LINEAR_HEADROOM);
            }
        }
        assert_ne!(
            baseline.image().rgb[3].to_bits(),
            compressed.image().rgb[3].to_bits()
        );
    }

    #[test]
    fn negative_or_non_finite_highlight_control_is_rejected() {
        let shared = shared_from_film_rgb(&[1.0; 3]);
        for invalid in [-1.0, f32::NAN, f32::INFINITY] {
            let err = render(&shared, HdrTransfer::Pq, invalid).unwrap_err();
            assert!(matches!(err, NcError::Usage(_)), "{err}");
        }
    }

    #[test]
    fn non_finite_input_fails_with_pixel_index() {
        let shared = shared_from_film_rgb(&[f32::NAN, 0.2, 0.3]);
        let err = render(&shared, HdrTransfer::Pq, 0.0).unwrap_err();
        assert!(err.to_string().contains("pixel 0"), "{err}");
        assert!(err.to_string().contains("non-finite"), "{err}");
    }
}
