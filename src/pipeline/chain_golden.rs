//! **Goldens for the new flow's stages** (`nf-verification/stage-goldens`): the fixed
//! decode, the NC film RGB v1 mapping, and each stage of [`chain`].
//!
//! The new chain's counterpart of `stages::golden`, written fresh rather than
//! re-pointing that module's vectors: `golden::pixels()` is shared with every
//! historical `PIPELINE_FINGERPRINTS` row, and it retires with the legacy path.
//!
//! **Small, curated, decode-independent**: a handful of values fed straight into each
//! stage, no file and no assets, captured as raw `f32` bits. A failure names its stage
//! and sample. Two kinds of vector:
//!
//! - **Per-stage**, so a failure is localized to the stage whose arithmetic moved.
//! - **Threaded** through [`chain::render`], the only vectors that see the chain's
//!   stages *wired together* — which params reach which stage, what the
//!   render reports, and the order the per-channel gains and the 3×3 run in (they do
//!   not commute). They start at the mapped ACEScg, not at the decode: the hand-off
//!   from the decode, and the recipe reaching `DecodeParams`, are the orchestrator's
//!   (`cli::render_new_flow_frame`) and are not covered here.
//!
//! **Which stages are bit-exact and which are windowed is decided by their libm
//! calls.** The decode makes two (`log10`, `powf`), a fractional exposure one
//! (`exp2`), fit range one (`log2`) — but only for an HDR peak and only above
//! diffuse white — the look's contrast one `powf` per positive channel, its per-channel
//! grade one on red and one on blue (both entering its luminance restore), and highlight
//! desaturation one per pixel at most: `log10` inside
//! its band's ramp, `log2` inside its brightness ramp (its constants' `powf` / `exp2`
//! only classify, never enter the arithmetic); all are pinned within a window [`reachable_window`] *derives* by
//! enumerating what a 1-ULP-accurate libm can return (CLAUDE.md, determinism). Every
//! other stage here is IEEE `+ − × /` and `sqrt` with no FMA contraction — Rust never
//! fuses implicitly — plus sorts and fixed-order sums, so it is pinned bit for bit.
//! A whole-stop exposure still calls `exp2`, but at an integer, and is **taken as
//! exact** there — a power of two every shipped libm returns exactly, and an
//! assumption the 1-ULP premise alone does not give.
//!
//! **Read a failure from the most upstream red golden — and never read a green one as
//! covering the stages above it.** A stage's input can only be minted by the stage
//! before it, so the downstream vectors enter through the mapping, and fit gamut's
//! through scene correction and the look at their identities and fit range at zero
//! headroom. A fault in
//! one of those paths can therefore red goldens below it, and the first red one in
//! chain order names the stage that moved. But every downstream vector bypasses the
//! decode (`FilmRgbImage::fixture`), so a decode fault reds only the decode's goldens;
//! and the per-stage vectors below scene correction bypass its multiply (they run it
//! at identity), so a fault there reds its own goldens and the threaded ones only.
//!
//! **NaN bits are never pinned.** A NaN's payload after arithmetic is a property of the
//! target's FPU and libm, not of the chain; an expected NaN is asserted as *a* NaN.
//! Fit range refuses a non-finite sample, so it and every stage below it are pinned on
//! the finite pixels only, and the NaN pixel's refusal is a test of its own.
//!
//! Nothing past fit gamut is pinned here: the encode (and any future colour-managed
//! transform) is verified by same-machine before/after, never by a committed vector.
//!
//! A deliberate change to a stage's arithmetic or default recaptures that stage's
//! vector (and the threaded one) in the same change, with a dated note saying why.

use crate::algo::FilmRgbImage;
use crate::algo::fixed::DIFFUSE_WHITE;
use crate::algo::fixed::{self, DENSITY_OFFSET, DecodeParams, SCAN_FLOOR};
use crate::pipeline::chain::{self, ChainParams, DisplayTarget, SharedParams};
use crate::pipeline::colorimetry::pinned::ACESCG_LUMA;
use crate::pipeline::fit_gamut::{self, DestinationGamut, FitGamutParams};
use crate::pipeline::fit_range::{self, DisplayPeak, FitRange, FitRangeParams};
use crate::pipeline::look::{self, HighlightDesaturation, LookParams, LookSection};
use crate::pipeline::scene_correction::{
    self, SceneCorrection, SceneCorrectionParams, WhiteBalance,
};
use crate::pipeline::working_space::{AcesCgImage, map_nc_film_rgb_v1};
use crate::types::{DEFAULT_HEADROOM_STOPS, FilmBase, LinearImage};

// --- shared harness ----------------------------------------------------------

/// An expected value that is "some NaN" — see the module docs on NaN payloads.
const NAN: u32 = 0x7fc0_0000;

/// The accuracy premise every window rests on: a conforming libm is within 1 ULP on
/// `log10`, `powf` and `exp2`. Deliberately the weak bound — `stages::golden`'s
/// `LIBM_MAX_ERROR_ULPS` records why a margin-based argument failed on real targets.
const LIBM_MAX_ERROR_ULPS: i64 = 1;

/// Sanity ceiling on a derived window: a sample landing somewhere the chain amplifies
/// steeply is reported rather than silently granted a huge tolerance.
const MAX_REASONABLE_WINDOW_ULPS: i64 = 200;

fn bits(pixels: &[f32]) -> Vec<u32> {
    pixels.iter().map(|v| v.to_bits()).collect()
}

/// Assert `got` is bit-for-bit the captured `want`, [`NAN`] meaning any NaN.
fn assert_stage_bits(stage: &str, got: &[f32], want: &[u32]) {
    assert_eq!(got.len(), want.len(), "stage `{stage}`: sample count");
    for (i, (&g, &w)) in got.iter().zip(want).enumerate() {
        let wf = f32::from_bits(w);
        if wf.is_nan() {
            assert!(
                g.is_nan(),
                "stage `{stage}` sample {i}: {g:e} where a NaN was captured"
            );
        } else {
            assert_eq!(
                g.to_bits(),
                w,
                "stage `{stage}` sample {i}: {:08x} ({g:e}) drifted from the captured \
                 {w:08x} ({wf:e})",
                g.to_bits()
            );
        }
    }
}

/// A finite `f32` on a line where adjacent values are one apart, `±0` both at zero —
/// so a ULP distance is defined across a sign change as well.
fn ordered(x: f32) -> i64 {
    let magnitude = i64::from(x.to_bits() & 0x7fff_ffff);
    if x.is_sign_negative() {
        -magnitude
    } else {
        magnitude
    }
}

fn ulps_between(a: f32, b: f32) -> i64 {
    debug_assert!(a.is_finite() && b.is_finite(), "{a} / {b}");
    (ordered(a) - ordered(b)).abs()
}

