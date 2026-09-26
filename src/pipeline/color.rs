//! Working-space → output color transforms via lcms2, and the ICC blobs to embed.
//!
//! ## Working space
//! Step-1 decode produces "linear scanner RGB" with no input ICC, so the source
//! colorimetry must be pinned to build any transform. We treat the working space
//! as **Rec.709/sRGB primaries, D65 white, linear TRC**. sRGB output is then a
//! pure tone-curve application (identical primaries); wide-gamut output is a
//! clean primaries remap. The input semantic resolver
//! (`pipeline::input_semantics`) runs upstream of this stage and only admits
//! inputs whose measurement meaning is scanner-device with a supported linear
//! transfer — it never applies a source→working transform before density (an
//! embedded scanner ICC is reported, not applied), so this fixed working space
//! still holds. A characterized scanner/film → working transform is the separate,
//! deferred `post-reconstruction-color-characterization` task.
//!
//! ## Output spaces
//! The tone curve is a property of the space, so every embedded profile
//! self-describes its data:
//! - `SRgb`      — Rec.709 / D65, sRGB curve   (display-referred)
//! - `AcesCg`    — AP1     / ~D60, linear       (scene-referred; the `film-master` tag)
//! - `DisplayP3` — P3      / D65, sRGB curve    (display-referred SDR)
//!
//! The new chain's destinations are not `OutputSpace`s: `fit_gamut` has already moved
//! the pixels into the destination's primaries, so [`encode_display_linear`] applies
//! only the transfer, and the profile is chosen by `DestinationGamut` — Display P3 as
//! above, or Adobe RGB (1998): its primaries, D65, the pure `563/256` power law.
//!
//! ProPhoto and user-supplied ICC paths retired with the `legacy` preset, the only
//! path that selected an output space by name; an arbitrary destination returns,
//! if at all, as a gamut-mapped destination of its own.
//!
//! Values may leave `[0, 1]` after a gamut remap; range clamping and clipping
//! warnings are the encoder's job ("fail loudly" at encode), not this stage's.

use lcms2::{
    CIExyY, CIExyYTRIPLE, DisallowCache, Flags, GlobalContext, Intent, PixelFormat, Profile,
    ToneCurve, Transform,
};
use rayon::prelude::*;

use crate::pipeline::colorimetry::definitions::{self, ColorSpace, transfer};
use crate::pipeline::colorimetry::pinned;
use crate::pipeline::fit_gamut::DestinationGamut;
use crate::pipeline::hdr;
use crate::pipeline::pixels;
use crate::pipeline::sdr::{RenderedSdr, SdrGamut, SdrRenderMetadata};
use crate::types::{LinearImage, NcError, Result};

/// The output color space to transform into and tag the file with.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputSpace {
    SRgb,
    AcesCg,
    /// Display P3 SDR: P3 primaries, D65 encoding white, piecewise sRGB TRC.
    /// The standardized wide-gamut SDR destination (and the planned gain-map
    /// base).
    ///
    /// Every caller hands it pixels already in the P3 primaries, so its transform
    /// is a pure transfer-encode (see [`display_p3_transfer_profiles`]).
    DisplayP3,
}

/// The ICC bytes to embed for a given space, **without** building a transform.
///
/// The `film-master` branch has no transform to build — its pixels are already
/// linear ACEScg — so `pipeline::stages`' film-master render calls this to fetch the
/// tag that matches them.
pub fn icc_profile(space: &OutputSpace) -> Result<Vec<u8>> {
    profile_icc(&build_profile(space)?)
}

/// ICC header offset of the creation `dateTimeNumber` (ICC.1 §7.2, bytes 24–35).
const ICC_HEADER_DATETIME: std::ops::Range<usize> = 24..36;

/// Serialize an already-built profile to ICC bytes. Shared by every caller that
/// already holds the profile, so none rebuilds it.
///
/// Little CMS stamps profiles with the wall-clock creation time on synthesis, so
/// two otherwise-identical runs seconds apart would embed different ICC bytes and
/// break the byte-identical determinism contract (§8) — the failure is a single
/// seconds byte deep inside the TIFF. Zero the header `dateTimeNumber` (an
/// ICC-legal "unknown" value) so the embedded blob is reproducible.
fn profile_icc(profile: &Profile) -> Result<Vec<u8>> {
    let mut bytes = profile
        .icc()
        .map_err(|e| NcError::Other(format!("failed to serialize ICC profile: {e}")))?;
    if let Some(dt) = bytes.get_mut(ICC_HEADER_DATETIME) {
        dt.fill(0);
    }
    Ok(bytes)
}

/// Apply only the destination's sRGB transfer curve to an SDR-rendered linear
/// image and return the matching ICC profile. The renderer already performed
/// ACEScg → destination gamut conversion, so the transform must not remap the
/// primaries a second time.
pub fn encode_rendered_sdr(
    rendered: RenderedSdr,
) -> Result<(LinearImage, Vec<u8>, SdrRenderMetadata)> {
    let (mut image, metadata) = rendered.into_parts();
    let (linear, output) = match metadata.gamut {
        SdrGamut::DisplayP3 => display_p3_transfer_profiles()?,
        SdrGamut::SRgb => (working_profile()?, build_profile(&OutputSpace::SRgb)?),
    };
    transform_in_place(&mut image, &linear, &output)?;
    Ok((image, profile_icc(&output)?, metadata))
}

/// Apply only the destination's transfer curve to the new chain's output, which
/// `fit_gamut` has already moved into the destination's primaries, and return the
/// matching ICC profile — the new flow's counterpart of [`encode_rendered_sdr`], for
/// the same reason: a second gamut transform would remap values that are already in
/// the destination's primaries.
///
/// The gamut is read off the chain's exit rather than chosen here, so the embedded
/// profile names the primaries the pixels are actually in. Consumes and returns the
/// image, so no second full-frame buffer exists.
pub fn encode_display_linear(
    mut image: LinearImage,
    gamut: DestinationGamut,
) -> Result<(LinearImage, Vec<u8>)> {
    let (linear, output) = match gamut {
        DestinationGamut::DisplayP3 => display_p3_transfer_profiles()?,
        DestinationGamut::AdobeRgb => adobe_rgb_transfer_profiles()?,
        // BT.2020 is rendered only for HDR, whose transfers (PQ, HLG, or none) and
        // profiles are `pipeline::hdr`'s and the HDR encoders'. The destination table
        // (`crate::destination::ROWS`) has no SDR row in it.
        DestinationGamut::Bt2020 => {
            return Err(NcError::Other(
                "BT.2020 has no SDR display encoding; its destinations are HDR".into(),
            ));
        }
    };
    transform_in_place(&mut image, &linear, &output)?;
    Ok((image, profile_icc(&output)?))
}

/// Linear Display P3 → the Display P3 output profile: the same primaries on both
/// sides, so the transform applies the destination's sRGB curve and nothing else.
fn display_p3_transfer_profiles() -> Result<(Profile, Profile)> {
    let (white, primaries) = lcms_inputs(definitions::DISPLAY_P3);
    Ok((
        synth(white, primaries, 1.0)?,
        build_profile(&OutputSpace::DisplayP3)?,
    ))
}

/// The Adobe RGB profile's `profileDescriptionTag`.
///
/// **Not** plain `"Adobe RGB (1998)"`: that names Adobe's own profile, and this one is
/// synthesized from the published encoding, so it says it is compatible — the
/// convention other tools' equivalents follow. `(Hanten)`, not the coded-HDR
/// profiles' `(nc)`: theirs stays only because it already shipped in file bytes, and
/// a user-visible name is Hanten (CLAUDE.md). Once a destination writes it, this
/// string is in every file's bytes too, so it is an identifier from then on.
const ADOBE_RGB_DESCRIPTION: &str = "Adobe RGB (1998) compatible (Hanten)";

/// Linear Adobe RGB → the Adobe RGB output profile: the same primaries on both sides,
/// so the transform applies the `563/256` power law and nothing else.
///
/// Named, unlike the Display P3 profile's `"RGB built-in"` (whose bytes already
/// shipped, see [`hdr_linear_bt2020_icc`]): this destination exists for a workflow
/// that continues in an editor, where the profile is what the user sees.
fn adobe_rgb_transfer_profiles() -> Result<(Profile, Profile)> {
    let (white, primaries) = lcms_inputs(definitions::ADOBE_RGB);
    let mut output = synth(white, primaries, transfer::adobe_rgb::GAMMA)?;
    describe(&mut output, ADOBE_RGB_DESCRIPTION)?;
    Ok((synth(white, primaries, 1.0)?, output))
}

/// The ICC blob for the `hdr-linear-tiff` output: linear BT.2020 / D65.
///
/// **No transform runs here, and that is the whole point.** `pipeline::hdr` already
/// rendered into BT.2020 primaries, so this only *describes* the samples the
/// encoder writes verbatim. A transform from the Rec.709 working profile would treat
/// display-linear BT.2020 as Rec.709 working RGB and remap it a second time — the
/// same trap [`encode_rendered_sdr`] documents for the SDR rendition.
///
/// Two deliberate omissions, both recorded so they are not "fixed" later:
///
/// * **No `cicpTag`.** ICC.1:2022 §9.2.17 would permit one here (RGB data space,
///   Display class), and the PQ/HLG TIFFs in `output/lossless-hdr-tiff`'s second
///   half do carry one. But H.273's `VideoFullRangeFlag` describes a *bounded*
///   code range, and these samples deliberately run past 1.0 to the ≈4.926108
///   peak — so a CICP claim would add nothing the colorants and linear TRC do not
///   already state, while over-stating the range. A linear transfer code point
///   (H.273 value 8) is not a substitute for that.
/// * **No attempt to encode luminance semantics in the profile.** The ICC PCS
///   stops at the media white, so no v4 profile can say "1.0 means 203 cd/m² and
///   4.926108 means 1000 cd/m²". The report and sidecar own those facts; the task
///   requires that this profile never be claimed to carry them.
///
/// ⚠ This is a runtime consumer of a `colorimetry::definitions` colour space
/// (beside `REC709`, `DISPLAY_P3`, `ACESCG` and `ADOBE_RGB`): editing
/// `definitions::BT2020` now changes ICC bytes and every lcms2-transformed pixel on
/// this path *even with `pinned.rs` untouched and every audit ulp at 0*. Nothing
/// automated catches it — `PIPELINE_FINGERPRINTS` stops before lcms2 and the audit
/// only compares pinned artifacts. Treat a `BT2020` edit as a pixel change.
pub fn hdr_linear_bt2020_icc() -> Result<Vec<u8>> {
    let (white, primaries) = lcms_inputs(definitions::BT2020);
    let mut profile = synth(white, primaries, 1.0)?;
    // A real name, like the coded-HDR profiles carry. `Profile::new_rgb` leaves
    // Little CMS's default `"RGB built-in"`, which is useless in an application's
    // profile list — and this profile is one a user picks out of such a list.
    //
    // Deliberately **not** applied to the older sRGB/P3/ACEScg builders:
    // doing that in the shared `synth` helper would change the embedded ICC bytes of
    // already-shipped outputs, which is a separate reviewed decision.
    describe(&mut profile, "NC Display-Linear BT.2020 (D65)")?;
    profile_icc(&profile)
}

