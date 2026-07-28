//! NC film RGB v1 working-space mapping (design-spec §7, stage 4):
//!
//! ```text
//! FilmRgbImage → NC film RGB v1 interpretation → linear ACEScg/AP1 at D60
//! ```
//!
//! This is the one deterministic mapping shared by `simple` reconstruction and
//! both density curves (exponential / sigmoid). It expresses NC's
//! **film-rendering intent** — it does *not* claim to recover physically neutral
//! scene color, and it deliberately preserves the differences caused by film
//! stock, lens, development, scanner, and the selected density curve. It adds no
//! fitted curves or matrices.
//!
//! ## The mapping (pinned as "nc-film-rgb-v1")
//! 1. **Interpret** the reconstructed film RGB as **linear Rec.709 primaries at
//!    D65** — the existing intentional interpretation the output color stage
//!    already uses for the working space (`pipeline::color`).
//! 2. **Primary transform + chromatic adaptation** into **linear ACEScg (AP1)
//!    at the ACES white point (~D60)**, via the Bradford CAT.
//!
//! The composed 3×3 matrix is pinned as [`NC_FILM_RGB_V1_TO_ACESCG`] and applied
//! per pixel. A future mapping change requires a **new identifier** and a
//! behavioral-version decision by `conversion-versioning`; it must never
//! silently alter v1.
//!
//! ## Typed boundary
//! [`AcesCgImage`] has private fields and a module-private constructor, so
//! [`map_nc_film_rgb_v1`] — the only function in this module that builds one — is
//! the sole producer, and that half is **compiler-enforced**. The named-output split
//! (`pipeline::render_split`: the `film-master` branch and the shared display
//! controls) accepts an `AcesCgImage` and nothing else, so a raw
//! [`FilmRgbImage`](crate::algo::FilmRgbImage) cannot *enter* a named output branch
//! without first crossing this mapper. This is the working-space analogue of
//! `FilmRgbImage`'s own construction restriction.
//!
//! It does **not** follow that profile tagging is type-checked:
//! `io::encode(image: &LinearImage, params: &OutputParams, …)` will happily write any
//! buffer with any profile, so keeping the ACEScg tag matched to ACEScg pixels remains
//! the orchestrator's responsibility (`pipeline::stages` fetches the tag on the same
//! branch that maps the pixels).
//!
//! ## Precision, clamping, non-finite
//! The matrix multiply runs in **binary64** and stores `f32` — so the only
//! *significant* error versus an independent f64 reference is the final f32
//! rounding (the pinned const itself differs from the re-derived f64 matrix by
//! < 1×10⁻¹², negligible against the f32 store; pinned at ≤ 2×10⁻⁶ per channel by
//! the fixtures below). Values are passed through
//! **unclamped**: this stage owns neither tone nor gamut limiting (that is the
//! display renderer's job), and range-clamping happens only at the u16 encode
//! step. Non-finite inputs propagate as non-finite (the pipeline's explicit
//! policy: `io::encode` counts them, they are never silently swallowed here).

use crate::algo::FilmRgbImage;
use crate::types::LinearImage;

/// The pinned identifier this mapping records in the convert report
/// (`working_mapping`, design-spec §8). A future mapping is a *new* string, not a
/// silent edit to v1.
pub const WORKING_MAPPING_ID: &str = "nc-film-rgb-v1";

