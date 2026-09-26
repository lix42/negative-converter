//! The current chain's display tone: extended Reinhard against a checked white point —
//! the one operator both `pipeline::sdr` and `pipeline::hdr` apply.
//!
//! The `shoulder` and `none` tones retired (`nf-retire/display-tones`): both existed for
//! reconstructions already bounded at white, and none ships. What is left is fit range's
//! operator — bit-identical on the SDR branch (`fit_range`'s
//! `the_sdr_operator_is_the_legacy_reinhard`), while the HDR branch keeps its own
//! asymptotic-base form, `highlight_lifted_reinhard`, until the flip retires this chain.
//!
//! **The range policy differs per branch, and zero headroom is where it bites.** On SDR the
//! curve overshoots display white by design, so the loss is counted at `io::encode`; on
//! HDR the composite stays strictly under the peak, so a sample above it is refused. At
//! `headroom_stops = 0` the operator is the identity on both branches
//! ([`Headroom::is_identity`]), and each renderer then refuses content above its ceiling
//! instead of counting it — the self-policing the retired `none` tone provided.

use crate::types::Result;

/// The resolved display tone: a finite, non-negative specular headroom in stops, bounded
/// above by [`crate::types::MAX_HEADROOM_STOPS`], with the white point and the mid-grey
/// gain it implies resolved **once** — the renderers apply it per pixel, and neither value
/// varies across a frame.
///
/// **The private fields are the enforcement.** An unchecked headroom is not loud on its own:
/// a negative one is a white point below `1`, which renders a solid white field at exit 0
/// with the clip merely counted. [`Headroom::new`] is the only way to obtain one, and the
/// rule it applies is [`crate::types::check_headroom_stops`] — the one `cli::validate`
/// gates on too, so the CLI and a stage caller cannot bound the knob differently.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Headroom {
    white_point: f32,
    gain: f64,
}

/// The default headroom — what every test that is not *about* the tone should use.
/// Test-only: the product reaches it through the recipe's default.
#[cfg(test)]
impl Default for Headroom {
    fn default() -> Self {
        Self::new(crate::types::DEFAULT_HEADROOM_STOPS).expect("the default is in range")
    }
}

impl Headroom {
    /// Check a headroom, or refuse it.
    pub fn new(stops: f32) -> Result<Self> {
        crate::types::check_headroom_stops(stops)?;
        let white_point = crate::types::headroom_white_point(stops);
        Ok(Self {
            white_point,
            gain: mid_grey_preserving_gain(white_point),
        })
    }

    /// The curve's white-point **parameter**: `2^stops`.
    ///
    /// Not the input that maps to reference white — `extended_reinhard` preserves
    /// mid-grey instead of pinning this value to `1.0`, so the unity point sits at
    /// `W / gain`. It is the scale that sizes the curve, which is what the knob names.
    #[cfg(test)]
    pub fn white_point(self) -> f32 {
        self.white_point
    }

    /// Whether the operator is the **identity** here, so an overshoot has nothing to roll
    /// it off and the renderer must refuse it rather than pass it on.
    ///
    /// **`crossover` is a parameter, and must be the value the caller passes
    /// `highlight_lifted_reinhard`** — that function returns its input unchanged when the
    /// white point leaves no span above the crossover, and a hardcoded `1.0` here would
    /// disagree with it in the quiet direction: a render that is the identity would not be
    /// diagnosed as one. The SDR branch passes `1.0`, where `W = 1` is exactly the identity.
    pub fn is_identity(self, crossover: f32) -> bool {
        self.white_point <= crossover
    }

    /// The operator the report names: [`EXTENDED_REINHARD`], or
    /// [`fit_range::IDENTITY`](crate::pipeline::fit_range::IDENTITY) where
    /// [`is_identity`](Self::is_identity) holds — the new chain's rule, so the two chains
    /// name identical pixels identically.
    pub fn operator(self, crossover: f32) -> &'static str {
        if self.is_identity(crossover) {
            crate::pipeline::fit_range::IDENTITY
        } else {
            EXTENDED_REINHARD
        }
    }

    /// The SDR curve at this headroom: `extended_reinhard` with the gain resolved once.
    pub fn sdr(self, value: f32) -> f32 {
        extended_reinhard_raw(value, self.white_point, self.gain)
    }

    /// The HDR form at this headroom: `highlight_lifted_reinhard` with the gain resolved
    /// once.
    pub fn hdr(self, value: f32, crossover: f32, ceiling: f32) -> f32 {
        lifted_reinhard_raw(value, self.white_point, self.gain, crossover, ceiling)
    }
}

