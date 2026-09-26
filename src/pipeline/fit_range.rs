//! **Stage 3 of the new rendering chain — fit range.**
//!
//! Fit the scene's dynamic range into the display's. The industry term is *tone
//! mapping*; "tone" there means brightness levels, not colour. One operator, with the
//! display's **peak** as its argument — which is what makes SDR and HDR the same
//! function rather than two renderers:
//!
//! ```text
//! Y′ = r(Y) · (1 + (P − 1) · s(Y))            per pixel, on ACEScg luminance
//! r  = mid-grey-preserving extended reinhard at W = 2^headroom_stops
//! s  = smoothstep in stops, 0 at diffuse white → 1 at W
//! ```
//!
//! All three channels are scaled by `Y′/Y`, so hue and the neutral axis are kept and
//! chroma is left to the look and to fit gamut. At `Y ≤ 0` — a saturated colour a
//! wide-gamut linear space can hold — the scale is its limit at black, the mid-grey
//! gain, so it stays continuous where luminance crosses zero.
//!
//! - **`P = 1` is exactly reinhard.** The lift multiplies by `1 + 0·s = 1`, so the SDR
//!   branch's per-pixel arithmetic is transcendental-free. Its one libm call is the
//!   white point, `2^headroom_stops` (`exp2`), exact at whole stops — so SDR output is
//!   bit-reproducible across targets at a whole-stop headroom, and may differ by the
//!   last ULP of `W` at a fractional one.
//! - **Below diffuse white every peak agrees exactly**, because the lift is zero there
//!   and `r` is shared. A gain map needs the two renditions to agree below diffuse
//!   white, and here that is a property of the formula rather than a tolerance.
//! - **Mid-grey is preserved** (`r(0.18) = 0.18` at every `W`), and `headroom_stops = 0`
//!   is the identity on every branch.
//! - **Not bounded by the peak above `W`.** `r` keeps rising past `W` (as `v/W²`), so
//!   content brighter than the headroom exceeds the display's peak on every branch —
//!   SDR above `1.0`, HDR above `P` — and the encoder clamps and counts it. The legacy
//!   HDR renderer instead dropped `r`'s tail to stay strictly under its peak, and paid
//!   for it with a below-white disagreement between the branches; this stage takes the
//!   exact agreement. No destination adds a hard ceiling: at the default headroom no
//!   real frame reaches past the HDR peak, and what a lower headroom pushes past is
//!   clamped and counted (`nf-display-stages/branch-contract` has the measurement; for
//!   a gain map the destination clamps and counts, since `gain_ratio` does not).
//!
//! **Reinhard compresses upward only**: below mid-grey it is nearly a gain (log-log
//! slope 1.00 at 0.002, 0.94 near 0.05), so the approach to black is left to the
//! decode's linearization and the look's contrast.
//! A toe belongs here, where the display's range is known; whether the operator earns
//! one is `nf-display-stages/parametric-operator`'s question.
//!
//! Written fresh, per CLAUDE.md's migration rule — `pipeline::sdr` and `pipeline::hdr`
//! each fuse tone, luminance rescale and gamut map into one loop body and retire with
//! the legacy chain; `pipeline::display_tone`'s reinhard is the same curve at `P = 1`,
//! which `tests::the_sdr_operator_is_the_legacy_reinhard` pins while both exist.

use std::fmt;

use serde::Serialize;

use crate::algo::fixed::DIFFUSE_WHITE;
use crate::pipeline::colorimetry::dot;
use crate::pipeline::colorimetry::pinned::ACESCG_LUMA;
use crate::pipeline::look::GradedImage;
use crate::pipeline::pixels;
use crate::pipeline::working_image::WorkingBuffer;
use crate::types::{NcError, Result, headroom_fault, headroom_white_point};

/// Pinned identifier of the operator, for the report. A change to its pixels moves
/// the version.
pub const OPERATOR: &str = "reinhard-peak-lifted-v1";

/// What the report names in place of an operator when the headroom leaves nothing to
/// compress (`headroom_stops = 0`), so a report never names an operator that moved no
/// pixel. Shared with the current chain's `display_tone`.
pub const IDENTITY: &str = "identity";

/// Scene mid-grey, which the operator leaves where the decode put it.
const MID_GREY: f64 = 0.18;

