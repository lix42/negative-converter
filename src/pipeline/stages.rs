//! Stage wiring as pure functions — threads film-base → reconstruction →
//! legacy print render → output color transform together for the orchestrator
//! to call.
//!
//! This is the in-memory core of the `convert` pipeline (design-spec §6, stages
//! 2–4). Decode (stage 1) and encode (stage 5) are I/O and stay with the
//! orchestrator (`cli`); everything here is pure `(input, params) -> output` so
//! it composes and unit-tests without touching the filesystem — with one
//! documented exception: [`render`] reads a wall clock to fill [`StageTimings`]
//! for the telemetry record (a report-only channel; the pixels stay
//! deterministic and untouched by the measurement).

use std::time::Instant;

use crate::algo;
use crate::pipeline::color;
use crate::types::{FilmBase, LinearImage, OutputParams, PrintParams, Reconstruction, Result};

/// The in-memory pipeline result the orchestrator hands to the encoder: the
/// output-color-transformed positive image and the ICC blob to embed alongside
/// it.
pub struct Rendered {
    pub image: LinearImage,
    pub icc: Vec<u8>,
    /// Resolved-value diagnostics (e.g. the `Dmax` anchor the curve used) for
    /// the JSON report.
    pub convert: ConvertReport,
    /// Wall-clock per-stage timings measured around the calls in [`render`], for
    /// the telemetry record's `timing_ms` block. Like [`ConvertReport`], a
    /// report-only channel: it is never serialized into the recipe sidecar and
    /// never read back by any stage, so the byte-identical-output determinism
    /// contract is untouched.
    pub timings: StageTimings,
}

/// Per-conversion diagnostics the render surfaces for the JSON report — the
/// reconstruction stage's resolved values plus the legacy print stage's
/// resolved gains. A reporting channel, not a control surface (controls live in
/// the recipe structs).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConvertReport {
    /// The resolved display-white anchor density (`Dmax`) the density curve
    /// used, when one was applied. `None` for `simple` (no curve stage) and for
    /// the exponential curve with `dmax = none`.
    pub dmax: Option<f32>,
    /// The resolved stage-4 white-balance gains `[r, g, b]` the legacy print
    /// render applied — the explicit gains, or the auto-estimated ones
    /// (`print.white_balance = gray-world | percentile`). Reported so a roll can
    /// freeze one frame's estimate into a recipe / `--white-balance` (measure
    /// once, reuse). `None` for `simple` (no print stage).
    pub white_balance: Option<[f32; 3]>,
    /// The resolved regional-balance tone-ramp range `[lo, hi]` (corrected
    /// density), when the density reconstruction applied a shadow/highlight
    /// balance. `None` for `simple` or when both balances are the neutral
    /// `[0, 0, 0]`. Reported so a roll can reuse one frame's measured range via
    /// `--balance-range` (design-spec §9).
    pub balance_range: Option<[f32; 2]>,
}

/// Stages 3–4 in memory: reconstruct the negative into the typed film positive
/// ([`algo::reconstruct`], stage 3 — every path returns a
/// [`FilmRgbImage`](algo::FilmRgbImage)), then run the legacy no-preset print
/// finishing ([`algo::finish_print`], stage 4) back to the plain working-space
/// image the output color transform consumes. Split out from [`render`] so the
/// pre-color-transform pixels are directly testable — the golden fixtures below
/// pin them bit-for-bit against the pre-split converters.
pub(crate) fn reconstruct_and_print(
    image: &LinearImage,
    film_base: &FilmBase,
    reconstruction: &Reconstruction,
    print: &PrintParams,
) -> Result<(LinearImage, ConvertReport)> {
    let (film, recon) = algo::reconstruct(image, film_base, reconstruction)?;
    let (positive, white_balance) = algo::finish_print(film, reconstruction, print)?;
    Ok((
        positive,
        ConvertReport {
            dmax: recon.dmax,
            white_balance,
            balance_range: recon.balance_range,
        },
    ))
}