/// Pinned identifier for a rendition tone-mapped by extended Reinhard.
///
/// **v2 since the operator absorbed its own midtone cost** — v1 delivered mid-grey 0.24
/// stop dark, v2 delivers it at 0.18. Same shape, different pixels, so the identifier
/// moves: a consumer comparing two renders needs to know which operator produced them,
/// and `extended-reinhard` alone no longer says.
pub const EXTENDED_REINHARD: &str = "extended-reinhard-mid-preserving-v2";

/// Extended Reinhard: `u · (1 + u/W²) / (1 + u)` over `u = gain · v`.
///
/// **Mid-grey is preserved: `f(0.18) = 0.18` at every white point**, which is what
/// [`mid_grey_preserving_gain`] buys and why the input gain exists at all. `white_point`
/// therefore sizes the curve without being its unity point — the value mapping to `1.0`
/// is `W / gain` (52.48 at `W = 64`, 13.13 at `W = 16`), and `f(W)` overshoots slightly
/// (1.0062 at `W = 64`, 1.0236 at `W = 16`). Two further properties shape every caller:
/// it is **not bounded** (the value tends to `gain · v/W²`, so `f(200)` at `W = 64` is
/// `1.06`), and it is **global rather than a knee** — `f(1.0) = 0.550` at `W = 64`, so it
/// moves the whole curve between mid-grey and white rather than only the top.
///
/// Monotonic for every `W > 0` over `v >= 0`: the derivative is
/// `gain · [1 + (2u + u²)/W²] / (1 + u)²`, positive throughout. Monotonic is not the same as
/// representable — the f64 result tends to `v/W²`, so a tiny white point overflows f32
/// on bright input. [`Headroom`] bounds `W` from below at `2^0 = 1`, which keeps it in
/// range; the renderers' non-finite checks are the backstop.
#[cfg(test)]
pub fn extended_reinhard(value: f32, white_point: f32) -> f32 {
    extended_reinhard_raw(value, white_point, mid_grey_preserving_gain(white_point))
}

/// The curve itself, with the input gain supplied by the caller.
///
/// Split out for one reason: the HDR branch's base uses this curve's *shape* with no white
/// point (`highlight_lifted_reinhard`) but must apply the **same** gain as the SDR branch
/// it is paired with — a gain-map's two renditions are ratioed against each other, so a
/// difference between them shows up as encoded gain. Computing the gain once, from the real
/// white point, and handing it to both keeps their only disagreement the documented
/// `v/W²` tail.
///
/// `pub(crate)` for its second, test-only caller: `pipeline::shadow_metrics::hdr_gain_probe`
/// hand-builds the shipped HDR operator from these two parts and asserts it equals
/// `highlight_lifted_reinhard`, which is a drift detector only as long as the probe does
/// *not* call that function. Building the mirror out of `extended_reinhard` instead gives it
/// `gain(∞)` where the shipped operator uses `gain(W)`, and the two disagree by up to 33x
/// the assertion's tolerance.
pub(crate) fn extended_reinhard_raw(value: f32, white_point: f32, gain: f64) -> f32 {
    if value <= 0.0 {
        return 0.0;
    }
    // Binary64 so a large `v` cannot lose the `v/W²` term to rounding before the
    // division. Multiply and divide are IEEE-exact, so this is bit-reproducible across
    // targets — unlike a transcendental, which is why no `powf` appears here.
    let v = f64::from(value) * gain;
    let w = f64::from(white_point);
    (v * (1.0 + v / (w * w)) / (1.0 + v)) as f32
}

/// Scene mid-grey: an 18 % reflector on a correctly exposed frame.
///
/// The reconstruction places it here by construction (the curve's anchor pins mid-grey
/// at 0.18), so it is the tone operator's obligation to leave it alone.
const MID_GREY: f64 = 0.18;

