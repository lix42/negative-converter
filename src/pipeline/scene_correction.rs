//! **Stage 1 of the new rendering chain — scene correction.**
//!
//! Photographic corrections toward what the scene was: **white balance** and
//! **exposure**, and later the flare/fog half of the black point
//! (`nf-scene-correction/flare-removal`). Scene-referred and linear: each is a
//! per-channel gain on linear ACEScg, so the two fold into one multiply and nothing
//! is clamped.
//!
//! Written fresh, per CLAUDE.md's migration rule. `render_split::apply_shared_controls`
//! runs the current chain's version of the same arithmetic, fused with the black
//! point and `linear_range`; it is context for what the controls do, not a template.
//!
//! The corrections act on working-space channels, **after** the NC film RGB v1 3×3 —
//! so this white balance is not reconstruction's `offset`, and this exposure is not
//! its anchor (`docs/design-update.md` Part 1 measures the two white-balance bases
//! ≈2.6 % apart on a neutral). With the anchor a reference-free convention,
//! exposure here is where brightness is set.
//!
//! **Where an already-positive scan will enter** (`io/positive-input-mode`): ahead of
//! this stage, at the working space — a positive is brought to linear ACEScg and
//! then corrected like a negative, because a slide needs white balance and exposure
//! as much as a negative does. So [`AcesCgImage`] is the chain's entry for both, and
//! [`apply`] stays the only producer of a [`SceneReferredImage`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::pipeline::pixels;
use crate::pipeline::working_image::WorkingBuffer;
use crate::pipeline::working_space::AcesCgImage;
use crate::types::{NcError, Result};

/// The white-balance gains, always **stated**: a roll's are measured once by
/// `hanten measure-roll` (`pipeline::roll_white`) and frozen here, so every frame of
/// the roll — and a lone `convert` of one — applies the same gains.
///
/// There is no per-frame estimate. `gray-world` and `percentile` retired with
/// `nf-scene-correction/roll-white-balance`: a frame's own statistics read a sunset
/// as the cast and remove it before highlight desaturation can protect it
/// (`docs/spike/desaturation-band.md`). `crate::recipe::check_body` refuses them by
/// name, and `crate::flow` refuses `--auto-wb`.
///
/// Serialized as `{ "explicit": [r, g, b] }` — the tagged form is kept, one member or
/// not, because it is the recipe contract every new-chain recipe already spells.
/// Unlike the current chain's `print.white_balance` it accepts no bare `[r, g, b]`
/// array: that form is a compatibility alias for recipes older than the tagged one,
/// and this recipe has none.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhiteBalance {
    /// Stated per-channel gains. The default `[1, 1, 1]` is neutral.
    Explicit([f32; 3]),
}

impl Default for WhiteBalance {
    fn default() -> Self {
        WhiteBalance::Explicit([1.0, 1.0, 1.0])
    }
}

/// Scene correction's knobs — and the new chain's `scene_correction` recipe section
/// (`crate::recipe`), field for field.
///
/// A struct rather than an `Option`: a stage is always in the chain, and "this stage
/// is off" is deliberately not expressible — the defaults are the identity, and the
/// report says so.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SceneCorrectionParams {
    /// Where the white-balance gains come from (default neutral).
    pub white_balance: WhiteBalance,
    /// Exposure in stops (EV); `0` is neutral. Applied as the gain `2^exposure`.
    pub exposure: f32,
}

impl Default for SceneCorrectionParams {
    fn default() -> Self {
        Self {
            white_balance: WhiteBalance::default(),
            exposure: 0.0,
        }
    }
}

/// A value [`SceneCorrectionParams::check`] refuses, carried as data so each caller
/// can name the knob the way its command spells it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SceneFault {
    /// A stated white-balance gain is not finite and positive.
    WhiteBalance { channel: usize, value: f32 },
    /// The exposure is not finite, or its gain `2^exposure` is not a normal `f32`.
    Exposure(f32),
    /// A gain times the exposure gain is not a positive normal `f32` — a channel
    /// would render as `0`, `inf` or inverted, though each is usable alone.
    Combined { channel: usize, gain: f32 },
}

