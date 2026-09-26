//! `density` — density-domain reconstruction (Cineon / negadoctor style), the
//! default, plus the exponential density curve. Density reconstruction and the
//! density curve are **separate** sub-stages, and print rendering happens later,
//! past the ACEScg boundary (core fidelity rule from design-spec §3/§7.2):
//!
//! ## Model (per channel `c`)
//!
//! ```text
//! 1. transmission → density:   D_c  = -log10(max(scan_c, EPS) / base_c)
//! 2. density correction:       D'_c = scale_c · D_c + offset_c
//! 3. density curve:            exponential lin_c = 10^(gamma · (D'_c − A))
//!                              → FilmRgbImage
//! ```
//!
//! Stages 1–2 are [`to_density`] — **density
//! reconstruction**, owned by [`reconstruct`], which then applies the
//! curve ([`apply_curve`], stage 3) to produce the typed
//! [`FilmRgbImage`] boundary. The print controls run downstream of it, in
//! `pipeline::render_split`, after the NC film RGB v1 → ACEScg mapping.
//!
//! **Anchor — owned by the curve stage.** The exponential renders density
//! relative to an anchor `A`: `10^(γ·(D′ − A))`, so `D′ = A` maps to `1.0`. `A`
//! comes from [`AnchorPlacement`], whose one rule pins mid-grey a stated density
//! above the film base. It reads nothing off the frame and no roll reference
//! density, so the render carries no leader's roll-to-roll error, and darker
//! frames render darker (faithful relative exposure).
//!
//! **Polarity.** With `D = -log10(scan/base)` the density is `≥ 0` and *grows*
//! with the film's optical density: the unexposed base (scene black) sits at
//! `D = 0`, and a dense negative area (a scene highlight) has a large `D`. A
//! true positive must get *brighter* as `D` grows, so stage 3 uses
//! `10^(+γ·D')`. This matches darktable `negadoctor`, whose print output
//! increases with film density (verified against its source: denser negative →
//! brighter print).
//!
//! Output is linear, and nothing is clamped here — the encode stage counts and
//! reports any out-of-range samples.

use rayon::prelude::*;

use crate::algo::{FilmRgbImage, ReconstructionReport};
#[cfg(doc)]
use crate::types::AnchorPlacement;
use crate::types::{DensityParams, ExponentialParams, FilmBase, LinearImage, NcError, Result};

/// Floor applied to the scan transmission before the `log10`, so a zero / negative
/// / denormal sample can't produce `-inf`/`NaN` density (design "fail loudly, never
/// a quietly wrong image" — a dead pixel becomes a very high but finite density
/// rather than poisoning the channel). `1e-6` ≈ −20 stops below unity: darker than
/// any real detail, yet leaves ample headroom before `10^(γ·D)` can overflow `f32`.
///
/// `pub(crate)` rather than `pub(super)` only so `pipeline::stages::golden` can recompute
/// stage 1 in `f64` when it measures how close the shipped `log10` lands to an f32
/// rounding boundary; nothing outside `algo` consumes it at runtime.
pub(crate) const SCAN_EPSILON: f32 = 1e-6;

/// Corrected per-pixel film density `D'` (interleaved RGB), the boundary between
/// the reconstruction sub-stages: the output of [`to_density`] (stages 1–2) and the input to the density curve
/// ([`apply_curve`], stage 3). The IR plane is carried through untouched
/// (Step-1 rule: preserve, don't consume).
///
/// Algo-internal (`pub(crate)`), not a cross-stage contract type — the typed
/// cross-stage boundary is [`FilmRgbImage`], which only the curve stage mints.
/// It has no validated constructor; its length invariants
/// (`density.len() == w*h*3`, `ir.len() == w*h`) hold by construction because
/// [`to_density`], its only producer, derives them from a validated
/// [`LinearImage`].
#[derive(Clone, Debug)]
pub(crate) struct DensityImage {
    pub width: u32,
    pub height: u32,
    /// Corrected density `D'`, interleaved `r,g,b, r,g,b, …`, `len == w*h*3`.
    pub density: Vec<f32>,
    /// Carried-through IR plane (HDRi input), `len == w*h` when present.
    pub ir: Option<Vec<f32>>,
}