/// The input gain that makes `extended_reinhard` deliver [`MID_GREY`] *at* mid-grey.
///
/// **Why the operator carries this rather than the user.** The raw curve costs a fixed
/// ≈0.24 stop at mid-grey — 0.238 at `W = 16`, 0.239 at `W = 64` — so every render through
/// it came out that much dark, on *every* reconstruction: measured 0.235 (sigmoid at its
/// shipped defaults), 0.238 (characteristic), 0.245 (shoulder-less sigmoid). A fixed cost
/// that no reconstruction escapes and no user asked for is the operator's to absorb; left
/// outside, it becomes a magic `--print-exposure 0.28` that every user and every recipe has
/// to know. `pipeline::stages::midtone_placement` pins the measurement.
///
/// Solving `f(x) = MID_GREY` for the raw curve gives `x² + (1−m)W²x − mW² = 0`, hence the
/// closed form below; the gain is `x / m`. Two properties matter:
///
/// - **`W = 1` returns exactly 1**, so `--display-tone-headroom 0` stays the exact identity
///   it is documented to be (the raw curve is already the identity there, and the algebra
///   agrees: `x = m`). Computed in binary64, the residual is ~1e-16 and vanishes in the
///   `f32` round-trip, so the identity holds bit-for-bit.
/// - **It moves the unity point.** The value now mapping to `1.0` is `W / gain`, not `W`.
///   That trade is forced: the curve's white-to-mid ratio cannot go below 6.17 for any
///   `W`, while preserving both endpoints would need 1/0.18 = 5.56, so no member of this
///   family fixes mid-grey *and* pins `W` to 1.0. Overshoot is already this tone's
///   documented behaviour on SDR, with the loss counted at `io::encode` — so the cost
///   lands where the design already accepts it.
///
/// `pub(crate)` for the same reason as [`extended_reinhard_raw`] — see the note there.
pub(crate) fn mid_grey_preserving_gain(white_point: f32) -> f64 {
    // Written in the rationalized form `2m / ((1−m) + √((1−m)² + 4m/W²))` rather than the
    // textbook quadratic root, and the reason is cancellation, measured: the textbook form
    // is `(−b + √(b² + …))/2` with `b = (1−m)W²`, which at `W = 16` subtracts 209.92 from
    // 210.36 and throws away three digits. The loss grows with `W` and bites *inside* the
    // admissible range — past `W ≈ 2¹²` the `4mW²` term stops surviving beside `b²`, so at
    // `MAX_HEADROOM_STOPS` (`W = 2²⁴`) the textbook root returns 1.2153 against the correct
    // 1.2195. `mod tests`' `reference_gain` spells that form out and is checked against this
    // one only over `stops <= 12`, for exactly that reason.
    //
    // This form has no subtraction at all, and it stays finite in the limit besides: at
    // `W = ∞` the textbook root is `∞ − ∞`, i.e. **NaN**, while this one tends to
    // `1/(1−m)`, the pure-Reinhard answer. No caller reaches that limit today — `Headroom`
    // bounds `W` at `2²⁴`, and the HDR base's `f32::INFINITY` goes to
    // `extended_reinhard_raw`'s *white point* while its gain comes from the finite paired
    // one (see `highlight_lifted_reinhard`) — so it is a property of the form rather than
    // a case in the render path.
    let inv_w2 = 1.0 / (f64::from(white_point) * f64::from(white_point));
    let k = 1.0 - MID_GREY;
    2.0 / (k + (k * k + 4.0 * MID_GREY * inv_w2).sqrt())
}

