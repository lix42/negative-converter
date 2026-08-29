//! Stage wiring as pure functions — threads film-base → reconstruction → the
//! selected output branch together for the orchestrator to call.
//!
//! This is the in-memory core of the `convert` pipeline (design-spec §6, stages
//! 3–5b). [`render`] owns the legacy/film-master branches;
//! [`render_display_source`] is the CLI-reachable shared display source for the
//! explicit `ultra-hdr-v1` path, whose SDR/HDR/gain-map rendering and packaging
//! the CLI orchestrates afterward. Film-base estimation (stage 2) is the
//! orchestrator's, and
//! decode (stage 1) and encode (the final stage) are I/O and stay with
//! the orchestrator (`cli`); everything here is pure `(input, params) -> output`
//! so it composes and unit-tests without touching the filesystem — with one
//! documented exception: [`render`] reads a wall clock to fill [`StageTimings`]
//! for the telemetry record (a report-only channel; the pixels stay
//! deterministic and untouched by the measurement).
//!
//! The resolved [`OutputPreset`] routes through these stage entrypoints:
//!
//! - **`legacy`** (no preset, the default) — the frozen transitional path
//!   `reconstruct → finish_print → color::to_output`: the print controls run
//!   *before* the working→output ICC transform, exactly as they did before
//!   presets existed. `golden` (below, `#[cfg(test)]`) pins its
//!   **pre-colour-transform** pixels bit-for-bit by calling
//!   [`reconstruct_and_print`] directly, and
//!   `legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence` pins
//!   that this branch of [`render`] is still exactly that sequence — the boundary
//!   `golden` cannot see, because it never crosses the preset `match`.
//! - **`film-master`** — `reconstruct → map_nc_film_rgb_v1 → render_split::film_master`:
//!   the mapped unclamped linear ACEScg buffer is encoded directly with the
//!   ACEScg ICC attached and **no** colour transform, print control, or display
//!   rendering. Running `color::to_output` here would re-apply the
//!   Rec.709→ACEScg matrix on values that already crossed it, so the master
//!   deliberately bypasses that stage and only fetches the profile blob.
//! - **`ultra-hdr-v1`** — `render_display_source` reconstructs once, maps to
//!   ACEScg, and applies the shared print controls once; the orchestrator then
//!   feeds that source to both display renderers and the legacy gain-map encoder.

use std::time::Instant;

use crate::algo;
use crate::pipeline::color::{self, OutputSpace};
use crate::pipeline::{render_split, sdr, working_space};
use crate::types::{
    FilmBase, LinearImage, OutputParams, OutputPreset, PrintParams, Reconstruction, Result,
};

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

/// The shared display source and diagnostics used to build a gain-map output.
pub struct DisplaySource {
    pub shared: render_split::SharedDisplaySource,
    pub convert: ConvertReport,
    pub timings: StageTimings,
}

/// Per-conversion diagnostics the render surfaces for the JSON report — the
/// reconstruction stage's resolved values plus the legacy print stage's
/// resolved gains. A reporting channel, not a control surface (controls live in
/// the recipe structs).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ConvertReport {
    /// The resolved **reference** density (`curve.dmax`) — the roll calibration, and the
    /// value to freeze back into a recipe. `None` for `simple` (no curve stage) and for
    /// the exponential curve with `dmax = none`.
    ///
    /// Not necessarily the density that rendered to `1.0`: the sigmoid derives its anchor
    /// from this reference via `AnchorPlacement`. See [`Self::curve_anchor`].
    pub dmax: Option<f32>,
    /// The **derived** anchor the curve used — the corrected density that rendered to
    /// `1.0`, hence the black floor at `10^(−contrast·curve_anchor)`. Equal to
    /// [`Self::dmax`] for the exponential curve and the sigmoid's `white-at-dmax`
    /// placement; larger under the default mid-grey placement.
    pub curve_anchor: Option<f32>,
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
            curve_anchor: recon.curve_anchor,
            white_balance,
            balance_range: recon.balance_range,
        },
    ))
}