/// Set a profile's `profileDescriptionTag`.
///
/// Tagged **`en`/`US`**, not the null locale, for two reasons. ICC.1:2022 §10.15
/// specifies each `multiLocalizedUnicodeType` record's language as an ISO 639-1 code
/// and its country as an ISO 3166-1 one, and an all-zero pair is neither; and Little
/// CMS's own default path writes `enUS` for the `cprt` tag it fills in, so a null
/// `desc` record left the *same profile* internally inconsistent. A reader that asks
/// for a specific locale without falling back to record 0 then found no description
/// at all — the defect naming the profile was meant to remove.
///
/// `synth_coded_hdr` writes its text through `cmsMLUsetASCII` with the same pair, so
/// every profile nc names agrees.
fn describe(profile: &mut Profile, description: &str) -> Result<()> {
    let mut text = lcms2::MLU::new(1);
    // The `bool` is checked, not discarded: dropping it would let a failed
    // `set_text` produce a description-*less* profile while this function still
    // returned `Ok`, which is the silent-wrong-output shape the project forbids.
    // `synth_coded_hdr` checks every `cmsMLUsetASCII` next door for the same reason.
    if !text.set_text(description, lcms2::Locale::new("en_US")) {
        return Err(NcError::Other(format!(
            "failed to set the ICC profile description text {description:?}"
        )));
    }
    if profile.write_tag(
        lcms2::TagSignature::ProfileDescriptionTag,
        lcms2::Tag::MLU(&text),
    ) {
        Ok(())
    } else {
        Err(NcError::Other(format!(
            "failed to write the ICC profile description {description:?}"
        )))
    }
}

// ---------------------------------------------------------------------------
// Coded-HDR (PQ / HLG) A2B profiles
// ---------------------------------------------------------------------------

/// Entries in the tabulated tone curve of a coded-HDR profile.
///
/// 1024 was chosen by measurement, not habit: against the exact transfer function,
/// 1024 and 4096 entries give the *same* accuracy (≤0.1% above 20 nits) because the
/// limit is the 16-bit quantization of each stored value, not the table length —
/// so the larger table would cost 18 KB of profile for nothing. Adobe's reference
/// BT.2100 profiles use 512.
const CODED_CURVE_ENTRIES: usize = 1024;

/// ICC PCSXYZ in a 16-bit LUT tag is a **`u1Fixed15Number`** (ICC.1:2022 §4.8):
/// *unsigned*, 15 fractional bits, so `1.0` encodes as `0x8000` and the
/// representable maximum is `65535/32768 ≈ 1.99997`. A CMM scales by that when it
/// reads the tag, so the matrix stage is pre-divided by it and the pipeline's PCS
/// output is true XYZ.
///
/// The *unsigned* part matters: read as a signed 1.15 type the maximum would be
/// ≈0.99997 and this scale would be mis-derived by 2x.
///
/// Not a fudge factor and not guessed: Adobe's reference BT.2100 profiles carry
/// exactly this halving in their own matrix (their entries are the BT.2020
/// colorants × 0.5), and omitting it makes every luminance come out 2× — which is
/// how it was found.
const PCS_XYZ_U1FIXED15: f64 = 32768.0 / 65535.0;

/// What a coded-HDR ICC profile must state, gathered in one place so the PQ and HLG
/// builders differ only in data.
struct CodedHdrProfile {
    /// `profileDescriptionTag` — a real name, unlike the `"RGB built-in"` Little
    /// CMS puts on nc's matrix-shaper profiles.
    description: &'static str,
    /// CICP `(ColourPrimaries, TransferCharacteristics, MatrixCoefficients,
    /// VideoFullRangeFlag)` per ITU-T H.273.
    cicp: (u8, u8, u8, u8),
    /// PCS-relative value the largest code (`1.0`) maps to — `10000/203` for PQ.
    /// The matrix carries this scale, which is what puts the HDR range above
    /// PCS `1.0`; the curve itself stays inside `[0, 1]` as ICC requires.
    peak_relative: f64,
    /// `code -> normalized value in [0, 1]`, i.e. the transfer function's own
    /// inverse divided by its peak. Multiplying by
    /// [`peak_relative`](Self::peak_relative) recovers the PCS value.
    curve: fn(f64) -> f64,
}

/// The ICC blob for `hdr-pq-tiff`: BT.2020 primaries, the ST 2084 transfer, full
/// range, PCS `Y = L / 203`. See [`synth_coded_hdr`] for the ICC.1:2022 Display-class
/// requirements it satisfies (`BToA0Tag`, `chromaticAdaptationTag`) and why Input
/// class was evaluated and rejected.
///
/// **Display-referred and unclipped.** A 203 cd/m² diffuse white lands on PCS
/// `1.0`, and highlights carry on past it to `10000/203 ≈ 49.26` — an *extended
/// range* PCS. That is what makes the profile simultaneously honest (it is a pure
/// scaling of absolute luminance: nothing is clipped, nothing invented, so it
/// satisfies ICC.1:2022 §9.2.17's requirement that the profile encoding be
/// "equivalent to the data colour space encoding" the [`cicp`](CodedHdrProfile::cicp)
/// tag declares) and useful (a CMM that ignores `cicp` still renders diffuse white
/// correctly instead of at 2% of it). "Equivalent" here is about the *encoding*
/// the `cicp` tag declares; it is a separate question from the two Display-class
/// tag requirements [`synth_coded_hdr`] documents as still open.
///
/// It is an **A2B pipeline, not a matrix-shaper**, and it has to be: a
/// `redTRCTag` curve output is confined to `[0, 1]`, so a shaper profile could only
/// reach an HDR range by clipping at reference white or by normalizing to
/// 10,000 nits and rendering everything near-black. Adobe's reference BT.2100
/// profiles take the same A2B route for the same reason.
///
/// **The extended range survives only in a floating-point CMM pipeline**, and that
/// limit belongs with the claim rather than buried. The `AToB0`'s own output
/// encoding is the same `u1Fixed15` PCS, capped at ≈1.99997 — about 406 cd/m² here —
/// so any *integer* ICC pipeline (including Little CMS's own `cmsDoTransform` with
/// 16-bit pixel formats) clamps there and flattens every highlight. The ≈49.26 the
/// tests measure survives because lcms evaluates in float **and** the identity B
/// curves are parametric, hence unclamped. A consumer that colour-manages in 16-bit
/// therefore sees a 406-nit ceiling, not the full range; the `cicp` tag, which such a
/// consumer can read directly, remains the reliable signal.
///
/// Measured accuracy of the round trip through Little CMS: ≤0.1% above 20 nits,
/// ≤0.8% above 5 nits, degrading below ~1 nit where the 16-bit curve step
/// (0.153 nits) dominates — an absolute error under 0.08 nits, i.e. below any
/// display's black level. **This affects only how a colour-managed viewer
/// interprets the file; the stored code values are untouched.**
pub fn hdr_pq_tiff_icc() -> Result<Vec<u8>> {
    synth_coded_hdr(&CodedHdrProfile {
        description: "Rec.ITU-R BT.2100 PQ Full Range (nc)",
        // 9-16-0-1 — ICC.1:2022 §10.3 lists this quadruple as "PQ R'G'B' full range
        // representation specified in Recommendation ITU-R BT.2100-2, Table 9".
        // MatrixCoefficients is **0 because the data colour space is RGB**, which
        // §10.3 requires; `io::avif` writes 9 for the same rendition because AVIF
        // stores Y'CbCr. Copying that 9 here would be non-conformant.
        cicp: (9, 16, 0, 1),
        peak_relative: transfer::pq::PEAK_NITS / f64::from(hdr::REFERENCE_WHITE_NITS),
        curve: |code| hdr::pq_decode_nits(code) / transfer::pq::PEAK_NITS,
    })
}

/// The ICC blob for `hdr-hlg-tiff`: BT.2020 primaries, the HLG transfer, full
/// range, **scene-referred** PCS with diffuse white at `1.0`. **See
/// [`synth_coded_hdr`] for the ICC conformance requirements it satisfies.**
///
/// **Why scene-referred, when the PQ profile is display-referred.** HLG's OOTF is
/// `R_D = α · Y_S^(γ-1) · R_S` — it scales each channel by a function of the
/// *pixel's* scene luminance, so it is not separable per channel and no set of 1D
/// curves can represent it. Applying it as a per-channel power anyway is a common
/// shortcut and a wrong one, so this profile stops at the inverse OETF and states
/// scene-referred values; the OOTF is the display's job, which is what HLG's design
/// intends. Adobe ships exactly this split — their BT.2100 HLG *Scene* profiles are
/// 1D-plus-matrix like this one, while their *Display* profiles are ~66 KB because
/// they need a 3D CLUT for the same reason.
///
/// The PCS is anchored so [`hdr::hlg_reference_white_signal`] — the signal level
/// the renderer actually produces for 203 cd/m², ≈0.75, i.e. BT.2100's nominal
/// diffuse white — maps to PCS `1.0`, matching the PQ profile's placement of
/// diffuse white. The `cicp` tag remains the authoritative signal, and the
/// report's `hdr_coded_tiff` block carries the display-referred contract
/// (1000-nit peak, zero black, system gamma 1.2) that this profile deliberately
/// does not encode.
pub fn hdr_hlg_tiff_icc() -> Result<Vec<u8>> {
    let white_signal = f64::from(hdr::hlg_reference_white_signal());
    let white_scene = hdr::hlg_decode_scene(white_signal);
    if !(white_scene.is_finite() && white_scene > 0.0) {
        return Err(NcError::Other(format!(
            "HLG reference-white scene value is not usable as a PCS anchor ({white_scene})"
        )));
    }
    synth_coded_hdr(&CodedHdrProfile {
        description: "Rec.ITU-R BT.2100 HLG Scene-Referred Full Range (nc)",
        // 9-18-0-1, the HLG row of the same §10.3 table; MatrixCoefficients 0 for
        // the same normative reason as PQ.
        cicp: (9, 18, 0, 1),
        // Scene 1.0 is the largest code's value, so the peak relative to diffuse
        // white is `1 / scene(diffuse white)` and the curve needs no normalization
        // of its own — the inverse OETF already lands in `[0, 1]`.
        peak_relative: 1.0 / white_scene,
        curve: hdr::hlg_decode_scene,
    })
}

