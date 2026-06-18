//! Shared core types — the neutral contract between pipeline stages.
//!
//! This module is pure data: no I/O, and no crate-specific image types
//! (conversions to/from `image`/`tiff` live in the `io` stages). Every stage
//! takes `(input, params) -> output`; these are the `input`/`output` and the
//! `params`. Param structs mirror the CLI/recipe keys in design-spec §9 so a
//! recipe JSON round-trips to exactly the knobs the pipeline reads.

use serde::{Deserialize, Serialize};

/// Linear scanner image in `f32`, interleaved RGB plus optional IR plane.
///
/// Values are in a linear working space, range ~`[0, 1]`. `rgb` is interleaved
/// (`r,g,b, r,g,b, …`) with `len == width * height * 3`. The IR plane, when
/// present (HDRi input), is `len == width * height` and is **carried through but
/// not consumed** in Step 1 (design-spec §6.1).
#[derive(Clone, Debug)]
pub struct LinearImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<f32>,
    pub ir: Option<Vec<f32>>,
}

impl LinearImage {
    /// Validated constructor — the single entry point `io::decode` should use to
    /// build an image, so the buffer-length invariants (`rgb.len() == w*h*3`,
    /// `ir.len() == w*h`) are checked once at the boundary instead of surfacing
    /// as a panic deep in the pipeline. Fields stay `pub` for stage ergonomics.
    pub fn new(width: u32, height: u32, rgb: Vec<f32>, ir: Option<Vec<f32>>) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(NcError::Other(format!(
                "image dimensions must be non-zero (got {width}x{height})"
            )));
        }
        // Checked arithmetic: a hostile/corrupt header advertising huge
        // dimensions must surface as an error, not a debug panic / release wrap.
        let overflow = || {
            NcError::Other(format!(
                "image dimensions {width}x{height} overflow address space"
            ))
        };
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(overflow)?;
        let rgb_len = pixels.checked_mul(3).ok_or_else(overflow)?;
        if rgb.len() != rgb_len {
            return Err(NcError::Other(format!(
                "rgb buffer length {} != width*height*3 ({rgb_len})",
                rgb.len()
            )));
        }
        if let Some(ir_plane) = &ir {
            let ir_len = ir_plane.len();
            if ir_len != pixels {
                return Err(NcError::Other(format!(
                    "ir buffer length {ir_len} != width*height ({pixels})"
                )));
            }
        }
        Ok(Self {
            width,
            height,
            rgb,
            ir,
        })
    }
}

/// Per-channel unexposed-film base transmission — the `Dmin` anchor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilmBase {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

// The recipe/CLI carries the film base as an `[r, g, b]` array (mirroring the
// `--film-base R,G,B` flag), while the pipeline prefers the named `FilmBase`.
// Keep that one conversion here so the two representations can't drift.
impl From<[f32; 3]> for FilmBase {
    fn from([r, g, b]: [f32; 3]) -> Self {
        Self { r, g, b }
    }
}

impl From<FilmBase> for [f32; 3] {
    fn from(b: FilmBase) -> Self {
        [b.r, b.g, b.b]
    }
}

/// Negative→positive algorithm selector (design-spec §9, `--algorithm`).
///
/// A neutral selector that mirrors the CLI/recipe key, like the param structs —
/// it does not depend on the `algo` implementations. Serializes lowercase
/// (`"simple"` / `"density"`) and parses the same on the CLI via `ValueEnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    /// Channel-inversion baseline (debug / B&W).
    Simple,
    /// Density-domain inversion (Cineon / negadoctor) — the default.
    #[default]
    Density,
}

/// Output bit depth selector. Serializes as `"u16"` / `"f32"` to match the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OutDepth {
    #[default]
    U16,
    F32,
}

/// BigTIFF promotion policy for the encoder. Serializes lowercase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BigTiff {
    /// Promote to BigTIFF only when the output would exceed the classic limit.
    #[default]
    Auto,
    On,
    Off,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Top-level error type for the whole tool. Each variant maps to a stable exit
/// code (design-spec §11) via [`NcError::exit_code`].
#[derive(Clone, Debug)]
pub enum NcError {
    /// Invalid CLI usage or parameters. Exit 2.
    Usage(String),
    /// Input read/decode error (unreadable or unsupported file). Exit 3.
    Decode(String),
    /// Unsupported variant (e.g. a channel layout we can't handle yet). Exit 4.
    Unsupported(String),
    /// Output write error. Exit 5.
    Write(String),
    /// Generic / unexpected error. Exit 1.
    Other(String),
}