/// An **asymptotic** Reinhard base plus a smooth highlight lift, for a branch with
/// headroom above reference white.
///
/// `g(v) = b(v) · (1 + (ceiling − 1)·s(v))`, where `s` ramps from `0` at `crossover` to
/// `1` at `white_point`, and `b` is `extended_reinhard`'s shape with **no white point**
/// carrying the input gain of the SDR branch it is paired with —
/// `extended_reinhard_raw(v, ∞, mid_grey_preserving_gain(white_point))`. The base is
/// asymptotic rather than the SDR curve so the multiplicative lift cannot leave the
/// ceiling; the body's opening comment holds the measurement that settled that, and the
/// shared gain is `extended_reinhard_raw`'s reason for existing.
///
/// **Why this shape and not a ceiling-parameterized Reinhard.** A gain map is only
/// meaningful when the two renditions agree below diffuse white and differ above it — the
/// ratio must be `1` in the midtones. Both obvious generalizations fail that:
/// `v(1 + vC/W²)/(1 + v/C)` and `C·f(v/C)` each lift mid-grey ≈14% and diffuse white ≈66%
/// (`f` being the SDR branch's `extended_reinhard` at `white_point`), because their
/// denominators compress less *everywhere* rather than only in highlights. Here the lift
/// is identically zero below `crossover`, so what renders there is the bare base — and
/// that agreement with `f` is **near-exact, not exact**: `b` drops `f`'s `v/W²` tail. What
/// makes the difference immaterial is that it stays a fraction of one 8-bit gain-map code
/// step, so the *encoded* gain is still exactly 1.
/// `the_hdr_base_agrees_with_sdr_within_a_fraction_of_a_gain_code_step` pins the bound,
/// the figure, and its algebra.
///
/// **The operator is the gain map**, to that same tolerance: `g/b` is exactly
/// `1 + (ceiling − 1)·s(v)`, so the HDR rendition is the SDR rendition plus recovered
/// highlight headroom — which is what the container encodes.
///
/// Monotonic wherever `b` is, being a product of two non-decreasing factors, and
/// **bounded**: `b < 1` at every finite input, so the composite stays strictly under
/// `ceiling` and never attains it, which is why the HDR range check is never relaxed. The
/// *SDR* branch runs `f` instead, which is unbounded — one operator per branch, so the
/// same headroom is counted on SDR and bounded on HDR.
///
/// `ceiling` is a parameter, never a literal: the 1000/203 headroom is binding policy
/// owned by `hdr::LINEAR_HEADROOM` and `docs/spike/hdr-output-spike.md`. `crossover` is stated
/// rather than assumed to be `1.0` because where diffuse white actually lands depends on
/// the reconstruction's anchor offset, which is measured-but-uncalibrated
/// (`algo/exponential-anchor-placement`).
///
/// Unlike `extended_reinhard`, this uses `log2` and so is **not** bit-reproducible
/// across libm implementations to the last ulp. That is acceptable only because it is
/// HDR-only: that branch already applies `powf` for the PQ and HLG transfers, so its
/// goldens are already curated for cross-target agreement. Do not reach for this on the
/// SDR path, whose transcendental-free arithmetic is a property worth keeping.
#[cfg(test)]
pub fn highlight_lifted_reinhard(
    value: f32,
    white_point: f32,
    crossover: f32,
    ceiling: f32,
) -> f32 {
    lifted_reinhard_raw(
        value,
        white_point,
        mid_grey_preserving_gain(white_point),
        crossover,
        ceiling,
    )
}

