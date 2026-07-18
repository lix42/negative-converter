//! `sigmoid` — density-domain S-curve (photographic H&D / paper-response) tone
//! mapping, the roadmap's third converter (design-spec §7.3).
//!
//! Shares stages 1–2 ([`to_density`] + the [`regional_balance`] shadow/highlight
//! color balance) and stage 4 (the [`render_print`] print render,
//! [`PrintParams`]) with `density`; only stage 3 — the corrected-density
//! → positive-linear curve — is replaced: the straight line `10^(γ·(D'−Dmax))`
//! becomes an S-curve with toe/shoulder control. Density conversion and print
//! rendering stay **separate** sub-stages (core fidelity rule). Because regional
//! balance is a stage-2 operation on the corrected density, the
//! `density.shadow_balance`/`highlight_balance` knobs apply under `sigmoid`
//! exactly as under `density` (its `Auto` `Dmax` anchor is likewise measured
//! from the *post-balance* densities).
//!
//! ## Curve (per channel, in log₁₀-output space)
//!
//! ```text
//! t = contrast·(D' − Dmax)                       the straight line (log₁₀ of it)
//! F = −contrast·Dmax                             paper-black floor (the line's value at D' = 0)
//! p = F + toe·log10(1 + 10^((t−F)/toe))          toe  FIRST: soft-max with F   (skip if toe = 0)
//! v = p − shoulder·log10(1 + 10^(p/shoulder))    shoulder LAST: soft-min with 0 (skip if shoulder = 0)
//! lin = 10^v
//! ```
//!
//! i.e. the `density` algorithm's straight line passed through two soft knees —
//! a **toe** compressing the approach to paper black, then a **shoulder**
//! compressing the approach to display white.
//!
//! **Knee order matters.** The shoulder (soft-min with the log-output-`0`
//! ceiling) is applied *last*, so nothing can lift the result back above white:
//! for `shoulder > 0`, `v ≤ 0` for every finite density, hence this **stage-3
//! output** is `≤ 1.0` by construction. (The stage-4 print render —
//! `print_exposure`/gains — can lift samples back above `1.0`, and a non-finite
//! density is deliberately passed through unbounded; so u16 clipping is
//! impossible only for finite densities under *neutral* print params, which is the
//! default.) Applying the toe last instead — an earlier version of this curve —
//! lifted the white asymptote to `(1 + 10^(−contrast·Dmax/toe))^toe > 1.0`, which
//! actually clips for a small anchor, e.g. `Dmax = 0.1`, default `toe = 0.2` →
//! ≈ `1.056`; the reorder is the fix. `toe`/`shoulder` are the knee widths in log₁₀ density
//! units; `contrast` is the mid-density slope in log-output space (the
//! `density_gamma` analogue — `density_gamma` itself is **ignored** here; the
//! orchestrator warns when it was customized).
//!
//! Properties (pinned by tests):
//! - **White (highlights):** with `shoulder > 0`, `lin` approaches display white
//!   `1.0` from strictly below as `D' → ∞` and never reaches or exceeds it for any
//!   finite density — so the default u16 encode cannot clip highlights, for *any*
//!   valid params (including a small `Dmax` or a low-contrast auto anchor). With
//!   `shoulder = 0` there is no roll-off and highlights follow the (toe-shaped)
//!   line, which *can* exceed `1.0` (like `density`).
//! - **Black (shadows):** with `toe > 0`, `lin` approaches the paper-black floor
//!   `10^(−contrast·Dmax)` as `D' → −∞` (the shoulder's effect on the floor is
//!   negligible for realistic params; the floor is exactly `10^(−contrast·Dmax)`
//!   when `shoulder = 0`).
//! - **Reduction:** `toe = shoulder = 0` skips both knees and reproduces
//!   `density`'s stage 3 **bit-for-bit** (`10^(contrast·(D'−Dmax))`), so `density`
//!   stays the debuggable straight-line reference.
//! - **Monotonic:** a composition of two monotone-increasing soft knees.
//!
//! **A positive anchor is required.** The S-curve is anchored on `[0, Dmax]` —
//! both the white knee and the black floor (`F = −contrast·Dmax`) derive from a
//! *positive* `Dmax` — so `density.dmax = none` (scene-referred, no anchor) and a
//! degenerate non-positive `Auto` anchor (an all-non-finite buffer, or a wrong
//! film base that pushes most corrected densities negative) are both unusable:
//! with `anchor ≤ 0` the floor sits at or above display white and every sample
//! renders above `1.0` (a quietly-wrong all-white image). The CLI rejects the
//! `none` case at `validate` (exit 2); [`Sigmoid::convert_reported`] guards the
//! resolved anchor finite-and-positive and fails loudly (exit 1) for the `none`
//! programmatic path *and* the degenerate-`Auto` case (the CLAUDE.md film-base
//! gotcha pattern). The anchor is resolved by the same [`resolve_dmax`] (`Auto`
//! percentile / `Explicit`) as `density` — one measurement, not a second one.
//!
//! **Interaction with `--highlight-compress`:** the print render's soft-clip
//! also compresses highlights, but in linear space *after* exposure/WB; the
//! shoulder compresses in density space *before* them. With the shoulder on,
//! default print params keep everything below `1.0`, so the soft-clip (default
//! off) never engages — but both knobs stay honored when set: they compose,
//! neither silently disables the other.
//!
//! **Numerical care.** `log10(1 + 10^y)` is evaluated in the stable form
//! `max(y, 0) + log10(1 + 10^(−|y|))` — the naive form overflows `10^y` to
//! `inf` once `y ≳ 38` (e.g. a tiny-but-nonzero knee width), which would send
//! the knee to `−inf` instead of its asymptote.
//!
//! **Non-finite input propagates (fail-loud).** A non-finite corrected density
//! (`NaN`/`±inf`, e.g. an accepted-but-huge `--density-scale`/`--density-offset`
//! overflowing `to_density`) is returned as-is *before* the knees, and a finite
//! density whose knee math overflows to a non-finite `p` is surfaced too — because
//! the bounded shoulder would otherwise map `+inf` to a clean `10^v = 1.0` and
//! hide the fault from `io::encode`'s non-finite counter. `density` surfaces such
//! blow-ups as `+inf`; `sigmoid` must not be quieter (pinned by tests). This means
//! `10^v ≤ 1.0` is guaranteed only for *finite* stage-3 output — a non-finite
//! sample rides through to the counter instead.

