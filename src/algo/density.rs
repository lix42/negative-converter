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
//! **Auto neutral white balance (`WbSource`).** The stage-4 white-balance gains
//! come from [`WbSource`]: `Explicit` gains (the default, `[1,1,1]`) are applied
//! directly; the `GrayWorld` / `Percentile` auto modes first *estimate* the gains
//! from a neutrally-rendered positive (deterministic statistics — trimmed channel
//! means / matched near-white percentiles — over finite samples only), then apply
//! them through the **same stage-4 slot**. Because application is the standard
//! slot (not a post-hoc multiply after `black_point` / the soft-clip), a later
//! run reusing the reported gains via `--white-balance` reproduces the output
//! bit-for-bit — the measure-once-reuse-for-the-roll contract. Gains are
//! green-anchored (`g = 1`): auto WB corrects color, not overall exposure.
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
    DensityParams, DmaxSource, FilmBase, LinearImage, NcError, PrintParams, Result, WbSource,
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
/// white into `inf` instead of `1.0`. A `None` anchor is applied as exactly
/// `0.0`, so it reproduces the pre-anchor render bit-for-bit (`d − 0.0 == d` for
/// every `f32`).
///
/// The anchor arrives **resolved** (see [`resolve_dmax`]) and the white-balance
/// gains **resolved** (explicit, or auto-estimated by [`estimate_wb_gains`]) —
/// not as their source enums — because [`Density::convert_reported`] may run this
/// render twice (a neutral analysis pass, then the real one) and both passes must
/// share the exact same anchor without re-measuring. `print` supplies only the
/// remaining stage-4 controls (`print_exposure`, `black_point`,
/// `highlight_compress`); its `white_balance` *source* field is deliberately not
/// read here.
///
/// Does not clamp; values may land outside `[0, 1]`.
///
/// Consumes the `DensityImage` (it is a use-once intermediate): the density buffer
/// is transformed into the output in place and the IR plane is moved, so no
/// image-sized buffer is allocated or cloned here.
pub(crate) fn render(
    density: DensityImage,
    density_gamma: f32,
    dmax: Option<f32>,
    white_balance: [f32; 3],
    print: &PrintParams,
) -> LinearImage {
    // The anchor is subtracted in the exponent (see the doc above); `None` ⇒
    // anchor 0.0 ⇒ `d − 0.0 == d` bit-exactly, so the per-pixel arithmetic is
    // bit-identical to the pre-anchor render. Stage 4 is delegated to the shared
    // `render_print`, with the density stage-3 curve fused in as its `tone` map.
    let anchor = dmax.unwrap_or(0.0);
    render_print(
        density,
        |d| 10f32.powf(density_gamma * (d - anchor)), // stage 3, anchored
        white_balance,
        print,
    )
}