/// Run the in-memory pipeline on a decoded image and an **already-resolved** film
/// base, taking whichever branch `output_params.preset` selects: `legacy`
/// (reconstruct → print render → working→output ICC transform) or `film-master`
/// (reconstruct → NC film RGB v1 → the unwrapped ACEScg buffer, no transform).
/// Returns the image to encode and the ICC blob to embed alongside it.
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
    match output_params.preset {
        // `custom` is the same legacy branch, explicitly chosen rather than
        // inherited from the default — same bytes, different provenance.
        OutputPreset::Legacy | OutputPreset::Custom => {
            render_legacy(image, film_base, reconstruction, print, output_params)
        }
        OutputPreset::FilmMaster => render_film_master(image, film_base, reconstruction),
        // Every display preset renders from the shared source instead, so that
        // reconstruction and the print controls resolve exactly once for whichever
        // rendition(s) the preset needs.
        OutputPreset::UltraHdrV1
        | OutputPreset::GainMapHdr
        | OutputPreset::HdrPq
        | OutputPreset::HdrHlg
        | OutputPreset::HdrLinearTiff
        | OutputPreset::HdrPqTiff
        | OutputPreset::HdrHlgTiff
        | OutputPreset::DisplayP3
        | OutputPreset::Compatibility => Err(crate::types::NcError::Other(format!(
            "`{}` must use stages::render_display_source",
            output_params.preset.name()
        ))),
    }
}

/// The `display-p3` / `compatibility` render: the shared display source, one SDR
/// rendition in the requested gamut, and the output-encoded pixels plus their ICC.
///
/// Returns the same [`Rendered`] the legacy branch does, which is deliberate — an
/// SDR preset's product *is* a rendered image and a profile, so it reuses the
/// existing 16-bit TIFF encode path rather than introducing a container of its own.
/// What differs from `legacy` is everything upstream: this crosses the ACEScg
/// boundary and runs the shared print controls plus `pipeline::sdr`'s
/// reference-white-preserving shoulder and gamut mapping, where `legacy` applies
/// the print controls before a plain ICC transform.
pub fn render_sdr_preset(
    image: &LinearImage,
    film_base: &FilmBase,
    reconstruction: &Reconstruction,
    print: &PrintParams,
    gamut: sdr::SdrGamut,
) -> Result<Rendered> {
    let source = render_display_source(image, film_base, reconstruction, print)?;
    let mut timings = source.timings;
    let started = Instant::now();
    let rendered = sdr::render(&source.shared, gamut, print.highlight_compress)?;
    // The transform into the output space is colour work, like the gain-map and
    // Rec.2100 branches; only the container write counts as encode.
    let (image, icc, _metadata) = color::encode_rendered_sdr(rendered)?;
    timings.color_ms += ms_since(started);
    Ok(Rendered {
        image,
        icc,
        convert: source.convert,
        timings,
    })
}

/// Reconstruct once, cross the NC film RGB v1 boundary, then resolve and apply
/// the shared print controls exactly once for both gain-map renditions.
pub fn render_display_source(
    image: &LinearImage,
    film_base: &FilmBase,
    reconstruction: &Reconstruction,
    print: &PrintParams,
) -> Result<DisplaySource> {
    let started = Instant::now();
    let (film, recon) = algo::reconstruct(image, film_base, reconstruction)?;
    let shared = render_split::display_source(working_space::map_nc_film_rgb_v1(film), print)?;
    let algorithm_ms = ms_since(started);

    Ok(DisplaySource {
        convert: ConvertReport {
            dmax: recon.dmax,
            curve_anchor: recon.curve_anchor,
            white_balance: Some(shared.controls.white_balance()),
            balance_range: recon.balance_range,
        },
        shared,
        timings: StageTimings {
            algorithm_ms,
            color_ms: 0.0,
        },
    })
}

