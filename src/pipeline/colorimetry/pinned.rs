//! **Category 2 — derived artifacts, as shipped.** The reviewed, checked-in
//! coefficients the runtime actually multiplies by.
//!
//! Every literal here was moved **verbatim** from the stage that used to own it,
//! preserving both its precision and the arithmetic order it is applied in, so
//! the centralization is bit-identical. The migrated stages re-export these
//! names; none of them keeps a private copy.
//!
//! ## What each entry must document
//!
//! Direction (which space is source), encoding domain (these are all *linear*
//! transforms — no transfer function is folded in), the two white points, the
//! chromatic-adaptation convention, and the measured deviation from the canonical
//! `f64` re-derivation in [`super::derive`].
//!
//! ## Why the deviations are stated in `f32` ulps
//!
//! 33 of the 36 matrix entries below reproduce the canonical derivation
//! **bit-exactly**. Three are exactly **+1 `f32` ulp** away, and they are named
//! individually in the docs below. (The audit reports `ulps` as
//! `derived − shipped` on a monotonic ordering, so `+1` means the derivation
//! sits one ulp *above* the shipped literal. All three are negative values,
//! where "above" is the smaller magnitude.) Reaching those neighbouring values needs a
//! ~3e-9 relative shift in the derivation — far too large to be `f64`
//! accumulation noise (a sweep over inverse algorithms, association orders, and
//! summation orders moves the result by ~1e-17) and far too small to be a
//! different primaries or white-point choice. They are consistent with the
//! original values having been composed from intermediate matrices rounded to
//! ~9–10 significant digits; no derivation script was committed alongside them,
//! so the exact historical route is not recoverable.
//!
//! **This is not a coefficient error and re-pinning it would be the pixel change
//! this module's task forbids.** The chromaticities these matrices come from are
//! specified to three decimals, and perturbing one primary by its own ±5e-4
//! rounding moves entries by up to 4.2e-4 — about **3,500× one `f32` ulp**. A
//! 1-ulp disagreement is three orders of magnitude below the precision the
//! standards themselves define; neither value is "more correct". The check mode
//! therefore verifies agreement within ±1 `f32` ulp, and that tolerance is
//! justified by this measurement rather than by convenience.