impl SceneCorrectionParams {
    /// The value rules on these parameters, which [`apply`] checks again: a
    /// programmatic caller can reach it without `check`.
    pub fn check(&self) -> std::result::Result<(), SceneFault> {
        self.gains().map(|_| ())
    }

    /// Whether the user asked for a correction — whether the stage would move a pixel.
    /// The one predicate a destination that runs no scene correction (`film-master`)
    /// reads to refuse, rather than one rule per knob (the refusal is
    /// `recipe::destination`; the look's is `LookSection::asks_for_a_look`).
    ///
    /// Keyed on the folded **gains**, the test [`apply`] and the report use: the
    /// default is the identity, so there is no default to spare separately, and a
    /// stated identity — an exposure too small to move `2^EV` off `1.0`, a white
    /// balance the exposure cancels — renders exactly what such a destination does.
    /// A value [`check`](Self::check) refuses asks for one too; validation names it
    /// first.
    pub fn asks_for_a_correction(&self) -> bool {
        self.gains() != Ok([1.0, 1.0, 1.0])
    }

    /// The one per-channel multiplier the stage applies — white balance times
    /// `2^exposure` — or the first rule it breaks.
    fn gains(&self) -> std::result::Result<[f32; 3], SceneFault> {
        let WhiteBalance::Explicit(gains) = self.white_balance;
        for (channel, &value) in gains.iter().enumerate() {
            if !value.is_finite() || value <= 0.0 {
                return Err(SceneFault::WhiteBalance { channel, value });
            }
        }
        combined_gains(gains, exposure_gain(self.exposure)?)
    }
}

/// `2^stops`, refused unless it is a **normal** `f32`: an exposure far enough from
/// zero underflows to a subnormal or `0` (every sample silently black) or overflows
/// to `inf`.
fn exposure_gain(stops: f32) -> std::result::Result<f32, SceneFault> {
    let gain = stops.exp2();
    if !stops.is_finite() || !gain.is_normal() {
        return Err(SceneFault::Exposure(stops));
    }
    Ok(gain)
}

/// Fold the white-balance gains and the exposure gain into the one per-channel
/// multiplier the stage applies, refusing a product that is not a normal `f32`.
fn combined_gains(
    white_balance: [f32; 3],
    exposure_gain: f32,
) -> std::result::Result<[f32; 3], SceneFault> {
    let mut gains = [0.0; 3];
    for (channel, (out, wb)) in gains.iter_mut().zip(white_balance).enumerate() {
        let gain = wb * exposure_gain;
        // Positive too: a negative gain is normal, and would invert the channel.
        if !gain.is_normal() || gain < 0.0 {
            return Err(SceneFault::Combined { channel, gain });
        }
        *out = gain;
    }
    Ok(gains)
}

/// What scene correction applied to one frame, for the report.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SceneCorrection {
    /// The white-balance gains applied.
    pub white_balance: [f32; 3],
    /// The exposure applied, in stops.
    pub exposure: f32,
}

impl SceneCorrection {
    /// What the stage did, for the report's stage list. Derived from the resolved
    /// **gains**, by the same test [`apply`] uses to decide whether to touch a pixel,
    /// so the report never names an operation that moved none: an exposure too small
    /// to change `2^EV` from `1.0`, and a white balance the exposure cancels, both
    /// read as `"identity"`.
    pub fn applied(&self) -> &'static str {
        let exposure_gain = self.exposure.exp2();
        if self.white_balance.map(|wb| wb * exposure_gain) == [1.0, 1.0, 1.0] {
            return "identity";
        }
        let white_balance = self.white_balance != [1.0, 1.0, 1.0];
        let exposure = exposure_gain != 1.0;
        match (white_balance, exposure) {
            (false, false) => "identity",
            (true, false) => "white-balance",
            (false, true) => "exposure",
            (true, true) => "white-balance+exposure",
        }
    }
}

/// Linear ACEScg after scene correction: still **scene-referred**, now carrying
/// the photographic corrections.
///
/// The field is private to this module, so [`apply`] is the only function that
/// can mint one — the [`AcesCgImage`] pattern, and what keeps a buffer that
/// skipped the stage out of the next one.
pub struct SceneReferredImage(WorkingBuffer);

