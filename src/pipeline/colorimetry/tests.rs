//! **Category 4 — verification.** Tolerances, invariants, and independent
//! reference vectors.
//!
//! The organizing rule: an oracle that shares a source with the thing it checks
//! validates nothing. [`audit`](super::audit) already proves each shipped literal
//! matches *our* derivation from *our* definitions — which would still pass if a
//! primary were mistyped in `definitions.rs`. The tests here exist to catch that
//! class of error, so each one is anchored on something outside the derivation:
//! an externally published matrix, a chromaticity recovered from the standard's
//! own numbers, or a structural invariant that must hold for any correct
//! white-adapted transform.
//!
//! ## Where each space's external anchor actually lives
//!
//! Not all of them are in this file, and a reader auditing coverage needs the
//! map:
//!
//! | Space | External anchor | Where |
//! |---|---|---|
//! | Rec.709 / ACEScg | published sRGB↔ACEScg Bradford matrix | this file |
//! | BT.2020 | primaries re-typed from BT.2020-2, recovered through the transform | this file |
//! | Display P3 | ICC-registry colorants and the registered D65 encoding | `pipeline::color` tests |
//! | ProPhoto | none — no NC-derived matrix exists for it (Little CMS owns that colorimetry) | — |
//!
//! **The recovery test anchors the *source* space only.** In
//! `transformed_primaries_recover_the_standards_chromaticities`, the destination
//! NPM appears both in the pinned matrix and in the recovery step
//! (`NPM_dst · NPM_dst⁻¹ · NPM_src == NPM_src`), so it cancels: a mistyped
//! *destination* primary is invisible to it. That is why Display P3 needs its own
//! anchor elsewhere rather than being covered transitively here.
//!
//! This was measured, not assumed. Tampering `DISPLAY_P3` by one digit
//! (`0.680 → 0.690`), regenerating the audit artifact *and* re-pinning every
//! affected matrix so definition, derivation and pin all agree — the
//! self-validating scenario — leaves every test in this file green and is caught
//! by `color`'s `display_p3_colorants_match_icc_registry_reference` and
//! `display_p3_decodes_to_registered_d65_encoding`. Deleting or loosening those
//! two removes Display P3's only real anchor.
//!
//! Sub-rounding perturbations (below the three decimals the standards actually
//! specify) are deliberately *not* covered by anything. They are not errors: the
//! standard does not define the value to that precision, and the check tolerances
//! are sized against that fact rather than against arithmetic noise.

use super::audit::ulps_f32;
use super::definitions::{
    ACESCG, BRADFORD, BRADFORD_PUBLISHED_INVERSE, BT2020, BT2020_LUMA_TABULATED, ColorSpace, D50,
    D65, DISPLAY_P3, PROPHOTO, REC709,
};
use super::derive::{self, Matrix3, inverse, multiply, rgb_to_rgb, transform};
use super::pinned;

/// Largest permitted disagreement between a shipped `f32` literal and the
/// canonical derivation.
///
/// **One ulp, and the justification is measured, not assumed.** Three of the 36
/// shipped matrix entries sit exactly one ulp from the canonical derivation (see
/// [`pinned`] for which, and why the historical route is unrecoverable). Tightening
/// this to zero would mean re-pinning those three — a pixel change. Loosening it
/// would stop catching real transcription errors, which are ≥ 2 ulps in practice.
///
/// For scale: the chromaticities involved are specified to three decimals, and
/// perturbing one primary by its own ±5e-4 rounding moves entries by up to
/// 4.2e-4, roughly 3,500 ulps. This tolerance is three orders of magnitude
/// tighter than the standards' own precision.
const MAX_ULPS: i64 = 1;

/// Every `[i][j]` position of a 3×3 matrix.
///
/// Comparisons below name the failing position in their message, so they need
/// the indices rather than just the values.
fn cells() -> impl Iterator<Item = (usize, usize)> {
    (0..3).flat_map(|i| (0..3).map(move |j| (i, j)))
}

fn assert_matrix_within_tolerance(name: &str, derived: Matrix3, shipped: [[f32; 3]; 3]) {
    for (i, j) in cells() {
        let ulps = ulps_f32(derived[i][j] as f32, shipped[i][j]);
        assert!(
            ulps.abs() <= MAX_ULPS,
            "{name}[{i}][{j}]: shipped {} is {ulps} ulps from derived {} (limit {MAX_ULPS})",
            shipped[i][j],
            derived[i][j] as f32,
        );
    }
}