/// Every output a conforming target can produce for one sample, as a window in ULPs
/// around the correctly-rounded one.
///
/// `rounded` is the correctly-rounded value of a libm call's result; a conforming libm
/// may return it or either neighbour, so each is rendered through `render` (the rest
/// of the stage, evaluated target-independently) and the widest excursion taken.
/// `final_call` adds the error of one more libm call at the very end, if the stage
/// makes one.
///
/// **It measures the reachable set's own spread and never looks at the captured
/// value** — a window derived from the value under test widens exactly as far as that
/// value drifts, which `stages::golden::reachable_window` learned by passing a frame
/// moved 115,523 ULPs.
fn reachable_window(render: impl Fn(f32) -> f32, rounded: f32, final_call: i64) -> i64 {
    let centre = render(rounded);
    let widest = [rounded.next_down(), rounded.next_up()]
        .into_iter()
        .map(|neighbour| ulps_between(render(neighbour), centre))
        .max()
        .expect("two neighbours");
    widest + final_call
}

// --- the fixed decode (`algo::fixed`) ----------------------------------------

fn base() -> FilmBase {
    FilmBase::from([0.9, 0.55, 0.42])
}

/// Near-base shadow, mid, dense, the base itself (`D = 0`), above the base (`D < 0`),
/// `0.5 / 0.9`, the three floored cases (zero, negative, subnormal), and the three
/// non-finite ones.
fn decode_scan() -> LinearImage {
    LinearImage::new(
        8,
        1,
        vec![
            0.85,
            0.5,
            0.38,
            0.3,
            0.18,
            0.12,
            0.02,
            0.012,
            0.009,
            0.9,
            0.55,
            0.42,
            0.95,
            0.6,
            0.45,
            0.5,
            0.3,
            0.2,
            0.0,
            -0.25,
            f32::MIN_POSITIVE / 4.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ],
        Some(vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]),
    )
    .unwrap()
}

/// A non-zero offset, distinct per channel. The shipped `DENSITY_OFFSET` is
/// `[0, 0, 0]`, where the `+ offset` term is invisible; this second pass keeps it
/// pinned, and keeps a vector in place if `nf-calibration/offset-question` moves the
/// constant.
const PROBE_OFFSET: [f32; 3] = [-0.05, 0.02, 0.07];

/// The captured decode at the shipped defaults, and at [`PROBE_OFFSET`]. Recaptured
/// 2026-09-24 when the default slope became the linearization alone, 1.8
/// (`nf-reconstruction/gamma-split`); before it the decode ran at the bundled 2.0.
const DECODE_DEFAULT: [u32; 24] = [
    0x3c7a401a, 0x3c826425, 0x3c80c234, 0x3dcbe6ce, 0x3d98c6f9, 0x3d92638a, 0x41508878, 0x408f42e1,
    0x40099236, 0x3c61c89a, 0x3c61c89a, 0x3c61c89a, 0x3c4cd871, 0x3c45f341, 0x3c4e371b, 0x3d2299d4,
    0x3d0d241d, 0x3d15a226, 0x4e2b7e76, 0x4ac90670, 0x48a4c66f, NAN, NAN, NAN,
];
const DECODE_PROBE_OFFSET: [u32; 24] = [
    0x3c4b6944, 0x3c8da90b, 0x3cac1927, 0x3da5bcca, 0x3da5fb2d, 0x3dc3a99d, 0x41298088, 0x409ba486,
    0x4037e083, 0x3c37861a, 0x3c754c0e, 0x3c96e404, 0x3c268133, 0x3c570eed, 0x3c89d02e, 0x3d042abe,
    0x3d1956da, 0x3d47ffd5, 0x4e0b653f, 0x4ada6624, 0x48dc3cd7, NAN, NAN, NAN,
];

/// The anchor the defaults resolve (`MID_ABOVE_BASE + 0.745 / LINEARIZATION`).
const DECODE_ANCHOR: u32 = 0x3f845183;

fn decode_params(offset: [f32; 3]) -> DecodeParams {
    DecodeParams {
        offset,
        ..DecodeParams::default()
    }
}

/// The correctly-rounded density of each finite sample, `None` for a non-finite one.
/// Written as the decode writes it: the floor and the division in f32, then `log10`.
fn correctly_rounded_densities() -> Vec<Option<f32>> {
    let base = <[f32; 3]>::from(base());
    decode_scan()
        .rgb
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            s.is_finite().then(|| {
                let ratio = s.max(SCAN_FLOOR) / base[i % 3];
                -(f64::from(ratio).log10()) as f32
            })
        })
        .collect()
}

/// The decode after its `log10`, correctly rounded: the calibration and exponent in
/// f32 as the decode writes them, the `10^` in f64.
fn decode_from_density(d: f32, channel: usize, params: &DecodeParams) -> f32 {
    let anchor = params.anchor.anchor(params.linearization);
    let corrected = params.scale[channel] * d + params.offset[channel];
    10f64.powf(f64::from(params.linearization * (corrected - anchor))) as f32
}

#[test]
fn golden_decode_is_correct_within_its_libm_window() {
    let densities = correctly_rounded_densities();
    for (offset, expected) in [
        (DENSITY_OFFSET, &DECODE_DEFAULT),
        (PROBE_OFFSET, &DECODE_PROBE_OFFSET),
    ] {
        let params = decode_params(offset);
        let (film, report) = fixed::decode(&decode_scan(), &base(), &params).unwrap();
        assert_eq!(film.rgb().len(), expected.len());

        for (i, (&got, &want)) in film.rgb().iter().zip(expected).enumerate() {
            let Some(d) = densities[i] else {
                assert!(got.is_nan(), "stage `decode` sample {i}: {got:e}, not NaN");
                continue;
            };
            let captured = f32::from_bits(want);
            let window = reachable_window(
                |d| decode_from_density(d, i % 3, &params),
                d,
                LIBM_MAX_ERROR_ULPS,
            );
            let drift = ulps_between(got, captured);
            assert!(
                drift <= window,
                "stage `decode` (offset {offset:?}) sample {i}: {:08x} is {drift} ULP from \
                 the captured {want:08x}, outside the {window} ULP a conforming libm can reach",
                got.to_bits()
            );
        }

        // IEEE arithmetic on constants: bit-exact on every target.
        assert_eq!(
            report.anchor.to_bits(),
            DECODE_ANCHOR,
            "stage `decode`: anchor"
        );
        assert_eq!(report.anchor_rule, "mid-at-base-offset");
        assert!(!report.reads_reference);
        assert_eq!(
            film.ir(),
            Some(&[0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8][..])
        );
    }
}

