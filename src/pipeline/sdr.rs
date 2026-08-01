//! Deterministic SDR display rendering from the shared adjusted ACEScg source.
//!
//! This stage owns the display rendering only: ACEScg/D60 → destination-linear
//! RGB, a reference-white-preserving shoulder, and neutral-axis gamut mapping.
//! Transfer encoding and ICC attachment remain in [`super::color`].

use serde::Serialize;

use crate::pipeline::colorimetry::pinned::{
    ACESCG_TO_DISPLAY_P3, ACESCG_TO_SRGB, DISPLAY_P3_LUMA, SRGB_LUMA,
};
use crate::pipeline::render_split::SharedDisplaySource;
use crate::types::{LinearImage, NcError, Result};

/// The two SDR destination gamuts. Both use D65 and are returned linear.
#[allow(dead_code)] // variants become product-reachable with `output/presets`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SdrGamut {
    DisplayP3,
    SRgb,
}

/// Stable, reportable policy metadata for an SDR render.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SdrRenderMetadata {
    pub gamut: SdrGamut,
    pub reference_white_nits: f32,
    pub highlight_compress: f32,
    pub shoulder_start: f32,
    pub tone_curve: &'static str,
    pub gamut_mapping: &'static str,
    pub linear_domain: &'static str,
    pub required_transfer: &'static str,
    pub required_profile: &'static str,
}

/// Rendered-linear SDR pixels paired inseparably with their resolved policy.
///
/// Keeping the fields private prevents destination encoding from tagging P3
/// pixels as sRGB (or vice versa). Gain-map construction may borrow the
/// pre-transfer pixels; destination encoding consumes the pair and derives its
/// profile solely from the metadata.
#[derive(Debug)]
pub struct RenderedSdr {
    image: LinearImage,
    metadata: SdrRenderMetadata,
}

impl RenderedSdr {
    /// Borrow the pre-transfer, destination-linear rendition.
    #[allow(dead_code)] // borrowed next by the pre-container gain-map seam.
    pub fn image(&self) -> &LinearImage {
        &self.image
    }

    /// Borrow the fully resolved rendering policy for reporting.
    #[allow(dead_code)] // consumed next by `output/presets` report wiring.
    pub fn metadata(&self) -> &SdrRenderMetadata {
        &self.metadata
    }

    /// Consume the typed pair at the destination-encoding boundary.
    #[allow(dead_code)] // consumed next by output container activation.
    pub(crate) fn into_parts(self) -> (LinearImage, SdrRenderMetadata) {
        (self.image, self.metadata)
    }
}

/// Render the shared adjusted source into destination-linear SDR.
///
/// Adjusted `1.0` is reference white (203 cd/m²). The shoulder begins below
/// reference white and lands at `1.0` with zero slope; values above reference
/// white remain at the display peak. Out-of-gamut colour is moved radially
/// toward the same-luminance neutral axis until it reaches the destination
/// gamut boundary, rather than clipping channels independently.
#[allow(dead_code)] // activated by `output/presets`; currently exercised as a pure stage.
pub fn render(
    shared: &SharedDisplaySource,
    gamut: SdrGamut,
    highlight_compress: f32,
) -> Result<RenderedSdr> {
    if !highlight_compress.is_finite() || highlight_compress < 0.0 {
        return Err(NcError::Usage(format!(
            "print.highlight_compress must be finite and non-negative (got \
             {highlight_compress})"
        )));
    }
    // Named SDR always has a baseline shoulder. The optional control moves the
    // knee earlier, but is bounded to [0.5, 0.75] so even a huge finite value
    // cannot flatten the whole tonal range. For a huge finite f32, adding one
    // rounds back to that same huge finite value, so the reciprocal term
    // stably tends toward zero.
    let shoulder_start = 0.5 + 0.25 / (1.0 + highlight_compress);
    let mut rgb = Vec::with_capacity(shared.source.rgb().len());
    for (index, px) in shared.source.rgb().as_chunks::<3>().0.iter().enumerate() {
        let rendered = render_pixel_checked(*px, index, gamut, shoulder_start)?;
        rgb.extend_from_slice(&rendered);
    }
    let image = LinearImage::new(shared.source.width(), shared.source.height(), rgb, None)?;
    Ok(RenderedSdr {
        image,
        metadata: SdrRenderMetadata {
            gamut,
            reference_white_nits: 203.0,
            highlight_compress,
            shoulder_start,
            tone_curve: "reference-white-hermite-shoulder-v1",
            gamut_mapping: "neutral-axis-radial-boundary-v1",
            linear_domain: "display-linear-relative-to-203-nit-reference-white",
            required_transfer: "srgb",
            required_profile: match gamut {
                SdrGamut::DisplayP3 => "display-p3",
                SdrGamut::SRgb => "srgb",
            },
        },
    })
}