// -- every pinned artifact reproduces its derivation --------------------------

#[test]
fn pinned_display_matrices_reproduce_the_canonical_derivation() {
    assert_matrix_within_tolerance(
        "ACESCG_TO_SRGB",
        rgb_to_rgb(ACESCG, REC709, BRADFORD),
        pinned::ACESCG_TO_SRGB,
    );
    assert_matrix_within_tolerance(
        "ACESCG_TO_DISPLAY_P3",
        rgb_to_rgb(ACESCG, DISPLAY_P3, BRADFORD),
        pinned::ACESCG_TO_DISPLAY_P3,
    );
    assert_matrix_within_tolerance(
        "ACESCG_TO_BT2020",
        rgb_to_rgb(ACESCG, BT2020, BRADFORD),
        pinned::ACESCG_TO_BT2020,
    );
    assert_matrix_within_tolerance(
        "BT2020_TO_DISPLAY_P3",
        rgb_to_rgb(BT2020, DISPLAY_P3, BRADFORD),
        pinned::BT2020_TO_DISPLAY_P3,
    );
}

#[test]
fn nc_film_rgb_v1_reproduces_its_historical_published_inverse_derivation() {
    // v1 is a frozen versioned identifier and is held in f64, so it gets an f64
    // tolerance and the historical cone-response convention. 1e-12 is far below
    // the f32 store the mapper performs.
    let derived = rgb_to_rgb(REC709, ACESCG, BRADFORD_PUBLISHED_INVERSE);
    for (i, j) in cells() {
        let error = (derived[i][j] - pinned::NC_FILM_RGB_V1_TO_ACESCG[i][j]).abs();
        assert!(
            error < 1e-12,
            "NC_FILM_RGB_V1_TO_ACESCG[{i}][{j}]: {} vs derived {} (err {error:.2e})",
            pinned::NC_FILM_RGB_V1_TO_ACESCG[i][j],
            derived[i][j],
        );
    }
}

#[test]
fn the_two_bradford_conventions_are_genuinely_different_and_v1_needs_its_own() {
    // Guards the reason `BRADFORD_PUBLISHED_INVERSE` exists. If someone "tidies
    // up" by pointing v1 at the canonical BRADFORD, this fails loudly rather
    // than shifting the frozen mapping by 9.1e-8.
    let canonical = rgb_to_rgb(REC709, ACESCG, BRADFORD);
    let historical = rgb_to_rgb(REC709, ACESCG, BRADFORD_PUBLISHED_INVERSE);
    let gap = (0..3)
        .flat_map(|i| (0..3).map(move |j| (i, j)))
        .map(|(i, j)| (canonical[i][j] - historical[i][j]).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        (5e-8..2e-7).contains(&gap),
        "expected the two Bradford conventions to differ by ~9e-8, got {gap:.2e}"
    );
    let shipped_gap = (0..3)
        .flat_map(|i| (0..3).map(move |j| (i, j)))
        .map(|(i, j)| (canonical[i][j] - pinned::NC_FILM_RGB_V1_TO_ACESCG[i][j]).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        shipped_gap > 1e-8,
        "v1 unexpectedly matches the canonical convention ({shipped_gap:.2e}); \
         BRADFORD_PUBLISHED_INVERSE may no longer be needed — re-check before removing it"
    );
}

// -- independent external anchors ---------------------------------------------

#[test]
fn acescg_to_srgb_matches_the_externally_published_matrix() {
    // Independent oracle: the widely-published sRGB-linear <-> ACEScg Bradford
    // matrix (colour-science / OpenColorIO ACES, e.g.
    // `colour.matrix_RGB_to_RGB(sRGB, ACEScg, "Bradford")`), quoted at 4 dp.
    // These are externally authored numbers, so unlike the audit they would
    // catch a mistyped primary in our own definitions.
    //
    // It is quoted in the sRGB->ACEScg direction, so invert the shipped
    // ACEScg->sRGB matrix to compare. Tolerance 1e-3 absorbs both the 4 dp
    // rounding and its amplification through the inversion.
    const PUBLISHED_SRGB_TO_ACESCG: Matrix3 = [
        [0.6131, 0.3395, 0.0474],
        [0.0702, 0.9164, 0.0134],
        [0.0206, 0.1096, 0.8698],
    ];
    let shipped_inverse = inverse(pinned::ACESCG_TO_SRGB.map(|row| row.map(|v| v as f64)));
    for (i, j) in cells() {
        let error = (shipped_inverse[i][j] - PUBLISHED_SRGB_TO_ACESCG[i][j]).abs();
        assert!(
            error < 1e-3,
            "inverse(ACESCG_TO_SRGB)[{i}][{j}] = {} vs published {} (err {error:.2e})",
            shipped_inverse[i][j],
            PUBLISHED_SRGB_TO_ACESCG[i][j],
        );
    }
}