/// The frozen legacy no-preset path: reconstruct → print render → working→output
/// ICC transform — the pre-preset contract. `golden` pins the
/// [`reconstruct_and_print`] half's pixels bit-for-bit, and
/// `legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence` pins that
/// this function is still that sequence composed with `color::to_output`.
fn render_legacy(
    image: &LinearImage,
    film_base: &FilmBase,
    reconstruction: &Reconstruction,
    print: &PrintParams,
    output_params: &OutputParams,
) -> Result<Rendered> {
    let started = Instant::now();
    let (positive, convert) = reconstruct_and_print(image, film_base, reconstruction, print)?;
    let algorithm_ms = ms_since(started);

    // No copy here (`io/memory-preflight`): the pre-transform positive has no
    // consumer, so it is *moved* into `to_output`, which transforms those very
    // buffers and hands them back — one full-frame RGB buffer and one full-frame IR
    // plane less at peak than the clone this used to make.
    let started = Instant::now();
    let (image, icc) = color::to_output(positive, output_params)?;
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

/// The `film-master` branch: reconstruct → NC film RGB v1 → encode directly.
///
/// No print controls are consumed (they are validated all-default at the CLI
/// boundary before this runs — a requested adjustment is a loud error, never
/// silently dropped), no `color::to_output` transform runs (the pixels are
/// *already* linear ACEScg; transforming again would double-apply the matrix),
/// and nothing is clamped. The ICC blob is the ACEScg profile the values are
/// genuinely in, fetched without building a transform.
///
/// [`ConvertReport::white_balance`] stays `None` here by construction: no
/// white-balance stage ran, and reporting resolved gains for a master that
/// applied none would be a false provenance claim. The reconstruction's own
/// resolved diagnostics (`dmax`, `balance_range`) *are* reported — they are part
/// of what the master contains.
fn render_film_master(
    image: &LinearImage,
    film_base: &FilmBase,
    reconstruction: &Reconstruction,
) -> Result<Rendered> {
    let started = Instant::now();
    let (film, recon) = algo::reconstruct(image, film_base, reconstruction)?;
    let master = render_split::film_master(working_space::map_nc_film_rgb_v1(film));
    let algorithm_ms = ms_since(started);

    let started = Instant::now();
    // Profile only — no transform. `icc_profile` builds the same ACEScg profile
    // `to_output` would embed, so the tag matches the pixels exactly.
    let icc = color::icc_profile(&OutputSpace::AcesCg)?;
    let color_ms = ms_since(started);

    Ok(Rendered {
        image: master,
        icc,
        convert: ConvertReport {
            dmax: recon.dmax,
            curve_anchor: recon.curve_anchor,
            white_balance: None,
            balance_range: recon.balance_range,
        },
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
    use crate::types::OutDepth;

    /// `OutputParams::default()` pinned to the **legacy** preset.
    ///
    /// The product default is `gain-map-hdr` since the `output/presets` migration,
    /// and `render` refuses it by design (display presets go through
    /// `render_display_source`). Any test calling `render` therefore has to say
    /// which TIFF branch it means.
    fn legacy_output() -> OutputParams {
        OutputParams {
            preset: OutputPreset::Legacy,
            ..OutputParams::default()
        }
    }
    use super::*;
    use crate::pipeline::film_base;
    use crate::types::{
        DensityCurve, DensityParams, DmaxSource, ExponentialParams, FilmBaseSource, SigmoidParams,
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

    /// A reconstruction that lands this module's synthetic pixels in the **middle**
    /// of the range, where a transfer function visibly changes a value.
    ///
    /// The default (sigmoid at the nominal 1.3 anchor) renders these vectors to
    /// ~0.999, and near white the sRGB transfer and the linear value converge — so
    /// a test asking "was the transfer applied?" stops being able to tell. That is
    /// a property of the fixture, not of the branch under test, so the fixture is
    /// pinned rather than the assertion loosened.
    fn midtone_reconstruction() -> Reconstruction {
        Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Exponential(ExponentialParams {
                gamma: 1.0,
                dmax: DmaxSource::Explicit(2.0),
                anchor: crate::types::AnchorPlacement::WhiteAtDmax,
            }),
        }
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
        film_base::estimate(img, &source, crate::types::FilmType::Unknown)
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
            &legacy_output(),
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
                preset: OutputPreset::Legacy,
                depth: OutDepth::F32,
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
                preset: OutputPreset::Legacy,
                depth: OutDepth::F32,
                ..OutputParams::default()
            },
        )
        .unwrap();
        assert_eq!(out.image.rgb.len(), 16 * 16 * 3);
        // The default Fixed anchor rides back through ConvertReport.
        assert!(out.convert.dmax.is_some_and(f32::is_finite));
    }

    #[test]
    fn legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence() {
        // `golden` pins `reconstruct_and_print`'s pixels, but it calls that helper
        // *directly* — it never crosses `render`'s `match output_params.preset`. So
        // swapping the two match arms would leave every golden green, and the
        // legacy-vs-legacy e2e comparison (implicit vs explicit `--output-preset
        // legacy`) would stay byte-identical too. Pin the boundary itself: the
        // no-preset render must BE `color::to_output(reconstruct_and_print(…))`,
        // bit-for-bit, image and ICC.
        //
        // In-process, so both sides come from this build's lcms2 and this is not the
        // cross-target ICC/post-transform trap.
        let img = synthetic_negative(8, 8);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let print = PrintParams {
            print_exposure: -0.5,
            black_point: 0.01,
            ..PrintParams::default()
        };
        for reconstruction in [Reconstruction::Simple, density_default(), sigmoid_default()] {
            for output in [
                legacy_output(),
                OutputParams {
                    preset: OutputPreset::Legacy,
                    depth: OutDepth::F32,
                    ..OutputParams::default()
                },
                OutputParams {
                    preset: OutputPreset::Legacy,
                    output_profile: Some("srgb".into()),
                    ..OutputParams::default()
                },
            ] {
                let got = render(&img, &base, &reconstruction, &print, &output).unwrap();
                let (positive, convert) =
                    reconstruct_and_print(&img, &base, &reconstruction, &print).unwrap();
                let (want_image, want_icc) = color::to_output(positive, &output).unwrap();
                let bits = |v: &[f32]| -> Vec<u32> { v.iter().map(|x| x.to_bits()).collect() };
                assert_eq!(
                    bits(&got.image.rgb),
                    bits(&want_image.rgb),
                    "{reconstruction:?} / {output:?}"
                );
                assert_eq!(
                    got.image.ir, want_image.ir,
                    "{reconstruction:?} / {output:?}"
                );
                assert_eq!(got.icc, want_icc, "{reconstruction:?} / {output:?}");
                assert_eq!(got.convert, convert, "{reconstruction:?} / {output:?}");
            }
        }
    }

    #[test]
    fn film_master_render_bypasses_the_colour_transform_and_print_controls() {
        // The film-master branch must hand back the *mapped* ACEScg pixels: no
        // print render, and no `color::to_output` transform (which would
        // re-apply the Rec.709→ACEScg matrix on values that already crossed it).
        // Pin that by recomputing the expected buffer through the mapper directly.
        let img = synthetic_negative(8, 8);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let out = render(
            &img,
            &base,
            &density_default(),
            // Non-default print controls are a *usage error* under film-master
            // (`cli::validate`); a defaulted set proves the branch simply never
            // consults them.
            &PrintParams::default(),
            &OutputParams {
                preset: OutputPreset::FilmMaster,
                ..OutputParams::default()
            },
        )
        .unwrap();

        let (film, _) = algo::reconstruct(&img, &base, &density_default()).unwrap();
        let want = working_space::map_nc_film_rgb_v1(film).into_linear();
        let bits = |v: &[f32]| -> Vec<u32> { v.iter().map(|x| x.to_bits()).collect() };
        assert_eq!(bits(&out.image.rgb), bits(&want.rgb));
        // The embedded tag is the *linear* ACEScg profile the values are genuinely
        // in — byte-identical to what `icc_profile` builds, so no display profile
        // and no transfer curve was substituted for the master. (Local-only byte
        // comparison: both sides come from the same lcms2 build in this process, so
        // this is not the cross-target ICC-bytes trap.)
        assert_eq!(out.icc, color::icc_profile(&OutputSpace::AcesCg).unwrap());
        // The master applied no white balance, so it claims none…
        assert_eq!(out.convert.white_balance, None);
        // …but the reconstruction's own resolved anchor IS part of the master.
        assert_eq!(out.convert.dmax, Some(crate::algo::density::NOMINAL_DMAX));
    }

    #[test]
    fn no_tone_gamut_or_transfer_operation_runs_on_film_master() {
        // The branch fixture for "`film-master` bypasses display rendering": run the
        // *same* inputs down both branches and show the legacy branch's operations
        // are observable while the master's pixels stay the mapper's output.
        let img = synthetic_negative(8, 8);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let master_params = OutputParams {
            preset: OutputPreset::FilmMaster,
            ..OutputParams::default()
        };
        let (film, _) = algo::reconstruct(&img, &base, &midtone_reconstruction()).unwrap();
        let mapped = working_space::map_nc_film_rgb_v1(film).into_linear();

        // (a) A print control the legacy branch honours (2^1 exposure doubles every
        //     sample) leaves the master untouched. `render` is a pure function, so it
        //     is callable with the combination `cli::validate` rejects — which is
        //     exactly what makes this a bypass proof rather than a validation proof.
        let hot = PrintParams {
            print_exposure: 1.0,
            ..PrintParams::default()
        };
        let master = render(&img, &base, &midtone_reconstruction(), &hot, &master_params).unwrap();
        let bits = |v: &[f32]| -> Vec<u32> { v.iter().map(|x| x.to_bits()).collect() };
        assert_eq!(
            bits(&master.image.rgb),
            bits(&mapped.rgb),
            "a print control must not reach the master"
        );
        let legacy_hot = render(
            &img,
            &base,
            &midtone_reconstruction(),
            &hot,
            &OutputParams {
                preset: OutputPreset::Legacy,
                depth: OutDepth::F32,
                ..OutputParams::default()
            },
        )
        .unwrap();
        // `master * 1.5` is only a meaningful bar if the master sample is positive —
        // post-matrix ACEScg legitimately goes negative, and a `<= 0` master would
        // make the comparison trivially true.
        assert!(
            master.image.rgb[0] > 0.0,
            "the fixture's first master sample must be positive for the bound below \
             to mean anything (got {})",
            master.image.rgb[0]
        );
        assert!(
            legacy_hot.image.rgb[0] > master.image.rgb[0] * 1.5,
            "the legacy branch must actually apply the exposure it was given \
             (legacy {} vs master {})",
            legacy_hot.image.rgb[0],
            master.image.rgb[0]
        );

        // (b) A transfer/gamut operation the legacy branch honours (`srgb`, whose ICC
        //     carries the piecewise sRGB TRC) also leaves the master untouched: the
        //     master's samples stay linear, so a mid value must be visibly lower than
        //     the display-encoded one.
        let legacy_srgb = render(
            &img,
            &base,
            &midtone_reconstruction(),
            &PrintParams::default(),
            &OutputParams {
                preset: OutputPreset::Legacy,
                output_profile: Some("srgb".into()),
                ..OutputParams::default()
            },
        )
        .unwrap();
        let master_plain = render(
            &img,
            &base,
            &midtone_reconstruction(),
            &PrintParams::default(),
            &master_params,
        )
        .unwrap();
        assert!(
            legacy_srgb.image.rgb[0] > master_plain.image.rgb[0] + 0.05,
            "the legacy branch must apply the sRGB transfer it was given \
             (legacy {} vs master {})",
            legacy_srgb.image.rgb[0],
            master_plain.image.rgb[0]
        );
    }

    #[test]
    fn film_master_render_ignores_the_output_depth_knob_entirely() {
        // `film-master` resolves f32 by definition, not via `output.depth`. The depth
        // half is pinned in `types`; what belongs *here* is that `render` itself does
        // not consult the knob — change it and the master's pixels and ICC must be
        // bit-identical.
        // (`render` is pure, so it is callable with the combination `cli::validate`
        // rejects, which is what makes this a bypass proof.)
        let img = synthetic_negative(8, 8);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let render_with = |depth: OutDepth| {
            render(
                &img,
                &base,
                &density_default(),
                &PrintParams::default(),
                &OutputParams {
                    preset: OutputPreset::FilmMaster,
                    depth,
                    ..OutputParams::default()
                },
            )
            .unwrap()
        };
        let (off, on) = (render_with(OutDepth::U16), render_with(OutDepth::F32));
        let bits = |v: &[f32]| -> Vec<u32> { v.iter().map(|x| x.to_bits()).collect() };
        assert_eq!(bits(&off.image.rgb), bits(&on.image.rgb));
        assert_eq!(off.icc, on.icc);
        // …and both resolve the f32 encode depth the branch is defined by.
        for depth in [OutDepth::U16, OutDepth::F32] {
            assert_eq!(
                OutputParams {
                    preset: OutputPreset::FilmMaster,
                    depth,
                    ..OutputParams::default()
                }
                .depth(),
                crate::types::OutDepth::F32
            );
        }
    }

    #[test]
    fn film_master_render_works_for_every_reconstruction_path() {
        // The split is producer-agnostic: simple, exponential, and sigmoid all
        // reach the master through the same mapper.
        let img = synthetic_negative(8, 8);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        let params = OutputParams {
            preset: OutputPreset::FilmMaster,
            ..OutputParams::default()
        };
        for reconstruction in [Reconstruction::Simple, density_default(), sigmoid_default()] {
            let out = render(
                &img,
                &base,
                &reconstruction,
                &PrintParams::default(),
                &params,
            )
            .unwrap();
            assert_eq!(out.image.rgb.len(), 8 * 8 * 3, "{reconstruction:?}");
            assert_eq!(out.convert.white_balance, None, "{reconstruction:?}");
        }
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
            &legacy_output(),
        ) {
            Err(e) => assert_eq!(e.exit_code(), 1),
            Ok(_) => panic!("expected a degenerate-base error"),
        }
    }
}