fn render_pixel_checked(
    aces: [f32; 3],
    index: usize,
    gamut: SdrGamut,
    shoulder_start: f32,
) -> Result<[f32; 3]> {
    if !aces.iter().all(|value| value.is_finite()) {
        return Err(NcError::Other(format!(
            "SDR display rendering received a non-finite ACEScg sample at pixel {index}"
        )));
    }
    let (rgb, weights) = destination_rgb(aces, gamut);
    let luminance = dot(rgb, weights);
    if !rgb.iter().all(|value| value.is_finite()) || !luminance.is_finite() {
        return Err(NcError::Other(format!(
            "SDR display rendering produced a non-finite sample at pixel {index}"
        )));
    }
    let rendered = render_destination_pixel(rgb, luminance, shoulder_start);
    if !rendered.iter().all(|value| value.is_finite()) {
        return Err(NcError::Other(format!(
            "SDR display rendering produced a non-finite sample at pixel {index}"
        )));
    }
    if !rendered.iter().all(|value| (0.0..=1.0).contains(value)) {
        return Err(NcError::Other(format!(
            "SDR display rendering produced an out-of-range sample at pixel {index}"
        )));
    }
    Ok(rendered)
}

#[cfg(test)]
fn render_pixel(aces: [f32; 3], gamut: SdrGamut, shoulder_start: f32) -> [f32; 3] {
    let (rgb, weights) = destination_rgb(aces, gamut);
    render_destination_pixel(rgb, dot(rgb, weights), shoulder_start)
}

fn destination_rgb(aces: [f32; 3], gamut: SdrGamut) -> ([f32; 3], [f32; 3]) {
    let matrix = match gamut {
        SdrGamut::DisplayP3 => ACESCG_TO_DISPLAY_P3,
        SdrGamut::SRgb => ACESCG_TO_SRGB,
    };
    // Both vectors are `colorimetry::pinned` constants, not literals. The P3 one
    // used to be spelled out here as well as in `gain_map`, so a colour-space
    // update could have moved one copy and left the other silently stale.
    let weights = match gamut {
        SdrGamut::DisplayP3 => DISPLAY_P3_LUMA,
        SdrGamut::SRgb => SRGB_LUMA,
    };
    (mul(matrix, aces), weights)
}

fn render_destination_pixel(mut rgb: [f32; 3], luminance: f32, shoulder_start: f32) -> [f32; 3] {
    if luminance <= 0.0 {
        return [0.0; 3];
    }
    let rendered_luminance = shoulder(luminance, shoulder_start);
    let scale = rendered_luminance / luminance;
    for channel in &mut rgb {
        *channel *= scale;
    }
    gamut_map(rgb, rendered_luminance)
}

/// C¹-continuous cubic shoulder from `(start, start, slope=1)` to
/// `(1, 1, slope=0)`, followed by the documented display-peak plateau.
fn shoulder(value: f32, start: f32) -> f32 {
    if value <= 0.0 {
        0.0
    } else if value <= start {
        value
    } else if value >= 1.0 {
        1.0
    } else {
        let span = 1.0 - start;
        let t = (value - start) / span;
        let t2 = t * t;
        let t3 = t2 * t;
        (2.0 * t3 - 3.0 * t2 + 1.0) * start + (t3 - 2.0 * t2 + t) * span + (-2.0 * t3 + 3.0 * t2)
    }
}