impl SceneReferredImage {
    /// Hand the buffer to the next stage. Consuming, so the pixels move rather
    /// than copy.
    pub(in crate::pipeline) fn into_buffer(self) -> WorkingBuffer {
        self.0
    }
}

impl fmt::Debug for SceneReferredImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_named(f, "SceneReferredImage")
    }
}

/// Apply scene correction: `v_c ← v_c · wb_c · 2^exposure` per channel.
///
/// The identity configuration returns the buffer untouched, bit for bit. Otherwise
/// nothing is clamped (clamping happens only at the encoder) and a non-finite sample
/// stays non-finite: repairing one is not this stage's to do, and fit range refuses
/// the frame at the first one, naming the pixel.
pub fn apply(
    image: AcesCgImage,
    params: &SceneCorrectionParams,
) -> Result<(SceneReferredImage, SceneCorrection)> {
    let WhiteBalance::Explicit(white_balance) = params.white_balance;
    let gains = params.gains().map_err(|_| {
        NcError::Other(format!(
            "scene correction cannot apply white balance {white_balance:?} at exposure {} \
             EV: a gain would render as 0, inf or inverted. `SceneCorrectionParams::check` \
             refuses these before a render, so reaching here is a wiring fault",
            params.exposure
        ))
    })?;

    let mut buffer = WorkingBuffer::from_aces(image);
    if gains != [1.0, 1.0, 1.0] {
        pixels::map_in_place(buffer.rgb_mut(), |px| {
            for c in 0..3 {
                px[c] *= gains[c];
            }
        });
    }
    let resolved = SceneCorrection {
        white_balance,
        exposure: params.exposure,
    };
    Ok((SceneReferredImage(buffer), resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::FilmRgbImage;
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::LinearImage;

    /// An `AcesCgImage` whose *film RGB* was exactly `rgb`.
    fn aces_from(width: u32, height: u32, rgb: &[f32]) -> AcesCgImage {
        let film =
            FilmRgbImage::fixture(LinearImage::new(width, height, rgb.to_vec(), None).unwrap());
        map_nc_film_rgb_v1(film)
    }

    fn bits(pixels: &[f32]) -> Vec<u32> {
        pixels.iter().map(|v| v.to_bits()).collect()
    }

    fn run(
        image: AcesCgImage,
        params: &SceneCorrectionParams,
    ) -> Result<(Vec<f32>, SceneCorrection)> {
        let (out, resolved) = apply(image, params)?;
        Ok((out.into_buffer().into_linear().rgb, resolved))
    }

    #[test]
    fn the_default_is_a_bit_exact_identity_and_says_so() {
        let rgb = [
            0.0,
            0.18,
            1.0,
            5.0,
            -0.25,
            f32::INFINITY,
            f32::NAN,
            -0.0,
            0.5,
        ];
        let aces = aces_from(3, 1, &rgb);
        let before = bits(aces.rgb());
        let (out, resolved) = run(aces, &SceneCorrectionParams::default()).unwrap();
        assert_eq!(bits(&out), before);
        assert_eq!(resolved.applied(), "identity");
    }

    #[test]
    fn stated_white_balance_and_exposure_reproduce_a_hand_computed_value() {
        let aces = aces_from(2, 1, &[0.2, 0.4, 0.6, 1.5, f32::NAN, -0.1]);
        let input = aces.rgb().to_vec();
        let params = SceneCorrectionParams {
            white_balance: WhiteBalance::Explicit([1.25, 1.0, 0.5]),
            exposure: 1.0,
        };
        let (out, resolved) = run(aces, &params).unwrap();
        // One multiply per sample by the folded gain `wb_c · 2^1`, exactly.
        let gains = [2.5f32, 2.0, 1.0];
        for (i, (&o, &v)) in out.iter().zip(&input).enumerate() {
            let want = v * gains[i % 3];
            assert_eq!(o.to_bits(), want.to_bits(), "sample {i}: {o} vs {want}");
        }
        assert!(out[4].is_nan(), "a non-finite sample stays non-finite");
        assert_eq!(resolved.applied(), "white-balance+exposure");
        assert_eq!(resolved.white_balance, [1.25, 1.0, 0.5]);
        assert_eq!(resolved.exposure, 1.0);
    }

    #[test]
    fn the_report_names_each_correction_that_ran() {
        let cases = [
            (WhiteBalance::Explicit([1.0, 1.0, 1.0]), 0.0, "identity"),
            (
                WhiteBalance::Explicit([1.1, 1.0, 0.9]),
                0.0,
                "white-balance",
            ),
            (WhiteBalance::Explicit([1.0, 1.0, 1.0]), -0.5, "exposure"),
            // `2^1e-9` rounds to exactly 1.0: no pixel moves, so nothing is claimed.
            (WhiteBalance::Explicit([1.0, 1.0, 1.0]), 1e-9, "identity"),
            // A white balance the exposure cancels exactly is also a no-op.
            (WhiteBalance::Explicit([2.0, 2.0, 2.0]), -1.0, "identity"),
        ];
        for (white_balance, exposure, want) in cases {
            let params = SceneCorrectionParams {
                white_balance,
                exposure,
            };
            let aces = aces_from(1, 1, &[0.3, 0.3, 0.3]);
            let before = bits(aces.rgb());
            let (out, resolved) = run(aces, &params).unwrap();
            assert_eq!(resolved.applied(), want, "{params:?}");
            // A destination that runs no scene correction refuses exactly the
            // parameters that would move a pixel.
            assert_eq!(
                params.asks_for_a_correction(),
                want != "identity",
                "{params:?}"
            );
            // The label and the pixels agree: "identity" exactly when nothing moved.
            assert_eq!(want == "identity", bits(&out) == before, "{params:?}");
        }
    }

    #[test]
    fn unusable_values_are_refused_before_any_pixel_moves() {
        let bad = [
            (WhiteBalance::Explicit([1.0, 0.0, 1.0]), 0.0),
            (WhiteBalance::Explicit([1.0, -1.0, 1.0]), 0.0),
            (WhiteBalance::Explicit([f32::NAN, 1.0, 1.0]), 0.0),
            (WhiteBalance::default(), f32::INFINITY),
            (WhiteBalance::default(), 200.0),
            (WhiteBalance::default(), -200.0),
            // Each usable alone, but the product underflows to a subnormal.
            (WhiteBalance::Explicit([1e-30, 1.0, 1.0]), -100.0),
        ];
        for (white_balance, exposure) in bad {
            let params = SceneCorrectionParams {
                white_balance,
                exposure,
            };
            assert!(params.check().is_err(), "check must refuse {params:?}");
            assert!(
                run(aces_from(1, 1, &[0.3, 0.3, 0.3]), &params).is_err(),
                "apply must refuse {params:?}"
            );
        }
        assert_eq!(SceneCorrectionParams::default().check(), Ok(()));
    }

    #[test]
    fn the_recipe_section_round_trips_and_refuses_a_bare_array() {
        let params = SceneCorrectionParams {
            white_balance: WhiteBalance::Explicit([1.1, 1.0, 0.9]),
            exposure: 0.25,
        };
        let json = serde_json::to_string(&params).unwrap();
        assert_eq!(
            json,
            r#"{"white_balance":{"explicit":[1.1,1.0,0.9]},"exposure":0.25}"#
        );
        assert_eq!(
            serde_json::from_str::<SceneCorrectionParams>(&json).unwrap(),
            params
        );
        // The retired per-frame modes no longer load; `recipe::check_body` names
        // their replacement before serde is reached.
        for retired in [r#""gray-world""#, r#""percentile""#] {
            let json = format!(r#"{{"white_balance":{retired}}}"#);
            assert!(serde_json::from_str::<SceneCorrectionParams>(&json).is_err());
        }
        assert!(
            serde_json::from_str::<SceneCorrectionParams>(r#"{"white_balance":[1,1,1]}"#).is_err()
        );
    }
}