/// Density reconstruction + the tagged curve (stages 1–3, design-spec §7.2):
/// Dmin-normalize into corrected density `D′`, place the curve's anchor, then map `D′` through the
/// curve into the typed [`FilmRgbImage`]. Pure; the print controls are
/// deliberately **not** here — they run past the ACEScg boundary
/// (`pipeline::render_split`).
pub(super) fn reconstruct(
    image: &LinearImage,
    base: &FilmBase,
    params: &DensityParams,
    curve: &ExponentialParams,
) -> Result<(FilmRgbImage, ReconstructionReport)> {
    // `to_density` divides by the per-channel base, so a zero / negative /
    // non-finite base would yield a silently-black or non-finite image.
    // `film_base::estimate` guards every source at birth; this is defense-in-depth
    // for a base reaching the algorithm by another route. Fail loudly instead.
    check_base(base)?;
    let density = to_density(image, base, params);

    // The anchor is applied **in the exponent** — `10^(γ·(D' − A))` — not as a
    // separate `10^(−γ·A)` gain: mathematically equivalent, but the factored
    // form overflows `f32` when `γ·D'` alone exceeds the pow10 range even
    // though the anchored exponent is small (e.g. `γ = 5`, EPS-clamped
    // `D' ≈ 8`), turning white into `inf` instead of `1.0`.
    let gamma = curve.gamma;
    let anchor = curve.anchor.anchor(gamma);
    // Defense in depth. Two ways the exponent goes non-finite, and **both**
    // render `10^(−inf) = 0.0` for every sample — an all-black frame that trips
    // neither the clip nor the non-finite counter: the placement's division by
    // the slope can overflow the *anchor* (a positive-but-tiny gamma), and a
    // large-but-finite anchor can overflow the *product* `gamma · anchor`.
    // `validate` rejects both at the CLI boundary, naming the flag; a
    // programmatic caller reaches here first.
    if !anchor.is_finite() || !(gamma * anchor).is_finite() {
        return Err(NcError::Other(format!(
            "the exponential anchor placement derived a non-usable anchor \
             ({anchor:e}) at gamma {gamma}: the curve's exponent \
             `gamma · (density − anchor)` is not finite, so every sample would \
             render as exactly 0.0. Use a photographic gamma and a smaller anchor \
             offset"
        )));
    }
    let film = apply_curve(density, move |d| 10f32.powf(gamma * (d - anchor)));
    Ok((
        film,
        ReconstructionReport {
            curve_anchor: anchor,
        },
    ))
}

/// Stages 1–2 — transmission → corrected density (pure).
///
/// `D_c = -log10(max(scan_c, EPS) / base_c)` then `D'_c = scale_c·D_c + offset_c`.
/// Dividing by the *per-channel* base is what neutralizes the orange mask: at the
/// base every channel lands on `D = 0`, so an unexposed sample is neutral before
/// any correction; `offset` / `scale` then trim the per-channel density balance
/// and contrast.
///
/// `base` must be finite and `> 0` per channel; [`reconstruct`] enforces this
/// before calling (the CLI validates an explicit base, but an auto/region-estimated
/// base is only checked there), so this stage trusts its inputs and never fails.
///
/// A **non-finite** scan sample (`NaN`/`±inf`) is propagated as `NaN` density rather
/// than laundered by the floor, so `io::encode`'s non-finite counter still surfaces
/// corrupt input downstream. The `SCAN_EPSILON` floor applies only to *finite*
/// zero/negative/denormal transmission (the physically-real dead-pixel case).
pub(crate) fn to_density(
    image: &LinearImage,
    base: &FilmBase,
    params: &DensityParams,
) -> DensityImage {
    let base = [base.r, base.g, base.b];
    let scale = params.scale;
    let offset = params.offset;

    let mut density = vec![0.0f32; image.rgb.len()];
    density
        .par_chunks_exact_mut(3)
        .zip(image.rgb.par_chunks_exact(3))
        .for_each(|(out, px)| {
            for c in 0..3 {
                let s = px[c];
                let d = if s.is_finite() {
                    -(s.max(SCAN_EPSILON) / base[c]).log10()
                } else {
                    f32::NAN
                };
                out[c] = scale[c] * d + offset[c];
            }
        });

    DensityImage {
        width: image.width,
        height: image.height,
        density,
        ir: image.ir.clone(),
    }
}