/// `highlight_lifted_reinhard` with the input gain supplied by the caller — the paired
/// SDR branch's, resolved once per frame by [`Headroom`].
fn lifted_reinhard_raw(
    value: f32,
    white_point: f32,
    gain: f64,
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
    // Its cost is dropping `f`'s `u/W²` tail, so the base disagrees with the SDR branch's
    // below the crossover — by at most **0.0298%**, at the crossover itself. One 8-bit
    // gain-map code step over `[1, ceiling]` is a factor of 1.00627, **21.1x larger**, so
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
    let base = extended_reinhard_raw(value, f32::INFINITY, gain);
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
        // the worst case is at the crossover, where the deficit is exactly
        // `1 − 1/(1 + gain/W²)`, and measures 0.0298%. (It was 0.0244% before the shared
        // input gain scaled the dropped tail by the same 1.219 — a bound of `< 3e-4` was
        // then 1% from failing, which is why this one states the algebra.)
        let expected = 1.0 - 1.0 / (1.0 + mid_grey_preserving_gain(w) / f64::from(w * w));
        assert!(
            (f64::from(worst) - expected).abs() < 1e-6,
            "worst disagreement {worst:.6} is not the predicted {expected:.6}"
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
        // The base carries the *paired SDR branch's* gain, not its own — that sharing is
        // what keeps the encoded gain at 1 below the crossover, so spell it the same way
        // the operator does rather than through `extended_reinhard(_, INFINITY)`.
        let base_at = |v: f32| extended_reinhard_raw(v, f32::INFINITY, mid_grey_preserving_gain(w));
        let base = base_at(w);
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
            base_at(8.0).to_bits()
        );
    }

    /// The gain itself — `hdr/sdr`, which is what the container stores — is 1 to within a
    /// code step below the crossover, rises above it, and never exceeds the ceiling.
    ///
    /// **"Rises" is bounded by the container, not by strict monotonicity**, and that is a
    /// property of the operator rather than a relaxation. The smoothstep saturates *at*
    /// the white point (zero slope there, by construction), while the SDR branch's
    /// `u/W²` tail keeps climbing — so the ratio turns over shortly before `W` and eases
    /// back. It always has: at gain 1 the turning point sat between this sweep's last two
    /// samples, so the old grid stepped straight over it. What the gain map needs is not
    /// a monotonic ratio but one whose dip is unresolvable in 8 bits, so that is what is
    /// asserted — measured at 9.6% of a code step over the whole roll-off.
    #[test]
    fn the_encoded_gain_rises_and_never_falls_by_a_resolvable_step() {
        let (w, xo, c) = (64.0f32, 1.0f32, 4.926_108f32);
        let step = c.powf(1.0 / 255.0) - 1.0;
        let budget = step / 10.0;
        let gain = |v: f32| highlight_lifted_reinhard(v, w, xo, c) / extended_reinhard(v, w);
        let mut previous = 0.0f32;
        let mut peak = 0.0f32;
        let mut falling = false;
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
                // Unimodal, asserted as such: rising until the turning point, easing
                // back after it, never wobbling. A dip that later recovered would mean
                // the lift and the tail are fighting somewhere in the middle of the
                // ramp, which is a different defect from the roll-off at the top.
                if falling {
                    assert!(g <= previous + 1e-6, "gain rose again at {v} after falling");
                } else if g < previous - 1e-6 {
                    falling = true;
                }
                peak = peak.max(g);
                previous = g;
            }
            v *= 1.05;
        }
        // The whole roll-off, peak to white point, must be finer than the container can
        // resolve — that is the real requirement the old monotonicity assertion stood in
        // for. A fine sweep puts the peak at v = 60.0 and the drop at 9.6% of a code
        // step; bounded at a quarter of one, so a regression that grew it 2.6x fails.
        let drop = (peak - gain(w)) / peak;
        assert!(
            drop > 0.0 && drop < step / 4.0,
            "the gain rolled off by {drop:.2e}, against a quarter code step of {:.2e}",
            step / 4.0
        );
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

    /// `f(u) = u(1 + u/W²)/(1 + u)` over `u = gain · v`, in binary64 — the curve's
    /// algebra as its docs state it, for cross-checking the shipped f32 entry point.
    /// Takes the shipped gain on purpose: this checks the *curve*, and
    /// [`reference_gain`] separately checks the gain.
    fn reference(v: f64, w: f64) -> f64 {
        let u = v * mid_grey_preserving_gain(w as f32);
        u * (1.0 + u / (w * w)) / (1.0 + u)
    }

    /// `x/m` where `x` solves `x² + (1−m)W²x − mW² = 0`, spelled the textbook way —
    /// an independent derivation of [`mid_grey_preserving_gain`], usable only over
    /// moderate white points. Above `W ≈ 2¹²` the `4mW²` term stops surviving beside
    /// `b²` and this returns a visibly wrong gain (1.2153 against 1.2195 at
    /// `W = 2²⁴`), which is the measured reason the shipped form is rationalized.
    fn reference_gain(w: f64) -> f64 {
        let b = (1.0 - MID_GREY) * w * w;
        ((-b + (b * b + 4.0 * MID_GREY * w * w).sqrt()) / 2.0) / MID_GREY
    }

    #[test]
    fn mid_grey_is_preserved_and_the_unity_point_moves_by_the_gain() {
        // The operator's defining property since v2, and it replaced the older one: the
        // parameter is still spelled as a white point, but what is now exact is
        // **mid-grey**, not `f(W) = 1`. No member of this family can do both — the curve's
        // white-to-mid ratio bottoms out at 6.17 while pinning both ends needs
        // `1/0.18 = 5.56` — so the unity point moves to `W / gain` and `f(W)` overshoots
        // slightly. Both halves are asserted, because a future change must move them
        // together.
        for stops in [
            0.0f32,
            1.0,
            4.0,
            6.0,
            10.0,
            crate::types::MAX_HEADROOM_STOPS,
        ] {
            let w = Headroom::new(stops).unwrap().white_point();
            let gain = mid_grey_preserving_gain(w) as f32;
            let moderate = stops <= 12.0;
            assert_eq!(
                extended_reinhard(0.18, w),
                0.18,
                "{stops} stops (W = {w}) did not preserve mid-grey"
            );
            // Independently derived, so the closed form is checked and not just echoed
            // — over the range where the textbook root is still accurate. See
            // `reference_gain`: past `W ≈ 2¹²` it is the one that is wrong.
            assert!(
                !moderate
                    || (mid_grey_preserving_gain(w) - reference_gain(f64::from(w))).abs() < 1e-9,
                "W = {w}: {} vs textbook {}",
                mid_grey_preserving_gain(w),
                reference_gain(f64::from(w))
            );
            // `W = 1` is the exact identity, so its unity point is still `W` itself.
            let unity = w / gain;
            assert!(
                (extended_reinhard(unity, w) - 1.0).abs() < 1e-6,
                "{stops} stops: {unity} should map to 1.0, got {}",
                extended_reinhard(unity, w)
            );
            assert!(
                extended_reinhard(w, w) >= 1.0,
                "{stops} stops: `f(W)` fell below reference white"
            );
        }
        // The overshoot at `W` itself, pinned as numbers so "slightly" is checkable.
        let near = |a: f32, b: f32| assert!((a - b).abs() < 5e-4, "{a} != {b}");
        near(extended_reinhard(64.0, 64.0), 1.0062);
        near(extended_reinhard(16.0, 16.0), 1.0236);
        near(64.0 / mid_grey_preserving_gain(64.0) as f32, 52.483);
    }

    #[test]
    fn zero_headroom_is_the_exact_identity() {
        // `W = 1` gives `v(1 + v)/(1 + v) = v`, which is what makes
        // `--display-tone-headroom 0` the exact identity. The binary64 multiply-then-divide need not round back to
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
        // The difference in kind from a knee (the retired Hermite shoulder): this moves the
        // *whole* curve, so midtones pay too. Comparing it against a knee-shaped render
        // therefore requires matching brightness first — a probe that skips that is measuring the
        // brightness difference, not the operator.
        let w = Headroom::new(6.0).unwrap().white_point();
        assert_eq!(w, 64.0);
        let near = |a: f32, b: f32| assert!((a - b).abs() < 5e-4, "{a} != {b}");
        near(extended_reinhard(1.0, w), 0.5496);
        near(extended_reinhard(0.5, w), 0.3788);
        // What v2 changed and what it did not. Mid-grey is now free — the cost that used
        // to be ≈0.238 stop at every white point is zero at every white point — but the
        // tone above it is still compressed, and that is what "global rather than a
        // knee" means: diffuse white pays 0.86 stop with no knee anywhere near it.
        let cost = |v: f32, w: f32| -(extended_reinhard(v, w) / v).log2();
        for w in [16.0f32, 64.0] {
            assert!(cost(0.18, w).abs() < 1e-5, "W = {w}: {}", cost(0.18, w));
        }
        assert!((cost(1.0, w) - 0.864).abs() < 5e-3, "{}", cost(1.0, w));
        // Below mid-grey it *lifts* rather than compressing — the gain is a plain input
        // multiplier and the `1/(1 + u)` denominator has barely engaged — so the
        // shadows are not simply "less compressed", they move the other way.
        assert!(
            cost(0.09, w) < 0.0,
            "shadows should lift: {}",
            cost(0.09, w)
        );
    }

    #[test]
    fn it_is_not_bounded_by_the_branch_ceiling() {
        // Why the SDR branch counts rather than refuses. Content above the white point still
        // exceeds `1.0`; the value tends to `v/W²`, so the overshoot is real but slow.
        let w = 64.0;
        assert!(extended_reinhard(200.0, w) > 1.0);
        let near = |a: f32, b: f32| assert!((a - b).abs() < 5e-3, "{a} != {b}");
        near(extended_reinhard(200.0, w), 1.0552);
    }

    /// The HDR form really is bounded by the ceiling it is given — why the HDR range check
    /// is never relaxed, and the reason the hard clamp was rejected.
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
    fn zero_headroom_is_the_identity_and_nothing_else_is() {
        assert!(Headroom::new(0.0).unwrap().is_identity(1.0));
        assert!(!Headroom::new(0.01).unwrap().is_identity(1.0));
        assert!(!Headroom::default().is_identity(1.0));
        // A crossover above `1.0` widens the identity span with it — the parameter is
        // what keeps this predicate and `highlight_lifted_reinhard` in step.
        assert!(Headroom::new(1.0).unwrap().is_identity(2.0));
    }

    #[test]
    fn an_unusable_headroom_is_rejected_at_construction() {
        for bad in [-0.1, -1.0, f32::NAN, f32::INFINITY, 25.0] {
            assert!(Headroom::new(bad).is_err(), "{bad}");
        }
        assert_eq!(Headroom::new(6.0).unwrap(), Headroom::default());
    }
}
