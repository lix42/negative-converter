//! `simple` — channel-inversion baseline. A **debugging** path, not the B&W
//! one: B&W runs through `density` (`algo/bw-support`).
//!
//! A literal per-channel inversion: neutralize the film base, then invert.
//! Cheap and predictable — the trustworthy reference against which the
//! `density` reconstruction is judged. It deliberately does **no**
//! density-domain math (log/exp, orange-mask modeling); that is what
//! distinguishes `density`.
//!
//! The pre-split simple converter also applied an inversion white balance and
//! a clip-range remap here. Those are **not** reconstruction parameters
//! (design-spec §7.1): the typed [`FilmRgbImage`] ends at the direct unclamped
//! positive `1 − scan/Dmin`, and the controls now live downstream as explicit
//! `print.white_balance` and `print.linear_range`. Both homes *exist*, but neither
//! is reachable for `simple` yet: `film-master` — the one named preset this build
//! accepts — bypasses print controls entirely, and the shared display stage that
//! would apply `linear_range` has no accepted preset. So the §7.1 migration
//! aliases activate with `output/{sdr,hdr}-display-rendering`, not with
//! `film-master`. Their defaults were the exact identity (`(x·1 − 0)/1`), so the
//! default simple output is bit-identical to the pre-split converter.

use rayon::prelude::*;

use crate::algo::FilmRgbImage;
use crate::types::{FilmBase, LinearImage, NcError, Result};

