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
//! 3. density → positive:       lin_c = 10^(density_gamma · (D'_c − Dmax))
//! 4. print render:             lin_c = white_balance_c · 2^print_exposure · lin_c
//!                                      − black_point, then highlight soft-clip
//! ```
//!
//! Stages 1–2 are [`to_density`]; 3–4 are [`render`], which composes this
//! algorithm's stage-3 curve with the shared stage-4 print render
//! [`render_print`] (also used by `sigmoid`, which swaps in an S-curve stage 3).
//!
//! **Display-white anchor (`Dmax`).** Stage 3 renders density *relative to* the
//! scene-white density `Dmax`: scene white (`D' = Dmax`) maps to `1.0` and the base
//! (`D' = 0`) to `10^(−γ·Dmax) ≈ 0`, so the default u16 encode fills the display
//! range instead of leaving every real sample above `1.0`. `10^(γ·(D'−Dmax))`
//! factors into today's `10^(γ·D')` times a constant gain `10^(−γ·Dmax)`, so the
//! anchor composes with `print_exposure` as one multiplicative scalar (both fold
//! into the stage-4 exposure gain). The anchor source is [`DmaxSource`]: `Auto`
//! (default) measures it per frame from the corrected-density distribution;
//! `Explicit` fixes it; `None` disables it (gain `1.0`) and reproduces the
//! scene-referred output bit-for-bit. `Dmax` is **frame-local** (scene white),
//! unlike the roll-level `Dmin` base.
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
//! Output is linear. With the default `Auto` anchor scene white lands at ≈ `1.0`
//! (display-range-filling); with `--no-d-max` the base maps to `1.0` and exposed
//! detail sits above it (HDR / **scene-referred**), consistent with the project's
//! "don't clamp before encode" rule. Nothing is clamped here either way — the
//! encode stage counts and reports any out-of-range samples. Keep the full HDR
//! range with `--output-hdr` (typically alongside `--no-d-max`).

use rayon::prelude::*;

use crate::algo::{ConvertReport, Converter};
use crate::types::{
    DensityParams, DmaxSource, FilmBase, LinearImage, NcError, PrintParams, Result,
};

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
/// - Stage 3 (density → positive): `lin_c = 10^(density_gamma · (D'_c − Dmax))`.
///   Increases with density (correct positive polarity; see the module note).
///   `density_gamma` is the film/print curve contrast and `dmax` the display-white
///   anchor; both live in [`DensityParams`] but are applied here at the
///   density→linear boundary, so they are passed in explicitly rather than the
///   whole density-params struct.
/// - Stage 4 (print controls): per-channel highlight/neutral white balance, an
///   overall `2^print_exposure` gain (exposure is in **stops**), a `black_point`
///   floor subtraction, and a highlight soft-clip.
///
/// The anchor is applied **in the exponent** — `10^(γ·(D' − Dmax))` — not as a
/// separate `10^(−γ·Dmax)` gain: mathematically equivalent, but the factored form
/// overflows `f32` when `γ·D'` alone exceeds the pow10 range even though the
/// anchored exponent is small (e.g. `γ = 5`, EPS-clamped `D' ≈ 8`), turning scene
/// white into `inf` instead of `1.0`. With [`DmaxSource::None`] the anchor term is
/// exactly `0.0`, so this reproduces the pre-anchor render bit-for-bit
/// (`d − 0.0 == d` for every `f32`).
///
/// Returns the rendered image and the **resolved anchor density** — `Some(Dmax)`
/// for `Auto`/`Explicit`, `None` for `DmaxSource::None` — so the orchestrator can
/// report it (it does not clamp; values may land outside `[0, 1]`).
///
/// Consumes the `DensityImage` (it is a use-once intermediate): the density buffer
/// is transformed into the output in place and the IR plane is moved, so no
/// image-sized buffer is allocated or cloned here.
pub(crate) fn render(
    density: DensityImage,
    density_gamma: f32,
    dmax: DmaxSource,
    print: &PrintParams,
) -> (LinearImage, Option<f32>) {
    // Resolve the anchor from the corrected-density buffer *before* the in-place
    // transform overwrites it. The anchor is subtracted in the exponent (see the
    // doc above); `None` ⇒ anchor 0.0 ⇒ `d − 0.0 == d` bit-exactly, so the
    // per-pixel arithmetic below is bit-identical to the pre-anchor render.
    let resolved = resolve_dmax(&density.density, dmax);
    let anchor = resolved.unwrap_or(0.0);
    let image = render_print(
        density,
        // stage 3, anchored
        |d| 10f32.powf(density_gamma * (d - anchor)),
        print,
    );
    (image, resolved)
}

