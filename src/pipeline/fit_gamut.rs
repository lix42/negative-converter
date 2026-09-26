//! **Stage 4 of the new rendering chain — fit gamut.**
//!
//! Move out-of-gamut colour to the destination's boundary, keeping hue: the change of
//! primaries out of ACEScg, then [`radial_to_boundary`] against the cube
//! `[0, max(peak, Y)]` in the destination's primaries.
//!
//! **The ceiling is fit range's output, never decided here**: the display's peak, read
//! off [`RangeFittedImage`]. Above it the cube follows the pixel (see
//! [`radial_to_boundary`]), so content fit range left above the peak renders neutral at
//! its own luminance, and the encoder clamps and counts its level.
//!
//! [`radial_to_boundary`] is the **one** gamut-mapping implementation: the legacy
//! renderers (`sdr`, `hdr`, `gain_map`) call it too, each passing its own ceiling. The
//! ceilings differ for real reasons — the gain map's must match the base as stored —
//! so unifying the arithmetic must never unify them.
//!
//! **This is the one stage that may split an SDR/HDR pair below diffuse white.** A
//! saturated colour with one channel above `1` at a luminance under white is mapped
//! onto the SDR cube and left alone under an HDR peak. That is the branch contract's
//! one permitted difference below white (`pipeline::chain`), carried by the gain map
//! per channel; mapping HDR into the SDR cube to remove it would discard colour the HDR
//! display can show.
//!
//! **What this stage discards is not counted.** It is the chain's last stage and clamps
//! nothing, but the map is a gamut policy: a negative channel moves onto `0` with the
//! rest of the colour, and a pixel with `Y ≤ 0` is written black. Neither reaches the
//! encoder's clip count, so `--strict` cannot see it — as in the legacy renderers,
//! whose map is the same.

use std::fmt;

use crate::pipeline::colorimetry::dot;
use crate::pipeline::colorimetry::pinned::{
    ACESCG_TO_ADOBE_RGB, ACESCG_TO_BT2020, ACESCG_TO_DISPLAY_P3, ADOBE_RGB_LUMA, BT2020_LUMA,
    DISPLAY_P3_LUMA,
};
use crate::pipeline::fit_range::RangeFittedImage;
use crate::pipeline::pixels;
use crate::pipeline::working_image::WorkingBuffer;
use crate::types::{LinearImage, NcError, Result};

/// The gamut a destination renders into — which primaries the chain's output is in.
///
/// **Display-referred encodings with a bounded range only** — the cube fit gamut maps
/// into ends at the display's peak. A scene-referred or unbounded working space
/// (ACEScg, ProPhoto as nc used it) does not qualify, however often editors use it;
/// `film-master` is the output for those. **Which destination renders into which gamut
/// is the destination table's** (`crate::destination::ROWS`), not this type's. An enum rather than a matrix
/// field so the value can travel with the image to the encoder, which reads it off
/// [`DisplayReferredImage`] instead of re-deriving it from a preset.
///
/// **Adding a variant** means a pinned ACEScg → destination matrix and luma row
/// (`docs/colorimetry-maintenance.md`), an arm in each method here, and one in
/// `color::encode_display_linear`, which owns the destination's transfer and ICC
/// profile. Each match is exhaustive, so a missed arm fails to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationGamut {
    /// Display P3 primaries, D65 white.
    DisplayP3,
    /// Adobe RGB (1998) primaries, D65 white — Rec.709's red and blue with a wider
    /// green. It qualifies as a display encoding (bounded at white, a fixed transfer);
    /// that editors expect it is why `nf-destinations/direct-preset` wants it, not
    /// why it belongs here.
    AdobeRgb,
    /// ITU-R BT.2020 primaries, D65 white — the gamut of the Rec.2100 HDR signals (PQ,
    /// HLG) and of the linear HDR interchange TIFF. Encoded only by the HDR encoders:
    /// no SDR destination renders into it.
    Bt2020,
}

