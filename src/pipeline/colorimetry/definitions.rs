//! **Category 1 — standard definitions.** The normative source data every
//! derived artifact is computed from: chromaticities, white points, cone-response
//! matrices, and normatively-tabulated vectors.
//!
//! Nothing in this file is derived, and nothing in it may be edited to "fix" a
//! coefficient. A value changes here only when the standard it cites changes, and
//! that edit is the *start* of the workflow in `docs/colorimetry-maintenance.md`,
//! never the end of it.
//!
//! ## On citation precision
//!
//! Each entry records the standard and edition it comes from. Where the quantity
//! is unambiguous within that standard (there is exactly one primaries table) the
//! citation stops at the edition rather than inventing a clause number — a wrong
//! clause reference is worse than an absent one, because it looks checked. The
//! maintenance workflow's step 1 is to confirm the reference against the actual
//! standard text when a value is next touched.
//!
//! The chromaticities below are all specified by their standards to **three
//! decimal places**. That rounding, not our arithmetic, is the dominant error
//! term in every derived matrix: perturbing one primary by ±5e-4 (its own
//! rounding) moves composed matrix entries by up to 4.2e-4, roughly 3,500× a
//! single `f32` ulp. Tolerances downstream are calibrated against that fact.
//!
//! ## Why the dead-code allow
//!
//! Several definitions here have no *runtime* consumer by design: the runtime
//! multiplies by [`super::pinned`]'s reviewed literals and never derives, so both
//! cone-response matrices and the tabulated BT.2020 luma are reached only from the
//! `#[cfg(test)]` derivation and audit harness. A source-of-truth module is meant
//! to describe the colorimetry completely, not only the parts today's code paths
//! happen to touch.
//!
//! ## The lcms2-consumed spaces are a pixel-change hazard
//!
//! [`REC709`], [`DISPLAY_P3`], [`ACESCG`], [`PROPHOTO`] and — since
//! `hdr-linear-tiff` — [`BT2020`] are handed **directly to Little CMS** by
//! `pipeline::color` to synthesize profiles. Editing any of those five changes
//! embedded ICC bytes and every lcms2-transformed pixel on the affected path *even
//! with `pinned.rs` untouched and every audit `ulps` at 0*, and **nothing
//! automated catches it**: `version::PIPELINE_FINGERPRINTS` stops before lcms2 and
//! the audit only compares pinned artifacts. Treat an edit to one of the five as a
//! pixel change and verify by same-machine before/after comparison.
//!
//! The allow is scoped to `not(test)` so the lint stays **on** in a test build:
//! a definition that nothing at all references — not even the audit — is still
//! reported, and adding one fails `cargo clippy --all-targets`. That keeps the
//! allow from becoming a place for genuinely dead code to hide.
#![cfg_attr(not(test), allow(dead_code))]

/// A CIE 1931 `(x, y)` chromaticity.
///
/// Held in `f64` because every derivation runs in binary64; the values
/// themselves carry only the precision their standard specifies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chromaticity {
    pub x: f64,
    pub y: f64,
}

impl Chromaticity {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// CIE XYZ of this chromaticity normalized to `Y = 1`.
    ///
    /// Used both for white points (directly) and for primaries (as the unscaled
    /// columns of a normalized primary matrix).
    pub fn to_xyz(self) -> [f64; 3] {
        [self.x / self.y, 1.0, (1.0 - self.x - self.y) / self.y]
    }
}

/// The `(R, G, B)` chromaticities bounding an additive RGB gamut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Primaries {
    pub red: Chromaticity,
    pub green: Chromaticity,
    pub blue: Chromaticity,
}

impl Primaries {
    pub const fn new(red: Chromaticity, green: Chromaticity, blue: Chromaticity) -> Self {
        Self { red, green, blue }
    }

    pub fn as_array(self) -> [Chromaticity; 3] {
        [self.red, self.green, self.blue]
    }
}