/// The capture is what a correctly-rounded chain produces, the host's `log10` and `powf`
/// are conforming, and
/// no window is suspiciously wide — the three things the golden above assumes.
#[test]
fn the_decode_capture_is_correctly_rounded_and_the_host_conforms() {
    let densities = correctly_rounded_densities();
    let scan = decode_scan();
    let base = <[f32; 3]>::from(base());
    let mut widest = 0;
    for (offset, expected) in [
        (DENSITY_OFFSET, &DECODE_DEFAULT),
        (PROBE_OFFSET, &DECODE_PROBE_OFFSET),
    ] {
        let params = decode_params(offset);
        for (i, &want) in expected.iter().enumerate() {
            let Some(d) = densities[i] else {
                assert_eq!(want, NAN, "sample {i}: a non-finite scan decodes to NaN");
                continue;
            };
            // Conformance, never correct rounding: that is exactly what varies.
            let host = -(scan.rgb[i].max(SCAN_FLOOR) / base[i % 3]).log10();
            let off = ulps_between(host, d);
            assert!(
                off <= LIBM_MAX_ERROR_ULPS,
                "sample {i}: this host's `log10` is {off} ULP from the correctly rounded {d:e}"
            );
            // The second libm call, `powf`, at the exponent the decode forms from `d`.
            let corrected = params.scale[i % 3] * d + params.offset[i % 3];
            let exponent =
                params.linearization * (corrected - params.anchor.anchor(params.linearization));
            let off = ulps_between(10f32.powf(exponent), decode_from_density(d, i % 3, &params));
            assert!(
                off <= LIBM_MAX_ERROR_ULPS,
                "sample {i}: this host's `powf` is {off} ULP from the correctly rounded 10^{exponent}"
            );
            assert_eq!(
                decode_from_density(d, i % 3, &params).to_bits(),
                want,
                "sample {i} (offset {offset:?}): the capture is not the correctly-rounded decode"
            );
            widest = widest.max(reachable_window(
                |d| decode_from_density(d, i % 3, &params),
                d,
                LIBM_MAX_ERROR_ULPS,
            ));
        }
    }
    assert!(
        widest <= MAX_REASONABLE_WINDOW_ULPS,
        "the widest derived decode window is {widest} ULP — understand it before accepting it"
    );
}

// --- the working-space mapping and the chain ---------------------------------

/// Film-RGB values for everything downstream of the decode: mid-grey, a saturated
/// blue, a red with a negative channel (outside the film cube, which an unclamped
/// reconstruction can produce, and outside Display P3 after the destination matrix, so
/// fit gamut maps it), diffuse white,
/// above white, black, a near-black, and a pixel with a NaN channel.
const FILM_RGB: [f32; 24] = [
    0.18,
    0.18,
    0.18,
    0.002,
    0.004,
    0.9,
    0.9,
    0.004,
    -0.02,
    1.0,
    1.0,
    1.0,
    4.0,
    3.0,
    2.5,
    0.0,
    0.0,
    0.0,
    0.003,
    0.004,
    0.002,
    0.4,
    f32::NAN,
    0.2,
];

/// The first `pixels` of [`FILM_RGB`], with an IR plane, which every stage must carry.
fn film_of(pixels: usize) -> FilmRgbImage {
    FilmRgbImage::fixture(
        LinearImage::new(
            pixels as u32,
            1,
            FILM_RGB[..pixels * 3].to_vec(),
            Some(FILM_IR[..pixels].to_vec()),
        )
        .unwrap(),
    )
}

fn film() -> FilmRgbImage {
    film_of(8)
}

/// How many of [`FILM_RGB`]'s pixels are finite — all but the last. Fit range refuses
/// a non-finite sample, so it and every stage below it are pinned on these.
const FINITE_PIXELS: usize = 7;

const FILM_IR: [f32; 8] = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];

fn aces() -> AcesCgImage {
    map_nc_film_rgb_v1(film())
}

/// [`aces`] without its NaN pixel.
fn finite_aces() -> AcesCgImage {
    map_nc_film_rgb_v1(film_of(FINITE_PIXELS))
}

const MAPPED: [u32; 24] = [
    0x3e3851ec, 0x3e3851ed, 0x3e3851eb, 0x3d393ec1, 0x3c825bc6, 0x3f48872e, 0x3f0d5cdc, 0x3d88563e,
    0x3ad1310b, 0x3f800000, 0x3f800000, 0x3f7fffff, 0x4065b8db, 0x40440fdb, 0x40257c3e, 0x00000000,
    0x00000000, 0x00000000, 0x3b57c101, 0x3b7fc7d4, 0x3b12c8db, NAN, NAN, NAN,
];

#[test]
fn golden_working_space_mapping_is_bit_identical() {
    // The frozen `nc-film-rgb-v1` identifier's runtime half: its matrix is audited in
    // `colorimetry`, but only this pins what the mapper does with it.
    let out = aces();
    assert_stage_bits("working-space", out.rgb(), &MAPPED);
    assert_eq!(out.ir(), Some(&FILM_IR[..]));
}

fn scene_correct(params: &SceneCorrectionParams) -> Vec<f32> {
    let (out, _) = scene_correction::apply(aces(), params).unwrap();
    out.into_buffer().into_linear().rgb
}

const SCENE_STATED: [u32; 24] = [
    0x3ee66667, 0x3eb851ed, 0x3e3851eb, 0x3de78e71, 0x3d025bc6, 0x3f48872e, 0x3fb0b413, 0x3e08563e,
    0x3ad1310b, 0x40200000, 0x40000000, 0x3f7fffff, 0x410f9389, 0x40c40fdb, 0x40257c3e, 0x00000000,
    0x00000000, 0x00000000, 0x3c06d8a1, 0x3bffc7d4, 0x3b12c8db, NAN, NAN, NAN,
];

#[test]
fn golden_scene_correction_stated_is_bit_identical() {
    // A whole-stop exposure: `2^1` is exact on every conforming `exp2`, so this is
    // pure IEEE arithmetic.
    let params = SceneCorrectionParams {
        white_balance: WhiteBalance::Explicit([1.25, 1.0, 0.5]),
        exposure: 1.0,
    };
    assert_stage_bits("scene-correction", &scene_correct(&params), &SCENE_STATED);
}

const SCENE_FRACTIONAL: [u32; 24] = [
    0x3e8f5e09, 0x3e82557d, 0x3e6a99dd, 0x3d90163f, 0x3cb85ad0, 0x3f7f3b03, 0x3f5be8a8, 0x3dc0cf39,
    0x3b0520f2, 0x3fc71f0c, 0x3fb504f3, 0x3fa2ead9, 0x40b2ae8e, 0x408aa300, 0x4052a0df, 0x00000000,
    0x00000000, 0x00000000, 0x3ba7d132, 0x3bb4dd3b, 0x3b3ad386, NAN, NAN, NAN,
];
const FRACTIONAL_EV: f32 = 0.5;
const FRACTIONAL_WB: [f32; 3] = [1.1, 1.0, 0.9];

