//! The new rendering chain, composed: scene correction → look → fit range → fit
//! gamut.
//!
//! The chain `--new-flow` selects (`docs/design-update.md` Part 2,
//! `docs/nf-migration.md`), fed by the fixed decode (`algo::fixed`) and rendering
//! into one destination (`cli::convert_frame`, `nf-core/minimal-end-to-end`). Scene
//! correction applies white balance and exposure; the look applies print contrast and
//! the per-channel grade and desaturates near-neutral highlights (the rest of its epic
//! fills it); fit range compresses the scene's range against the destination's peak,
//! and fit gamut maps into the destination's gamut, keeping hue.
//!
//! **The order is carried by the types, not by this function.** Each stage's
//! input is the previous stage's output type, and each of those can be minted
//! only inside the module that produces it — so a chain composed out of order
//! does not compile. That is the whole point of the skeleton: the boundaries are
//! decided once, deliberately, rather than falling out of whichever stage ships
//! first.
//!
//! Entering the chain, crossing each boundary, and **leaving** it
//! (`DisplayReferredImage::into_parts`) all move the pixel buffers, so a type per
//! stage costs no allocation. The exit is the one worth stating separately: the
//! encoder takes `&LinearImage`, so a boundary with only `&`-accessors would force
//! a full-frame copy at the hand-off — ~0.9 GB on a 74.6 MP scan — and the "no
//! allocation" claim would be false at exactly the point it matters most. What each
//! stage then *does* with the buffer it owns is `nf-core/buffer-strategy`'s to
//! settle.
//!
//! # The SDR/HDR branch contract
//!
//! ```text
//! scene correction → look ─┬─ fit range(peak 1) → fit gamut   SDR
//!      (shared)            └─ fit range(peak P) → fit gamut   HDR
//! ```
//!
//! **The chain splits after the look ([`GradedImage`]) and the branches differ in one
//! argument: the display's peak.** A gain map needs the two renditions to agree below
//! diffuse white ([`DIFFUSE_WHITE`]), so every stage that shapes contrast or colour
//! sits above the split, and so does fit range's headroom ([`SharedParams`]). The
//! types carry it: nothing above the split can read a [`DisplayTarget`], and
//! [`render_pair`] cannot set the headroom per branch.
//!
//! What each branch may differ in, measured on the graded image's ACEScg luminance:
//!
//! - **Below diffuse white, only where the SDR cube binds.** Fit range agrees bit for
//!   bit there (its lift is zero below white). Fit gamut's ceiling is the peak, so a
//!   saturated colour with one channel above `1` is mapped onto the SDR cube's top and
//!   left alone in HDR. Everywhere else the two renditions are bit-identical. The
//!   difference is real colour the HDR display can show, and a per-channel gain map
//!   carries it; forcing agreement would mean mapping HDR into the SDR cube.
//! - **Above diffuse white, freely**: fit range lifts toward the peak there.
//!
//! **A single-rendition destination goes through the same split**: [`render`] is one
//! branch of [`render_pair`], rendered by the same function, so skipping a branch
//! skips a call, not a code path. The pair costs one full-frame copy of the graded
//! image.
//!
//! [`DIFFUSE_WHITE`]: crate::algo::fixed::DIFFUSE_WHITE

use crate::pipeline::fit_gamut::{self, DestinationGamut, DisplayReferredImage, FitGamutParams};
use crate::pipeline::fit_range::{self, DisplayPeak, FitRange, FitRangeParams};
use crate::pipeline::look::{self, GradedImage, LookParams, LookSection};
use crate::pipeline::scene_correction::{self, SceneCorrection, SceneCorrectionParams};
use crate::pipeline::working_space::AcesCgImage;
use crate::types::Result;

/// Everything every rendition of a frame shares: the stages above the branch point,
/// and fit range's headroom.
///
/// **The headroom is here, not in [`DisplayTarget`], on purpose.** Reinhard's white
/// point `W = 2^headroom_stops` shapes every luminance, the midtones included, so two
/// renditions at different headrooms disagree below diffuse white and no gain map can
/// pair them. Only the peak may differ, and it cannot act below white.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedParams {
    pub scene_correction: SceneCorrectionParams,
    pub look: LookParams,
    /// Fit range's headroom in stops (the recipe's `fit_range.headroom_stops`).
    pub headroom_stops: f32,
}

/// What a destination states about the display it renders for: the peak fit range
/// compresses against, and the gamut fit gamut maps into. Never a recipe key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayTarget {
    pub peak: DisplayPeak,
    pub gamut: DestinationGamut,
}

/// One rendition's parameters: the shared half and the destination's half.
///
/// Not a recipe type, though parts of it are: `scene_correction`, `look` and the
/// headroom come from the new chain's recipe (`crate::recipe::Recipe`), while the
/// [`DisplayTarget`] is the destination's, so [`crate::recipe::Recipe::chain_params`]
/// adds it. No `Default`, because no destination is implied.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainParams {
    pub shared: SharedParams,
    pub target: DisplayTarget,
}

/// What [`render`] produced: the display-referred image, and what the chain applied
/// to reach it.
pub struct Rendered {
    pub image: DisplayReferredImage,
    /// Each stage, in the order `render` ran it, with what it applied — the report's
    /// account of the chain. Built inside this module so a stage inserted, moved or
    /// renamed here cannot leave the report listing the old chain, and read off the
    /// values each stage applied, so an operation that moved no pixel is reported as
    /// `"identity"`.
    pub applied: [(&'static str, &'static str); 4],
    /// Scene correction's values as applied to this frame.
    pub scene_correction: SceneCorrection,
    /// The look's controls as applied.
    pub look: LookSection,
    /// Fit range's operator and its arguments, as resolved.
    pub fit_range: FitRange,
}