impl DestinationGamut {
    /// Linear ACEScg → linear destination primaries, from `colorimetry::pinned`.
    fn acescg_matrix(self) -> [[f32; 3]; 3] {
        match self {
            DestinationGamut::DisplayP3 => ACESCG_TO_DISPLAY_P3,
            DestinationGamut::AdobeRgb => ACESCG_TO_ADOBE_RGB,
            DestinationGamut::Bt2020 => ACESCG_TO_BT2020,
        }
    }

    /// The destination's luminance weights: the cube being fitted into is the
    /// destination's, so luminance is measured in its primaries.
    pub(in crate::pipeline) fn luma(self) -> [f32; 3] {
        match self {
            DestinationGamut::DisplayP3 => DISPLAY_P3_LUMA,
            DestinationGamut::AdobeRgb => ADOBE_RGB_LUMA,
            DestinationGamut::Bt2020 => BT2020_LUMA,
        }
    }

    /// The identifier the report states.
    pub fn name(self) -> &'static str {
        match self {
            DestinationGamut::DisplayP3 => "display-p3",
            DestinationGamut::AdobeRgb => "adobe-rgb",
            DestinationGamut::Bt2020 => "bt2020",
        }
    }
}

/// Fit gamut's knobs. **No `Default`**: the target gamut is the destination's to
/// state, and a default would let a chain render into primaries nobody chose.
#[derive(Clone, Debug, PartialEq)]
pub struct FitGamutParams {
    pub target: DestinationGamut,
}

impl FitGamutParams {
    /// What fit gamut does under these parameters, for the report: the change of
    /// primaries, then the radial map.
    pub fn applied(&self) -> &'static str {
        match self.target {
            DestinationGamut::DisplayP3 => {
                "acescg-to-display-p3-matrix+neutral-axis-radial-boundary-v2"
            }
            DestinationGamut::AdobeRgb => {
                "acescg-to-adobe-rgb-matrix+neutral-axis-radial-boundary-v2"
            }
            DestinationGamut::Bt2020 => "acescg-to-bt2020-matrix+neutral-axis-radial-boundary-v2",
        }
    }
}

/// The chain's output: **display-referred** pixels in the destination's primaries,
/// ready to encode.
///
/// [`into_parts`] is the whole boundary: the chain's **exit**, and it moves, so
/// leaving the chain costs no more than entering it did. The gamut rides out beside
/// the pixels, so the encoder learns which primaries it holds from the image itself.
///
/// **The carried IR plane rides out with it.** The design says carry the plane
/// through rather than consume it (CLAUDE.md), and this chain must not be the thing
/// that loses it — today's `pipeline::sdr` render does drop it
/// (`LinearImage::new(w, h, rgb, None)`), and the new chain deliberately does not
/// copy that. Nothing downstream depends on the plane arriving here: `--export-ir`
/// writes from the *decoded* image, and both IR warnings are derived before the
/// render. How the plane *travels* is `nf-core/buffer-strategy`'s to settle; that it
/// is not lost is decided here.
///
/// [`into_parts`]: Self::into_parts
pub struct DisplayReferredImage(WorkingBuffer, DestinationGamut);

impl DisplayReferredImage {
    /// Leave the chain, handing the buffers and their gamut to the encoder.
    ///
    /// `io::encode` takes `&LinearImage`, so this is how a destination gets one
    /// **without copying** a full-frame buffer. Consuming, mirroring
    /// `AcesCgImage::into_linear` at the other end of the chain.
    pub(crate) fn into_parts(self) -> (LinearImage, DestinationGamut) {
        (self.0.into_linear(), self.1)
    }
}

impl fmt::Debug for DisplayReferredImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_named(f, "DisplayReferredImage")
    }
}

