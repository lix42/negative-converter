//! The canonical binary64 derivation: standard definitions → derived artifacts.
//!
//! **This module is `#[cfg(test)]`.** That is a structural guarantee, not a
//! convenience: the task's rule is that runtime rendering uses reviewed,
//! checked-in coefficients and never derives them per run, and compiling the
//! derivation out of the shipping binary enforces it more reliably than a comment
//! could. It also means the module needs no `allow(dead_code)`.
//!
//! ## Fixed operation order
//!
//! The composed RGB→RGB derivation is pinned to
//!
//! ```text
//! M = inverse(NPM_dst) · ( CAT(src_white → dst_white) · NPM_src )
//! ```
//!
//! with 3-term dot products accumulated left to right. That order is recorded
//! because it is part of what makes regeneration reproducible — but it is *not*
//! load-bearing for correctness: a sweep over inverse algorithms (adjugate vs
//! Gauss-Jordan), both association orders, and four summation orders moves the
//! `f64` result by at most ~5 `f64` ulp (~1e-17), which is seven orders of
//! magnitude below the `f32` rounding step. Don't treat a change here as
//! dangerous; do treat it as needing a regenerated audit artifact.

use super::definitions::{Chromaticity, ColorSpace, ConeResponse};

pub type Matrix3 = [[f64; 3]; 3];

/// Left-to-right accumulation of a 3-term dot product.
fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn multiply(a: Matrix3, b: Matrix3) -> Matrix3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = dot3(a[i], [b[0][j], b[1][j], b[2][j]]);
        }
    }
    out
}

pub fn transform(m: Matrix3, v: [f64; 3]) -> [f64; 3] {
    [dot3(m[0], v), dot3(m[1], v), dot3(m[2], v)]
}

/// Exact 3×3 inverse by adjugate over determinant.
///
/// Panics on a singular matrix — every input here is a well-conditioned
/// colorimetric matrix, so a singular one means the source definitions are
/// corrupt and failing loudly is correct.
pub fn inverse(m: Matrix3) -> Matrix3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    assert!(
        det.is_finite() && det != 0.0,
        "singular colorimetric matrix (det = {det})"
    );
    let cof = |a: f64, b: f64, c: f64, d: f64| a * d - b * c;
    [
        [
            cof(m[1][1], m[1][2], m[2][1], m[2][2]) / det,
            -cof(m[0][1], m[0][2], m[2][1], m[2][2]) / det,
            cof(m[0][1], m[0][2], m[1][1], m[1][2]) / det,
        ],
        [
            -cof(m[1][0], m[1][2], m[2][0], m[2][2]) / det,
            cof(m[0][0], m[0][2], m[2][0], m[2][2]) / det,
            -cof(m[0][0], m[0][2], m[1][0], m[1][2]) / det,
        ],
        [
            cof(m[1][0], m[1][1], m[2][0], m[2][1]) / det,
            -cof(m[0][0], m[0][1], m[2][0], m[2][1]) / det,
            cof(m[0][0], m[0][1], m[1][0], m[1][1]) / det,
        ],
    ]
}

/// Normalized primary matrix: linear RGB → CIE XYZ, for a space's own white.
///
/// The primaries give the unscaled column directions; the per-column scale is the
/// one making `RGB = (1,1,1)` map to the adopted white.
pub fn normalized_primary_matrix(space: ColorSpace) -> Matrix3 {
    let columns = space.primaries.as_array().map(|c| c.to_xyz());
    let unscaled: Matrix3 = [
        [columns[0][0], columns[1][0], columns[2][0]],
        [columns[0][1], columns[1][1], columns[2][1]],
        [columns[0][2], columns[1][2], columns[2][2]],
    ];
    let scale = transform(inverse(unscaled), space.white.to_xyz());
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = unscaled[i][j] * scale[j];
        }
    }
    out
}

/// XYZ → XYZ chromatic adaptation between two whites, in a cone-response space.
///
/// Whether `cone` carries an explicit inverse or is inverted numerically is part
/// of the source data, not an implementation detail — see
/// [`BRADFORD_PUBLISHED_INVERSE`](super::definitions::BRADFORD_PUBLISHED_INVERSE).
pub fn adaptation(
    cone: ConeResponse,
    source_white: Chromaticity,
    destination_white: Chromaticity,
) -> Matrix3 {
    let forward = cone.matrix;
    let backward = cone.inverse.unwrap_or_else(|| inverse(forward));
    let source = transform(forward, source_white.to_xyz());
    let destination = transform(forward, destination_white.to_xyz());
    let gain: Matrix3 = [
        [destination[0] / source[0], 0.0, 0.0],
        [0.0, destination[1] / source[1], 0.0],
        [0.0, 0.0, destination[2] / source[2]],
    ];
    multiply(backward, multiply(gain, forward))
}

/// Composed linear RGB → linear RGB transform, adapting between adopted whites.
///
/// When the two spaces share a white point the adaptation term is skipped
/// entirely rather than multiplied by a near-identity matrix — that is both the
/// correct thing to do and what `gain_map::BT2020_TO_DISPLAY_P3` (D65 → D65) was
/// originally derived with.
pub fn rgb_to_rgb(source: ColorSpace, destination: ColorSpace, cone: ConeResponse) -> Matrix3 {
    let src = normalized_primary_matrix(source);
    let dst_inverse = inverse(normalized_primary_matrix(destination));
    if source.white == destination.white {
        return multiply(dst_inverse, src);
    }
    let cat = adaptation(cone, source.white, destination.white);
    multiply(dst_inverse, multiply(cat, src))
}

/// The luminance (Y) row of a space's normalized primary matrix — i.e. the luma
/// weights implied by its primaries and white.
///
/// Only meaningful for spaces whose standard does *not* tabulate rounded luma
/// coefficients. BT.2020 does tabulate them; see
/// [`definitions::BT2020_LUMA_TABULATED`](super::definitions::BT2020_LUMA_TABULATED).
pub fn luma_row(space: ColorSpace) -> [f64; 3] {
    normalized_primary_matrix(space)[1]
}
