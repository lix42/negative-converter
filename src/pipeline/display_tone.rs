//! Which tone curve a display branch applies — the one resolved choice both
//! `pipeline::sdr` and `pipeline::hdr` read.
//!
//! The branches deliberately keep *different* domains (SDR rolls to `1.0`, HDR to
//! the 1000/203 peak) but share one **normalized knee position**, which is why the
//! `0.5 + 0.25 / (1 + highlight_compress)` resolution lives here rather than being
//! spelled out twice — it had been, and design-spec §6 describes it as a single
//! shared formula.
//!
//! Two illegal states are unrepresentable here rather than merely rejected. The knee
//! width is carried *inside* the shouldered variant, so "no tone curve, and here is
//! how wide its knee is" cannot be handed to a renderer at all — that is why this is
//! an enum and not a bool plus a float. And the width is a checked [`KneeWidth`]
//! rather than a bare `f32`, because an enum variant's fields are as public as the
//! enum: see that type for what an unchecked width does.
//!
//! **Skipping the curve does not skip the range check.** For a tone that bounds its own
//! output, both renderers bound their result (`[0, 1]` for SDR,
//! `[0, LINEAR_HEADROOM]` for HDR), and with the shoulder gone that bound stops being
//! decorative: it is what makes [`DisplayTone::None`] self-policing on a reconstruction
//! that overshoots reference white, instead of a silent clip. See each renderer's
//! `above_range_error`.
//!
//! [`DisplayTone::ExtendedReinhard`] is the exception and says so through
//! [`DisplayTone::bounds_sdr_output`]: it exists to carry content past the ceiling, so its
//! overshoot rides to `io::encode`, which counts every clamped sample. Negativity stays
//! a hard error under every tone. The upper bound is therefore a property of the
//! **resolved tone, not of the branch** — never restate it as one.

use crate::types::{DisplayToneCurve, NcError, PrintParams, Result};

/// The display tone curve a branch applies, already resolved.
///
/// [`None`](Self::None) leaves tone alone entirely; gamut mapping, the transfer
/// encode, and the output range check all still run, so it is *not* "raw pixels
/// out" — see the module docs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DisplayTone {
    /// The C¹ Hermite shoulder, carrying an already-checked knee width.
    HermiteShoulder(KneeWidth),
    /// No display tone curve at all — `print.display_tone = none`.
    None,
    /// Extended Reinhard against a checked white point.
    ///
    /// The one variant whose output is **not** bounded by the branch's ceiling, which
    /// is why [`Self::bounds_sdr_output`] exists: content above the white point still
    /// exceeds it, and that loss is counted at the encode boundary rather than refused.
    /// [`Self::None`] takes the opposite policy deliberately — it *relies* on the range
    /// check — because it is for a reconstruction that is already bounded, while this is
    /// for one that deliberately overshoots.
    ExtendedReinhard(Headroom),
}

/// A finite, non-negative specular headroom in stops, bounded above by
/// [`crate::types::MAX_HEADROOM_STOPS`].
///
/// Same enforcement argument as [`KneeWidth`]: an enum variant's fields are as public
/// as the enum, so a bare `f32` payload would let any module build a Reinhard tone that
/// skipped this check. What an unchecked headroom does, and the bound's reasoning, are
/// documented once with the rule in [`crate::types::check_headroom_stops`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Headroom(f32);

impl Headroom {
    /// Check a headroom, or refuse it.
    ///
    /// The rule itself is [`crate::types::check_headroom_stops`], the single definition
    /// `cli::validate` gates on too — so a headroom the CLI accepts is one this
    /// constructor accepts, and the bound cannot be raised in one place only.
    pub fn new(stops: f32) -> Result<Self> {
        crate::types::check_headroom_stops(stops)?;
        Ok(Self(stops))
    }

    /// The white point the operator maps to the branch's reference white: `2^stops`.
    pub fn white_point(self) -> f32 {
        crate::types::headroom_white_point(self.0)
    }
}

/// A finite, non-negative `print.highlight_compress`.
///
/// **The wrapper is the enforcement.** An enum variant's fields are as public as the
/// enum, so a bare `f32` payload would let any module build a shouldered tone that
/// skipped [`DisplayTone::shoulder`]'s check — and skipping it is not loud:
/// `highlight_compress = -1` divides by zero in
/// [`DisplayTone::knee_position`], giving an infinite knee that no pixel ever
/// reaches, so the frame silently renders with an identity tone curve, exit 0, and
/// metadata reporting `shoulder_start: inf`. This type's field *is* private, so
/// [`KneeWidth::new`] is the only way to obtain one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KneeWidth(f32);

