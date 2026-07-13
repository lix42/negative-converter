//! `simple` — channel-inversion baseline (debug / B&W).
//!
//! A literal per-channel inversion: neutralize the film base, invert, apply
//! white balance, then set black/white points. Cheap and predictable — the
//! trustworthy reference against which the `density` algorithm is judged. It
//! deliberately does **no** density-domain math (log/exp, orange-mask modeling);
//! that is what distinguishes `density`.

use rayon::prelude::*;

use crate::algo::Converter;
use crate::types::{FilmBase, LinearImage, NcError, Result, SimpleParams};

/// Channel-inversion converter configured by [`SimpleParams`].
pub struct Simple {
    pub params: SimpleParams,
}

impl Converter for Simple {
    /// Per channel, in the linear working space:
    ///
    /// 1. neutralize the film base — divide by `base` transmission, so an
    ///    unexposed base pixel maps to 1.0 (a neutral base of `[1,1,1]` is inert,
    ///    leaving a pure `1 - v` inversion);
    /// 2. invert — `positive = 1 - normalized`;
    /// 3. apply the per-channel white-balance gain;
    /// 4. set black/white points — linearly remap `[clip_low, clip_high]` onto
    ///    `[0, 1]`.
    ///
    /// Output is left **unclamped** (values may fall outside `[0, 1]`); range
    /// clamping happens only at the u16 encode step. The IR plane is carried
    /// through untouched.
    ///
    /// The white-balance gains and clip points are CLI-validated
    /// (`cli::validate` guarantees positive, finite gains and
    /// `clip_low < clip_high`, so the clip remap's `span` is never zero). The
    /// film `base`, by contrast, is only CLI-validated for an explicit
    /// `--film-base`; a `Region`/`Auto` base is estimated from pixels at runtime
    /// (`pipeline::film_base::estimate`) and carries no positivity guarantee — a
    /// region over the dark holder can yield a zero channel. This stage is the
    /// first to divide by it, so it guards the base explicitly and fails loudly
    /// rather than emit silent `inf`/`NaN`.
    fn convert(&self, image: &LinearImage, base: &FilmBase) -> Result<LinearImage> {
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

        let wb = self.params.invert_white_balance;
        let low = self.params.clip_low;
        let span = self.params.clip_high - self.params.clip_low;

        let per_channel = |value: f32, c: usize| -> f32 {
            let positive = (1.0 - value / base[c]) * wb[c];
            (positive - low) / span
        };

        // Per-pixel independent; writing through zipped position-matched chunks
        // keeps the result deterministic without per-thread collect buffers.
        // `rgb.len()` is a multiple of 3 (a `LinearImage` invariant), so every
        // chunk is exactly one RGB triple.
        let mut rgb = vec![0.0f32; image.rgb.len()];
        rgb.par_chunks_exact_mut(3)
            .zip(image.rgb.par_chunks_exact(3))
            .for_each(|(out, px)| {
                for c in 0..3 {
                    out[c] = per_channel(px[c], c);
                }
            });

        LinearImage::new(image.width, image.height, rgb, image.ir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neutral base + identity params + full `[0, 1]` clip range: a plain
    /// `v -> 1 - v` inversion, per channel.
    fn identity_simple() -> Simple {
        Simple {
            params: SimpleParams::default(),
        }
    }

    fn neutral_base() -> FilmBase {
        FilmBase::from([1.0, 1.0, 1.0])
    }

    fn convert(simple: &Simple, image: &LinearImage, base: &FilmBase) -> LinearImage {
        simple.convert(image, base).unwrap()
    }

    #[test]
    fn inverts_each_channel_before_wb_and_clip() {
        let img = LinearImage::new(1, 1, vec![0.0, 0.25, 1.0], None).unwrap();
        let out = convert(&identity_simple(), &img, &neutral_base());
        assert_eq!(out.rgb, vec![1.0, 0.75, 0.0]);
    }

    #[test]
    fn film_base_neutralization_divides_before_inverting() {
        // A base pixel (value == base) normalizes to 1.0, then inverts to 0.0.
        let base = FilmBase::from([0.8, 0.5, 0.4]);
        let img = LinearImage::new(1, 1, vec![0.8, 0.5, 0.4], None).unwrap();
        let out = convert(&identity_simple(), &img, &base);
        assert_eq!(out.rgb, vec![0.0, 0.0, 0.0]);

        // Half the base transmission → normalized 0.5 → inverted 0.5.
        let img = LinearImage::new(1, 1, vec![0.4, 0.25, 0.2], None).unwrap();
        let out = convert(&identity_simple(), &img, &base);
        assert_eq!(out.rgb, vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn white_balance_scales_the_inverted_channels() {
        let simple = Simple {
            params: SimpleParams {
                invert_white_balance: [2.0, 1.0, 0.5],
                ..SimpleParams::default()
            },
        };
        // invert 0.75 -> 0.25, then per-channel gain.
        let img = LinearImage::new(1, 1, vec![0.75, 0.75, 0.75], None).unwrap();
        let out = convert(&simple, &img, &neutral_base());
        assert_eq!(out.rgb, vec![0.5, 0.25, 0.125]);
    }

    #[test]
    fn clip_points_remap_endpoints_to_zero_and_one() {
        let simple = Simple {
            params: SimpleParams {
                clip_low: 0.2,
                clip_high: 0.8,
                ..SimpleParams::default()
            },
        };
        // input 0.8 -> invert 0.2 (== clip_low)  -> 0.0
        // input 0.2 -> invert 0.8 (== clip_high) -> 1.0
        // input 0.5 -> invert 0.5 (midpoint)     -> 0.5
        let img = LinearImage::new(
            3,
            1,
            vec![0.8, 0.8, 0.8, 0.2, 0.2, 0.2, 0.5, 0.5, 0.5],
            None,
        )
        .unwrap();
        let out = convert(&simple, &img, &neutral_base());
        // Tolerance, not exact equality: f32 `1 - 0.8 - 0.2` is a few ulps off 0.
        let expect = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.5, 0.5, 0.5];
        for (got, want) in out.rgb.iter().zip(expect) {
            assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
        }
    }

    #[test]
    fn does_not_clamp_out_of_range_values() {
        // A value brighter than the base normalizes above 1.0, so the inverted
        // positive goes negative — and must pass through unclamped (HDR / the
        // encoder clamps, not the algo stage).
        let base = FilmBase::from([0.5, 0.5, 0.5]);
        let img = LinearImage::new(1, 1, vec![1.0, 1.0, 1.0], None).unwrap();
        let out = convert(&identity_simple(), &img, &base);
        // 1.0 / 0.5 = 2.0 -> 1 - 2 = -1.0
        assert_eq!(out.rgb, vec![-1.0, -1.0, -1.0]);
    }

    #[test]
    fn ir_plane_passes_through_unchanged() {
        let ir = vec![0.1, 0.9];
        let img = LinearImage::new(2, 1, vec![0.0; 6], Some(ir.clone())).unwrap();
        let out = convert(&identity_simple(), &img, &neutral_base());
        assert_eq!(out.ir, Some(ir));
    }

    #[test]
    fn no_ir_plane_stays_none() {
        let img = LinearImage::new(1, 1, vec![0.0, 0.0, 0.0], None).unwrap();
        let out = convert(&identity_simple(), &img, &neutral_base());
        assert!(out.ir.is_none());
    }

    #[test]
    fn preserves_dimensions_over_a_multi_pixel_image() {
        let img = LinearImage::new(2, 3, vec![0.3; 18], None).unwrap();
        let out = convert(&identity_simple(), &img, &neutral_base());
        assert_eq!((out.width, out.height), (2, 3));
        assert_eq!(out.rgb.len(), 18);
        assert!(out.rgb.iter().all(|&v| (v - 0.7).abs() < 1e-6));
    }

    #[test]
    fn applies_base_then_invert_then_wb_then_clip_in_order() {
        // All four steps active with distinct per-channel values, so a reordering
        // (e.g. clip before WB) would change the result. Formula:
        // ((1 - v/base) * wb - clip_low) / (clip_high - clip_low).
        let simple = Simple {
            params: SimpleParams {
                invert_white_balance: [2.0, 1.0, 0.5],
                clip_low: 0.1,
                clip_high: 0.6,
            },
        };
        let base = FilmBase::from([0.8, 0.5, 0.4]);
        let img = LinearImage::new(1, 1, vec![0.4, 0.25, 0.2], None).unwrap();
        let out = convert(&simple, &img, &base);
        // ch0: 1-0.4/0.8=0.5 -> *2.0=1.0  -> (1.0-0.1)/0.5 = 1.8
        // ch1: 1-0.25/0.5=0.5 -> *1.0=0.5 -> (0.5-0.1)/0.5 = 0.8
        // ch2: 1-0.2/0.4=0.5 -> *0.5=0.25 -> (0.25-0.1)/0.5 = 0.3
        let expect = [1.8, 0.8, 0.3];
        for (got, want) in out.rgb.iter().zip(expect) {
            assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
        }
    }

    #[test]
    fn parallel_path_preserves_sample_order() {
        // A large image with a distinct value per sample: any pixel/channel
        // reorder in the rayon collect would show up here. With base/wb/clip all
        // neutral, output[i] == 1 - rgb[i].
        let n = 100 * 10 * 3;
        let rgb: Vec<f32> = (0..n).map(|i| i as f32 * 1e-4).collect();
        let img = LinearImage::new(100, 10, rgb.clone(), None).unwrap();
        let out = convert(&identity_simple(), &img, &neutral_base());
        for (i, (&got, &v)) in out.rgb.iter().zip(&rgb).enumerate() {
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
            let err = identity_simple().convert(&img, &bad).unwrap_err();
            assert_eq!(err.exit_code(), 1); // NcError::Other
        }
    }
}
