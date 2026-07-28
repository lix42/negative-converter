//! Working-space → output color transforms via lcms2; depth-aware default
//! profile selection and the ICC blob to embed.
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
//! The tone curve is a property of the space, not the output depth, so every
//! embedded profile self-describes its data:
//! - `SRgb`      — Rec.709 / D65, sRGB curve   (display-referred)
//! - `ProPhoto`  — ROMM    / D50, gamma 1.8     (display-referred)
//! - `AcesCg`    — AP1     / ~D60, linear       (scene-referred)
//! - `DisplayP3` — P3      / D65, sRGB curve    (display-referred SDR)
//! - `Custom`    — whatever the supplied ICC file declares
//!
//! Depth-aware default: `u16 → SRgb`, `f32 → AcesCg` (linear scene-referred to
//! avoid clipping the extended range of HDR data).
//!
//! Values may leave `[0, 1]` after a gamut remap; range clamping and clipping
//! warnings are the encoder's job ("fail loudly" at encode), not this stage's.

use std::path::PathBuf;

use lcms2::{
    CIExyY, CIExyYTRIPLE, ColorSpaceSignature, Intent, PixelFormat, Profile, ToneCurve, Transform,
};

use crate::types::{LinearImage, NcError, OutDepth, OutputParams, Result};

/// The output color space to transform into and tag the file with.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputSpace {
    SRgb,
    ProPhoto,
    AcesCg,
    /// Display P3 SDR: P3 primaries, D65 encoding white, piecewise sRGB TRC.
    /// The standardized wide-gamut SDR destination (and the planned gain-map
    /// base).
    ///
    /// **Shipped behavior:** like every output space, this transforms *from* the
    /// linear Rec.709 working profile (see module docs). Little CMS
    /// colorimetrically remaps those working values to the P3 primaries — lossless,
    /// since Rec.709 ⊂ P3, so no gamut compression — and applies the sRGB TRC.
    ///
    /// **Target state (`sdr-display-rendering`):** once that task lands, its output
    /// is already-rendered **linear Display P3**, and this space's transform becomes
    /// a pure transfer-encode (identity P3→P3 primaries + sRGB TRC). The ACEScg→P3
    /// render and all SDR tone/gamut mapping belong to `sdr-display-rendering`, not
    /// here.
    DisplayP3,
    Custom(PathBuf),
}

impl OutputSpace {
    /// Parse the `--output-profile` value: the case-insensitive keywords
    /// `srgb`/`prophoto`/`acescg`/`display-p3`, otherwise a path to a user ICC
    /// file.
    ///
    /// Fails loudly on a bare word that is neither a known keyword nor a path
    /// (e.g. a misspelled `prophooto`) instead of deferring it to a confusing
    /// "cannot read ICC profile" later. A value that looks like a path (contains
    /// a separator or a `.`) is taken as `Custom`; the path itself is not checked
    /// here — a bad path surfaces when the profile is loaded. The `display-p3`
    /// keyword carries a `-` (not a path separator), so it is matched here and
    /// never mistaken for a path.
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "srgb" => Ok(Self::SRgb),
            "prophoto" => Ok(Self::ProPhoto),
            "acescg" => Ok(Self::AcesCg),
            "display-p3" | "displayp3" => Ok(Self::DisplayP3),
            _ if trimmed.contains(['/', '\\', '.']) => Ok(Self::Custom(PathBuf::from(trimmed))),
            _ => Err(NcError::Usage(format!(
                "unknown output profile {trimmed:?}; expected srgb, prophoto, acescg, \
                 display-p3, or a path to an ICC file"
            ))),
        }
    }
}

/// Resolve the effective output space from an explicit choice + output depth.
/// Explicit wins; otherwise the depth-aware default (`u16 → sRGB`,
/// `f32 → ACEScg`).
pub fn resolve_output_space(explicit: Option<OutputSpace>, depth: OutDepth) -> OutputSpace {
    explicit.unwrap_or(match depth {
        OutDepth::U16 => OutputSpace::SRgb,
        OutDepth::F32 => OutputSpace::AcesCg,
    })
}