/// NC film RGB v1: linear Rec.709/D65 → linear ACEScg (AP1)/ACES white.
///
/// - **Direction**: Rec.709 → ACEScg. **Domain**: linear, unclamped.
/// - **Whites**: D65 → ACES (~D60), adapted.
/// - **Adaptation**: Bradford with Lindbloom's **published 7-decimal inverse**
///   ([`BRADFORD_PUBLISHED_INVERSE`]) — *not* the canonical exact inverse. This is
///   the historical convention this frozen mapping was pinned with; re-deriving
///   with [`BRADFORD`] shifts it by 9.1e-8, which would change pixels.
/// - **Order**: `inverse(NPM_AP1) · ( CAT(D65→ACES) · NPM_Rec709 )`.
/// - **Deviation**: 1.1e-16 from the canonical derivation *using the published
///   inverse*; every entry is exact to `f64` round-off.
///
/// Kept in `f64` (not `f32` like the display matrices) because that is how it
/// shipped: `working_space::map_nc_film_rgb_v1` multiplies in binary64 and stores
/// `f32`. Each row sums to ~1.0, so neutral maps to neutral.
///
/// This mapping is a **versioned identifier** (`nc-film-rgb-v1`). A future
/// mapping is a new identifier and a `conversion-versioning` decision — never a
/// silent edit here.
///
/// [`BRADFORD`]: super::definitions::BRADFORD
/// [`BRADFORD_PUBLISHED_INVERSE`]: super::definitions::BRADFORD_PUBLISHED_INVERSE
pub const NC_FILM_RGB_V1_TO_ACESCG: [[f64; 3]; 3] = [
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

/// Linear ACEScg/ACES white → linear Rec.709(sRGB primaries)/D65.
///
/// - **Direction**: ACEScg → Rec.709. **Domain**: linear; the sRGB transfer
///   function is applied later, by the encoder, not folded in here.
/// - **Whites**: ACES (~D60) → D65, adapted. **Adaptation**: canonical
///   [`BRADFORD`](super::definitions::BRADFORD) (exact inverse).
/// - **Order**: `inverse(NPM_Rec709) · ( CAT(ACES→D65) · NPM_AP1 )`.
/// - **Deviation**: 8/9 entries exact; `[2][1]` is +1 `f32` ulp
///   (`-0.128_968_98` shipped vs `-0.128_968_97` canonical).
pub const ACESCG_TO_SRGB: [[f32; 3]; 3] = [
    [1.705_051, -0.621_792_14, -0.083_258_875],
    [-0.130_256_41, 1.140_804_8, -0.010_548_319],
    [-0.024_003_357, -0.128_968_98, 1.152_972_3],
];

/// Linear ACEScg/ACES white → linear Display P3/D65.
///
/// - **Direction**: ACEScg → Display P3. **Domain**: linear; sRGB transfer applied
///   downstream.
/// - **Whites**: ACES (~D60) → D65, adapted. **Adaptation**: canonical
///   [`BRADFORD`](super::definitions::BRADFORD).
/// - **Order**: `inverse(NPM_P3) · ( CAT(ACES→D65) · NPM_AP1 )`.
/// - **Deviation**: 8/9 entries exact; `[2][0]` is +1 `f32` ulp
///   (`-0.002_159_009_7` shipped vs `-0.002_159_009_5` canonical).
pub const ACESCG_TO_DISPLAY_P3: [[f32; 3]; 3] = [
    [1.379_214_2, -0.308_864_15, -0.070_349_984],
    [-0.069_334_86, 1.082_296_7, -0.012_961_888],
    [-0.002_159_009_7, -0.045_459_326, 1.047_618_4],
];

/// Linear ACEScg/ACES white → linear BT.2020/D65.
///
/// - **Direction**: ACEScg → BT.2020. **Domain**: linear; the Rec.2100 PQ/HLG
///   transfer is applied downstream in `pipeline::hdr`.
/// - **Whites**: ACES (~D60) → D65, adapted. **Adaptation**: canonical
///   [`BRADFORD`](super::definitions::BRADFORD).
/// - **Order**: `inverse(NPM_BT2020) · ( CAT(ACES→D65) · NPM_AP1 )`.
/// - **Deviation**: **9/9 entries exact.**
pub const ACESCG_TO_BT2020: [[f32; 3]; 3] = [
    [1.025_824_8, -0.020_053_191, -0.005_771_557],
    [-0.002_234_369_5, 1.004_586_5, -0.002_352_132_5],
    [-0.005_013_351_4, -0.025_290_072, 1.030_303_5],
];

/// Linear BT.2020/D65 → linear Display P3/D65.
///
/// - **Direction**: BT.2020 → Display P3. **Domain**: linear, and both sides are
///   display-linear and reference-white-relative — the gain-map common domain.
/// - **Whites**: D65 → D65. **Adaptation: none, deliberately.** The two spaces
///   share an adopted white, so no chromatic adaptation belongs at this boundary
///   and the derivation skips the term rather than multiplying by a
///   near-identity matrix.
/// - **Order**: `inverse(NPM_P3) · NPM_BT2020`.
/// - **Deviation**: 8/9 entries exact; `[0][2]` is +1 `f32` ulp
///   (`-0.061_398_584` shipped vs `-0.061_398_58` canonical).
pub const BT2020_TO_DISPLAY_P3: [[f32; 3]; 3] = [
    [1.343_578_2, -0.282_179_68, -0.061_398_584],
    [-0.065_297_455, 1.075_787_9, -0.010_490_463],
    [0.002_821_787_3, -0.019_598_495, 1.016_776_7],
];

/// BT.2020 non-constant-luminance luma weights, as used by `pipeline::hdr`.
///
/// **This one is a transcription of a normative table, not a derivation** — see
/// [`BT2020_LUMA_TABULATED`](super::definitions::BT2020_LUMA_TABULATED). Its
/// verification rule is exact equality with the tabulated value, narrowed to
/// `f32`. Deriving it from the BT.2020 primaries instead would give
/// `[0.262_700_21, 0.677_998_1, 0.059_301_716]`, differing by ~2e-6 — about 17
/// `f32` ulps, i.e. a *different number*, not a rounding of the same one. The
/// standard rounds and encoders are expected to use the rounded form.
pub const BT2020_LUMA: [f32; 3] = [0.2627, 0.6780, 0.0593];

/// BT.2020 non-constant-luminance R'G'B' → Y'CbCr matrix, used by `io::avif`.
///
/// **Applied to nonlinear PQ/HLG code values, not to linear light** — see
/// [`derive::ycbcr_from_luma`](super::derive::ycbcr_from_luma). It is the matrix
/// AVIF signals as `matrix_coefficients = 9`, so it is a *container* coefficient:
/// a decoder inverts exactly this to recover R'G'B', which is why it must be the
/// standard's matrix and not a convenient approximation.
///
/// Derived from the **tabulated** [`BT2020_LUMA`] rather than from the BT.2020
/// primaries, and that choice is load-bearing: encoders and decoders both use the
/// rounded tabulated weights, so deriving from primaries here would put nc's
/// forward transform ~2e-6 away from every decoder's inverse. Row 0 is
/// [`BT2020_LUMA`] verbatim; the `0.5` entries and the zero row sums are exact.
pub const BT2020_NCL_RGB_TO_YCBCR: [[f32; 3]; 3] = [
    [0.2627, 0.678, 0.0593],
    [-0.139_630_06, -0.360_369_95, 0.5],
    [0.5, -0.459_785_7, -0.040_214_296],
];

/// Display P3 luma weights, used by `pipeline::gain_map` and `pipeline::sdr`.
///
/// Unlike [`BT2020_LUMA`] this has no tabulated form: it **is** derived, as the
/// luminance (Y) row of the Display P3 normalized primary matrix at D65. All
/// three entries reproduce the canonical derivation exactly.
pub const DISPLAY_P3_LUMA: [f32; 3] = [0.228_974_57, 0.691_738_55, 0.079_286_91];

/// Rec.709 / sRGB luma weights, as used by `pipeline::sdr`'s sRGB branch.
///
/// **A third provenance kind — neither of the other two luma vectors' rule
/// applies.** [`BT2020_LUMA`] is a normative table; [`DISPLAY_P3_LUMA`] is an
/// exact derivation. This one is the derivation **rounded to six decimals**:
///
/// | | value |
/// |---|---|
/// | shipped here | `0.212_639, 0.715_169, 0.072_192` |
/// | exact derivation | `0.212_639_00, 0.715_168_65, 0.072_192_32` |
/// | BT.709's own table | `0.2126, 0.7152, 0.0722` |
///
/// So it is 0 / −6 / **43** `f32` ulps from the canonical derivation. That is far
/// looser than the ±1 ulp the matrices hold to, which is why the audit gives this
/// entry its own tolerance instead of relaxing the shared one.
///
/// The 43-ulp gap is still ~4.4e-6 relative, and BT.709 specifies these
/// coefficients to four decimals — a ±5e-5 rounding of its own, ~150× larger. So
/// the shipped value is well inside the standard's precision and re-pinning to the
/// exact derivation would be a **pixel change** (the sRGB SDR branch multiplies by
/// it), not a correction. No script recording the 6-decimal rounding was
/// committed, so its exact origin is unrecoverable — the same situation as the
/// three 1-ulp matrix entries, with a larger number.
pub const SRGB_LUMA: [f32; 3] = [0.212_639, 0.715_169, 0.072_192];
