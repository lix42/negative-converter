//! `density` — density-domain inversion (Cineon / negadoctor style). Default.
//!
//! Density conversion and print rendering are **separate** sub-stages (core
//! fidelity rule from design-spec §3/§7.2): [`to_density`] owns the density-domain
//! conversion (transmission → corrected density), [`render`] owns the print /
//! tone render (density → positive linear). [`Density::convert`] only composes
//! them, and each is a pure, independently-testable function.
//!
//! ## Model (per channel `c`)
//!
//! ```text
//! 1. transmission → density:   D_c  = -log10(max(scan_c, EPS) / base_c)
//! 2. density correction:       D'_c = density_scale_c · D_c + density_offset_c
//! 3. density → positive:       lin_c = 10^(density_gamma · D'_c)
//! 4. print render:             lin_c = white_balance_c · 2^print_exposure · lin_c
//!                                      − black_point, then highlight soft-clip
//! ```
//!
//! Stages 1–2 are [`to_density`]; 3–4 are [`render`].
//!
//! **Polarity (deliberate correction to the task-file sketch).** With
//! `D = -log10(scan/base)` the density is `≥ 0` and *grows* with the film's
//! optical density: the unexposed base (scene black) sits at `D = 0`, and a dense
//! negative area (a scene highlight) has a large `D`. A true positive must get
//! *brighter* as `D` grows, so stage 3 uses `10^(+γ·D')`. The task-file sketch
//! wrote `10^(-γ·D')`; taken literally with this `D` that yields `scan/base` — the
//! original *negative* — so the sign is flipped here on purpose. This matches
//! darktable `negadoctor`, whose print output increases with film density
//! (verified against its source: denser negative → brighter print).
//!
//! Output is linear and **scene-referred**: with neutral defaults the base maps to
//! `1.0` and exposed detail sits above it (HDR), consistent with the project's
//! "don't clamp before encode" rule. Fit to a display range with a negative
//! `--print-exposure` and/or `--black-point`, or keep it via `--out-depth f32`.

use rayon::prelude::*;

use crate::algo::Converter;
use crate::types::{DensityParams, FilmBase, LinearImage, NcError, PrintParams, Result};

/// Floor applied to the scan transmission before the `log10`, so a zero / negative
/// / denormal sample can't produce `-inf`/`NaN` density (design "fail loudly, never
/// a quietly wrong image" — a dead pixel becomes a very high but finite density
/// rather than poisoning the channel). `1e-6` ≈ −20 stops below unity: darker than
/// any real detail, yet leaves ample headroom before `10^(γ·D)` can overflow `f32`.
const SCAN_EPSILON: f32 = 1e-6;

/// Corrected per-pixel film density `D'` (interleaved RGB), the boundary between
/// the two sub-stages: the output of [`to_density`] (stages 1–2) and the input to
/// [`render`] (stages 3–4). The IR plane is carried through untouched (Step-1 rule:
/// preserve, don't consume).
///
/// Algo-internal (`pub(crate)`), not a cross-stage contract type — the neutral
/// contract lives in `types.rs`. It has no validated constructor; its length
/// invariants (`density.len() == w*h*3`, `ir.len() == w*h`) hold by construction
/// because [`to_density`], its only producer, derives them from a validated
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

/// Density-domain converter.
///
/// Holds **both** sub-stages' params: `density` (transmission→density correction)
/// and `print` (the separate print-render controls). Keeping them as distinct
/// fields preserves the core fidelity rule — the two are parameterized
/// independently even though one algorithm owns them.
pub struct Density {
    pub density: DensityParams,
    pub print: PrintParams,
}

/// Stages 1–2 — transmission → corrected density (pure).
///
/// `D_c = -log10(max(scan_c, EPS) / base_c)` then `D'_c = scale_c·D_c + offset_c`.
/// Dividing by the *per-channel* base is what neutralizes the orange mask: at the
/// base every channel lands on `D = 0`, so an unexposed sample is neutral before
/// any correction; `density_offset` / `density_scale` then trim the per-channel
/// density balance and contrast.
///
/// `base` must be finite and `> 0` per channel; [`Density::convert`] enforces this
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
    let scale = params.density_scale;
    let offset = params.density_offset;

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