impl KneeWidth {
    /// Check a width, or refuse it.
    pub fn new(highlight_compress: f32) -> Result<Self> {
        if !highlight_compress.is_finite() || highlight_compress < 0.0 {
            return Err(NcError::Usage(format!(
                "print.highlight_compress must be finite and non-negative (got \
                 {highlight_compress})"
            )));
        }
        Ok(Self(highlight_compress))
    }

    /// The checked width, for reporting.
    pub fn amount(self) -> f32 {
        self.0
    }
}

/// Pinned identifier for a rendition that applied no display tone curve. Reported
/// in place of each branch's shoulder identifier, so a report says which of the two
/// happened rather than carrying a shoulder name with null parameters.
pub const NO_TONE_CURVE: &str = "no-tone-curve-v1";

/// Pinned identifier for a rendition tone-mapped by extended Reinhard.
pub const EXTENDED_REINHARD: &str = "extended-reinhard-white-point-v1";

/// Extended Reinhard: `v · (1 + v/W²) / (1 + v)`.
///
/// `white_point` is the input mapping **exactly** to `1.0`. Two properties shape every
/// caller: it is **not bounded** (the value tends to `v/W²`, so `f(200)` at `W = 64` is
/// `1.04`), and it is **global rather than a knee** — `f(0.18) = 0.153` and
/// `f(1.0) = 0.500` at `W = 64`, so it moves the whole curve. That fixed ≈0.24-stop
/// midtone cost belongs to the operator, essentially independent of the white point
/// (0.238 stop at `W = 16`, 0.239 at `W = 64`), which is why comparing it against
/// another operator requires matching brightness first.
///
/// Monotonic for every `W > 0` over `v >= 0`: the derivative is
/// `[1 + (2v + v²)/W²] / (1 + v)²`, positive throughout. Monotonic is not the same as
/// representable — the f64 result tends to `v/W²`, so a tiny white point overflows f32
/// on bright input. [`Headroom`] bounds `W` from below at `2^0 = 1`, which keeps it in
/// range; the renderers' non-finite checks are the backstop.
pub fn extended_reinhard(value: f32, white_point: f32) -> f32 {
    if value <= 0.0 {
        return 0.0;
    }
    // Binary64 so a large `v` cannot lose the `v/W²` term to rounding before the
    // division. Multiply and divide are IEEE-exact, so this is bit-reproducible across
    // targets — unlike a transcendental, which is why no `powf` appears here.
    let v = f64::from(value);
    let w = f64::from(white_point);
    (v * (1.0 + v / (w * w)) / (1.0 + v)) as f32
}

impl DisplayTone {
    /// The baseline shoulder (`highlight_compress = 0`) — what the shipped default
    /// resolves to, and the value every test that is not *about* the tone curve
    /// should use. Test-only: the product reaches it through [`Self::resolve`], and a
    /// shipped constant would be a second way to spell the default.
    #[cfg(test)]
    pub const DEFAULT: Self = Self::HermiteShoulder(KneeWidth(0.0));

    /// Resolve the display tone the print controls ask for.
    ///
    /// The single resolution point for every display branch — the orchestrator calls
    /// it once per frame and hands the result to whichever renderer(s) the preset
    /// needs, so SDR and HDR cannot resolve differently.
    ///
    /// The knee width is refused rather than dropped when no curve is selected. That
    /// duplicates `cli::validate`'s rule on purpose: this is what a *stage* caller
    /// gets, and "silently ignored knob" is the failure both are guarding.
    pub fn resolve(print: &PrintParams) -> Result<Self> {
        match print.display_tone {
            DisplayToneCurve::Shoulder => Self::shoulder(print.highlight_compress),
            DisplayToneCurve::None => {
                let default = PrintParams::default().highlight_compress;
                if print.highlight_compress != default {
                    return Err(NcError::Usage(format!(
                        "print.highlight_compress ({}) places the shoulder's knee, but \
                         print.display_tone = none applies no shoulder to place",
                        print.highlight_compress
                    )));
                }
                Ok(Self::None)
            }
            DisplayToneCurve::Reinhard { headroom_stops } => {
                // Same refusal as `None`, for the same reason: this operator has no
                // knee to place, so a stated width would be silently ignored.
                let default = PrintParams::default().highlight_compress;
                if print.highlight_compress != default {
                    return Err(NcError::Usage(format!(
                        "print.highlight_compress ({}) places the shoulder's knee, but \
                         print.display_tone = reinhard has no knee — its shape is set by \
                         --display-tone-headroom",
                        print.highlight_compress
                    )));
                }
                Ok(Self::ExtendedReinhard(Headroom::new(headroom_stops)?))
            }
        }
    }