#[test]
fn golden_scene_correction_fractional_exposure_is_correct_within_its_libm_window() {
    // `2^0.5` goes through `exp2`, which may differ by a ULP across targets; everything
    // after it is IEEE. The window enumerates the three gains a conforming libm returns.
    let rounded = (f64::from(FRACTIONAL_EV).exp2()) as f32;
    let host = FRACTIONAL_EV.exp2();
    assert!(
        ulps_between(host, rounded) <= LIBM_MAX_ERROR_ULPS,
        "this host's `exp2` is not conforming"
    );
    let params = SceneCorrectionParams {
        white_balance: WhiteBalance::Explicit(FRACTIONAL_WB),
        exposure: FRACTIONAL_EV,
    };
    let input = aces().rgb().to_vec();
    let got = scene_correct(&params);
    assert_eq!(got.len(), SCENE_FRACTIONAL.len());
    let mut widest = 0;
    for (i, (&g, &want)) in got.iter().zip(&SCENE_FRACTIONAL).enumerate() {
        if want == NAN {
            assert!(g.is_nan(), "stage `scene-correction` sample {i}: {g:e}");
            continue;
        }
        let render = |gain: f32| input[i] * (FRACTIONAL_WB[i % 3] * gain);
        assert_eq!(
            render(rounded).to_bits(),
            want,
            "sample {i}: capture integrity"
        );
        let window = reachable_window(render, rounded, 0);
        widest = widest.max(window);
        let drift = ulps_between(g, f32::from_bits(want));
        assert!(
            drift <= window,
            "stage `scene-correction` sample {i}: {drift} ULP from the capture, outside the \
             {window} ULP a conforming `exp2` can reach"
        );
    }
    assert!(widest <= MAX_REASONABLE_WINDOW_ULPS, "{widest}");
}

// --- fit range -----------------------------------------------------------------

/// Fit range at `headroom_stops` against `peak`.
fn fit_range_params(headroom_stops: f32, peak: DisplayPeak) -> FitRangeParams {
    FitRangeParams {
        headroom_stops,
        peak,
    }
}

/// [`finite_aces`] through scene correction and the look at their identities, then
/// fit range under `params` — the only way to mint fit range's and fit gamut's input.
fn through_fit_range(params: &FitRangeParams) -> fit_range::RangeFittedImage {
    let (corrected, _) =
        scene_correction::apply(finite_aces(), &SceneCorrectionParams::default()).unwrap();
    let graded = look::apply(corrected, &LookParams::off()).unwrap();
    fit_range::apply(graded, params).unwrap()
}

#[test]
fn golden_look_and_zero_headroom_fit_range_are_bit_exact_identities() {
    // An empty look is an identity, and so is fit range at zero headroom — which is
    // also how fit gamut's golden below enters its stage. Highlight desaturation has its
    // own golden below.
    let input = bits(finite_aces().rgb());
    let out = through_fit_range(&fit_range_params(0.0, DisplayPeak::SDR))
        .into_buffer()
        .into_linear();
    assert_eq!(
        bits(&out.rgb),
        input,
        "stage `look` or `fit-range` moved a pixel"
    );
    assert_eq!(out.ir.as_deref(), Some(&FILM_IR[..FINITE_PIXELS]));
}

/// Fit range at the default six stops against an SDR peak. `+ − × /` and `sqrt` in
/// binary64, all correctly rounded by IEEE 754, so pinned bit for bit.
const FIT_RANGE_SDR: [u32; 21] = [
    0x3e3851ec, 0x3e3851ed, 0x3e3851eb, 0x3d514922, 0x3c9346a9, 0x3f628d4e, 0x3f0b3c44, 0x3d864902,
    0x3ace0b24, 0x3f0cb273, 0x3f0cb273, 0x3f0cb272, 0x3f65e127, 0x3f44323f, 0x3f259945, 0x00000000,
    0x00000000, 0x00000000, 0x3b82f761, 0x3b9b4360, 0x3b323396,
];

#[test]
fn golden_fit_range_sdr_is_bit_identical() {
    let out = through_fit_range(&fit_range_params(DEFAULT_HEADROOM_STOPS, DisplayPeak::SDR))
        .into_buffer()
        .into_linear();
    assert_stage_bits("fit-range (sdr)", &out.rgb, &FIT_RANGE_SDR);
    assert_eq!(out.ir.as_deref(), Some(&FILM_IR[..FINITE_PIXELS]));
}

/// An HDR display's peak: 1000 nits over 203-nit reference white. Stated here rather
/// than borrowed from `pipeline::hdr`, which retires with the legacy chain.
const HDR_PEAK: f32 = 1000.0 / 203.0;

/// The HDR peak's operator for one pixel, written out independently of the stage,
/// with `log2(Y)` supplied — the one libm call, which the window enumerates.
fn hdr_fit_range_pixel(px: [f32; 3], log2_luminance: f32) -> [f32; 3] {
    let y = px[0] * ACESCG_LUMA[0] + px[1] * ACESCG_LUMA[1] + px[2] * ACESCG_LUMA[2];
    let w = f64::from(DEFAULT_HEADROOM_STOPS.exp2());
    let k = 1.0 - 0.18;
    let gain = 2.0 / (k + (k * k + 4.0 * 0.18 / (w * w)).sqrt());
    let u = f64::from(y) * gain;
    let base = (u * (1.0 + u / (w * w)) / (1.0 + u)) as f32;
    // The lift starts at diffuse white, `1.0`, whose `log2` is exactly `0`.
    let t = (log2_luminance / DEFAULT_HEADROOM_STOPS).clamp(0.0, 1.0);
    let lift = t * t * (3.0 - 2.0 * t);
    let scale = base * (1.0 + (HDR_PEAK - 1.0) * lift) / y;
    px.map(|c| c * scale)
}