/// Build a coded-HDR profile: `mAB` pipeline (tone curves → scaled colorant matrix
/// → identity) plus the `cicp` tag, description, and D50 media white.
///
/// # The two ICC.1:2022 conformance requirements, and how they are met
///
/// Both were **open gaps** when `output/lossless-hdr-tiff` shipped and were closed by
/// `output/presets` on 2026-08-09. Recorded here because the reasoning is what a
/// future edit needs, not the fact:
///
/// * **§8.4.2 requires `BToA0Tag` as well as `AToB0Tag`** for an N-component
///   LUT-based Display profile. Written as a `mBA ` mirror of the A2B — B curves
///   (identity) → inverse matrix → M curves (the forward transfer). nc only ever
///   uses these profiles as *sources*, so nothing here depends on it; it is written
///   because the profile claims Display class and must therefore satisfy the class.
/// * **§8.2 requires `chromaticAdaptationTag`** "when the measurement data used to
///   calculate the profile was specified for an adopted white with a chromaticity
///   different from that of the PCS adopted white". The colorants are Bradford
///   D65→D50 adapted and `mediaWhitePointTag` declares D50, so `chad` is required;
///   it carries `pinned::BRADFORD_D65_TO_ICC_PCS`, the same adaptation the colorants
///   were built with. Every profile Little CMS builds for nc gets one automatically
///   — including `hdr_linear_bt2020_icc` — because `Profile::new_rgb` writes it;
///   an authored profile does not.
///
/// **Input class was evaluated and rejected.** ICC §8.3.2 requires no `BToA0Tag` for
/// an Input profile and §9.2.17 still permits `cicpTag`, so declaring Input class
/// would have closed the first gap for free. Measured: the two profiles differ in
/// exactly **3 bytes** (the class signature), macOS ColorSync parses and names both
/// identically, and transforming through each as a source produces byte-identical
/// output. Since the class is not observably load-bearing, the choice falls to
/// truthfulness — an Input profile describes a *capture device*, and these describe a
/// display encoding — so Display class stays and the tag is written. That also keeps
/// nc's profiles consistent (every other one is Display class) and matches Adobe's
/// reference BT.2100 profiles.
///
/// **Neither tag is enforced by any decoder we can test**, which is worth stating
/// plainly: macOS ColorSync used the profile as a transform *destination* even
/// without `BToA0`, so the acceptance evidence does not discriminate. These are
/// conformance requirements met on the strength of the normative text.
///
/// Note that a conformant `BToA0` is *inherently* range-limited here: its PCS input
/// is `u1Fixed15Number`, capping it at ≈1.99997 — about 406 cd/m² — so it cannot
/// round-trip the extended range the `AToB0` carries (to ≈49.26). That is a property
/// to document, not to engineer around; Adobe's reference profiles ship one anyway.
///
/// **One of two places nc reaches past the safe `lcms2` wrapper**, and they are
/// unsafe for unrelated reasons. *Here* it is to **build a profile**: the safe crate
/// cannot insert stages into a `Pipeline` (it exposes only `cat`) and does not expose
/// a profile's raw handle, so an A2B profile is unreachable through it. The unsafe
/// region is confined to profile *construction* — no pixel ever passes through this
/// code, and the result is plain ICC bytes that the safe API reads back (the tests do
/// exactly that). The other is `cli`'s startup call to
/// `lcms2_sys::cmsSetLogErrorHandler`, which **installs a process-global handler**
/// (the safe wrapper exposes only per-`ThreadContext` ones, and `color.rs` transforms
/// on the global context); that one is the wider-reaching of the two — it changes
/// process-wide state for the whole run rather than producing a byte buffer.
fn synth_coded_hdr(spec: &CodedHdrProfile) -> Result<Vec<u8>> {
    use lcms2_sys as sys;

    // The curve stays in `[0, 1]` (an ICC `curveType` cannot express more) and the
    // matrix below carries `peak_relative`. That split is the only way a 16-bit
    // curve can describe an HDR range at all.
    let table: Vec<u16> = (0..CODED_CURVE_ENTRIES)
        .map(|i| {
            let code = i as f64 / (CODED_CURVE_ENTRIES as f64 - 1.0);
            ((spec.curve)(code).clamp(0.0, 1.0) * 65535.0).round() as u16
        })
        .collect();

    let matrix: Vec<f64> = pinned::BT2020_TO_XYZ_D50
        .iter()
        .flatten()
        .map(|value| value * spec.peak_relative * PCS_XYZ_U1FIXED15)
        .collect();

    // The `BToA0Tag` matrix is the exact inverse of the one above, scaling undone.
    // ICC.1:2022 §8.4.2 requires the tag for an N-component LUT-based Display
    // profile, and without it a strict CMM cannot use the profile as a transform
    // *destination*.
    //
    // **It is inherently range-limited and that is stated, not engineered around.**
    // Its PCS input is `u1Fixed15Number`, capped at ≈1.99997 — about 406 cd/m² — so
    // it cannot round-trip the extended range the `AToB0` carries (to ≈49.26, i.e.
    // 10,000 nits). Adobe's reference BT.2100 profiles ship a `BToA0` anyway, and
    // matching that precedent is the point; over-claiming its reach is not. nc only
    // ever uses these profiles as *sources*, so nothing in nc depends on it.
    let inverse_matrix: Vec<f64> = pinned::XYZ_D50_TO_BT2020
        .iter()
        .flatten()
        .map(|value| value / (spec.peak_relative * PCS_XYZ_U1FIXED15))
        .collect();

    // `chromaticAdaptationTag`, ICC.1:2022 §8.2: required because the colorants are
    // Bradford D65→D50 adapted while `mediaWhitePointTag` declares D50. Without it a
    // consumer cannot recover that the *encoding* white is D65. Row-major, and the
    // same adaptation the colorants carry — a test pins that relationship.
    let chad: Vec<f64> = pinned::BRADFORD_D65_TO_ICC_PCS
        .iter()
        .flatten()
        .copied()
        .collect();

    let description = cstring(spec.description)?;
    let copyright = cstring("No copyright, use freely")?;
    let language = cstring("en")?;
    let country = cstring("US")?;
    // ICC.1:2022's PCS white point: X = 0,9642, Y = 1,0000, Z = 0,8249.
    //
    // The colorant matrix now adapts to **this** value
    // (`definitions::ICC_PCS_WHITE_XYZ`), so a neutral lands on the white the profile
    // declares. It previously adapted to `D50.to_xyz()` — D50 derived from its
    // *rounded chromaticities*, `[0.96429568, 1, 0.82510460]` — leaving a neutral
    // ≈2.4e-4 off. If that ever needs revisiting: the spec value here is the correct
    // target and the *matrix* is what adapts to it, never the reverse.
    let media_white = sys::CIEXYZ {
        X: 0.9642,
        Y: 1.0,
        Z: 0.8249,
    };
    let (primaries, transfer_characteristics, matrix_coefficients, full_range) = spec.cicp;
    let cicp = sys::VideoSignalType {
        ColourPrimaries: primaries,
        TransferCharacteristics: transfer_characteristics,
        MatrixCoefficients: matrix_coefficients,
        VideoFullRangeFlag: full_range,
    };

    // SAFETY (whole block): every raw pointer below is either freshly allocated by
    // Little CMS and checked non-null before use, or a pointer to a local that
    // outlives the call it is passed to. `cmsWriteTag` copies the data it is given,
    // so the locals may be dropped afterwards. Each owned handle is released on
    // every path via the guards, including the early returns `fail!` performs.
    unsafe {
        let profile = sys::cmsCreateProfilePlaceholder(std::ptr::null_mut());
        if profile.is_null() {
            return Err(NcError::Other("failed to allocate an ICC profile".into()));
        }
        let profile = ProfileHandle(profile);

        sys::cmsSetDeviceClass(profile.0, sys::ProfileClassSignature::DisplayClass);
        sys::cmsSetColorSpace(profile.0, sys::ColorSpaceSignature::RgbData);
        sys::cmsSetPCS(profile.0, sys::ColorSpaceSignature::XYZData);
        sys::cmsSetProfileVersion(profile.0, 4.4);
        sys::cmsSetHeaderRenderingIntent(profile.0, sys::Intent::RelativeColorimetric);

        macro_rules! fail {
            ($what:expr) => {
                return Err(NcError::Other(format!(
                    "failed to write the {} of the {} ICC profile",
                    $what, spec.description
                )))
            };
        }

        if sys::cmsWriteTag(
            profile.0,
            sys::TagSignature::MediaWhitePointTag,
            (&raw const media_white).cast(),
        ) == 0
        {
            fail!("media white point");
        }
        if sys::cmsWriteTag(
            profile.0,
            sys::TagSignature::CicpTag,
            (&raw const cicp).cast(),
        ) == 0
        {
            fail!("cicp tag");
        }
        for (tag, text) in [
            (sys::TagSignature::ProfileDescriptionTag, &description),
            (sys::TagSignature::CopyrightTag, &copyright),
        ] {
            let mlu = sys::cmsMLUalloc(std::ptr::null_mut(), 1);
            if mlu.is_null() {
                fail!("localized text allocation");
            }
            let mlu = MluHandle(mlu);
            if sys::cmsMLUsetASCII(mlu.0, language.as_ptr(), country.as_ptr(), text.as_ptr()) == 0
                || sys::cmsWriteTag(profile.0, tag, mlu.0.cast()) == 0
            {
                fail!("localized text");
            }
        }

        let curve = sys::cmsBuildTabulatedToneCurve16(
            std::ptr::null_mut(),
            u32::try_from(table.len()).map_err(|_| {
                NcError::Other("coded-HDR tone curve is too large for ICC".to_string())
            })?,
            table.as_ptr(),
        );
        if curve.is_null() {
            fail!("tone curve");
        }
        let curve = CurveHandle(curve);
        // Little CMS serializes `mAB ` only for a stage pattern it recognizes. The
        // compact one that fits here is M curves → Matrix → B curves, so the
        // identity B curves must be present even though they do nothing; a
        // two-stage pipeline is rejected with "LUT is not suitable to be saved as
        // LutAToB".
        let identity = sys::cmsBuildGamma(std::ptr::null_mut(), 1.0);
        if identity.is_null() {
            fail!("identity curve");
        }
        let identity = CurveHandle(identity);

        let pipeline = sys::cmsPipelineAlloc(std::ptr::null_mut(), 3, 3);
        if pipeline.is_null() {
            fail!("A2B pipeline");
        }
        let pipeline = PipelineHandle(pipeline);
        for (stage, what) in [
            (
                sys::cmsStageAllocToneCurves(
                    std::ptr::null_mut(),
                    3,
                    [curve.0.cast_const(); 3].as_ptr(),
                ),
                "tone-curve stage",
            ),
            (
                sys::cmsStageAllocMatrix(
                    std::ptr::null_mut(),
                    3,
                    3,
                    matrix.as_ptr(),
                    std::ptr::null(),
                ),
                "matrix stage",
            ),
            (
                sys::cmsStageAllocToneCurves(
                    std::ptr::null_mut(),
                    3,
                    [identity.0.cast_const(); 3].as_ptr(),
                ),
                "identity stage",
            ),
        ] {
            // Leak window, stated accurately: all three stages are allocated
            // *eagerly* when the array literal is built, before any insert runs, so a
            // null from the matrix or identity alloc leaks its already-allocated
            // siblings — not merely "a failed insert". `cmsPipelineInsertStage` takes
            // ownership on success, so inserted stages are the pipeline's. Reachable
            // only under an lcms allocation failure that fails the run anyway, which
            // is why it is documented rather than given a guard per stage.
            if stage.is_null()
                || sys::cmsPipelineInsertStage(pipeline.0, sys::StageLoc::AT_END, stage) == 0
            {
                fail!(what);
            }
        }
        if sys::cmsWriteTag(
            profile.0,
            sys::TagSignature::AToB0Tag,
            pipeline.0.cast_const().cast(),
        ) == 0
        {
            fail!("A2B0 tag");
        }

        if sys::cmsWriteTag(
            profile.0,
            sys::TagSignature::ChromaticAdaptationTag,
            chad.as_ptr().cast(),
        ) == 0
        {
            fail!("chromatic adaptation tag");
        }

        // The `BToA0` mirror. A `lutBtoAType` runs B curves → matrix → M curves —
        // the reverse order of the `AToB` above — so the identity goes first and the
        // *forward* transfer (linear → code) last. Little CMS recognizes only that
        // pattern for the compact `mBA `; a two-stage pipeline is rejected with "LUT
        // is not suitable to be saved as LutBtoA", the same way its A2B twin is.
        let reverse = sys::cmsReverseToneCurve(curve.0.cast_const());
        if reverse.is_null() {
            fail!("reverse tone curve");
        }
        let reverse = CurveHandle(reverse);
        let b2a = sys::cmsPipelineAlloc(std::ptr::null_mut(), 3, 3);
        if b2a.is_null() {
            fail!("B2A pipeline");
        }
        let b2a = PipelineHandle(b2a);
        for (stage, what) in [
            (
                sys::cmsStageAllocToneCurves(
                    std::ptr::null_mut(),
                    3,
                    [identity.0.cast_const(); 3].as_ptr(),
                ),
                "B2A identity stage",
            ),
            (
                sys::cmsStageAllocMatrix(
                    std::ptr::null_mut(),
                    3,
                    3,
                    inverse_matrix.as_ptr(),
                    std::ptr::null(),
                ),
                "B2A matrix stage",
            ),
            (
                sys::cmsStageAllocToneCurves(
                    std::ptr::null_mut(),
                    3,
                    [reverse.0.cast_const(); 3].as_ptr(),
                ),
                "B2A tone-curve stage",
            ),
        ] {
            // Same eager-allocation leak window as the A2B loop above, and the same
            // reasoning for documenting rather than guarding it.
            if stage.is_null()
                || sys::cmsPipelineInsertStage(b2a.0, sys::StageLoc::AT_END, stage) == 0
            {
                fail!(what);
            }
        }
        if sys::cmsWriteTag(
            profile.0,
            sys::TagSignature::BToA0Tag,
            b2a.0.cast_const().cast(),
        ) == 0
        {
            fail!("B2A0 tag");
        }

        let mut length: u32 = 0;
        if sys::cmsSaveProfileToMem(profile.0, std::ptr::null_mut(), &raw mut length) == 0 {
            fail!("profile size");
        }
        let mut bytes = vec![0u8; length as usize];
        if sys::cmsSaveProfileToMem(profile.0, bytes.as_mut_ptr().cast(), &raw mut length) == 0 {
            fail!("serialized profile");
        }
        // Same determinism rule as every other ICC profile Hanten writes: Little CMS stamps the
        // wall-clock creation time, so zero it or two runs seconds apart embed
        // different bytes.
        if let Some(datetime) = bytes.get_mut(ICC_HEADER_DATETIME) {
            datetime.fill(0);
        }
        Ok(bytes)
    }
}