impl NcError {
    /// Stable process exit code for this error (design-spec §11). Kept here so
    /// `cli` and `pipeline` map errors to codes in exactly one place.
    pub fn exit_code(&self) -> i32 {
        match self {
            NcError::Other(_) => 1,
            NcError::Usage(_) => 2,
            NcError::Decode(_) => 3,
            NcError::Unsupported(_) => 4,
            NcError::Write(_) => 5,
        }
    }
}

impl std::fmt::Display for NcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, msg) = match self {
            NcError::Usage(m) => ("usage", m),
            NcError::Decode(m) => ("decode", m),
            NcError::Unsupported(m) => ("unsupported", m),
            NcError::Write(m) => ("write", m),
            NcError::Other(m) => ("error", m),
        };
        write!(f, "{kind}: {msg}")
    }
}

impl std::error::Error for NcError {}

/// Convenience alias for fallible operations across the tool.
pub type Result<T> = std::result::Result<T, NcError>;

// ---------------------------------------------------------------------------
// Stage parameter structs (one per stage; CLI/recipe keys, design-spec §9)
// ---------------------------------------------------------------------------
//
// Downstream tasks fill in the behavior; these establish the stable shape and
// serde key names. Defaults are deliberately neutral (identity-ish) placeholders
// — the algorithm tasks refine them.

/// How the input's color is interpreted on decode (design-spec §9, Input/decode).
///
/// A single mutually-exclusive choice, not independent flags: the input is taken
/// as already linear, interpreted through an explicit ICC profile, or (default)
/// decoded with the file's embedded / default profile. Serializes as `"auto"` /
/// `"linear"` / `{ "profile": "<icc>" }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InputColor {
    /// Decode with the file's embedded profile, or the documented fallback when
    /// none is present. This is what passing no input-color flag does.
    #[default]
    Auto,
    /// Treat the scanner data as already linear; apply no input transfer curve.
    Linear,
    /// Interpret the input through this ICC profile selector / path.
    Profile(String),
}

/// Input / decode knobs (design-spec §9, stage 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct InputParams {
    /// How to interpret the input's color (default `auto`).
    pub color: InputColor,
    /// Write the decoded IR plane to this path (HDRi only); `None` skips export.
    /// An input/decode-domain artifact (design-spec §9, Input/decode) — carried
    /// here so `pipeline-orchestration` can drive the IR exporter.
    pub export_ir: Option<String>,
}

/// Where the film base comes from (design-spec §9, stage 2).
///
/// A single mutually-exclusive choice, not independent flags: more-specific
/// sources always win with no fallback, so this is one selection. Serializes as
/// `"auto"` / `{ "region": [x, y, w, h] }` / `{ "explicit": [r, g, b] }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FilmBaseSource {
    /// Estimate the base from the detected unexposed border.
    #[default]
    Auto,
    /// Sample the base from this border region `[x, y, w, h]`.
    Region([u32; 4]),
    /// Explicit per-channel base transmission `[r, g, b]`.
    Explicit([f32; 3]),
}

/// Film-base / `Dmin` estimation knobs (design-spec §9, stage 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FilmBaseParams {
    /// Where the film base comes from (default `auto`).
    pub source: FilmBaseSource,
}

/// Density-domain algorithm knobs (design-spec §9, `algorithm = density`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DensityParams {
    /// Per-channel density gain `[r, g, b]`.
    pub density_scale: [f32; 3],
    /// Per-channel density offset `[r, g, b]` (orange-mask compensation).
    pub density_offset: [f32; 3],
    /// Film/print curve gamma.
    pub density_gamma: f32,
}

impl Default for DensityParams {
    fn default() -> Self {
        Self {
            density_scale: [1.0, 1.0, 1.0],
            density_offset: [0.0, 0.0, 0.0],
            density_gamma: 1.0,
        }
    }
}