/// Stage 3 — apply a density curve `tone` (corrected density → positive
/// linear) to every sample, minting the typed [`FilmRgbImage`] boundary.
///
/// Pure and unclamped; a non-finite density (or a curve output that overflows) rides
/// through so `io::encode`'s counters surface it.
///
/// Consumes the `DensityImage` (a use-once intermediate): the density buffer is
/// transformed in place and the IR plane is moved, so no image-sized buffer is
/// allocated or cloned here.
pub(crate) fn apply_curve(density: DensityImage, tone: impl Fn(f32) -> f32 + Sync) -> FilmRgbImage {
    let mut rgb = density.density;
    rgb.par_chunks_exact_mut(3).for_each(|px| {
        for v in px.iter_mut() {
            *v = tone(*v);
        }
    });

    // Lengths are inherited unchanged from a `DensityImage` built from a validated
    // `LinearImage`, so the invariants hold by construction. Route through the
    // validated constructor anyway — its checks are O(1) (buffer lengths, not a
    // per-sample scan), so a future regression that breaks the invariant panics
    // loudly here instead of minting a silently-malformed image.
    FilmRgbImage::from_linear(
        LinearImage::new(density.width, density.height, rgb, density.ir)
            .expect("the curve preserves the validated buffer-length invariants"),
    )
}

