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
//! **Skipping the curve does not skip the range check.** Both renderers bound their
//! output (`[0, 1]` for SDR, `[0, LINEAR_HEADROOM]` for HDR), and with the shoulder
//! gone that bound stops being decorative: it is what makes [`DisplayTone::None`]
//! self-policing on a reconstruction that overshoots reference white, instead of a
//! silent clip. See each renderer's `above_range_error`.

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
        }
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
            Self::None => Option::None,
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

#[cfg(test)]
mod tests {
    use super::*;

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