/// An SDR and an HDR rendition of one frame, split from one graded image — what a
/// gain map is built from. Each carries the full account of its own render; the
/// shared stages' entries are identical by construction.
#[cfg_attr(not(test), allow(dead_code))] // the gain-map destination (`nf-destinations/preset-set`)
pub struct RenderedPair {
    pub sdr: Rendered,
    pub hdr: Rendered,
}

/// Render an [`AcesCgImage`] through the new chain, for one destination.
///
/// **Today this is scene correction's per-channel gains, the look's print contrast,
/// per-channel grade and highlight desaturation, fit range's luminance operator, and
/// the destination's 3×3 with the radial gamut map.** Nothing is clamped: content fit
/// range left above the peak rides through to the encoder, which is the only place
/// clamping happens. The gamut map is
/// a policy, not a clamp, and what it discards (a colour at `Y ≤ 0`, written black) is
/// not counted there. A non-finite sample is **refused**, naming the pixel: by fit
/// range on input, by fit gamut if the change of primaries overflows.
///
/// **Fallible by construction.** Every stage's signature returns a `Result` and so
/// does this, and today every stage can fail. That is the
/// point of settling the boundaries once: every stage this chain will host has a
/// *fallible* counterpart in the shipped code — `render_split::display_source`,
/// `sdr::render` (which errors on a non-finite sample) and `hdr::render_linear` all
/// return `Result` — so a stage that gains its arithmetic would otherwise change its
/// signature, this function's, every call site and every test here. The cost for a
/// stage that cannot fail is an `Ok` wrapper.
///
/// A single-rendition destination goes through the same branch point as a pair: it
/// is [`render_pair`] with one branch, not a second code path (see the module docs).
pub fn render(image: AcesCgImage, params: &ChainParams) -> Result<Rendered> {
    let (graded, scene_correction) = grade(image, &params.shared)?;
    display(graded, scene_correction, &params.shared, params.target)
}

/// Render an SDR and an HDR rendition of one frame, for a gain map: one graded image,
/// split once, and each branch rendered by the same function with its own peak.
///
/// **The contract** (see the module docs): below diffuse white the two are identical
/// wherever the SDR cube leaves a pixel alone, and differ only where its ceiling
/// binds. Both renditions share one gamut, because a gain map is a ratio between them.
///
/// Costs one full-frame copy of the graded image — the branch point's only
/// allocation — on top of what [`render`] holds.
#[cfg_attr(not(test), allow(dead_code))] // the gain-map destination (`nf-destinations/preset-set`)
pub fn render_pair(
    image: AcesCgImage,
    shared: &SharedParams,
    gamut: DestinationGamut,
    hdr_peak: DisplayPeak,
) -> Result<RenderedPair> {
    let (graded, scene_correction) = grade(image, shared)?;
    let hdr_source = graded.split();
    let branch = |peak| DisplayTarget { peak, gamut };
    Ok(RenderedPair {
        sdr: display(graded, scene_correction, shared, branch(DisplayPeak::SDR))?,
        hdr: display(hdr_source, scene_correction, shared, branch(hdr_peak))?,
    })
}

/// Above the branch point: scene correction, then the look. Nothing here may read the
/// destination — it has no way to.
fn grade(image: AcesCgImage, shared: &SharedParams) -> Result<(GradedImage, SceneCorrection)> {
    let (corrected, scene_correction) = scene_correction::apply(image, &shared.scene_correction)?;
    Ok((look::apply(corrected, &shared.look)?, scene_correction))
}

/// Below the branch point: fit range, then fit gamut, against one display.
fn display(
    graded: GradedImage,
    scene_correction: SceneCorrection,
    shared: &SharedParams,
    target: DisplayTarget,
) -> Result<Rendered> {
    let fit_range_params = FitRangeParams {
        headroom_stops: shared.headroom_stops,
        peak: target.peak,
    };
    let fit_gamut_params = FitGamutParams {
        target: target.gamut,
    };
    let fitted = fit_range::apply(graded, &fit_range_params)?;
    let image = fit_gamut::apply(fitted, &fit_gamut_params)?;
    let fit_range = fit_range_params.resolved();
    Ok(Rendered {
        image,
        applied: [
            ("scene_correction", scene_correction.applied()),
            ("look", shared.look.applied()),
            ("fit_range", fit_range.operator),
            ("fit_gamut", fit_gamut_params.applied()),
        ],
        scene_correction,
        look: shared.look.section,
        fit_range,
    })
}

/// The branch contract as a check over one rendered pair — shared by the unit tests
/// and the real-frame probe (`pipeline::branch_probe`).
#[cfg(test)]
pub(in crate::pipeline) mod contract {
    use super::{SharedParams, grade};
    use crate::algo::fixed::DIFFUSE_WHITE;
    use crate::pipeline::colorimetry::dot;
    use crate::pipeline::colorimetry::pinned::ACESCG_LUMA;
    use crate::pipeline::fit_gamut::{DestinationGamut, radial_to_boundary};
    use crate::pipeline::fit_range::DisplayPeak;
    use crate::pipeline::working_space::AcesCgImage;

    /// The graded pixels `shared` produces from `image` — what both branches start
    /// from, and what [`check`] reads "below white" off.
    pub fn graded(image: AcesCgImage, shared: &SharedParams) -> Vec<f32> {
        grade(image, shared)
            .unwrap()
            .0
            .into_buffer()
            .into_linear()
            .rgb
    }