/// The ICC bytes to embed for a given space, **without** building a transform.
///
/// The legacy path gets its blob from [`to_output`] (which returns it alongside the
/// transformed image), but the `film-master` branch has no transform to build — its
/// pixels are already linear ACEScg — so `pipeline::stages`' film-master render calls
/// this directly to fetch the tag that matches them. Byte-identical to what
/// `to_output` would embed for the same space: both route through [`profile_icc`].
pub fn icc_profile(space: &OutputSpace) -> Result<Vec<u8>> {
    profile_icc(&build_profile(space)?)
}

/// ICC header offset of the creation `dateTimeNumber` (ICC.1 §7.2, bytes 24–35).
const ICC_HEADER_DATETIME: std::ops::Range<usize> = 24..36;

/// Serialize an already-built profile to ICC bytes. Shared by `icc_profile` and
/// `to_output` so the latter doesn't rebuild (and re-read from disk) a profile
/// it already holds.
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

/// Transform `image` from the linear working space into the output profile
/// selected by `params`, returning the converted image and the ICC blob to
/// embed at encode time. The IR plane is carried through untouched.
pub fn to_output(image: &LinearImage, params: &OutputParams) -> Result<(LinearImage, Vec<u8>)> {
    let explicit = params
        .output_profile
        .as_deref()
        .map(OutputSpace::parse)
        .transpose()?;
    let space = resolve_output_space(explicit, params.depth());

    let working = working_profile()?;
    let output = build_profile(&space)?;
    let transform: Transform<[f32; 3], [f32; 3]> = Transform::new(
        &working,
        PixelFormat::RGB_FLT,
        &output,
        PixelFormat::RGB_FLT,
        Intent::RelativeColorimetric,
    )
    .map_err(|e| NcError::Other(format!("failed to build color transform: {e}")))?;

    let mut out = image.clone();
    // `rgb` is interleaved RGB with len == w*h*3 (enforced by `LinearImage::new`),
    // but the field is `pub`, so guard the invariant loudly: `as_chunks_mut`
    // silently drops a trailing 1–2 elements, which would leave the tail pixels
    // un-transformed in release — a quietly-wrong image, which "fail loudly"
    // forbids.
    let rgb_len = out.rgb.len();
    let (pixels, rest) = out.rgb.as_chunks_mut::<3>();
    if !rest.is_empty() {
        return Err(NcError::Other(format!(
            "rgb buffer length {rgb_len} is not a multiple of 3"
        )));
    }
    transform.transform_in_place(pixels);

    let icc = profile_icc(&output)?;
    Ok((out, icc))
}

// ---------------------------------------------------------------------------
// Profile construction
// ---------------------------------------------------------------------------

/// xyY chromaticity with luminance fixed at 1.0 (as used for the white point and
/// primaries passed to `Profile::new_rgb`).
fn xyy(x: f64, y: f64) -> CIExyY {
    CIExyY { x, y, Y: 1.0 }
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
    // Type 4 params [g, a, b, c, d] for the standard sRGB curve.
    ToneCurve::new_parametric(4, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
        .map_err(|e| NcError::Other(format!("failed to build sRGB tone curve: {e}")))
}

/// The linear Rec.709 / D65 working-space profile (see module docs).
fn working_profile() -> Result<Profile> {
    synth(
        xyy(0.3127, 0.3290),
        [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)],
        1.0,
    )
}