/// Simple reconstruction: per channel, in the linear working space,
///
/// 1. neutralize the film base — divide by `base` transmission, so an
///    unexposed base pixel maps to 1.0 (a neutral base of `[1,1,1]` is inert,
///    leaving a pure `1 - v` inversion);
/// 2. invert — `positive = 1 - normalized`.
///
/// Output is left **unclamped** (values may fall outside `[0, 1]`); range
/// clamping happens only at the u16 encode step. The IR plane is carried
/// through untouched.
///
/// The film `base` is only CLI-validated for an explicit `--film-base`; a
/// `Region`/`Auto` base is estimated from pixels at runtime
/// (`pipeline::film_base::estimate`) and carries no positivity guarantee — a
/// region over the dark holder can yield a zero channel. This stage is the
/// first to divide by it, so it guards the base explicitly and fails loudly
/// rather than emit silent `inf`/`NaN`.
pub(super) fn reconstruct(image: &LinearImage, base: &FilmBase) -> Result<FilmRgbImage> {
    let base = [base.r, base.g, base.b];
    for (chan, b) in ["r", "g", "b"].into_iter().zip(base) {
        if !(b.is_finite() && b > 0.0) {
            return Err(NcError::Other(format!(
                "film base {chan} channel must be finite and > 0 (got {b}); the \
                 estimated base is degenerate — pass an explicit --film-base or point \
                 --base-region at the unexposed film rebate"
            )));
        }
    }

    // Per-pixel independent; writing through zipped position-matched chunks
    // keeps the result deterministic without per-thread collect buffers.
    // `rgb.len()` is a multiple of 3 (a `LinearImage` invariant), so every
    // chunk is exactly one RGB triple.
    let mut rgb = vec![0.0f32; image.rgb.len()];
    rgb.par_chunks_exact_mut(3)
        .zip(image.rgb.par_chunks_exact(3))
        .for_each(|(out, px)| {
            for c in 0..3 {
                out[c] = 1.0 - px[c] / base[c];
            }
        });

    Ok(FilmRgbImage::from_linear(LinearImage::new(
        image.width,
        image.height,
        rgb,
        image.ir.clone(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neutral_base() -> FilmBase {
        FilmBase::from([1.0, 1.0, 1.0])
    }

    fn convert(image: &LinearImage, base: &FilmBase) -> FilmRgbImage {
        reconstruct(image, base).unwrap()
    }

    #[test]
    fn inverts_each_channel() {
        let img = LinearImage::new(1, 1, vec![0.0, 0.25, 1.0], None).unwrap();
        let out = convert(&img, &neutral_base());
        assert_eq!(out.rgb(), &[1.0, 0.75, 0.0]);
    }

    #[test]
    fn film_base_neutralization_divides_before_inverting() {
        // A base pixel (value == base) normalizes to 1.0, then inverts to 0.0.
        let base = FilmBase::from([0.8, 0.5, 0.4]);
        let img = LinearImage::new(1, 1, vec![0.8, 0.5, 0.4], None).unwrap();
        let out = convert(&img, &base);
        assert_eq!(out.rgb(), &[0.0, 0.0, 0.0]);

        // Half the base transmission → normalized 0.5 → inverted 0.5.
        let img = LinearImage::new(1, 1, vec![0.4, 0.25, 0.2], None).unwrap();
        let out = convert(&img, &base);
        assert_eq!(out.rgb(), &[0.5, 0.5, 0.5]);
    }

    #[test]
    fn does_not_clamp_out_of_range_values() {
        // A value brighter than the base normalizes above 1.0, so the inverted
        // positive goes negative — and must pass through unclamped (HDR / the
        // encoder clamps, not the reconstruction stage).
        let base = FilmBase::from([0.5, 0.5, 0.5]);
        let img = LinearImage::new(1, 1, vec![1.0, 1.0, 1.0], None).unwrap();
        let out = convert(&img, &base);
        // 1.0 / 0.5 = 2.0 -> 1 - 2 = -1.0
        assert_eq!(out.rgb(), &[-1.0, -1.0, -1.0]);
    }

    #[test]
    fn ir_plane_passes_through_unchanged() {
        let ir = vec![0.1, 0.9];
        let img = LinearImage::new(2, 1, vec![0.0; 6], Some(ir.clone())).unwrap();
        let out = convert(&img, &neutral_base());
        assert_eq!(out.ir(), Some(&ir[..]));
    }

    #[test]
    fn no_ir_plane_stays_none() {
        let img = LinearImage::new(1, 1, vec![0.0, 0.0, 0.0], None).unwrap();
        let out = convert(&img, &neutral_base());
        assert!(out.ir().is_none());
    }

    #[test]
    fn preserves_dimensions_over_a_multi_pixel_image() {
        let img = LinearImage::new(2, 3, vec![0.3; 18], None).unwrap();
        let out = convert(&img, &neutral_base());
        assert_eq!((out.width(), out.height()), (2, 3));
        assert_eq!(out.rgb().len(), 18);
        assert!(out.rgb().iter().all(|&v| (v - 0.7).abs() < 1e-6));
    }

    #[test]
    fn parallel_path_preserves_sample_order() {
        // A large image with a distinct value per sample: any pixel/channel
        // reorder in the rayon write would show up here. With a neutral base,
        // output[i] == 1 - rgb[i].
        let n = 100 * 10 * 3;
        let rgb: Vec<f32> = (0..n).map(|i| i as f32 * 1e-4).collect();
        let img = LinearImage::new(100, 10, rgb.clone(), None).unwrap();
        let out = convert(&img, &neutral_base());
        for (i, (&got, &v)) in out.rgb().iter().zip(&rgb).enumerate() {
            let want = 1.0 - v;
            assert!(
                (got - want).abs() < 1e-6,
                "sample {i}: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn degenerate_base_fails_loudly() {
        // A Region/Auto base is runtime-estimated and can land on the dark holder,
        // yielding a zero (or non-finite) channel. The stage must error rather
        // than divide into silent inf/NaN.
        let img = LinearImage::new(1, 1, vec![0.5, 0.5, 0.5], None).unwrap();
        for bad in [
            FilmBase::from([0.5, 0.0, 0.5]),
            FilmBase::from([-0.1, 0.5, 0.5]),
            FilmBase::from([0.5, 0.5, f32::NAN]),
            FilmBase::from([f32::INFINITY, 0.5, 0.5]),
        ] {
            let err = reconstruct(&img, &bad).unwrap_err();
            assert_eq!(err.exit_code(), 1); // NcError::Other
        }
    }
}