/// The display's peak, relative to diffuse white — the one argument the two display
/// branches differ in.
///
/// Checked, so an unusable peak cannot reach the operator: below `1.0` the lift would
/// *darken* above diffuse white, and a non-finite one poisons every bright pixel.
/// Bounded above by PQ's 10,000 nits over the 203-nit reference white, the brightest
/// signal any display destination can carry.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DisplayPeak(f32);

impl DisplayPeak {
    /// An SDR display: diffuse white is the peak.
    pub const SDR: Self = DisplayPeak(1.0);

    /// The largest representable peak: PQ's 10,000 nits over 203-nit reference white.
    const MAX: f32 = 10_000.0 / 203.0;

    /// Check a peak, or refuse it.
    pub fn new(peak: f32) -> Result<Self> {
        if !peak.is_finite() || !(1.0..=Self::MAX).contains(&peak) {
            return Err(NcError::Other(format!(
                "fit range's display peak must be finite and within [1, {}] times \
                 diffuse white, got {peak}",
                Self::MAX
            )));
        }
        Ok(DisplayPeak(peak))
    }

    /// The peak as a multiple of diffuse white.
    pub fn value(self) -> f32 {
        self.0
    }
}

/// Fit range's parameters: the recipe's `fit_range` section plus the destination's
/// peak. **No `Default`**, for the reason [`FitGamutParams`] has none: the peak is the
/// destination's to state.
///
/// [`FitGamutParams`]: crate::pipeline::fit_gamut::FitGamutParams
#[derive(Clone, Debug, PartialEq)]
pub struct FitRangeParams {
    /// How much scene range above diffuse white the operator compresses, in stops:
    /// reinhard's white point is `W = 2^headroom_stops`. `0` is the identity.
    pub headroom_stops: f32,
    /// The display's peak.
    pub peak: DisplayPeak,
}

impl FitRangeParams {
    /// Reinhard's white point, `2^headroom_stops`.
    fn white_point(&self) -> f32 {
        headroom_white_point(self.headroom_stops)
    }

    /// What fit range resolved, for the report — the operator by name and its
    /// arguments as values, so a report never describes the fit in prose.
    pub fn resolved(&self) -> FitRange {
        let white_point = self.white_point();
        FitRange {
            // By the same test `apply` uses to skip the pixels, so the report never
            // names an operator that moved none.
            operator: if white_point == 1.0 {
                IDENTITY
            } else {
                OPERATOR
            },
            headroom_stops: self.headroom_stops,
            white_point,
            display_peak: self.peak,
        }
    }
}

/// What fit range applied to a frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct FitRange {
    /// [`OPERATOR`], or `"identity"` when the headroom leaves nothing to compress.
    pub operator: &'static str,
    pub headroom_stops: f32,
    /// Reinhard's white point, `2^headroom_stops`.
    pub white_point: f32,
    /// The display's peak, as a multiple of diffuse white.
    pub display_peak: DisplayPeak,
}

/// Pixels whose **luminance** now fits the display's range, before the gamut is
/// fitted — and the peak they were fitted to.
///
/// A separate boundary from [`DisplayReferredImage`] because the two stages are
/// coupled but distinct: the gamut ceiling follows the range this stage produced,
/// which is why they stay adjacent rather than merged. The peak rides here so fit
/// gamut reads the one this stage used rather than being handed it a second time.
///
/// [`DisplayReferredImage`]: crate::pipeline::fit_gamut::DisplayReferredImage
pub struct RangeFittedImage(WorkingBuffer, DisplayPeak);

impl RangeFittedImage {
    /// The display peak these pixels were fitted to.
    pub fn peak(&self) -> DisplayPeak {
        self.1
    }

    /// Hand the buffer to the next stage. Consuming, so the pixels move rather
    /// than copy.
    pub(in crate::pipeline) fn into_buffer(self) -> WorkingBuffer {
        self.0
    }
}

impl fmt::Debug for RangeFittedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_named(f, "RangeFittedImage")
    }
}