/// Stages 3–4 — corrected density → positive linear (pure print render).
///
/// - Stage 3 (density → positive): `lin_c = 10^(density_gamma · D'_c)`. Increases
///   with density (correct positive polarity; see the module note). `density_gamma`
///   is the film/print curve contrast; it lives in [`DensityParams`] but is applied
///   here at the density→linear boundary, so it is passed in explicitly rather than
///   the whole density-params struct.
/// - Stage 4 (print controls): per-channel highlight/neutral white balance, an
///   overall `2^print_exposure` gain (exposure is in **stops**), a `black_point`
///   floor subtraction, and a highlight soft-clip.
///
/// No clamping: values may land outside `[0, 1]` (scene-referred / HDR); the encode
/// stage counts and reports any clipping. The result preserves the carried IR plane.
///
/// Consumes the `DensityImage` (it is a use-once intermediate): the density buffer
/// is transformed into the output in place and the IR plane is moved, so no
/// image-sized buffer is allocated or cloned here.
pub(crate) fn render(
    density: DensityImage,
    density_gamma: f32,
    print: &PrintParams,
) -> LinearImage {
    let exposure_gain = 2f32.powf(print.print_exposure);
    let wb = print.white_balance;
    let black = print.black_point;
    let hc = print.highlight_compress;

    let mut rgb = density.density;
    rgb.par_chunks_exact_mut(3).for_each(|d| {
        for c in 0..3 {
            let paper = 10f32.powf(density_gamma * d[c]); // stage 3
            let exposed = paper * wb[c] * exposure_gain; // stage 4
            d[c] = soft_clip(exposed - black, hc);
        }
    });

    // Lengths are inherited unchanged from a `DensityImage` built from a validated
    // `LinearImage`, so the invariants hold by construction. Route through the
    // validated constructor anyway — its checks are O(1) (buffer lengths, not a
    // per-sample scan), so a future regression that breaks the invariant panics
    // loudly here instead of minting a silently-malformed image.
    LinearImage::new(density.width, density.height, rgb, density.ir)
        .expect("render preserves the validated buffer-length invariants")
}

/// Highlight soft-clip: a smooth roll-off of values above the nominal display
/// white (`1.0`). Below white the value passes through unchanged; above it the
/// excess is compressed with an exponential knee of width `amount`, so the output
/// asymptotes to `1.0 + amount` instead of shooting off. `amount <= 0` disables it
/// (the default — a plain identity), so highlights are preserved verbatim unless
/// the user asks for compression.
///
/// The `1.0` threshold is the nominal white anchor — the definition of "highlight"
/// — not a tunable hidden knob; `highlight_compress` is the exposed control.
///
/// Non-finite input (`NaN`/`±inf`, e.g. from an overflowed `10^(γD')`) passes
/// through unchanged so `io::encode`'s non-finite counter still surfaces it —
/// without the `is_finite` guard, `+inf` would roll off to a clean `1.0 + amount`
/// and silently hide the overflow (consistent with [`to_density`] propagating NaN).
fn soft_clip(x: f32, amount: f32) -> f32 {
    const WHITE: f32 = 1.0;
    if amount <= 0.0 || !x.is_finite() || x <= WHITE {
        return x;
    }
    WHITE + amount * (1.0 - (-(x - WHITE) / amount).exp())
}

impl Converter for Density {
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage> {
        // `to_density` divides by the per-channel base, so a zero / negative /
        // non-finite base would yield a silently-black or non-finite image. The CLI
        // validates an *explicit* base, but an auto/region-estimated one is only
        // guarded here — the base's consumption point. Fail loudly instead.
        check_base(base)?;
        let density = to_density(image, base, &self.density);
        Ok(render(density, self.density.density_gamma, &self.print))
    }
}