/// NC film RGB v1: linear Rec.709/D65 → linear ACEScg (AP1)/D60, Bradford CAT.
///
/// `out[i] = Σ_j M[i][j] · in[j]` with `in = [r, g, b]`. Pinned as `f64`
/// constants; derived once (and re-verified in-crate by
/// `matrix_matches_independent_bradford_derivation`) from:
/// - **source** linear Rec.709 primaries `R(0.640,0.330) G(0.300,0.600)
///   B(0.150,0.060)`, white **D65** `(0.3127, 0.3290)`;
/// - **destination** ACEScg **AP1** primaries `R(0.713,0.293) G(0.165,0.830)
///   B(0.128,0.044)`, white **ACES ~D60** `(0.32168, 0.33767)` — the same
///   primaries/white the `AcesCg` output profile carries (`pipeline::color`);
/// - **adaptation** the Lindbloom **Bradford** cone-response CAT, D65 → ACES white;
/// - **operation order** `M = NPM_AP1⁻¹ · CAT(D65→D60) · NPM_Rec709`.
///
/// Each row sums to ~1.0, so a neutral (equal-RGB) input maps to a neutral
/// ACEScg value — the white point is correctly adapted. These specific values
/// coincide with the published sRGB-linear → ACEScg Bradford matrix
/// (colour-science / OpenColorIO ACES), an external cross-check.
const NC_FILM_RGB_V1_TO_ACESCG: [[f64; 3]; 3] = [
    [
        0.613_097_395_458_146,
        0.339_523_075_654_473_1,
        0.047_379_528_684_925_34,
    ],
    [
        0.070_193_747_370_320_72,
        0.916_353_970_053_562,
        0.013_452_331_311_908_228,
    ],
    [
        0.020_615_576_583_812_915,
        0.109_569_734_502_356_13,
        0.869_814_637_185_856_3,
    ],
];

/// The intentional film rendering after NC film RGB v1 interpretation: unclamped
/// linear **ACEScg (AP1) at D60**, with the IR plane carried through untouched.
///
/// Fields are **private** and [`new`](AcesCgImage::new) is module-private, so
/// [`map_nc_film_rgb_v1`] is the only producer — no other stage can mint an
/// `AcesCgImage`, and a named color output that accepts one therefore cannot be
/// handed a value that skipped the mapping. Values may be non-finite or leave
/// `[0, 1]`: this boundary preserves the working range (clamping is the encoder's
/// job).
///
/// `Debug` prints only the dimensions (never the pixel buffers), matching
/// [`FilmRgbImage`](crate::algo::FilmRgbImage).
pub struct AcesCgImage {
    width: u32,
    height: u32,
    /// Interleaved `r,g,b` in linear ACEScg, `len == width * height * 3`.
    rgb: Vec<f32>,
    /// Carried-through IR plane (HDRi input), `len == width * height`.
    ir: Option<Vec<f32>>,
}

impl AcesCgImage {
    /// Sole constructor — **private to this module**, so only
    /// [`map_nc_film_rgb_v1`] (below) can build an `AcesCgImage`. Takes the
    /// already-validated buffers out of a [`LinearImage`], so the length
    /// invariants hold by construction.
    fn new(image: LinearImage) -> Self {
        Self {
            width: image.width,
            height: image.height,
            rgb: image.rgb,
            ir: image.ir,
        }
    }

    // Read accessors — the boundary's inspection API, consumed by the
    // named-output split (`pipeline::render_split`).
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn width(&self) -> u32 {
        self.width
    }

    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Read-only view of the interleaved linear-ACEScg pixels.
    pub fn rgb(&self) -> &[f32] {
        &self.rgb
    }

    /// Read-only view of the carried IR plane, when the input had one.
    #[allow(dead_code)] // read by `output/{sdr,hdr}-display-rendering`.
    pub fn ir(&self) -> Option<&[f32]> {
        self.ir.as_deref()
    }

    /// Unwrap into the plain working-space image — the **read** direction of the
    /// boundary, for the named-output split (`pipeline::render_split`): the
    /// `film-master` encode and the shared display stage both consume an
    /// `AcesCgImage` this way. Constructing one stays restricted to the mapper;
    /// reading one out is not the invariant the type protects.
    pub(crate) fn into_linear(self) -> LinearImage {
        // The fields came from a validated LinearImage and are never resized, so
        // the invariants hold; route through the validated constructor anyway
        // (O(1) checks) so a future regression fails loudly.
        LinearImage::new(self.width, self.height, self.rgb, self.ir)
            .expect("AcesCgImage preserves the validated buffer-length invariants")
    }
}

impl std::fmt::Debug for AcesCgImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcesCgImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("ir", &self.ir.is_some())
            .finish_non_exhaustive()
    }
}

