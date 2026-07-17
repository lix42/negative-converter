//! Stage wiring as pure functions — threads film-base → algorithm → output
//! color transform together for the orchestrator to call.
//!
//! This is the in-memory core of the `convert` pipeline (design-spec §6, stages
//! 2–4). Decode (stage 1) and encode (stage 5) are I/O and stay with the
//! orchestrator (`cli`); everything here is pure `(input, params) -> output` so
//! it composes and unit-tests without touching the filesystem — with one
//! documented exception: [`render`] reads a wall clock to fill [`StageTimings`]
//! for the telemetry record (a report-only channel; the pixels stay
//! deterministic and untouched by the measurement).

use std::time::Instant;

use crate::algo::{self, AlgoParams, ConvertReport};
use crate::pipeline::color;
use crate::types::{
    Algorithm, DensityParams, FilmBase, LinearImage, OutputParams, PrintParams, Result,
    SigmoidParams, SimpleParams,
};

/// The in-memory pipeline result the orchestrator hands to the encoder: the
/// output-color-transformed positive image and the ICC blob to embed alongside
/// it.
pub struct Rendered {
    pub image: LinearImage,
    pub icc: Vec<u8>,
    /// Algorithm-reported diagnostics (e.g. the resolved `Dmax` anchor) for the
    /// JSON report.
    pub convert: ConvertReport,
    /// Wall-clock per-stage timings measured around the calls in [`render`], for
    /// the telemetry record's `timing_ms` block. Like [`ConvertReport`], a
    /// report-only channel: it is never serialized into the recipe sidecar and
    /// never read back by any stage, so the byte-identical-output determinism
    /// contract is untouched.
    pub timings: StageTimings,
}

/// Wall-clock durations of the two in-memory stages [`render`] runs, in
/// milliseconds. Report-only diagnostics the orchestrator folds into the
/// telemetry record alongside its own decode / film-base / encode timings.
#[derive(Clone, Copy, Debug, Default)]
pub struct StageTimings {
    pub algorithm_ms: f64,
    pub color_ms: f64,
}

/// Assemble the algorithm's parameter set for the selected `algorithm` from the
/// resolved per-stage params. Only the selected algorithm's params are carried;
/// the other algorithm's knobs stay inert in the config (so a recipe round-trips
/// across an `--algorithm` switch) but never reach a converter.
///
/// Downstream of this function the built `AlgoParams` *is* the algorithm
/// selector (its variant carries the choice), so nothing past here takes a
/// separate algorithm argument that could disagree with the params.
pub fn algo_params(
    algorithm: Algorithm,
    simple: &SimpleParams,
    density: &DensityParams,
    sigmoid: &SigmoidParams,
    print: &PrintParams,
) -> AlgoParams {
    match algorithm {
        Algorithm::Simple => AlgoParams::Simple(simple.clone()),
        Algorithm::Density => AlgoParams::Density {
            density: density.clone(),
            print: print.clone(),
        },
        // `sigmoid` shares the density (stages 1–2 + anchor) and print (stage 4)
        // params with `density`; only stage 3 is its own.
        Algorithm::Sigmoid => AlgoParams::Sigmoid {
            density: density.clone(),
            sigmoid: sigmoid.clone(),
            print: print.clone(),
        },
    }
}

/// Run pipeline stages 3–4 on a decoded image and an **already-resolved** film
/// base: convert negative → positive with the selected algorithm, then
/// transform the result into the output color space. Returns the
/// color-transformed image and the ICC blob to embed.
///
/// Film-base estimation (stage 2) is deliberately **not** done here — the
/// orchestrator resolves the base first (via [`film_base::estimate`]) so it can
/// surface the estimate's quality warnings before this fallible render runs (a
/// downstream failure must not swallow the "non-uniform region" warning that
/// explains a bad base). Total in its inputs: any failure (a degenerate film
/// base, an unreadable custom ICC profile) surfaces as an
/// [`NcError`](crate::types::NcError) with the right exit code, never a
/// silently-wrong image. The IR plane is carried through untouched (Step-1 rule:
/// preserve, don't consume).
///
/// The algorithm and output-color stages are each timed with [`Instant`] pairs
/// (returned as [`StageTimings`] for the telemetry record; the film-base stage is
/// timed by the orchestrator, which now owns that estimation). The measurement is
/// the one deliberate impurity here; it never reads back into the pipeline, so the
/// same inputs still produce bit-identical pixels and ICC whether or not telemetry
/// is collected.
pub fn render(
    image: &LinearImage,
    film_base: &FilmBase,
    algo_params: AlgoParams,
    output_params: &OutputParams,
) -> Result<Rendered> {
    let started = Instant::now();
    let converter = algo::build(algo_params);
    let (positive, convert) = converter.convert_reported(image, film_base)?;
    let algorithm_ms = ms_since(started);

    let started = Instant::now();
    let (image, icc) = color::to_output(&positive, output_params)?;
    let color_ms = ms_since(started);

    Ok(Rendered {
        image,
        icc,
        convert,
        timings: StageTimings {
            algorithm_ms,
            color_ms,
        },
    })
}