#[test]
fn golden_fit_range_hdr_agrees_below_white_and_is_correct_within_its_libm_window() {
    let peak = DisplayPeak::new(HDR_PEAK).unwrap();
    let hdr = through_fit_range(&fit_range_params(DEFAULT_HEADROOM_STOPS, peak))
        .into_buffer()
        .into_linear()
        .rgb;
    let input = finite_aces().rgb().to_vec();
    let (mut lifted, mut widest) = (0, 0);
    for (p, px) in input.as_chunks::<3>().0.iter().enumerate() {
        let y = px[0] * ACESCG_LUMA[0] + px[1] * ACESCG_LUMA[1] + px[2] * ACESCG_LUMA[2];
        if y <= DIFFUSE_WHITE {
            // Below diffuse white the lift is zero: the HDR peak renders the SDR
            // pixel bit for bit — the gain-map contract, at the stage.
            assert_stage_bits(
                "fit-range (hdr, below white)",
                &hdr[p * 3..p * 3 + 3],
                &FIT_RANGE_SDR[p * 3..p * 3 + 3],
            );
            continue;
        }
        lifted += 1;
        let rounded = f64::from(y).log2() as f32;
        assert!(
            ulps_between(y.log2(), rounded) <= LIBM_MAX_ERROR_ULPS,
            "this host's `log2` is not conforming at {y:e}"
        );
        for (c, &want) in FIT_RANGE_HDR_LIFTED.iter().enumerate() {
            let i = p * 3 + c;
            let render = |l: f32| hdr_fit_range_pixel(*px, l)[c];
            let captured = f32::from_bits(want);
            assert_eq!(
                render(rounded).to_bits(),
                captured.to_bits(),
                "sample {i}: capture integrity"
            );
            let window = reachable_window(render, rounded, 0);
            widest = widest.max(window);
            let drift = ulps_between(hdr[i], captured);
            assert!(
                drift <= window,
                "stage `fit-range (hdr)` sample {i}: {drift} ULP from the capture, outside \
                 the {window} ULP a conforming `log2` can reach"
            );
        }
    }
    assert_eq!(lifted, 1, "exactly one pixel sits above diffuse white");
    assert!(widest <= MAX_REASONABLE_WINDOW_ULPS, "{widest}");
}

/// The one lifted pixel's three samples, correctly rounded.
const FIT_RANGE_HDR_LIFTED: [u32; 3] = [0x3fc84f43, 0x3faaf58a, 0x3f904c23];

// --- the look --------------------------------------------------------------------

/// Film RGB for highlight desaturation, one pixel per path through the operator, each
/// making at most one libm call: (a) full pull above white — none; (b) in the band's
/// ramp above white — `log10` of the max/min ratio; (c) full pull in the brightness
/// ramp — `log2(Y)`; (d) a coloured highlight past the band — untouched.
const LOOK_FILM: [f32; 12] = [
    1.25, 1.2, 1.18, 1.3, 1.21, 1.14, 0.8, 0.78, 0.77, 1.6, 1.2, 0.9,
];

/// Highlight desaturation alone, at a whole contrast of 2 carried by the linearization
/// with the look's contrast at its identity — so these vectors pin the pull without the
/// contrast's `powf` (which [`LOOK_CONTRAST_FILM`] pins).
fn look_params() -> LookParams {
    LookParams {
        section: LookSection {
            contrast: 1.0,
            channel_grade: look::IDENTITY_CHANNEL_GRADE,
            highlight_desaturation: HighlightDesaturation {
                strength: 0.8,
                ..HighlightDesaturation::default()
            },
        },
        linearization: 2.0,
    }
}

/// The look's input: [`LOOK_FILM`] through the mapper and an identity scene correction.
fn look_input() -> AcesCgImage {
    map_nc_film_rgb_v1(FilmRgbImage::fixture(
        LinearImage::new(4, 1, LOOK_FILM.to_vec(), None).unwrap(),
    ))
}

/// Highlight desaturation for one pixel, written out independently of the stage at
/// [`look_params`], with its one libm result supplied: `log10(max/min)` for a pixel in
/// the band's ramp, `log2(Y)` for one in the brightness ramp.
fn look_pixel(px: [f32; 3], log10_ratio: Option<f32>, log2_luminance: Option<f32>) -> [f32; 3] {
    let y = px[0] * ACESCG_LUMA[0] + px[1] * ACESCG_LUMA[1] + px[2] * ACESCG_LUMA[2];
    let [s0, s1] = HighlightDesaturation::default().band;
    let key = log10_ratio.map_or(1.0, |l| ((s1 - l / 2.0) / (s1 - s0)).clamp(0.0, 1.0));
    let brightness = log2_luminance.map_or(1.0, |l| {
        let t = ((l + 1.0) / 1.0).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    });
    let a = 0.8 * key * brightness;
    px.map(|c| c + a * (y - c))
}

#[test]
fn golden_look_highlight_desaturation_is_correct_within_its_libm_window() {
    let (corrected, _) =
        scene_correction::apply(look_input(), &SceneCorrectionParams::default()).unwrap();
    let out = look::apply(corrected, &look_params())
        .unwrap()
        .into_buffer()
        .into_linear()
        .rgb;
    let input = look_input().rgb().to_vec();
    let px = |p: usize| [input[p * 3], input[p * 3 + 1], input[p * 3 + 2]];
    let luminance =
        |q: [f32; 3]| q[0] * ACESCG_LUMA[0] + q[1] * ACESCG_LUMA[1] + q[2] * ACESCG_LUMA[2];

    for p in 0..4 {
        assert_clear_of_look_edges(&format!("look pixel {p}"), px(p));
    }
    // (a) and (d) are pure IEEE: pinned exactly. (d) is also the input itself.
    assert_stage_bits("look (full pull)", &out[0..3], &LOOK_FULL);
    assert_eq!(
        look_pixel(px(0), None, None).map(f32::to_bits),
        LOOK_FULL,
        "capture integrity"
    );
    assert_eq!(
        bits(&out[9..12]),
        bits(&input[9..12]),
        "a coloured highlight is untouched"
    );

    // (b) and (c): each within the window its one libm call can reach.
    let q = px(1);
    let ratio = q[0].max(q[1]).max(q[2]) / q[0].min(q[1]).min(q[2]);
    let y = luminance(px(2));
    let mut widest = 0;
    for p in [1, 2] {
        let (host, rounded, captured) = if p == 1 {
            (
                ratio.log10(),
                f64::from(ratio).log10() as f32,
                LOOK_BAND_RAMP,
            )
        } else {
            (y.log2(), f64::from(y).log2() as f32, LOOK_BRIGHTNESS_RAMP)
        };
        assert!(
            ulps_between(host, rounded) <= LIBM_MAX_ERROR_ULPS,
            "this host's libm is not conforming on pixel {p}"
        );
        for (c, &want) in captured.iter().enumerate() {
            let render = |l: f32| {
                if p == 1 {
                    look_pixel(px(p), Some(l), None)[c]
                } else {
                    look_pixel(px(p), None, Some(l))[c]
                }
            };
            assert_eq!(
                render(rounded).to_bits(),
                want,
                "pixel {p} sample {c}: capture integrity"
            );
            let window = reachable_window(render, rounded, 0);
            widest = widest.max(window);
            let drift = ulps_between(out[p * 3 + c], f32::from_bits(want));
            assert!(
                drift <= window,
                "stage `look` pixel {p} sample {c}: {drift} ULP from the capture, outside the \
                 {window} ULP a conforming libm can reach"
            );
        }
    }
    assert!(widest <= MAX_REASONABLE_WINDOW_ULPS, "{widest}");
}