/// Move the pixels into the target gamut's primaries, then onto its boundary.
///
/// Per pixel, in place: the 3×3, the destination's luminance `Y`, and
/// [`radial_to_boundary`] with the peak as the ceiling — so `[0, max(peak, Y)]`, a
/// neutral at `Y` above the peak, and black at `Y ≤ 0`.
///
/// **Refuses a non-finite result**, naming the lowest such pixel. Fit range hands over
/// finite samples, but a large enough one of mixed sign overflows the 3×3, and the
/// encoder must never receive what that makes.
pub fn apply(image: RangeFittedImage, params: &FitGamutParams) -> Result<DisplayReferredImage> {
    let peak = image.peak().value();
    let mut buffer = image.into_buffer();
    let m = params.target.acescg_matrix();
    let luma = params.target.luma();
    pixels::try_map_in_place(buffer.rgb_mut(), |index, px| {
        let [r, g, b] = *px;
        let rgb = [
            m[0][0] * r + m[0][1] * g + m[0][2] * b,
            m[1][0] * r + m[1][1] * g + m[1][2] * b,
            m[2][0] * r + m[2][1] * g + m[2][2] * b,
        ];
        // Finite luminance implies finite channels: an infinite or NaN channel makes
        // the weighted sum infinite or NaN.
        let luminance = dot(luma, rgb);
        if !luminance.is_finite() {
            return Err(NcError::Other(format!(
                "fit gamut's change of primaries overflowed at pixel {index} ({px:?})"
            )));
        }
        *px = radial_to_boundary(rgb, luminance, peak);
        Ok(())
    })?;
    Ok(DisplayReferredImage(buffer, params.target))
}

