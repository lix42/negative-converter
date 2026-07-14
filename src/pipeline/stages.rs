//! Stage wiring as pure functions — threads film-base → algorithm → output
//! color transform together for the orchestrator to call.
//!
//! This is the in-memory core of the `convert` pipeline (design-spec §6, stages
//! 2–4). Decode (stage 1) and encode (stage 5) are I/O and stay with the
//! orchestrator (`cli`); everything here is pure `(input, params) -> output` so
//! it composes and unit-tests without touching the filesystem.

use crate::algo::{self, AlgoParams};
use crate::pipeline::{color, film_base};
use crate::types::{
    Algorithm, DensityParams, FilmBase, FilmBaseParams, LinearImage, OutputParams, PrintParams,
    Result, SimpleParams,
};

/// The in-memory pipeline result the orchestrator hands to the encoder: the
/// output-color-transformed positive image, the ICC blob to embed alongside it,
/// and the film base that was resolved (for the JSON report).
pub struct Rendered {
    pub image: LinearImage,
    pub icc: Vec<u8>,
    pub film_base: FilmBase,
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
    print: &PrintParams,
) -> AlgoParams {
    match algorithm {
        Algorithm::Simple => AlgoParams::Simple(simple.clone()),
        Algorithm::Density => AlgoParams::Density {
            density: density.clone(),
            print: print.clone(),
        },
    }
}

/// Run pipeline stages 2–4 on a decoded image: estimate the film base, convert
/// negative → positive with the selected algorithm, then transform the result
/// into the output color space. Returns the color-transformed image, the ICC
/// blob to embed, and the resolved film base.
///
/// Pure and total in its inputs: any failure (a degenerate estimated film base,
/// an out-of-bounds `--base-region`, an unreadable custom ICC profile) surfaces
/// as an [`NcError`](crate::types::NcError) with the right exit code, never a
/// silently-wrong image. The IR plane is carried through untouched (Step-1 rule:
/// preserve, don't consume).
pub fn render(
    image: &LinearImage,
    film_base_params: &FilmBaseParams,
    algo_params: AlgoParams,
    output_params: &OutputParams,
) -> Result<Rendered> {
    let film_base = film_base::estimate(image, film_base_params)?;
    let converter = algo::build(algo_params);
    let positive = converter.convert(image, &film_base)?;
    let (image, icc) = color::to_output(&positive, output_params)?;
    Ok(Rendered {
        image,
        icc,
        film_base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FilmBaseSource, OutDepth};

    /// A small synthetic negative: a bright, uniform orange border (the film
    /// base) around a darker interior, so `Auto` estimation has a border to find.
    fn synthetic_negative(w: u32, h: u32) -> LinearImage {
        let border = [0.9, 0.55, 0.42];
        let interior = [0.3, 0.22, 0.18];
        let margin = 3;
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let edge = x < margin || y < margin || x >= w - margin || y >= h - margin;
                rgb.extend_from_slice(if edge { &border } else { &interior });
            }
        }
        LinearImage::new(w, h, rgb, None).unwrap()
    }

    fn density_params() -> (SimpleParams, DensityParams, PrintParams) {
        (
            SimpleParams::default(),
            DensityParams::default(),
            PrintParams::default(),
        )
    }

    #[test]
    fn algo_params_selects_the_requested_algorithm() {
        let (s, d, p) = density_params();
        assert!(matches!(
            algo_params(Algorithm::Simple, &s, &d, &p),
            AlgoParams::Simple(_)
        ));
        assert!(matches!(
            algo_params(Algorithm::Density, &s, &d, &p),
            AlgoParams::Density { .. }
        ));
    }

    #[test]
    fn render_runs_the_full_simple_path_and_transforms_color() {
        let img = synthetic_negative(40, 40);
        let (s, d, p) = density_params();
        let fb = FilmBaseParams {
            source: FilmBaseSource::Auto,
        };
        let out = render(
            &img,
            &fb,
            algo_params(Algorithm::Simple, &s, &d, &p),
            &OutputParams::default(),
        )
        .unwrap();
        assert_eq!((out.image.width, out.image.height), (40, 40));
        assert!(!out.icc.is_empty(), "an ICC profile must be produced");
        // The auto border estimate lands on the bright orange base.
        assert!(out.film_base.r > out.film_base.b, "orange base: r > b");
    }

    #[test]
    fn render_runs_the_density_path_with_explicit_base() {
        let img = synthetic_negative(16, 16);
        let (s, d, p) = density_params();
        let fb = FilmBaseParams {
            source: FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
        };
        let out = render(
            &img,
            &fb,
            algo_params(Algorithm::Density, &s, &d, &p),
            &OutputParams {
                out_depth: OutDepth::F32,
                ..OutputParams::default()
            },
        )
        .unwrap();
        assert_eq!(out.film_base, FilmBase::from([0.9, 0.55, 0.42]));
        assert_eq!(out.image.rgb.len(), 16 * 16 * 3);
    }

    #[test]
    fn render_propagates_a_degenerate_estimated_base() {
        // A base region over a black (zero-transmission) patch yields a zero
        // channel; both converters must reject it rather than divide by zero.
        let mut img = synthetic_negative(20, 20);
        for px in img.rgb.chunks_exact_mut(3).take(20) {
            px.copy_from_slice(&[0.0, 0.0, 0.0]);
        }
        let (s, d, p) = density_params();
        let fb = FilmBaseParams {
            source: FilmBaseSource::Region([0, 0, 20, 1]),
        };
        match render(
            &img,
            &fb,
            algo_params(Algorithm::Density, &s, &d, &p),
            &OutputParams::default(),
        ) {
            Err(e) => assert_eq!(e.exit_code(), 1),
            Ok(_) => panic!("expected a degenerate-base error"),
        }
    }
}