/// A pixel whose path through the look is pinned must sit clear of every threshold a
/// conforming libm can move: the band edges `10^(contrast·s)` (`powf`) and the
/// brightness start `2^start_stops` (`exp2`), each enumerated at the correctly rounded
/// value and one ULP either side — otherwise a 1-ULP libm difference could send the
/// pixel down another path.
fn assert_clear_of_look_edges(label: &str, px: [f32; 3]) {
    let [s0, s1] = HighlightDesaturation::default().band;
    let ratio = px[0].max(px[1]).max(px[2]) / px[0].min(px[1]).min(px[2]);
    let y = px[0] * ACESCG_LUMA[0] + px[1] * ACESCG_LUMA[1] + px[2] * ACESCG_LUMA[2];
    let start = HighlightDesaturation::default().start_stops;
    for (name, value, edge) in [
        ("band s0", ratio, 10f64.powf(f64::from(2.0 * s0)) as f32),
        ("band s1", ratio, 10f64.powf(f64::from(2.0 * s1)) as f32),
        ("start", y, f64::from(start).exp2() as f32),
    ] {
        assert!(
            value < edge.next_down() || value > edge.next_up(),
            "{label}: {value} is within one ULP of the {name} edge {edge}"
        );
    }
}

const LOOK_FULL: [u32; 3] = [0x3f9b5286, 0x3f9aa512, 0x3f9a2494];
const LOOK_BAND_RAMP: [u32; 3] = [0x3f9f996c, 0x3f9c0a9a, 0x3f971ca4];
const LOOK_BRIGHTNESS_RAMP: [u32; 3] = [0x3f498013, 0x3f48597c, 0x3f474de1];

/// Film RGB for the look's contrast: a near-neutral mid-grey, a deep shadow, a
/// saturated midtone and a highlight above diffuse white. Each channel makes one libm
/// call (`powf`), the final one before a multiply by [`look::MID_GREY`].
const LOOK_CONTRAST_FILM: [f32; 12] = [
    0.18, 0.18, 0.18, 0.01, 0.012, 0.015, 0.5, 0.3, 0.1, 2.0, 1.6, 1.2,
];

/// The contrast alone, at its default, over the default linearization.
fn look_contrast_params() -> LookParams {
    let mut params = LookParams::off();
    params.section.contrast = look::DEFAULT_CONTRAST;
    params
}

/// Captured 2026-09-24 when the contrast landed (`nf-reconstruction/gamma-split`).
const LOOK_CONTRAST: [u32; 12] = [
    0x3e3851ec, 0x3e3851ed, 0x3e3851eb, 0x3c02fd3c, 0x3c102c62, 0x3c34833c, 0x3ee7fc97, 0x3ea96adc,
    0x3e009182, 0x401733ea, 0x4004981f, 0x3fc84399,
];

#[test]
fn golden_look_contrast_is_correct_within_its_libm_window() {
    let input = map_nc_film_rgb_v1(FilmRgbImage::fixture(
        LinearImage::new(4, 1, LOOK_CONTRAST_FILM.to_vec(), None).unwrap(),
    ));
    let before = input.rgb().to_vec();
    let (corrected, _) = scene_correction::apply(input, &SceneCorrectionParams::default()).unwrap();
    let params = look_contrast_params();
    assert_eq!(params.applied(), "contrast");
    let out = look::apply(corrected, &params)
        .unwrap()
        .into_buffer()
        .into_linear()
        .rgb;
    let k = params.section.contrast;
    let mut widest = 0;
    for (i, (&x, &want)) in before.iter().zip(&LOOK_CONTRAST).enumerate() {
        let base = x / look::MID_GREY;
        let rounded = f64::from(base).powf(f64::from(k)) as f32;
        assert!(
            ulps_between(base.powf(k), rounded) <= LIBM_MAX_ERROR_ULPS,
            "this host's `powf` is not conforming on sample {i}"
        );
        let render = |p: f32| look::MID_GREY * p;
        assert_eq!(
            render(rounded).to_bits(),
            want,
            "sample {i}: capture integrity"
        );
        let window = reachable_window(render, rounded, 0);
        widest = widest.max(window);
        let drift = ulps_between(out[i], f32::from_bits(want));
        assert!(
            drift <= window,
            "stage `look` (contrast) sample {i}: {drift} ULP from the capture, outside \
             the {window} ULP a conforming libm can reach"
        );
    }
    assert!(widest <= MAX_REASONABLE_WINDOW_ULPS, "{widest}");
}

/// Film RGB for the per-channel grade: a shadow, a saturated midtone and a highlight
/// above diffuse white. Each pixel makes two libm calls (`powf` on red and on blue;
/// green's exponent is 1 and skips it), and both enter the luminance restore.
const LOOK_GRADE_FILM: [f32; 9] = [0.02, 0.018, 0.015, 0.5, 0.3, 0.1, 2.0, 1.6, 1.2];

/// The grade alone: contrast 1, desaturation off.
const LOOK_GRADE_EXPONENTS: [f32; 2] = [1.15, 0.9];

/// Captured 2026-09-24 when the grade landed (`nf-look/per-channel-grade`).
const LOOK_GRADE: [u32; 9] = [
    0x3c6fcf1b, 0x3c9f3bc7, 0x3cad901d, 0x3ee50fda, 0x3e98656e, 0x3e039e33, 0x401414ca, 0x3fb9e398,
    0x3f6db006,
];

/// The grade for one pixel, written out independently of the stage, with its two libm
/// results supplied: `(x_r / MID_GREY)^r` and `(x_b / MID_GREY)^b`.
fn look_grade_pixel(px: [f32; 3], red: f32, blue: f32) -> [f32; 3] {
    let luminance =
        |q: [f32; 3]| q[0] * ACESCG_LUMA[0] + q[1] * ACESCG_LUMA[1] + q[2] * ACESCG_LUMA[2];
    let powered = [look::MID_GREY * red, px[1], look::MID_GREY * blue];
    let restore = luminance(px) / luminance(powered);
    powered.map(|c| c * restore)
}