/// Reject a film base that would make the density conversion ill-defined: each
/// per-channel value is a transmission in `(0, 1]`. Non-positive / non-finite
/// values would divide into inf/NaN; values above `1.0` are impossible for a
/// `[0, 1]`-normalized scan (a typo like `--film-base 90` for `0.90`) and would
/// silently render every real sample above white.
fn check_base(base: &FilmBase) -> Result<()> {
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

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    /// A 1×1 RGB image (optionally with a 1-sample IR plane) for pixel-math tests.
    fn pixel(rgb: [f32; 3], ir: Option<f32>) -> LinearImage {
        LinearImage::new(1, 1, rgb.to_vec(), ir.map(|v| vec![v])).unwrap()
    }

    // --- stage 1–2: to_density -------------------------------------------------

    #[test]
    fn to_density_computes_neg_log10_ratio() {
        // base = 1 makes D = -log10(scan): 0.1 → 1, 0.01 → 2, 1.0 → 0.
        let img = pixel([0.1, 0.01, 1.0], None);
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let d = to_density(&img, &base, &DensityParams::default());
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
        let d = to_density(&img, &base, &DensityParams::default());
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
            density_scale: [2.0, 1.0, 0.5],
            density_offset: [0.5, -0.25, 0.0],
            density_gamma: 1.0,
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
        let d = to_density(&img, &base, &DensityParams::default());
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

    // --- stage 3–4: render -----------------------------------------------------

    #[test]
    fn render_maps_density_through_ten_to_the_power() {
        // Neutral print params, gamma 1: lin = 10^D'. D'=[1,0,2] → [10,1,100].
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![1.0, 0.0, 2.0],
            ir: None,
        };
        let out = render(d, 1.0, &PrintParams::default());
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
        let out = render(d, 0.5, &PrintParams::default());
        for c in 0..3 {
            assert!(approx(out.rgb[c], 10f32.powf(0.5), 1e-3), "channel {c}");
        }
    }

    #[test]
    fn render_applies_wb_exposure_then_black() {
        // D'=0 → paper=1. R: 1·wb(2)·2^exp(2) − black(0.5) = 4 − 0.5 = 3.5.
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![0.0, 0.0, 0.0],
            ir: None,
        };
        let print = PrintParams {
            print_exposure: 1.0, // 2^1 = 2
            black_point: 0.5,
            white_balance: [2.0, 1.0, 0.5],
            highlight_compress: 0.0,
        };
        let out = render(d, 1.0, &print);
        assert!(approx(out.rgb[0], 3.5, 1e-4)); // 1·2·2 − 0.5
        assert!(approx(out.rgb[1], 1.5, 1e-4)); // 1·1·2 − 0.5
        assert!(approx(out.rgb[2], 0.5, 1e-4)); // 1·0.5·2 − 0.5
    }

    #[test]
    fn render_carries_ir_untouched() {
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![0.3, 0.3, 0.3],
            ir: Some(vec![0.7]),
        };
        let out = render(d, 1.0, &PrintParams::default());
        assert_eq!(out.ir.as_deref(), Some(&[0.7_f32][..]));
    }

    // --- soft_clip -------------------------------------------------------------

    #[test]
    fn soft_clip_is_identity_when_disabled_or_below_white() {
        assert_eq!(soft_clip(5.0, 0.0), 5.0); // disabled
        assert_eq!(soft_clip(5.0, -1.0), 5.0); // disabled (non-positive)
        assert_eq!(soft_clip(0.5, 1.0), 0.5); // below white, untouched
        assert_eq!(soft_clip(1.0, 1.0), 1.0); // exactly white, untouched
    }

    #[test]
    fn soft_clip_rolls_off_and_bounds_highlights() {
        // Above white: compressed toward the 1.0 + amount asymptote, monotonically.
        let a = 0.5;
        let y2 = soft_clip(2.0, a);
        let y10 = soft_clip(10.0, a);
        assert!(y2 > 1.0 && y2 < 1.0 + a); // rolled off, below the asymptote
        assert!(y10 > y2); // still monotonic increasing
        assert!(y10 <= 1.0 + a); // bounded by 1 + amount (reached exactly in f32)
        // Small excess ≈ identity to first order (knee is smooth at white).
        assert!(approx(soft_clip(1.001, a), 1.001, 1e-4));
        // Non-finite passes through (not masked to 1+amount) so encode can count it.
        assert!(soft_clip(f32::INFINITY, a).is_infinite());
        assert!(soft_clip(f32::NAN, a).is_nan());
    }

    // --- Converter: composition + polarity ------------------------------------

    // Wiring test: confirms `convert` = `to_density` then `render` with the right
    // `density_gamma` threaded through (catches a dropped/wrong gamma or a swapped
    // stage). It does NOT independently verify `render`'s output values — both sides
    // share the same `render` — so `render`'s math is pinned by the `render_*` tests.
    #[test]
    fn convert_equals_render_of_to_density() {
        let img = pixel([0.3, 0.15, 0.08], Some(0.5));
        let base = FilmBase::from([0.6, 0.3, 0.18]);
        let conv = Density {
            density: DensityParams {
                density_scale: [1.1, 1.0, 0.9],
                density_offset: [0.05, 0.0, -0.05],
                density_gamma: 1.4,
            },
            print: PrintParams {
                print_exposure: -1.0,
                black_point: 0.01,
                white_balance: [1.0, 1.05, 1.1],
                highlight_compress: 0.2,
            },
        };
        let via_convert = conv.convert(&img, &base).unwrap();
        let via_parts = render(
            to_density(&img, &base, &conv.density),
            conv.density.density_gamma,
            &conv.print,
        );
        assert_eq!(via_convert.rgb, via_parts.rgb);
        assert_eq!(via_convert.ir, via_parts.ir);
    }

    #[test]
    fn convert_is_positive_polarity_denser_is_brighter() {
        // Two pixels sharing a base: pixel 0 is thinner (near base → scene shadow),
        // pixel 1 is denser (lower transmission → scene highlight). A correct
        // positive renders the denser negative *brighter*. This is the guard that
        // pins the sign fix — a `10^(-γD')` regression would flip it.
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = LinearImage::new(2, 1, vec![0.55, 0.55, 0.55, 0.05, 0.05, 0.05], None).unwrap();
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let out = conv.convert(&img, &base).unwrap();
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
        // Same fraction of each (orange) base channel → equal output channels,
        // with default params (orange-mask removal is structural in to_density).
        let base = FilmBase::from([0.5, 0.25, 0.15]);
        let img = pixel([0.2, 0.1, 0.06], None); // 0.4 × base per channel
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let out = conv.convert(&img, &base).unwrap();
        assert!(approx(out.rgb[0], out.rgb[1], 1e-4));
        assert!(approx(out.rgb[1], out.rgb[2], 1e-4));
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
    fn render_applies_highlight_soft_clip_above_white() {
        // Non-tautological render test of the soft-clip branch: exposed values
        // above/at/below the 1.0 white anchor. gamma=1, neutral print but hc=0.5.
        // R: 10^log10(2)=2.0 → soft_clip(2.0,0.5)=1+0.5(1−e^−2)≈1.4323
        // G: 10^0=1.0 (== white) → unchanged.  B: 10^−log10(2)=0.5 → unchanged.
        let d = DensityImage {
            width: 1,
            height: 1,
            density: vec![(2.0f32).log10(), 0.0, -(2.0f32).log10()],
            ir: None,
        };
        let print = PrintParams {
            highlight_compress: 0.5,
            ..PrintParams::default()
        };
        let out = render(d, 1.0, &print);
        let expected_r = 1.0 + 0.5 * (1.0 - (-2.0f32).exp());
        assert!(approx(out.rgb[0], expected_r, 1e-4), "got {}", out.rgb[0]);
        assert!(approx(out.rgb[1], 1.0, 1e-5));
        assert!(approx(out.rgb[2], 0.5, 1e-5));
    }

    #[test]
    fn convert_default_output_is_finite_no_blowup() {
        // Requirement 4's "no channel blow-outs": a normal pixel under default
        // params yields finite, bounded output (not NaN/inf).
        let base = FilmBase::from([0.55, 0.27, 0.16]);
        let img = pixel([0.39, 0.19, 0.09], None);
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let out = conv.convert(&img, &base).unwrap();
        for c in 0..3 {
            assert!(out.rgb[c].is_finite(), "channel {c} not finite");
            assert!(out.rgb[c] < 1000.0, "channel {c} blew up: {}", out.rgb[c]);
        }
    }

    #[test]
    fn convert_rejects_non_positive_or_non_finite_base() {
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let img = pixel([0.2, 0.2, 0.2], None);
        for bad in [
            [1.0, 0.0, 1.0],      // zero channel → division by zero
            [1.0, -0.5, 1.0],     // negative transmission
            [f32::NAN, 1.0, 1.0], // non-finite
            [1.0, f32::INFINITY, 1.0],
            [1.0, 90.0, 1.0], // transmission > 1 (e.g. "90" typo for "0.90")
        ] {
            let err = conv.convert(&img, &FilmBase::from(bad)).unwrap_err();
            assert_eq!(err.exit_code(), 1, "base {bad:?} should fail loudly");
        }
        // A valid base still converts.
        assert!(conv.convert(&img, &FilmBase::from([0.5, 0.5, 0.5])).is_ok());
    }

    #[test]
    fn convert_preserves_ir_plane() {
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let img = pixel([0.2, 0.2, 0.2], Some(0.33));
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let out = conv.convert(&img, &base).unwrap();
        assert_eq!(out.ir.as_deref(), Some(&[0.33_f32][..]));
    }
}