#[test]
fn nc_film_rgb_v1_matches_the_same_published_matrix_in_its_own_direction() {
    const PUBLISHED_SRGB_TO_ACESCG: Matrix3 = [
        [0.6131, 0.3395, 0.0474],
        [0.0702, 0.9164, 0.0134],
        [0.0206, 0.1096, 0.8698],
    ];
    for (i, j) in cells() {
        let error = (pinned::NC_FILM_RGB_V1_TO_ACESCG[i][j] - PUBLISHED_SRGB_TO_ACESCG[i][j]).abs();
        assert!(error < 1e-4, "v1[{i}][{j}] vs published (err {error:.2e})");
    }
}

#[test]
fn transformed_primaries_recover_the_standards_chromaticities() {
    // The strongest independent anchor available without quoting an external
    // matrix: push each source primary through the shipped matrix, convert the
    // result to XYZ with the destination's normalized primary matrix, and recover
    // the chromaticity. It must come back as the *source standard's* published
    // primary chromaticity.
    //
    // Restricted to the shared-white pair: with a chromatic adaptation in the
    // chain the recovered chromaticity is the adapted one, which is not a
    // published number.
    //
    // **The expected values below are re-typed from ITU-R BT.2020-2 on purpose
    // and must stay that way.** Writing `BT2020.primaries` here instead would be
    // shorter and would look equivalent — and it would destroy the only thing
    // this test contributes. The documented way to change a colour space is "edit
    // the definition, then re-pin the matrix" (`docs/colorimetry-maintenance.md`),
    // so in the flow that matters the definition and the pinned matrix move
    // *together*: a typo in `definitions::BT2020` would be faithfully carried into
    // the re-pinned matrix, recovered back out here, and compared against itself.
    // Independently transcribed literals are what make that self-validation fail
    // instead of pass — which is the property the task requires of at least one
    // reference per transform. Do not "tidy this up" by pointing it at the const.
    const BT2020_PRIMARIES_FROM_THE_STANDARD: [(f64, f64); 3] =
        [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)];

    let matrix = pinned::BT2020_TO_DISPLAY_P3.map(|row| row.map(|v| v as f64));
    let destination_npm = derive::normalized_primary_matrix(DISPLAY_P3);

    for (channel, &(want_x, want_y)) in BT2020_PRIMARIES_FROM_THE_STANDARD.iter().enumerate() {
        let mut rgb = [0.0; 3];
        rgb[channel] = 1.0;
        let xyz = transform(destination_npm, transform(matrix, rgb));
        let sum = xyz[0] + xyz[1] + xyz[2];
        let (x, y) = (xyz[0] / sum, xyz[1] / sum);
        assert!(
            (x - want_x).abs() < 1e-6 && (y - want_y).abs() < 1e-6,
            "channel {channel}: recovered ({x:.6}, {y:.6}), standard says ({want_x}, {want_y})",
        );
    }
}

// -- structural invariants ----------------------------------------------------

#[test]
fn every_white_adapted_matrix_maps_neutral_to_neutral() {
    // Rows summing to 1 is exactly "source white maps to destination white". A
    // missing or wrong chromatic adaptation tints white, and this catches it
    // without reference to any derivation.
    let matrices: [(&str, [[f64; 3]; 3]); 5] = [
        ("NC_FILM_RGB_V1_TO_ACESCG", pinned::NC_FILM_RGB_V1_TO_ACESCG),
        (
            "ACESCG_TO_SRGB",
            pinned::ACESCG_TO_SRGB.map(|r| r.map(|v| v as f64)),
        ),
        (
            "ACESCG_TO_DISPLAY_P3",
            pinned::ACESCG_TO_DISPLAY_P3.map(|r| r.map(|v| v as f64)),
        ),
        (
            "ACESCG_TO_BT2020",
            pinned::ACESCG_TO_BT2020.map(|r| r.map(|v| v as f64)),
        ),
        (
            "BT2020_TO_DISPLAY_P3",
            pinned::BT2020_TO_DISPLAY_P3.map(|r| r.map(|v| v as f64)),
        ),
    ];
    for (name, m) in matrices {
        for (i, row) in m.iter().enumerate() {
            let sum: f64 = row.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-6,
                "{name} row {i} sums to {sum}, not 1 — neutral would be tinted"
            );
        }
    }
}