/// Fit the scene's range to the display's, in place.
///
/// **Refuses a non-finite sample**, naming the lowest such pixel: a NaN or infinity
/// here is an upstream fault, and scaling by it would hand the encoder a value it can
/// only clamp. Checked at every setting, the identity included, so whether a frame
/// renders never depends on the headroom. The headroom is checked by `types`'
/// [`headroom_fault`], the rule the recipe's validation reports; reaching here with a
/// bad one is a wiring bug.
pub fn apply(image: GradedImage, params: &FitRangeParams) -> Result<RangeFittedImage> {
    if let Some(fault) = headroom_fault(params.headroom_stops) {
        return Err(NcError::Other(format!(
            "fit range was handed an unusable headroom ({fault:?}); the recipe's \
             validation should have refused it"
        )));
    }
    let mut buffer = image.into_buffer();
    let white_point = params.white_point();
    let identity = white_point == 1.0;
    let operator = Operator::new(white_point, params.headroom_stops, params.peak);
    pixels::try_map_in_place(buffer.rgb_mut(), |index, px| {
        if !px.iter().all(|v| v.is_finite()) {
            return Err(NcError::Other(format!(
                "fit range received a non-finite sample at pixel {index} ({px:?})"
            )));
        }
        if identity {
            return Ok(());
        }
        let luminance = dot(*px, ACESCG_LUMA);
        let scale = operator.scale(luminance);
        for channel in px.iter_mut() {
            *channel *= scale;
        }
        if !px.iter().all(|v| v.is_finite()) {
            return Err(NcError::Other(format!(
                "fit range produced a non-finite sample at pixel {index} from luminance \
                 {luminance}"
            )));
        }
        Ok(())
    })?;
    Ok(RangeFittedImage(buffer, params.peak))
}

/// The operator with its per-frame constants resolved once.
#[derive(Clone, Copy, Debug)]
struct Operator {
    white_point: f32,
    /// The input gain that keeps mid-grey at mid-grey.
    gain: f64,
    /// `log2` of diffuse white, where the lift starts.
    bottom_stops: f32,
    /// `log2(W)`, i.e. the headroom in stops — taken from the knob rather than
    /// recomputed, so the lift's end costs no libm call.
    top_stops: f32,
    peak: f32,
}

impl Operator {
    fn new(white_point: f32, headroom_stops: f32, peak: DisplayPeak) -> Self {
        Self {
            white_point,
            gain: mid_grey_preserving_gain(white_point),
            bottom_stops: DIFFUSE_WHITE.log2(),
            top_stops: headroom_stops,
            peak: peak.value(),
        }
    }

    /// The factor a pixel of this luminance is scaled by: `f(Y)/Y`.
    ///
    /// **At `Y ≤ 0` it is the curve's limit at black, the mid-grey gain** — not `1`. `f`
    /// is a pure gain near zero (`f(Y)/Y → gain`, ≈1.22 at six stops), so leaving those
    /// pixels unscaled would step every channel by that much where a saturated colour's
    /// luminance crosses zero. The limit keeps the scale continuous, and still clamps
    /// nothing.
    fn scale(self, luminance: f32) -> f32 {
        if luminance <= 0.0 {
            return self.gain as f32;
        }
        self.apply(luminance) / luminance
    }

    /// `r(v) · (1 + (P − 1)·s(v))` for a positive luminance.
    fn apply(self, value: f32) -> f32 {
        let base = reinhard(value, self.white_point, self.gain);
        // Skipped rather than multiplied by one: bit-identical, and it keeps the `log2`
        // off the SDR branch and off everything below diffuse white.
        if self.peak == 1.0 || value <= DIFFUSE_WHITE {
            return base;
        }
        base * (1.0 + (self.peak - 1.0) * self.lift(value))
    }