/// A named additive RGB colour space: its primaries and its adopted white.
///
/// Deliberately *not* a description of a transfer function or an encoding — this
/// type carries only what RGB↔XYZ derivation needs. Transfer functions live in
/// [`transfer`], and encoding/tagging policy lives with the stage that owns it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorSpace {
    /// Stable identifier used in derived-artifact names and audit output.
    pub name: &'static str,
    pub primaries: Primaries,
    pub white: Chromaticity,
}

const fn xy(x: f64, y: f64) -> Chromaticity {
    Chromaticity::new(x, y)
}

// -- white points -------------------------------------------------------------

/// CIE D65, as adopted by BT.709, sRGB, BT.2020, and Display P3.
///
/// `(0.3127, 0.3290)`. Source: ITU-R BT.709-6 and ITU-R BT.2020-2, which both
/// adopt this four-decimal form. Note this is the *rounded* daylight D65, not the
/// chromaticity computed from the D65 spectral power distribution; the standards
/// specify the rounded pair, so that is what NC derives from.
pub const D65: Chromaticity = xy(0.3127, 0.3290);

/// The ACES adopted white, ~D60 but *not* CIE D60.
///
/// `(0.32168, 0.33767)`. Source: SMPTE ST 2065-1 (ACES). Five decimals here is
/// the specified precision, not extra confidence.
pub const ACES_WHITE: Chromaticity = xy(0.32168, 0.33767);

/// CIE D50, the ICC profile connection space white.
///
/// `(0.3457, 0.3585)`. Source: ICC.1:2010 / ISO 15076-1. Present for
/// completeness of the definitions surface; NC's runtime never adapts to D50
/// itself — Little CMS does that inside profile construction
/// (`pipeline::color`).
pub const D50: Chromaticity = xy(0.3457, 0.3585);

/// The ICC PCS adopted white, **as XYZ** — ICC.1:2022 §6.3.4.3 / Annex D:
/// `X = 0,9642`, `Y = 1,0000`, `Z = 0,8249`.
///
/// **Not the same number as `D50.to_xyz()`**, and that is the point. Deriving XYZ
/// from D50's rounded four-decimal chromaticities gives
/// `[0.96429568…, 1, 0.82510460…]`, which sits ≈2.4e-4 from the value ICC *declares*
/// a profile's `mediaWhitePointTag` to carry. A colorant matrix adapted to the
/// derived white therefore maps a neutral slightly off the white the same profile
/// announces — small, invisible, and still wrong in the direction that matters,
/// because the profile's own declaration is the contract a CMM reads.
///
/// So an ICC colorant matrix and the `chromaticAdaptationTag` beside it must both
/// adapt to **this** value, not to `D50`. Anything colorimetric that is not about
/// serializing an ICC profile keeps using [`D50`] — this constant is the ICC
/// encoding's rounded convention, not a better measurement of D50.
pub const ICC_PCS_WHITE_XYZ: [f64; 3] = [0.9642, 1.0, 0.8249];

// -- primaries / colour spaces ------------------------------------------------

/// ITU-R BT.709 / sRGB primaries with D65 white.
///
/// Primaries source: ITU-R BT.709-6. IEC 61966-2-1 (sRGB) adopts the identical
/// primaries and white point, so one definition serves both; the two differ only
/// in transfer function, which is not modelled by this type.
pub const REC709: ColorSpace = ColorSpace {
    name: "rec709",
    primaries: Primaries::new(xy(0.640, 0.330), xy(0.300, 0.600), xy(0.150, 0.060)),
    white: D65,
};