#[test]
fn matrices_round_trip_through_their_inverses() {
    for (name, m) in [
        ("ACESCG_TO_SRGB", rgb_to_rgb(ACESCG, REC709, BRADFORD)),
        (
            "ACESCG_TO_DISPLAY_P3",
            rgb_to_rgb(ACESCG, DISPLAY_P3, BRADFORD),
        ),
        ("ACESCG_TO_BT2020", rgb_to_rgb(ACESCG, BT2020, BRADFORD)),
        (
            "BT2020_TO_DISPLAY_P3",
            rgb_to_rgb(BT2020, DISPLAY_P3, BRADFORD),
        ),
    ] {
        let identity = multiply(m, inverse(m));
        for (i, j) in cells() {
            let want = if i == j { 1.0 } else { 0.0 };
            assert!(
                (identity[i][j] - want).abs() < 1e-12,
                "{name}: M·M⁻¹ is not the identity at [{i}][{j}] ({})",
                identity[i][j],
            );
        }
    }
}

#[test]
fn matrix_direction_is_not_reversed() {
    // A transposed or reversed matrix would still pass the row-sum test, so pin
    // direction separately: ACEScg is wider than Rec.709, so a saturated Rec.709
    // primary expressed in ACEScg must stay inside [0,1], while an ACEScg
    // primary expressed in Rec.709 must go negative (outside the smaller gamut).
    let to_acescg = rgb_to_rgb(REC709, ACESCG, BRADFORD);
    let rec709_red_in_acescg = transform(to_acescg, [1.0, 0.0, 0.0]);
    assert!(
        rec709_red_in_acescg.iter().all(|&c| c >= -1e-9),
        "Rec.709 red should be inside ACEScg, got {rec709_red_in_acescg:?}"
    );

    let to_rec709 = pinned::ACESCG_TO_SRGB.map(|r| r.map(|v| v as f64));
    let acescg_green_in_rec709 = transform(to_rec709, [0.0, 1.0, 0.0]);
    assert!(
        acescg_green_in_rec709.iter().any(|&c| c < -0.01),
        "ACEScg green should fall outside Rec.709, got {acescg_green_in_rec709:?} \
         — the matrix may be inverted or transposed"
    );
}

#[test]
fn white_point_adaptation_actually_happens() {
    // Distinguishes a real CAT from "no adaptation at all". Without adaptation,
    // ACEScg's white would land off-neutral in D65 space by ~1e-2 — far above the
    // 1e-6 the row-sum test tolerates.
    let unadapted = {
        let src = derive::normalized_primary_matrix(ACESCG);
        let dst = inverse(derive::normalized_primary_matrix(REC709));
        multiply(dst, src)
    };
    let neutral = transform(unadapted, [1.0, 1.0, 1.0]);
    let spread = neutral.iter().cloned().fold(f64::MIN, f64::max)
        - neutral.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
        spread > 1e-2,
        "an unadapted ACEScg→Rec.709 matrix should tint neutral noticeably \
         (got spread {spread:.2e}); if it does not, the two white points may have \
         become equal and the adaptation tests are vacuous"
    );

    let adapted = transform(rgb_to_rgb(ACESCG, REC709, BRADFORD), [1.0, 1.0, 1.0]);
    let adapted_spread = adapted.iter().cloned().fold(f64::MIN, f64::max)
        - adapted.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
        adapted_spread < 1e-9,
        "adapted neutral is tinted: {adapted:?}"
    );
}