    /// The lift's ramp: `0` at diffuse white, `1` at the white point, smooth at both
    /// ends so the lifted curve joins `r` without a crease. In stops, because a ramp in
    /// linear value would spend nearly all of its span in the top stop.
    fn lift(self, value: f32) -> f32 {
        let span = self.top_stops - self.bottom_stops;
        // No span above diffuse white: nothing to lift across (zero headroom).
        if span <= 0.0 {
            return 0.0;
        }
        let t = ((value.log2() - self.bottom_stops) / span).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
}

/// Extended reinhard with an input gain: `u·(1 + u/W²)/(1 + u)` over `u = gain·v`.
///
/// Binary64 so a large `v` keeps its `u/W²` term; multiply and divide are IEEE-exact,
/// so this is bit-reproducible across targets. Monotonic for every `W > 0`.
fn reinhard(value: f32, white_point: f32, gain: f64) -> f32 {
    let u = f64::from(value) * gain;
    let w = f64::from(white_point);
    (u * (1.0 + u / (w * w)) / (1.0 + u)) as f32
}

/// The input gain that makes [`reinhard`] return [`MID_GREY`] at mid-grey.
///
/// Solving `r(x) = m` gives `x² + (1−m)W²x − mW² = 0`; the gain is `x/m`, written in the
/// rationalized form `2 / ((1−m) + √((1−m)² + 4m/W²))`, which has no subtraction to
/// cancel at large `W`. Exactly `1` at `W = 1`, where the curve is the identity. `sqrt`
/// is correctly rounded by IEEE 754, so this stays target-independent.
fn mid_grey_preserving_gain(white_point: f32) -> f64 {
    let inv_w2 = 1.0 / (f64::from(white_point) * f64::from(white_point));
    let k = 1.0 - MID_GREY;
    2.0 / (k + (k * k + 4.0 * MID_GREY * inv_w2).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::display_tone;
    use crate::types::{DEFAULT_HEADROOM_STOPS, MAX_HEADROOM_STOPS};

    /// An HDR display's peak for these tests: 1000 nits over 203-nit reference white.
    const HDR_PEAK: f32 = 1000.0 / 203.0;

    fn op(stops: f32, peak: f32) -> Operator {
        Operator::new(
            headroom_white_point(stops),
            stops,
            DisplayPeak::new(peak).unwrap(),
        )
    }

    /// A log-spaced sweep from deep shadow to far past any white point.
    fn sweep() -> impl Iterator<Item = f32> {
        (0..=400).map(|i| 2f32.powf(-12.0 + i as f32 * 0.05))
    }

    #[test]
    fn mid_grey_is_preserved_at_every_headroom_and_peak() {
        for stops in [0.0, 1.0, 4.0, 6.0, 8.0, 12.0, MAX_HEADROOM_STOPS] {
            for peak in [1.0, HDR_PEAK] {
                let out = op(stops, peak).apply(0.18);
                assert!(
                    (out - 0.18).abs() < 1e-6,
                    "stops {stops}, peak {peak}: {out}"
                );
            }
        }
    }

    #[test]
    fn zero_headroom_is_the_identity_on_every_branch() {
        for peak in [1.0, HDR_PEAK] {
            let o = op(0.0, peak);
            for v in sweep() {
                assert_eq!(o.apply(v).to_bits(), v.to_bits(), "peak {peak}, v {v}");
            }
        }
    }

    #[test]
    fn the_sdr_operator_is_the_legacy_reinhard() {
        // The same curve written fresh; bit-identical while both exist, so the two
        // chains' SDR tones cannot drift apart before `nf-retire/display-tones`.
        for stops in [0.0, 2.0, DEFAULT_HEADROOM_STOPS, 10.0] {
            let w = headroom_white_point(stops);
            for v in sweep() {
                assert_eq!(
                    op(stops, 1.0).apply(v).to_bits(),
                    display_tone::extended_reinhard(v, w).to_bits(),
                    "stops {stops}, v {v}"
                );
            }
        }
    }

    #[test]
    fn every_peak_agrees_exactly_below_diffuse_white() {
        // The gain-map contract, as a property of the formula: bit-identical, not
        // merely within a code step.
        let sdr = op(DEFAULT_HEADROOM_STOPS, 1.0);
        for peak in [1.5, HDR_PEAK, DisplayPeak::MAX] {
            let hdr = op(DEFAULT_HEADROOM_STOPS, peak);
            for v in sweep().filter(|&v| v <= DIFFUSE_WHITE) {
                assert_eq!(hdr.apply(v).to_bits(), sdr.apply(v).to_bits(), "{v}");
            }
            // And the lift is real above it — the control that makes the equality
            // above mean something.
            assert!(hdr.apply(4.0) > sdr.apply(4.0) * 1.1, "peak {peak}");
        }
    }

    #[test]
    fn it_is_monotonic_and_finite() {
        for stops in [0.5, 2.0, DEFAULT_HEADROOM_STOPS, 12.0, MAX_HEADROOM_STOPS] {
            for peak in [1.0, 1.01, HDR_PEAK, DisplayPeak::MAX] {
                let o = op(stops, peak);
                let mut last = 0.0;
                for v in sweep().chain([1e6, 1e20]) {
                    let out = o.apply(v);
                    assert!(out.is_finite(), "stops {stops}, peak {peak}, v {v}");
                    assert!(out >= last, "stops {stops}, peak {peak}: falls at {v}");
                    last = out;
                }
            }
        }
    }

    #[test]
    fn the_white_point_lands_at_the_peak_and_content_above_it_overshoots() {
        // At `W` the lift is complete, so the output is the peak times `r(W)` — just
        // over it, as the SDR curve is just over `1.0` there. Past `W` the tail keeps
        // rising on every branch: the documented, encoder-counted overshoot.
        let w = headroom_white_point(DEFAULT_HEADROOM_STOPS);
        for peak in [1.0, HDR_PEAK] {
            let o = op(DEFAULT_HEADROOM_STOPS, peak);
            let at_w = o.apply(w);
            assert!(at_w > peak && at_w < peak * 1.01, "peak {peak}: {at_w}");
            assert!(o.apply(8.0 * w) > at_w, "peak {peak}");
        }
    }

    #[test]
    fn the_lift_joins_the_base_without_a_crease() {
        // The lifted curve's slope just above diffuse white matches the shared one
        // just below it — smoothstep has zero slope at `t = 0`.
        let o = op(DEFAULT_HEADROOM_STOPS, HDR_PEAK);
        let h = 1e-3;
        let below = (o.apply(DIFFUSE_WHITE) - o.apply(DIFFUSE_WHITE - h)) / h;
        let above = (o.apply(DIFFUSE_WHITE + h) - o.apply(DIFFUSE_WHITE)) / h;
        assert!((above - below).abs() / below < 0.02, "{below} vs {above}");
    }

    #[test]
    fn reinhard_compresses_upward_only() {
        // Below mid-grey the curve is nearly a gain: its log-log slope is 1 deep in the
        // shadows and still 0.94 near 0.05 (design-update Part 2, "The shadow end") —
        // the shadow end the parametric operator exists to question. Pinned so a
        // change there is a decision.
        let o = op(DEFAULT_HEADROOM_STOPS, 1.0);
        let slope = |v: f32| (o.apply(v * 1.001) / o.apply(v)).ln() / 1.001f32.ln();
        assert!((slope(0.002) - 1.0).abs() < 0.01, "{}", slope(0.002));
        assert!((slope(0.05) - 0.94).abs() < 0.01, "{}", slope(0.05));
        assert!((slope(1.0) - 0.45).abs() < 0.02, "{}", slope(1.0));
    }

    #[test]
    fn the_peak_is_checked() {
        assert!(DisplayPeak::new(1.0).is_ok());
        assert!(DisplayPeak::new(HDR_PEAK).is_ok());
        for bad in [
            0.99,
            0.0,
            -1.0,
            f32::NAN,
            f32::INFINITY,
            DisplayPeak::MAX * 1.01,
        ] {
            assert!(DisplayPeak::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_scale_is_continuous_where_luminance_crosses_zero() {
        // Just above zero the curve is a pure gain, so its scale meets the one a pixel
        // at or below zero gets — no step for a saturated colour straddling black.
        for peak in [1.0, HDR_PEAK] {
            let o = op(DEFAULT_HEADROOM_STOPS, peak);
            let below = o.scale(-1e-3);
            assert_eq!(below, o.scale(0.0));
            for above in [1e-9, 1e-6] {
                assert!(
                    (o.scale(above) - below).abs() / below < 1e-4,
                    "peak {peak}: {} vs {below}",
                    o.scale(above)
                );
            }
            assert!(
                below > 1.2,
                "the limit is the mid-grey gain, not 1: {below}"
            );
        }
        // And at zero headroom the gain is exactly 1.
        assert_eq!(op(0.0, 1.0).scale(-1.0), 1.0);
    }

    #[test]
    fn the_headroom_rule_is_the_shared_one() {
        use crate::types::{HeadroomFault, MAX_HEADROOM_STOPS};
        assert_eq!(headroom_fault(0.0), None);
        assert_eq!(headroom_fault(MAX_HEADROOM_STOPS), None);
        assert_eq!(headroom_fault(-0.5), Some(HeadroomFault::Negative(-0.5)));
        assert!(matches!(
            headroom_fault(f32::NAN),
            Some(HeadroomFault::Negative(_))
        ));
        assert_eq!(headroom_fault(25.0), Some(HeadroomFault::TooLarge(25.0)));
    }

    #[test]
    fn the_report_names_the_operator_only_when_it_runs() {
        let params = |stops| FitRangeParams {
            headroom_stops: stops,
            peak: DisplayPeak::SDR,
        };
        let r = params(DEFAULT_HEADROOM_STOPS).resolved();
        assert_eq!(r.operator, OPERATOR);
        assert_eq!(r.white_point, 64.0);
        assert_eq!(r.display_peak, DisplayPeak::SDR);
        assert_eq!(params(0.0).resolved().operator, "identity");
    }
}