    /// Whether the **SDR** branch's ceiling bounds this tone's output, so a sample above
    /// it is a bug rather than an expected loss.
    ///
    /// `true` for the shoulder (bounded by its plateau) and for [`Self::None`], whose whole
    /// policy *is* the range check. `false` only for [`Self::ExtendedReinhard`], which
    /// exists to carry content past reference white — there the overshoot rides to
    /// `io::encode`, which counts every clamped sample.
    ///
    /// **Deliberately not one predicate for both branches.** Reinhard is unbounded on SDR
    /// (measured pre-clamp peak 1.26–1.30) and *bounded* on HDR, because the HDR form uses
    /// an asymptotic base — see [`highlight_lifted_reinhard`]. A single boolean asserted
    /// one of those two wrongly, and the HDR side is the one that would have lost a real
    /// guarantee: its ceiling is the declared 1000-nit peak the CICP and `clli` contract
    /// commits to, so it must stay strictly enforced.
    pub fn bounds_sdr_output(self) -> bool {
        !matches!(self, Self::ExtendedReinhard(_))
    }

    /// Whether this resolved tone leaves the signal **untouched**, so an overshoot has
    /// nothing to roll it off.
    ///
    /// True for [`Self::None`], and for an extended-Reinhard whose white point leaves no
    /// span above `crossover` — that is the identity, so it reaches the range check in
    /// precisely `None`'s situation. Keyed on the resolved tone rather than on
    /// `shoulder_start.is_none()` for the same reason the range check is: the absence of a
    /// knee is a proxy that would also match a future knee-less curve which *does* shape
    /// the signal.
    ///
    /// **`crossover` is a parameter, and must be the same value the caller passes
    /// [`highlight_lifted_reinhard`].** That function states its crossover rather than
    /// assuming `1.0`, because where diffuse white lands depends on the reconstruction's
    /// anchor offset. Hardcoding `1.0` here duplicated the identity rule with a different
    /// constant, and the two would have disagreed in the quiet direction: a config that
    /// renders as the identity would report `false`, this diagnosis would not fire, and the
    /// tone-aware remedy would degrade to the bare internal-looking "out-of-range sample"
    /// for a user-reachable config — the exact failure that remedy was added to remove.
    ///
    /// Its whole job is diagnosis. Both cases already failed loudly at the same pixel; only
    /// `None` explained itself, so zero headroom got a bare "out-of-range sample" with no
    /// hint that zero headroom means no compression.
    pub fn applies_no_curve(self, crossover: f32) -> bool {
        match self {
            DisplayTone::None => true,
            DisplayTone::ExtendedReinhard(headroom) => headroom.white_point() <= crossover,
            DisplayTone::HermiteShoulder(_) => false,
        }
    }

    /// Whether the **HDR** branch's ceiling bounds this tone's output.
    ///
    /// Always `true`: the shoulder plateaus at the peak, `None` relies on the range check,
    /// and the Reinhard form's asymptotic base holds the composite strictly under the
    /// ceiling. So the HDR range check is never relaxed, and a sample above
    /// `LINEAR_HEADROOM` is a renderer bug on every tone.
    pub fn bounds_hdr_output(self) -> bool {
        true
    }

    /// Resolve a shouldered tone from a knee width, rejecting a width the knee
    /// resolution cannot use.
    ///
    /// Checked once, here, rather than inside each renderer: [`KneeWidth`] makes an
    /// unchecked width unrepresentable, so the renderers and the gain-map config need
    /// no defensive re-check of their own. Callers resolve *before* rendering the
    /// display source, so a bad width also fails before the render allocates.
    pub fn shoulder(highlight_compress: f32) -> Result<Self> {
        Ok(Self::HermiteShoulder(KneeWidth::new(highlight_compress)?))
    }

    /// The resolved knee width, or `None` when no curve is applied.
    pub fn highlight_compress(self) -> Option<f32> {
        match self {
            Self::HermiteShoulder(width) => Some(width.amount()),
            Self::None | Self::ExtendedReinhard(_) => Option::None,
        }
    }

    /// Where the knee sits as a fraction of the branch's own domain, or `None` when
    /// no curve is applied.
    ///
    /// Bounded to `[0.5, 0.75]` so even an extreme finite width cannot flatten the
    /// whole tonal range: for a huge finite `f32`, adding one rounds back to that
    /// same value, so the reciprocal term stably tends toward zero.
    pub fn knee_position(self) -> Option<f32> {
        self.highlight_compress()
            .map(|amount| 0.5 + 0.25 / (1.0 + amount))
    }
}

