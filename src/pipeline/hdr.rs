//! Deterministic Rec.2100 display-HDR rendering from the shared adjusted ACEScg
//! source.
//!
//! This stage owns ACEScg/D60 → BT.2020/D65 rendering, the 203-nit
//! reference-white / 1000-nit peak placement, a luminance-preserving highlight
//! shoulder, neutral-axis gamut mapping, and the Rec.2100 PQ or HLG transfer.
//! AVIF quantization, coding, and container metadata remain downstream.

use serde::Serialize;

use crate::pipeline::render_split::SharedDisplaySource;
use crate::types::{LinearImage, NcError, Result};

/// Binding display reference white for every named HDR rendition.
pub const REFERENCE_WHITE_NITS: f32 = 203.0;
/// Initial mastering-display peak selected by the HDR output spike.
pub const TARGET_PEAK_NITS: f32 = 1000.0;
/// Linear capacity above reference white: `1000 / 203`.
pub const LINEAR_HEADROOM: f32 = TARGET_PEAK_NITS / REFERENCE_WHITE_NITS;

const HLG_SYSTEM_GAMMA: f32 = 1.2;
const BT2020_LUMA: [f32; 3] = [0.2627, 0.6780, 0.0593];

/// Rec.2100 transfer function selected for the encoded HDR pixels.
#[allow(dead_code)] // variants become product-reachable with `output/presets`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HdrTransfer {
    /// ST 2084 / perceptual quantizer; the primary still-image path.
    Pq,
    /// Hybrid log-gamma with the reference 1000-nit display OOTF.
    Hlg,
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
}

impl LinearBt2020Hdr {
    /// Borrow the finite, non-negative, reference-white-relative BT.2020 pixels.
    #[allow(dead_code)] // consumed next by `output/gain-map-hdr-output`.
    pub fn image(&self) -> &LinearImage {
        &self.image
    }

    /// Borrow the fully resolved linear rendering policy.
    #[allow(dead_code)] // consumed next by output report wiring.
    pub fn metadata(&self) -> &LinearHdrMetadata {
        &self.metadata
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
    #[allow(dead_code)] // consumed next by `output/hdr-avif-output`.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Encoded image height in pixels.
    #[allow(dead_code)] // consumed next by `output/hdr-avif-output`.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the full-range nonlinear PQ/HLG RGB samples.
    #[allow(dead_code)] // consumed next by `output/hdr-avif-output`.
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
    #[allow(dead_code)] // consumed next by `output/hdr-avif-output`.
    pub fn image(&self) -> &EncodedHdrImage {
        &self.image
    }

    /// Borrow the fully resolved rendering and signaling contract.
    #[allow(dead_code)] // consumed next by output report wiring.
    pub fn metadata(&self) -> &HdrRenderMetadata {
        &self.metadata
    }

    /// Consume the typed pair at the AVIF-encoding boundary.
    #[allow(dead_code)] // consumed next by `output/hdr-avif-output`.
    pub(crate) fn into_parts(self) -> (EncodedHdrImage, HdrRenderMetadata) {
        (self.image, self.metadata)
    }
}

/// Render and transfer-encode the shared adjusted source as Rec.2100 HDR.
#[allow(dead_code)] // activated by `output/presets`; currently exercised as a pure stage.
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
#[allow(dead_code)] // consumed next by `output/gain-map-hdr-output`.
pub fn render_linear(
    shared: &SharedDisplaySource,
    highlight_compress: f32,
) -> Result<LinearBt2020Hdr> {
    let shoulder_start = shoulder_start(highlight_compress)?;
    let mut rgb = Vec::with_capacity(shared.source.rgb().len());
    for (index, px) in shared.source.rgb().as_chunks::<3>().0.iter().enumerate() {
        let rendered = render_pixel_checked(*px, index, shoulder_start)?;
        rgb.extend_from_slice(&rendered);
    }
    let image = LinearImage::new(shared.source.width(), shared.source.height(), rgb, None)?;
    Ok(LinearBt2020Hdr {
        image,
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
#[allow(dead_code)] // consumed next by `output/hdr-avif-output`.
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
    let m1 = 2610.0 / 16_384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let power = (nits / 10_000.0).powf(m1);
    ((c1 + c2 * power) / (1.0 + c3 * power)).powf(m2)
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
        const A: f32 = 0.178_832_77;
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

// AP1/D60 → XYZ, Bradford D60→D65, then XYZ → BT.2020. Pinned constants
// keep rendering independent of an installed ICC profile or CMM.
const ACESCG_TO_BT2020: [[f32; 3]; 3] = [
    [1.025_824_8, -0.020_053_191, -0.005_771_557],
    [-0.002_234_369_5, 1.004_586_5, -0.002_352_132_5],
    [-0.005_013_351_4, -0.025_290_072, 1.030_303_5],
];

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