/// Display P3: the SMPTE RP 431-2 (DCI-P3) primaries with a **D65** white.
///
/// The primaries are DCI's; the white is *not* DCI's green-ish `(0.314, 0.351)`
/// projector white. Display P3 (Apple's, and the one every consumer display and
/// ICC "Display P3" profile means) pairs those primaries with D65. Conflating the
/// two whites is the classic P3 error, so the distinction is spelled out here
/// rather than left to the reader.
pub const DISPLAY_P3: ColorSpace = ColorSpace {
    name: "display-p3",
    primaries: Primaries::new(xy(0.680, 0.320), xy(0.265, 0.690), xy(0.150, 0.060)),
    white: D65,
};

/// Adobe RGB (1998), with D65 white.
///
/// Source: Adobe RGB (1998) Color Image Encoding (Adobe, version 2005-05); IEC
/// 61966-2-5 adopts the same primaries and white point. The transfer function is
/// a pure `563/256` power law with no linear segment, which this type does not
/// model.
///
/// Its red and blue primaries are **identical to [`REC709`]'s**; only green moves
/// (0.300, 0.600) -> (0.210, 0.710). That is the whole difference between the two
/// gamuts, and it is also the easiest pair to transcribe wrongly, so the tests
/// assert both halves of the relationship rather than just the values.
///
/// nc itself does not render to this space. It is defined here because
/// `scripts/analysis/nctool/metrics.py` measures Adobe RGB exports against nc's
/// output, and colorimetry has exactly one home in this repository — a set of
/// primaries transcribed into the Python instead would be a second source of
/// truth by definition. That analysis tool's tests re-read this file.
pub const ADOBE_RGB: ColorSpace = ColorSpace {
    name: "adobe-rgb",
    primaries: Primaries::new(xy(0.640, 0.330), xy(0.210, 0.710), xy(0.150, 0.060)),
    white: D65,
};

/// ITU-R BT.2020 (and BT.2100, which adopts the same primaries) with D65 white.
///
/// Source: ITU-R BT.2020-2. BT.2100-2 references these primaries unchanged, so
/// the Rec.2100 PQ/HLG renditions in `pipeline::hdr` share this definition.
///
/// ⚠ **Also fed straight to Little CMS**, by
/// `color::hdr_linear_bt2020_icc` for the `hdr-linear-tiff` output — so an edit
/// here changes embedded ICC bytes even when `pinned.rs` is untouched and every
/// audit `ulps` is 0. See the module note's warning about the lcms2-consumed
/// definitions; this is the fifth.
pub const BT2020: ColorSpace = ColorSpace {
    name: "bt2020",
    primaries: Primaries::new(xy(0.708, 0.292), xy(0.170, 0.797), xy(0.131, 0.046)),
    white: D65,
};

/// ProPhoto / ROMM RGB, with its **D50** adopted white.
///
/// Source: ISO 22028-2 (ROMM RGB). Only ever reached as a user-selected output
/// ICC profile (`--output-profile prophoto`), where Little CMS does the
/// colorimetry — NC derives no matrix for it, so it appears in the definitions
/// but not in the pinned artifacts.
///
/// The blue primary sits at `y = 0.0001`, essentially on the x axis. That is
/// ROMM's actual specification, not a typo, and it makes the space's blue
/// luminance weight ~0 — which is why the "every primary carries positive
/// luminance" invariant is asserted for the display spaces and not for this one.
pub const PROPHOTO: ColorSpace = ColorSpace {
    name: "prophoto",
    primaries: Primaries::new(xy(0.7347, 0.2653), xy(0.1596, 0.8404), xy(0.0366, 0.0001)),
    white: D50,
};

/// ACEScg working space: the AP1 primaries with the ACES adopted white.
///
/// Source: the AP1 primaries are specified for ACEScg (Academy TB-2014-004 /
/// SMPTE ST 2065-4 family); the white point is ST 2065-1's ACES white. This is
/// NC's working space after `working_space::map_nc_film_rgb_v1`.
pub const ACESCG: ColorSpace = ColorSpace {
    name: "acescg",
    primaries: Primaries::new(xy(0.713, 0.293), xy(0.165, 0.830), xy(0.128, 0.044)),
    white: ACES_WHITE,
};