/// Stage 4 — the print render, shared by every density-domain algorithm
/// (`density` and `sigmoid`), fused with a caller-supplied stage-3 `tone` map
/// (corrected density → positive linear) so the buffer is traversed once.
///
/// The fusion is mechanical, not conceptual: `tone` *is* stage 3 (the algorithm's
/// curve), this function owns only stage 4 — per-channel highlight/neutral white
/// balance, an overall `2^print_exposure` gain (exposure in **stops**), the
/// `black_point` floor subtraction, and the highlight soft-clip — so the two
/// sub-stages stay separately parameterized (the core fidelity rule).
///
/// Consumes the `DensityImage` (a use-once intermediate): the density buffer is
/// transformed into the output in place and the IR plane is moved, so no
/// image-sized buffer is allocated or cloned here.
pub(crate) fn render_print(
    density: DensityImage,
    tone: impl Fn(f32) -> f32 + Sync,
    print: &PrintParams,
) -> LinearImage {
    let exposure_gain = 2f32.powf(print.print_exposure);
    let wb = print.white_balance;
    let black = print.black_point;
    let hc = print.highlight_compress;

    let mut rgb = density.density;
    rgb.par_chunks_exact_mut(3).for_each(|d| {
        for c in 0..3 {
            let paper = tone(d[c]); // stage 3 (the algorithm's curve)
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

/// Percentile of the corrected-density distribution taken as the `Auto` anchor.
/// High enough to sit at genuine scene white while ignoring the top fraction of a
/// percent (specular sparkle, dust, hot pixels) that would otherwise anchor white
/// too bright and leave the image dim. Mirrors the robustness intent of
/// `film_base`'s sampling percentile, applied to the density (not transmission)
/// distribution.
const AUTO_DMAX_PERCENTILE: f32 = 0.995;

/// Resolve the display-white anchor density for a corrected-density buffer.
/// `Auto` measures a high percentile of the *finite* densities (scalar, pooled
/// across channels — a per-channel anchor would double as color correction, which
/// is the future auto-WB task's job); `Explicit` returns the given value; `None`
/// yields no anchor. Deterministic: same buffer + params ⇒ same value.
/// `pub(crate)` because `sigmoid` anchors its S-curve on the same resolved `Dmax`
/// rather than inventing a second measurement.
pub(crate) fn resolve_dmax(densities: &[f32], source: DmaxSource) -> Option<f32> {
    match source {
        DmaxSource::None => None,
        DmaxSource::Explicit(d) => Some(d),
        DmaxSource::Auto => Some(auto_dmax(densities)),
    }
}

/// Cap on how many density samples the `Auto` anchor examines. A 99.5th
/// percentile over ~1M samples is statistically indistinguishable from the full
/// population for anchoring purposes, and the cap bounds the measuring pass to a
/// ~4 MB transient buffer instead of a second image-sized allocation on large
/// scans (the render itself is in-place).
const AUTO_DMAX_MAX_SAMPLES: usize = 1 << 20;

/// Deterministic sampling stride for a density buffer of `len` samples: the
/// smallest stride that keeps the sample count under [`AUTO_DMAX_MAX_SAMPLES`],
/// bumped off multiples of 3 — the buffer is interleaved RGB, so a stride
/// divisible by 3 would sample a single channel and bias the pooled percentile.
fn auto_dmax_stride(len: usize) -> usize {
    let stride = len.div_ceil(AUTO_DMAX_MAX_SAMPLES).max(1);
    if stride > 1 && stride.is_multiple_of(3) {
        stride + 1
    } else {
        stride
    }
}

/// The [`AUTO_DMAX_PERCENTILE`] of the finite corrected densities, by nearest-rank
/// over a deterministic strided sample (see [`auto_dmax_stride`]).
///
/// Non-finite densities (`NaN` from corrupt/overflowed input) are excluded rather
/// than ranked, so a stray non-finite pixel can't become the anchor. An empty /
/// all-non-finite buffer yields `0.0` — a neutral anchor rather than a panic; the
/// encoder's non-finite counter still surfaces the underlying fault.
/// Uses `select_nth_unstable` (O(n)) — the returned order-statistic value is
/// independent of tie ordering — and a fixed stride derived only from the buffer
/// length, so the result stays deterministic: same buffer ⇒ same anchor.
fn auto_dmax(densities: &[f32]) -> f32 {
    let stride = auto_dmax_stride(densities.len());
    let mut finite: Vec<f32> = Vec::with_capacity(densities.len().div_ceil(stride));
    finite.extend(
        densities
            .iter()
            .step_by(stride)
            .copied()
            .filter(|v| v.is_finite()),
    );
    if finite.is_empty() {
        return 0.0;
    }
    let rank = ((finite.len() - 1) as f32 * AUTO_DMAX_PERCENTILE).round() as usize;
    let (_, nth, _) = finite.select_nth_unstable_by(rank, f32::total_cmp);
    *nth
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
        Ok(self.convert_reported(image, base)?.0)
    }

    fn convert_reported(
        &self,
        image: &LinearImage,
        base: &FilmBase,
    ) -> Result<(LinearImage, ConvertReport)> {
        // `to_density` divides by the per-channel base, so a zero / negative /
        // non-finite base would yield a silently-black or non-finite image. The CLI
        // validates an *explicit* base, but an auto/region-estimated one is only
        // guarded here — the base's consumption point. Fail loudly instead.
        check_base(base)?;
        let density = to_density(image, base, &self.density);
        let (image, dmax) = render(
            density,
            self.density.density_gamma,
            self.density.dmax,
            &self.print,
        );
        Ok((image, ConvertReport { dmax }))
    }
}

/// Reject a film base that would make the density conversion ill-defined: each
/// per-channel value is a transmission in `(0, 1]`. Non-positive / non-finite
/// values would divide into inf/NaN; values above `1.0` are impossible for a
/// `[0, 1]`-normalized scan (a typo like `--film-base 90` for `0.90`) and would
/// silently render every real sample above white. Shared with `sigmoid`, whose
/// [`to_density`] call has the same precondition.
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
            dmax: DmaxSource::None, // unused by to_density
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
        let (out, _) = render(d, 1.0, DmaxSource::None, &PrintParams::default());
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
        let (out, _) = render(d, 0.5, DmaxSource::None, &PrintParams::default());
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
        let (out, _) = render(d, 1.0, DmaxSource::None, &print);
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
        let (out, _) = render(d, 1.0, DmaxSource::None, &PrintParams::default());
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
                dmax: DmaxSource::Auto,
            },
            print: PrintParams {
                print_exposure: -1.0,
                black_point: 0.01,
                white_balance: [1.0, 1.05, 1.1],
                highlight_compress: 0.2,
            },
        };
        let via_convert = conv.convert(&img, &base).unwrap();
        let (via_parts, _) = render(
            to_density(&img, &base, &conv.density),
            conv.density.density_gamma,
            conv.density.dmax,
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
        let (out, _) = render(d, 1.0, DmaxSource::None, &print);
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
    fn anchored_exponent_survives_extreme_gamma_and_dmax() {
        // Regression (PR review): the anchor used to be a separate 10^(−γ·Dmax)
        // gain, so γ·D' alone could overflow f32 before the gain cancelled it
        // (γ = 5, D' = 8 ⇒ 10^40 = inf ⇒ scene white rendered inf/NaN). With the
        // anchored exponent, D' = Dmax maps to exactly 1.0 regardless of scale.
        let gamma = 5.0f32;
        let dmax = 8.0f32;
        let dimg = DensityImage {
            width: 1,
            height: 1,
            density: vec![dmax, dmax, dmax],
            ir: None,
        };
        let (out, resolved) = render(
            dimg,
            gamma,
            DmaxSource::Explicit(dmax),
            &PrintParams::default(),
        );
        assert_eq!(resolved, Some(dmax));
        for v in &out.rgb {
            assert!(v.is_finite(), "overflowed: {v}");
            assert!(approx(*v, 1.0, 1e-5), "scene white should be 1.0, got {v}");
        }
    }

    #[test]
    fn auto_dmax_stride_is_bounded_and_channel_unbiased() {
        // Small buffers are sampled exhaustively.
        assert_eq!(auto_dmax_stride(0), 1);
        assert_eq!(auto_dmax_stride(3 * 100), 1);
        assert_eq!(auto_dmax_stride(AUTO_DMAX_MAX_SAMPLES), 1);
        // Large buffers are strided to stay under the cap...
        let big = 10 * AUTO_DMAX_MAX_SAMPLES;
        let stride = auto_dmax_stride(big);
        assert!(big.div_ceil(stride) <= AUTO_DMAX_MAX_SAMPLES + 1);
        // ...and the stride is never a multiple of 3 (interleaved RGB — a
        // 3-divisible stride would sample one channel only).
        for len in [
            big,
            3 * AUTO_DMAX_MAX_SAMPLES,
            6 * AUTO_DMAX_MAX_SAMPLES + 5,
        ] {
            let s = auto_dmax_stride(len);
            assert!(
                s == 1 || !s.is_multiple_of(3),
                "len {len}: stride {s} is 3-divisible"
            );
        }
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

    // --- Dmax white anchor -----------------------------------------------------

    #[test]
    fn none_anchor_is_bit_exact_with_pre_anchor_render() {
        // `DmaxSource::None` must reproduce the pre-anchor render bit-for-bit: the
        // anchor term is exactly 0.0 and `d − 0.0 == d` for every f32, so every
        // output sample must equal the direct pre-anchor arithmetic to the bit
        // (HDR f32 workflows depend on this). Uses `assert_eq!`, not an epsilon.
        let density = vec![0.7f32, -0.3, 1.2, 0.0, 2.0, -1.1];
        let dimg = DensityImage {
            width: 2,
            height: 1,
            density: density.clone(),
            ir: None,
        };
        let print = PrintParams {
            print_exposure: -0.6,
            black_point: 0.01,
            white_balance: [1.0, 1.05, 0.9],
            highlight_compress: 0.3,
        };
        let gamma = 1.3;
        let (out, resolved) = render(dimg, gamma, DmaxSource::None, &print);
        assert_eq!(resolved, None, "no anchor reported for None");
        let exposure_gain = 2f32.powf(print.print_exposure);
        for (i, &d) in density.iter().enumerate() {
            let c = i % 3;
            let paper = 10f32.powf(gamma * d);
            let expected = soft_clip(
                paper * print.white_balance[c] * exposure_gain - print.black_point,
                print.highlight_compress,
            );
            assert_eq!(out.rgb[i], expected, "sample {i} not bit-exact");
        }
    }

    #[test]
    fn explicit_anchor_maps_that_density_to_display_white() {
        // With a neutral print, the pixel at `D' = Dmax` (scene white) renders to
        // exactly 1.0, and the base (`D' = 0`) to `10^(−γ·Dmax) < 1` (near black).
        let dmax = 1.5f32;
        let gamma = 2.0f32;
        let dimg = DensityImage {
            width: 2,
            height: 1,
            density: vec![dmax, dmax, dmax, 0.0, 0.0, 0.0],
            ir: None,
        };
        let (out, resolved) = render(
            dimg,
            gamma,
            DmaxSource::Explicit(dmax),
            &PrintParams::default(),
        );
        assert_eq!(resolved, Some(dmax));
        for c in 0..3 {
            assert!(
                approx(out.rgb[c], 1.0, 1e-5),
                "scene white → 1.0 (chan {c})"
            );
            assert!(approx(out.rgb[3 + c], 10f32.powf(-gamma * dmax), 1e-6));
            assert!(out.rgb[3 + c] < 1.0, "base below white (chan {c})");
        }
    }

    #[test]
    fn auto_dmax_high_percentile_resists_outliers() {
        // 200 samples at 1.0 plus one blown 1000.0 (< 0.5% of the data): the
        // 99.5th percentile stays on the bulk value, not the specular/dust outlier.
        let mut d = vec![1.0f32; 200];
        d.push(1000.0);
        assert!(approx(auto_dmax(&d), 1.0, 1e-6), "got {}", auto_dmax(&d));
    }

    #[test]
    fn auto_dmax_nearest_rank_matches_the_percentile_index() {
        // Distinct values pin the exact nearest-rank index `round((n−1)·p)` in both
        // directions (a constant-bulk test would pass for any rank ≤ the top).
        // 1000 values 0..=999: index = round(999·0.995) = round(994.005) = 994.
        let d: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        assert_eq!(auto_dmax(&d), 994.0);
    }

    #[test]
    fn auto_dmax_ignores_non_finite() {
        // Non-finite densities are excluded from the rank, never returned.
        let d = vec![f32::NAN, 0.5, f32::INFINITY, 0.5, f32::NEG_INFINITY, 0.5];
        assert!(approx(auto_dmax(&d), 0.5, 1e-6));
        // All-non-finite / empty → 0.0 neutral fallback (gain 1.0), not a panic.
        assert_eq!(auto_dmax(&[f32::NAN, f32::INFINITY]), 0.0);
        assert_eq!(auto_dmax(&[]), 0.0);
    }

    #[test]
    fn auto_anchor_is_deterministic() {
        // Same input + params ⇒ identical output (the determinism contract).
        let base = FilmBase::from([0.6, 0.3, 0.18]);
        let img = LinearImage::new(
            3,
            1,
            vec![0.5, 0.25, 0.15, 0.3, 0.15, 0.09, 0.1, 0.05, 0.03],
            None,
        )
        .unwrap();
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let a = conv.convert(&img, &base).unwrap();
        let b = conv.convert(&img, &base).unwrap();
        assert_eq!(a.rgb, b.rgb);
    }

    #[test]
    fn convert_reported_surfaces_the_resolved_anchor() {
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = pixel([0.2, 0.2, 0.2], None);

        // Explicit → reports exactly that value.
        let conv = Density {
            density: DensityParams {
                dmax: DmaxSource::Explicit(1.25),
                ..DensityParams::default()
            },
            print: PrintParams::default(),
        };
        let (_, rep) = conv.convert_reported(&img, &base).unwrap();
        assert_eq!(rep.dmax, Some(1.25));

        // None → no anchor reported.
        let conv = Density {
            density: DensityParams {
                dmax: DmaxSource::None,
                ..DensityParams::default()
            },
            print: PrintParams::default(),
        };
        let (_, rep) = conv.convert_reported(&img, &base).unwrap();
        assert_eq!(rep.dmax, None);

        // Auto → a finite measured anchor.
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams::default(),
        };
        let (_, rep) = conv.convert_reported(&img, &base).unwrap();
        assert!(rep.dmax.is_some_and(f32::is_finite));
    }

    #[test]
    fn auto_anchor_maps_measured_scene_white_to_display_white() {
        // End-to-end: a uniform-density image has one density value, so the auto
        // percentile equals it and the render maps it to display white ≈ 1.0. Ties
        // the measured percentile to the render gain (catches an anchor_gain sign
        // error the explicit test's separate path could miss).
        let gamma = 1.8f32;
        let base = FilmBase::from([0.8, 0.8, 0.8]);
        let img = LinearImage::new(4, 1, vec![0.2f32; 12], None).unwrap(); // scan < base ⇒ D > 0
        let dimg = to_density(
            &img,
            &base,
            &DensityParams {
                density_gamma: gamma,
                ..DensityParams::default()
            },
        );
        let (out, resolved) = render(dimg, gamma, DmaxSource::Auto, &PrintParams::default());
        let dmax = resolved.unwrap();
        assert!(
            dmax > 0.0,
            "measured scene-white density should be positive"
        );
        for v in &out.rgb {
            assert!(
                approx(*v, 1.0, 1e-4),
                "scene white → 1.0, got {v} (dmax {dmax})"
            );
        }
    }

    #[test]
    fn explicit_anchor_composes_with_print_exposure() {
        // The anchor and print exposure fold into one multiplicative scalar: scene
        // white (D' = Dmax) at `print_exposure = k` renders to exactly `2^k`
        // (`10^(γ·(Dmax−Dmax)) · 1 · 2^k − 0`). Pins the composition at a known value.
        let dmax = 1.2f32;
        let dimg = DensityImage {
            width: 1,
            height: 1,
            density: vec![dmax, dmax, dmax],
            ir: None,
        };
        let print = PrintParams {
            print_exposure: 2.0,
            ..PrintParams::default()
        };
        let (out, _) = render(dimg, 1.5, DmaxSource::Explicit(dmax), &print);
        for c in 0..3 {
            assert!(approx(out.rgb[c], 4.0, 1e-4), "chan {c}: {}", out.rgb[c]); // 2^2
        }
    }

    #[test]
    fn auto_anchor_is_a_scalar_pooled_across_channels() {
        // Channel-asymmetric densities (R high, B low): the anchor is a single
        // pooled scalar — the *same* gain on every channel — so it can't double as
        // color correction (that's the future auto-WB task). Prove the per-channel
        // ratio `out_c / 10^(γ·D'_c)` is identical across channels (== anchor gain).
        let dimg = DensityImage {
            width: 2,
            height: 1,
            density: vec![2.0, 1.0, 0.1, 2.0, 1.0, 0.1],
            ir: None,
        };
        let gamma = 1.0f32;
        let (out, resolved) = render(
            dimg.clone(),
            gamma,
            DmaxSource::Auto,
            &PrintParams::default(),
        );
        let dmax = resolved.unwrap();
        let gain = 10f32.powf(-gamma * dmax);
        for c in 0..3 {
            let expected = 10f32.powf(gamma * dimg.density[c]) * gain;
            assert!(
                approx(out.rgb[c], expected, 1e-4),
                "chan {c}: {}",
                out.rgb[c]
            );
        }
    }
}