/// Golden fixtures pinning the reconstruction split bit-for-bit against the
/// **pre-split monolithic converters** (`Algorithm::{Simple,Density,Sigmoid}`).
/// Most expected values below were captured by running the pre-refactor code on
/// these exact inputs and printing `f32::to_bits` — so any arithmetic drift in the
/// split (a reordered multiply, a changed intermediate, a lost anchor) fails these
/// tests immediately, not in a downstream image diff. That was the split task's
/// acceptance gate: the split is a structural refactor, the default pixels are the
/// contract.
///
/// **Two of the twelve are not reference captures**, and the claim has to be scoped
/// or it stops being true. `golden_sigmoid_default_is_numerically_exact` was
/// recaptured on 2026-08-03 when the sigmoid's own defaults deliberately moved, and
/// `golden_new_default_is_bit_identical` was captured fresh from this build on
/// 2026-08-08 for the new default render. Both honestly pin "this has not drifted
/// since it was set", which is strictly weaker than "matches the reference
/// implementation" — no golden can claim the stronger thing about a value that was
/// deliberately changed. Each says so at its own call site; read it before treating
/// a failure there as proof of a regression. Nothing here hashes an encoded TIFF:
/// see the note at the end of the module for why that could never be checked in.
///
/// The module is (test-only) `pub(crate)` so the `pipeline_version` drift gate in
/// [`crate::version`] fingerprints **these exact** curated vectors instead of a
/// second copy that could quietly drift away from them.
#[cfg(test)]
pub(crate) mod golden {
    use super::*;
    use crate::types::{
        AnchorPlacement, BalanceRange, DensityCurve, DensityParams, DmaxSource, ExponentialParams,
        SigmoidParams, WbSource,
    };