// -- chromatic adaptation -----------------------------------------------------

/// A cone-response ("sharpened") matrix and the inverse used with it.
///
/// The inverse is carried explicitly rather than always computed because **which
/// inverse you use is itself source data**: a published 7-decimal inverse and the
/// exact numerical inverse give adaptation matrices that differ at ~1e-7, which is
/// visible at `f32`. See [`BRADFORD`] and [`BRADFORD_PUBLISHED_INVERSE`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConeResponse {
    pub name: &'static str,
    pub matrix: [[f64; 3]; 3],
    /// `None` means "invert [`matrix`](Self::matrix) numerically in `f64`".
    pub inverse: Option<[[f64; 3]; 3]>,
}

/// The Bradford cone-response matrix, inverted exactly in `f64`.
///
/// **This is the canonical convention for new artifacts.** Source: the Bradford
/// transform of Lam (1985), in the form universally reproduced for chromatic
/// adaptation (e.g. Lindbloom's published cone matrix). Taking the exact inverse
/// rather than a printed one removes an avoidable rounding from the chain.
pub const BRADFORD: ConeResponse = ConeResponse {
    name: "bradford",
    matrix: [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ],
    inverse: None,
};

/// Bradford paired with Lindbloom's **published 7-decimal** inverse.
///
/// Retained for exactly one reason: [`pinned::NC_FILM_RGB_V1_TO_ACESCG`] was
/// derived with it, that mapping is a frozen versioned identifier
/// (`nc-film-rgb-v1`), and re-deriving it with [`BRADFORD`] instead shifts it by
/// 9.1e-8 — a pixel change this refactor is forbidden to make. Using the exact
/// inverse reproduces v1 only to 9.1e-8; using this one reproduces it to 1.1e-16.
///
/// **Do not use this for new artifacts.** It exists to make history auditable,
/// not to be extended. A future working-space mapping gets a new identifier and
/// uses [`BRADFORD`].
///
/// [`pinned::NC_FILM_RGB_V1_TO_ACESCG`]: super::pinned::NC_FILM_RGB_V1_TO_ACESCG
pub const BRADFORD_PUBLISHED_INVERSE: ConeResponse = ConeResponse {
    name: "bradford-lindbloom-published-inverse",
    matrix: BRADFORD.matrix,
    inverse: Some([
        [0.9869929, -0.1470543, 0.1599627],
        [0.4323053, 0.5183603, 0.0492912],
        [-0.0085287, 0.0400428, 0.9684867],
    ]),
};

// -- normatively tabulated vectors --------------------------------------------

/// BT.2020 non-constant-luminance luma coefficients, **as tabulated**.
///
/// `[0.2627, 0.6780, 0.0593]`. Source: ITU-R BT.2020-2 (and BT.2100-2, which
/// reuses them).
///
/// This is a *standard definition*, not a derived artifact, and the distinction
/// is load-bearing: deriving the luma row from the BT.2020 primaries instead
/// gives `[0.262700212, 0.677998072, 0.059301716]`, which agrees only to ~2e-6.
/// The standard rounds to four decimals and encoders are expected to use the
/// rounded values, so NC uses them too — and the verification rule for this
/// vector is "matches the tabulated value exactly", not "matches a derivation".
/// Contrast [`super::pinned::DISPLAY_P3_LUMA`], which has no tabulated form and
/// *is* derived.
pub const BT2020_LUMA_TABULATED: [f64; 3] = [0.2627, 0.6780, 0.0593];

// -- transfer-function constants ----------------------------------------------