/// Move `rgb` toward the neutral point at `luminance` until it lies inside the cube
/// `[0, max(ceiling, luminance)]³` — the one gamut map (`neutral-axis-radial-boundary`),
/// every caller passing its own ceiling.
///
/// - **One common scale in `[0, 1]`** on every channel's distance from neutral, so the
///   neutral axis and the direction away from it in linear RGB — not perceptual hue —
///   are kept. An in-gamut colour keeps scale `1` and comes back unchanged up to
///   binary64 rounding; the limiting channel of an out-of-gamut one is assigned its
///   boundary so the intersection is exact in `f32`. No channel is clipped alone.
/// - **Above the ceiling the cube follows the pixel**, so it holds only the neutral:
///   highlights desaturate to white continuously. Mapping only up to the ceiling and
///   leaving brighter pixels alone restores full chroma in one step — a ring around
///   every bright saturated highlight. Held here so no caller can pass a luminance the
///   cube cannot contain, which would mirror the hue.
/// - **At `luminance ≤ 0` the result is black**: no point on the neutral axis there is
///   inside the cube. The one place the map discards colour.
///
/// The report ids naming it (the legacy metadata's `…-v1`, fit gamut's `…-v2`) version
/// each caller's use of this map, not the map itself.
pub(crate) fn radial_to_boundary(rgb: [f32; 3], luminance: f32, ceiling: f32) -> [f32; 3] {
    if luminance <= 0.0 {
        return [0.0; 3];
    }
    let ceiling = ceiling.max(luminance);
    let neutral = f64::from(luminance);
    let upper = f64::from(ceiling);
    let delta = rgb.map(|channel| f64::from(channel) - neutral);
    let mut scale = 1.0_f64;
    let mut limit = None;
    for (channel, d) in delta.into_iter().enumerate() {
        let (candidate, boundary) = if d > 0.0 {
            ((upper - neutral) / d, ceiling)
        } else if d < 0.0 {
            (-neutral / d, 0.0)
        } else {
            continue;
        };
        if candidate < scale {
            scale = candidate;
            limit = Some((channel, boundary));
        }
    }
    let mut out = delta.map(|d| (neutral + scale * d) as f32);
    if let Some((channel, boundary)) = limit {
        out[channel] = boundary;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::FilmRgbImage;
    use crate::pipeline::fit_range::{self, DisplayPeak, FitRangeParams};
    use crate::pipeline::look::{self, LookParams};
    use crate::pipeline::scene_correction::{self, SceneCorrectionParams};
    use crate::pipeline::working_space::map_nc_film_rgb_v1;

    /// Film RGB through the stages above at their identities — fit range at zero
    /// headroom — against `peak`, and the ACEScg values that reach this stage.
    fn fitted(film_rgb: &[f32], peak: DisplayPeak) -> (RangeFittedImage, Vec<f32>) {
        let image = LinearImage::new((film_rgb.len() / 3) as u32, 1, film_rgb.to_vec(), None);
        let aces = map_nc_film_rgb_v1(FilmRgbImage::fixture(image.unwrap()));
        let input = aces.rgb().to_vec();
        let (corrected, _) =
            scene_correction::apply(aces, &SceneCorrectionParams::default()).unwrap();
        let graded = look::apply(corrected, &LookParams::off()).unwrap();
        let params = FitRangeParams {
            headroom_stops: 0.0,
            peak,
        };
        (fit_range::apply(graded, &params).unwrap(), input)
    }

    fn render(film_rgb: &[f32], peak: DisplayPeak) -> (Vec<f32>, Vec<[f32; 3]>) {
        render_into(film_rgb, peak, DestinationGamut::DisplayP3)
    }

    /// Fit gamut into `gamut`, and the destination values before the map — the pinned
    /// matrix written out here, independently of the stage.
    fn render_into(
        film_rgb: &[f32],
        peak: DisplayPeak,
        gamut: DestinationGamut,
    ) -> (Vec<f32>, Vec<[f32; 3]>) {
        let (image, aces) = fitted(film_rgb, peak);
        let m = match gamut {
            DestinationGamut::DisplayP3 => ACESCG_TO_DISPLAY_P3,
            DestinationGamut::AdobeRgb => ACESCG_TO_ADOBE_RGB,
            DestinationGamut::Bt2020 => ACESCG_TO_BT2020,
        };
        let pre_map = aces
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[r, g, b]| {
                [
                    m[0][0] * r + m[0][1] * g + m[0][2] * b,
                    m[1][0] * r + m[1][1] * g + m[1][2] * b,
                    m[2][0] * r + m[2][1] * g + m[2][2] * b,
                ]
            })
            .collect();
        let params = FitGamutParams { target: gamut };
        let (out, stated) = apply(image, &params).unwrap().into_parts();
        assert_eq!(stated, gamut);
        (out.rgb, pre_map)
    }

    /// Film RGB that the NC film RGB v1 mapping takes to the colour `rgb` states in
    /// `space` — so a test can name a colour by the gamut it is saturated in.
    fn film_for(
        rgb: [f64; 3],
        space: crate::pipeline::colorimetry::definitions::ColorSpace,
    ) -> [f32; 3] {
        use crate::pipeline::colorimetry::definitions::{ACESCG, BRADFORD};
        use crate::pipeline::colorimetry::derive::{inverse, rgb_to_rgb, transform};
        use crate::pipeline::colorimetry::pinned::NC_FILM_RGB_V1_TO_ACESCG;
        let aces = transform(rgb_to_rgb(space, ACESCG, BRADFORD), rgb);
        transform(inverse(NC_FILM_RGB_V1_TO_ACESCG), aces).map(|v| v as f32)
    }

    #[test]
    fn each_gamut_maps_what_it_cannot_hold_and_keeps_what_it_can() {
        // A render into Adobe RGB is gamut-mapped, not tagged: a red P3 holds and Adobe
        // RGB cannot (P3's red primary is the more saturated), and a green Adobe RGB
        // holds and P3 cannot. Each comes out of its own gamut untouched and is mapped
        // onto the other's boundary at its own luminance, with its hue direction kept.
        use crate::pipeline::colorimetry::definitions::{ADOBE_RGB, DISPLAY_P3};
        let cases = [
            (
                film_for([0.9, 0.01, 0.01], DISPLAY_P3),
                DestinationGamut::DisplayP3,
                DestinationGamut::AdobeRgb,
            ),
            (
                film_for([0.06, 0.8, 0.1], ADOBE_RGB),
                DestinationGamut::AdobeRgb,
                DestinationGamut::DisplayP3,
            ),
        ];
        for (film, holds, cannot) in cases {
            let (kept, inside) = render_into(&film, DisplayPeak::SDR, holds);
            let inside = inside[0];
            assert!(
                inside.iter().all(|v| (0.0..=1.0).contains(v)),
                "{holds:?}: {inside:?}"
            );
            assert_eq!(kept, inside.to_vec(), "{holds:?} moved a colour it holds");

            let (mapped, outside) = render_into(&film, DisplayPeak::SDR, cannot);
            let outside = outside[0];
            assert!(
                outside.iter().any(|v| !(0.0..=1.0).contains(v)),
                "{cannot:?} holds {outside:?}; the case does not test the map"
            );
            let mapped: [f32; 3] = mapped.try_into().unwrap();
            assert!(mapped.iter().all(|v| (0.0..=1.0).contains(v)), "{mapped:?}");
            assert!(mapped.contains(&0.0) || mapped.contains(&1.0), "{mapped:?}");
            let y = dot(cannot.luma(), outside);
            assert!((dot(cannot.luma(), mapped) - y).abs() < 1e-6, "{mapped:?}");
            let scales: Vec<f32> = (0..3).map(|c| (mapped[c] - y) / (outside[c] - y)).collect();
            assert!(
                scales
                    .iter()
                    .all(|s| (s - scales[0]).abs() < 1e-4 && *s > 0.0 && *s < 1.0),
                "{cannot:?}: hue direction moved, scales {scales:?}"
            );
        }
    }

    #[test]
    fn the_ceiling_is_the_peak_fit_range_used() {
        // A colour whose P3 rendition has one channel between 1 and the HDR peak, at
        // a luminance under both: out of gamut for SDR, inside the HDR cube. Mapped on
        // one branch and passed through on the other — so the stage read the peak off
        // its input, and the two ceilings disagree exactly where one binds.
        let film = [1.6, 0.3, 0.2];
        let hdr_peak = DisplayPeak::new(1000.0 / 203.0).unwrap();
        let (sdr, p3) = render(&film, DisplayPeak::SDR);
        let (hdr, _) = render(&film, hdr_peak);
        let p3 = p3[0];
        assert!(
            p3.iter().all(|&v| v >= 0.0) && p3.iter().any(|&v| v > 1.0),
            "{p3:?}"
        );
        assert!(luminance(p3) < 1.0, "{p3:?}");

        assert_eq!(hdr, p3.to_vec(), "inside the HDR cube, untouched");
        assert!(sdr.iter().all(|v| (0.0..=1.0).contains(v)), "{sdr:?}");
        assert!(sdr.contains(&1.0), "{sdr:?} did not reach the SDR boundary");
    }

    #[test]
    fn content_above_the_peak_renders_neutral_at_its_own_luminance() {
        // Fit range lets content past its white point exceed the peak, and the ceiling
        // follows it there: at `Y` above the peak the cube `[0, Y]` holds only the
        // neutral at `Y`, so colour desaturates to white — and the level is left for
        // the encoder to clamp and count, never clamped here.
        let (out, p3) = render(&[4.0, 4.0, 4.0, 6.0, 2.5, 1.5], DisplayPeak::SDR);
        for (px, p3) in out.as_chunks::<3>().0.iter().zip(&p3) {
            let y = luminance(*p3);
            assert!(y > 1.0, "{p3:?}");
            assert_eq!(*px, [y; 3], "{p3:?}");
        }
        assert!(p3[1][0] - p3[1][2] > 1.0, "the second pixel is saturated");
    }

    #[test]
    fn a_saturated_sweep_across_the_peak_has_no_step() {
        // The ring the ceiling rule exists to prevent: mapping only up to the peak and
        // leaving brighter pixels alone restores full chroma in one step as luminance
        // crosses it. Swept finely across `Y = 1`, neighbouring outputs must stay
        // neighbours.
        let steps: Vec<f32> = (0..=400)
            .flat_map(|i| {
                let t = 0.6 + i as f32 * 0.002;
                [3.0 * t, 1.0 * t, 0.3 * t]
            })
            .collect();
        let (out, p3) = render(&steps, DisplayPeak::SDR);
        let crossed = p3.first().map(|&p| luminance(p) < 1.0) == Some(true)
            && p3.last().map(|&p| luminance(p) > 1.0) == Some(true);
        assert!(crossed, "the sweep must straddle the peak");
        for (i, pair) in out.as_chunks::<3>().0.windows(2).enumerate() {
            let jump = (0..3)
                .map(|c| (pair[1][c] - pair[0][c]).abs())
                .fold(0.0, f32::max);
            assert!(jump < 0.02, "step {i}: {:?} → {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn an_overflowing_change_of_primaries_is_refused_naming_the_pixel() {
        // Finite through fit range at zero headroom, but the 3×3's first row passes
        // `f32::MAX` (`1.379 × 3.3e38`). Refused, not handed to the encoder.
        let input = [0.18, 0.18, 0.18, 3.3e38, 3.3e38, 3.3e38];
        let (image, aces) = fitted(&input, DisplayPeak::SDR);
        assert!(aces.iter().all(|v| v.is_finite()), "{aces:?}");
        let params = FitGamutParams {
            target: DestinationGamut::DisplayP3,
        };
        let err = apply(image, &params).expect_err("an overflow");
        assert!(err.message().contains("pixel 1"), "{}", err.message());
    }

    #[test]
    fn non_positive_luminance_renders_black() {
        // A saturated colour a wide-gamut linear space holds below zero luminance has
        // no in-gamut rendition at that luminance.
        let (out, p3) = render(&[1.0, -0.5, -0.5], DisplayPeak::SDR);
        assert!(luminance(p3[0]) <= 0.0, "{:?}", p3[0]);
        assert_eq!(out, vec![0.0; 3]);
    }

    fn luminance(rgb: [f32; 3]) -> f32 {
        dot(DISPLAY_P3_LUMA, rgb)
    }

    /// Out-of-gamut against a unit cube: a negative channel, one above the ceiling,
    /// both, and a bright saturated one.
    const OUT_OF_GAMUT: [[f32; 3]; 5] = [
        [-0.2, 0.4, 1.1],
        [1.4, 0.3, 0.05],
        [0.0, 1.2, -0.2],
        [-0.05, 0.1, 0.9],
        [0.95, -0.1, 0.4],
    ];

    #[test]
    fn out_of_gamut_colour_stays_finite_and_lands_on_the_boundary() {
        // At the higher ceiling one of them is inside the cube, so it must come back
        // untouched instead.
        let mut mapped = 0;
        for ceiling in [1.0_f32, 4.0] {
            for rgb in OUT_OF_GAMUT {
                let y = luminance(rgb);
                let out = radial_to_boundary(rgb, y, ceiling);
                assert!(out.iter().all(|v| v.is_finite()), "{rgb:?} → {out:?}");
                assert!(
                    out.iter().all(|v| (0.0..=ceiling).contains(v)),
                    "{rgb:?} → {out:?} at ceiling {ceiling}"
                );
                if rgb.iter().all(|v| (0.0..=ceiling).contains(v)) {
                    assert_eq!(out, rgb);
                } else {
                    mapped += 1;
                    assert!(
                        out.iter().any(|&v| v == 0.0 || v == ceiling),
                        "{rgb:?} → {out:?} did not reach the boundary"
                    );
                }
            }
        }
        assert_eq!(mapped, 2 * OUT_OF_GAMUT.len() - 1);
    }

    #[test]
    fn the_map_keeps_luminance_and_hue_direction() {
        // Hue as a contract: the output's distance from neutral is the input's scaled
        // by one factor in (0, 1) — the same direction in linear RGB, shorter.
        for rgb in OUT_OF_GAMUT {
            let y = luminance(rgb);
            let out = radial_to_boundary(rgb, y, 1.0);
            let scales: Vec<f64> = (0..3)
                .filter(|&c| (f64::from(rgb[c]) - f64::from(y)).abs() > 1e-3)
                .map(|c| (f64::from(out[c]) - f64::from(y)) / (f64::from(rgb[c]) - f64::from(y)))
                .collect();
            assert!(scales.len() >= 2, "{rgb:?}");
            for s in &scales {
                assert!(*s > 0.0 && *s < 1.0, "{rgb:?}: scale {s}");
                assert!((s - scales[0]).abs() < 1e-5, "{rgb:?}: scales {scales:?}");
            }
            assert!((luminance(out) - y).abs() < 1e-6, "{rgb:?} → {out:?}");
        }
    }

    #[test]
    fn in_gamut_colour_and_the_neutral_axis_pass_through_bit_for_bit() {
        for rgb in [
            [0.2, 0.4, 0.6],
            [0.0, 0.5, 1.0],
            [0.18, 0.18, 0.18],
            [1.0, 1.0, 1.0],
            [3.0, 3.0, 3.0],
        ] {
            let y = luminance(rgb);
            let out = radial_to_boundary(rgb, y, y.max(1.0));
            assert_eq!(out.map(f32::to_bits), rgb.map(f32::to_bits), "{rgb:?}");
        }
    }

    #[test]
    fn the_limiting_channel_lands_exactly_on_its_boundary() {
        // Not every out-of-gamut colour's intersection rounds onto the boundary in
        // `f32`; assigning the limiting channel its boundary is what makes it exact.
        // Swept, so the guarantee is exercised wherever rounding alone would miss.
        let mut mapped = 0;
        for i in 0..64 {
            for j in 0..64 {
                let rgb = [
                    1.0 + i as f32 * 0.037,
                    0.3 + j as f32 * 0.011,
                    -0.013 * j as f32,
                ];
                let y = luminance(rgb);
                if y <= 0.0 || y >= 1.0 {
                    continue;
                }
                let out = radial_to_boundary(rgb, y, 1.0);
                mapped += 1;
                assert!(
                    out.contains(&1.0) || out.contains(&0.0),
                    "{rgb:?} → {out:?}"
                );
            }
        }
        assert!(mapped > 1000, "{mapped}");
    }

    #[test]
    fn two_ceilings_disagree_only_where_one_of_them_binds() {
        // The callers share this function and differ only in the ceiling they pass —
        // SDR's display white, HDR's and the gain map's linear headroom. Every colour
        // here sits at a luminance under both, in four groups by which boundary, if
        // any, limits it. Saturated on purpose: on the neutral axis the two ceilings
        // could never be told apart.
        let (sdr, hdr) = (1.0_f32, 1000.0_f32 / 203.0);
        let map = |rgb: [f32; 3]| {
            let y = luminance(rgb);
            assert!(0.0 < y && y < sdr, "{rgb:?}: luminance {y}");
            (
                radial_to_boundary(rgb, y, sdr),
                radial_to_boundary(rgb, y, hdr),
            )
        };
        // Inside both cubes: both untouched.
        for rgb in [[0.2, 0.4, 0.6], [0.9, 0.1, 0.5]] {
            let (a, b) = map(rgb);
            assert_eq!(a, rgb);
            assert_eq!(b, rgb);
        }
        // Only black limits it: the same scale under both ceilings, bit for bit.
        for rgb in [[0.3, 0.6, -0.1], [-0.05, 0.5, 0.9]] {
            let (a, b) = map(rgb);
            assert_ne!(a, rgb);
            assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits), "{rgb:?}");
        }
        // Over SDR's ceiling, inside HDR's: mapped onto SDR's top, untouched by HDR.
        for rgb in [[1.6, 0.3, 0.2], [0.1, 0.2, 3.0]] {
            let (a, b) = map(rgb);
            assert!(a.contains(&sdr), "{rgb:?} → {a:?}");
            assert_eq!(b, rgb);
        }
        // Over both: each lands on its own top, so the two differ.
        for rgb in [[0.1, 0.2, 6.0], [0.05, 0.1, 8.0]] {
            let (a, b) = map(rgb);
            assert!(a.contains(&sdr), "{rgb:?} → {a:?}");
            assert!(b.contains(&hdr), "{rgb:?} → {b:?}");
            assert_ne!(a, b);
        }
    }

    /// Colours for the shared map's golden: [`OUT_OF_GAMUT`], one whose intersection
    /// misses the boundary in `f32` without the limiting channel's assignment, one past
    /// the HDR ceiling, and one above display white.
    const GOLDEN_IN: [[f32; 3]; 8] = [
        [-0.2, 0.4, 1.1],
        [1.4, 0.3, 0.05],
        [0.0, 1.2, -0.2],
        [-0.05, 0.1, 0.9],
        [0.95, -0.1, 0.4],
        [1.0, 0.399_000_02, -0.117],
        [0.1, 0.2, 6.0],
        [4.0, 1.0, 0.1],
    ];

    /// `GOLDEN_IN` at SDR's display white — the last colour above it, so the cube
    /// follows it.
    const GOLDEN_SDR: [u32; 24] = [
        0x00000000, 0x3ebc9dd6, 0x3f4c55a8, 0x3f800000, 0x3ed05aae, 0x3e8b5804, 0x3ed82181,
        0x3f800000, 0x3ea6d1c1, 0x00000000, 0x3ddd6e0d, 0x3f2f4c74, 0x3f2cd2a8, 0x00000000,
        0x3ea497dd, 0x3f6758e9, 0x3ed5be18, 0x00000000, 0x3f19c221, 0x3f1b7dc1, 0x3f800000,
        0x3fcecada, 0x3fcecada, 0x3fcecada,
    ];

    /// At the HDR renderer's and the gain map's linear headroom, 1000/203.
    const GOLDEN_HDR: [u32; 24] = [
        0x00000000, 0x3ebc9dd6, 0x3f4c55a8, 0x3fb33333, 0x3e99999a, 0x3d4ccccd, 0x3e246a26,
        0x3f8fdce1, 0x00000000, 0x00000000, 0x3ddd6e0d, 0x3f2f4c74, 0x3f2cd2a8, 0x00000000,
        0x3ea497dd, 0x3f6758e9, 0x3ed5be18, 0x00000000, 0x3e5480a5, 0x3e9332f1, 0x409da2ae,
        0x40800000, 0x3f800000, 0x3dcccccd,
    ];

    #[test]
    fn golden_the_shared_map_is_bit_identical_at_every_callers_ceiling() {
        // The current chain's `sdr`, `hdr` and `gain_map` render through this
        // function, and no fingerprint reaches past reconstruction — so a change made
        // here for the new flow would move shipped pixels with every other gate green.
        // Pinned at each ceiling a caller passes (HLG's scene-linear `1` is SDR's); a deliberate change recaptures these
        // and re-checks the current chain's output byte for byte.
        let run = |ceiling: f32| -> Vec<u32> {
            GOLDEN_IN
                .iter()
                .flat_map(|&rgb| radial_to_boundary(rgb, luminance(rgb), ceiling).map(f32::to_bits))
                .collect()
        };
        assert_eq!(run(1.0), GOLDEN_SDR);
        assert_eq!(run(1000.0 / 203.0), GOLDEN_HDR);
    }
}