    /// Five pixels spanning the tonal range plus out-of-range finite values,
    /// with an IR plane (`[0.1, 0.2, 0.3, 0.4, 0.5]`):
    /// near-base shadow, midtone, dense highlight, out-of-range (above base /
    /// negative / zero → epsilon floor), and exactly-the-base.
    ///
    /// These five pixels are the crate's cross-platform bit-identity substrate:
    /// their default-path results are pinned here **and** hashed into the
    /// `pipeline_version` drift gate (`crate::version`), so both mechanisms measure
    /// the same thing.
    pub(crate) fn pixels() -> LinearImage {
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

    /// The film base [`pixels`] is reconstructed against (shared with the drift
    /// gate, for the same reason).
    pub(crate) fn base() -> FilmBase {
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
            ..PrintParams::default()
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

    /// The configuration the vectors below were captured under: the exponential
    /// straight line at gamma **1.0** with the anchor pinned at the old
    /// `NOMINAL_DMAX` of **2.0**.
    ///
    /// Pinned **explicitly** rather than through `DensityCurve::default()`, because
    /// on 2026-08-08 the default became the sigmoid, `NOMINAL_DMAX` became 1.3, and
    /// this curve's gamma became 2.0. A golden vector that silently follows the
    /// default stops pinning anything the moment the default moves — it just
    /// re-describes whatever the build now does. Naming the configuration keeps
    /// every bit below exactly as captured from the reference code, and the *new*
    /// default gets its own golden (`golden_new_default_...`).
    ///
    /// `Explicit(2.0)` is arithmetically identical to the old `Fixed`: both resolve
    /// to the same anchor value, so these are the original captures, not a rebase.
    fn frozen_reference_curve() -> DensityCurve {
        DensityCurve::Exponential(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::Explicit(2.0),
            anchor: AnchorPlacement::WhiteAtDmax,
        })
    }

    /// `Reconstruction::default()`'s density knobs with [`frozen_reference_curve`].
    fn frozen_reference_config() -> Reconstruction {
        Reconstruction::Density {
            density: DensityParams::default(),
            curve: frozen_reference_curve(),
        }
    }

    /// The defining property of `BlackAtBase`: the film base renders to exactly the
    /// stated floor, whatever the slope.
    ///
    /// Asserted as a property rather than as captured bits, because captured bits for a
    /// rule introduced in the same commit only re-describe the build. Pixel 5 of
    /// [`pixels`] *is* the base, so its corrected density is exactly 0 and its rendered
    /// value is the floor by construction — which is what makes this falsifiable: a sign
    /// error in `−log10(floor)/contrast` moves it immediately.
    #[test]
    fn black_at_base_renders_the_film_base_to_the_stated_floor() {
        for floor in [0.002f32, 0.005, 0.05] {
            for gamma in [1.0f32, 2.0, 2.5] {
                let reconstruction = Reconstruction::Density {
                    density: DensityParams::default(),
                    curve: DensityCurve::Exponential(ExponentialParams {
                        gamma,
                        dmax: DmaxSource::Fixed,
                        anchor: AnchorPlacement::BlackAtBase(floor),
                    }),
                };
                let (out, _) = reconstruct_and_print(
                    &pixels(),
                    &base(),
                    &reconstruction,
                    &PrintParams::default(),
                )
                .unwrap();
                // Pixel 5 (rgb offsets 12..15) is exactly the base.
                for c in 0..3 {
                    let got = out.rgb[12 + c];
                    assert!(
                        (got - floor).abs() <= floor * 1e-5,
                        "floor {floor} gamma {gamma} channel {c}: rendered {got}"
                    );
                }
            }
        }
    }

    /// `BlackAtBase` is not a new curve — it is the same straight line at a derived
    /// anchor, so it must be **bit-identical** to `WhiteAtDmax` at that anchor.
    ///
    /// This is what keeps the exponential usable as the debuggable reference: the two
    /// spellings cannot diverge. It also pins the anchors the 2026-08-03 candidate retest
    /// recorded (floor 0.002 → 1.349, 0.005 → 1.151 at contrast 2.0).
    #[test]
    fn black_at_base_equals_white_at_dmax_at_the_derived_anchor() {
        let gamma = 2.0f32;
        for floor in [0.002f32, 0.005] {
            let derived = -floor.log10() / gamma;
            let render = |anchor| {
                let reconstruction = Reconstruction::Density {
                    density: DensityParams::default(),
                    curve: DensityCurve::Exponential(ExponentialParams {
                        gamma,
                        dmax: if matches!(anchor, AnchorPlacement::WhiteAtDmax) {
                            DmaxSource::Explicit(derived)
                        } else {
                            DmaxSource::Fixed
                        },
                        anchor,
                    }),
                };
                let (out, _) = reconstruct_and_print(
                    &pixels(),
                    &base(),
                    &reconstruction,
                    &PrintParams::default(),
                )
                .unwrap();
                out.rgb.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            };
            assert_eq!(
                render(AnchorPlacement::BlackAtBase(floor)),
                render(AnchorPlacement::WhiteAtDmax),
                "floor {floor} (derived anchor {derived})"
            );
        }
    }

    #[test]
    fn golden_new_default_is_bit_identical() {
        // THE default path as of 2026-08-08: density reconstruction, **sigmoid**
        // curve with its derived contrast/shoulder, the nominal anchor at the new
        // NOMINAL_DMAX = 1.3, neutral print.
        //
        // Captured from THIS build, not from the reference implementation — so it
        // pins "the default has not drifted since it was set", which is what a
        // default golden can honestly claim after the default deliberately moved.
        // The reference-derived captures live in the `frozen_reference_*` goldens
        // above and are untouched.
        assert_golden(
            Reconstruction::default(),
            PrintParams::default(),
            &[
                0x3c23e35f, 0x3c2d03d9, 0x3c2e457f, 0x3da066cb, 0x3da68379, 0x3ddb53b4, 0x3f7f12f0,
                0x3f7f2169, 0x3f7f2ec2, 0x3c05798b, 0x3f800000, 0x3f800000, 0x3c1928cc, 0x3c1928cc,
                0x3c1928cc,
            ],
            Some(0x3fa66666), // NOMINAL_DMAX = 1.3
            Some(UNIT_WB),
            None,
        );
    }

    #[test]
    fn golden_density_exponential_reference_is_bit_identical() {
        // The exponential straight line at gamma 1.0 / anchor 2.0 — what used to be
        // THE default path, and still the reference capture these bits came from.
        // No longer reached by `Reconstruction::default()` (see
        // `frozen_reference_curve`), so it is named here instead.
        assert_golden(
            frozen_reference_config(),
            PrintParams::default(),
            &[
                0x3c2d7a46, 0x3c343958, 0x3c35161a, 0x3cf5c28f, 0x3cfa4fa3, 0x3d0f5c2a, 0x3ee66668,
                0x3eeaaaab, 0x3eeeeef1, 0x3bc49ba7, 0x45abdfff, 0x45833ffb, 0x3c23d70a, 0x3c23d70a,
                0x3c23d70a,
            ],
            Some(0x40000000), // the anchor these bits were captured at
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
                    anchor: AnchorPlacement::WhiteAtDmax,
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
                    anchor: AnchorPlacement::WhiteAtDmax,
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
                    anchor: AnchorPlacement::WhiteAtDmax,
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

    /// The sigmoid with its shipped parameters but the anchor pinned at the **old**
    /// `NOMINAL_DMAX` of 2.0, so the 2026-08-03 recapture below stays the captured
    /// bits rather than silently following `NOMINAL_DMAX` to 1.3.
    fn sigmoid_at_reference_anchor_2_0() -> Reconstruction {
        Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::Sigmoid(SigmoidParams {
                dmax: DmaxSource::Explicit(2.0),
                ..SigmoidParams::default()
            }),
        }
    }

    #[test]
    fn golden_sigmoid_at_the_reference_anchor_is_numerically_exact() {
        // RECAPTURED 2026-08-03 (`algo/reference-anchored-sigmoid`, Phase 4). The sigmoid
        // defaults changed deliberately: contrast 1.0 → REFERENCE_CONTRAST (≈2.07), shoulder
        // 0.2 → 0.6, and the anchor is now mid-grey at half the reference rather than white
        // at the reference. On this synthetic vector the base pixel moves 0.0115 → 0.00177
        // (≈28/255 → ≈6/255, i.e. an actual black — the defect this task was opened for) and
        // the dense highlight 0.448 → 0.946.
        //
        // NOTE this is **no longer the sigmoid default**, which is why the name says
        // "at the reference anchor" instead. It runs the shipped sigmoid parameters at
        // `Explicit(2.0)` — the anchor `NOMINAL_DMAX` carried when these bits were
        // captured — so the capture keeps pinning what it was captured for. The live
        // default is the same curve at `NOMINAL_DMAX = 1.3`, pinned separately by
        // `golden_new_default_is_bit_identical`. On a *measured* roll (reference ≈1.35)
        // mid-grey lands at ≈0.67, matching real mid-tones; the fallback constant is
        // `film-base/dmax-anchor-reliability`'s to settle and this vector is not
        // evidence about it.
        //
        // The drift-gate fingerprints do not move for *this* vector: it is not a default
        // path, so `version::PIPELINE_FINGERPRINTS` never hashed it. The bump that flipped
        // the default curve to the sigmoid is v2, recorded on 2026-08-08 by this same
        // change — not, as this note used to predict, deferred to `output/presets`.
        assert_golden(
            sigmoid_at_reference_anchor_2_0(),
            PrintParams::default(),
            &[
                0x3af793c5, 0x3b02af7d, 0x3b03a290, 0x3c7438ec, 0x3c7da965, 0x3ca7e975, 0x3f72198a,
                0x3f72e4fa, 0x3f73a232, 0x3ac99e34, 0x3f800000, 0x3f800000, 0x3ae75cf6, 0x3ae75cf6,
                0x3ae75cf6,
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
                    // The golden vectors were captured with the anchor == dmax.
                    anchor: AnchorPlacement::WhiteAtDmax,
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
            frozen_reference_config(),
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
            frozen_reference_config(),
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
                curve: frozen_reference_curve(),
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
            sigmoid_at_reference_anchor_2_0(),
            PrintParams {
                white_balance: WbSource::Percentile,
                ..PrintParams::default()
            },
            &[
                0x3b02e55f, 0x3b02af7d, 0x3b03a290, 0x3c811f4b, 0x3c7da965, 0x3ca7e975, 0x3f800000,
                0x3f72e4fa, 0x3f73a232, 0x3ad531a7, 0x3f800000, 0x3f800000, 0x3af4a59d, 0x3ae75cf6,
                0x3ae75cf6,
            ],
            Some(0x40000000),
            // RECAPTURED 2026-08-03: the auto-WB gain drops 2.2304 → 1.0574 because the
            // estimator samples the *rendered* positive, and the new sigmoid defaults produce
            // a far better-balanced one — so it has much less to correct. A large WB gain was
            // partly compensating for the old curve, which is worth knowing: WB and the curve
            // were entangled, and they are less so now.
            Some([0x3f875963, 0x3f800000, 0x3f800000]),
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
                curve: frozen_reference_curve(),
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

    // --- legacy no-preset regression ----------------------------------------
    //
    // No full-frame / whole-TIFF bit-exact hash is checked in: it can't be a
    // portable CI gate. The reconstruction's transcendental math (`10^`, `log10`)
    // diverges by ~1 ULP across libm implementations over a full frame — x86_64 CI
    // produced a different frame hash than the capture host, while the 5-pixel
    // per-pixel goldens matched exactly — and the downstream lcms2 color transform
    // + encode add further per-target bytes. nc's determinism contract is
    // byte-identity per build/architecture (design-spec §8), not across hosts.
    //
    // The per-pixel goldens above (a curated tonal-range + out-of-range vector with
    // dmax/white-balance/balance-range/IR all pinned, captured from the pre-split
    // code) are the portable bit-identity / no-`pipeline_version`-bump gate.
}