use crate::algo::density::{
    check_base, estimate_wb_gains, regional_balance, render_print, resolve_dmax,
    sample_toned_positive, to_density,
};
use crate::algo::{ConvertReport, Converter};
use crate::types::{
    DensityParams, DmaxSource, FilmBase, LinearImage, NcError, PrintParams, Result, SigmoidParams,
    WbSource,
};

/// Sigmoid / H&D-curve converter.
///
/// Holds all three sub-stages' params: `density` (shared stages 1–2 plus the
/// `dmax` anchor source; its `density_gamma` is ignored — `sigmoid.contrast` is
/// the analogue), `sigmoid` (the stage-3 S-curve), and `print` (the separate
/// stage-4 print render). Keeping them distinct fields preserves the
/// density/print separation.
pub struct Sigmoid {
    pub density: DensityParams,
    pub sigmoid: SigmoidParams,
    pub print: PrintParams,
}

/// Upper bound on `sigmoid.contrast` (mid-density slope), enforced by the CLI
/// `validate`. Beyond this the S-curve degenerates into a near-vertical hard
/// black/white threshold whose knees launder the blow-out into a finite two-level
/// image that trips neither the clip nor the non-finite counter — a silent
/// destruction. `50` is far past any photographic H&D gamma (real curves are
/// ~0.5–3); anyone wanting extreme contrast should use `--algorithm density`,
/// which surfaces the blow-out as `+inf`.
///
/// **Scope of the cap (accepted tradeoff).** This and [`SIGMOID_KNEE_MAX`] reject
/// only *nonsense / degenerate-asymptote* values (a slope so steep, or a knee so
/// wide — e.g. `10000` — that the frame collapses to a two-level or uniform
/// image). They do **not** police aggressive-but-valid params: within the caps an
/// extreme contrast/knee produces faithful, deliberate output that may
/// posterize/crush shadows or highlights — that is the user's choice and is
/// intentionally **not** warned. A "your params look extreme" warning band would
/// false-positive on legitimate high-contrast conversions, so there isn't one.
pub(crate) const SIGMOID_CONTRAST_MAX: f32 = 50.0;

/// Upper bound on the knee widths `sigmoid.toe` / `sigmoid.shoulder` (log₁₀
/// density units), enforced by the CLI `validate`. A huge *finite* width flattens
/// the whole density range into a near-uniform tone — a giant shoulder crushes
/// everything toward black, a giant toe lifts everything toward the floor — and,
/// like an over-large contrast, does so with samples that stay finite and in
/// `[0, 1]`, so *neither* the clip nor the non-finite counter fires: a quietly
/// wrong image. `10` is far beyond the ~`0.05–0.9` photographic knee range (and
/// even ~5× the full density range of a scan), so it rejects only degenerate
/// values while leaving every usable roll-off free. See [`SIGMOID_CONTRAST_MAX`]
/// for why an in-cap-but-aggressive knee is a deliberate, un-warned user choice.
pub(crate) const SIGMOID_KNEE_MAX: f32 = 10.0;

/// `log10(1 + 10^y)` in a numerically stable form: `max(y, 0) + log10(1 + 10^(−|y|))`.
///
/// The naive form overflows `10^y` to `inf` for `y ≳ 38` and would return `inf`
/// where the true value is `≈ y`; the stable form's `10^(−|y|)` only ever
/// *underflows* (to a harmless `0`). NaN input yields NaN output — `max(NaN, 0)`
/// is `0.0` in Rust, but the second term keeps the NaN (see the module doc).
fn log10_1p_pow10(y: f32) -> f32 {
    y.max(0.0) + (1.0 + 10f32.powf(-y.abs())).log10()
}

/// Stage 3 — the S-curve: corrected density `d` → positive linear, anchored on
/// `[0, anchor]`. See the module doc for the formula and its properties. Pure and
/// deterministic; `toe = 0` / `shoulder = 0` skip their knee exactly, so with
/// both zero this is bit-identical to `density`'s `10^(contrast·(d − anchor))`.
///
/// The knees are applied **toe then shoulder** so the shoulder — the soft-min
/// with the white ceiling — runs last and the ceiling can't be lifted afterward
/// (`shoulder > 0` ⇒ `v ≤ 0` ⇒ `lin ≤ 1.0`; see the module doc's "Knee order").
fn s_curve(d: f32, contrast: f32, toe: f32, shoulder: f32, anchor: f32) -> f32 {
    // The pure stage trusts CLI-validated params; a debug assert catches a caller
    // that skipped `validate` (fail-loud discipline for the invariants the curve
    // relies on — a positive slope and non-negative knee widths; a negative width
    // would otherwise be silently read as "knee off").
    debug_assert!(
        contrast > 0.0 && toe >= 0.0 && shoulder >= 0.0,
        "s_curve invariants: contrast > 0 (got {contrast}), toe >= 0 (got {toe}), \
         shoulder >= 0 (got {shoulder})"
    );
    // Fail-loud: a non-finite corrected density (`NaN`/`±inf`, e.g. an accepted-but-
    // huge `--density-scale`/`--density-offset` overflowing `to_density`) must
    // *propagate* — the bounded knees would otherwise launder `+inf` into a clean
    // `10^v = 1.0`, hiding the numerical fault from `io::encode`'s non-finite
    // counter. `density` surfaces this as `+inf`; `sigmoid` must not be quieter.
    if !d.is_finite() {
        return d;
    }
    let t = contrast * (d - anchor);
    // Toe (FIRST): soft-max of the straight line `t` with the paper-black floor
    // `F = −contrast·anchor`, shaping only the shadow approach.
    let floor = -contrast * anchor;
    let p = if toe > 0.0 {
        floor + toe * log10_1p_pow10((t - floor) / toe)
    } else {
        t
    };
    // A *finite* density can still overflow the knee math to a non-finite `p`
    // (e.g. capped-but-large contrast × a huge offset drives `t` past `f32::MAX`);
    // surface that fault too rather than let the shoulder clamp it to white.
    if !p.is_finite() {
        return p;
    }
    // Shoulder (LAST): soft-min of `p` with the white ceiling (log-output 0).
    // Written in the **manifestly-bounded** form `−shoulder·log10(1 + 10^(−p/sh))`
    // (algebraically equal to `p − shoulder·log10(1 + 10^(p/sh))` — factor out
    // `10^(p/sh)`). This form is a negative scalar times a non-negative log, so it
    // is `≤ 0` in f32 *by construction* — the subtraction form rounds a hair above
    // `0` for some `p` (e.g. `10^v = 1.0000006`), which would clip. Running last +
    // this form makes `lin = 10^v ≤ 1.0` truly inviolable for `shoulder > 0`.
    let v = if shoulder > 0.0 {
        -shoulder * log10_1p_pow10(-p / shoulder)
    } else {
        p
    };
    10f32.powf(v)
}