fn cstring(text: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(text)
        .map_err(|e| NcError::Other(format!("ICC profile text is not representable: {e}")))
}

/// RAII guards for the Little CMS handles [`synth_coded_hdr`] owns, so an early
/// return cannot leak one. Each frees exactly what it holds.
struct ProfileHandle(lcms2_sys::HPROFILE);
impl Drop for ProfileHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `cmsCreateProfilePlaceholder` and is closed
        // exactly once, here.
        unsafe { lcms2_sys::cmsCloseProfile(self.0) };
    }
}

struct MluHandle(*mut lcms2_sys::MLU);
impl Drop for MluHandle {
    fn drop(&mut self) {
        // SAFETY: allocated by `cmsMLUalloc`; `cmsWriteTag` copies, so freeing
        // after the write is correct.
        unsafe { lcms2_sys::cmsMLUfree(self.0) };
    }
}

struct CurveHandle(*mut lcms2_sys::ToneCurve);
impl Drop for CurveHandle {
    fn drop(&mut self) {
        // SAFETY: allocated by `cmsBuildTabulatedToneCurve16` / `cmsBuildGamma`.
        // `cmsStageAllocToneCurves` duplicates the curves it is given, so this does
        // not free anything the pipeline still owns.
        unsafe { lcms2_sys::cmsFreeToneCurve(self.0) };
    }
}

struct PipelineHandle(*mut lcms2_sys::Pipeline);
impl Drop for PipelineHandle {
    fn drop(&mut self) {
        // SAFETY: allocated by `cmsPipelineAlloc`. `cmsWriteTag` duplicates the
        // pipeline, so freeing it after the write does not disturb the profile.
        unsafe { lcms2_sys::cmsPipelineFree(self.0) };
    }
}

/// Rows per `cmsDoTransform` call when the transform runs across cores. Each call
/// has a fixed setup cost that a band amortises; whole rows keep every pixel in the
/// order the single sequential call used.
const TRANSFORM_ROWS_PER_BAND: usize = 8;

fn transform_in_place(image: &mut LinearImage, input: &Profile, output: &Profile) -> Result<()> {
    // `NO_CACHE` drops Little CMS's one-pixel result cache, the only per-transform
    // mutable state, which is what lets one transform serve every rayon worker; it
    // changes no output value. The flag has to reach the *type*: lcms2 marks only
    // `Transform<_, _, _, DisallowCache>` as `Sync`, and the `GlobalContext`
    // shortcut constructors (`new`, `new_flags`) erase it, so the transform is built
    // through `new_flags_context` with the same null global context — identical
    // bytes, and Little CMS keeps reporting faults to the global handler `cli`
    // installs (a `ThreadContext` is equally `Sync`, but its faults would go to a
    // per-context handler nothing reads).
    let transform: Transform<[f32; 3], [f32; 3], GlobalContext, DisallowCache> =
        Transform::new_flags_context(
            GlobalContext::new(),
            input,
            PixelFormat::RGB_FLT,
            output,
            PixelFormat::RGB_FLT,
            Intent::RelativeColorimetric,
            Flags::NO_CACHE,
        )
        .map_err(|e| NcError::Other(format!("failed to build color transform: {e}")))?;
    // `rgb` is interleaved RGB with len == w*h*3 (enforced by `LinearImage::new`),
    // but the field is public, so guard against silently dropping a trailing tail.
    let band = (image.width as usize).max(1) * TRANSFORM_ROWS_PER_BAND;
    pixels::triples_mut(&mut image.rgb)?
        .par_chunks_mut(band)
        .for_each(|rows| transform.transform_in_place(rows));
    Ok(())
}

// ---------------------------------------------------------------------------
// Profile construction
// ---------------------------------------------------------------------------

/// xyY chromaticity with luminance fixed at 1.0 (as used for the white point and
/// primaries passed to `Profile::new_rgb`).
fn xyy(x: f64, y: f64) -> CIExyY {
    CIExyY { x, y, Y: 1.0 }
}

/// Little CMS profile inputs for a colour space named in
/// [`colorimetry::definitions`](crate::pipeline::colorimetry::definitions).
///
/// Every profile builder below goes through this rather than repeating
/// chromaticities. Before it existed, the Display P3 primaries and D65 white were
/// written out twice in this file, with nothing keeping the two copies in step.
/// Transfer curves are deliberately *not* part of this — they stay with the
/// builder, because the same primaries serve different curves.
fn lcms_inputs(space: ColorSpace) -> (CIExyY, [(f64, f64); 3]) {
    let [red, green, blue] = space.primaries.as_array();
    (
        xyy(space.white.x, space.white.y),
        [(red.x, red.y), (green.x, green.y), (blue.x, blue.y)],
    )
}

/// Synthesize an RGB profile from a white point, primaries `[r, g, b]` and one
/// tone curve shared by all three channels. Little CMS builds an ICC v4 profile
/// here: the PCS is D50, so a non-D50 `white` (Rec.709/ACES D65/D60) is
/// Bradford-adapted into D50-relative colorants with the `chromaticAdaptationTag`
/// written and D50 stored as the media white — the encoding white stays the
/// `white` argument.
fn synth_curve(white: CIExyY, primaries: [(f64, f64); 3], curve: &ToneCurve) -> Result<Profile> {
    let prim = CIExyYTRIPLE {
        Red: xyy(primaries[0].0, primaries[0].1),
        Green: xyy(primaries[1].0, primaries[1].1),
        Blue: xyy(primaries[2].0, primaries[2].1),
    };
    Profile::new_rgb(&white, &prim, &[curve, curve, curve])
        .map_err(|e| NcError::Other(format!("failed to build RGB profile: {e}")))
}

