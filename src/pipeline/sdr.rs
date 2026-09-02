//! Deterministic SDR display rendering from the shared adjusted ACEScg source.
//!
//! This stage owns the display rendering only: ACEScg/D60 → destination-linear
//! RGB, whichever tone curve `print.display_tone` resolved to, and neutral-axis gamut
//! mapping. Transfer encoding and ICC attachment remain in [`super::color`].
//!
//! The tone is **selected, not fixed**: the reference-white-preserving Hermite shoulder
//! is the default, `none` skips tone entirely, and extended Reinhard compresses globally
//! against a stated white point — and that last one is deliberately *not*
//! reference-white-preserving, which is what `DisplayTone::bounds_sdr_output` reports and
//! why the output range check here is a property of the tone rather than of SDR.

use serde::Serialize;

use crate::pipeline::colorimetry::pinned::{
    ACESCG_TO_DISPLAY_P3, ACESCG_TO_SRGB, DISPLAY_P3_LUMA, SRGB_LUMA,
};
use crate::pipeline::display_tone::{self, DisplayTone};
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
    /// The resolved knee width and where the shoulder began — both `None` when no
    /// display tone curve was applied, since neither exists in that render. They
    /// travel together: a knee width without a shoulder describes nothing.
    pub highlight_compress: Option<f32>,
    pub shoulder_start: Option<f32>,
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
    // `output/presets` finished without wiring it — the SDR report block was never
    // in that task's scope. `output/sdr-preset-followups` owns it (item 3).
    #[allow(dead_code)] // consumed next by `output/sdr-preset-followups`'s report block.
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
/// Adjusted `1.0` is reference white (203 cd/m²). Under
/// [`DisplayTone::HermiteShoulder`] the shoulder begins below reference white and
/// lands at `1.0` with zero slope; values above reference white remain at the
/// display peak. Under [`DisplayTone::None`] tone is left alone and input above
/// reference white is a loud error instead — the reconstruction is then the only
/// thing placing the highlights, which is the point. Under
/// [`DisplayTone::ExtendedReinhard`] the whole curve is compressed against a stated
/// white point; that operator is **not** bounded by reference white, so its overshoot
/// is carried to `io::encode` and counted there rather than refused. Negativity stays
/// a hard error under all three.
///
/// Out-of-gamut colour is moved
/// radially toward the same-luminance neutral axis until it reaches the
/// destination gamut boundary, rather than clipping channels independently, in
/// every mode. Above display white that boundary follows the pixel's own rendered
/// luminance — reachable only under the unbounded tone — which keeps it continuous
/// across the crossing; see [`render_destination_pixel`].
pub fn render(
    shared: &SharedDisplaySource,
    gamut: SdrGamut,
    tone: DisplayTone,
) -> Result<RenderedSdr> {
    let mut rgb = Vec::with_capacity(shared.source.rgb().len());
    for (index, px) in shared.source.rgb().as_chunks::<3>().0.iter().enumerate() {
        let rendered = render_pixel_checked(*px, index, gamut, tone)?;
        rgb.extend_from_slice(&rendered);
    }
    let image = LinearImage::new(shared.source.width(), shared.source.height(), rgb, None)?;
    Ok(RenderedSdr {
        image,
        metadata: SdrRenderMetadata {
            gamut,
            reference_white_nits: 203.0,
            highlight_compress: tone.highlight_compress(),
            // In this domain the normalized knee position *is* the shoulder start:
            // SDR rolls off within `[0, 1]`, so no scaling separates the two. Absent
            // for any tone without a knee.
            shoulder_start: tone.knee_position(),
            tone_curve: match tone {
                DisplayTone::HermiteShoulder(_) => "reference-white-hermite-shoulder-v1",
                DisplayTone::None => display_tone::NO_TONE_CURVE,
                DisplayTone::ExtendedReinhard(_) => display_tone::EXTENDED_REINHARD,
            },
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
    tone: DisplayTone,
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
    // Diagnosed here rather than left to the range check below. Without a tone
    // curve nothing pulls luminance down, and the gamut map holds it constant, so
    // the cube violation the range check would report is a *consequence* — this
    // says which sample was already over reference white before rendering.
    if matches!(tone, DisplayTone::None) && luminance > 1.0 {
        return Err(above_range_error(index, luminance));
    }
    let rendered = render_destination_pixel(rgb, luminance, tone);
    if !rendered.iter().all(|value| value.is_finite()) {
        return Err(NcError::Other(format!(
            "SDR display rendering produced a non-finite sample at pixel {index}"
        )));
    }
    // The bound is a property of the resolved tone, not of SDR. A tone that bounds its
    // own output escaping the ceiling is a renderer bug and must fail loudly — that is
    // what makes `--display-tone none` self-policing. Extended Reinhard is *expected*
    // past the ceiling, so there the loss rides to `io::encode`, which counts every
    // clamped sample into `EncodeReport`; only negativity stays a hard error, since
    // nothing downstream is defined below black.
    if tone.bounds_sdr_output() {
        if !rendered.iter().all(|value| (0.0..=1.0).contains(value)) {
            return Err(NcError::Other(format!(
                "SDR display rendering produced an out-of-range sample at pixel {index}"
            )));
        }
    } else if !rendered.iter().all(|value| *value >= 0.0) {
        return Err(NcError::Other(format!(
            "SDR display rendering produced a negative sample at pixel {index}"
        )));
    }
    Ok(rendered)
}

/// The loud half of "a linear render is self-policing": with no tone curve, a
/// reconstruction that overshoots reference white fails naming the sample and every
/// way out, instead of clipping quietly.
///
/// **The print controls are named deliberately.** They run *before* this render, so
/// the likeliest cause is a lift applied to an already-bounded reconstruction — and a
/// message offering only "bound the reconstruction" tells such a user to fix the one
/// thing that is not wrong.
fn above_range_error(index: usize, luminance: f32) -> NcError {
    NcError::Other(format!(
        "SDR display rendering applied no display tone curve, but pixel {index} sits \
         above reference white (luminance {luminance}), which this mode has no curve to \
         roll off. Three things reach here: the reconstruction may exceed reference \
         white (a sigmoid curve with `shoulder > 0` does not), a print control applied \
         before this render may have lifted it there (--print-exposure, --white-balance \
         / --auto-wb, --linear-range), or the shoulder may simply be wanted. So: keep \
         the print controls neutral, bound the reconstruction, or drop --display-tone \
         none."
    ))
}

#[cfg(test)]
fn render_pixel(aces: [f32; 3], gamut: SdrGamut, tone: DisplayTone) -> [f32; 3] {
    let (rgb, weights) = destination_rgb(aces, gamut);
    render_destination_pixel(rgb, dot(rgb, weights), tone)
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

fn render_destination_pixel(mut rgb: [f32; 3], luminance: f32, tone: DisplayTone) -> [f32; 3] {
    if luminance <= 0.0 {
        return [0.0; 3];
    }
    let rendered_luminance = match tone {
        DisplayTone::HermiteShoulder(_) => shoulder(
            luminance,
            tone.knee_position().expect("a shoulder has a knee"),
        ),
        DisplayTone::None => luminance,
        DisplayTone::ExtendedReinhard(headroom) => {
            display_tone::extended_reinhard(luminance, headroom.white_point())
        }
    };
    let scale = rendered_luminance / luminance;
    for channel in &mut rgb {
        *channel *= scale;
    }
    // The radial intersection is taken against `[0, max(display white, this pixel's
    // rendered luminance)]`, which matters only for a tone that can exceed the ceiling.
    //
    // Constant-luminance radial mapping already squeezes chroma out as luminance
    // approaches the cube's top — at luminance 1.0 the only in-gamut colour *is* white —
    // so the boundary reaches `neutral` continuously. Letting the ceiling follow the
    // pixel keeps that continuous above display white: the intersection degenerates to
    // the neutral axis and highlights desaturate toward white, which is what film and
    // print do anyway.
    //
    // Gating the ceiling on `rendered_luminance <= 1` instead looked equivalent and is
    // not: it restores full chroma in one step. Measured on the shipped arithmetic
    // (reinhard `W = 2`, sRGB direction `[3, 1, 0.1]`), rendered luminance 0.9998 gives
    // `[1.000, 1.000, 1.000]` and 1.0000 gives `[2.913, 0.532, 0.000]` — green and blue
    // *fall* as scene luminance rises, a hard ring around every bright saturated
    // highlight. Under the two bounded tones the `max` is always `1.0`, so their path is
    // unchanged.
    gamut_map(rgb, rendered_luminance, rendered_luminance.max(1.0))
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
fn gamut_map(rgb: [f32; 3], luminance: f32, ceiling: f32) -> [f32; 3] {
    let neutral = f64::from(luminance);
    let delta = rgb.map(|channel| f64::from(channel) - neutral);
    let mut chroma_scale = 1.0_f64;
    let mut limiting_boundary = None;
    for (channel, d) in delta.into_iter().enumerate() {
        if d > 0.0 {
            let candidate = (f64::from(ceiling) - neutral) / d;
            if candidate < chroma_scale {
                chroma_scale = candidate;
                limiting_boundary = Some((channel, ceiling));
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
    use crate::pipeline::display_tone::Headroom;
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
                let px = render_pixel([value; 3], gamut, DisplayTone::DEFAULT);
                close(px[0], px[1]);
                close(px[1], px[2]);
                assert!(px[0] >= previous);
                previous = px[0];
            }
            close(render_pixel([0.0; 3], gamut, DisplayTone::DEFAULT)[0], 0.0);
            close(render_pixel([1.0; 3], gamut, DisplayTone::DEFAULT)[0], 1.0);
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
                let out = render_pixel(input, gamut, DisplayTone::DEFAULT);
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
        let output = gamut_map(input, luminance, 1.0);

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
        let p3 = render_pixel(aces, SdrGamut::DisplayP3, DisplayTone::DEFAULT);
        let srgb = render_pixel(aces, SdrGamut::SRgb, DisplayTone::DEFAULT);
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
            let tone = DisplayTone::resolve(&print).unwrap();
            let first = render(&shared, gamut, tone).unwrap();
            let second = render(&shared, gamut, tone).unwrap();
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
            assert_eq!(metadata.highlight_compress, Some(0.4));
            assert_eq!(metadata.required_transfer, "srgb");
            assert_eq!(metadata.required_profile, profile);
        }
    }

    #[test]
    fn highlight_control_adds_bounded_rolloff_to_the_mandatory_baseline() {
        let shared = shared_from_film_rgb(&[0.7, 0.7, 0.7], &PrintParams::default());
        let baseline = render(&shared, SdrGamut::SRgb, DisplayTone::DEFAULT).unwrap();
        let stronger =
            render(&shared, SdrGamut::SRgb, DisplayTone::shoulder(4.0).unwrap()).unwrap();

        close(baseline.metadata().shoulder_start.unwrap(), 0.75);
        close(stronger.metadata().shoulder_start.unwrap(), 0.55);
        close(baseline.image().rgb[0], 0.7);
        assert!(stronger.image().rgb[0] > baseline.image().rgb[0]);
        assert!(stronger.image().rgb[0] < 1.0);

        let maximum = render(
            &shared,
            SdrGamut::SRgb,
            DisplayTone::shoulder(f32::MAX).unwrap(),
        )
        .unwrap();
        assert!((0.5..=0.75).contains(&maximum.metadata().shoulder_start.unwrap()));
    }

    #[test]
    fn no_tone_curve_passes_tone_through_and_only_differs_above_the_knee() {
        // Everything from black to reference white, straddling the 0.75 knee.
        let ramp: Vec<f32> = [0.0, 0.18, 0.5, 0.75, 0.9, 1.0]
            .into_iter()
            .flat_map(|v| [v; 3])
            .collect();
        let shared = shared_from_film_rgb(&ramp, &PrintParams::default());
        let shouldered = render(&shared, SdrGamut::SRgb, DisplayTone::DEFAULT).unwrap();
        let linear = render(&shared, SdrGamut::SRgb, DisplayTone::None).unwrap();

        for (index, input) in [0.0, 0.18, 0.5, 0.75, 0.9, 1.0].into_iter().enumerate() {
            let (with, without) = (
                shouldered.image().rgb[index * 3],
                linear.image().rgb[index * 3],
            );
            // No tone curve means exactly that: the adjusted value survives.
            close(without, input);
            // The knee itself is a fixed point of the shoulder, so it belongs with
            // the identical region rather than the compressed one.
            if input <= 0.75 {
                assert_eq!(with.to_bits(), without.to_bits(), "at {input}");
            } else if input < 1.0 {
                // The shoulder is *above* the identity line — it lifts highlights
                // toward white, which is the separation this mode gives back.
                assert!(with > without, "at {input}: {with} !> {without}");
            }
        }
        // Both still land black on black and reference white on reference white.
        close(shouldered.image().rgb[15], 1.0);
        close(linear.image().rgb[15], 1.0);
    }

    #[test]
    fn no_tone_curve_accepts_diffuse_white_sitting_exactly_on_the_bound() {
        // The intended pairing places diffuse white *at* reference white, so the
        // brightest pixel of a bounded reconstruction lands exactly on this mode's
        // bound rather than below it. Measured: film RGB `[1,1,1]`'s destination
        // luminance rounds to exactly `1.0` on both gamuts — **zero ulps** over, so it
        // passes `> 1.0` with nothing to spare. Display P3's red channel is itself one
        // ulp above 1.0 (`1.0000001`); the radial gamut map is what pulls it back into
        // the cube, which is why the final range check does not fire either.
        //
        // That margin is a property of the pinned matrices, not a chosen tolerance, so
        // this test is the tripwire: re-pinning `ACESCG_TO_DISPLAY_P3` /
        // `DISPLAY_P3_LUMA`, or changing `dot`'s accumulation order, fails here — not
        // in a user's conversion, which would refuse its own diffuse white at exit 1
        // after paying for the whole render.
        for gamut in [SdrGamut::DisplayP3, SdrGamut::SRgb] {
            let shared = shared_from_film_rgb(&[1.0; 3], &PrintParams::default());
            let rendered = render(&shared, gamut, DisplayTone::None)
                .unwrap_or_else(|e| panic!("{gamut:?}: diffuse white must not be refused: {e}"));
            for value in &rendered.image().rgb {
                close(*value, 1.0);
            }
            // The guard's own input, so a failure says which way it drifted.
            let (rgb, weights) = destination_rgb([1.0; 3], gamut);
            let luminance = dot(rgb, weights);
            assert!(
                luminance <= 1.0,
                "{gamut:?}: destination luminance of diffuse white drifted to {luminance} \
                 ({} ulps above 1.0), which this mode now refuses",
                luminance.to_bits() as i64 - 1.0f32.to_bits() as i64
            );
        }
    }

    #[test]
    fn no_tone_curve_reports_no_knee() {
        let shared = shared_from_film_rgb(&[0.5; 3], &PrintParams::default());
        let metadata = *render(&shared, SdrGamut::DisplayP3, DisplayTone::None)
            .unwrap()
            .metadata();
        assert_eq!(metadata.highlight_compress, None);
        assert_eq!(metadata.shoulder_start, None);
        assert_eq!(metadata.tone_curve, display_tone::NO_TONE_CURVE);
        // The rest of the policy is untouched: only tone was skipped.
        assert_eq!(metadata.gamut_mapping, "neutral-axis-radial-boundary-v1");
        assert_eq!(metadata.required_transfer, "srgb");
    }

    #[test]
    fn no_tone_curve_refuses_input_above_reference_white_naming_the_pixel() {
        // Reference white, then a sample a third of a stop over it.
        let shared =
            shared_from_film_rgb(&[1.0, 1.0, 1.0, 1.25, 1.25, 1.25], &PrintParams::default());
        // The shouldered render accepts it — this is the mode's own bound, not a
        // property of the source.
        assert!(render(&shared, SdrGamut::DisplayP3, DisplayTone::DEFAULT).is_ok());

        let err = render(&shared, SdrGamut::DisplayP3, DisplayTone::None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("pixel 1"), "{message}");
        assert!(message.contains("above reference white"), "{message}");
        assert!(message.contains("no display tone curve"), "{message}");
    }

    #[test]
    fn extreme_finite_input_fails_if_render_arithmetic_overflows() {
        for (index, input) in [(17, [f32::MAX; 3]), (23, [-f32::MAX; 3])] {
            let err = render_pixel_checked(input, index, SdrGamut::DisplayP3, DisplayTone::DEFAULT)
                .unwrap_err();
            assert!(err.to_string().contains(&format!("pixel {index}")), "{err}");
            assert!(err.to_string().contains("produced a non-finite"), "{err}");
        }
    }

    #[test]
    fn a_chromatic_ramp_through_display_white_stays_continuous_and_monotonic() {
        // The guard on the one line an unbounded tone changed: the radial gamut ceiling
        // is `max(1.0, this pixel's rendered luminance)` rather than a pinned `1.0`.
        //
        // **Verified falsifiable against both plausible mutations, and they trip
        // different assertions** — which is why the test carries both.
        //
        // *Ceiling pinned at `1.0`.* Above it every positive `delta` channel's boundary
        // candidate `(1.0 - neutral)/d` goes negative, so the common chroma scale does
        // too and the hue inverts. Measured at `t = 1.350`: `[1.0, 1.0009373,
        // 1.0011468]` from a **red-dominant** input — red pinned at the boundary while
        // green and blue climb past it. Per-channel continuity and monotonicity both
        // *pass*; only the hue-order assertion catches it.
        //
        // *Gamut map skipped above the crossing* (the "gate on `rendered_luminance <= 1`"
        // shape the render comment records). Measured at the same step:
        // `[1.0, 0.9978602, 0.99738216] -> [3.0262587, 0.5053716, -0.05781346]` — full
        // chroma restored in one step, with a negative channel. The jump bound catches
        // that one.
        //
        // With the shipped ceiling the boundary degenerates onto the neutral axis
        // instead, so chroma is squeezed out continuously and highlights desaturate
        // toward white, as film and print do.
        //
        // A saturated direction is essential: on a neutral ramp both ceilings agree, so
        // the same sweep in grey passes either way and proves nothing.
        let tone = DisplayTone::ExtendedReinhard(Headroom::new(1.0).unwrap());
        // `W = 2` puts the crossing in easy reach — rendered luminance passes 1.0 near
        // `t = 1.35` on this direction — instead of needing an absurd input scale.
        let direction = [3.0f32, 1.0, 0.1];
        let scale = |step: usize| 0.5 + step as f32 * 0.005;
        let sweep: Vec<[f32; 3]> = (0..=400)
            .map(|step| render_pixel(direction.map(|c| c * scale(step)), SdrGamut::SRgb, tone))
            .collect();

        // The sweep must actually straddle display white, or it guards nothing. Measured
        // on the *rendered luminance*, not on the channels: this direction is already
        // out of gamut at the bottom of the ramp, so red sits pinned at the cube
        // boundary throughout and no sample ever has all three channels under 1.0.
        let rendered_luminance = |step: usize| {
            let (rgb, weights) =
                destination_rgb(direction.map(|c| c * scale(step)), SdrGamut::SRgb);
            display_tone::extended_reinhard(dot(rgb, weights), 2.0)
        };
        assert!(
            rendered_luminance(0) < 1.0 && rendered_luminance(400) > 1.0,
            "the ramp never crossed display white: {} .. {}",
            rendered_luminance(0),
            rendered_luminance(400)
        );

        // Measured worst per-step jump on the shipped arithmetic is 0.0056; the bound is
        // an order of magnitude above it and two below the ~1.9 a hue flip produces.
        for (index, pair) in sweep.windows(2).enumerate() {
            let (previous, current) = (pair[0], pair[1]);
            for channel in 0..3 {
                assert!(
                    current[channel] >= 0.0 && current[channel].is_finite(),
                    "step {index}, channel {channel}: {current:?}"
                );
                assert!(
                    current[channel] >= previous[channel],
                    "step {index}, channel {channel} fell as scene luminance rose: \
                     {previous:?} -> {current:?}"
                );
                assert!(
                    current[channel] - previous[channel] < 0.05,
                    "step {index}, channel {channel} jumped: {previous:?} -> {current:?}"
                );
            }
            // Hue order survives the crossing. The input is red-dominant, so the render
            // is either red >= green >= blue or exactly neutral — never re-ordered. This
            // is the assertion the pinned ceiling breaks.
            assert!(
                current[0] >= current[1] && current[1] >= current[2],
                "step {index}: hue order inverted at the crossing: {current:?}"
            );
        }

        // ...and the mechanism, not just the symptom: above display white the pixel is
        // on the neutral axis, which is what "desaturates toward white" means.
        let brightest = sweep.last().unwrap();
        assert!(brightest[0] > 1.0, "{brightest:?}");
        assert_eq!(brightest[0], brightest[1], "{brightest:?}");
        assert_eq!(brightest[1], brightest[2], "{brightest:?}");
    }

    #[test]
    fn an_unbounded_tone_carries_over_range_output_to_the_encode_boundary() {
        // The `!bounds_sdr_output()` half of `render_pixel_checked`'s postcondition. A
        // bounded tone escaping `[0, 1]` is a renderer bug and fails loudly; extended
        // Reinhard is *expected* past the ceiling, so the overshoot rides to
        // `io::encode`, which counts every clamped sample into `EncodeReport`.
        let shared = shared_from_film_rgb(&[1.0, 1.0, 1.0, 4.0, 4.0, 4.0], &PrintParams::default());
        // `none` refuses the same source — so this is the tone's range policy, not a
        // property of the input.
        assert!(render(&shared, SdrGamut::DisplayP3, DisplayTone::None).is_err());

        let tone = DisplayTone::ExtendedReinhard(Headroom::new(1.0).unwrap());
        let rendered = render(&shared, SdrGamut::DisplayP3, tone)
            .unwrap_or_else(|e| panic!("an unbounded tone must not be refused: {e}"));
        let rgb = &rendered.image().rgb;
        assert!(
            rgb.iter().any(|v| *v > 1.0),
            "nothing exceeded the ceiling, so the branch was not exercised: {rgb:?}"
        );
        assert!(rgb.iter().all(|v| v.is_finite() && *v >= 0.0), "{rgb:?}");
        // A negative *input* channel is legal and still renders: the radial gamut map pulls
        // it back onto the boundary, so it never reaches the postcondition as a negative
        // output. Stated plainly because the comment here used to claim this asserted
        // "negativity stays a hard error", which is the opposite of what the line below
        // checks — and the negative-output half of `render_pixel_checked` is in fact
        // unreachable through this entry point, so nothing here exercises it.
        let ok = render_pixel_checked([-1.0, 0.0, 2.0], 9, SdrGamut::DisplayP3, tone);
        assert!(ok.is_ok(), "a legal chromatic sample must still render");
        assert!(
            ok.unwrap().iter().all(|v| *v >= 0.0),
            "the gamut map is what keeps the output non-negative"
        );
        // The metadata names the operator rather than reporting a knee it has none of.
        let metadata = *rendered.metadata();
        assert_eq!(metadata.tone_curve, display_tone::EXTENDED_REINHARD);
        assert_eq!(metadata.highlight_compress, None);
        assert_eq!(metadata.shoulder_start, None);
    }

    #[test]
    fn finite_non_positive_luminance_maps_to_black() {
        assert_eq!(
            render_pixel_checked([-1.0; 3], 0, SdrGamut::DisplayP3, DisplayTone::DEFAULT).unwrap(),
            [0.0; 3]
        );
    }
}