/// Extended Reinhard plus a smooth highlight lift, for a branch with headroom above
/// reference white.
///
/// `g(v) = f(v) · (1 + (ceiling − 1)·s(v))`, where `f` is [`extended_reinhard`] at the
/// **same** `white_point` the SDR branch uses and `s` ramps from `0` at `crossover` to
/// `1` at `white_point`.
///
/// **Why this shape and not a ceiling-parameterized Reinhard.** A gain map is only
/// meaningful when the two renditions agree below diffuse white and differ above it — the
/// ratio must be exactly `1` in the midtones. Both obvious generalizations fail that:
/// `v(1 + vC/W²)/(1 + v/C)` and `C·f(v/C)` each lift mid-grey ≈14% and diffuse white ≈66%,
/// because their denominators compress less *everywhere* rather than only in highlights.
/// This form is `1 · f(v)` below `crossover` **by construction**, so the agreement is
/// exact rather than approximate.
///
/// That also means **the operator is the gain map**: `g/f` is exactly
/// `1 + (ceiling − 1)·s(v)`, so the HDR rendition is defined as the SDR rendition plus
/// recovered highlight headroom — which is what the container encodes.
///
/// Monotonic wherever `f` is, being a product of two non-decreasing factors, and
/// **unbounded above `white_point`** for the same reason `f` is, so a branch using it
/// reports `bounds_sdr_output() == false` and its overshoot is counted at the encode
/// boundary. Note the HDR branch reports `bounds_hdr_output() == true` for the same tone —
/// the lifted form is bounded there — which is why the two predicates are separate.
///
/// `ceiling` is a parameter, never a literal: the 1000/203 headroom is binding policy
/// owned by `hdr::LINEAR_HEADROOM` and `docs/hdr-output-spike.md`. `crossover` is stated
/// rather than assumed to be `1.0` because where diffuse white actually lands depends on
/// the reconstruction's anchor offset, which is measured-but-uncalibrated
/// (`algo/exponential-anchor-placement`).
///
/// Unlike [`extended_reinhard`], this uses `log2` and so is **not** bit-reproducible
/// across libm implementations to the last ulp. That is acceptable only because it is
/// HDR-only: that branch already applies `powf` for the PQ and HLG transfers, so its
/// goldens are already curated for cross-target agreement. Do not reach for this on the
/// SDR path, whose transcendental-free arithmetic is a property worth keeping.
pub fn highlight_lifted_reinhard(
    value: f32,
    white_point: f32,
    crossover: f32,
    ceiling: f32,
) -> f32 {
    // **The base is asymptotic, not the SDR curve, and that is the measured conclusion.**
    // The lift is multiplicative, so the composite can only be bounded by bounding the
    // base — and that is the whole design space. Measured across seven frames: leaving the
    // base unbounded put the peak at 5.3–17.0 against a 4.926 ceiling; clamping it hard
    // held the peak but collapsed separation above reference white to *zero* on four of the
    // seven, which is the zero-slope plateau this operator exists to remove. The
    // asymptotic base (`extended_reinhard` with no white point, i.e. `v/(1 + v)`) reaches
    // neither failure: peak 4.912–4.919, strictly *below* the ceiling and never attaining
    // it, with separation preserved everywhere.
    //
    // Its cost is dropping `f`'s `v/W²` tail, so the base disagrees with the SDR branch's
    // below the crossover — by at most **0.0244%**, at the crossover itself. One 8-bit
    // gain-map code step over `[1, ceiling]` is a factor of 1.00627, **25.7x larger**, so
    // the *encoded* gain is still exactly 1 and the two renditions agree as far as the
    // container can express. Stated as a ratio rather than as "negligible" so the next
    // person can re-check it.
    // `W <= crossover` leaves no span for the lift to ramp across, so there is no headroom
    // to carry and the operator has nothing to do. This is what makes `headroom_stops = 0`
    // the identity on *this* branch too — `extended_reinhard` is exactly `v` at `W = 1`, so
    // without this the same knob at the same setting was the identity on SDR and a full stop
    // of darkening on the seven HDR presets, against a doc promise of byte-identical to
    // `None`.
    //
    // Note what this does **not** make continuous: the base is `v/(1 + v)` regardless of `W`,
    // so the HDR form does not *approach* the identity as `W → 1` the way the SDR form does.
    // A very small non-zero headroom is therefore a near-step curve on HDR (at `W = 1.07` the
    // lift spans 0.1 stops, mapping 1.07 to 2.55), and that is inherent to an asymptotic base
    // rather than something this early return introduces. Sized headroom is what the operator
    // is for; the degenerate low end is documented, not smoothed over.
    if white_point <= crossover {
        return value;
    }
    let base = extended_reinhard(value, f32::INFINITY);
    // Below the crossover the lift is identically zero, so this returns the base
    // unchanged. Also the guard that keeps `log2` off non-positive input.
    if value <= crossover || ceiling <= 1.0 {
        return base;
    }
    let (lo, hi) = (crossover.log2(), white_point.log2());
    // Only a NaN bound can still reach this, now that `white_point <= crossover` returns
    // above — but it must, and silently: `partial_cmp` rather than `hi <= lo` because a NaN
    // bound must take
    // this branch too, and a direct comparison would let it through to produce a NaN
    // pixel — the renderers' non-finite guards would then blame this stage for a bad
    // argument.
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        return base;
    }
    let t = ((value.log2() - lo) / (hi - lo)).clamp(0.0, 1.0);
    // Smoothstep: zero slope at both ends, so the lift joins `f` C¹-continuously at the
    // crossover instead of creasing there.
    let s = t * t * (3.0 - 2.0 * t);
    base * (1.0 + (ceiling - 1.0) * s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gain-map requirement, restated as the measurement left it. The HDR form uses an
    /// asymptotic base so the composite stays under the ceiling, which costs `f`'s `v/W²`
    /// tail — so agreement below the crossover is *near*-exact rather than bit-exact. The
    /// bound that matters is the container's: one 8-bit gain-map code step over
    /// `[1, ceiling]`. Anything inside a fraction of that encodes as gain = 1.
    #[test]
    fn the_hdr_base_agrees_with_sdr_within_a_fraction_of_a_gain_code_step() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        // `ceiling^(1/255)`: the multiplicative width of one code step, ≈1.00627.
        let step = c.powf(1.0 / 255.0) - 1.0;
        let budget = step / 10.0;
        let mut worst = 0.0f32;
        for v in [1e-4f32, 0.02, 0.18, 0.5, 0.9, 1.0] {
            let sdr = extended_reinhard(v, w);
            let hdr = highlight_lifted_reinhard(v, w, xo, c);
            let deficit = (1.0 - hdr / sdr).abs();
            worst = worst.max(deficit);
            assert!(
                deficit < budget,
                "v = {v}: disagreement {deficit:.6} exceeds a tenth of a code step \
                 ({budget:.6})"
            );
        }
        // Pinned so a regression that widens the gap shows up as a number, not a pass:
        // the worst case is at the crossover and measured 0.0244%.
        assert!(
            worst > 1e-5 && worst < 3e-4,
            "worst disagreement {worst:.6} is not the documented ≈2.44e-4"
        );
        // Non-positive input stays black rather than reaching `log2`.
        assert_eq!(highlight_lifted_reinhard(-1.0, w, xo, c), 0.0);
    }

    /// The lift is *fully applied* at the white point — the ramp has saturated there — even
    /// though the composite is below the ceiling, because the asymptotic base is itself
    /// below 1 at any finite input. Both halves matter: a lift that saturated later would
    /// waste headroom, and a composite that reached the ceiling would plateau.
    #[test]
    fn the_lift_saturates_at_the_white_point_while_the_composite_stays_below_the_ceiling() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        let base = extended_reinhard(w, f32::INFINITY);
        let got = highlight_lifted_reinhard(w, w, xo, c);
        assert!(
            (got - base * c).abs() < 1e-4,
            "at the white point expected base*ceiling = {}, got {got}",
            base * c
        );
        assert!(got < c, "the composite must stay under the ceiling");
        // A ceiling of 1 is no headroom at all, so the operator degenerates to its base.
        assert_eq!(
            highlight_lifted_reinhard(8.0, w, xo, 1.0).to_bits(),
            extended_reinhard(8.0, f32::INFINITY).to_bits()
        );
    }

    /// The gain itself — `hdr/sdr`, which is what the container stores — is 1 to within a
    /// code step below the crossover, rises monotonically above it, and never exceeds the
    /// ceiling.
    #[test]
    fn the_encoded_gain_rises_monotonically_and_only_above_the_crossover() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        let budget = (c.powf(1.0 / 255.0) - 1.0) / 10.0;
        let gain = |v: f32| highlight_lifted_reinhard(v, w, xo, c) / extended_reinhard(v, w);
        let mut previous = 0.0f32;
        let mut v = 0.01f32;
        while v <= w {
            let g = gain(v);
            assert!(g.is_finite(), "non-finite gain at {v}");
            assert!(g <= c + 1e-5, "gain {g} exceeded the ceiling at {v}");
            // Monotonicity is asserted only **above** the crossover, and that is the real
            // contract rather than a relaxation: below it the gain is `1/(1 + v/W²)`, which
            // *decreases* from 1 toward 0.99976 because the asymptotic base drops `f`'s
            // tail. The code-step budget is what covers that dip; asserting a rising gain
            // across the whole range would assert something false.
            if v <= xo {
                assert!(
                    (g - 1.0).abs() < budget,
                    "gain must encode as 1 below the crossover (v = {v}, got {g})"
                );
            } else {
                assert!(
                    g >= previous - 1e-6,
                    "gain fell at {v}: {g} after {previous}"
                );
                previous = g;
            }
            v *= 1.05;
        }
        assert!(gain(w) > c - 0.1, "the gain never approached the ceiling");
    }

    /// The ramp is in **log2**, not linear, and that is a design choice worth pinning:
    /// stops are how highlight range is actually spent, so the lift must be half-applied
    /// at the *geometric* midpoint of the crossover→white-point span, not the arithmetic
    /// one. A linear ramp passes every other test here while barely lifting anything until
    /// the last stop — at 3 of 6 stops it reaches a gain of 1.14 where log reaches 2.96 —
    /// so without this a "simplification" that drops `log2` would ship silently.
    #[test]
    fn the_lift_ramps_in_stops_not_in_linear_value() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        let gain = |v: f32| highlight_lifted_reinhard(v, w, xo, c) / extended_reinhard(v, w);
        // Geometric midpoint of [1, 64] is 8 — three of the six stops.
        let mid_gain = gain(8.0);
        let half = 1.0 + (c - 1.0) * 0.5;
        assert!(
            (mid_gain - half).abs() < 0.05,
            "at the geometric midpoint the gain should be ~{half:.3} (half applied), got \
             {mid_gain:.3}; a linear ramp would give ~1.14"
        );
        // And the arithmetic midpoint must be well past half, not at it.
        assert!(
            gain((w + xo) / 2.0) > half + 0.5,
            "gain at the arithmetic midpoint should be far past half under a log ramp"
        );
    }

    /// Monotone in the rendered value too — a product of two non-decreasing factors — so
    /// the lift cannot invert tonal order the way a naive ceiling swap would.
    #[test]
    fn the_highlight_lift_is_monotonic_across_the_crossover() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        let mut previous = -1.0f32;
        let mut v = 0.01f32;
        while v < 400.0 {
            let g = highlight_lifted_reinhard(v, w, xo, c);
            assert!(g.is_finite(), "non-finite at {v}");
            assert!(g >= previous, "decreased at {v}: {g} after {previous}");
            previous = g;
            v *= 1.02;
        }
    }

    /// No crease where the lift switches on: smoothstep has zero slope at both ends, so
    /// the joint is C¹. A plain linear ramp would kink here, and a kink at diffuse white
    /// is exactly where the eye is most sensitive.
    #[test]
    fn the_lift_joins_the_sdr_curve_without_a_crease() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        let d = |v: f32| {
            let h = v * 1e-3;
            (highlight_lifted_reinhard(v + h, w, xo, c)
                - highlight_lifted_reinhard(v - h, w, xo, c))
                / (2.0 * h)
        };
        // Slope just below the crossover is the SDR curve's; just above it must match to
        // within a few percent rather than jumping.
        let (below, above) = (d(xo * 0.99), d(xo * 1.01));
        assert!(
            (above - below).abs() / below < 0.05,
            "slope jumped at the crossover: {below} -> {above}"
        );
    }

    /// NaN bounds take the degenerate path rather than producing a NaN pixel — the
    /// renderers' non-finite guards would otherwise blame this stage for a bad argument.
    #[test]
    fn non_finite_bounds_fall_back_instead_of_poisoning_the_pixel() {
        for (w, xo) in [(f32::NAN, 1.0f32), (64.0, f32::NAN), (f32::NAN, f32::NAN)] {
            let got = highlight_lifted_reinhard(8.0, w, xo, 4.926_108);
            assert!(
                got.is_finite() || !extended_reinhard(8.0, w).is_finite(),
                "w={w} xo={xo} produced {got}"
            );
        }
    }

    /// A degenerate span (crossover at or above the white point) must not step the lift on
    /// or divide by zero. It returns the **input unchanged**.
    ///
    /// That value changed on 2026-09-02, from the unlifted base to the identity. The
    /// no-step/no-divide property this test exists for is satisfied either way, so the
    /// choice was free here — and it is not free at `white_point <= crossover`, which is
    /// reachable as `headroom_stops = 0` with the production crossover of `1.0`. There the
    /// identity is what makes zero headroom mean what it says on both branches, so the two
    /// cases resolve the same way rather than the reachable one carrying a special case.
    #[test]
    fn a_degenerate_span_returns_the_input_unchanged() {
        for xo in [64.0f32, 128.0] {
            assert_eq!(
                highlight_lifted_reinhard(80.0, 64.0, xo, 4.926_108).to_bits(),
                80.0f32.to_bits(),
                "crossover {xo} should degenerate to the identity"
            );
        }
    }

    /// Zero headroom is the identity on the HDR branch, not just on the SDR one.
    ///
    /// The regression this pins: the base is `v/(1 + v)` regardless of `W`, so before the
    /// early return, `--display-tone-headroom 0` left mid-grey at 0.153 and reference white
    /// at 0.5 — a full stop down — on all seven single-rendition HDR presets, while the SDR
    /// presets rendered the exact identity and the docs promised byte-identity with `none`.
    #[test]
    fn zero_headroom_is_the_identity_on_the_hdr_branch_too() {
        let w = Headroom::new(0.0).unwrap().white_point();
        for v in [0.0f32, 0.05, 0.18, 0.5, 1.0, 2.0, 8.0, 64.0] {
            assert_eq!(
                highlight_lifted_reinhard(v, w, 1.0, 4.926_108).to_bits(),
                v.to_bits(),
                "v={v} under zero headroom"
            );
            // And it agrees with the SDR branch at the same setting, which is the property
            // the shared doc promise is about.
            assert_eq!(extended_reinhard(v, w).to_bits(), v.to_bits(), "sdr v={v}");
        }
    }

    /// `f(v) = v(1 + v/W²)/(1 + v)` in binary64, for cross-checking the shipped f32
    /// entry point against the algebra its docs state.
    fn reference(v: f64, w: f64) -> f64 {
        v * (1.0 + v / (w * w)) / (1.0 + v)
    }

    #[test]
    fn the_white_point_maps_exactly_to_reference_white() {
        // The operator's defining property, and the reason the parameter is spelled as
        // a white point at all: input `W` lands on `1.0`, so "how many stops of specular
        // headroom" is a promise about where diffuse-white-plus-N-stops ends up.
        // Exactly, not approximately — `W·(1 + W/W²) = W + 1` cancels the denominator.
        for stops in [
            0.0f32,
            1.0,
            4.0,
            6.0,
            10.0,
            crate::types::MAX_HEADROOM_STOPS,
        ] {
            let w = Headroom::new(stops).unwrap().white_point();
            assert_eq!(
                extended_reinhard(w, w),
                1.0,
                "{stops} stops (W = {w}) did not map its white point to 1.0"
            );
        }
    }

    #[test]
    fn zero_headroom_is_the_exact_identity() {
        // `W = 1` gives `v(1 + v)/(1 + v) = v`, which is what makes
        // `--display-tone reinhard --display-tone-headroom 0` render byte-identically to
        // `--display-tone none`. The binary64 multiply-then-divide need not round back to
        // `v` in f64, but the error is ~1 f64 ulp — far below f32 — so the returned f32
        // is bit-identical. Asserted on bits, since "byte-identical output" is the claim.
        let w = Headroom::new(0.0).unwrap().white_point();
        assert_eq!(w, 1.0);
        for v in [
            0.0f32, 1e-6, 0.018, 0.18, 0.5, 1.0, 1.000_001, 4.0, 64.0, 1e6,
        ] {
            assert_eq!(
                extended_reinhard(v, w).to_bits(),
                v.to_bits(),
                "W = 1 was not the identity at {v}"
            );
        }
    }

    #[test]
    fn it_is_global_rather_than_a_knee() {
        // The difference in kind from the Hermite shoulder: this moves the *whole*
        // curve, so midtones pay too. Comparing it against a shouldered render therefore
        // requires matching brightness first — a probe that skips that is measuring the
        // brightness difference, not the operator.
        let w = Headroom::new(6.0).unwrap().white_point();
        assert_eq!(w, 64.0);
        let near = |a: f32, b: f32| assert!((a - b).abs() < 5e-4, "{a} != {b}");
        near(extended_reinhard(0.18, w), 0.153);
        near(extended_reinhard(1.0, w), 0.500);
        // The midtone cost is a property of the operator, not of the white point: ≈0.238
        // stop at `W = 16` and at `W = 64` alike (they differ by 0.001 stop), which is
        // why raising the headroom does not buy the midtones back. The claim is that the
        // two barely move, so assert their *difference*, not just each value.
        let cost = |w: f32| -(extended_reinhard(0.18, w) / 0.18).log2();
        assert!((cost(16.0) - 0.238).abs() < 5e-3, "{}", cost(16.0));
        assert!((cost(64.0) - 0.238).abs() < 5e-3, "{}", cost(64.0));
        assert!(
            (cost(16.0) - cost(64.0)).abs() < 2e-3,
            "the midtone cost tracked the white point: {} vs {}",
            cost(16.0),
            cost(64.0)
        );
    }

    #[test]
    fn it_is_not_bounded_by_the_branch_ceiling() {
        // The whole reason `bounds_sdr_output()` exists. Content above the white point still
        // exceeds `1.0`; the value tends to `v/W²`, so the overshoot is real but slow.
        let w = 64.0;
        assert!(extended_reinhard(200.0, w) > 1.0);
        let near = |a: f32, b: f32| assert!((a - b).abs() < 5e-3, "{a} != {b}");
        near(extended_reinhard(200.0, w), 1.04);
        // ...and the shipped resolution reports exactly that, so the renderers can key
        // their range policy off it rather than off the variant name.
        let reinhard = DisplayTone::ExtendedReinhard(Headroom::new(6.0).unwrap());
        assert!(!reinhard.bounds_sdr_output());
        assert!(DisplayTone::None.bounds_sdr_output());
        assert!(DisplayTone::DEFAULT.bounds_sdr_output());
        // The HDR branch is a different answer for the same tone, which is why these are
        // two predicates: its asymptotic base holds the composite under the ceiling, so
        // its range check is never relaxed and an over-peak sample stays a renderer bug.
        for tone in [reinhard, DisplayTone::None, DisplayTone::DEFAULT] {
            assert!(tone.bounds_hdr_output(), "{tone:?}");
        }
    }

    /// The HDR form really is bounded by the ceiling it is given — the property
    /// `bounds_hdr_output` asserts, and the reason the hard clamp was rejected.
    #[test]
    fn the_hdr_form_stays_strictly_under_its_ceiling() {
        let c = 4.926_108f32;
        for w in [16.0f32, 64.0] {
            let mut peak = 0.0f32;
            let mut v = 0.01f32;
            while v < 5000.0 {
                let g = highlight_lifted_reinhard(v, w, 1.0, c);
                assert!(g < c, "v = {v}, W = {w}: {g} reached the ceiling {c}");
                peak = peak.max(g);
                v *= 1.05;
            }
            // Asymptotic, so it approaches the ceiling without a plateau: close, but the
            // strict inequality above is what "peak below the ceiling" means.
            assert!(peak > c * 0.98, "W = {w}: peak {peak} never approached {c}");
        }
    }

    #[test]
    fn it_is_monotonic_and_finite_for_every_admissible_white_point() {
        // Monotonic for every `W > 0` over `v >= 0` — the derivative
        // `[1 + (2v + v²)/W²]/(1 + v)²` is positive throughout. `Headroom` bounds `W`
        // from below at `2^0 = 1`, which is also what keeps `v/W²` from overflowing f32
        // on bright input; both halves are asserted here because a future bound change
        // has to move them together.
        for stops in [0.0f32, 1.0, 6.0, 12.0, crate::types::MAX_HEADROOM_STOPS] {
            let w = Headroom::new(stops).unwrap().white_point();
            let mut previous = f32::NEG_INFINITY;
            for step in 0..=400 {
                let v = (step as f32 / 20.0).exp2() * 1e-3;
                let out = extended_reinhard(v, w);
                assert!(out.is_finite(), "W = {w}, v = {v} produced {out}");
                assert!(out > previous, "W = {w}: not increasing at v = {v}");
                previous = out;
            }
            // ...and against the algebra its docs state, not just against itself.
            for v in [0.018f32, 0.18, 1.0, 5.0, 100.0] {
                let expected = reference(f64::from(v), f64::from(w)) as f32;
                assert_eq!(extended_reinhard(v, w).to_bits(), expected.to_bits());
            }
        }
    }

    #[test]
    fn non_positive_input_is_black() {
        // Guarded before the arithmetic: the algebra is only defined for `v >= 0`, and a
        // negative input would come back negative and then trip the renderers'
        // negativity check with a less useful diagnosis.
        for v in [0.0f32, -0.0, -1e-9, -1.0, -f32::MAX] {
            assert_eq!(extended_reinhard(v, 64.0), 0.0, "{v}");
        }
    }

    #[test]
    fn knee_position_is_bounded_and_monotonic_and_absent_without_a_curve() {
        assert_eq!(DisplayTone::DEFAULT.knee_position(), Some(0.75));
        let a = DisplayTone::shoulder(1.0).unwrap().knee_position().unwrap();
        let b = DisplayTone::shoulder(4.0).unwrap().knee_position().unwrap();
        let huge = DisplayTone::shoulder(f32::MAX)
            .unwrap()
            .knee_position()
            .unwrap();
        assert!(0.5 <= huge && huge < b && b < a && a < 0.75);
        assert_eq!(DisplayTone::None.knee_position(), None);
        assert_eq!(DisplayTone::None.highlight_compress(), None);
    }

    #[test]
    fn an_unusable_knee_width_is_rejected_at_construction() {
        for bad in [-0.1, -1.0, f32::NAN, f32::INFINITY] {
            let err = DisplayTone::shoulder(bad).unwrap_err();
            assert!(matches!(err, NcError::Usage(_)), "{bad}: {err:?}");
        }
        // Why construction has to be the gate: `-1` divides by zero, and the result
        // is *silent* rather than a loud non-finite failure — an infinite knee is one
        // no pixel reaches, so the frame would render with an identity tone curve and
        // exit 0. `KneeWidth`'s private field is what keeps that unreachable.
        assert!((0.5f32 + 0.25 / (1.0 + -1.0f32)).is_infinite());
        // The boundary value is usable, not rejected.
        assert!(DisplayTone::shoulder(0.0).is_ok());
        assert_eq!(
            DisplayTone::shoulder(0.0).unwrap(),
            DisplayTone::DEFAULT,
            "the default must be exactly the neutral checked width"
        );
    }
}