/// **Category 1 — transfer-function constants.**
///
/// Recorded here for provenance and single-definition reasons only. This task
/// does not change transfer functions; these constants are consumed by
/// `pipeline::hdr` and `pipeline::color` exactly as before.
///
/// Everything is held in `f64` — the width every derivation and Little CMS use.
/// Consumers that work in `f32` narrow explicitly at the use site. Two facts make
/// that safe rather than merely plausible:
///
/// - The PQ constants and the HLG system gamma are ratios of small integers, so
///   they are exactly representable in both widths.
/// - [`hlg::OETF_A`] is not, but `0.178_832_77` as `f32` and `0.178_832_77_f64 as
///   f32` have the same bit pattern (`3e371ff0`), so the single `f64` definition
///   narrows to the literal `pipeline::hdr` previously carried.
///
/// The [`srgb`] parameters are consumed as `f64` and are written as the
/// standard's own quotients rather than pre-evaluated decimals, so the values
/// handed to Little CMS are bit-for-bit what they were when the expressions sat
/// inline.
pub mod transfer {
    /// SMPTE ST 2084 (PQ) inverse-EOTF constants, in the standard's rational form.
    ///
    /// Source: SMPTE ST 2084:2014, reproduced in ITU-R BT.2100-2 Table 4. Kept as
    /// explicit rationals rather than decimals because that is how the standard
    /// states them and because it makes the exact representability obvious.
    pub mod pq {
        pub const M1: f64 = 2610.0 / 16_384.0;
        pub const M2: f64 = 2523.0 / 32.0;
        pub const C1: f64 = 3424.0 / 4096.0;
        pub const C2: f64 = 2413.0 / 128.0;
        pub const C3: f64 = 2392.0 / 128.0;
        /// PQ's absolute peak signal level, in cd/m².
        pub const PEAK_NITS: f64 = 10_000.0;
    }

    /// Hybrid log-gamma system gamma for the reference 1000 cd/m² display.
    ///
    /// Source: ITU-R BT.2100-2. NC renders to a 1000-nit reference display, for
    /// which the reference OOTF system gamma is 1.2.
    pub const HLG_SYSTEM_GAMMA: f64 = 1.2;

    /// Hybrid log-gamma OETF constants.
    ///
    /// Source: ITU-R BT.2100-2. The standard states the OETF as
    /// `sqrt(3·E)` for `E ≤ 1/12` and `a·ln(12·E − b) + c` above it, with `b` and
    /// `c` defined *in terms of* `a` (`b = 1 − 4a`, `c = 0.5 − a·ln(4a)`). Only
    /// `a` is an independent number, so only `a` is recorded here; the two derived
    /// terms stay next to the OETF in `pipeline::hdr`, which is where the
    /// standard's own formulation puts them.
    pub mod hlg {
        /// The OETF constant `a`.
        pub const OETF_A: f64 = 0.178_832_77;
    }

    /// sRGB transfer-function parameters, in Little CMS parametric **type 4**
    /// form: `Y = (a·X + b)^g` for `X ≥ d`, and `Y = c·X` below it.
    ///
    /// Source: IEC 61966-2-1 (sRGB). The standard writes the power segment as
    /// `1.055·X^(1/2.4) − 0.055` in the encode direction; type 4 stores the
    /// device→PCS (decode) direction, which is that expression rearranged — hence
    /// `a = 1/1.055`, `b = 0.055/1.055`, `c = 1/12.92`.
    ///
    /// Each is written as its defining quotient rather than a rounded decimal:
    /// these are handed to Little CMS as `f64`, and evaluating the quotient is
    /// what produces the exact value the profile has always carried.
    pub mod srgb {
        /// `g` — exponent of the power segment.
        pub const G: f64 = 2.4;
        /// `a` — scale inside the power segment.
        pub const A: f64 = 1.0 / 1.055;
        /// `b` — offset inside the power segment.
        pub const B: f64 = 0.055 / 1.055;
        /// `c` — slope of the near-black linear segment.
        pub const C: f64 = 1.0 / 12.92;
        /// `d` — encoded breakpoint between the linear and power segments.
        pub const D: f64 = 0.04045;
    }
}