/// Synthesize an RGB profile with a single power-law `gamma` on every channel.
fn synth(white: CIExyY, primaries: [(f64, f64); 3], gamma: f64) -> Result<Profile> {
    synth_curve(white, primaries, &ToneCurve::new(gamma))
}

/// The piecewise sRGB transfer curve (IEC 61966-2.1) — the same curve Little
/// CMS's built-in `new_srgb()` carries, built explicitly here for the Display P3
/// profile (`new_srgb()` covers the sRGB output, so `srgb_trc` is Display P3's
/// only caller). A Little CMS **parametric** type-4 curve —
/// `Y = ((a·X + b))^g` for `X ≥ d`, else `Y = c·X`, with the standard sRGB
/// parameters — so the near-black linear toe is exact rather than the visibly
/// wrong result of approximating the whole curve by a single gamma-2.2 power. The
/// stored curve is the device→PCS (decode) direction; the working→output
/// transform inverts it to encode linear values, so a linear value maps to its
/// exact sRGB-encoded counterpart (e.g. linear 0.5 → 0.735357).
fn srgb_trc() -> Result<ToneCurve> {
    // Type 4 params [g, a, b, c, d] for the standard sRGB curve, from the single
    // definition in `colorimetry::definitions::transfer::srgb`.
    use transfer::srgb;
    ToneCurve::new_parametric(4, &[srgb::G, srgb::A, srgb::B, srgb::C, srgb::D])
        .map_err(|e| NcError::Other(format!("failed to build sRGB tone curve: {e}")))
}

/// The linear Rec.709 / D65 working-space profile (see module docs).
fn working_profile() -> Result<Profile> {
    let (white, primaries) = lcms_inputs(definitions::REC709);
    synth(white, primaries, 1.0)
}