/// Print / tone-render knobs (design-spec §9). A **separate** sub-stage from
/// density conversion — the core fidelity rule; don't collapse the two.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrintParams {
    /// Overall positive exposure, in stops (EV); `0.0` is neutral (design-spec §9).
    pub print_exposure: f32,
    /// Paper black / shadow floor.
    pub black_point: f32,
    /// Highlight/neutral white-balance gains `[r, g, b]`.
    pub white_balance: [f32; 3],
    /// Highlight roll-off amount.
    pub highlight_compress: f32,
}

impl Default for PrintParams {
    fn default() -> Self {
        Self {
            print_exposure: 0.0,
            black_point: 0.0,
            white_balance: [1.0, 1.0, 1.0],
            highlight_compress: 0.0,
        }
    }
}

/// Simple inversion-baseline knobs (design-spec §9, `algorithm = simple`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SimpleParams {
    /// White-balance gains applied to the inverted result `[r, g, b]`.
    pub invert_white_balance: [f32; 3],
    /// Low clip point.
    pub clip_low: f32,
    /// High clip point.
    pub clip_high: f32,
}

impl Default for SimpleParams {
    fn default() -> Self {
        Self {
            invert_white_balance: [1.0, 1.0, 1.0],
            clip_low: 0.0,
            clip_high: 1.0,
        }
    }
}

/// Output / encode knobs (design-spec §9, stage 5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct OutputParams {
    /// Output bit depth (default `u16`).
    pub out_depth: OutDepth,
    /// Output ICC profile selector (`sRGB`/`prophoto`/`acescg`/path). `None`
    /// means the depth-aware default (sRGB for u16, wide-gamut for f32).
    pub output_profile: Option<String>,
    /// BigTIFF promotion policy (default `auto`).
    pub bigtiff: BigTiff,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nc_error_exit_codes() {
        assert_eq!(NcError::Other(String::new()).exit_code(), 1);
        assert_eq!(NcError::Usage(String::new()).exit_code(), 2);
        assert_eq!(NcError::Decode(String::new()).exit_code(), 3);
        assert_eq!(NcError::Unsupported(String::new()).exit_code(), 4);
        assert_eq!(NcError::Write(String::new()).exit_code(), 5);
    }

    #[test]
    fn linear_image_new_checks_buffer_lengths() {
        // 2x1 RGB needs 6 floats; IR needs 2.
        assert!(LinearImage::new(2, 1, vec![0.0; 6], Some(vec![0.0; 2])).is_ok());
        assert!(LinearImage::new(2, 1, vec![0.0; 6], None).is_ok());
        // Wrong rgb length and wrong ir length both fail loudly.
        assert!(LinearImage::new(2, 1, vec![0.0; 5], None).is_err());
        assert!(LinearImage::new(2, 1, vec![0.0; 6], Some(vec![0.0; 3])).is_err());
        // Zero dimensions are rejected, not silently accepted as an empty image.
        assert!(LinearImage::new(0, 1, vec![], None).is_err());
        assert!(LinearImage::new(2, 0, vec![], None).is_err());
        // A pathological size that overflows is an error, not a panic.
        assert!(LinearImage::new(u32::MAX, u32::MAX, vec![0.0; 1], None).is_err());
    }

    #[test]
    fn film_base_array_round_trip() {
        let base = FilmBase::from([0.9, 0.5, 0.4]);
        assert_eq!(
            base,
            FilmBase {
                r: 0.9,
                g: 0.5,
                b: 0.4
            }
        );
        assert_eq!(<[f32; 3]>::from(base), [0.9, 0.5, 0.4]);
    }

    #[test]
    fn density_params_json_round_trip() {
        let params = DensityParams {
            density_scale: [1.2, 1.0, 0.8],
            density_offset: [0.1, 0.0, -0.05],
            density_gamma: 0.6,
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: DensityParams = serde_json::from_str(&json).unwrap();
        assert_eq!(params, back);
    }

    #[test]
    fn out_depth_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&OutDepth::U16).unwrap(), "\"u16\"");
        assert_eq!(serde_json::to_string(&OutDepth::F32).unwrap(), "\"f32\"");
    }

    #[test]
    fn partial_recipe_fills_defaults() {
        // A recipe that sets only one knob should leave the rest at defaults.
        let params: PrintParams = serde_json::from_str(r#"{"print_exposure": 2.0}"#).unwrap();
        assert_eq!(params.print_exposure, 2.0);
        assert_eq!(params.white_balance, [1.0, 1.0, 1.0]);
    }
}