#[test]
fn golden_look_channel_grade_is_correct_within_its_libm_window() {
    let input = map_nc_film_rgb_v1(FilmRgbImage::fixture(
        LinearImage::new(3, 1, LOOK_GRADE_FILM.to_vec(), None).unwrap(),
    ));
    let before = input.rgb().to_vec();
    let (corrected, _) = scene_correction::apply(input, &SceneCorrectionParams::default()).unwrap();
    let mut params = LookParams::off();
    params.section.channel_grade = LOOK_GRADE_EXPONENTS;
    assert_eq!(params.applied(), "channel-grade");
    let out = look::apply(corrected, &params)
        .unwrap()
        .into_buffer()
        .into_linear()
        .rgb;
    let [r, b] = LOOK_GRADE_EXPONENTS;
    let mut widest = 0;
    for p in 0..3 {
        let px = [before[p * 3], before[p * 3 + 1], before[p * 3 + 2]];
        assert!(
            px.iter().all(|&c| c > 0.0),
            "pixel {p} must reach both powers"
        );
        let libm = |x: f32, g: f32| {
            let base = x / look::MID_GREY;
            let rounded = f64::from(base).powf(f64::from(g)) as f32;
            assert!(
                ulps_between(base.powf(g), rounded) <= LIBM_MAX_ERROR_ULPS,
                "this host's `powf` is not conforming on pixel {p}"
            );
            rounded
        };
        let (red, blue) = (libm(px[0], r), libm(px[2], b));
        let centre = look_grade_pixel(px, red, blue);
        for c in 0..3 {
            // Both libm results move independently: enumerate each at its correctly
            // rounded value and one ULP either side.
            let window = [red.next_down(), red, red.next_up()]
                .into_iter()
                .flat_map(|rv| {
                    [blue.next_down(), blue, blue.next_up()]
                        .map(|bv| ulps_between(look_grade_pixel(px, rv, bv)[c], centre[c]))
                })
                .max()
                .unwrap();
            widest = widest.max(window);
            let want = LOOK_GRADE[p * 3 + c];
            assert_eq!(
                centre[c].to_bits(),
                want,
                "pixel {p} sample {c}: capture integrity"
            );
            let drift = ulps_between(out[p * 3 + c], f32::from_bits(want));
            assert!(
                drift <= window,
                "stage `look` (channel grade) pixel {p} sample {c}: {drift} ULP from the \
                 capture, outside the {window} ULP a conforming libm can reach"
            );
        }
    }
    assert!(widest <= MAX_REASONABLE_WINDOW_ULPS, "{widest}");
}

// --- fit gamut -------------------------------------------------------------------

/// Fit gamut against an SDR peak. The vector exercises every case the stage has: in
/// gamut (untouched), a negative P3 channel (pixel 2), a channel just over the peak at
/// luminance ≈ 1 (pixel 3), and a pixel above the peak, rendered neutral at its own
/// luminance (pixel 4). Recaptured 2026-09-24 when the radial map landed
/// (`nf-display-stages/fit-gamut`); before it the stage was the matrix alone.
const FIT_GAMUT_P3: [u32; 21] = [
    0x3e3851ed, 0x3e3851ed, 0x3e3851ec, 0x3b1a5990, 0x3b80e4f0, 0x3f51dddf, 0x3f3bd908, 0x3d12b332,
    0x00000000, 0x3f800000, 0x3f800000, 0x3f800000, 0x404b4c7b, 0x404b4c7b, 0x404b4c7b, 0x00000000,
    0x00000000, 0x00000000, 0x3b503e3d, 0x3b81fbfb, 0x3b0dae49,
];

#[test]
fn golden_fit_gamut_is_bit_identical() {
    let fitted = through_fit_range(&fit_range_params(0.0, DisplayPeak::SDR));
    let params = FitGamutParams {
        target: DestinationGamut::DisplayP3,
    };
    let (out, gamut) = fit_gamut::apply(fitted, &params).unwrap().into_parts();
    assert_stage_bits("fit-gamut", &out.rgb, &FIT_GAMUT_P3);
    assert_eq!(gamut, DestinationGamut::DisplayP3);
    assert_eq!(out.ir.as_deref(), Some(&FILM_IR[..FINITE_PIXELS]));
}

/// [`FIT_GAMUT_P3`]'s input into Adobe RGB, which runs the same map through a
/// different pinned matrix and luma row. The cases are P3's except one: diffuse white
/// (pixel 3) lands just *inside* the Adobe RGB cube, so it passes untouched here where
/// P3 maps it. Captured 2026-09-24 when the gamut landed (`output/adobe-rgb-gamut`).
const FIT_GAMUT_ADOBE_RGB: [u32; 21] = [
    0x3e3851ec, 0x3e3851ee, 0x3e3851ea, 0x3b286ac0, 0x3b8311ce, 0x3f5cf55c, 0x3f1aabc1, 0x3cab980d,
    0x00000000, 0x3f800000, 0x3f800000, 0x3f7ffffe, 0x404b4c7b, 0x404b4c7b, 0x404b4c7b, 0x00000000,
    0x00000000, 0x00000000, 0x3b57470b, 0x3b831270, 0x3b087799,
];

#[test]
fn golden_fit_gamut_adobe_rgb_is_bit_identical() {
    let fitted = through_fit_range(&fit_range_params(0.0, DisplayPeak::SDR));
    let params = FitGamutParams {
        target: DestinationGamut::AdobeRgb,
    };
    let (out, gamut) = fit_gamut::apply(fitted, &params).unwrap().into_parts();
    assert_stage_bits("fit-gamut-adobe-rgb", &out.rgb, &FIT_GAMUT_ADOBE_RGB);
    assert_eq!(gamut, DestinationGamut::AdobeRgb);
    assert_eq!(out.ir.as_deref(), Some(&FILM_IR[..FINITE_PIXELS]));
}

/// [`FIT_GAMUT_P3`]'s input into BT.2020, the HDR destinations' gamut: the same map
/// through BT.2020's pinned matrix and luma row. Run against the SDR peak, as its
/// siblings are, so the vector stays bit-exact — fit range's HDR lift calls `log2`
/// above diffuse white; what this pins is the gamut's arithmetic, not the peak's. The
/// widest gamut takes pixel 2's negative P3 channel inside the cube, so it passes
/// through the matrix alone. Captured 2026-09-25 with the destination set
/// (`nf-destinations/preset-set`).
const FIT_GAMUT_BT2020: [u32; 21] = [
    0x3e3851ed, 0x3e3851ed, 0x3e3851ec, 0x3d2a344a, 0x3c661203, 0x3f4e7194, 0x3f0f4d19, 0x3d8a2a5d,
    0x00000000, 0x3f800000, 0x3f800000, 0x3f800000, 0x404b4c7d, 0x404b4c7d, 0x404b4c7d, 0x00000000,
    0x00000000, 0x00000000, 0x3b57596e, 0x3b80102d, 0x3b0faead,
];