#[test]
fn shared_white_pairs_use_no_adaptation() {
    // BT.2020 and Display P3 are both D65, so `rgb_to_rgb` must skip the CAT
    // entirely. Applying a D65→D65 Bradford would be a near-identity but not an
    // identity, and would shift the matrix off the shipped literal.
    assert_eq!(BT2020.white, D65);
    assert_eq!(DISPLAY_P3.white, D65);
    let skipped = rgb_to_rgb(BT2020, DISPLAY_P3, BRADFORD);
    let forced = {
        let cat = derive::adaptation(BRADFORD, D65, D65);
        let src = derive::normalized_primary_matrix(BT2020);
        let dst = inverse(derive::normalized_primary_matrix(DISPLAY_P3));
        multiply(dst, multiply(cat, src))
    };
    // They agree colorimetrically, but the shipped literal was derived the
    // skipped way; assert the code takes that path.
    for (i, j) in cells() {
        assert!(
            (skipped[i][j] - forced[i][j]).abs() < 1e-12,
            "a D65→D65 adaptation should be a no-op at [{i}][{j}]"
        );
    }
    assert_matrix_within_tolerance(
        "BT2020_TO_DISPLAY_P3",
        skipped,
        pinned::BT2020_TO_DISPLAY_P3,
    );
}

// -- luma vectors -------------------------------------------------------------

#[test]
fn display_p3_luma_is_the_derived_luminance_row() {
    let derived = derive::luma_row(DISPLAY_P3);
    for (i, (&derived, &shipped)) in derived.iter().zip(&pinned::DISPLAY_P3_LUMA).enumerate() {
        let ulps = ulps_f32(derived as f32, shipped);
        assert!(
            ulps.abs() <= MAX_ULPS,
            "DISPLAY_P3_LUMA[{i}]: shipped {shipped} is {ulps} ulps from derived {}",
            derived as f32,
        );
    }
}

/// Largest permitted disagreement for [`pinned::SRGB_LUMA`].
///
/// This vector gets its own tolerance because it is neither an exact derivation
/// nor a normative table: it is the derivation rounded to six decimals, which
/// puts the blue weight 43 ulps out. Relaxing the shared [`MAX_ULPS`] to cover it
/// would blind every matrix check; a separate, named allowance keeps the strict
/// bound where it belongs and documents the loose one where it is needed.
const SRGB_LUMA_MAX_ULPS: i64 = 43;

#[test]
fn srgb_luma_is_the_derivation_rounded_to_six_decimals() {
    // Pins the actual relationship rather than asserting a bound nobody checked:
    // each shipped entry must equal the canonical derivation rounded to 6 dp, and
    // must sit within the documented ulp allowance of it.
    let derived = derive::luma_row(REC709);
    for (i, (&d, &shipped)) in derived.iter().zip(&pinned::SRGB_LUMA).enumerate() {
        let rounded_to_6dp = (d * 1e6).round() / 1e6;
        assert_eq!(
            shipped, rounded_to_6dp as f32,
            "SRGB_LUMA[{i}] should be the derivation rounded to 6 decimals"
        );
        let ulps = ulps_f32(d as f32, shipped);
        assert!(
            ulps.abs() <= SRGB_LUMA_MAX_ULPS,
            "SRGB_LUMA[{i}]: shipped {shipped} is {ulps} ulps from derived {}",
            d as f32,
        );
    }
    // The gap is real and worth keeping visible: if a future change makes the
    // shipped vector exact, this fails and the 6-dp story above must be retired
    // rather than left as a misleading comment.
    let worst = derived
        .iter()
        .zip(&pinned::SRGB_LUMA)
        .map(|(&d, &s)| ulps_f32(d as f32, s).abs())
        .max()
        .unwrap();
    assert_eq!(
        worst, SRGB_LUMA_MAX_ULPS,
        "the 6-decimal rounding gap moved; re-read pinned::SRGB_LUMA's docs"
    );
}

#[test]
fn bt2020_luma_is_the_tabulated_vector_not_a_derivation() {
    // The load-bearing distinction from `DISPLAY_P3_LUMA`. BT.2020 tabulates
    // rounded luma coefficients and encoders are expected to use them, so the
    // rule for this vector is exact equality with the table...
    for (i, (&shipped, &tabulated)) in pinned::BT2020_LUMA
        .iter()
        .zip(&BT2020_LUMA_TABULATED)
        .enumerate()
    {
        assert_eq!(
            shipped, tabulated as f32,
            "BT2020_LUMA[{i}] must equal the tabulated value exactly"
        );
    }

    // ...and *not* agreement with a derivation, which differs by ~2e-6. Asserting
    // that gap keeps anyone from "fixing" the tabulated vector to the derived one.
    let derived = derive::luma_row(BT2020);
    let gap = (0..3)
        .map(|i| (derived[i] - BT2020_LUMA_TABULATED[i]).abs())
        .fold(0.0_f64, f64::max);
    assert!(
        (1e-6..1e-5).contains(&gap),
        "expected the tabulated and derived BT.2020 luma to differ by ~2e-6, got {gap:.2e}"
    );
}