/// Stage 4 — the print render, shared by every density-domain algorithm
/// (`density` and `sigmoid`), fused with a caller-supplied stage-3 `tone` map
/// (corrected density → positive linear) so the buffer is traversed once.
///
/// The fusion is mechanical, not conceptual: `tone` *is* stage 3 (the algorithm's
/// curve), this function owns only stage 4 — the per-channel white-balance gains,
/// an overall `2^print_exposure` gain (exposure in **stops**), the `black_point`
/// floor subtraction, and the highlight soft-clip — so the two sub-stages stay
/// separately parameterized (the core fidelity rule).
///
/// The white-balance gains arrive **resolved** (`[f32; 3]`), not as the
/// `print.white_balance` [`WbSource`]: an auto mode is estimated from a neutral
/// analysis render *before* this call (the algorithms' `convert_reported`, via
/// [`estimate_wb_gains`]) and applied here through the standard slot, so a later
/// run reusing the reported gains via explicit `--white-balance` is bit-identical.
/// `print.white_balance` itself is deliberately not read here.
///
/// Consumes the `DensityImage` (a use-once intermediate): the density buffer is
/// transformed into the output in place and the IR plane is moved, so no
/// image-sized buffer is allocated or cloned here.
pub(crate) fn render_print(
    density: DensityImage,
    tone: impl Fn(f32) -> f32 + Sync,
    white_balance: [f32; 3],
    print: &PrintParams,
) -> LinearImage {
    let exposure_gain = 2f32.powf(print.print_exposure);
    let wb = white_balance;
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
/// is the auto-WB modes' job, see [`estimate_wb_gains`]); `Explicit` returns the
/// given value; `None` yields no anchor. Deterministic: same buffer + params ⇒
/// same value. `pub(crate)` because `sigmoid` anchors its S-curve on the same
/// resolved `Dmax` rather than inventing a second measurement.
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

// ---------------------------------------------------------------------------
// Auto white balance (stage-4 gain estimation)
// ---------------------------------------------------------------------------

/// Cap on how many *pixels* the auto-WB statistics examine. Like
/// [`AUTO_DMAX_MAX_SAMPLES`], ~1M pixels are statistically indistinguishable
/// from the full population for a mean/percentile, and the cap bounds the
/// analysis to a small transient buffer per channel on large scans.
const AUTO_WB_MAX_PIXELS: usize = 1 << 20;

/// Percentile equalized by [`WbSource::Percentile`] (per channel, nearest rank).
/// High enough to sit on near-white content — where a neutral rendition matters
/// most — while the top 5% (specular sparkle, dust, would-be-clipped extremes)
/// never enters the statistic.
const AUTO_WB_PERCENTILE: f32 = 0.95;

/// Fraction trimmed from *each* end of a channel's distribution before the
/// [`WbSource::GrayWorld`] mean, so dead blacks and clipped/specular extremes
/// can't skew it. Frame-relative (a quantile, not an absolute level), so it
/// works for display-anchored and scene-referred (`--no-d-max`) renders alike.
const AUTO_WB_TRIM: f32 = 0.01;

/// Deterministic pixel stride for the WB statistics: the smallest stride that
/// keeps the examined pixel count under [`AUTO_WB_MAX_PIXELS`]. Strides whole
/// pixels (interleaved RGB triples), so unlike [`auto_dmax_stride`] there is no
/// channel-bias concern — every sampled pixel contributes all three channels.
fn auto_wb_stride(pixels: usize) -> usize {
    pixels.div_ceil(AUTO_WB_MAX_PIXELS).max(1)
}

/// Per-channel *finite* samples from a deterministic strided pixel walk, each
/// channel sorted ascending (`total_cmp`). Non-finite samples (`NaN`/`±inf`
/// from corrupt input) are excluded per sample, so a bad pixel can't poison a
/// statistic. The full sort makes every downstream statistic order-defined,
/// hence deterministic: same buffer ⇒ same samples ⇒ same gains.
fn wb_channel_samples(rgb: &[f32]) -> [Vec<f32>; 3] {
    let stride = auto_wb_stride(rgb.len() / 3);
    let cap = (rgb.len() / 3).div_ceil(stride);
    let mut channels = [
        Vec::with_capacity(cap),
        Vec::with_capacity(cap),
        Vec::with_capacity(cap),
    ];
    for px in rgb.chunks_exact(3).step_by(stride) {
        for (c, channel) in channels.iter_mut().enumerate() {
            if px[c].is_finite() {
                channel.push(px[c]);
            }
        }
    }
    for channel in &mut channels {
        channel.sort_unstable_by(f32::total_cmp);
    }
    channels
}

/// Nearest-rank percentile of a sorted, non-empty slice (`round((n−1)·p)`, the
/// same convention as [`auto_dmax`]).
fn nearest_rank(sorted: &[f32], p: f32) -> f32 {
    sorted[(((sorted.len() - 1) as f32) * p).round() as usize]
}

/// Mean of the central `[trim, 1 − trim]` quantile span of a sorted, non-empty
/// slice. Accumulates in `f64` sequentially over the sorted order — a fully
/// order-defined sum, so the result is deterministic (a parallel float
/// reduction would not be).
fn trimmed_mean(sorted: &[f32], trim: f32) -> f32 {
    let lo = (((sorted.len() - 1) as f32) * trim).round() as usize;
    let hi = (((sorted.len() - 1) as f32) * (1.0 - trim)).round() as usize;
    let span = &sorted[lo..=hi];
    (span.iter().map(|&v| f64::from(v)).sum::<f64>() / span.len() as f64) as f32
}

/// Resolve the stage-4 white-balance gains `[r, g, b]` from a **neutrally
/// rendered** positive (`rgb`: stage 3 output — anchored `10^(γ·(D'−Dmax))`
/// with unit gains and no exposure/black/soft-clip applied).
///
/// `Explicit` gains pass through untouched, keeping the function total (callers
/// shortcut that case to skip the analysis render). The auto modes are pure,
/// deterministic statistics over the finite samples of a strided pixel walk
/// (see [`wb_channel_samples`]); distribution extremes are excluded by
/// construction (the percentile's top tail / the trimmed mean), so clipped
/// speculars and dead pixels don't skew the estimate:
///
/// - [`WbSource::GrayWorld`]: equalize the per-channel trimmed means — a cast
///   shows up as unequal channel averages (assumes the frame averages neutral).
/// - [`WbSource::Percentile`]: equalize the per-channel [`AUTO_WB_PERCENTILE`]
///   levels — a cast shows up as unequal near-white levels; robust to a
///   dominant scene color that would bias the means.
///
/// Gains are **green-anchored** (`g = 1`): auto WB corrects *color*, not
/// overall brightness — exposure is `print_exposure`'s job. Fails loudly
/// ([`NcError::Other`], exit 1) when a channel yields no usable level (all
/// samples non-finite, or a non-positive level no multiplicative gain can
/// correct) — never silently-neutral or garbage gains.
pub(crate) fn estimate_wb_gains(rgb: &[f32], source: WbSource) -> Result<[f32; 3]> {
    let mode = match source {
        WbSource::Explicit(gains) => return Ok(gains),
        WbSource::GrayWorld => "gray-world",
        WbSource::Percentile => "percentile",
    };
    let level_of = |sorted: &[f32]| match source {
        WbSource::GrayWorld => trimmed_mean(sorted, AUTO_WB_TRIM),
        // `Explicit` returned above; only `Percentile` reaches here.
        _ => nearest_rank(sorted, AUTO_WB_PERCENTILE),
    };

    let channels = wb_channel_samples(rgb);
    let mut level = [0.0f32; 3];
    for (c, name) in ["red", "green", "blue"].into_iter().enumerate() {
        let l = if channels[c].is_empty() {
            f32::NAN // no usable sample in this channel
        } else {
            level_of(&channels[c])
        };
        if !l.is_finite() || l <= 0.0 {
            return Err(NcError::Other(format!(
                "auto white balance ({mode}): the {name} channel has no usable \
                 level (got {l}); pass explicit --white-balance gains instead"
            )));
        }
        level[c] = l;
    }

    let gains = [level[1] / level[0], 1.0, level[1] / level[2]];
    for (g, name) in gains.into_iter().zip(["red", "green", "blue"]) {
        // Positive finite levels can still divide into inf/0 across an extreme
        // dynamic range (subnormal denominators); guard the gains themselves.
        if !g.is_finite() || g <= 0.0 {
            return Err(NcError::Other(format!(
                "auto white balance ({mode}): estimated {name} gain is not a \
                 positive finite value (got {g}); pass explicit --white-balance \
                 gains instead"
            )));
        }
    }
    Ok(gains)
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

        // Resolve the display-white anchor once, from the corrected densities:
        // the WB analysis pass and the final render must share the exact same
        // anchor (re-measuring would be wasted work; a different anchor would
        // break the reuse-the-reported-gains bit-exactness).
        let dmax = resolve_dmax(&density.density, self.density.dmax);

        // Resolve the white-balance gains. Explicit gains skip the analysis
        // pass entirely — the default path costs nothing and its per-pixel
        // arithmetic is unchanged.
        let wb = match self.print.white_balance {
            WbSource::Explicit(gains) => gains,
            auto_mode => {
                // Estimation: render the positive with a fully *neutral* print
                // (unit gains, 0 EV, no black point, no soft-clip) so the
                // statistics measure exactly the quantity the white-balance
                // slot multiplies. The user's `print_exposure` would cancel in
                // the channel ratios anyway, while `black_point` /
                // `highlight_compress` apply *after* the gains and would
                // distort them. The density buffer is cloned because it is the
                // cached stage-3 input the final render below consumes —
                // measure on the copy, render the original. The analysis pass
                // reads only `rgb`, so it drops the IR plane (`ir: None`) rather
                // than cloning an image-sized buffer for nothing; the final
                // render still consumes the original `density` with IR intact.
                let analysis = DensityImage {
                    width: density.width,
                    height: density.height,
                    density: density.density.clone(),
                    ir: None,
                };
                let neutral = render(
                    analysis,
                    self.density.density_gamma,
                    dmax,
                    [1.0, 1.0, 1.0],
                    &PrintParams::default(),
                );
                estimate_wb_gains(&neutral.rgb, auto_mode)?
            }
        };

        // Application: always the standard stage-4 white-balance slot (before
        // `black_point` and the soft-clip), never a post-hoc multiply — so a
        // later run reusing the reported gains via `--white-balance` is
        // bit-identical (measure once, reuse for the roll).
        let image = render(density, self.density.density_gamma, dmax, wb, &self.print);
        Ok((
            image,
            ConvertReport {
                dmax,
                white_balance: Some(wb),
            },
        ))
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
        let out = render(d, 1.0, None, [1.0; 3], &PrintParams::default());
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
        let out = render(d, 0.5, None, [1.0; 3], &PrintParams::default());
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
            ..PrintParams::default()
        };
        let out = render(d, 1.0, None, [2.0, 1.0, 0.5], &print);
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
        let out = render(d, 1.0, None, [1.0; 3], &PrintParams::default());
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
        let wb = [1.0, 1.05, 1.1];
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
                white_balance: WbSource::Explicit(wb),
                highlight_compress: 0.2,
            },
        };
        let via_convert = conv.convert(&img, &base).unwrap();
        let dimg = to_density(&img, &base, &conv.density);
        let anchor = resolve_dmax(&dimg.density, conv.density.dmax);
        let via_parts = render(dimg, conv.density.density_gamma, anchor, wb, &conv.print);
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
        let out = render(d, 1.0, None, [1.0; 3], &print);
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
        let out = render(dimg, gamma, Some(dmax), [1.0; 3], &PrintParams::default());
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
        // A `None` anchor must reproduce the pre-anchor render bit-for-bit: the
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
        let wb = [1.0, 1.05, 0.9];
        let print = PrintParams {
            print_exposure: -0.6,
            black_point: 0.01,
            white_balance: WbSource::Explicit(wb),
            highlight_compress: 0.3,
        };
        let gamma = 1.3;
        assert_eq!(
            resolve_dmax(&density, DmaxSource::None),
            None,
            "no anchor resolved for None"
        );
        let out = render(dimg, gamma, None, wb, &print);
        let exposure_gain = 2f32.powf(print.print_exposure);
        for (i, &d) in density.iter().enumerate() {
            let c = i % 3;
            let paper = 10f32.powf(gamma * d);
            let expected = soft_clip(
                paper * wb[c] * exposure_gain - print.black_point,
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
        assert_eq!(
            resolve_dmax(&dimg.density, DmaxSource::Explicit(dmax)),
            Some(dmax)
        );
        let out = render(dimg, gamma, Some(dmax), [1.0; 3], &PrintParams::default());
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
        // The (explicit, default-neutral) gains are surfaced too.
        assert_eq!(rep.white_balance, Some([1.0, 1.0, 1.0]));

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
        let resolved = resolve_dmax(&dimg.density, DmaxSource::Auto);
        let out = render(dimg, gamma, resolved, [1.0; 3], &PrintParams::default());
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
        let out = render(dimg, 1.5, Some(dmax), [1.0; 3], &print);
        for c in 0..3 {
            assert!(approx(out.rgb[c], 4.0, 1e-4), "chan {c}: {}", out.rgb[c]); // 2^2
        }
    }

    #[test]
    fn auto_anchor_is_a_scalar_pooled_across_channels() {
        // Channel-asymmetric densities (R high, B low): the anchor is a single
        // pooled scalar — the *same* gain on every channel — so it can't double as
        // color correction (that's the auto-WB modes' job). Prove the per-channel
        // ratio `out_c / 10^(γ·D'_c)` is identical across channels (== anchor gain).
        let dimg = DensityImage {
            width: 2,
            height: 1,
            density: vec![2.0, 1.0, 0.1, 2.0, 1.0, 0.1],
            ir: None,
        };
        let gamma = 1.0f32;
        let resolved = resolve_dmax(&dimg.density, DmaxSource::Auto);
        let out = render(
            dimg.clone(),
            gamma,
            resolved,
            [1.0; 3],
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

    // --- auto white balance ------------------------------------------------

    /// An interleaved RGB buffer of `n` copies of `px` (a rendered positive for
    /// the estimator tests).
    fn uniform_positive(px: [f32; 3], n: usize) -> Vec<f32> {
        px.iter().copied().cycle().take(3 * n).collect()
    }

    #[test]
    fn estimate_wb_explicit_gains_pass_through() {
        // Explicit is a pass-through: no statistics run, the image is ignored.
        let gains = [1.3, 1.0, 0.7];
        let got = estimate_wb_gains(&[], WbSource::Explicit(gains)).unwrap();
        assert_eq!(got, gains);
    }

    #[test]
    fn gray_world_gains_neutralize_a_uniform_cast() {
        // Every pixel carries the same cast, so the trimmed means are exactly the
        // cast and the gains are the green-anchored inverse: applying them
        // equalizes the channels.
        let rgb = uniform_positive([0.4, 0.5, 0.8], 200);
        let gains = estimate_wb_gains(&rgb, WbSource::GrayWorld).unwrap();
        assert!(approx(gains[0], 0.5 / 0.4, 1e-5), "r gain {}", gains[0]);
        assert_eq!(gains[1], 1.0, "green-anchored");
        assert!(approx(gains[2], 0.5 / 0.8, 1e-5), "b gain {}", gains[2]);
        for px in rgb.chunks_exact(3) {
            let balanced = [px[0] * gains[0], px[1] * gains[1], px[2] * gains[2]];
            assert!(approx(balanced[0], balanced[1], 1e-5));
            assert!(approx(balanced[1], balanced[2], 1e-5));
        }
    }

    #[test]
    fn percentile_gains_equalize_near_white_levels() {
        // Channels are the same ramp scaled per channel, so every per-channel
        // statistic scales with it and both modes recover the inverse scale.
        let scale = [0.8f32, 1.0, 1.2];
        let mut rgb = Vec::new();
        for i in 0..100 {
            let t = (i + 1) as f32 / 100.0;
            rgb.extend_from_slice(&[scale[0] * t, scale[1] * t, scale[2] * t]);
        }
        for mode in [WbSource::Percentile, WbSource::GrayWorld] {
            let gains = estimate_wb_gains(&rgb, mode).unwrap();
            assert!(approx(gains[0], 1.0 / 0.8, 1e-4), "{mode:?} r {}", gains[0]);
            assert_eq!(gains[1], 1.0, "{mode:?} green-anchored");
            assert!(approx(gains[2], 1.0 / 1.2, 1e-4), "{mode:?} b {}", gains[2]);
        }
    }

    #[test]
    fn percentile_mode_resists_a_dominant_color_gray_world_does_not() {
        // 90% of the frame is a strong green subject; 10% is genuinely neutral
        // near-white. The near-white percentile lands on the neutral highlights
        // (gains ≈ 1), while the gray-world means are dragged by the subject —
        // the documented tradeoff between the two modes.
        let mut rgb = uniform_positive([0.2, 0.6, 0.2], 90);
        rgb.extend(uniform_positive([0.9, 0.9, 0.9], 10));
        let p = estimate_wb_gains(&rgb, WbSource::Percentile).unwrap();
        for (c, gain) in p.into_iter().enumerate() {
            assert!(approx(gain, 1.0, 1e-5), "percentile chan {c}: {gain}");
        }
        let gw = estimate_wb_gains(&rgb, WbSource::GrayWorld).unwrap();
        assert!(
            gw[0] > 2.0,
            "gray-world red gain dragged by cast: {}",
            gw[0]
        );
    }

    #[test]
    fn estimate_wb_ignores_non_finite_and_extreme_samples() {
        // A NaN sample, an inf sample, and a huge finite outlier (< 1% of the
        // data) must not move either statistic off the bulk values.
        let mut rgb = uniform_positive([0.4, 0.5, 0.6], 200);
        rgb.extend_from_slice(&[1000.0, f32::NAN, f32::INFINITY]);
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            let gains = estimate_wb_gains(&rgb, mode).unwrap();
            assert!(approx(gains[0], 0.5 / 0.4, 1e-3), "{mode:?} r {}", gains[0]);
            assert!(approx(gains[2], 0.5 / 0.6, 1e-3), "{mode:?} b {}", gains[2]);
        }
    }

    #[test]
    fn estimate_wb_fails_loudly_on_an_unusable_channel() {
        // An all-non-finite channel has no usable level — that must be a loud
        // error (exit 1), never silently-neutral or garbage gains.
        let rgb = uniform_positive([f32::NAN, 0.5, 0.5], 8);
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            let err = estimate_wb_gains(&rgb, mode).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{mode:?}");
        }
        // A non-positive level (possible only for degenerate input — the neutral
        // analysis render itself produces 10^x > 0) is rejected the same way.
        let rgb = uniform_positive([0.0, 0.5, 0.5], 8);
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            assert!(estimate_wb_gains(&rgb, mode).is_err(), "{mode:?}");
        }
    }

    #[test]
    fn auto_wb_stride_is_bounded() {
        assert_eq!(auto_wb_stride(0), 1);
        assert_eq!(auto_wb_stride(AUTO_WB_MAX_PIXELS), 1);
        let big = 7 * AUTO_WB_MAX_PIXELS + 3;
        let stride = auto_wb_stride(big);
        assert!(big.div_ceil(stride) <= AUTO_WB_MAX_PIXELS);
    }

    #[test]
    fn auto_wb_convert_neutralizes_a_cast_end_to_end() {
        // A wrong (neutral) base under an orange-mask scan leaves a constant
        // per-channel cast in the positive; both auto modes must estimate gains
        // that equalize the channels of this two-tone frame.
        let base = FilmBase::from([0.8, 0.8, 0.8]); // deliberately ignores the mask
        let cast = [0.5f32, 0.3, 0.2]; // orange-ish transmissions
        let mut rgb = Vec::new();
        for i in 0..64 {
            let t = if i % 2 == 0 { 1.0 } else { 0.5 }; // two-tone content
            rgb.extend_from_slice(&[cast[0] * t, cast[1] * t, cast[2] * t]);
        }
        let img = LinearImage::new(64, 1, rgb, None).unwrap();
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            let conv = Density {
                density: DensityParams::default(),
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
    fn auto_wb_carries_ir_through_the_final_output() {
        // The auto-WB analysis pass renders on an IR-dropped copy (perf: no
        // image-sized IR clone), but the *final* render must still consume the
        // original density with its IR plane intact — assert the IR rides through.
        let base = FilmBase::from([0.6, 0.6, 0.6]);
        let img = pixel([0.2, 0.2, 0.2], Some(0.42));
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
        };
        let out = conv.convert(&img, &base).unwrap();
        assert_eq!(out.ir.as_deref(), Some(&[0.42_f32][..]));
    }

    #[test]
    fn auto_wb_output_is_bit_exact_with_explicit_rerun_of_reported_gains() {
        // The measure-once-reuse-for-the-roll contract: a run that reuses the
        // reported gains via explicit `--white-balance` must reproduce the auto
        // run bit-for-bit — this is why application goes through the standard
        // stage-4 slot and shares the resolved anchor, never a post-hoc multiply.
        // Non-default print params prove the equality holds with black_point and
        // the soft-clip in play.
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
        let auto = Density {
            density: DensityParams::default(),
            print: print.clone(),
        };
        let (out_auto, rep) = auto.convert_reported(&img, &base).unwrap();
        let gains = rep.white_balance.expect("auto gains reported");

        let explicit = Density {
            density: DensityParams::default(),
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
    fn auto_wb_survives_scene_referred_no_dmax_render() {
        // Pins the AUTO_WB_TRIM / percentile robustness claim for scene-referred
        // output: with `DmaxSource::None` the render is unanchored (base → 1.0,
        // detail far above — a wide dynamic range), so the analysis positive spans
        // orders of magnitude. The extremes-excluding statistics must still yield
        // finite, channel-equalizing, green-anchored gains rather than being
        // dragged to inf by the brightest samples.
        let base = FilmBase::from([0.9, 0.9, 0.9]); // neutral base ⇒ leaves a cast
        // A wide density spread per pixel (thin → very dense), same per-channel
        // cast ratio throughout, so correct gains equalize every pixel.
        let cast = [0.6f32, 0.4, 0.25];
        let mut rgb = Vec::new();
        for i in 0..128 {
            let t = 0.9f32.powi(i % 32); // transmissions from ~1 down to ~0.03
            rgb.extend_from_slice(&[cast[0] * t, cast[1] * t, cast[2] * t]);
        }
        let img = LinearImage::new(128, 1, rgb, None).unwrap();
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            let conv = Density {
                density: DensityParams {
                    dmax: DmaxSource::None,
                    ..DensityParams::default()
                },
                print: PrintParams {
                    white_balance: mode,
                    ..PrintParams::default()
                },
            };
            let (out, rep) = conv.convert_reported(&img, &base).unwrap();
            assert_eq!(rep.dmax, None, "{mode:?}: no anchor for --no-d-max");
            let gains = rep.white_balance.expect("gains reported");
            assert_eq!(gains[1], 1.0, "{mode:?} green-anchored");
            for g in gains {
                assert!(g.is_finite() && g > 0.0, "{mode:?}: gain {g} not usable");
            }
            for px in out.rgb.chunks_exact(3) {
                assert!(
                    px.iter().all(|v| v.is_finite()),
                    "{mode:?}: non-finite output {px:?}"
                );
                assert!(approx(px[0], px[1], 1e-3), "{mode:?}: {px:?}");
                assert!(approx(px[1], px[2], 1e-3), "{mode:?}: {px:?}");
            }
        }
    }

    #[test]
    fn auto_wb_is_deterministic() {
        // Same input + params ⇒ identical gains and identical output.
        let base = FilmBase::from([0.7, 0.5, 0.3]);
        let img = LinearImage::new(
            2,
            2,
            vec![
                0.4, 0.3, 0.2, 0.35, 0.22, 0.11, //
                0.5, 0.4, 0.25, 0.1, 0.07, 0.04,
            ],
            None,
        )
        .unwrap();
        let conv = Density {
            density: DensityParams::default(),
            print: PrintParams {
                white_balance: WbSource::GrayWorld,
                ..PrintParams::default()
            },
        };
        let (a, ra) = conv.convert_reported(&img, &base).unwrap();
        let (b, rb) = conv.convert_reported(&img, &base).unwrap();
        assert_eq!(ra.white_balance, rb.white_balance);
        assert_eq!(a.rgb, b.rgb);
    }
}