/// Actionable message for a missing / non-positive resolved anchor, pointing at
/// the *actual* cause. Distinguishable cases (only reached on the error path, so
/// the extra finite-scan is fine):
/// - `None` → the anchor is disabled (`density.dmax = none` / `--no-d-max`).
/// - `Some(≤0)` with **no finite densities** → corrupt / all-non-finite input
///   made `Auto` fall back to `0.0`; the base is a red herring.
/// - `Some(≤0)`, `Explicit` source → a non-positive explicit `--d-max` (only
///   reachable programmatically; the CLI validates it positive).
/// - `Some(≤0)`, `Auto` source with finite densities → the film base is wrong
///   enough that scene densities sit at/below it, so the percentile is ≤ 0.
fn anchor_error(resolved: Option<f32>, source: DmaxSource, densities: &[f32]) -> String {
    match resolved {
        None => {
            // The `None` message names `density.dmax = none`, which is sound only
            // because `resolve_dmax` returns `None` *iff* the source is `None`
            // (`Auto`/`Explicit` always return `Some`). Pin that so a future
            // `resolve_dmax` change can't silently misattribute this arm.
            debug_assert!(
                matches!(source, DmaxSource::None),
                "anchor_error None arm assumes source == None, got {source:?}"
            );
            "the sigmoid algorithm needs a display-white anchor (its tone curve is \
             anchored on [0, Dmax]) but density.dmax is `none`; use --auto-d-max / \
             --d-max <d>, or --algorithm density for scene-referred output"
                .to_string()
        }
        Some(a) if matches!(source, DmaxSource::Explicit(_)) => format!(
            "the sigmoid algorithm needs a positive display-white anchor (its tone curve \
             is anchored on [0, Dmax]) but the explicit --d-max is {a}; pass a finite \
             positive density"
        ),
        Some(_) if !densities.iter().any(|d| d.is_finite()) => {
            "the sigmoid algorithm needs a positive display-white anchor but no finite \
             corrected densities were found (the auto anchor fell back to 0.0) — the \
             input is likely corrupt or all-non-finite; check the scan"
                .to_string()
        }
        Some(a) => format!(
            "the sigmoid algorithm needs a positive display-white anchor (its tone curve \
             is anchored on [0, Dmax]) but the auto-measured anchor is {a} — the film \
             base is likely wrong (scene densities sit at/below it); measure a valid Dmin \
             and pass --film-base / --base-region, or set --d-max <d>"
        ),
    }
}

impl Converter for Sigmoid {
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage> {
        Ok(self.convert_reported(image, base)?.0)
    }