/// Milliseconds elapsed since `started`, as an `f64` for the telemetry record.
fn ms_since(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::film_base;
    use crate::types::{FilmBaseParams, FilmBaseSource};

    /// A small synthetic negative with the real scan layout — a near-black
    /// holder ring, then a bright, uniform orange rebate band (the film base),
    /// then a varied interior — so `Auto` estimation has a rebate to find.
    fn synthetic_negative(w: u32, h: u32) -> LinearImage {
        let holder = [0.01, 0.01, 0.01];
        let rebate = [0.9, 0.55, 0.42];
        let (holder_px, rebate_px) = (1, 2);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let depth = x.min(y).min(w - 1 - x).min(h - 1 - y);
                if depth < holder_px {
                    rgb.extend_from_slice(&holder);
                } else if depth < holder_px + rebate_px {
                    rgb.extend_from_slice(&rebate);
                } else {
                    // Varied picture content, darker than the rebate.
                    let t = (x + y) as f32 / (w + h) as f32;
                    rgb.extend_from_slice(&[0.1 + 0.3 * t, 0.08 + 0.2 * t, 0.05 + 0.15 * t]);
                }
            }
        }
        LinearImage::new(w, h, rgb, None).unwrap()
    }

    fn density_params() -> (SimpleParams, DensityParams, SigmoidParams, PrintParams) {
        (
            SimpleParams::default(),
            DensityParams::default(),
            SigmoidParams::default(),
            PrintParams::default(),
        )
    }

    #[test]
    fn algo_params_selects_the_requested_algorithm() {
        let (s, d, g, p) = density_params();
        assert!(matches!(
            algo_params(Algorithm::Simple, &s, &d, &g, &p),
            AlgoParams::Simple(_)
        ));
        assert!(matches!(
            algo_params(Algorithm::Density, &s, &d, &g, &p),
            AlgoParams::Density { .. }
        ));
        assert!(matches!(
            algo_params(Algorithm::Sigmoid, &s, &d, &g, &p),
            AlgoParams::Sigmoid { .. }
        ));
    }

    /// Resolve the film base the way the orchestrator does (stage 2), so the
    /// render tests exercise the same estimate → render sequence as `cli`.
    fn resolve(img: &LinearImage, source: FilmBaseSource) -> FilmBase {
        film_base::estimate(img, &FilmBaseParams { source })
            .unwrap()
            .base
    }

    #[test]
    fn render_runs_the_full_simple_path_and_transforms_color() {
        let img = synthetic_negative(40, 40);
        let (s, d, g, p) = density_params();
        // The auto estimate lands on the bright orange base (r > b).
        let base = resolve(&img, FilmBaseSource::Auto);
        assert!(base.r > base.b, "orange base: r > b");
        let out = render(
            &img,
            &base,
            algo_params(Algorithm::Simple, &s, &d, &g, &p),
            &OutputParams::default(),
        )
        .unwrap();
        assert_eq!((out.image.width, out.image.height), (40, 40));
        assert!(!out.icc.is_empty(), "an ICC profile must be produced");
    }

    #[test]
    fn render_runs_the_density_path_with_explicit_base() {
        let img = synthetic_negative(16, 16);
        let (s, d, g, p) = density_params();
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let out = render(
            &img,
            &base,
            algo_params(Algorithm::Density, &s, &d, &g, &p),
            &OutputParams {
                hdr: true,
                ..OutputParams::default()
            },
        )
        .unwrap();
        assert_eq!(out.image.rgb.len(), 16 * 16 * 3);
    }

    #[test]
    fn render_runs_the_sigmoid_path_and_reports_the_anchor() {
        let img = synthetic_negative(16, 16);
        let (s, d, g, p) = density_params();
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let out = render(
            &img,
            &base,
            algo_params(Algorithm::Sigmoid, &s, &d, &g, &p),
            &OutputParams {
                hdr: true,
                ..OutputParams::default()
            },
        )
        .unwrap();
        assert_eq!(out.image.rgb.len(), 16 * 16 * 3);
        // The default Auto anchor rides back through ConvertReport.
        assert!(out.convert.dmax.is_some_and(f32::is_finite));
    }

    #[test]
    fn render_rejects_a_degenerate_base() {
        // Defense-in-depth: even if a zero-channel base reached `render` (estimate
        // now rejects it at birth), the converter must reject it rather than
        // divide by zero — exit 1, never a silently-wrong image.
        let img = synthetic_negative(20, 20);
        let (s, d, g, p) = density_params();
        let base = FilmBase::from([0.0, 0.55, 0.42]);
        match render(
            &img,
            &base,
            algo_params(Algorithm::Density, &s, &d, &g, &p),
            &OutputParams::default(),
        ) {
            Err(e) => assert_eq!(e.exit_code(), 1),
            Ok(_) => panic!("expected a degenerate-base error"),
        }
    }
}