/// Map a reconstructed [`FilmRgbImage`] through NC film RGB v1 into linear
/// ACEScg/D60 (design-spec §7, stage 4). The **same** mapper for `simple`,
/// density/exponential, and density/sigmoid — it consumes the typed film-RGB
/// boundary and returns the typed ACEScg boundary, so the reconstruction path
/// makes no difference to how the mapping is applied.
///
/// Pure and total: the matrix multiply runs in binary64 (stored `f32`), applies
/// no clamp or gamut limit, and carries the IR plane through untouched. Any
/// non-finite input channel propagates as non-finite (counted downstream at
/// encode, never swallowed here).
pub fn map_nc_film_rgb_v1(film: FilmRgbImage) -> AcesCgImage {
    // Consume the typed film boundary into its validated buffers.
    let mut image = film.into_linear();
    let m = &NC_FILM_RGB_V1_TO_ACESCG;

    // Interleaved r,g,b; `len % 3 == 0` is a LinearImage invariant, but guard the
    // chunking loudly rather than silently dropping a tail (matches `color.rs`).
    let (pixels, rest) = image.rgb.as_chunks_mut::<3>();
    debug_assert!(
        rest.is_empty(),
        "LinearImage rgb length must be a multiple of 3"
    );
    for px in pixels {
        // Compute in f64 for precision, then store f32. Read the source triple
        // first so the in-place write of channel 0 doesn't feed channels 1/2.
        let (r, g, b) = (px[0] as f64, px[1] as f64, px[2] as f64);
        px[0] = (m[0][0] * r + m[0][1] * g + m[0][2] * b) as f32;
        px[1] = (m[1][0] * r + m[1][1] * g + m[1][2] * b) as f32;
        px[2] = (m[2][0] * r + m[2][1] * g + m[2][2] * b) as f32;
    }

    AcesCgImage::new(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::reconstruct;
    use crate::types::{
        DensityCurve, DensityParams, ExponentialParams, FilmBase, Reconstruction, SigmoidParams,
    };

    // -- test-only f64 linear algebra, independent of the pinned const ---------
    //
    // These helpers re-derive the mapping matrix from first principles (primaries
    // + Bradford CAT) so the const can be checked against an *independent*
    // computation, and produce binary64 reference vectors for the golden fixtures.

    type M3 = [[f64; 3]; 3];
    type V3 = [f64; 3];

    fn matmul(a: M3, b: M3) -> M3 {
        let mut o = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                for k in 0..3 {
                    o[i][j] += a[i][k] * b[k][j];
                }
            }
        }
        o
    }

    fn matvec(a: &M3, v: V3) -> V3 {
        let mut o = [0.0; 3];
        for i in 0..3 {
            for k in 0..3 {
                o[i] += a[i][k] * v[k];
            }
        }
        o
    }

    fn inverse(m: M3) -> M3 {
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
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

    fn white_xyz(x: f64, y: f64) -> V3 {
        [x / y, 1.0, (1.0 - x - y) / y]
    }

    /// Normalized primary matrix (rgb → XYZ) for `primaries` + `white`.
    fn npm(primaries: [(f64, f64); 3], white: V3) -> M3 {
        let col = |(x, y): (f64, f64)| [x / y, 1.0, (1.0 - x - y) / y];
        let (r, g, b) = (col(primaries[0]), col(primaries[1]), col(primaries[2]));
        let mprim: M3 = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        let s = matvec(&inverse(mprim), white);
        let mut out = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] = mprim[i][j] * s[j];
            }
        }
        out
    }

    /// Bradford chromatic-adaptation matrix `src → dst` (Lindbloom cone matrix).
    fn bradford_cat(src: V3, dst: V3) -> M3 {
        const MA: M3 = [
            [0.8951000, 0.2664000, -0.1614000],
            [-0.7502000, 1.7135000, 0.0367000],
            [0.0389000, -0.0685000, 1.0296000],
        ];
        const MA_INV: M3 = [
            [0.9869929, -0.1470543, 0.1599627],
            [0.4323053, 0.5183603, 0.0492912],
            [-0.0085287, 0.0400428, 0.9684867],
        ];
        let rs = matvec(&MA, src);
        let rd = matvec(&MA, dst);
        let d: M3 = [
            [rd[0] / rs[0], 0.0, 0.0],
            [0.0, rd[1] / rs[1], 0.0],
            [0.0, 0.0, rd[2] / rs[2]],
        ];
        matmul(MA_INV, matmul(d, MA))
    }

    /// Re-derive NC film RGB v1 from the pinned colorimetry, independently of the
    /// shipping const.
    fn derived_matrix() -> M3 {
        let rec709 = [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)];
        let ap1 = [(0.713, 0.293), (0.165, 0.830), (0.128, 0.044)];
        let d65 = white_xyz(0.3127, 0.3290);
        let aces = white_xyz(0.32168, 0.33767);
        matmul(
            inverse(npm(ap1, aces)),
            matmul(bradford_cat(d65, aces), npm(rec709, d65)),
        )
    }

    // -- fixtures --------------------------------------------------------------

    /// Build a `FilmRgbImage` whose film-RGB values are *exactly* `rgb`.
    /// Reconstruction is the only public producer, so drive `simple`
    /// (`positive = 1 - scan/Dmin`) through a unit base with a pre-inverted scan
    /// (`scan = 1 - target`), giving `1 - (1-target)/1 == target` bit-for-bit.
    fn film_from(width: u32, height: u32, rgb: Vec<f32>, ir: Option<Vec<f32>>) -> FilmRgbImage {
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let scan: Vec<f32> = rgb.iter().map(|&t| 1.0 - t).collect();
        let img = LinearImage::new(width, height, scan, ir).unwrap();
        let (film, _) = reconstruct(&img, &base, &Reconstruction::Simple).unwrap();
        film
    }

    /// Every supported reconstruction config — all must use the same mapper.
    fn all_configs() -> [Reconstruction; 3] {
        [
            Reconstruction::Simple,
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Exponential(ExponentialParams::default()),
            },
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Sigmoid(SigmoidParams::default()),
            },
        ]
    }

    // -- matrix correctness ----------------------------------------------------

    #[test]
    fn matrix_matches_independent_bradford_derivation() {
        // The pinned const equals the from-primaries Bradford derivation (both
        // f64), to well under an f32 ulp — proof the literal was not mistranscribed
        // and encodes exactly Rec.709/D65 → ACEScg/D60 with Bradford adaptation.
        let d = derived_matrix();
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (NC_FILM_RGB_V1_TO_ACESCG[i][j] - d[i][j]).abs() < 1e-12,
                    "M[{i}][{j}] pinned {} != derived {}",
                    NC_FILM_RGB_V1_TO_ACESCG[i][j],
                    d[i][j]
                );
            }
        }
    }

    #[test]
    fn matrix_matches_published_srgb_to_acescg() {
        // Third, *independent* oracle: the widely-published sRGB-linear → ACEScg
        // Bradford matrix. Unlike `derived_matrix` (which re-runs the same
        // primaries/white the const author used, so a mistyped primary would slip
        // through both), these are externally-authored numbers — the actual
        // external cross-check the module doc advertises. Source: colour-science /
        // OpenColorIO ACES sRGB→ACEScg (Bradford CAT), 4-decimal precision, e.g.
        // `colour.matrix_RGB_to_RGB(sRGB, ACEScg, "Bradford")`. Tolerance ~1e-4 to
        // match the published 4-dp rounding.
        const PUBLISHED_SRGB_TO_ACESCG: M3 = [
            [0.6131, 0.3395, 0.0474],
            [0.0702, 0.9164, 0.0134],
            [0.0206, 0.1096, 0.8698],
        ];
        for i in 0..3 {
            for j in 0..3 {
                let err = (NC_FILM_RGB_V1_TO_ACESCG[i][j] - PUBLISHED_SRGB_TO_ACESCG[i][j]).abs();
                assert!(
                    err < 1e-4,
                    "M[{i}][{j}] pinned {} != published {} (err {err:.2e})",
                    NC_FILM_RGB_V1_TO_ACESCG[i][j],
                    PUBLISHED_SRGB_TO_ACESCG[i][j]
                );
            }
        }
    }

    #[test]
    fn neutral_maps_to_neutral_rows_sum_to_one() {
        // External ground truth independent of the derivation: a correctly
        // white-adapted RGB→RGB matrix maps equal-energy neutral to neutral, i.e.
        // each row sums to 1. This is what pins the *adaptation* (a missing or
        // wrong CAT would tint white).
        for (i, row) in NC_FILM_RGB_V1_TO_ACESCG.iter().enumerate() {
            let sum: f64 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-6, "row {i} sums to {sum}, not 1");
        }
    }

    #[test]
    fn pinned_vectors_match_binary64_reference_within_2e_minus_6() {
        // Primary / neutral / saturated / negative / above-one inputs. The
        // reference is computed in binary64 by the independent `derived_matrix` +
        // `matvec` (NOT the shipping loop), so the only permitted gap is the f32
        // store rounding. These do NOT invoke any named-output renderer/profile.
        let m = derived_matrix();
        let inputs: [[f32; 3]; 6] = [
            [1.0, 1.0, 1.0],  // neutral white
            [1.0, 0.0, 0.0],  // Rec.709 red primary
            [0.0, 1.0, 0.0],  // Rec.709 green primary
            [0.0, 0.0, 1.0],  // Rec.709 blue primary
            [0.8, 0.1, 0.4],  // saturated mix
            [1.5, -0.2, 0.0], // above-one + negative (out-of-range finite)
        ];
        for input in inputs {
            let film = film_from(1, 1, input.to_vec(), None);
            let aces = map_nc_film_rgb_v1(film);
            let got = aces.rgb();
            let want = matvec(&m, [input[0] as f64, input[1] as f64, input[2] as f64]);
            for ch in 0..3 {
                let err = (got[ch] as f64 - want[ch]).abs();
                assert!(
                    err <= 2e-6,
                    "input {input:?} ch {ch}: mapped {} vs f64 ref {} (err {err:.2e})",
                    got[ch],
                    want[ch]
                );
            }
        }
    }

    #[test]
    fn multi_pixel_values_match_binary64_reference_within_2e_minus_6() {
        // `pinned_vectors…` is 1×1, and the multi-pixel tests below check only
        // shape/IR/determinism — so nothing pins actual mapped *values* across a
        // chunk boundary. Map a 3-pixel buffer and check every channel of every
        // pixel against the independent binary64 `derived_matrix` + `matvec`,
        // guarding a per-pixel loop / `as_chunks_mut` regression.
        let m = derived_matrix();
        let px: [[f32; 3]; 3] = [
            [0.2, 0.5, 0.9], // distinct per-pixel triples so a cross-pixel
            [0.7, 0.1, 0.3], // bleed (wrong chunk boundary) would show up as a
            [1.2, 0.4, 0.6], // mismatch rather than cancelling out
        ];
        let flat: Vec<f32> = px.iter().flatten().copied().collect();
        let film = film_from(3, 1, flat, None);
        let aces = map_nc_film_rgb_v1(film);
        let got = aces.rgb();
        for (p, input) in px.iter().enumerate() {
            let want = matvec(&m, [input[0] as f64, input[1] as f64, input[2] as f64]);
            for ch in 0..3 {
                let err = (got[p * 3 + ch] as f64 - want[ch]).abs();
                assert!(
                    err <= 2e-6,
                    "pixel {p} ch {ch}: mapped {} vs f64 ref {} (err {err:.2e})",
                    got[p * 3 + ch],
                    want[ch]
                );
            }
        }
    }

    // -- typed boundary / mapper behavior --------------------------------------

    #[test]
    fn every_reconstruction_path_uses_the_same_mapper_and_preserves_shape_ir() {
        // simple, density/exponential, density/sigmoid all reach the mapper and
        // yield an `AcesCgImage` (compiler-enforced by the return type) with the
        // dimensions and IR plane intact.
        let scan = vec![0.5, 0.3, 0.2, 0.05, 0.03, 0.02];
        let ir = Some(vec![0.25, 0.75]);
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        for config in all_configs() {
            let img = LinearImage::new(2, 1, scan.clone(), ir.clone()).unwrap();
            let (film, _) = reconstruct(&img, &base, &config).unwrap();
            let aces = map_nc_film_rgb_v1(film);
            assert_eq!((aces.width(), aces.height()), (2, 1), "{config:?}");
            assert_eq!(aces.rgb().len(), 6, "{config:?}");
            assert_eq!(aces.ir(), Some(&[0.25_f32, 0.75][..]), "{config:?}");
            // Read direction round-trips dims + IR.
            let linear = aces.into_linear();
            assert_eq!((linear.width, linear.height), (2, 1));
            assert_eq!(linear.ir.as_deref(), Some(&[0.25_f32, 0.75][..]));
        }
    }

    #[test]
    fn mapping_is_deterministic_across_repeat_runs_and_configs() {
        // Same input + config ⇒ byte-identical float buffers (per-pixel bits), and
        // the identical mapper runs for every reconstruction/curve combination —
        // the determinism contract. Curated per-pixel bits (never a full-frame or
        // post-lcms2 checksum), per CLAUDE.md's cross-platform caveat.
        let scan = vec![0.85, 0.5, 0.38, 0.3, 0.18, 0.12, 0.02, 0.012, 0.009];
        let base = FilmBase::from([0.9, 0.55, 0.42]);
        for config in all_configs() {
            let run = || {
                let img = LinearImage::new(3, 1, scan.clone(), None).unwrap();
                let (film, _) = reconstruct(&img, &base, &config).unwrap();
                map_nc_film_rgb_v1(film)
                    .rgb()
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>()
            };
            assert_eq!(run(), run(), "non-deterministic mapping for {config:?}");
        }
    }

    #[test]
    fn nonfinite_and_out_of_range_pass_through_unclamped() {
        // The stage neither clamps nor gamut-limits: an above-one input stays
        // above one where the matrix keeps it there, and a non-finite channel
        // propagates as non-finite (encode counts it — it is never swallowed).
        let film = film_from(2, 1, vec![5.0, 5.0, 5.0, f32::NAN, 0.1, 0.2], None);
        let aces = map_nc_film_rgb_v1(film);
        let out = aces.rgb();
        // Neutral 5.0 maps to ~5.0 (rows sum to 1) — well above the [0,1] gamut,
        // not clamped.
        for &c in &out[0..3] {
            assert!(c > 4.9, "unclamped neutral 5.0 became {c}");
        }
        // A NaN in any source channel contaminates the whole output pixel (each
        // output channel sums all three inputs) — and is preserved, not zeroed.
        assert!(
            out[3..6].iter().all(|c| c.is_nan()),
            "NaN must propagate, got {:?}",
            &out[3..6]
        );
    }

    #[test]
    fn ir_absent_stays_absent() {
        let film = film_from(1, 1, vec![0.4, 0.5, 0.6], None);
        let aces = map_nc_film_rgb_v1(film);
        assert_eq!(aces.ir(), None);
    }

    // Construction privacy is compiler-enforced and cannot be exercised at
    // runtime: `AcesCgImage`'s fields are private and `AcesCgImage::new` is
    // module-private, so `map_nc_film_rgb_v1` is the only producer, and the only
    // input it accepts is a `FilmRgbImage` (itself only mintable by `algo`'s
    // reconstruction). Thus a named output that takes an `AcesCgImage` cannot be
    // handed a raw film/scan buffer, and no code outside this module can mint one.
    // A `trybuild` compile-fail case would need a dev-dependency the crate
    // deliberately avoids (see `algo/mod.rs`); the privacy annotations are the
    // guarantee. Every fixture above reaches a `FilmRgbImage` only through the
    // public `reconstruct`, confirming the mapper is the sole ACEScg entry point.
}