/// Run pipeline stages 3–4 on a decoded image and an **already-resolved** film
/// base: reconstruct negative → typed film positive → legacy print render, then
/// transform the result into the output color space. Returns the
/// color-transformed image and the ICC blob to embed.
///
/// Film-base estimation (stage 2) is deliberately **not** done here — the
/// orchestrator resolves the base first (via [`film_base::estimate`]) so it can
/// surface the estimate's quality warnings before this fallible render runs (a
/// downstream failure must not swallow the "non-uniform region" warning that
/// explains a bad base). Total in its inputs: any failure (a degenerate film
/// base, an unusable curve anchor, an unreadable custom ICC profile) surfaces as
/// an [`NcError`](crate::types::NcError) with the right exit code, never a
/// silently-wrong image. The IR plane is carried through untouched (Step-1 rule:
/// preserve, don't consume).
///
/// The reconstruction+print and output-color stages are each timed with
/// [`Instant`] pairs (returned as [`StageTimings`] for the telemetry record; the
/// film-base stage is timed by the orchestrator, which owns that estimation).
/// The measurement is the one deliberate impurity here; it never reads back into
/// the pipeline, so the same inputs still produce bit-identical pixels and ICC
/// whether or not telemetry is collected.
pub fn render(
    image: &LinearImage,
    film_base: &FilmBase,
    reconstruction: &Reconstruction,
    print: &PrintParams,
    output_params: &OutputParams,
) -> Result<Rendered> {
    let started = Instant::now();
    let (positive, convert) = reconstruct_and_print(image, film_base, reconstruction, print)?;
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

/// Wall-clock durations of the two in-memory stages [`render`] runs, in
/// milliseconds. Report-only diagnostics the orchestrator folds into the
/// telemetry record alongside its own decode / film-base / encode timings.
#[derive(Clone, Copy, Debug, Default)]
pub struct StageTimings {
    pub algorithm_ms: f64,
    pub color_ms: f64,
}

/// Milliseconds elapsed since `started`, as an `f64` for the telemetry record.
fn ms_since(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::film_base;
    use crate::types::{
        DensityCurve, DensityParams, FilmBaseParams, FilmBaseSource, SigmoidParams,
    };

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

    fn density_default() -> Reconstruction {
        Reconstruction::default()
    }

    fn sigmoid_default() -> Reconstruction {
        Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Sigmoid(SigmoidParams::default()),
        }
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
        // The auto estimate lands on the bright orange base (r > b).
        let base = resolve(&img, FilmBaseSource::Auto);
        assert!(base.r > base.b, "orange base: r > b");
        let out = render(
            &img,
            &base,
            &Reconstruction::Simple,
            &PrintParams::default(),
            &OutputParams::default(),
        )
        .unwrap();
        assert_eq!((out.image.width, out.image.height), (40, 40));
        assert!(!out.icc.is_empty(), "an ICC profile must be produced");
        // Simple has no curve or print stage to report.
        assert_eq!(out.convert, ConvertReport::default());
    }

    #[test]
    fn render_runs_the_density_path_with_explicit_base() {
        let img = synthetic_negative(16, 16);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let out = render(
            &img,
            &base,
            &density_default(),
            &PrintParams::default(),
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
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let out = render(
            &img,
            &base,
            &sigmoid_default(),
            &PrintParams::default(),
            &OutputParams {
                hdr: true,
                ..OutputParams::default()
            },
        )
        .unwrap();
        assert_eq!(out.image.rgb.len(), 16 * 16 * 3);
        // The default Fixed anchor rides back through ConvertReport.
        assert!(out.convert.dmax.is_some_and(f32::is_finite));
    }

    #[test]
    fn render_rejects_a_degenerate_base() {
        // Defense-in-depth: even if a zero-channel base reached `render` (estimate
        // now rejects it at birth), the reconstruction must reject it rather than
        // divide by zero — exit 1, never a silently-wrong image.
        let img = synthetic_negative(20, 20);
        let base = FilmBase::from([0.0, 0.55, 0.42]);
        match render(
            &img,
            &base,
            &density_default(),
            &PrintParams::default(),
            &OutputParams::default(),
        ) {
            Err(e) => assert_eq!(e.exit_code(), 1),
            Ok(_) => panic!("expected a degenerate-base error"),
        }
    }
}

/// Golden fixtures pinning the refactor bit-for-bit against the **pre-split
/// monolithic converters** (`Algorithm::{Simple,Density,Sigmoid}`). Every
/// expected value below was captured by running the pre-refactor code on these
/// exact inputs and printing `f32::to_bits` / hashing the encoded TIFF bytes —
/// so any arithmetic drift in the reconstruction split (a reordered multiply, a
/// changed intermediate, a lost anchor) fails these tests immediately, not in a
/// downstream image diff. This is the task's acceptance gate: the split is a
/// structural refactor, the default pixels are the contract.
#[cfg(test)]
mod golden {
    use super::*;
    use crate::io::encode;
    use crate::types::{
        BalanceRange, DensityCurve, DensityParams, DmaxSource, ExponentialParams, SigmoidParams,
        WbSource,
    };

    /// Five pixels spanning the tonal range plus out-of-range finite values,
    /// with an IR plane (`[0.1, 0.2, 0.3, 0.4, 0.5]`):
    /// near-base shadow, midtone, dense highlight, out-of-range (above base /
    /// negative / zero → epsilon floor), and exactly-the-base.
    fn pixels() -> LinearImage {
        LinearImage::new(
            5,
            1,
            vec![
                0.85, 0.5, 0.38, // near-base shadow
                0.3, 0.18, 0.12, // midtone
                0.02, 0.012, 0.009, // dense highlight
                1.5, -0.2, 0.0, // out-of-range finite
                0.9, 0.55, 0.42, // exactly the base
            ],
            Some(vec![0.1, 0.2, 0.3, 0.4, 0.5]),
        )
        .unwrap()
    }

    fn base() -> FilmBase {
        FilmBase::from([0.9, 0.55, 0.42])
    }

    /// The custom density-reconstruction block the customized cases share.
    fn custom_density() -> DensityParams {
        DensityParams {
            scale: [1.1, 1.0, 0.9],
            offset: [0.05, 0.0, -0.05],
            shadow_balance: [0.05, 0.0, -0.02],
            highlight_balance: [-0.05, 0.01, 0.0],
            balance_range: BalanceRange::Explicit([0.2, 1.6]),
        }
    }

    /// Non-neutral regional balances over otherwise-default density correction
    /// — the block the auto-WB/auto-range composition goldens were captured
    /// with (unlike [`custom_density`], scale/offset stay at their defaults).
    fn balanced_density() -> DensityParams {
        DensityParams {
            shadow_balance: [0.05, 0.0, -0.02],
            highlight_balance: [-0.05, 0.01, 0.0],
            balance_range: BalanceRange::Explicit([0.2, 1.6]),
            ..DensityParams::default()
        }
    }

    /// The custom print block the customized cases share.
    fn custom_print() -> PrintParams {
        PrintParams {
            print_exposure: -1.0,
            black_point: 0.01,
            white_balance: WbSource::Explicit([1.0, 1.05, 1.1]),
            highlight_compress: 0.2,
        }
    }

    /// Assert the pre-color-transform pixels (and the resolved diagnostics)
    /// match the captured pre-refactor bits exactly.
    fn assert_golden(
        reconstruction: Reconstruction,
        print: PrintParams,
        expected_rgb_bits: &[u32],
        expected_dmax_bits: Option<u32>,
        expected_wb_bits: Option<[u32; 3]>,
        expected_range_bits: Option<[u32; 2]>,
    ) {
        let (out, report) =
            reconstruct_and_print(&pixels(), &base(), &reconstruction, &print).unwrap();
        let got: Vec<u32> = out.rgb.iter().map(|v| v.to_bits()).collect();
        assert_eq!(got, expected_rgb_bits, "pixel bits drifted");
        assert_eq!(report.dmax.map(f32::to_bits), expected_dmax_bits, "dmax");
        assert_eq!(
            report.white_balance.map(|w| w.map(f32::to_bits)),
            expected_wb_bits,
            "white balance"
        );
        assert_eq!(
            report.balance_range.map(|r| r.map(f32::to_bits)),
            expected_range_bits,
            "balance range"
        );
        // IR rides through untouched on every path.
        assert_eq!(out.ir.as_deref(), Some(&[0.1f32, 0.2, 0.3, 0.4, 0.5][..]));
    }

    const UNIT_WB: [u32; 3] = [0x3f800000; 3]; // [1.0, 1.0, 1.0]

    #[test]
    fn golden_density_exponential_default_is_bit_identical() {
        // THE default path (density reconstruction, exponential curve, fixed
        // nominal anchor, neutral print) — the task's headline guarantee.
        assert_golden(
            Reconstruction::default(),
            PrintParams::default(),
            &[
                0x3c2d7a46, 0x3c343958, 0x3c35161a, 0x3cf5c28f, 0x3cfa4fa3, 0x3d0f5c2a, 0x3ee66668,
                0x3eeaaaab, 0x3eeeeef1, 0x3bc49ba7, 0x45abdfff, 0x45833ffb, 0x3c23d70a, 0x3c23d70a,
                0x3c23d70a,
            ],
            Some(0x40000000), // NOMINAL_DMAX = 2.0
            Some(UNIT_WB),
            None,
        );
    }

    #[test]
    fn golden_density_exponential_customized_is_bit_identical() {
        // Every density knob non-default at once: scale/offset, regional balance
        // with an explicit range, gamma 1.4, an explicit anchor, and a full
        // custom print (exposure, black point, gains, soft-clip).
        assert_golden(
            Reconstruction::Density {
                density: custom_density(),
                curve: DensityCurve::Exponential(ExponentialParams {
                    gamma: 1.4,
                    dmax: DmaxSource::Explicit(1.8),
                }),
            },
            custom_print(),
            &[
                0xbbfd1875, 0xbc0627d2, 0xbc0b3486, 0x3a6a9290, 0xbb1d24fc, 0xbb670fb4, 0x3f055214,
                0x3eac5540, 0x3e2d3fe0, 0xbc18931f, 0x3f99999a, 0x3f99999a, 0xbc01b0a7, 0xbc09dd14,
                0xbc0e1fb5,
            ],
            Some(0x3fe66666),                           // 1.8
            Some([0x3f800000, 0x3f866666, 0x3f8ccccd]), // [1.0, 1.05, 1.1]
            Some([0x3e4ccccd, 0x3fcccccd]),             // [0.2, 1.6]
        );
    }

    #[test]
    fn golden_density_exponential_no_anchor_is_bit_identical() {
        // `dmax = none` — the scene-referred unity placement (base → 1.0).
        assert_golden(
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Exponential(ExponentialParams {
                    gamma: 1.0,
                    dmax: DmaxSource::None,
                }),
            },
            PrintParams::default(),
            &[
                0x3f878787, 0x3f8ccccd, 0x3f8d7943, 0x403fffff, 0x40438e38, 0x40600000, 0x42340001,
                0x42375556, 0x423aaaac, 0x3f199999, 0x490646ff, 0x48cd13f9, 0x3f800000, 0x3f800000,
                0x3f800000,
            ],
            None,
            Some(UNIT_WB),
            None,
        );
    }

    #[test]
    fn golden_density_exponential_auto_anchor_is_bit_identical() {
        // `dmax = auto` — the demoted per-frame percentile measurement.
        assert_golden(
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Exponential(ExponentialParams {
                    gamma: 1.0,
                    dmax: DmaxSource::Auto,
                }),
            },
            PrintParams::default(),
            &[
                0x3601318e, 0x360637c0, 0x3606dc23, 0x36b70634, 0x36ba69df, 0x36d58734, 0x38ab95cf,
                0x38aec33f, 0x38b1f0b1, 0x35926b56, 0x3f800000, 0x3f437da7, 0x35f40842, 0x35f40842,
                0x35f40842,
            ],
            Some(1085780237), // the measured per-frame anchor, captured verbatim
            Some(UNIT_WB),
            None,
        );
    }

    fn sigmoid_default_config() -> Reconstruction {
        Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Sigmoid(SigmoidParams::default()),
        }
    }

    #[test]
    fn golden_sigmoid_default_is_numerically_exact() {
        // The shipped sigmoid equation, default knobs (contrast 1, toe/shoulder
        // 0.2, fixed anchor) — the refactor changes ownership and schema, not
        // one bit of the numeric behavior.
        assert_golden(
            sigmoid_default_config(),
            PrintParams::default(),
            &[
                0x3c420db4, 0x3c468086, 0x3c471719, 0x3cf5f640, 0x3cfa7fad, 0x3d0f6a21, 0x3ee58f1a,
                0x3ee9ba90, 0x3eede3b3, 0x3c264ff3, 0x3f800000, 0x3f800000, 0x3c3c33e8, 0x3c3c33e8,
                0x3c3c33e8,
            ],
            Some(0x40000000),
            Some(UNIT_WB),
            None,
        );
    }

    #[test]
    fn golden_sigmoid_customized_is_numerically_exact() {
        // Custom knees + explicit anchor + the custom density/print blocks.
        assert_golden(
            Reconstruction::Density {
                density: custom_density(),
                curve: DensityCurve::Sigmoid(SigmoidParams {
                    contrast: 1.7,
                    toe: 0.1,
                    shoulder: 0.4,
                    dmax: DmaxSource::Explicit(1.5),
                }),
            },
            custom_print(),
            &[
                0xbbfb9f9b, 0xbc06d065, 0xbc09c550, 0x3bb51ff2, 0xb8997d80, 0xbafaba00, 0x3ef67b34,
                0x3ef5d7ac, 0x3ebb17ec, 0xbc0cc06c, 0x3f03d70a, 0x3f0a3d71, 0xbc019f5f, 0xbc09db81,
                0xbc0a489a,
            ],
            Some(0x3fc00000), // 1.5
            Some([0x3f800000, 0x3f866666, 0x3f8ccccd]),
            Some([0x3e4ccccd, 0x3fcccccd]),
        );
    }

    #[test]
    fn golden_simple_inversion_is_bit_identical() {
        // The pre-split simple converter with its (identity) default WB/clip —
        // the pure `1 − scan/Dmin` inversion must reproduce it exactly.
        assert_golden(
            Reconstruction::Simple,
            PrintParams::default(),
            &[
                0x3d638e30, 0x3dba2e90, 0x3dc30c30, 0x3f2aaaaa, 0x3f2c37da, 0x3f36db6e, 0x3f7a4fa5,
                0x3f7a6a20, 0x3f7a83a8, 0xbf2aaaac, 0x3fae8ba3, 0x3f800000, 0x00000000, 0x00000000,
                0x00000000,
            ],
            None,
            None,
            None,
        );
    }

    #[test]
    fn golden_auto_wb_estimation_is_bit_identical() {
        // The auto-WB path moved from "tone a strided density sample" to "stride
        // the film positive" — bit-identical because a per-sample map commutes
        // with striding. Pin both estimators' gains AND output pixels.
        assert_golden(
            Reconstruction::default(),
            PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
            &[
                0x43016967, 0x3c343958, 0x3c6d2311, 0x43b75553, 0x3cfa4fa3, 0x3d3bbbc3, 0x45abdfff,
                0x3eeaaaab, 0x3f1c71cd, 0x4292aaaa, 0x45abdfff, 0x45abdfff, 0x42f471c3, 0x3c23d70a,
                0x3c568d6f,
            ],
            Some(0x40000000),
            Some([1178532065, 1065353216, 1067949695]),
            None,
        );
        assert_golden(
            Reconstruction::default(),
            PrintParams {
                white_balance: WbSource::GrayWorld,
                ..PrintParams::default()
            },
            &[
                0x42e5eed9, 0x3c343958, 0x3c6d2125, 0x43a2de85, 0x3cfa4fa3, 0x3d3bba3d, 0x4598b09e,
                0x3eeaaaab, 0x3f1c7088, 0x42824b9f, 0x45abdfff, 0x45abde9a, 0x42d928b2, 0x3c23d70a,
                0x3c568bb2,
            ],
            Some(0x40000000),
            Some([1177135051, 1065353216, 1067949347]),
            None,
        );
    }

    #[test]
    fn golden_auto_wb_with_regional_balance_is_bit_identical() {
        // Auto-WB estimated on the POST-regional-balance positive (the ordering
        // contract) — the two features composed, with the exponential default
        // curve. Bits captured from the proven-bit-identical pipeline.
        assert_golden(
            Reconstruction::Density {
                density: balanced_density(),
                curve: DensityCurve::default(),
            },
            PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
            &[
                0x4326b6f7, 0x3c343958, 0x3c67bd38, 0x43e5c3a8, 0x3cfb0056, 0x3d38792c, 0x45afe0e9,
                0x3ef021fc, 0x3f2016b4, 0x42961542, 0x45afe0e9, 0x45afe0e9, 0x431d73e7, 0x3c23d70a,
                0x3c51ab35,
            ],
            Some(0x40000000),
            Some([1180386296, 1065353216, 1068205576]),
            Some([0x3e4ccccd, 0x3fcccccd]), // the explicit [0.2, 1.6] echoed
        );
    }

    #[test]
    fn golden_auto_wb_with_sigmoid_curve_is_bit_identical() {
        // Auto-WB under the sigmoid curve: the estimator samples the S-curve's
        // film positive (not the exponential one), and the gains apply through
        // the same stage-4 slot.
        assert_golden(
            sigmoid_default_config(),
            PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
            &[
                0x3cd867ac, 0x3c468086, 0x3c471719, 0x3d892568, 0x3cfa7fad, 0x3d0f6a21, 0x3f800000,
                0x3ee9ba90, 0x3eede3b3, 0x3cb977ed, 0x3f800000, 0x3f800000, 0x3cd1e15b, 0x3c3c33e8,
                0x3c3c33e8,
            ],
            Some(0x40000000),
            Some([1074708039, 1065353216, 1065353216]),
            None,
        );
    }

    #[test]
    fn golden_auto_measured_balance_range_is_bit_identical() {
        // The default `BalanceRange::Auto` with non-zero balances: the ramp
        // anchors are MEASURED from this frame's tone distribution (the other
        // regional-balance goldens use an explicit range), and both the measured
        // `[lo, hi]` and the resulting pixels are pinned.
        assert_golden(
            Reconstruction::Density {
                density: DensityParams {
                    balance_range: BalanceRange::Auto,
                    ..balanced_density()
                },
                curve: DensityCurve::default(),
            },
            PrintParams::default(),
            &[
                0x3c42a1d5, 0x3c3439a6, 0x3c2cf03a, 0x3d084c85, 0x3cfa994a, 0x3d093901, 0x3eea9e5a,
                0x3eecf423, 0x3ee8a619, 0x3baf3a23, 0x45afe0e9, 0x45833ffb, 0x3c37d4dc, 0x3c23d70a,
                0x3c1c774b,
            ],
            Some(0x40000000),
            Some(UNIT_WB),
            Some([0, 1080930529]), // the frame-measured [lo, hi], captured verbatim
        );
    }

    // --- legacy no-preset TIFF regression -----------------------------------

    /// Deterministic synthetic negative mirroring the real-scan layout, with an
    /// IR plane — the whole-pipeline regression input.
    fn synthetic(w: u32, h: u32) -> LinearImage {
        let holder = [0.01, 0.01, 0.01];
        let rebate = [0.9, 0.55, 0.42];
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let depth = x.min(y).min(w - 1 - x).min(h - 1 - y);
                if depth < 1 {
                    rgb.extend_from_slice(&holder);
                } else if depth < 3 {
                    rgb.extend_from_slice(&rebate);
                } else {
                    let t = (x + y) as f32 / (w + h) as f32;
                    rgb.extend_from_slice(&[0.1 + 0.3 * t, 0.08 + 0.2 * t, 0.05 + 0.15 * t]);
                }
            }
        }
        let ir: Vec<f32> = (0..w * h).map(|i| (i % 7) as f32 / 7.0).collect();
        LinearImage::new(w, h, rgb, Some(ir)).unwrap()
    }

    /// FNV-1a over raw bytes (the same stable hash `telemetry::params_hash`
    /// uses for strings) — enough to pin an encoded file byte-for-byte.
    fn fnv(bytes: &[u8]) -> String {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let mut h = OFFSET;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(PRIME);
        }
        format!("{h:016x}")
    }

    /// Render + encode the synthetic negative and return the output file's
    /// FNV-1a hash. The ICC profile is deterministic (its `dateTimeNumber` is
    /// zeroed — see `pipeline::color`), so the whole file hashes stably.
    fn tiff_hash(label: &str, reconstruction: &Reconstruction, output: &OutputParams) -> String {
        let img = synthetic(16, 16);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let rendered =
            render(&img, &base, reconstruction, &PrintParams::default(), output).unwrap();
        let dir = std::env::temp_dir().join("nc-golden-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{label}.tiff"));
        let _ = encode::encode(&rendered.image, output, Some(&rendered.icc), &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        fnv(&bytes)
    }

    #[test]
    fn golden_no_preset_tiff_bytes_are_unchanged() {
        // The legacy no-preset TIFF regression: the full pipeline (reconstruct →
        // legacy print → output color transform → u16/f32 encode + deterministic
        // ICC) produces byte-identical files to the pre-refactor binary. Hashes
        // captured from the pre-split code on the same synthetic input. This is
        // also the proof this task claims no `pipeline_version` bump: default
        // pixels did not change.
        let sdr = OutputParams::default();
        assert_eq!(
            tiff_hash("density-default-u16", &Reconstruction::default(), &sdr),
            "60944dbb1ea2600e"
        );
        assert_eq!(
            tiff_hash("simple-default-u16", &Reconstruction::Simple, &sdr),
            "3afd68a372eef92b"
        );
        assert_eq!(
            tiff_hash("sigmoid-default-u16", &sigmoid_default_config(), &sdr),
            "28a5827801675e34"
        );
        let hdr = OutputParams {
            hdr: true,
            ..OutputParams::default()
        };
        assert_eq!(
            tiff_hash("density-default-f32", &Reconstruction::default(), &hdr),
            "003fee67f70848b8"
        );
    }
}