/// Build the lcms2 profile for an output space.
fn build_profile(space: &OutputSpace) -> Result<Profile> {
    match space {
        // Built-in sRGB: Rec.709 primaries, D65, sRGB TRC.
        OutputSpace::SRgb => Ok(Profile::new_srgb()),
        // ACEScg: AP1 primaries, ACES white (~D60), linear.
        OutputSpace::AcesCg => {
            let (white, primaries) = lcms_inputs(definitions::ACESCG);
            synth(white, primaries, 1.0)
        }
        // Display P3 SDR: P3 primaries, D65 encoding white, piecewise sRGB TRC.
        // Little CMS Bradford-adapts the D65 colorants to the D50 PCS and writes
        // the `chromaticAdaptationTag`; D50 is the media white, D65 the encoding
        // white (colorants verified against the ICC-registry Display P3 reference
        // by the tests). Synthesized cross-platform — no dependency on macOS's
        // system `Display P3.icc`.
        OutputSpace::DisplayP3 => {
            let (white, primaries) = lcms_inputs(definitions::DISPLAY_P3);
            synth_curve(white, primaries, &srgb_trc()?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::FilmRgbImage;
    use crate::pipeline::display_tone::Headroom;
    use crate::pipeline::render_split::display_source;
    use crate::pipeline::sdr;
    use crate::pipeline::working_space::map_nc_film_rgb_v1;
    use crate::types::PrintParams;

    fn gray_image(v: f32) -> LinearImage {
        LinearImage::new(1, 1, vec![v, v, v], None).unwrap()
    }

    /// The linear working → `space` transform, built and run the way the encoders
    /// build and run theirs.
    fn transform_to(mut image: LinearImage, space: &OutputSpace) -> Result<LinearImage> {
        transform_in_place(&mut image, &working_profile()?, &build_profile(space)?)?;
        Ok(image)
    }

    fn render_sdr(rgb: &[f32], gamut: SdrGamut) -> RenderedSdr {
        let film = FilmRgbImage::fixture(
            LinearImage::new((rgb.len() / 3) as u32, 1, rgb.to_vec(), None).unwrap(),
        );
        let shared = display_source(map_nc_film_rgb_v1(film), &PrintParams::default()).unwrap();
        sdr::render(&shared, gamut, Headroom::default()).unwrap()
    }

    #[test]
    fn neutral_gray_maps_to_srgb_encoded_value() {
        // Linear 0.5 in the working space → sRGB-encoded ~0.7353.
        let out = transform_to(gray_image(0.5), &OutputSpace::SRgb).unwrap();
        for &c in &out.rgb {
            assert!((c - 0.7353).abs() < 0.005, "got {c}, expected ~0.7353");
        }
    }

    #[test]
    fn srgb_round_trip_within_tolerance() {
        // working → sRGB, then sRGB → working should recover the input.
        let encoded = transform_to(gray_image(0.5), &OutputSpace::SRgb).unwrap();
        let working = working_profile().unwrap();
        let srgb = Profile::new_srgb();
        let back: Transform<[f32; 3], [f32; 3]> = Transform::new(
            &srgb,
            PixelFormat::RGB_FLT,
            &working,
            PixelFormat::RGB_FLT,
            Intent::RelativeColorimetric,
        )
        .unwrap();
        let mut buf = encoded.rgb.clone();
        let (px, _) = buf.as_chunks_mut::<3>();
        back.transform_in_place(px);
        for &c in &buf {
            assert!((c - 0.5).abs() < 0.005, "round-trip got {c}, expected ~0.5");
        }
    }

    #[test]
    fn icc_profile_bytes_are_deterministic_with_zeroed_datetime() {
        // The header creation dateTimeNumber (bytes 24..36) is wall-clock time at
        // synthesis — it must be zeroed or byte-identical reruns fail across a
        // second boundary (caught by CI on the E2E recipe round-trip).
        let a = icc_profile(&OutputSpace::AcesCg).unwrap();
        assert!(
            a[24..36].iter().all(|&b| b == 0),
            "ICC creation dateTime must be zeroed for determinism"
        );
        let b = icc_profile(&OutputSpace::AcesCg).unwrap();
        assert_eq!(a, b, "same space must serialize to identical bytes");
    }

    #[test]
    fn icc_profile_bytes_are_valid_for_builtins() {
        for space in [
            OutputSpace::SRgb,
            OutputSpace::AcesCg,
            OutputSpace::DisplayP3,
        ] {
            let bytes = icc_profile(&space).unwrap();
            assert!(!bytes.is_empty(), "{space:?} produced empty ICC");
            // Re-openable as a valid profile.
            Profile::new_icc(&bytes).unwrap_or_else(|e| panic!("{space:?} ICC invalid: {e}"));
        }
    }

    #[test]
    fn ir_plane_is_carried_through_untouched() {
        // The IR plane must survive the color transform byte-for-byte (it is
        // preserved, not consumed, in Step 1).
        let img = LinearImage::new(1, 1, vec![0.5, 0.5, 0.5], Some(vec![0.42])).unwrap();
        let out = transform_to(img, &OutputSpace::SRgb).unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        assert_eq!(out.ir, Some(vec![0.42]));
    }

    #[test]
    fn malformed_rgb_length_fails_loudly() {
        // The loud `rgb.len() % 3` guard must still fire (`rgb` is `pub`, so a
        // malformed buffer is reachable): `as_chunks_mut` would otherwise silently
        // drop the tail and leave those pixels un-transformed.
        //
        let mut img = LinearImage::new(1, 1, vec![0.5, 0.5, 0.5], None).unwrap();
        img.rgb.push(0.25); // len 4 — not a multiple of 3
        let err = transform_to(img, &OutputSpace::SRgb).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not a multiple of 3"), "{err}");
    }

    #[test]
    fn wide_gamut_remap_moves_saturated_red() {
        // The ACEScg profile is the tag `film-master` embeds, so its primaries must
        // be real ones. Gray hides primaries errors; a saturated color does not.
        // Rec.709 red encoded into the wider AP1 gamut must pull R below 1.0 and
        // lift G/B off 0 — this pins down the primaries/white-point, not just the
        // (linear) TRC.
        let gray = transform_to(gray_image(0.5), &OutputSpace::AcesCg).unwrap();
        for &c in &gray.rgb {
            assert!((0.3..0.7).contains(&c), "AcesCg gray {c} far from 0.5");
        }
        let img = LinearImage::new(1, 1, vec![1.0, 0.0, 0.0], None).unwrap();
        let out = transform_to(img, &OutputSpace::AcesCg).unwrap();
        let [r, g, b] = [out.rgb[0], out.rgb[1], out.rgb[2]];
        assert!(r < 1.0, "expected R pulled below 1.0, got {r}");
        assert!(g > 0.0 && b > 0.0, "expected G/B lifted off 0, got {g}/{b}");
    }

    // -----------------------------------------------------------------------
    // Display P3 SDR output
    // -----------------------------------------------------------------------

    /// Reference (Bradford D65→D50) Display P3 colorant XYZ from the ICC registry
    /// profile — the external values a correctly synthesized profile must match
    /// (byte identity is not required; these are cross-checked, not copied from
    /// our own primaries). Same values macOS's `Display P3.icc` carries.
    const P3_RED_COLORANT: [f64; 3] = [0.51512, 0.24119, -0.00105];
    const P3_GREEN_COLORANT: [f64; 3] = [0.29198, 0.69225, 0.04189];
    const P3_BLUE_COLORANT: [f64; 3] = [0.15710, 0.06657, 0.78407];
    /// D50 PCS white (ICC `mediaWhitePoint` for every v4 display profile).
    const D50_WHITE: [f64; 3] = [0.9642, 1.0, 0.8249];

    /// The standard piecewise sRGB OETF (linear → encoded), used to compute the
    /// expected encoded value independently of Little CMS's parametric curve.
    ///
    /// The constants below are deliberately **not** sourced from
    /// `definitions::transfer::srgb`, and must not be "centralized" onto it: this
    /// is the independent oracle for the curve `srgb_trc` builds from those very
    /// parameters. Pointing it at them would make a mistyped parameter agree with
    /// itself and the check would stop checking anything. They are also the
    /// standard's *encode* direction, which is not the form type 4 stores.
    fn srgb_encode(l: f32) -> f32 {
        if l <= 0.003_130_8 {
            12.92 * l
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        }
    }

    /// Chromaticity `(x, y)` of an XYZ triple.
    fn xy_of(xyz: [f32; 3]) -> (f32, f32) {
        let sum = xyz[0] + xyz[1] + xyz[2];
        (xyz[0] / sum, xyz[1] / sum)
    }

    /// Standard Bradford chromatic adaptation D50 → D65 (Lindbloom reference
    /// matrix). Undoes the D50 PCS adaptation baked into the profile so the
    /// recovered chromaticities can be checked against the registered D65 P3
    /// encoding — an external constant, independent of our construction.
    fn bradford_d50_to_d65(xyz: [f32; 3]) -> [f32; 3] {
        const M: [[f32; 3]; 3] = [
            [0.9555766, -0.0230393, 0.0631636],
            [-0.0282895, 1.0099416, 0.0210077],
            [0.0122982, -0.0204830, 1.3299098],
        ];
        [
            M[0][0] * xyz[0] + M[0][1] * xyz[1] + M[0][2] * xyz[2],
            M[1][0] * xyz[0] + M[1][1] * xyz[1] + M[1][2] * xyz[2],
            M[2][0] * xyz[0] + M[2][1] * xyz[1] + M[2][2] * xyz[2],
        ]
    }

    #[test]
    fn display_p3_profile_is_rgb_display_class_with_d50_pcs() {
        use lcms2::{ColorSpaceSignature, ProfileClassSignature, Tag, TagSignature};
        let profile = build_profile(&OutputSpace::DisplayP3).unwrap();

        // RGB Display-class ICC v4 profile.
        assert_eq!(profile.color_space(), ColorSpaceSignature::RgbData);
        assert_eq!(profile.device_class(), ProfileClassSignature::DisplayClass);
        assert!(
            profile.version() >= 4.0,
            "expected ICC v4, got {}",
            profile.version()
        );

        // Media white is the D50 PCS white — NOT the D65 encoding white.
        let Tag::CIEXYZ(wp) = profile.read_tag(TagSignature::MediaWhitePointTag) else {
            panic!("missing media white point tag");
        };
        for (got, want) in [wp.X, wp.Y, wp.Z].into_iter().zip(D50_WHITE) {
            assert!((got - want).abs() < 1e-3, "media white {got} != D50 {want}");
        }
        // A D65-as-media-white bug would put x≈0.3127; assert we are NOT there.
        let (wx, _) = xy_of([wp.X as f32, wp.Y as f32, wp.Z as f32]);
        assert!(
            (wx - 0.3457).abs() < 1e-3,
            "media white chromaticity should be D50 (~0.3457), got x={wx}"
        );

        // Required chromatic-adaptation tag is present (D65→D50 Bradford).
        assert!(
            profile.has_tag(TagSignature::ChromaticAdaptationTag),
            "Display P3 profile must carry the chromaticAdaptationTag"
        );
    }

    #[test]
    fn display_p3_colorants_match_icc_registry_reference() {
        use lcms2::{Tag, TagSignature};
        let profile = build_profile(&OutputSpace::DisplayP3).unwrap();
        for (sig, want) in [
            (TagSignature::RedColorantTag, P3_RED_COLORANT),
            (TagSignature::GreenColorantTag, P3_GREEN_COLORANT),
            (TagSignature::BlueColorantTag, P3_BLUE_COLORANT),
        ] {
            let Tag::CIEXYZ(c) = profile.read_tag(sig) else {
                panic!("missing colorant tag {sig:?}");
            };
            for (got, want) in [c.X, c.Y, c.Z].into_iter().zip(want) {
                assert!(
                    (got - want).abs() < 2e-3,
                    "{sig:?} colorant {got} != registry reference {want}"
                );
            }
        }
    }

    #[test]
    fn display_p3_trc_is_parametric_srgb_not_gamma() {
        use lcms2::{Tag, TagSignature};
        let profile = build_profile(&OutputSpace::DisplayP3).unwrap();
        let Tag::ToneCurve(trc) = profile.read_tag(TagSignature::RedTRCTag) else {
            panic!("missing red TRC");
        };
        // Type 4 == IEC 61966-2.1 (sRGB) parametric curve, not a plain gamma.
        assert_eq!(
            trc.parametric_type(),
            4,
            "Display P3 TRC must be the parametric sRGB curve (type 4)"
        );
    }

    #[test]
    fn parallel_transform_matches_one_sequential_call() {
        // Same machine, same lcms2 build: the banded parallel transform must be
        // bit-identical to one sequential `cmsDoTransform` over the whole buffer.
        // 37 rows is not a multiple of the band, so the short tail band runs too.
        let working = working_profile().unwrap();
        let output = build_profile(&OutputSpace::DisplayP3).unwrap();
        let (w, h) = (64_u32, 37_u32);
        let rgb: Vec<f32> = (0..(w * h * 3) as usize)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 * 1.2 - 0.05)
            .collect();
        let mut image = LinearImage::new(w, h, rgb.clone(), None).unwrap();
        transform_in_place(&mut image, &working, &output).unwrap();

        let mut expect = rgb;
        let t: Transform<[f32; 3], [f32; 3]> = Transform::new(
            &working,
            PixelFormat::RGB_FLT,
            &output,
            PixelFormat::RGB_FLT,
            Intent::RelativeColorimetric,
        )
        .unwrap();
        t.transform_in_place(expect.as_chunks_mut::<3>().0);
        assert_eq!(image.rgb, expect);
    }

    #[test]
    fn display_p3_decodes_to_registered_d65_encoding() {
        // Independent end-to-end decode: transform encoded-P3 primaries/white
        // through the generated profile into the D50 PCS XYZ with Little CMS, then
        // un-adapt D50 → D65 (Bradford) and recover chromaticities. They must
        // match the registered D65 Display P3 encoding — proving the profile
        // round-trips through a color engine to the right primaries and white.
        let profile = build_profile(&OutputSpace::DisplayP3).unwrap();
        let xyz = Profile::new_xyz(); // D50 PCS
        let t: Transform<[f32; 3], [f32; 3]> = Transform::new(
            &profile,
            PixelFormat::RGB_FLT,
            &xyz,
            PixelFormat::XYZ_FLT,
            Intent::RelativeColorimetric,
        )
        .unwrap();
        // Encoded [1,1,1]/[1,0,0]/[0,1,0]/[0,0,1] → XYZ(D50); check recovered xy
        // after un-adapting back to the D65 encoding illuminant.
        let mut px = [
            [1.0f32, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ];
        t.transform_in_place(&mut px);
        let expect = [
            (0.3127, 0.3290), // D65 white
            (0.680, 0.320),   // P3 red
            (0.265, 0.690),   // P3 green
            (0.150, 0.060),   // P3 blue
        ];
        for (got, (ex, ey)) in px.into_iter().zip(expect) {
            let (x, y) = xy_of(bradford_d50_to_d65(got));
            assert!(
                (x - ex).abs() < 3e-3 && (y - ey).abs() < 3e-3,
                "recovered ({x},{y}) != registered ({ex},{ey})"
            );
        }
    }

    #[test]
    fn hdr_linear_bt2020_profile_is_rgb_display_class_with_a_linear_trc() {
        // Two properties, both load-bearing.
        //
        // **Linear TRC.** A gamma-1.0 curve is what makes the profile describe the
        // samples the encoder writes; any other curve would claim a transfer
        // function that `render_linear` never applied.
        //
        // **RGB data space + Display class.** ICC.1:2022 §9.2.17 permits a `cicpTag`
        // only for an RGB/YCbCr/XYZ data space in an Input or Display profile. This
        // profile carries no `cicpTag` (see `hdr_linear_bt2020_icc`), but the PQ/HLG
        // TIFFs in this task's second half must, and they are built by the same
        // `synth` helper — so pinning the class and space here is what tells the next
        // author the permission holds before they add the tag.
        use lcms2::{ColorSpaceSignature, ProfileClassSignature, Tag, TagSignature};
        let (white, primaries) = lcms_inputs(definitions::BT2020);
        let profile = synth(white, primaries, 1.0).unwrap();

        let Tag::ToneCurve(trc) = profile.read_tag(TagSignature::RedTRCTag) else {
            panic!("missing red TRC");
        };
        assert!(
            trc.is_linear(),
            "the linear-BT.2020 profile's TRC must be linear (parametric type {})",
            trc.parametric_type()
        );
        assert_eq!(profile.color_space(), ColorSpaceSignature::RgbData);
        assert_eq!(profile.device_class(), ProfileClassSignature::DisplayClass);
    }

    #[test]
    fn hdr_linear_bt2020_decodes_to_the_bt2020_primaries() {
        // The same independent end-to-end decode `display_p3_decodes_to_registered_
        // d65_encoding` performs, against the BT.2020 profile: push encoded
        // white/R/G/B through Little CMS into the D50 PCS, un-adapt to D65, and
        // recover chromaticities. This is what proves the profile actually describes
        // BT.2020 rather than merely having been built from those numbers.
        let (white, primaries) = lcms_inputs(definitions::BT2020);
        let profile = synth(white, primaries, 1.0).unwrap();
        let xyz = Profile::new_xyz();
        let t: Transform<[f32; 3], [f32; 3]> = Transform::new(
            &profile,
            PixelFormat::RGB_FLT,
            &xyz,
            PixelFormat::XYZ_FLT,
            Intent::RelativeColorimetric,
        )
        .unwrap();
        let mut px = [
            [1.0f32, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ];
        t.transform_in_place(&mut px);
        // Taken from `definitions::BT2020` rather than retyped, so the test cannot
        // drift from the source of truth it is checking.
        let [red, green, blue] = definitions::BT2020.primaries.as_array();
        let expect = [
            (definitions::BT2020.white.x, definitions::BT2020.white.y),
            (red.x, red.y),
            (green.x, green.y),
            (blue.x, blue.y),
        ];
        for (got, (ex, ey)) in px.into_iter().zip(expect) {
            let (x, y) = xy_of(bradford_d50_to_d65(got));
            assert!(
                (x - ex as f32).abs() < 3e-3 && (y - ey as f32).abs() < 3e-3,
                "recovered ({x},{y}) != BT.2020 ({ex},{ey})"
            );
        }
    }

    #[test]
    fn hdr_linear_bt2020_icc_is_deterministic_and_named() {
        // Same dateTime-zeroing guarantee the other synthesized profiles get: two
        // builds seconds apart must be byte-identical, or the TIFF's determinism
        // contract fails on a single seconds byte buried in the profile header.
        let first = hdr_linear_bt2020_icc().unwrap();
        let second = hdr_linear_bt2020_icc().unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
        // And it is *not* the ACEScg or Rec.709 profile — a copy-paste of the wrong
        // `definitions` constant would otherwise pass every assertion above.
        assert_ne!(first, icc_profile(&OutputSpace::AcesCg).unwrap());
        assert_ne!(first, icc_profile(&OutputSpace::SRgb).unwrap());
        // Carries a real description, not Little CMS's default "RGB built-in".
        let profile = Profile::new_icc(&first).unwrap();
        let description = profile
            .info(lcms2::InfoType::Description, lcms2::Locale::none())
            .unwrap_or_default();
        assert!(
            description.contains("BT.2020") && description.contains("Linear"),
            "unexpected profile description {description:?}"
        );
        // The older shipped profiles are deliberately left alone, so their bytes
        // do not move: this asserts the scope of the naming change.
        let srgb = Profile::new_icc(&icc_profile(&OutputSpace::SRgb).unwrap()).unwrap();
        assert_eq!(
            srgb.info(lcms2::InfoType::Description, lcms2::Locale::none())
                .unwrap_or_default(),
            "sRGB built-in",
            "the sRGB profile description must not change here"
        );
    }

    /// The language/country of tag `sig`'s first `multiLocalizedUnicodeType` record,
    /// read straight out of the profile's tag table.
    ///
    /// Deliberately *not* via `Profile::info`: that takes a [`lcms2::Locale`], so
    /// asking with the same null locale a bug would have written round-trips
    /// trivially and proves nothing. Parsing the bytes is what makes the assertion
    /// falsifiable.
    fn mluc_locale(icc: &[u8], sig: &[u8; 4]) -> (Vec<u8>, Vec<u8>) {
        let be32 = |at: usize| u32::from_be_bytes(icc[at..at + 4].try_into().unwrap()) as usize;
        let count = be32(128);
        let (offset, _) = (0..count)
            .map(|i| 132 + i * 12)
            .find(|&e| &icc[e..e + 4] == sig)
            .map(|e| (be32(e + 4), be32(e + 8)))
            .unwrap_or_else(|| panic!("{} tag is absent", String::from_utf8_lossy(sig)));
        assert_eq!(&icc[offset..offset + 4], b"mluc");
        // `mluc`: sig, reserved, record count, record size, then records of
        // (language, country, length, offset).
        assert!(be32(offset + 8) >= 1, "no localized records");
        let record = offset + 16;
        (
            icc[record..record + 2].to_vec(),
            icc[record + 2..record + 4].to_vec(),
        )
    }

    #[test]
    fn every_named_profile_tags_its_description_en_us() {
        // ICC.1:2022 §10.15 wants an ISO 639-1 language and an ISO 3166-1 country; an
        // all-zero pair is neither, and a reader that requests a locale without
        // falling back to record 0 then shows no description at all. The linear
        // profile used to write the null locale while Little CMS's own default path
        // put `enUS` on the *same profile's* `cprt` — internally inconsistent, which
        // is the shape of the bug this pins.
        for icc in [
            hdr_linear_bt2020_icc().unwrap(),
            hdr_pq_tiff_icc().unwrap(),
            hdr_hlg_tiff_icc().unwrap(),
            adobe_rgb_icc(),
        ] {
            assert_eq!(
                mluc_locale(&icc, b"desc"),
                (b"en".to_vec(), b"US".to_vec()),
                "description locale"
            );
            // And it agrees with the copyright record in the same profile, which is
            // the comparison that exposed the inconsistency.
            assert_eq!(mluc_locale(&icc, b"desc"), mluc_locale(&icc, b"cprt"));
        }
    }

    /// PQ code values for known absolute luminances, from the ST 2084 OETF
    /// evaluated independently (Python, binary64) rather than by calling nc's own
    /// encoder — so these anchor the profile against the standard, not against the
    /// function under test.
    const PQ_CODE_FOR_NITS: [(f64, f64); 8] = [
        (0.062_336_866, 0.1),
        (0.149_945_732, 1.0),
        (0.299_699_092, 10.0),
        (0.357_012_408, 20.0),
        (0.440_281_573, 50.0),
        (0.508_078_422, 100.0),
        (0.580_688_881, 203.0),
        (0.751_827_096, 1000.0),
    ];

    /// Transform a neutral code value through a profile into PCS XYZ.
    ///
    /// Takes the code in `f64` because the anchors are computed in binary64; the
    /// narrowing to the transform's `f32` pixel format happens here, once.
    fn pcs_y_of(icc: &[u8], code: f64) -> f64 {
        let profile = Profile::new_icc(icc).unwrap();
        let xyz = Profile::new_xyz();
        let transform: Transform<[f32; 3], [f32; 3]> = Transform::new(
            &profile,
            PixelFormat::RGB_FLT,
            &xyz,
            PixelFormat::XYZ_FLT,
            Intent::RelativeColorimetric,
        )
        .unwrap();
        let mut pixel = [[code as f32; 3]];
        transform.transform_in_place(&mut pixel);
        f64::from(pixel[0][1])
    }

    #[test]
    fn pq_tiff_profile_places_reference_white_at_pcs_one_and_does_not_clip() {
        // The property the whole A2B design exists for: diffuse white at PCS 1.0
        // (so a CMM ignoring `cicp` renders it correctly) *and* an extended range
        // above it (so highlights are not destroyed). A matrix-shaper profile can
        // have one or the other, never both.
        let icc = hdr_pq_tiff_icc().unwrap();
        let mut worst = 0.0_f64;
        for (code, nits) in PQ_CODE_FOR_NITS {
            let recovered = pcs_y_of(&icc, code) * f64::from(hdr::REFERENCE_WHITE_NITS);
            let error = (recovered - nits).abs() / nits;
            // Above 20 nits the transform is accurate to ~0.1%; below ~1 nit the
            // 16-bit curve step (0.153 nits) dominates, which is an absolute error
            // under 0.08 nits — measured, and documented on the builder.
            let allowed = if nits >= 20.0 { 0.005 } else { 1.0 };
            assert!(
                error < allowed,
                "PQ profile at {nits} nits recovered {recovered:.4} (rel {error:.4})"
            );
            if nits >= 20.0 {
                worst = worst.max(error);
            }
        }
        assert!(worst < 0.005, "worst error above 20 nits was {worst:.5}");

        // Extended range, and the peak lands on the renderer's own headroom.
        let peak = pcs_y_of(&icc, 1.0);
        assert!(peak > 40.0, "PCS peak {peak} is not an extended range");
        let thousand = pcs_y_of(&icc, 0.751_827_096);
        assert!(
            (thousand - f64::from(hdr::LINEAR_HEADROOM)).abs() < 0.01,
            "1000 nits should land on LINEAR_HEADROOM ({}), got {thousand}",
            hdr::LINEAR_HEADROOM
        );
    }

    #[test]
    fn hlg_tiff_profile_is_scene_referred_with_diffuse_white_at_pcs_one() {
        // Scene-referred by design (the OOTF is not per-channel separable), so the
        // only luminance claim it makes is where diffuse white sits.
        let icc = hdr_hlg_tiff_icc().unwrap();
        let white = pcs_y_of(&icc, f64::from(hdr::hlg_reference_white_signal()));
        assert!(
            (white - 1.0).abs() < 0.005,
            "HLG diffuse white should be PCS 1.0, got {white}"
        );
        let peak = pcs_y_of(&icc, 1.0);
        assert!(
            peak > 3.0,
            "HLG peak {peak} should carry scene range above diffuse white"
        );
        assert!(pcs_y_of(&icc, 0.0).abs() < 1e-6, "black must be zero");
    }

    #[test]
    fn coded_tiff_profiles_carry_the_normative_cicp_quadruples() {
        use lcms2::{Tag, TagSignature};
        for (icc, want, name) in [
            (hdr_pq_tiff_icc().unwrap(), (9u8, 16u8, 0u8, 1u8), "pq"),
            (hdr_hlg_tiff_icc().unwrap(), (9, 18, 0, 1), "hlg"),
        ] {
            let profile = Profile::new_icc(&icc).unwrap();
            let Tag::VideoSignal(cicp) = profile.read_tag(TagSignature::CicpTag) else {
                panic!("{name}: no cicp tag");
            };
            let got = (
                cicp.ColourPrimaries,
                cicp.TransferCharacteristics,
                cicp.MatrixCoefficients,
                cicp.VideoFullRangeFlag,
            );
            assert_eq!(got, want, "{name}: cicp quadruple");
            // The one that is easy to get wrong: ICC.1:2022 §10.3 requires
            // MatrixCoefficients 0 for an RGB data space. `io::avif` writes 9 for
            // the same rendition because AVIF stores Y'CbCr; if that value ever
            // leaks in here the file is non-conformant.
            assert_eq!(
                cicp.MatrixCoefficients, 0,
                "{name}: MatrixCoefficients must be 0 for an RGB profile, not AVIF's 9"
            );
            // `cicp` is only permitted for an RGB/YCbCr/XYZ data space in an Input
            // or Display profile, so the class and space are part of its validity.
            assert_eq!(profile.color_space(), lcms2::ColorSpaceSignature::RgbData);
            assert_eq!(
                profile.device_class(),
                lcms2::ProfileClassSignature::DisplayClass
            );
        }
    }

    #[test]
    fn coded_tiff_profiles_carry_the_tags_icc_requires_of_a_display_profile() {
        // The two conformance gaps `output/lossless-hdr-tiff` shipped with and
        // `output/presets` closed: ICC.1:2022 §8.4.2 requires `BToA0Tag` alongside
        // `AToB0Tag` for an N-component LUT-based Display profile, and §8.2 requires
        // `chromaticAdaptationTag` when the colorants' adopted white differs from the
        // PCS adopted white (they are Bradford D65→D50 adapted here).
        //
        // Read straight out of the serialized tag table rather than through Little
        // CMS, which would happily synthesize what it cannot find — the question is
        // what the *bytes* contain, since that is what another CMM reads.
        for (name, icc) in [
            ("PQ", hdr_pq_tiff_icc().unwrap()),
            ("HLG", hdr_hlg_tiff_icc().unwrap()),
        ] {
            let tags = icc_tag_signatures(&icc);
            for want in ["A2B0", "B2A0", "chad", "wtpt", "cicp"] {
                assert!(
                    tags.iter().any(|t| t == want),
                    "{name} profile is missing the {want} tag; has {tags:?}"
                );
            }
        }
    }

    /// The four-character signatures in a serialized profile's tag table.
    fn icc_tag_signatures(icc: &[u8]) -> Vec<String> {
        let count = u32::from_be_bytes(icc[128..132].try_into().unwrap()) as usize;
        (0..count)
            .map(|i| {
                let at = 132 + i * 12;
                String::from_utf8_lossy(&icc[at..at + 4])
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn coded_tiff_profiles_are_deterministic_named_and_distinct() {
        let pq = hdr_pq_tiff_icc().unwrap();
        let hlg = hdr_hlg_tiff_icc().unwrap();
        assert_eq!(pq, hdr_pq_tiff_icc().unwrap(), "PQ profile is not stable");
        assert_eq!(
            hlg,
            hdr_hlg_tiff_icc().unwrap(),
            "HLG profile is not stable"
        );
        assert_ne!(pq, hlg, "PQ and HLG profiles must differ");
        // Unlike the matrix-shaper profiles, these carry a real description rather
        // than Little CMS's default "RGB built-in".
        for (icc, expect) in [(&pq, "PQ"), (&hlg, "HLG")] {
            let profile = Profile::new_icc(icc).unwrap();
            let description = profile.info(lcms2::InfoType::Description, lcms2::Locale::none());
            let description = description.unwrap_or_default();
            assert!(
                description.contains("BT.2100") && description.contains(expect),
                "profile description {description:?} does not name the encoding"
            );
        }
    }

    #[test]
    fn rendered_p3_samples_encode_with_srgb_trc_and_matching_profile() {
        // Exercise the public renderer → opaque value → encoder seam. The encoder
        // receives no independently selectable gamut, so a mismatched profile is
        // unrepresentable; each rendered-linear channel receives only the sRGB TRC.
        let rendered = render_sdr(
            &[0.002, 0.002, 0.002, 0.5, 0.5, 0.5, 0.8, 0.1, 0.4],
            SdrGamut::DisplayP3,
        );
        let input: Vec<[f32; 3]> = rendered.image().rgb.as_chunks::<3>().0.to_vec();
        let (encoded, icc, metadata) = encode_rendered_sdr(rendered).unwrap();
        assert_eq!(icc, icc_profile(&OutputSpace::DisplayP3).unwrap());
        assert_eq!(metadata.gamut, SdrGamut::DisplayP3);
        let encoded: Vec<[f32; 3]> = encoded.rgb.as_chunks::<3>().0.to_vec();
        for (got, inp) in encoded.iter().copied().zip(input.iter().copied()) {
            for ch in 0..3 {
                let want = srgb_encode(inp[ch]);
                assert!(
                    (got[ch] - want).abs() < 2e-3,
                    "channel {ch}: {} != sRGB-encoded {want} (input {})",
                    got[ch],
                    inp[ch]
                );
            }
        }
    }

    #[test]
    fn the_new_flows_display_linear_encode_applies_only_the_srgb_curve() {
        // The new flow's destination half of "the declared profile matches the pixels":
        // `fit_gamut` already moved the pixels into Display P3, so the encode must apply
        // the sRGB curve and nothing else — no second gamut transform — and embed the
        // Display P3 profile. The IR plane rides through untouched.
        let linear = [0.002, 0.002, 0.002, 0.5, 0.5, 0.5, 0.8, 0.1, 0.4];
        let image = LinearImage::new(3, 1, linear.to_vec(), Some(vec![0.1, 0.2, 0.3])).unwrap();
        let (encoded, icc) = encode_display_linear(image, DestinationGamut::DisplayP3).unwrap();
        assert_eq!(icc, icc_profile(&OutputSpace::DisplayP3).unwrap());
        assert_eq!(encoded.ir, Some(vec![0.1, 0.2, 0.3]));
        for (got, inp) in encoded.rgb.iter().zip(linear) {
            let want = srgb_encode(inp);
            assert!(
                (got - want).abs() < 2e-3,
                "{got} != sRGB-encoded {want} (input {inp})"
            );
        }
    }

    #[test]
    fn rendered_srgb_samples_encode_with_srgb_trc_and_matching_profile() {
        let rendered = render_sdr(
            &[0.002, 0.002, 0.002, 0.5, 0.5, 0.5, 0.8, 0.1, 0.4],
            SdrGamut::SRgb,
        );
        let input: Vec<[f32; 3]> = rendered.image().rgb.as_chunks::<3>().0.to_vec();
        let (encoded, icc, metadata) = encode_rendered_sdr(rendered).unwrap();
        assert_eq!(icc, icc_profile(&OutputSpace::SRgb).unwrap());
        assert_eq!(metadata.gamut, SdrGamut::SRgb);
        let encoded: Vec<[f32; 3]> = encoded.rgb.as_chunks::<3>().0.to_vec();
        for (got, inp) in encoded.iter().copied().zip(input) {
            for ch in 0..3 {
                let want = srgb_encode(inp[ch]);
                assert!(
                    (got[ch] - want).abs() < 2e-3,
                    "channel {ch}: {} != sRGB-encoded {want} (input {})",
                    got[ch],
                    inp[ch]
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Adobe RGB (1998), the new chain's second destination gamut
    // -----------------------------------------------------------------------

    /// The Adobe RGB profile's bytes, as [`encode_display_linear`] embeds them.
    fn adobe_rgb_icc() -> Vec<u8> {
        let image = LinearImage::new(1, 1, vec![0.5; 3], None).unwrap();
        encode_display_linear(image, DestinationGamut::AdobeRgb)
            .unwrap()
            .1
    }

    /// Colorant XYZ (Bradford D65→D50) of Adobe's own `AdobeRGB1998.icc` — external
    /// values a profile synthesized from the published primaries must reproduce,
    /// re-typed from that profile and not from `definitions::ADOBE_RGB`.
    const ADOBE_RGB_RED_COLORANT: [f64; 3] = [0.60974, 0.31111, 0.01947];
    const ADOBE_RGB_GREEN_COLORANT: [f64; 3] = [0.20528, 0.62567, 0.06087];
    const ADOBE_RGB_BLUE_COLORANT: [f64; 3] = [0.14919, 0.06322, 0.74457];

    /// How far a synthesized colorant may sit from Adobe's: measured at most 1.74e-5
    /// (blue Z), which is the sum of three roundings — the profile's `s15Fixed16`
    /// step (1.5e-5), the published values' five decimals, and `definitions::D65`
    /// spelled to four decimals where Adobe's derivation used five. Moving any primary
    /// coordinate by 1e-3 moves a colorant by 1e-3 to 1.7e-3, twenty times this bound.
    const ADOBE_RGB_COLORANT_TOLERANCE: f64 = 5e-5;

    /// The Adobe RGB encode (linear → encoded), the oracle for the profile's curve.
    ///
    /// The exponent is typed here as the encoding specification writes it (`2 51/256`),
    /// deliberately **not** read from `definitions::transfer::adobe_rgb::GAMMA`: this is
    /// the independent check on that constant, for the reason [`srgb_encode`] gives.
    fn adobe_rgb_encode(l: f32) -> f32 {
        l.powf(1.0 / (2.0 + 51.0 / 256.0))
    }

    #[test]
    fn adobe_rgb_colorants_match_adobes_profile() {
        use lcms2::{Tag, TagSignature};
        let profile = Profile::new_icc(&adobe_rgb_icc()).unwrap();
        for (sig, want) in [
            (TagSignature::RedColorantTag, ADOBE_RGB_RED_COLORANT),
            (TagSignature::GreenColorantTag, ADOBE_RGB_GREEN_COLORANT),
            (TagSignature::BlueColorantTag, ADOBE_RGB_BLUE_COLORANT),
        ] {
            let Tag::CIEXYZ(c) = profile.read_tag(sig) else {
                panic!("missing colorant tag {sig:?}");
            };
            for (got, want) in [c.X, c.Y, c.Z].into_iter().zip(want) {
                assert!(
                    (got - want).abs() < ADOBE_RGB_COLORANT_TOLERANCE,
                    "{sig:?} colorant {got} != Adobe's {want}"
                );
            }
        }
    }

    #[test]
    fn adobe_rgb_encode_applies_only_the_563_256_power_law() {
        // As for Display P3: `fit_gamut` already moved the pixels into Adobe RGB, so the
        // encode applies the transfer and nothing else. The tolerance is tight enough to
        // tell `563/256` from the `2.2` it is often rounded to, and the near-black
        // sample tells a pure power law from a curve with a linear toe.
        let linear = [0.002, 0.002, 0.002, 0.5, 0.5, 0.5, 0.8, 0.1, 0.4];
        let image = LinearImage::new(3, 1, linear.to_vec(), Some(vec![0.1, 0.2, 0.3])).unwrap();
        let (encoded, icc) = encode_display_linear(image, DestinationGamut::AdobeRgb).unwrap();
        assert_eq!(icc, adobe_rgb_icc());
        assert_eq!(encoded.ir, Some(vec![0.1, 0.2, 0.3]));
        // Little CMS evaluates the parametric curve in float, measured within 4e-9.
        const TOLERANCE: f32 = 1e-6;
        for (got, inp) in encoded.rgb.iter().zip(linear) {
            let want = adobe_rgb_encode(inp);
            assert!(
                (got - want).abs() < TOLERANCE,
                "{got} != Adobe RGB-encoded {want} (input {inp})"
            );
            assert!(
                (want - inp.powf(1.0 / 2.2)).abs() > 10.0 * TOLERANCE,
                "input {inp} cannot tell 563/256 from 2.2"
            );
        }
    }

    #[test]
    fn adobe_rgb_profile_is_named_deterministic_and_distinct_from_display_p3() {
        let icc = adobe_rgb_icc();
        assert!(
            icc[24..36].iter().all(|&b| b == 0),
            "Adobe RGB ICC creation dateTime must be zeroed for determinism"
        );
        assert_eq!(icc, adobe_rgb_icc(), "must serialize to identical bytes");
        assert_ne!(icc, icc_profile(&OutputSpace::DisplayP3).unwrap());
        let profile = Profile::new_icc(&icc).unwrap();
        let lcms2::Tag::MLU(desc) = profile.read_tag(lcms2::TagSignature::ProfileDescriptionTag)
        else {
            panic!("missing description");
        };
        assert_eq!(
            desc.text(lcms2::Locale::new("en_US")).unwrap(),
            ADOBE_RGB_DESCRIPTION
        );
    }

    #[test]
    fn display_p3_icc_is_deterministic_with_zeroed_datetime() {
        // Same determinism contract as the other synthesized profiles: the header
        // creation dateTime is zeroed and repeated generation is byte-identical.
        let a = icc_profile(&OutputSpace::DisplayP3).unwrap();
        assert!(
            a[24..36].iter().all(|&b| b == 0),
            "Display P3 ICC creation dateTime must be zeroed for determinism"
        );
        let b = icc_profile(&OutputSpace::DisplayP3).unwrap();
        assert_eq!(a, b, "Display P3 must serialize to identical bytes");
    }
}