#[test]
fn luma_vectors_sum_to_one() {
    for (name, v) in [
        ("BT2020_LUMA", pinned::BT2020_LUMA),
        ("DISPLAY_P3_LUMA", pinned::DISPLAY_P3_LUMA),
        ("SRGB_LUMA", pinned::SRGB_LUMA),
    ] {
        let sum: f32 = v.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "{name} sums to {sum}: neutral would not preserve luminance"
        );
    }
}

#[test]
fn pinned_luma_agrees_with_the_pinned_matrix_it_is_applied_after() {
    // The two pinned artifacts the gain-map path uses back to back: pixels are
    // brought into Display P3 with `BT2020_TO_DISPLAY_P3`, then their luminance is
    // taken with `DISPLAY_P3_LUMA`. Neither artifact's own test can catch a drift
    // *between* them — each is only compared against its own derivation — yet a
    // drift is exactly what would make gain-map luminance stop describing the
    // pixels it is computed from.
    //
    // The oracle is that CIE Y is absolute and gamut-independent: a colour has one
    // luminance, whichever RGB space expresses it. So take Y for the *source*
    // BT.2020 colour straight off the BT.2020 normalized primary matrix — a
    // quantity that touches neither pinned literal — and require the pinned pair
    // to reproduce it.
    let matrix = pinned::BT2020_TO_DISPLAY_P3.map(|row| row.map(|v| v as f64));
    let luma = pinned::DISPLAY_P3_LUMA.map(|v| v as f64);
    let bt2020_npm = derive::normalized_primary_matrix(BT2020);

    // Non-neutral on purpose. A neutral passes on the row sums alone (both
    // vectors sum to 1) and says nothing about the individual weights; each
    // primary isolates one weight, and the mixtures would catch a compensating
    // pair of errors that the primaries happened to hide.
    //
    // Tolerance, measured rather than guessed: both literals are `f32` stores of
    // an `f64` derivation and a few entries are pinned a further ulp off it, so
    // each contributes ~1e-7 *relative*. Over these colours the disagreement
    // actually reaches 1.3e-8 absolute (worst case: pure BT.2020 green). 1e-7
    // sits ~8x above that — enough that an `f32` re-pin within the existing
    // MAX_ULPS budget cannot trip it — and five orders of magnitude below the
    // 1.4e-3 that a genuinely wrong luma weight produces (verified by perturbing
    // DISPLAY_P3_LUMA by 1e-3, which fails this test on the first colour).
    for rgb in [
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.8, 0.3, 0.1],
        [0.15, 0.62, 0.94],
    ] {
        let in_p3 = transform(matrix, rgb);
        let via_pinned: f64 = luma.iter().zip(&in_p3).map(|(w, c)| w * c).sum();
        let reference_y = transform(bt2020_npm, rgb)[1];
        assert!(
            (via_pinned - reference_y).abs() < 1e-7,
            "BT.2020 {rgb:?}: DISPLAY_P3_LUMA over BT2020_TO_DISPLAY_P3 gives Y \
             {via_pinned}, but the colour's own luminance is {reference_y} \
             (err {:.2e}) — the two pinned artifacts disagree",
            (via_pinned - reference_y).abs(),
        );
    }
}

// -- BT.2020 NCL Y'CbCr (AVIF matrix_coefficients = 9) ------------------------

/// The chroma coefficients as *published* in rounded form, independent of NC's
/// derivation.
///
/// BT.2020-2 § 3.4 and BT.2100-2 Table 6 give the Y'CbCr conversion as formulas
/// in `Kr`/`Kb` rather than as a coefficient table, so the external anchor is the
/// widely republished four-decimal evaluation of those formulas (the same values
/// carried by ffmpeg, libavif and dav1d's colour tables). Four decimals is all
/// the published form commits to, which sets the tolerance below.
const BT2020_YCBCR_PUBLISHED_ROUNDED: [[f64; 3]; 3] = [
    [0.2627, 0.6780, 0.0593],
    [-0.1396, -0.3604, 0.5000],
    [0.5000, -0.4598, -0.0402],
];