/// Same-luminance radial mapping to the RGB cube boundary. Because every
/// channel receives one common chroma scale, hue direction and the neutral axis
/// are preserved; no per-channel clip is used as the gamut policy.
fn gamut_map(rgb: [f32; 3], luminance: f32) -> [f32; 3] {
    let neutral = f64::from(luminance);
    let delta = rgb.map(|channel| f64::from(channel) - neutral);
    let mut chroma_scale = 1.0_f64;
    let mut limiting_boundary = None;
    for (channel, d) in delta.into_iter().enumerate() {
        if d > 0.0 {
            let candidate = (1.0 - neutral) / d;
            if candidate < chroma_scale {
                chroma_scale = candidate;
                limiting_boundary = Some((channel, 1.0_f32));
            }
        } else if d < 0.0 {
            let candidate = -neutral / d;
            if candidate < chroma_scale {
                chroma_scale = candidate;
                limiting_boundary = Some((channel, 0.0_f32));
            }
        }
    }
    // The radial calculation is the complete gamut policy. Do not terminally
    // clamp individual channels. Binary64 intersection arithmetic keeps every
    // non-limiting channel inside the cube; assigning the actual limiting
    // channel to its computed boundary makes the intersection exact after the
    // f32 conversion without changing the common radial scale.
    let mut out = delta.map(|d| (neutral + chroma_scale * d) as f32);
    if let Some((channel, boundary)) = limiting_boundary {
        out[channel] = boundary;
    }
    out
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

// AP1/D60 → XYZ, Bradford D60→D65, then XYZ → destination RGB. Reviewed,
// checked-in constants keep the renderer independent of an installed ICC/CMM;
// they are defined once in `colorimetry::pinned` (imported above), together with
// their standards provenance and the tests that re-derive them.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct;
    use crate::pipeline::render_split::display_source;
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::{FilmBase, PrintParams, Reconstruction};

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 2e-5, "{a} != {b}");
    }

    fn shared_from_film_rgb(rgb: &[f32], print: &PrintParams) -> SharedDisplaySource {
        let scan = rgb.iter().map(|value| 1.0 - value).collect();
        let image = LinearImage::new((rgb.len() / 3) as u32, 1, scan, None).unwrap();
        let (film, _) =
            reconstruct(&image, &FilmBase::from([1.0; 3]), &Reconstruction::Simple).unwrap();
        display_source(map_nc_film_rgb_v1(film), print).unwrap()
    }

    #[test]
    fn neutral_ramp_is_neutral_monotonic_and_pins_black_and_reference_white() {
        for gamut in [SdrGamut::DisplayP3, SdrGamut::SRgb] {
            let mut previous = 0.0;
            for value in [0.0, 0.18, 0.5, 0.75, 0.9, 1.0, 2.0] {
                let px = render_pixel([value; 3], gamut, 0.75);
                close(px[0], px[1]);
                close(px[1], px[2]);
                assert!(px[0] >= previous);
                previous = px[0];
            }
            close(render_pixel([0.0; 3], gamut, 0.75)[0], 0.0);
            close(render_pixel([1.0; 3], gamut, 0.75)[0], 1.0);
        }
    }

    #[test]
    fn shoulder_rolls_highlights_without_a_channel_clip_kink() {
        let a = shoulder(0.90, 0.75);
        let b = shoulder(0.95, 0.75);
        let c = shoulder(0.99, 0.75);
        assert!(0.90 < a && a < b && b < c && c < 1.0);
        assert!((1.0 - c) < (c - b));
    }

    #[test]
    fn synthetic_out_of_gamut_vectors_are_finite_and_reach_boundary_radially() {
        for gamut in [SdrGamut::DisplayP3, SdrGamut::SRgb] {
            for input in [[1.0, 0.0, 0.0], [0.0, 1.2, -0.2], [4.0, 0.1, 2.0]] {
                let out = render_pixel(input, gamut, 0.75);
                assert!(out.iter().all(|v| v.is_finite()));
                assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));
                assert!(out.iter().any(|v| *v == 0.0 || *v == 1.0));
            }
        }
    }

    #[test]
    fn gamut_mapping_uses_one_common_chroma_scale_at_constant_luminance() {
        let weights = [0.212_639, 0.715_169, 0.072_192];
        let input = [-0.2, 0.4, 1.1];
        let luminance = dot(input, weights);
        let output = gamut_map(input, luminance);

        close(dot(output, weights), luminance);
        let in_delta = input.map(|channel| channel - luminance);
        let out_delta = output.map(|channel| channel - luminance);
        let scales = [
            out_delta[0] / in_delta[0],
            out_delta[1] / in_delta[1],
            out_delta[2] / in_delta[2],
        ];
        close(scales[0], scales[1]);
        close(scales[1], scales[2]);
        assert!(scales[0] > 0.0 && scales[0] < 1.0);
        assert!(
            output
                .iter()
                .any(|value| value.abs() < 2e-6 || (value - 1.0).abs() < 2e-6)
        );
    }

    #[test]
    fn golden_vectors_pin_both_destination_gamuts() {
        let aces = [0.42, 0.18, 0.07];
        let p3 = render_pixel(aces, SdrGamut::DisplayP3, 0.75);
        let srgb = render_pixel(aces, SdrGamut::SRgb, 0.75);
        for (actual, expected) in p3
            .into_iter()
            .zip([0.518_749_9, 0.164_785_43, 0.064_243_82])
        {
            close(actual, expected);
        }
        for (actual, expected) in srgb
            .into_iter()
            .zip([0.598_370_73, 0.149_898_8, 0.047_412_235])
        {
            close(actual, expected);
        }
    }

    #[test]
    fn public_renderer_is_deterministic_and_returns_complete_resolved_metadata() {
        let print = PrintParams {
            highlight_compress: 0.4,
            ..PrintParams::default()
        };
        let shared = shared_from_film_rgb(&[0.18, 0.18, 0.18, 1.4, 0.5, 0.1], &print);
        for (gamut, profile) in [
            (SdrGamut::DisplayP3, "display-p3"),
            (SdrGamut::SRgb, "srgb"),
        ] {
            let first = render(&shared, gamut, print.highlight_compress).unwrap();
            let second = render(&shared, gamut, print.highlight_compress).unwrap();
            assert_eq!(
                first
                    .image()
                    .rgb
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                second
                    .image()
                    .rgb
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>()
            );
            assert!(
                first
                    .image()
                    .rgb
                    .iter()
                    .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
            );
            let metadata = first.metadata();
            assert_eq!(metadata.reference_white_nits, 203.0);
            assert_eq!(metadata.highlight_compress, 0.4);
            assert_eq!(metadata.required_transfer, "srgb");
            assert_eq!(metadata.required_profile, profile);
        }
    }

    #[test]
    fn highlight_control_adds_bounded_rolloff_to_the_mandatory_baseline() {
        let shared = shared_from_film_rgb(&[0.7, 0.7, 0.7], &PrintParams::default());
        let baseline = render(&shared, SdrGamut::SRgb, 0.0).unwrap();
        let stronger = render(&shared, SdrGamut::SRgb, 4.0).unwrap();

        close(baseline.metadata().shoulder_start, 0.75);
        close(stronger.metadata().shoulder_start, 0.55);
        close(baseline.image().rgb[0], 0.7);
        assert!(stronger.image().rgb[0] > baseline.image().rgb[0]);
        assert!(stronger.image().rgb[0] < 1.0);

        let maximum = render(&shared, SdrGamut::SRgb, f32::MAX).unwrap();
        assert!((0.5..=0.75).contains(&maximum.metadata().shoulder_start));
    }

    #[test]
    fn extreme_finite_input_fails_if_render_arithmetic_overflows() {
        for (index, input) in [(17, [f32::MAX; 3]), (23, [-f32::MAX; 3])] {
            let err = render_pixel_checked(input, index, SdrGamut::DisplayP3, 0.75).unwrap_err();
            assert!(err.to_string().contains(&format!("pixel {index}")), "{err}");
            assert!(err.to_string().contains("produced a non-finite"), "{err}");
        }
    }

    #[test]
    fn finite_non_positive_luminance_maps_to_black() {
        assert_eq!(
            render_pixel_checked([-1.0; 3], 0, SdrGamut::DisplayP3, 0.75).unwrap(),
            [0.0; 3]
        );
    }
}