    fn convert_reported(
        &self,
        image: &LinearImage,
        base: &FilmBase,
    ) -> Result<(LinearImage, ConvertReport)> {
        // Same precondition as `density`: `to_density` divides by the base.
        // `contrast <= 0` has no runtime backstop here (unlike the anchor): it is
        // a config-only value, fully validated at the CLI boundary (`validate`
        // requires finite `0 < contrast <= SIGMOID_CONTRAST_MAX`), so a stage that
        // trusts its inputs needs no re-check — the `s_curve` debug assert catches
        // a programmatic caller that skipped `validate`.
        check_base(base)?;
        let mut density = to_density(image, base, &self.density);
        // Regional balance completes stage 2 (shared with `density`) *before* the
        // anchor is resolved, so an `Auto` `Dmax` is measured from the
        // post-balance densities — the same ordering contract as `density`.
        let balance_range = regional_balance(&mut density, &self.density)?;
        // One anchor measurement, shared with `density`'s semantics. The S-curve
        // is anchored on `[0, Dmax]` — its white knee and its black floor
        // (`F = −contrast·anchor`) both derive from a *positive* anchor — so an
        // absent, zero, or negative anchor is unusable: with `anchor ≤ 0` the
        // floor sits at or above display white and every sample renders at/above
        // `1.0`, a quietly-wrong all-white image. Guard finite-and-positive and
        // fail loudly (the CLAUDE.md film-base gotcha pattern, mirroring
        // `simple.rs`). `Explicit` is CLI-validated positive, so this only fires
        // on `none` (config/programmatic) or a degenerate `Auto` measurement.
        let resolved = resolve_dmax(&density.density, self.density.dmax);
        let Some(anchor) = resolved.filter(|a| a.is_finite() && *a > 0.0) else {
            return Err(NcError::Other(anchor_error(
                resolved,
                self.density.dmax,
                &density.density,
            )));
        };
        let SigmoidParams {
            contrast,
            toe,
            shoulder,
        } = self.sigmoid;
        // Stage-3 S-curve. A `move` closure over the `Copy` curve params, so it is
        // itself `Copy` and can be passed by value to both the WB sampling and the
        // final render.
        let tone = move |d: f32| s_curve(d, contrast, toe, shoulder, anchor);

        // White balance: explicit gains apply directly; an auto mode is estimated
        // from a neutral positive (unit gains, default print — no exposure / black
        // point / soft-clip), which with those neutral params reduces stage 4 to
        // the identity, so the neutral positive is just the stage-3 `tone`. We
        // apply `tone` to only the strided sample of the density buffer (no
        // full-image render, no clone) and hand that small buffer to the estimator,
        // which no longer strides — bit-identical to rendering the whole neutral
        // positive and striding it. The gains still apply through the *same*
        // stage-4 slot in the final render, so reusing the reported gains via
        // `--white-balance` is bit-identical (measure once, reuse for the roll).
        // Shared with `density` via `sample_toned_positive` / `estimate_wb_gains`.
        // (Regional balance already ran on `density` above, before the anchor.)
        let wb = match self.print.white_balance {
            WbSource::Explicit(gains) => gains,
            auto_mode => {
                let sampled = sample_toned_positive(&density.density, tone);
                estimate_wb_gains(&sampled, auto_mode)?
            }
        };
        let image = render_print(density, tone, wb, &self.print);
        Ok((
            image,
            ConvertReport {
                dmax: resolved,
                white_balance: Some(wb),
                balance_range,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::density::Density;
    use crate::types::BalanceRange;

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    /// A 1×1 RGB image (optionally with a 1-sample IR plane) for pixel-math tests.
    fn pixel(rgb: [f32; 3], ir: Option<f32>) -> LinearImage {
        LinearImage::new(1, 1, rgb.to_vec(), ir.map(|v| vec![v])).unwrap()
    }

    /// The straight-line stage 3 the S-curve must reduce to / deviate from.
    fn line(d: f32, contrast: f32, anchor: f32) -> f32 {
        10f32.powf(contrast * (d - anchor))
    }

    // --- the curve ---------------------------------------------------------

    #[test]
    fn s_curve_reduces_bit_exactly_to_the_straight_line_when_knees_off() {
        // toe = shoulder = 0 must be the identical expression, not merely close —
        // `density` stays the debuggable reference (assert_eq on f32 bits).
        for d in [-0.7, 0.0, 0.31, 1.0, 1.9, 3.5] {
            for (c, a) in [(1.0, 1.1), (1.7, 0.9), (0.6, 2.0)] {
                assert_eq!(
                    s_curve(d, c, 0.0, 0.0, a),
                    line(d, c, a),
                    "d={d} c={c} a={a}"
                );
            }
        }
    }

    #[test]
    fn s_curve_is_monotonic() {
        // Strictly increasing across the anchored domain and its surroundings
        // (both knee derivatives are in (0, 1), so no flat spots).
        let (c, toe, sh, a) = (1.2, 0.2, 0.2, 1.1);
        let mut prev = s_curve(-0.5, c, toe, sh, a);
        for i in 1..=42 {
            let d = -0.5 + 0.05 * i as f32; // −0.5 ..= a + 0.5
            let y = s_curve(d, c, toe, sh, a);
            assert!(y > prev, "not strictly increasing at d={d}: {prev} -> {y}");
            prev = y;
        }
    }

    #[test]
    fn s_curve_white_ceiling_holds_for_any_valid_anchor() {
        // Regression (the toe-lift bug): with the shoulder ON, no finite density
        // may render at or above display white 1.0 — for ANY valid params,
        // including a small anchor and a low-contrast/small-auto-anchor case that
        // the old "shoulder-then-toe" order overshot (`Dmax = 0.1`, default
        // `toe = 0.2` → ≈ 1.056, which clips). Sweep densities well past the
        // anchor for several (contrast, toe, shoulder, anchor) sets.
        let cases = [
            (1.0, 0.2, 0.2, 0.1),              // the reported bug case (small anchor)
            (0.5, 0.2, 0.2, 0.3),              // low contrast, small anchor
            (1.2, 0.2, 0.2, 1.1),              // normal
            (2.0, 0.05, 0.4, 1.6),             // strong shoulder
            (1.0, 0.9, 0.05, 0.15),            // toe ≫ shoulder, small anchor
            (50.0, 0.2, 0.001, 1.5),           // FP-stressful: cap contrast + tiny shoulder
            (1.2, SIGMOID_KNEE_MAX, 0.2, 0.1), // toe at the widened cap, small anchor
        ];
        for (c, toe, sh, a) in cases {
            for i in 0..=600 {
                let d = a + 0.1 * i as f32; // up to a + 60 (deep into the shoulder)
                let y = s_curve(d, c, toe, sh, a);
                assert!(
                    y <= 1.0,
                    "overshoot: s_curve({d}, c={c}, toe={toe}, sh={sh}, a={a}) = {y} > 1.0"
                );
            }
        }
    }

    #[test]
    fn s_curve_manifest_form_beats_the_naive_subtraction_form() {
        // Pin the *manifestly-bounded* shoulder form (vs the algebraically-equal
        // subtraction form `p − sh·log10(1+10^(p/sh))`, which rounds a hair above 0
        // in f32 → `10^v > 1.0` → clips). At an FP-stressful point the naive form
        // must exceed 1.0 while `s_curve` stays ≤ 1.0 — a guard against a future
        // revert silently reintroducing the overshoot.
        let (c, toe, sh, a) = (50.0f32, 0.2f32, 0.001f32, 1.5f32);
        let mut naive_overshot = false;
        for i in 0..=600 {
            let d = a + 0.1 * i as f32;
            // Reproduce the toe (identical) then the *naive* subtraction shoulder.
            let t = c * (d - a);
            let floor = -c * a;
            let p = floor + toe * log10_1p_pow10((t - floor) / toe);
            let naive = 10f32.powf(p - sh * log10_1p_pow10(p / sh));
            if naive > 1.0 {
                naive_overshot = true;
            }
            assert!(
                s_curve(d, c, toe, sh, a) <= 1.0,
                "manifest form overshot at d={d}"
            );
        }
        assert!(
            naive_overshot,
            "test point not FP-stressful enough — the naive form never overshot, \
             so it doesn't prove the manifest form matters"
        );
    }

    #[test]
    fn s_curve_white_asymptote_bounds_highlights() {
        // Above the anchor the shoulder rolls off toward exactly 1.0 from below —
        // highlights never reach or overshoot display white (the straight line
        // would put d = a + 1 at 10^c ≈ 16×). Still monotonic on the way there.
        let (c, toe, sh, a) = (1.2, 0.2, 0.2, 1.1);
        let near = s_curve(a + 0.5, c, toe, sh, a);
        let far = s_curve(a + 5.0, c, toe, sh, a);
        assert!(near > s_curve(a, c, toe, sh, a) && near < 1.0);
        assert!(far > near && far <= 1.0);
        assert!(
            approx(far, 1.0, 1e-3),
            "asymptote is display white, got {far}"
        );
    }

    #[test]
    fn s_curve_shoulder_off_lets_highlights_exceed_white() {
        // The documented complement of the white-ceiling guarantee: with
        // `shoulder = 0` there is no roll-off, so a highlight above the anchor
        // follows the (toe-shaped) line and *can* exceed 1.0 — like `density`.
        let (c, toe, a) = (1.2, 0.2, 1.1);
        let bright = s_curve(a + 1.0, c, toe, 0.0, a);
        assert!(
            bright > 1.0,
            "shoulder = 0 must not cap highlights, got {bright}"
        );
    }

    #[test]
    fn s_curve_black_floor_bounds_shadows() {
        // Below D' = 0 (thinner than the base) the toe holds the paper-black
        // floor ≈ 10^(−c·a) — where the straight line renders the base itself.
        // (With the shoulder applied last it nudges the floor a hair below 10^(−c·a),
        // negligibly for these params — the toe-first reorder trades a raised white
        // asymptote for an imperceptibly lowered black floor; see the module doc.)
        let (c, toe, sh, a) = (1.2, 0.2, 0.2, 1.1);
        let floor = 10f32.powf(-c * a);
        let near = s_curve(-0.3, c, toe, sh, a);
        let far = s_curve(-4.0, c, toe, sh, a);
        assert!(near > floor && near < s_curve(0.0, c, toe, sh, a));
        assert!(far > 0.0 && far < near);
        assert!(approx(far, floor, floor * 1e-3), "asymptote is paper black");
    }

    #[test]
    fn s_curve_small_knees_approach_the_line() {
        // toe/shoulder → 0 converges to the straight-line map (mid-domain the
        // knees are negligible even at moderate widths).
        let (c, a) = (1.2, 1.1);
        for d in [0.2, 0.55, 0.9] {
            let l = line(d, c, a);
            assert!(approx(s_curve(d, c, 0.01, 0.01, a), l, l * 1e-3), "d={d}");
        }
    }

    #[test]
    fn s_curve_shoulder_and_toe_bend_in_opposite_directions() {
        // Relative to the straight line: the shoulder pulls near-white values
        // down (compressing them under the 1.0 asymptote), the toe lifts deep
        // shadows up (holding the black floor); midtones match the line.
        let (c, toe, sh, a) = (1.2, 0.2, 0.2, 1.1);
        assert!(s_curve(a, c, toe, sh, a) < line(a, c, a)); // shoulder: darker
        assert!(s_curve(-0.5, c, toe, sh, a) > line(-0.5, c, a)); // toe: lighter
        let mid = s_curve(a / 2.0, c, toe, sh, a);
        let lmid = line(a / 2.0, c, a);
        assert!(approx(mid, lmid, lmid * 1e-2)); // midtones: on the line
    }

    #[test]
    fn s_curve_propagates_non_finite() {
        // A non-finite corrected density must come out non-finite (not laundered by
        // the bounded knees into a clean in-range sample) so io::encode's
        // non-finite counter can surface it — `density` surfaces +inf, sigmoid must
        // not be quieter. Covers NaN and BOTH infinities, knees on and off.
        for (toe, sh) in [(0.2, 0.2), (0.0, 0.0)] {
            assert!(s_curve(f32::NAN, 1.2, toe, sh, 1.1).is_nan());
            assert!(!s_curve(f32::INFINITY, 1.2, toe, sh, 1.1).is_finite());
            assert!(!s_curve(f32::NEG_INFINITY, 1.2, toe, sh, 1.1).is_finite());
        }
        // A *finite* density whose knee math overflows `t` past f32::MAX (capped
        // contrast × a huge density) must also surface, not clamp to white — with
        // the knees ON (caught at the `p`-overflow guard) and OFF (the `p = t`
        // reduction path, caught at the same guard).
        assert!(!s_curve(1e38, SIGMOID_CONTRAST_MAX, 0.2, 0.2, 1.5).is_finite());
        assert!(!s_curve(1e38, SIGMOID_CONTRAST_MAX, 0.0, 0.0, 1.5).is_finite());
        assert!(log10_1p_pow10(f32::NAN).is_nan());
    }

    #[test]
    fn log10_1p_pow10_is_stable_for_extreme_arguments() {
        // Naive log10(1 + 10^y) overflows at y ≳ 38; the stable form must return
        // ≈ y there (and ≈ 0 for very negative y), keeping tiny knee widths sane.
        assert!(approx(log10_1p_pow10(1000.0), 1000.0, 1e-3));
        assert!(approx(log10_1p_pow10(-1000.0), 0.0, 1e-6));
        assert!(approx(log10_1p_pow10(0.0), std::f32::consts::LOG10_2, 1e-6));
    }

    // --- Converter ----------------------------------------------------------

    fn sigmoid(dmax: DmaxSource, params: SigmoidParams) -> Sigmoid {
        Sigmoid {
            density: DensityParams {
                dmax,
                ..DensityParams::default()
            },
            sigmoid: params,
            print: PrintParams::default(),
        }
    }

    #[test]
    fn convert_with_knees_off_matches_density_bit_exactly() {
        // End-to-end reduction: contrast = density_gamma, toe = shoulder = 0, same
        // explicit anchor ⇒ identical output bits (the whole shared path plus the
        // reduced stage 3 must line up, not just the curve in isolation).
        let base = FilmBase::from([0.6, 0.3, 0.18]);
        let img = LinearImage::new(
            2,
            1,
            vec![0.5, 0.25, 0.15, 0.06, 0.03, 0.02],
            Some(vec![0.4, 0.6]),
        )
        .unwrap();
        let gamma = 1.4;
        let dmax = DmaxSource::Explicit(1.2);
        let sig = Sigmoid {
            density: DensityParams {
                density_gamma: 99.0, // must be ignored — contrast drives the curve
                dmax,
                ..DensityParams::default()
            },
            sigmoid: SigmoidParams {
                contrast: gamma,
                toe: 0.0,
                shoulder: 0.0,
            },
            print: PrintParams::default(),
        };
        let den = Density {
            density: DensityParams {
                density_gamma: gamma,
                dmax,
                ..DensityParams::default()
            },
            print: PrintParams::default(),
        };
        let a = sig.convert(&img, &base).unwrap();
        let b = den.convert(&img, &base).unwrap();
        assert_eq!(
            a.rgb, b.rgb,
            "reduced sigmoid must equal density bit-for-bit"
        );
        assert_eq!(a.ir, b.ir);
    }

    #[test]
    fn regional_balance_applies_under_sigmoid() {
        // Regional balance is a shared stage-2 operation, so `--shadow-balance`
        // etc. must take effect under `sigmoid` (not be a silent no-op) and must
        // be reported. A crossover-injected shadow gets a non-zero shadow
        // balance; the output must differ from the neutral-balance sigmoid run,
        // and the reported range must be present.
        let base = FilmBase::from([0.6, 0.3, 0.18]);
        let img = LinearImage::new(2, 1, vec![0.5, 0.25, 0.15, 0.06, 0.03, 0.02], None).unwrap();
        let dmax = DmaxSource::Explicit(1.2);
        let params = SigmoidParams {
            contrast: 1.4,
            toe: 0.1,
            shoulder: 0.2,
        };
        let neutral = sigmoid(dmax, params.clone());
        let balanced = Sigmoid {
            density: DensityParams {
                dmax,
                shadow_balance: [0.2, 0.0, -0.1],
                highlight_balance: [-0.1, 0.05, 0.0],
                ..DensityParams::default()
            },
            sigmoid: params,
            print: PrintParams::default(),
        };
        let (out_neutral, rep_neutral) = neutral.convert_reported(&img, &base).unwrap();
        let (out_balanced, rep_balanced) = balanced.convert_reported(&img, &base).unwrap();
        assert_eq!(
            rep_neutral.balance_range, None,
            "neutral: no range reported"
        );
        assert!(
            rep_balanced.balance_range.is_some(),
            "balanced: range must be reported under sigmoid"
        );
        assert_ne!(
            out_neutral.rgb, out_balanced.rgb,
            "regional balance must change sigmoid output, not no-op"
        );
    }

    #[test]
    fn regional_balance_composes_the_same_in_sigmoid_and_density() {
        // With knees off, sigmoid reduces to density's straight line — and the
        // shared regional-balance sub-stage must apply identically in both, so
        // the two produce bit-identical output for the same balance params. Pins
        // that sigmoid reuses `regional_balance` rather than a divergent copy.
        let base = FilmBase::from([0.6, 0.3, 0.18]);
        let img = LinearImage::new(2, 1, vec![0.5, 0.25, 0.15, 0.06, 0.03, 0.02], None).unwrap();
        let gamma = 1.4;
        let dmax = DmaxSource::Explicit(1.2);
        let density = DensityParams {
            density_gamma: gamma,
            dmax,
            shadow_balance: [0.15, 0.0, -0.05],
            highlight_balance: [-0.05, 0.02, 0.0],
            balance_range: BalanceRange::Explicit([0.2, 1.8]),
            ..DensityParams::default()
        };
        let sig = Sigmoid {
            density: DensityParams {
                density_gamma: 99.0, // ignored — contrast drives the curve
                ..density.clone()
            },
            sigmoid: SigmoidParams {
                contrast: gamma,
                toe: 0.0,
                shoulder: 0.0,
            },
            print: PrintParams::default(),
        };
        let den = Density {
            density,
            print: PrintParams::default(),
        };
        let a = sig.convert(&img, &base).unwrap();
        let b = den.convert(&img, &base).unwrap();
        assert_eq!(
            a.rgb, b.rgb,
            "shared regional balance must match bit-for-bit"
        );
    }

    #[test]
    fn convert_requires_a_dmax_anchor() {
        // `dmax = none` cannot drive the S-curve: fail loudly (the CLI validate
        // rejects it earlier; this is the programmatic backstop).
        let conv = sigmoid(DmaxSource::None, SigmoidParams::default());
        let err = conv
            .convert(
                &pixel([0.2, 0.2, 0.2], None),
                &FilmBase::from([0.6, 0.6, 0.6]),
            )
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        // Assert a `None`-branch-specific token (all three anchor_error messages
        // contain "anchor", so that alone wouldn't pin the None branch).
        assert!(
            err.to_string().contains("scene-referred"),
            "None branch should point at scene-referred / density: {err}"
        );
    }

    #[test]
    fn convert_rejects_a_non_positive_auto_anchor() {
        // A degenerate Auto anchor (≤ 0) must fail loudly, not render an all-white
        // image: here every pixel has higher transmission than the base (scan > base), so
        // every corrected density is negative and the measured percentile is < 0.
        // Guards the CLAUDE.md film-base gotcha for the anchor path.
        let base = FilmBase::from([0.2, 0.2, 0.2]);
        let img = LinearImage::new(2, 1, vec![0.8, 0.8, 0.8, 0.7, 0.7, 0.7], None).unwrap();
        let conv = sigmoid(DmaxSource::Auto, SigmoidParams::default());
        let err = conv.convert(&img, &base).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        // Finite-but-wrong-base case → the message points at the film base.
        assert!(
            err.to_string().contains("base"),
            "wrong-base message: {err}"
        );
        // A negative explicit anchor smuggled past the CLI (programmatic) also
        // fails, and the message blames the *explicit* value — not the film base
        // (LOW-7: the wrong-base wording would misattribute this).
        let conv = sigmoid(DmaxSource::Explicit(-0.5), SigmoidParams::default());
        let err = conv
            .convert(
                &pixel([0.2, 0.2, 0.2], None),
                &FilmBase::from([0.6, 0.6, 0.6]),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("explicit") && !err.contains("film base"),
            "explicit-anchor message should blame --d-max, not the base: {err}"
        );
    }

    #[test]
    fn anchor_error_distinguishes_corrupt_input_from_bad_base() {
        // All-non-finite input makes `to_density` yield all-NaN densities, so the
        // Auto anchor falls back to 0.0 — the guard must blame the *input*, not the
        // (valid) base (LOW-4: a misleading "measure a valid Dmin" would misdirect).
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = pixel([f32::NAN, f32::NAN, f32::NAN], None);
        let conv = sigmoid(DmaxSource::Auto, SigmoidParams::default());
        let err = conv.convert(&img, &base).unwrap_err().to_string();
        assert_eq!(conv.convert(&img, &base).unwrap_err().exit_code(), 1);
        assert!(
            err.contains("corrupt") || err.contains("non-finite"),
            "corrupt-input message should not blame the base: {err}"
        );
        assert!(
            !err.contains("Dmin"),
            "must not misdirect to the film base: {err}"
        );
    }

    #[test]
    fn convert_propagates_non_finite_scan_to_output() {
        // End-to-end through the converter: a non-finite scan sample → NaN
        // corrected density (to_density) → the curve must propagate it (not launder
        // it via the bounded shoulder) so it reaches the output for io::encode's
        // non-finite counter to surface. A CLI-driven overflow isn't constructible
        // on the committed fixture (its densities are too small to overflow f32
        // within validated param ranges), so this pins the path deterministically.
        // Explicit anchor so the (mostly-finite) buffer still resolves one.
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img =
            LinearImage::new(2, 1, vec![0.2, 0.2, 0.2, f32::INFINITY, 0.2, 0.2], None).unwrap();
        let conv = sigmoid(DmaxSource::Explicit(1.2), SigmoidParams::default());
        let out = conv.convert(&img, &base).unwrap();
        assert!(
            !out.rgb[3].is_finite(),
            "non-finite scan must ride through to output, not be laundered: {}",
            out.rgb[3]
        );
        // Finite neighbours stay finite and bounded (the fault is isolated).
        assert!(out.rgb[0].is_finite() && out.rgb[0] <= 1.0);
    }

    #[test]
    fn convert_reported_surfaces_the_resolved_anchor() {
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = pixel([0.2, 0.2, 0.2], None);

        let conv = sigmoid(DmaxSource::Explicit(1.25), SigmoidParams::default());
        let (_, rep) = conv.convert_reported(&img, &base).unwrap();
        assert_eq!(rep.dmax, Some(1.25));
        // The default (neutral) print reports its resolved gains too.
        assert_eq!(rep.white_balance, Some([1.0, 1.0, 1.0]));

        let conv = sigmoid(DmaxSource::Auto, SigmoidParams::default());
        let (_, rep) = conv.convert_reported(&img, &base).unwrap();
        assert!(rep.dmax.is_some_and(f32::is_finite));
    }

    #[test]
    fn auto_wb_convert_neutralizes_a_cast_end_to_end() {
        // Mirror of the density end-to-end test for the sigmoid path: a wrong
        // (neutral) base leaves a constant per-channel cast in the positive, and
        // both auto modes must estimate gains that equalize the channels. Exercises
        // the sigmoid-owned estimate→apply orchestration (its own analysis pass).
        // Knees off (toe = shoulder = 0) so stage 3 is the straight-line power form
        // (which preserves the constant per-channel density offset across both
        // tones); with the S-curve's non-linear knees on, the two tones would map
        // by different channel ratios and per-pixel equalization wouldn't hold —
        // the bit-exact reuse test below covers the knees-on path.
        let base = FilmBase::from([0.8, 0.8, 0.8]); // deliberately ignores the mask
        let cast = [0.5f32, 0.3, 0.2]; // orange-ish transmissions
        let mut rgb = Vec::new();
        for i in 0..64 {
            let t = if i % 2 == 0 { 1.0 } else { 0.5 }; // two-tone content
            rgb.extend_from_slice(&[cast[0] * t, cast[1] * t, cast[2] * t]);
        }
        let img = LinearImage::new(64, 1, rgb, None).unwrap();
        let straight = SigmoidParams {
            toe: 0.0,
            shoulder: 0.0,
            ..SigmoidParams::default()
        };
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            let conv = Sigmoid {
                density: DensityParams::default(),
                sigmoid: straight.clone(),
                print: PrintParams {
                    white_balance: mode,
                    ..PrintParams::default()
                },
            };
            let (out, rep) = conv.convert_reported(&img, &base).unwrap();
            let gains = rep.white_balance.expect("gains reported");
            assert_eq!(gains[1], 1.0, "{mode:?} green-anchored");
            for px in out.rgb.chunks_exact(3) {
                assert!(approx(px[0], px[1], 1e-4), "{mode:?}: {px:?}");
                assert!(approx(px[1], px[2], 1e-4), "{mode:?}: {px:?}");
            }
        }
    }

    #[test]
    fn auto_wb_output_is_bit_exact_with_explicit_rerun_of_reported_gains() {
        // The sigmoid measure-once-reuse contract: reusing the reported gains via
        // explicit `--white-balance` reproduces the auto run bit-for-bit, because
        // application goes through the same stage-4 slot sharing the resolved
        // anchor. Non-default print + curve params prove it holds with black_point,
        // the soft-clip, and the S-curve knees in play.
        let base = FilmBase::from([0.6, 0.35, 0.2]);
        let img = LinearImage::new(
            3,
            2,
            vec![
                0.5, 0.3, 0.15, 0.3, 0.2, 0.1, 0.2, 0.1, 0.05, //
                0.45, 0.25, 0.12, 0.1, 0.06, 0.03, 0.55, 0.32, 0.18,
            ],
            None,
        )
        .unwrap();
        let print = PrintParams {
            print_exposure: 0.3,
            black_point: 0.02,
            white_balance: WbSource::Percentile,
            highlight_compress: 0.4,
        };
        let curve = SigmoidParams {
            contrast: 1.4,
            toe: 0.15,
            shoulder: 0.3,
        };
        let auto = Sigmoid {
            density: DensityParams::default(),
            sigmoid: curve.clone(),
            print: print.clone(),
        };
        let (out_auto, rep) = auto.convert_reported(&img, &base).unwrap();
        let gains = rep.white_balance.expect("auto gains reported");

        let explicit = Sigmoid {
            density: DensityParams::default(),
            sigmoid: curve,
            print: PrintParams {
                white_balance: WbSource::Explicit(gains),
                ..print
            },
        };
        let (out_explicit, rep2) = explicit.convert_reported(&img, &base).unwrap();
        assert_eq!(out_auto.rgb, out_explicit.rgb, "reuse must be bit-exact");
        assert_eq!(rep2.white_balance, Some(gains));
        assert_eq!(rep.dmax, rep2.dmax, "shared anchor");
    }

    #[test]
    fn auto_wb_carries_ir_through_the_final_output() {
        // The auto-WB analysis pass renders on an IR-dropped copy (perf: no
        // image-sized IR clone), but the *final* render must still consume the
        // original density with its IR plane intact — assert the IR rides through.
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = pixel([0.2, 0.2, 0.2], Some(0.42));
        let conv = Sigmoid {
            density: DensityParams::default(),
            sigmoid: SigmoidParams::default(),
            print: PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
        };
        let out = conv.convert(&img, &base).unwrap();
        assert_eq!(out.ir.as_deref(), Some(&[0.42_f32][..]));
    }

    #[test]
    fn convert_carries_ir_untouched_and_rejects_bad_base() {
        let conv = sigmoid(DmaxSource::Auto, SigmoidParams::default());
        let img = pixel([0.2, 0.2, 0.2], Some(0.33));
        let out = conv
            .convert(&img, &FilmBase::from([0.6, 0.6, 0.6]))
            .unwrap();
        assert_eq!(out.ir.as_deref(), Some(&[0.33_f32][..]));
        // Same base guard as density (shared check_base).
        let err = conv
            .convert(&img, &FilmBase::from([0.6, 0.0, 0.6]))
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn convert_is_positive_polarity_denser_is_brighter() {
        // The polarity contract holds for the S-curve too: a denser negative
        // area (scene highlight) renders brighter than a thin one.
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = LinearImage::new(2, 1, vec![0.55, 0.55, 0.55, 0.05, 0.05, 0.05], None).unwrap();
        let conv = sigmoid(DmaxSource::Auto, SigmoidParams::default());
        let out = conv.convert(&img, &base).unwrap();
        for c in 0..3 {
            assert!(
                out.rgb[3 + c] > out.rgb[c],
                "denser pixel should be brighter (channel {c})"
            );
        }
    }

    #[test]
    fn convert_default_output_fills_but_never_exceeds_display_range() {
        // With the shoulder on and neutral print params, every sample lands in
        // (0, 1]: the white asymptote makes u16 highlight clipping impossible by
        // construction (a key behavioral difference from the straight line).
        let base = FilmBase::from([0.55, 0.27, 0.16]);
        let img = LinearImage::new(
            2,
            1,
            vec![0.39, 0.19, 0.09, 0.001, 0.001, 0.001], // normal pixel + very dense pixel
            None,
        )
        .unwrap();
        let conv = sigmoid(DmaxSource::Auto, SigmoidParams::default());
        let out = conv.convert(&img, &base).unwrap();
        for (i, v) in out.rgb.iter().enumerate() {
            assert!(v.is_finite() && *v > 0.0 && *v <= 1.0, "sample {i}: {v}");
        }
    }

    #[test]
    fn auto_wb_measures_post_regional_balance_density() {
        // Sigmoid analogue of the density ordering guard: its own
        // `convert_reported` also runs `regional_balance` before estimating the
        // auto-WB gains, so the gains must reflect the post-balance density. No
        // other sigmoid test runs both features at once, so a refactor moving the
        // WB estimate ahead of the balance would go unnoticed here too. Knees off
        // (toe = shoulder = 0) so the analysis positive stays the straight-line
        // power form, matching the other sigmoid auto-WB tests.
        let base = FilmBase::from([0.6, 0.35, 0.2]);
        let img = LinearImage::new(
            3,
            2,
            vec![
                0.5, 0.3, 0.15, 0.3, 0.2, 0.1, 0.2, 0.1, 0.05, //
                0.45, 0.25, 0.12, 0.1, 0.06, 0.03, 0.55, 0.32, 0.18,
            ],
            None,
        )
        .unwrap();
        let straight = SigmoidParams {
            toe: 0.0,
            shoulder: 0.0,
            ..SigmoidParams::default()
        };
        // Tone-dependent crossover cast; green untouched so it stays the anchor.
        let balance = DensityParams {
            shadow_balance: [-0.15, 0.0, 0.08],
            highlight_balance: [0.15, 0.0, -0.08],
            ..DensityParams::default()
        };

        let neutral = Sigmoid {
            density: DensityParams::default(),
            sigmoid: straight.clone(),
            print: PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
        };
        let balanced = Sigmoid {
            density: balance,
            sigmoid: straight,
            print: PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
        };
        let (_, rep_neutral) = neutral.convert_reported(&img, &base).unwrap();
        let (_, rep_balanced) = balanced.convert_reported(&img, &base).unwrap();

        // (a) WB measured post-balance differs from WB with no balance applied.
        let wb_neutral = rep_neutral.white_balance.expect("neutral gains reported");
        let wb_balanced = rep_balanced.white_balance.expect("balanced gains reported");
        assert_ne!(
            wb_neutral, wb_balanced,
            "auto-WB must be measured on the post-balance density under sigmoid"
        );

        // (b) Both fields present and internally consistent in the one report.
        let [lo, hi] = rep_balanced.balance_range.expect("range reported");
        assert!(
            lo.is_finite() && hi.is_finite() && lo < hi,
            "range [{lo}, {hi}]"
        );
        assert_eq!(wb_balanced[1], 1.0, "green-anchored");
        assert!(
            wb_balanced.iter().all(|g| g.is_finite() && *g > 0.0),
            "usable gains {wb_balanced:?}"
        );
    }
}