/// Reject a film base that would make the density conversion ill-defined: each
/// per-channel value is a transmission in `(0, 1]`. Non-positive / non-finite
/// values would divide into inf/NaN; values above `1.0` are impossible for a
/// `[0, 1]`-normalized scan (a typo like `--film-base 90` for `0.90`) and would
/// silently render every real sample above white.
pub(crate) fn check_base(base: &FilmBase) -> Result<()> {
    for (name, v) in [("r", base.r), ("g", base.g), ("b", base.b)] {
        if !v.is_finite() || v <= 0.0 || v > 1.0 {
            return Err(NcError::Other(format!(
                "film base {name} channel must be a transmission in (0, 1] (got {v}); \
                 measure a valid Dmin or pass an explicit --film-base"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct as reconstruct_config;
    use crate::types::{AnchorPlacement, ExponentialParams, Reconstruction};

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    /// A 1×1 RGB image (optionally with a 1-sample IR plane) for pixel-math tests.
    fn pixel(rgb: [f32; 3], ir: Option<f32>) -> LinearImage {
        LinearImage::new(1, 1, rgb.to_vec(), ir.map(|v| vec![v])).unwrap()
    }

    /// The exponential curve carrying `gamma`, with mid-grey `offset` above the base.
    fn exponential(gamma: f32, offset: f32) -> ExponentialParams {
        ExponentialParams {
            gamma,
            anchor: AnchorPlacement::MidAtBaseOffset(offset),
        }
    }

    /// The result of a full density-path reconstruction.
    #[derive(Debug)]
    struct Converted {
        out: LinearImage,
        curve_anchor: f32,
    }

    /// Run the full density path through the public entry point, `reconstruct`
    /// (stages 1–3).
    fn run(
        img: &LinearImage,
        base: &FilmBase,
        density: DensityParams,
        curve: ExponentialParams,
    ) -> Result<Converted> {
        let config = Reconstruction { density, curve };
        let (film, rep) = reconstruct_config(img, base, &config)?;
        Ok(Converted {
            out: film.into_linear(),
            curve_anchor: rep.curve_anchor,
        })
    }

    /// The anchored exponential curve on a prepared density buffer (stage 3) — the
    /// same composition `reconstruct`'s exponential arm performs.
    fn render(density: DensityImage, gamma: f32, anchor: f32) -> LinearImage {
        apply_curve(density, move |d| 10f32.powf(gamma * (d - anchor))).into_linear()
    }

    // --- stage 1–2: to_density -------------------------------------------------

    /// `DensityParams` with an **identity** per-channel gain, for tests about the
    /// density transform itself rather than about the shipped default.
    ///
    /// The default gain is `[1, 0.84, 0.73]` — a scanner calibration, not part of the
    /// `D = −log10(scan / base)` definition — so a test asserting that definition, or
    /// asserting that equal base fractions give equal densities, has to state the
    /// identity or it is asserting the calibration instead.
    fn identity_gain() -> DensityParams {
        DensityParams {
            scale: [1.0, 1.0, 1.0],
            ..DensityParams::default()
        }
    }

    #[test]
    fn to_density_computes_neg_log10_ratio() {
        // base = 1 makes D = -log10(scan): 0.1 → 1, 0.01 → 2, 1.0 → 0.
        let img = pixel([0.1, 0.01, 1.0], None);
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let d = to_density(&img, &base, &identity_gain());
        assert!(approx(d.density[0], 1.0, 1e-5));
        assert!(approx(d.density[1], 2.0, 1e-5));
        assert!(approx(d.density[2], 0.0, 1e-5));
    }

    #[test]
    fn to_density_is_relative_to_per_channel_base() {
        // A neutral patch = the same fraction of each channel's base → equal density
        // across channels (this is the orange-mask removal). base is deliberately
        // orange (r>g>b); scan is 1/2 of base per channel.
        let base = FilmBase::from([0.5, 0.25, 0.15]);
        let img = pixel([0.25, 0.125, 0.075], None);
        let d = to_density(&img, &base, &identity_gain());
        let expected = -(0.5f32).log10(); // ≈ 0.30103
        for c in 0..3 {
            assert!(approx(d.density[c], expected, 1e-5), "channel {c}");
        }
    }

    #[test]
    fn to_density_applies_scale_then_offset() {
        // D = -log10(0.1/1) = 1; D' = scale·1 + offset.
        let img = pixel([0.1, 0.1, 0.1], None);
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let params = DensityParams {
            scale: [2.0, 1.0, 0.5],
            offset: [0.5, -0.25, 0.0],
        };
        let d = to_density(&img, &base, &params);
        assert!(approx(d.density[0], 2.5, 1e-5));
        assert!(approx(d.density[1], 0.75, 1e-5));
        assert!(approx(d.density[2], 0.5, 1e-5));
    }

    #[test]
    fn to_density_epsilon_clamp_keeps_zero_and_negative_finite() {
        // Zero / negative transmission (dead or noisy sample) must not become
        // -inf / NaN — the epsilon floor yields a high but finite density.
        let img = pixel([0.0, -5.0, f32::MIN_POSITIVE], None);
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let d = to_density(&img, &base, &identity_gain());
        for c in 0..3 {
            assert!(d.density[c].is_finite(), "channel {c} not finite");
        }
        // scan==0 and scan<0 both floor to the same SCAN_EPSILON-derived density.
        let expected = -(SCAN_EPSILON).log10();
        assert!(approx(d.density[0], expected, 1e-4));
        assert!(approx(d.density[1], expected, 1e-4));
    }

    #[test]
    fn to_density_carries_ir_untouched() {
        let img = pixel([0.2, 0.2, 0.2], Some(0.42));
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let d = to_density(&img, &base, &DensityParams::default());
        assert_eq!(d.ir.as_deref(), Some(&[0.42_f32][..]));
    }

    // --- stage 3–4: curve + print render ----------------------------------------

    #[test]
    fn render_maps_density_through_ten_to_the_power() {
        // Neutral print params, gamma 1: lin = 10^D'. D'=[1,0,2] → [10,1,100].
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![1.0, 0.0, 2.0],
            ir: None,
        };
        let out = render(d, 1.0, 0.0);
        assert!(approx(out.rgb[0], 10.0, 1e-3));
        assert!(approx(out.rgb[1], 1.0, 1e-5));
        assert!(approx(out.rgb[2], 100.0, 1e-2));
    }

    #[test]
    fn render_gamma_scales_the_density_exponent() {
        // lin = 10^(gamma·D'); D'=1, gamma=0.5 → 10^0.5 ≈ 3.1623.
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![1.0, 1.0, 1.0],
            ir: None,
        };
        let out = render(d, 0.5, 0.0);
        for c in 0..3 {
            assert!(approx(out.rgb[c], 10f32.powf(0.5), 1e-3), "channel {c}");
        }
    }

    #[test]
    fn render_carries_ir_untouched() {
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![0.3, 0.3, 0.3],
            ir: Some(vec![0.7]),
        };
        let out = render(d, 1.0, 0.0);
        assert_eq!(out.ir.as_deref(), Some(&[0.7_f32][..]));
    }

    // --- reconstruction: composition + polarity ---------------------------------

    // Wiring test: confirms the full path = `to_density` then the anchored
    // exponential curve, with the right gamma threaded through (catches a
    // dropped/wrong gamma or a swapped stage).
    #[test]
    fn full_path_equals_render_of_to_density() {
        let img = pixel([0.3, 0.15, 0.08], Some(0.5));
        let base = FilmBase::from([0.6, 0.3, 0.18]);
        let density = DensityParams {
            scale: [1.1, 1.0, 0.9],
            offset: [0.05, 0.0, -0.05],
        };
        let gamma = 1.4;
        let curve = exponential(gamma, 0.5);
        let via_config = run(&img, &base, density.clone(), curve).unwrap();
        let dimg = to_density(&img, &base, &density);
        let anchor = curve.anchor.anchor(gamma);
        let via_parts = render(dimg, gamma, anchor);
        assert_eq!(via_config.out.rgb, via_parts.rgb);
        assert_eq!(via_config.out.ir, via_parts.ir);
    }

    #[test]
    fn convert_is_positive_polarity_denser_is_brighter() {
        // Two pixels sharing a base: pixel 0 is thinner (near base → scene shadow),
        // pixel 1 is denser (lower transmission → scene highlight). A correct
        // positive renders the denser negative *brighter*. This is the guard that
        // pins the sign fix — a `10^(-γD')` regression would flip it.
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = LinearImage::new(2, 1, vec![0.55, 0.55, 0.55, 0.05, 0.05, 0.05], None).unwrap();
        let out = run(
            &img,
            &base,
            DensityParams::default(),
            ExponentialParams::default(),
        )
        .unwrap()
        .out;
        for c in 0..3 {
            assert!(
                out.rgb[3 + c] > out.rgb[c],
                "denser pixel should be brighter (channel {c}): \
                 thin={} dense={}",
                out.rgb[c],
                out.rgb[3 + c]
            );
        }
    }

    #[test]
    fn convert_neutral_patch_stays_neutral() {
        // **What "neutral in" means is the whole content of this test, and the default
        // gain changed it.** Equal fractions of each base channel are neutral only if a
        // neutral *scene* produces equal densities — which it does not: each layer has
        // its own slope, so a real neutral exposes them apart. Both halves are asserted
        // because a regression in either is a colour bug that no other test sees.
        let base = FilmBase::from([0.5, 0.25, 0.15]);
        let neutral_out = |img, params| {
            run(&img, &base, params, ExponentialParams::default())
                .unwrap()
                .out
        };

        // (a) Under the identity gain, equal base fractions still reconstruct neutral —
        // the structural orange-mask removal in `to_density`, unchanged.
        let out = neutral_out(pixel([0.2, 0.1, 0.06], None), identity_gain()); // 0.4 × base
        assert!(approx(out.rgb[0], out.rgb[1], 1e-4));
        assert!(approx(out.rgb[1], out.rgb[2], 1e-4));

        // (b) Under the shipped gain, a patch carrying the **measured** channel slope
        // ratios reconstructs *closer to* neutral. `algo::curve_probe::sigmoid_scale`
        // measured those ratios at green 1.115, blue 1.183 against red on its six-roll,
        // 21-frame corpus (three rolls have since left the asset folder, so a re-run
        // differs) — so build the patch from the ratios and assert the render improves it.
        //
        // **The patch is deliberately not rebuilt from the shipping gain, and the claim
        // is weaker since `pipeline_version` 5.** Two calibrations of this gain exist and
        // they disagree: the tone-scale slope over 21 frames nulls at `[1, 0.897, 0.845]`
        // (v4 shipped `[1, 0.90, 0.86]` from it and cancelled this patch ~4x), while the
        // 31 hand-marked neutral patches over five rolls null at `[1, 0.837, 0.733]`
        // (v5 ships `[1, 0.84, 0.73]`). On *this* patch — built from the slope corpus —
        // v5 leaves 0.1286 spread against 0.1791 uncorrected, a 1.39x improvement rather
        // than v4's 4x, because it corrects past what the slope measurement asked for.
        //
        // Rebuilding the patch from the neutral-patch ratios would make this assert that
        // the default nulls the data it was derived from, which is circular and would
        // delete the disagreement. Keeping it records that the two measurements do not
        // agree — the residual `io/scanner-density-calibration` owns — and still catches
        // a gain that stops correcting the measured slope at all.
        let (d_r, r_g, r_b) = (0.4f32, 1.115f32, 1.183f32);
        let transmission = |d: f32, b: f32| b * 10f32.powf(-d);
        let img = pixel(
            [
                transmission(d_r, 0.5),
                transmission(d_r * r_g, 0.25),
                transmission(d_r * r_b, 0.15),
            ],
            None,
        );
        // Asserted as an improvement *ratio* rather than an absolute tolerance: the
        // sigmoid's toe and shoulder are non-linear, so they amplify whatever density
        // residual survives by a local slope that is not the nominal contrast. The claim
        // the default makes is comparative — this patch is what it is calibrated for —
        // so compare it against the same patch under the identity gain.
        let spread = |out: &crate::types::LinearImage| {
            let m = (out.rgb[0] + out.rgb[1] + out.rgb[2]) / 3.0;
            (0..3)
                .map(|c| (out.rgb[c] - m).abs() / m)
                .fold(0.0f32, f32::max)
        };
        let corrected = spread(&neutral_out(img.clone(), DensityParams::default()));
        let uncorrected = spread(&neutral_out(img, identity_gain()));
        assert!(
            corrected < uncorrected / 1.25,
            "the default gain left {corrected:.4} spread on a scene-neutral patch built from \
             the measured slope ratios, against {uncorrected:.4} uncorrected — it must still \
             improve that patch, whatever else it is calibrated against"
        );
        assert!(
            corrected < 0.20,
            "residual spread {corrected:.4} is larger than either calibration predicts"
        );
    }

    #[test]
    fn to_density_propagates_non_finite_scan() {
        // NaN / +inf transmission must NOT be laundered to a finite density by the
        // epsilon floor — they propagate as NaN so io::encode's non-finite counter
        // surfaces corrupt input. A finite channel alongside them is unaffected.
        let img = pixel([f32::NAN, f32::INFINITY, 0.2], None);
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let d = to_density(&img, &base, &DensityParams::default());
        assert!(d.density[0].is_nan(), "NaN scan → NaN density");
        assert!(d.density[1].is_nan(), "+inf scan → NaN density");
        assert!(d.density[2].is_finite(), "finite scan stays finite");
    }

    #[test]
    fn convert_default_output_is_finite_no_blowup() {
        // "No channel blow-outs": a normal pixel under default params yields
        // finite, bounded output (not NaN/inf).
        let base = FilmBase::from([0.55, 0.27, 0.16]);
        let img = pixel([0.39, 0.19, 0.09], None);
        let out = run(
            &img,
            &base,
            DensityParams::default(),
            ExponentialParams::default(),
        )
        .unwrap()
        .out;
        for c in 0..3 {
            assert!(out.rgb[c].is_finite(), "channel {c} not finite");
            assert!(out.rgb[c] < 1000.0, "channel {c} blew up: {}", out.rgb[c]);
        }
    }

    #[test]
    fn convert_rejects_non_positive_or_non_finite_base() {
        let img = pixel([0.2, 0.2, 0.2], None);
        for bad in [
            [1.0, 0.0, 1.0],      // zero channel → division by zero
            [1.0, -0.5, 1.0],     // negative transmission
            [f32::NAN, 1.0, 1.0], // non-finite
            [1.0, f32::INFINITY, 1.0],
            [1.0, 90.0, 1.0], // transmission > 1 (e.g. "90" typo for "0.90")
        ] {
            let err = run(
                &img,
                &FilmBase::from(bad),
                DensityParams::default(),
                ExponentialParams::default(),
            )
            .unwrap_err();
            assert_eq!(err.exit_code(), 1, "base {bad:?} should fail loudly");
        }
        // A valid base still converts.
        assert!(
            run(
                &img,
                &FilmBase::from([0.5, 0.5, 0.5]),
                DensityParams::default(),
                ExponentialParams::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn anchored_exponent_survives_extreme_gamma_and_anchor() {
        // Regression (PR review): the anchor used to be a separate 10^(−γ·A)
        // gain, so γ·D' alone could overflow f32 before the gain cancelled it
        // (γ = 5, D' = 8 ⇒ 10^40 = inf ⇒ white rendered inf/NaN). With the
        // anchored exponent, D' = A maps to exactly 1.0 regardless of scale.
        let gamma = 5.0f32;
        let anchor = 8.0f32;
        let dimg = DensityImage {
            width: 1,
            height: 1,
            density: vec![anchor, anchor, anchor],
            ir: None,
        };
        let out = render(dimg, gamma, anchor);
        for v in &out.rgb {
            assert!(v.is_finite(), "overflowed: {v}");
            assert!(approx(*v, 1.0, 1e-5), "scene white should be 1.0, got {v}");
        }
    }

    #[test]
    fn convert_preserves_ir_plane() {
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let img = pixel([0.2, 0.2, 0.2], Some(0.33));
        let out = run(
            &img,
            &base,
            DensityParams::default(),
            ExponentialParams::default(),
        )
        .unwrap()
        .out;
        assert_eq!(out.ir.as_deref(), Some(&[0.33_f32][..]));
    }

    // --- the anchor ------------------------------------------------------------

    #[test]
    fn the_anchor_density_maps_to_display_white() {
        // The pixel at `D' = A` renders to exactly 1.0, and the base (`D' = 0`) to
        // `10^(−γ·A) < 1` (near black).
        let anchor = 1.5f32;
        let gamma = 2.0f32;
        let dimg = DensityImage {
            width: 2,
            height: 1,
            density: vec![anchor, anchor, anchor, 0.0, 0.0, 0.0],
            ir: None,
        };
        let out = render(dimg, gamma, anchor);
        for c in 0..3 {
            assert!(approx(out.rgb[c], 1.0, 1e-5), "anchor → 1.0 (chan {c})");
            assert!(approx(out.rgb[3 + c], 10f32.powf(-gamma * anchor), 1e-6));
            assert!(out.rgb[3 + c] < 1.0, "base below white (chan {c})");
        }
    }

    #[test]
    fn reconstruct_surfaces_the_resolved_anchor() {
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = pixel([0.2, 0.2, 0.2], None);

        // The curve reports the anchor its placement derived.
        let curve = exponential(1.0, 0.5);
        let rep = run(&img, &base, DensityParams::default(), curve).unwrap();
        assert_eq!(rep.curve_anchor, curve.anchor.anchor(1.0));
    }
}