#[test]
fn bt2020_ycbcr_matches_the_published_rounded_coefficients() {
    for (i, (row, published)) in pinned::BT2020_NCL_RGB_TO_YCBCR
        .iter()
        .zip(BT2020_YCBCR_PUBLISHED_ROUNDED)
        .enumerate()
    {
        for (j, (&shipped, want)) in row.iter().zip(published).enumerate() {
            // Half a unit in the published last decimal place.
            assert!(
                (f64::from(shipped) - want).abs() <= 5e-5,
                "BT2020_NCL_RGB_TO_YCBCR[{i}][{j}] = {shipped} disagrees with the \
                 published {want}",
            );
        }
    }
}

/// Bradford D65→D50 adapted BT.2020 colorants, as **Little CMS independently
/// computes them** — read out of a synthesized BT.2020 profile with `exiftool`
/// during `output/lossless-hdr-tiff` chunk A:
///
/// ```text
/// RedMatrixColumn:   0.67348  0.27904  -0.00194
/// GreenMatrixColumn: 0.16566  0.67534   0.02998
/// BlueMatrixColumn:  0.12505  0.04561   0.79684
/// ```
///
/// A genuinely independent anchor: a different implementation (lcms's own
/// adaptation code, quantized through ICC `s15Fixed16` and printed to five
/// decimals by a third tool) reaching the same matrix nc derives in binary64.
const BT2020_XYZ_D50_LCMS_OBSERVED: [[f64; 3]; 3] = [
    [0.67348, 0.16566, 0.12505],
    [0.27904, 0.67534, 0.04561],
    [-0.00194, 0.02998, 0.79684],
];

#[test]
fn bt2020_to_xyz_d50_matches_the_colorants_little_cms_computes() {
    for (i, (row, observed)) in pinned::BT2020_TO_XYZ_D50
        .iter()
        .zip(BT2020_XYZ_D50_LCMS_OBSERVED)
        .enumerate()
    {
        for (j, (&shipped, want)) in row.iter().zip(observed).enumerate() {
            // 2.5e-4 covers ICC `s15Fixed16` quantization plus `exiftool`'s
            // five-decimal printing; the worst observed entry is ~2.2e-4 (blue Z).
            assert!(
                (shipped - want).abs() <= 2.5e-4,
                "BT2020_TO_XYZ_D50[{i}][{j}] = {shipped} disagrees with the colorants \
                 Little CMS independently computed ({want})",
            );
        }
    }
}

#[test]
fn bt2020_to_xyz_d50_maps_white_to_the_d50_adopted_white() {
    // The defining property of an ICC colorant matrix: R=G=B=1 must land on the
    // PCS adopted white. `MediaWhitePointTag` states D50, so if this failed the
    // profile would claim a white it does not produce — and every neutral would
    // carry a tint no colorant check on its own would reveal.
    let sum: [f64; 3] =
        std::array::from_fn(|i: usize| -> f64 { pinned::BT2020_TO_XYZ_D50[i].iter().sum() });
    let d50 = D50.to_xyz();
    for (axis, (&got, want)) in sum.iter().zip(d50).enumerate() {
        assert!(
            (got - want).abs() < 1e-12,
            "column sum axis {axis} = {got}, D50 adopted white is {want}",
        );
    }
}

#[test]
fn bt2020_ycbcr_luma_row_is_the_tabulated_vector_verbatim() {
    // Not merely equal to within a tolerance: the row must *be* the same pinned
    // literal, so a future edit to one cannot silently desynchronize the encoder's
    // Y' row from the luma vector `pipeline::hdr` uses.
    assert_eq!(pinned::BT2020_NCL_RGB_TO_YCBCR[0], pinned::BT2020_LUMA);
    for (i, (&shipped, tabulated)) in pinned::BT2020_NCL_RGB_TO_YCBCR[0]
        .iter()
        .zip(BT2020_LUMA_TABULATED)
        .enumerate()
    {
        assert_eq!(
            shipped, tabulated as f32,
            "Y' row entry [{i}] is not the tabulated BT.2020 luma weight",
        );
    }
}

#[test]
fn bt2020_ycbcr_normalization_puts_the_primary_extremes_at_exactly_half() {
    // The `0.5` entries are the *reason* the chroma rows are scaled by
    // `2(1-Kb)` / `2(1-Kr)`: full blue must land at Cb = +0.5 and full red at
    // Cr = +0.5 exactly, which is what keeps a full-range signal inside its
    // code-value budget. Exact equality is required, not a tolerance.
    let blue = apply_f32(pinned::BT2020_NCL_RGB_TO_YCBCR, [0.0, 0.0, 1.0]);
    let red = apply_f32(pinned::BT2020_NCL_RGB_TO_YCBCR, [1.0, 0.0, 0.0]);
    assert_eq!(blue[1], 0.5, "full blue must give Cb = +0.5 exactly");
    assert_eq!(red[2], 0.5, "full red must give Cr = +0.5 exactly");
}

