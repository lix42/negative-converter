//! Working-space → output color transforms via lcms2; depth-aware default
//! profile selection and the ICC blob to embed.
//!
//! ## Working space
//! Step-1 decode produces "linear scanner RGB" with no input ICC, so the source
//! colorimetry must be pinned to build any transform. We treat the working space
//! as **Rec.709/sRGB primaries, D65 white, linear TRC**. sRGB output is then a
//! pure tone-curve application (identical primaries); wide-gamut output is a
//! clean primaries remap. The `--input-profile`/`--assume-linear` knobs
//! (`InputColor`) are parsed into config but not yet applied; once decode /
//! orchestration consumes them, any input→working conversion happens upstream of
//! this stage, so this fixed working space still holds.
//!
//! ## Output spaces
//! The tone curve is a property of the space, not the output depth, so every
//! embedded profile self-describes its data:
//! - `SRgb`     — Rec.709 / D65, sRGB curve   (display-referred)
//! - `ProPhoto` — ROMM    / D50, gamma 1.8     (display-referred)
//! - `AcesCg`   — AP1     / ~D60, linear       (scene-referred)
//! - `Custom`   — whatever the supplied ICC file declares
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
    Custom(PathBuf),
}

impl OutputSpace {
    /// Parse the `--output-profile` value: the case-insensitive keywords
    /// `srgb`/`prophoto`/`acescg`, otherwise a path to a user ICC file.
    ///
    /// Fails loudly on a bare word that is neither a known keyword nor a path
    /// (e.g. a misspelled `prophooto`) instead of deferring it to a confusing
    /// "cannot read ICC profile" later. A value that looks like a path (contains
    /// a separator or a `.`) is taken as `Custom`; the path itself is not checked
    /// here — a bad path surfaces when the profile is loaded.
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "srgb" => Ok(Self::SRgb),
            "prophoto" => Ok(Self::ProPhoto),
            "acescg" => Ok(Self::AcesCg),
            _ if trimmed.contains(['/', '\\', '.']) => Ok(Self::Custom(PathBuf::from(trimmed))),
            _ => Err(NcError::Usage(format!(
                "unknown output profile {trimmed:?}; expected srgb, prophoto, acescg, \
                 or a path to an ICC file"
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

/// The ICC bytes to embed for a given space. The `convert` pipeline obtains the
/// blob from [`to_output`] (which returns it alongside the transformed image);
/// this stays a standalone helper for building a space's ICC in isolation, used
/// by the tests here.
#[allow(dead_code)]
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
    let space = resolve_output_space(explicit, params.out_depth);

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

/// Synthesize an RGB profile from a white point, primaries `[r, g, b]` and a
/// single gamma applied to all three channels.
fn synth(white: CIExyY, primaries: [(f64, f64); 3], gamma: f64) -> Result<Profile> {
    let curve = ToneCurve::new(gamma);
    let prim = CIExyYTRIPLE {
        Red: xyy(primaries[0].0, primaries[0].1),
        Green: xyy(primaries[1].0, primaries[1].1),
        Blue: xyy(primaries[2].0, primaries[2].1),
    };
    Profile::new_rgb(&white, &prim, &[&curve, &curve, &curve])
        .map_err(|e| NcError::Other(format!("failed to build RGB profile: {e}")))
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
            out_depth: OutDepth::F32,
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
            out_depth: OutDepth::F32, // AcesCg
            ..Default::default()
        };
        let (out, _icc) = to_output(&img, &params).unwrap();
        let [r, g, b] = [out.rgb[0], out.rgb[1], out.rgb[2]];
        assert!(r < 1.0, "expected R pulled below 1.0, got {r}");
        assert!(g > 0.0 && b > 0.0, "expected G/B lifted off 0, got {g}/{b}");
    }
}