/// Build the lcms2 profile for an output space.
fn build_profile(space: &OutputSpace) -> Result<Profile> {
    match space {
        // Built-in sRGB: Rec.709 primaries, D65, sRGB TRC.
        OutputSpace::SRgb => Ok(Profile::new_srgb()),
        // ProPhoto / ROMM RGB: D50, gamma 1.8. Modeled as pure 1.8 — the small
        // ROMM linear toe near black is omitted (the common simplification).
        OutputSpace::ProPhoto => synth(
            xyy(0.3457, 0.3585),
            [(0.7347, 0.2653), (0.1596, 0.8404), (0.0366, 0.0001)],
            1.8,
        ),
        // ACEScg: AP1 primaries, ACES white (~D60), linear.
        OutputSpace::AcesCg => synth(
            xyy(0.32168, 0.33767),
            [(0.713, 0.293), (0.165, 0.830), (0.128, 0.044)],
            1.0,
        ),
        // Display P3 SDR: P3 primaries, D65 encoding white, piecewise sRGB TRC.
        // Little CMS Bradford-adapts the D65 colorants to the D50 PCS and writes
        // the `chromaticAdaptationTag`; D50 is the media white, D65 the encoding
        // white (colorants verified against the ICC-registry Display P3 reference
        // by the tests). Synthesized cross-platform — no dependency on macOS's
        // system `Display P3.icc`.
        OutputSpace::DisplayP3 => synth_curve(
            xyy(0.3127, 0.3290),
            [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)],
            &srgb_trc()?,
        ),
        OutputSpace::Custom(path) => {
            let bytes = std::fs::read(path).map_err(|e| {
                NcError::Usage(format!("cannot read ICC profile {}: {e}", path.display()))
            })?;
            let profile = Profile::new_icc(&bytes).map_err(|e| {
                NcError::Usage(format!("invalid ICC profile {}: {e}", path.display()))
            })?;
            // The working→output transform is RGB→RGB; a CMYK/Lab/gray profile
            // would otherwise fail later with an opaque transform-build error.
            let cs = profile.color_space();
            if cs != ColorSpaceSignature::RgbData {
                return Err(NcError::Usage(format!(
                    "ICC profile {} is not an RGB profile (color space {cs:?})",
                    path.display()
                )));
            }
            Ok(profile)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_image(v: f32) -> LinearImage {
        LinearImage::new(1, 1, vec![v, v, v], None).unwrap()
    }

    #[test]
    fn resolve_depth_aware_defaults() {
        assert_eq!(resolve_output_space(None, OutDepth::U16), OutputSpace::SRgb);
        assert_eq!(
            resolve_output_space(None, OutDepth::F32),
            OutputSpace::AcesCg
        );
    }

    #[test]
    fn explicit_choice_overrides_default() {
        assert_eq!(
            resolve_output_space(Some(OutputSpace::ProPhoto), OutDepth::U16),
            OutputSpace::ProPhoto
        );
        assert_eq!(
            resolve_output_space(Some(OutputSpace::SRgb), OutDepth::F32),
            OutputSpace::SRgb
        );
    }

    #[test]
    fn parse_keywords_and_path() {
        assert_eq!(OutputSpace::parse("sRGB").unwrap(), OutputSpace::SRgb);
        assert_eq!(
            OutputSpace::parse("  prophoto ").unwrap(),
            OutputSpace::ProPhoto
        );
        assert_eq!(OutputSpace::parse("ACEScg").unwrap(), OutputSpace::AcesCg);
        // Display P3 keyword (both hyphenated and joined spellings); the `-` is
        // not a path separator, so it is never mistaken for a `Custom` path.
        assert_eq!(
            OutputSpace::parse("Display-P3").unwrap(),
            OutputSpace::DisplayP3
        );
        assert_eq!(
            OutputSpace::parse(" displayp3 ").unwrap(),
            OutputSpace::DisplayP3
        );
        assert_eq!(
            OutputSpace::parse("/tmp/my.icc").unwrap(),
            OutputSpace::Custom(PathBuf::from("/tmp/my.icc"))
        );
        assert_eq!(
            OutputSpace::parse("profile.icc").unwrap(),
            OutputSpace::Custom(PathBuf::from("profile.icc"))
        );
    }

    #[test]
    fn parse_rejects_misspelled_keyword() {
        // A bare word that is neither a keyword nor path-like must fail loudly
        // (exit 2) rather than become a `Custom` path that errors confusingly.
        let err = OutputSpace::parse("prophooto").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn neutral_gray_maps_to_srgb_encoded_value() {
        // Linear 0.5 in the working space → sRGB-encoded ~0.7353.
        let params = OutputParams::default(); // u16 → sRGB
        let (out, _icc) = to_output(&gray_image(0.5), &params).unwrap();
        for &c in &out.rgb {
            assert!((c - 0.7353).abs() < 0.005, "got {c}, expected ~0.7353");
        }
    }

    #[test]
    fn srgb_round_trip_within_tolerance() {
        // working → sRGB, then sRGB → working should recover the input.
        let (encoded, _) = to_output(&gray_image(0.5), &OutputParams::default()).unwrap();
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
            OutputSpace::ProPhoto,
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
    fn custom_profile_loads_and_transforms_from_disk() {
        // Write a valid sRGB ICC, then drive the full transform through the
        // `Custom` branch (not just `icc_profile`).
        let bytes = icc_profile(&OutputSpace::SRgb).unwrap();
        let path = std::env::temp_dir().join("nc_color_test_custom.icc");
        std::fs::write(&path, &bytes).unwrap();

        let space = OutputSpace::parse(path.to_str().unwrap()).unwrap();
        assert!(matches!(space, OutputSpace::Custom(_)));

        let params = OutputParams {
            output_profile: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let (out, icc) = to_output(&gray_image(0.5), &params).unwrap();
        assert!(!icc.is_empty());
        // Custom == that sRGB profile, so 0.5 linear → ~0.7353 encoded.
        for &c in &out.rgb {
            assert!((c - 0.7353).abs() < 0.005, "got {c}, expected ~0.7353");
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_custom_profile_fails_loudly() {
        let space = OutputSpace::Custom(PathBuf::from("/nonexistent/definitely-not-here.icc"));
        let err = icc_profile(&space).unwrap_err();
        assert_eq!(
            err.exit_code(),
            2,
            "bad profile path should be a usage error"
        );
    }

    #[test]
    fn garbage_custom_profile_fails_loudly() {
        // A present-but-invalid ICC hits the parse branch (distinct from the
        // missing-file read branch) and must also be a usage error (exit 2).
        let path = std::env::temp_dir().join("nc_color_test_garbage.icc");
        std::fs::write(&path, b"not an icc profile at all").unwrap();
        let space = OutputSpace::Custom(path.clone());
        let err = icc_profile(&space).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ir_plane_is_carried_through_untouched() {
        // The IR plane must survive the color transform byte-for-byte (it is
        // preserved, not consumed, in Step 1).
        let img = LinearImage::new(1, 1, vec![0.5, 0.5, 0.5], Some(vec![0.42])).unwrap();
        let (out, _icc) = to_output(&img, &OutputParams::default()).unwrap();
        assert_eq!(out.width, 1);
        assert_eq!(out.height, 1);
        assert_eq!(out.ir, Some(vec![0.42]));
    }

    #[test]
    fn f32_default_runs_acescg_transform() {
        // The depth-aware f32 default (AcesCg, linear) must actually build a
        // transform and run, not just resolve to the right enum. AcesCg is
        // linear and wider than the working gamut, so neutral gray stays a
        // sensible near-0.5 value (no sRGB tone curve applied).
        let params = OutputParams {
            hdr: true, // → F32 → AcesCg
            ..Default::default()
        };
        let (out, icc) = to_output(&gray_image(0.5), &params).unwrap();
        assert!(!icc.is_empty());
        for &c in &out.rgb {
            assert!(
                (0.3..0.7).contains(&c),
                "AcesCg gray {c} unexpectedly far from 0.5"
            );
        }
    }

    #[test]
    fn wide_gamut_remap_moves_saturated_red() {
        // Gray hides primaries errors; a saturated color does not. Rec.709 red
        // encoded into the wider AP1 gamut must pull R below 1.0 and lift G/B
        // off 0 — this pins down the primaries/white-point, not just the TRC.
        let img = LinearImage::new(1, 1, vec![1.0, 0.0, 0.0], None).unwrap();
        let params = OutputParams {
            hdr: true, // → F32 → AcesCg
            ..Default::default()
        };
        let (out, _icc) = to_output(&img, &params).unwrap();
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
    fn linear_p3_samples_encode_with_srgb_trc_and_identity_primaries() {
        // The "linear Display P3 → encoded Display P3" encode step tested in
        // ISOLATION: feed rendered linear-P3 samples through a *synthetic
        // linear-P3 source* profile into the P3 output profile. Source and
        // destination share P3 primaries, so this transform is a pure TRC encode —
        // proving that step performs NO gamut mapping and NO ACEScg transform (that
        // is `sdr-display-rendering`'s job). This does NOT exercise the shipped
        // `to_output` path, which sources the linear Rec.709 working profile and
        // therefore *does* perform a (lossless) Rec.709→P3 primaries remap; the
        // real path is covered by `to_output_display_p3_*` below.
        let lin_p3 = synth(
            xyy(0.3127, 0.3290),
            [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)],
            1.0,
        )
        .unwrap();
        let p3 = build_profile(&OutputSpace::DisplayP3).unwrap();
        let t: Transform<[f32; 3], [f32; 3]> = Transform::new(
            &lin_p3,
            PixelFormat::RGB_FLT,
            &p3,
            PixelFormat::RGB_FLT,
            Intent::RelativeColorimetric,
        )
        .unwrap();
        // A deep shadow below the sRGB toe threshold (lin 0.002 < 0.0031308, in the
        // linear `c·X` segment → 12.92 × 0.002 = 0.02584), neutral mid-gray, a
        // saturated primary, and a mixed saturated colour. The toe sample is what
        // distinguishes the parametric curve from a gamma-2.2 power (which would
        // give ~0.081 here) — the curve's whole reason for existing.
        let mut px = [
            [0.002f32, 0.002, 0.002],
            [0.5, 0.5, 0.5],
            [1.0, 0.0, 0.0],
            [0.8, 0.1, 0.4],
        ];
        let input = px;
        t.transform_in_place(&mut px);
        for (got, inp) in px.into_iter().zip(input) {
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
        // A pure P3 red stays on the red axis: G and B remain ~0. A gamut remap or
        // an ACEScg transform would bleed energy into G/B — this asserts neither ran.
        let red = px[2];
        assert!(
            (red[0] - 1.0).abs() < 2e-3 && red[1] < 2e-3 && red[2] < 2e-3,
            "linear-P3 red must stay [~1,~0,~0], got {red:?} (gamut mapping leaked)"
        );
    }

    #[test]
    fn to_output_display_p3_remaps_rec709_and_encodes() {
        // The SHIPPED `to_output` path (not the isolation encode above): it sources
        // the linear Rec.709 working profile, so selecting Display P3 does a
        // lossless Rec.709→P3 primaries remap (Rec.709 ⊂ P3) PLUS the sRGB TRC.
        let params = OutputParams {
            output_profile: Some("display-p3".into()),
            ..Default::default()
        };

        // Neutral gray is invariant under a D65-preserving matrix, so linear 0.5 →
        // sRGB-encoded ~0.7353 pins only the TRC — necessary but not sufficient.
        let (out, _icc) = to_output(&gray_image(0.5), &params).unwrap();
        for &c in &out.rgb {
            assert!(
                (c - 0.7353).abs() < 5e-3,
                "P3 neutral got {c}, expected ~0.7353"
            );
        }

        // A saturated Rec.709 red is the assertion with teeth against a matrix /
        // extra-transform regression. Expected = sRGB-encode(Rec.709→P3 linear red)
        // using the standard linear Rec.709→Display P3 matrix (both D65, no
        // adaptation):
        //   [0.822462 0.177538 0.000000]
        //   [0.033194 0.966806 0.000000]
        //   [0.017083 0.072397 0.910520]
        // Red column → linear P3 (0.822462, 0.033194, 0.017083); encode each.
        let lin_p3_red = [0.822462_f32, 0.033194, 0.017083];
        let expect = lin_p3_red.map(srgb_encode); // ≈ (0.9175, 0.2004, 0.1385)
        let img = LinearImage::new(1, 1, vec![1.0, 0.0, 0.0], None).unwrap();
        let (out, _icc) = to_output(&img, &params).unwrap();
        let got = [out.rgb[0], out.rgb[1], out.rgb[2]];
        for ch in 0..3 {
            assert!(
                (got[ch] - expect[ch]).abs() < 5e-3,
                "channel {ch}: to_output got {} != expected {} (Rec.709→P3 remap + sRGB)",
                got[ch],
                expect[ch]
            );
        }
        // Teeth: unlike the isolation test (identity P3 primaries → red stays
        // [1,0,0]), the shipped Rec.709→P3 remap lifts G and B off zero and keeps R
        // dominant. An accidental extra transform or a wrong matrix moves these.
        assert!(
            got[1] > 0.0 && got[2] > 0.0,
            "Rec.709→P3 remap must lift G/B off 0, got {}/{}",
            got[1],
            got[2]
        );
        assert!(
            got[0] > got[1] && got[0] > got[2],
            "remapped red must stay dominant, got {got:?}"
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

    #[test]
    fn display_p3_end_to_end_embeds_p3_icc() {
        // Selecting `display-p3` via the recipe/CLI string surface drives the full
        // `to_output` path and embeds the generated P3 ICC (byte-identical to the
        // standalone `icc_profile`), so the encoder tags the file correctly.
        let params = OutputParams {
            output_profile: Some("display-p3".into()),
            ..Default::default()
        };
        let (_out, icc) = to_output(&gray_image(0.5), &params).unwrap();
        assert_eq!(
            icc,
            icc_profile(&OutputSpace::DisplayP3).unwrap(),
            "embedded blob must be the generated Display P3 profile"
        );
    }
}