#[test]
fn bt2020_ycbcr_maps_achromatic_input_to_zero_chroma() {
    // The invariant that matters for a neutral ramp: equal R'G'B' must produce
    // zero chroma, or the encoder tints greys.
    //
    // Both bounds are *measured maxima* over the exact 10-bit code ladder this
    // encoder quantizes onto — not round numbers. The worst chroma residual is
    // 2^-25 at code 546 and the worst luma residual is 2^-23; the chroma figure is
    // ~32,800x smaller than one 10-bit code value, so it cannot move a sample.
    // Sweeping the real ladder rather than a coarse fraction matters: a 33-step
    // sweep understates the peak by 8x, because the residual is a rounding
    // artifact that peaks near 0.5 rather than growing monotonically.
    const MAX_CHROMA_RESIDUAL: f32 = 2.980_233e-8; // 2^-25, measured
    const MAX_LUMA_RESIDUAL: f32 = 1.192_093e-7; // 2^-23, measured
    for code in 0..=1023_u32 {
        let v = code as f32 / 1023.0;
        let [y, cb, cr] = apply_f32(pinned::BT2020_NCL_RGB_TO_YCBCR, [v, v, v]);
        assert!(
            cb.abs() <= MAX_CHROMA_RESIDUAL && cr.abs() <= MAX_CHROMA_RESIDUAL,
            "achromatic code {code} ({v}) produced chroma ({cb:e}, {cr:e})",
        );
        assert!(
            (y - v).abs() <= MAX_LUMA_RESIDUAL,
            "achromatic code {code} produced Y' {y}, which is not the input back",
        );
    }
}

#[test]
fn bt2020_ycbcr_round_trips_through_its_own_inverse() {
    // A decoder inverts this matrix, so an ill-conditioned pin would show up as a
    // round-trip error even while every entry looked plausible.
    let forward = pinned::BT2020_NCL_RGB_TO_YCBCR.map(|row| row.map(f64::from));
    let back = inverse(forward);
    for rgb in [
        [0.0, 0.0, 0.0],
        [1.0, 1.0, 1.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.25, 0.5, 0.75],
        [0.9, 0.1, 0.4],
    ] {
        let recovered = transform(back, transform(forward, rgb));
        for (i, (got, want)) in recovered.iter().zip(rgb).enumerate() {
            assert!(
                (got - want).abs() < 1e-12,
                "channel {i} of {rgb:?} round-tripped to {got}",
            );
        }
    }
}

/// Apply a pinned `f32` matrix the way the encoder does, so the tests observe the
/// same accumulation order the shipped code uses.
fn apply_f32(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

// -- definitions sanity -------------------------------------------------------

#[test]
fn every_defined_space_has_a_sane_gamut() {
    for space in [REC709, DISPLAY_P3, BT2020, ACESCG] {
        let npm = derive::normalized_primary_matrix(space);
        // White maps to the adopted white.
        let white = transform(npm, [1.0, 1.0, 1.0]);
        for (got, want) in white.iter().zip(space.white.to_xyz()) {
            assert!(
                (got - want).abs() < 1e-12,
                "{}: RGB white does not map to the adopted white point",
                space.name
            );
        }
        // Luminance weights are positive and sum to 1.
        let luma = npm[1];
        assert!(
            luma.iter().all(|&w| w > 0.0),
            "{}: negative luma weight {luma:?}",
            space.name
        );
        assert!((luma.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }
}

#[test]
fn color_space_names_are_unique() {
    // PROPHOTO belongs here even though it is absent from
    // `every_defined_space_has_a_sane_gamut`: a name collision is a name
    // collision regardless of whether the space's gamut invariants apply.
    let spaces: [ColorSpace; 5] = [REC709, DISPLAY_P3, BT2020, ACESCG, PROPHOTO];
    for (i, a) in spaces.iter().enumerate() {
        for b in &spaces[i + 1..] {
            assert_ne!(a.name, b.name, "duplicate colour-space name {}", a.name);
        }
    }
}