    /// How a pair's pixels below diffuse white sit against the contract.
    #[derive(Debug, Default)]
    pub struct Agreement {
        /// Pixels whose graded luminance is at or below diffuse white.
        pub below_white: usize,
        /// …of which the two renditions are bit-identical.
        pub identical: usize,
        /// …of which they differ, and the SDR pixel is **exactly** the HDR pixel
        /// mapped against the SDR ceiling: the one permitted difference.
        pub sdr_bound: usize,
        /// …of which they differ with the HDR pixel on its own cube's boundary too, so
        /// the SDR pixel cannot be re-derived from it, and the SDR pixel reaches its
        /// cube's top (a channel at or above `1`). Permitted, but checked only loosely,
        /// so a test states how many it expects.
        pub both_bound: usize,
        /// …of which they differ for any other reason: the contract is broken. Pixel
        /// indices.
        pub violations: Vec<usize>,
    }

    /// Check `sdr` and `hdr` (display-referred, in `gamut`, the HDR one fitted to
    /// `peak`) against the contract, with `graded` the ACEScg pixels both came from.
    ///
    /// "Below white" is read exactly as fit range reads it — the ACEScg luminance of
    /// its input at or under [`DIFFUSE_WHITE`] — so the check cannot drift from the
    /// operator by a rounding. Below white fit range's output is the same for both
    /// peaks, so where the HDR cube leaves a pixel alone the HDR pixel **is** fit
    /// gamut's pre-map value, and the SDR pixel must be exactly that value mapped
    /// against the SDR ceiling. That ceiling is `max(1, Y)` in the *destination's*
    /// luminance, which can exceed `1` where the ACEScg luminance does not: a saturated
    /// blue at ACEScg `0.999` reads `1.012` in Display P3 and renders neutral in SDR.
    pub fn check(
        graded: &[f32],
        sdr: &[f32],
        hdr: &[f32],
        gamut: DestinationGamut,
        peak: DisplayPeak,
    ) -> Agreement {
        assert!(graded.len() == sdr.len() && sdr.len() == hdr.len());
        let luma = gamut.luma();
        let bits = |px: [f32; 3]| px.map(f32::to_bits);
        let mut out = Agreement::default();
        let pixels = graded
            .as_chunks::<3>()
            .0
            .iter()
            .zip(sdr.as_chunks::<3>().0)
            .zip(hdr.as_chunks::<3>().0);
        for (index, ((g, s), h)) in pixels.enumerate() {
            if dot(*g, ACESCG_LUMA) > DIFFUSE_WHITE {
                continue;
            }
            out.below_white += 1;
            let hdr_bound = h.iter().any(|v| *v <= 0.0 || *v >= peak.value());
            if bits(*s) == bits(*h) {
                out.identical += 1;
            } else if !hdr_bound && bits(*s) == bits(radial_to_boundary(*h, dot(luma, *h), 1.0)) {
                out.sdr_bound += 1;
            } else if hdr_bound && s.iter().any(|v| *v >= 1.0) {
                out.both_bound += 1;
            } else {
                out.violations.push(index);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::{FilmRgbImage, reconstruct};
    use crate::pipeline::colorimetry::dot;
    use crate::pipeline::colorimetry::pinned::{
        ACESCG_TO_ADOBE_RGB, ACESCG_TO_DISPLAY_P3, ADOBE_RGB_LUMA, DISPLAY_P3_LUMA,
    };
    use crate::pipeline::fit_range::RangeFittedImage;
    use crate::pipeline::gain_ratio;
    use crate::pipeline::scene_correction::WhiteBalance;
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::{FilmBase, LinearImage, Reconstruction};

    /// Every stage at its identity — fit range at zero headroom — so a test sees the
    /// wiring and the destination matrix rather than the operator.
    fn params() -> ChainParams {
        ChainParams {
            shared: SharedParams {
                scene_correction: SceneCorrectionParams::default(),
                look: LookParams::off(),
                headroom_stops: 0.0,
            },
            target: DisplayTarget {
                peak: DisplayPeak::SDR,
                gamut: DestinationGamut::DisplayP3,
            },
        }
    }

    /// [`params`] with fit range at the recipe's default headroom — what a run gets.
    fn shipped_params() -> ChainParams {
        let mut p = params();
        p.shared.headroom_stops = crate::types::DEFAULT_HEADROOM_STOPS;
        p
    }

    /// Fit range's parameters as `display` builds them from `p`.
    fn fit_range_params(p: &ChainParams) -> FitRangeParams {
        FitRangeParams {
            headroom_stops: p.shared.headroom_stops,
            peak: p.target.peak,
        }
    }

    /// An `AcesCgImage` whose *film RGB* input was exactly `rgb` — including a
    /// non-finite value — through the real working-space mapper.
    fn aces_from(width: u32, height: u32, rgb: &[f32], ir: Option<Vec<f32>>) -> AcesCgImage {
        let film =
            FilmRgbImage::fixture(LinearImage::new(width, height, rgb.to_vec(), ir).unwrap());
        map_nc_film_rgb_v1(film)
    }

    /// Bit patterns, not values: `NaN != NaN`, so an `==` comparison would pass
    /// over exactly the samples this asserts survive untouched.
    fn bits(pixels: &[f32]) -> Vec<u32> {
        pixels.iter().map(|v| v.to_bits()).collect()
    }

    /// The chain up to fit range with every stage at its identity.
    fn through_fit_range(image: AcesCgImage) -> RangeFittedImage {
        let p = params();
        let corrected = scene_correction::apply(image, &p.shared.scene_correction)
            .unwrap()
            .0;
        let graded = look::apply(corrected, &p.shared.look).unwrap();
        fit_range::apply(graded, &fit_range_params(&p)).unwrap()
    }

    /// The expected output of fit gamut, written out independently of the stage:
    /// the pinned matrix, row by row, in the same order of operations.
    fn to_p3(rgb: &[f32]) -> Vec<f32> {
        to_destination(rgb, ACESCG_TO_DISPLAY_P3)
    }

    /// [`to_p3`] for any destination's pinned matrix.
    fn to_destination(rgb: &[f32], m: [[f32; 3]; 3]) -> Vec<f32> {
        rgb.as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| {
                [
                    m[0][0] * p[0] + m[0][1] * p[1] + m[0][2] * p[2],
                    m[1][0] * p[0] + m[1][1] * p[1] + m[1][2] * p[2],
                    m[2][0] * p[0] + m[2][1] * p[1] + m[2][2] * p[2],
                ]
            })
            .collect()
    }

    /// Ordinary values, both ends of the working range, and the awkward finite ones:
    /// above 1.0 (unclamped is the contract), below 0.0 (a wide-gamut linear space
    /// contains them), and far past any white point.
    const FINITE: [f32; 9] = [0.0, 0.18, 1.0, 5.0, -0.25, 1e6, 0.5, 0.25, -1e-3];

    /// [`FINITE`]'s shape with non-finite samples in pixels 1 and 2, which fit range
    /// refuses.
    const AWKWARD: [f32; 9] = [
        0.0,
        0.18,
        1.0,
        5.0,
        -0.25,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        0.5,
    ];

    #[test]
    fn the_first_three_stages_are_a_bit_exact_identity() {
        let aces = aces_from(3, 1, &FINITE, None);
        let before = bits(aces.rgb());

        let out = through_fit_range(aces).into_buffer().into_linear();

        assert_eq!(bits(&out.rgb), before, "a stage moved a pixel");
    }

    #[test]
    fn a_one_ulp_move_in_the_rendered_output_is_visible() {
        // Falsifiability control for the identity test above, which is evidence only
        // if its comparison can fail on the values this chain actually carries. The
        // comparison is against the **unperturbed output**, not against the input:
        // asserting against the input would also pass on any chain that stops being
        // an identity, i.e. exactly when this control stops exercising ULP detection.
        // Making a stage non-identity needs a deliberately broken build, done by hand
        // when the chain landed (`docs/progress/nf-core.md`).
        let aces = aces_from(
            3,
            1,
            &[0.0, 0.18, 1.0, 5.0, -0.25, 0.5, 0.25, 0.75, 0.9],
            None,
        );
        let rendered = bits(&through_fit_range(aces).into_buffer().into_linear().rgb);
        let mut moved = rendered.clone();

        // No NaN in this vector, so `next_up` genuinely steps.
        let sample = f32::from_bits(moved[4]);
        assert!(sample.is_finite(), "the perturbed sample must be finite");
        moved[4] = sample.next_up().to_bits();

        assert_ne!(
            moved, rendered,
            "a one-ULP move must be visible to the compare"
        );
    }

    #[test]
    fn at_its_identities_the_chain_is_the_destination_matrix_then_the_gamut_map() {
        // Every stage above fit gamut at its identity: a colour the pinned ACEScg →
        // destination 3×3 puts inside `[0, max(1, Y)]` comes out bit for bit, and only
        // the others move — onto the boundary, nothing clamped past it. For every
        // destination gamut, each with its own matrix and luminance.
        let rgb = [0.0, 0.18, 1.0, 0.2, 0.4, 0.6, 0.9, 0.05, -0.3];
        for (gamut, matrix, luma) in [
            (
                DestinationGamut::DisplayP3,
                ACESCG_TO_DISPLAY_P3,
                DISPLAY_P3_LUMA,
            ),
            (
                DestinationGamut::AdobeRgb,
                ACESCG_TO_ADOBE_RGB,
                ADOBE_RGB_LUMA,
            ),
        ] {
            let aces = aces_from(3, 1, &rgb, None);
            let expected = to_destination(aces.rgb(), matrix);
            let mut p = params();
            p.target.gamut = gamut;
            let rendered = render(aces, &p).unwrap();
            assert_eq!(
                rendered.applied[3].1,
                FitGamutParams { target: gamut }.applied()
            );
            let (out, stated) = rendered.image.into_parts();
            assert_eq!(stated, gamut);
            let (mut kept, mut mapped) = (0, 0);
            for (px, want) in out
                .rgb
                .as_chunks::<3>()
                .0
                .iter()
                .zip(expected.as_chunks::<3>().0)
            {
                let y = dot(*want, luma);
                let ceiling = y.max(1.0);
                if want.iter().all(|v| (0.0..=ceiling).contains(v)) {
                    assert_eq!(bits(px), bits(want), "{gamut:?}");
                    kept += 1;
                } else {
                    assert!(px.iter().all(|v| (0.0..=ceiling).contains(v)), "{px:?}");
                    assert!(px.contains(&0.0) || px.contains(&ceiling), "{px:?}");
                    mapped += 1;
                }
            }
            assert!(
                kept > 0 && mapped > 0,
                "{gamut:?}: {kept} kept, {mapped} mapped"
            );
        }
    }

    #[test]
    fn a_non_finite_sample_is_refused_naming_the_first_pixel() {
        // At the identity headroom too: whether a frame renders must not depend on
        // the setting.
        for p in [params(), shipped_params()] {
            let aces = aces_from(3, 1, &AWKWARD, None);
            let err = render(aces, &p).err().expect("a non-finite sample");
            let msg = err.message();
            assert!(
                msg.contains("fit range") && msg.contains("pixel 1"),
                "{msg}"
            );
        }
    }

    #[test]
    fn the_shipped_fit_range_compresses_highlights_and_keeps_mid_grey_and_hue() {
        let aces = aces_from(
            3,
            1,
            &[0.18, 0.18, 0.18, 4.0, 4.0, 4.0, 3.0, 1.5, 0.5],
            None,
        );
        let input = aces.rgb().to_vec();
        let p = shipped_params();
        let corrected = scene_correction::apply(aces, &p.shared.scene_correction)
            .unwrap()
            .0;
        let graded = look::apply(corrected, &p.shared.look).unwrap();
        let out = fit_range::apply(graded, &fit_range_params(&p))
            .unwrap()
            .into_buffer()
            .into_linear()
            .rgb;
        for c in 0..3 {
            assert!((out[c] - input[c]).abs() < 1e-5, "mid-grey moved: {out:?}");
            assert!(out[3 + c] < 0.8 * input[3 + c], "not compressed: {out:?}");
        }
        // One scale for all three channels: the ratios survive.
        for c in 0..3 {
            let ratio = out[6 + c] / input[6 + c];
            assert!(
                (ratio - out[6] / input[6]).abs() < 1e-5,
                "hue moved: {out:?}"
            );
        }
        let rendered = render(aces_from(1, 1, &[0.5, 0.5, 0.5], None), &p).unwrap();
        assert_eq!(rendered.applied[2], ("fit_range", fit_range::OPERATOR));
        assert_eq!(rendered.fit_range.display_peak, DisplayPeak::SDR);
    }

    #[test]
    fn the_chain_maps_out_of_gamut_colour_and_clamps_nothing() {
        // Finite values only, so the assertions below cannot be satisfied by an
        // infinity. A bright neutral lands above 1.0 and stays there — clamping is the
        // encoder's alone — while a colour the matrix puts below zero in P3 is mapped
        // onto the cube rather than left for the encoder's per-channel clip.
        let rgb = [4.0, 4.0, 4.0, 0.9, 0.05, -0.3];
        let aces = aces_from(2, 1, &rgb, None);
        let p3 = to_p3(aces.rgb());
        assert!(p3[3..].iter().any(|v| *v < 0.0), "{:?}", &p3[3..]);
        let (out, _) = render(aces, &params()).unwrap().image.into_parts();

        assert!(out.rgb.iter().all(|v| v.is_finite()));
        assert!(out.rgb[..3].iter().all(|v| *v > 1.0), "{:?}", &out.rgb[..3]);
        assert!(
            out.rgb[3..].iter().all(|v| *v >= 0.0),
            "{:?}",
            &out.rgb[3..]
        );
        assert!(out.rgb[3..].contains(&0.0), "{:?}", &out.rgb[3..]);
        // Mapped, not discarded: the luminance survives, so this is not the black a
        // colour at `Y ≤ 0` gets.
        let luminance = |px: &[f32]| dot([px[0], px[1], px[2]], DISPLAY_P3_LUMA);
        let y = luminance(&p3[3..]);
        assert!(y > 0.05, "{y}");
        assert!(
            (luminance(&out.rgb[3..]) - y).abs() < 1e-6,
            "{:?}",
            &out.rgb[3..]
        );
    }

    #[test]
    fn a_neutral_stays_neutral_through_the_destination_matrix() {
        // The adapted matrix maps the ACES white to D65 white, so an ACEScg neutral
        // lands on the P3 neutral axis. Tolerance, not bits: the rows sum to 1 only to
        // f32 rounding.
        let aces = aces_from(1, 1, &[0.18, 0.18, 0.18], None);
        let neutral = aces.rgb().to_vec();
        assert!(
            (neutral[0] - neutral[1]).abs() < 1e-6 && (neutral[1] - neutral[2]).abs() < 1e-6,
            "the film-RGB neutral must reach the chain as an ACEScg neutral: {neutral:?}"
        );

        let (out, _) = render(aces, &params()).unwrap().image.into_parts();
        for c in 0..3 {
            assert!(
                (out.rgb[c] - neutral[0]).abs() < 1e-5,
                "channel {c}: {} vs {}",
                out.rgb[c],
                neutral[0]
            );
        }
    }

    #[test]
    fn leaving_the_chain_preserves_everything_the_encoder_reads() {
        // The exit boundary is a consuming unwrap so the hand-off moves rather than
        // copies a full-frame buffer. What it must not do is lose anything on the way
        // out, the carried IR plane included: the design says carry the plane rather
        // than consume it, and today's SDR render is the counter-example the new
        // chain declines to copy.
        let ir = vec![0.1, 0.2, 0.3, 0.4];
        let aces = aces_from(2, 2, &[0.25; 12], Some(ir.clone()));
        let expected = bits(&to_p3(aces.rgb()));

        let (linear, _) = render(aces, &params()).unwrap().image.into_parts();

        assert_eq!(linear.width, 2);
        assert_eq!(linear.height, 2);
        assert_eq!(bits(&linear.rgb), expected);
        assert_eq!(linear.ir, Some(ir));
    }

    #[test]
    fn an_ir_free_input_stays_ir_free() {
        // Falsifiability for the test above: the plane must be carried, not minted.
        let (out, _) = render(aces_from(2, 2, &[0.5; 12], None), &params())
            .unwrap()
            .image
            .into_parts();
        assert_eq!(out.ir, None);
    }

    #[test]
    fn the_chain_is_producer_agnostic() {
        // The chain's input is an `AcesCgImage` regardless of which reconstruction
        // produced it — and deliberately *not* only the one a real `--new-flow` run
        // takes (`algo::fixed`): the boundary is the type, so what produces it stays
        // free to change.
        type Producer = fn(&LinearImage, &FilmBase) -> crate::algo::FilmRgbImage;
        let producers: [(&str, Producer); 2] = [
            ("reconstruct", |img, base| {
                reconstruct(img, base, &Reconstruction::default())
                    .unwrap()
                    .0
            }),
            ("fixed::decode", |img, base| {
                crate::algo::fixed::decode(img, base, &Default::default())
                    .unwrap()
                    .0
            }),
        ];
        for (name, produce) in producers {
            let base = FilmBase::from([0.5, 0.5, 0.5]);
            let img = LinearImage::new(2, 1, vec![0.1, 0.2, 0.3, 0.4, 0.2, 0.1], None).unwrap();
            let aces = map_nc_film_rgb_v1(produce(&img, &base));
            let expected = bits(&to_p3(aces.rgb()));

            let (out, _) = render(aces, &params()).unwrap().image.into_parts();

            assert_eq!(bits(&out.rgb), expected, "{name}");
        }
    }

    #[test]
    fn the_stage_order_is_the_one_the_types_allow() {
        // The chain written out by hand. It compiles only in this order: each
        // stage takes the previous stage's output type, and nothing outside the
        // producing module can mint one — so changing a stage's position is a
        // compile error here. The runtime half — that scene correction's gains land
        // *before* the change of primaries — is
        // `scene_correction_runs_before_the_change_of_primaries`.
        let aces = aces_from(1, 1, &[0.2, 0.4, 0.6], None);
        let p = params();

        let corrected = scene_correction::apply(aces, &p.shared.scene_correction)
            .unwrap()
            .0;
        let graded = look::apply(corrected, &p.shared.look).unwrap();
        let fitted = fit_range::apply(graded, &fit_range_params(&p)).unwrap();
        let out: DisplayReferredImage = fit_gamut::apply(
            fitted,
            &FitGamutParams {
                target: p.target.gamut,
            },
        )
        .unwrap();

        assert_eq!(out.into_parts().0.width, 1);
    }

    #[test]
    fn scene_correction_runs_before_the_change_of_primaries() {
        // Per-channel gains do not commute with the 3×3, so the two orders give
        // different pixels — and only one of them is scene correction's contract:
        // white balance acts on ACEScg channels, before the destination's primaries.
        let rgb = [0.2, 0.4, 0.6];
        let aces = aces_from(1, 1, &rgb, None);
        let gains = [2.0f32, 1.0, 0.5];
        let balanced: Vec<f32> = aces
            .rgb()
            .iter()
            .enumerate()
            .map(|(i, v)| v * gains[i % 3])
            .collect();
        let after: Vec<f32> = to_p3(aces.rgb())
            .iter()
            .enumerate()
            .map(|(i, v)| v * gains[i % 3])
            .collect();
        let mut p = params();
        p.shared.scene_correction.white_balance = WhiteBalance::Explicit(gains);

        let rendered = render(aces, &p).unwrap();
        let (out, _) = rendered.image.into_parts();

        assert_eq!(bits(&out.rgb), bits(&to_p3(&balanced)));
        assert_ne!(
            bits(&out.rgb),
            bits(&after),
            "the other order must differ here"
        );
        assert_eq!(rendered.applied[0], ("scene_correction", "white-balance"));
    }

    #[test]
    fn a_boundary_type_can_be_minted_only_by_its_own_stage() {
        // What the order above rests on: each boundary wraps its payload in a field
        // private to the producing module, so no other module can build one. Nothing
        // else pins that — a `pub` added to one of these fields compiles and passes
        // every gate while silently opening the boundary — and a compile-fail harness
        // would cost a dev-dependency for one assertion, so read the sources back
        // instead.
        //
        // A private field is necessary but not sufficient: a second minting route
        // inside the module — a `pub(in crate::pipeline) fn new`, or an
        // `impl From<WorkingBuffer>` — would leave the declaration untouched. So count
        // the construction sites too: the tuple constructor may appear exactly twice —
        // the declaration and `apply` — and never spelled `Self(…)`. The `Self(` check
        // is textual over the whole module, so a helper newtype there must be built by
        // name (`DisplayPeak(1.0)`), not `Self(..)`. `GradedImage::split` is the one
        // counted exception: it copies an image `apply` minted.
        for (source, name, declaration) in [
            (
                include_str!("scene_correction.rs"),
                "SceneReferredImage",
                "pub struct SceneReferredImage(WorkingBuffer);",
            ),
            (
                include_str!("look.rs"),
                "GradedImage",
                "pub struct GradedImage(WorkingBuffer);",
            ),
            (
                include_str!("fit_range.rs"),
                "RangeFittedImage",
                "pub struct RangeFittedImage(WorkingBuffer, DisplayPeak);",
            ),
            (
                include_str!("fit_gamut.rs"),
                "DisplayReferredImage",
                "pub struct DisplayReferredImage(WorkingBuffer, DestinationGamut);",
            ),
        ] {
            assert!(
                source.contains(declaration),
                "the payload field must stay private to its stage: `{declaration}`"
            );
            // `GradedImage` has one more: `split`, the branch point's copy of an image
            // the stage already minted.
            let expected = if name == "GradedImage" { 3 } else { 2 };
            assert_eq!(
                source.matches(&format!("{name}(")).count(),
                expected,
                "`{name}` must be minted in one place only: its declaration and `apply`"
            );
            assert!(
                !source.contains("Self("),
                "`{name}`'s module must not mint one through `Self(…)` either"
            );
        }
    }

    // --- the branch contract ---------------------------------------------------

    /// An HDR display's peak: 1000 nits over 203-nit reference white.
    fn hdr_peak() -> DisplayPeak {
        DisplayPeak::new(1000.0 / 203.0).unwrap()
    }

    /// Shipped-like shared parameters with every pre-branch stage acting: a white
    /// balance, an exposure, the default look and the default headroom.
    fn shared_acting() -> SharedParams {
        SharedParams {
            scene_correction: SceneCorrectionParams {
                white_balance: WhiteBalance::Explicit([1.1, 1.0, 0.9]),
                exposure: 0.25,
            },
            look: LookParams {
                section: LookSection::default(),
                linearization: crate::algo::fixed::LINEARIZATION,
            },
            headroom_stops: crate::types::DEFAULT_HEADROOM_STOPS,
        }
    }

    /// A film-RGB grid from deep shadow to far past white on every channel
    /// independently: neutrals, near-neutrals the look reaches, saturated colours with
    /// one channel over `1` at a luminance under white, and highlights.
    fn grid() -> AcesCgImage {
        const LEVELS: [f32; 10] = [0.0, 0.01, 0.05, 0.18, 0.45, 0.8, 0.95, 1.3, 2.0, 5.0];
        let rgb: Vec<f32> = LEVELS
            .iter()
            .flat_map(|&r| {
                LEVELS
                    .iter()
                    .flat_map(move |&g| LEVELS.iter().flat_map(move |&b| [r, g, b]))
            })
            .collect();
        aces_from((rgb.len() / 3) as u32, 1, &rgb, None)
    }

    fn pixels_of(rendered: Rendered) -> LinearImage {
        rendered.image.into_parts().0
    }

    #[test]
    fn the_pair_agrees_below_white_except_where_the_sdr_cube_binds() {
        // At the shipped headroom, and at zero, where fit range is the identity and
        // pixels reach fit gamut at luminance near white.
        for stops in [crate::types::DEFAULT_HEADROOM_STOPS, 0.0] {
            let mut shared = shared_acting();
            shared.headroom_stops = stops;
            pair_agrees_below_white(&shared);
        }
    }

    fn pair_agrees_below_white(shared: &SharedParams) {
        let shared = shared.clone();
        let graded = contract::graded(grid(), &shared);
        let pair = render_pair(grid(), &shared, DestinationGamut::DisplayP3, hdr_peak()).unwrap();
        let (sdr, hdr) = (pixels_of(pair.sdr), pixels_of(pair.hdr));

        let agreement = contract::check(
            &graded,
            &sdr.rgb,
            &hdr.rgb,
            DestinationGamut::DisplayP3,
            hdr_peak(),
        );
        assert!(
            agreement.violations.is_empty(),
            "{} of {} below-white pixels differ without the SDR cube binding, first {:?}",
            agreement.violations.len(),
            agreement.below_white,
            &agreement.violations[..agreement.violations.len().min(5)]
        );
        // Not vacuous: both kinds of below-white pixel are in the grid, and so are
        // pixels above white, where the branches are free to differ and do.
        assert!(
            agreement.identical > 100 && agreement.sdr_bound > 0,
            "{agreement:?}"
        );
        // Most differences are re-derived exactly from the HDR pixel. The rest are the
        // grid's most saturated blues, which the look's print contrast pushes onto a face
        // of the HDR cube too (the black face, or the peak at ACEScg blue ≈ 6.7 with a
        // luminance under white), so only the loose rule applies. Real frames have none
        // (`pipeline::branch_probe` asserts it); a bound, not a count, because `powf`
        // differs by target.
        assert!(agreement.both_bound < agreement.sdr_bound, "{agreement:?}");
        let differing_samples = sdr
            .rgb
            .iter()
            .zip(&hdr.rgb)
            .filter(|(s, h)| s.to_bits() != h.to_bits())
            .count();
        assert!(
            differing_samples > 3 * agreement.sdr_bound,
            "only {differing_samples} samples differ in all, against {} SDR-bound \
             below-white pixels",
            agreement.sdr_bound
        );
    }

    #[test]
    fn the_contract_check_fails_when_the_headroom_is_set_per_branch() {
        // Falsifiability: the headroom shapes the midtones, which is why it is shared.
        // Two renders at different headrooms (which `render_pair` cannot express) break
        // the contract far below white, and the check must say so.
        let shared = shared_acting();
        let graded = contract::graded(grid(), &shared);
        let sdr = pixels_of(
            render(
                grid(),
                &ChainParams {
                    shared: shared.clone(),
                    target: DisplayTarget {
                        peak: DisplayPeak::SDR,
                        gamut: DestinationGamut::DisplayP3,
                    },
                },
            )
            .unwrap(),
        );
        let mut other = shared.clone();
        other.headroom_stops = 4.0;
        let hdr = pixels_of(
            render(
                grid(),
                &ChainParams {
                    shared: other,
                    target: DisplayTarget {
                        peak: hdr_peak(),
                        gamut: DestinationGamut::DisplayP3,
                    },
                },
            )
            .unwrap(),
        );

        let agreement = contract::check(
            &graded,
            &sdr.rgb,
            &hdr.rgb,
            DestinationGamut::DisplayP3,
            hdr_peak(),
        );
        assert!(
            agreement.violations.len() > agreement.below_white / 2,
            "{agreement:?}"
        );
    }

    #[test]
    fn the_contract_check_fails_when_a_pre_branch_stage_runs_on_one_branch_only() {
        // Falsifiability: the look moved below the split — applied to the SDR branch
        // only. It starts a stop under white, so near-neutral pixels there disagree.
        let shared = shared_acting();
        let graded = contract::graded(grid(), &shared);
        let target = |peak| DisplayTarget {
            peak,
            gamut: DestinationGamut::DisplayP3,
        };
        let sdr = pixels_of(
            render(
                grid(),
                &ChainParams {
                    shared: shared.clone(),
                    target: target(DisplayPeak::SDR),
                },
            )
            .unwrap(),
        );
        let mut without_look = shared.clone();
        without_look.look = LookParams::off();
        let hdr = pixels_of(
            render(
                grid(),
                &ChainParams {
                    shared: without_look,
                    target: target(hdr_peak()),
                },
            )
            .unwrap(),
        );

        let agreement = contract::check(
            &graded,
            &sdr.rgb,
            &hdr.rgb,
            DestinationGamut::DisplayP3,
            hdr_peak(),
        );
        assert!(!agreement.violations.is_empty(), "{agreement:?}");
    }

    #[test]
    fn a_single_destination_is_one_branch_of_the_pair() {
        // No second code path: `render` for either peak is bit-identical to that
        // branch of `render_pair`, report included.
        let shared = shared_acting();
        let pair = render_pair(grid(), &shared, DestinationGamut::DisplayP3, hdr_peak()).unwrap();
        for (branch, peak) in [(pair.sdr, DisplayPeak::SDR), (pair.hdr, hdr_peak())] {
            let single = render(
                grid(),
                &ChainParams {
                    shared: shared.clone(),
                    target: DisplayTarget {
                        peak,
                        gamut: DestinationGamut::DisplayP3,
                    },
                },
            )
            .unwrap();
            assert_eq!(single.applied, branch.applied);
            assert_eq!(single.fit_range, branch.fit_range);
            assert_eq!(single.fit_range.display_peak, peak);
            assert_eq!(single.scene_correction, branch.scene_correction);
            assert_eq!(bits(&pixels_of(single).rgb), bits(&pixels_of(branch).rgb));
        }
    }

    #[test]
    fn a_gain_map_from_the_pair_rebuilds_the_hdr_rendition() {
        let pair = render_pair(
            grid(),
            &shared_acting(),
            DestinationGamut::DisplayP3,
            hdr_peak(),
        )
        .unwrap();
        let (sdr, hdr) = (pixels_of(pair.sdr), pixels_of(pair.hdr));
        let gains = gain_ratio::between(&sdr, &hdr, 1.0 / 64.0).unwrap();
        assert!(!gains.range().flat, "{:?}", gains.range());
        // Below 1 too: the SDR cube lifted a channel the HDR rendition keeps lower.
        assert!(
            gains.range().min.iter().any(|g| *g < 1.0),
            "{:?}",
            gains.range()
        );
        let rebuilt = gains.apply_to(&sdr).unwrap();
        for (index, (got, want)) in rebuilt.iter().zip(&hdr.rgb).enumerate() {
            assert!(
                (got - want).abs() <= 1e-6 * want.max(1.0),
                "sample {index}: {got} vs {want}"
            );
        }
    }

    #[test]
    fn a_pair_with_nothing_to_carry_gives_a_flat_map() {
        // Neutrals and a mild colour, all below white: the renditions are identical and
        // the map says so rather than passing silently.
        let rgb = [
            0.02, 0.02, 0.02, 0.18, 0.18, 0.18, 0.3, 0.25, 0.2, 0.6, 0.6, 0.6,
        ];
        let pair = render_pair(
            aces_from(4, 1, &rgb, None),
            &shared_acting(),
            DestinationGamut::DisplayP3,
            hdr_peak(),
        )
        .unwrap();
        let (sdr, hdr) = (pixels_of(pair.sdr), pixels_of(pair.hdr));
        assert_eq!(bits(&sdr.rgb), bits(&hdr.rgb));
        let range = gain_ratio::between(&sdr, &hdr, 1.0 / 64.0).unwrap().range();
        assert!(range.flat, "{range:?}");
    }

    /// **Contrast, not fit range, decides shadow separation** — the measurement the
    /// look's contrast exists for (`nf-look/contrast`). A neutral ramp from 5.2 to 1.9
    /// stops below mid-grey, through the whole chain, at matched lightness: mid-grey
    /// renders at 0.18 in every cell, since contrast pivots there and reinhard keeps it.
    /// With fit range off the shadow log-log slope is the contrast exactly. At headrooms of
    /// 2, 3 and 6 stops the test bounds it at no more than 3% below the contrast, and its
    /// spread (max − min) across those headrooms under 0.005 (measured: 0.98–0.99× and
    /// under 0.003): below mid reinhard is nearly a gain, costing 1–3% of slope over this
    /// ramp (`fit_range::tests::reinhard_compresses_upward_only`). The bound covers
    /// headroom ≥ 2 only; from 0 up, reinhard switching on costs up to ~0.023 (≈2%) at
    /// contrast 1. A toe in fit range would fail this, which is the point — it would be a
    /// decision.
    #[test]
    fn contrast_not_fit_range_decides_shadow_separation() {
        let xs = [0.005_f32, 0.05, 0.18];
        let render_ramp = |contrast: f32, headroom_stops: f32| {
            let rgb: Vec<f32> = xs.iter().flat_map(|&v| [v, v, v]).collect();
            let mut p = params();
            p.shared.look.section.contrast = contrast;
            p.shared.headroom_stops = headroom_stops;
            let out = render(aces_from(3, 1, &rgb, None), &p).unwrap();
            let y: Vec<f32> = out
                .image
                .into_parts()
                .0
                .rgb
                .as_chunks::<3>()
                .0
                .iter()
                .map(|px| dot(*px, DISPLAY_P3_LUMA))
                .collect();
            assert!((y[2] - 0.18).abs() < 1e-5, "mid-grey moved: {y:?}");
            (y[1] / y[0]).ln() / (xs[1] / xs[0]).ln()
        };
        for contrast in [1.0, look::DEFAULT_CONTRAST, 1.5] {
            // Fit range off: the slope is the contrast, exactly.
            assert!((render_ramp(contrast, 0.0) - contrast).abs() < 1e-4);
            let slopes = [2.0, 3.0, 6.0].map(|h| render_ramp(contrast, h));
            for slope in slopes {
                assert!(
                    (0.97 * contrast..=contrast).contains(&slope),
                    "contrast {contrast}: shadow slope {slope}"
                );
            }
            let max = slopes.iter().copied().fold(f32::MIN, f32::max);
            let min = slopes.iter().copied().fold(f32::MAX, f32::min);
            let spread = max - min;
            assert!(
                spread < 0.005,
                "contrast {contrast}: headroom moved the slope {spread}"
            );
        }
    }
}