#[test]
fn golden_fit_gamut_bt2020_is_bit_identical() {
    let fitted = through_fit_range(&fit_range_params(0.0, DisplayPeak::SDR));
    let params = FitGamutParams {
        target: DestinationGamut::Bt2020,
    };
    let (out, gamut) = fit_gamut::apply(fitted, &params).unwrap().into_parts();
    assert_stage_bits("fit-gamut-bt2020", &out.rgb, &FIT_GAMUT_BT2020);
    assert_eq!(gamut, DestinationGamut::Bt2020);
    assert_eq!(out.ir.as_deref(), Some(&FILM_IR[..FINITE_PIXELS]));
}

// --- threaded ----------------------------------------------------------------

/// The new flow's default destination: an SDR display in Display P3.
const SDR_P3: DisplayTarget = DisplayTarget {
    peak: DisplayPeak::SDR,
    gamut: DestinationGamut::DisplayP3,
};

/// Every stage at its shipped setting except scene correction, which is not an
/// identity on purpose — see the threaded goldens — and the look, which is off: no
/// pixel here is near-neutral after these gains, so it would change nothing. The look
/// has its own threaded vector below.
fn threaded_params(white_balance: WhiteBalance) -> ChainParams {
    ChainParams {
        shared: SharedParams {
            scene_correction: SceneCorrectionParams {
                white_balance,
                exposure: -1.0,
            },
            look: LookParams::off(),
            headroom_stops: DEFAULT_HEADROOM_STOPS,
        },
        target: SDR_P3,
    }
}

/// Recaptured 2026-09-24 with fit gamut's radial map: pixel 2's negative channel and
/// pixel 4's channel over 1 now land on the boundary.
const THREADED: [u32; 21] = [
    0x3e0b2f73, 0x3dc78824, 0x3d3fe5ca, 0x3cdcecf4, 0x3b9fc30d, 0x3e78815e, 0x3efb981a, 0x3c6fe399,
    0x00000000, 0x3f03c63e, 0x3ebce85e, 0x3e35ae16, 0x3f800000, 0x3f1ac0ab, 0x3e8eada9, 0x00000000,
    0x00000000, 0x00000000, 0x3b2f11a9, 0x3b1c7185, 0x3a1d5f9a,
];

#[test]
fn golden_the_chain_threaded_is_bit_identical() {
    // Non-identity scene correction on purpose: per-channel gains, fit range's
    // luminance scale and the destination matrix do not commute, so a chain that ran
    // them in another order, or handed a stage the wrong params, lands elsewhere even
    // though every per-stage golden passes.
    let params = threaded_params(WhiteBalance::Explicit([1.25, 1.0, 0.5]));
    let rendered = chain::render(finite_aces(), &params).unwrap();
    assert_eq!(
        rendered.applied,
        [
            ("scene_correction", "white-balance+exposure"),
            ("look", "identity"),
            ("fit_range", fit_range::OPERATOR),
            (
                "fit_gamut",
                "acescg-to-display-p3-matrix+neutral-axis-radial-boundary-v2"
            ),
        ],
        "chain (threaded): the stage list the render reports"
    );
    assert_eq!(
        rendered.scene_correction,
        SceneCorrection {
            white_balance: [1.25, 1.0, 0.5],
            exposure: -1.0,
        },
        "chain (threaded): the scene correction the render reports"
    );
    assert_eq!(
        rendered.fit_range,
        FitRange {
            operator: fit_range::OPERATOR,
            headroom_stops: DEFAULT_HEADROOM_STOPS,
            white_point: 64.0,
            display_peak: DisplayPeak::SDR,
        },
        "chain (threaded): the fit range the render reports"
    );
    let (out, gamut) = rendered.image.into_parts();
    assert_stage_bits("chain (threaded)", &out.rgb, &THREADED);
    assert_eq!(gamut, DestinationGamut::DisplayP3);
    assert_eq!(out.ir.as_deref(), Some(&FILM_IR[..FINITE_PIXELS]));
}

/// Two bright pixels for the look inside the chain: (0) near-neutral only *after* the
/// threaded white balance, so the look pulls it; (1) near-neutral only *before* it, so
/// the look leaves it. A look run ahead of scene correction swaps which one moves.
const LOOK_THREADED_FILM: [f32; 6] = [0.72, 1.2, 2.5, 1.25, 1.2, 1.18];

const THREADED_LOOK: [u32; 6] = [
    0x3f185f16, 0x3f1766d7, 0x3f16d046, 0x3f520133, 0x3f122d2c, 0x3e8a4b94,
];

#[test]
fn golden_the_look_threaded_runs_after_scene_correction() {
    let input = || {
        map_nc_film_rgb_v1(FilmRgbImage::fixture(
            LinearImage::new(2, 1, LOOK_THREADED_FILM.to_vec(), None).unwrap(),
        ))
    };
    let scene_correction = SceneCorrectionParams {
        white_balance: WhiteBalance::Explicit([1.25, 1.0, 0.5]),
        exposure: 0.0,
    };
    let params = |look| ChainParams {
        shared: SharedParams {
            scene_correction: scene_correction.clone(),
            look,
            headroom_stops: DEFAULT_HEADROOM_STOPS,
        },
        target: SDR_P3,
    };
    // Both pixels take a pure-IEEE path (full pull, or untouched), so pin them exactly.
    let (corrected, _) = scene_correction::apply(input(), &scene_correction).unwrap();
    let corrected = corrected.into_buffer().into_linear().rgb;
    for (p, q) in corrected.chunks(3).enumerate() {
        assert_clear_of_look_edges(&format!("threaded look pixel {p}"), [q[0], q[1], q[2]]);
    }

    let on = chain::render(input(), &params(look_params())).unwrap();
    assert_eq!(on.applied[1], ("look", "highlight-desaturation"));
    let on = on.image.into_parts().0.rgb;
    let off = chain::render(input(), &params(LookParams::off()))
        .unwrap()
        .image
        .into_parts()
        .0
        .rgb;
    assert_ne!(
        bits(&on[0..3]),
        bits(&off[0..3]),
        "pixel 0 is near-neutral after the white balance: the look pulls it"
    );
    assert_eq!(
        bits(&on[3..6]),
        bits(&off[3..6]),
        "pixel 1 is coloured after the white balance: the look leaves it"
    );
    assert_stage_bits("chain (threaded look)", &on, &THREADED_LOOK);
}

#[test]
fn the_chain_threaded_refuses_the_nan_pixel() {
    // Scene correction carries the NaN through; fit range is the stage that refuses
    // it, and names the pixel.
    let params = threaded_params(WhiteBalance::Explicit([1.25, 1.0, 0.5]));
    let err = chain::render(aces(), &params).err().expect("a NaN pixel");
    assert!(err.message().contains("pixel 7"), "{}", err.message());
}
