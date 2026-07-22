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
/// (`"simple"` / `"density"` / `"sigmoid"`) and parses the same on the CLI via
/// `ValueEnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    /// Channel-inversion baseline (debug / B&W).
    Simple,
    /// Density-domain inversion (Cineon / negadoctor) — the default.
    #[default]
    Density,
    /// Density-domain S-curve (photographic H&D / paper-response) tone mapping.
    Sigmoid,
}

/// Output bit depth — an **internal** selector the encoder and the depth-aware
/// profile default branch on. Not part of the CLI/recipe surface (no serde/clap
/// derives on purpose): the user-facing knob is the `output.hdr` bool /
/// `--output-hdr` flag, and [`OutputParams::depth`] is the single place it
/// becomes a depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
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

/// Transfer-encoding assertion for the input (design-spec §9, `input.transfer`).
///
/// One of the two **independent** input axes (the other is [`MeaningAssertion`]).
/// It asserts only how the samples are *encoded*, never what they *measure*:
/// `Linear` says the transfer is linear (no inverse-transfer decoding needed),
/// which does not by itself prove scanner-device provenance. `Auto` (default)
/// lets the input semantic resolver (`pipeline::input_semantics`) decide from
/// container evidence, failing loudly in `convert` when it stays ambiguous.
/// Serializes kebab-case (`"auto"` / `"linear"`; kebab-case matches its mirror
/// [`MeaningAssertion`] and `TransferDescription`, so a future multi-word variant
/// stays consistent); parsed the same on the CLI via `ValueEnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TransferAssertion {
    /// Resolve the transfer from container evidence (structural raw-mode, a
    /// descriptive gamma tag).
    #[default]
    Auto,
    /// Assert a supported linear transfer. Overrides a contradicting descriptive
    /// gamma tag (recorded as displaced evidence); it cannot override container
    /// structure that proves a non-linear encoding.
    Linear,
}

/// Measurement-meaning assertion for the input (design-spec §9, `input.meaning`).
///
/// The second independent input axis: what the pixel values *are*. Only
/// [`ScannerDevice`](Self::ScannerDevice) measurements paired with a supported
/// linear transfer enter Dmin/density without a source→working color transform.
/// [`Colorimetric`](Self::Colorimetric) is recognized but unsupported (no inverse
/// transfer/reconstruction path exists yet). `Auto` (default) resolves from
/// container evidence — an embedded ICC alone does not establish it. Serializes
/// kebab-case (`"auto"` / `"scanner-device"` / `"colorimetric"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum MeaningAssertion {
    /// Resolve the meaning from container evidence.
    #[default]
    Auto,
    /// Assert scanner-device measurements (the supported meaning).
    ScannerDevice,
    /// Assert colorimetric RGB. Recognized but unsupported; `convert` rejects it
    /// even when asserted (an override cannot make it supported).
    Colorimetric,
}

/// Descriptive transfer/gamma evidence parsed from container metadata, with a
/// **third state** the resolver needs: a gamma tag that is *present but
/// uninterpretable* is ambiguous, **not** absent. Collapsing malformed → absent
/// would let a raw scan whose gamma is actually non-linear but written unparseably
/// (e.g. a German-locale `"2,2"` — LaserSoft is German software) silently resolve
/// to linear and skip the contradiction path. Lives here (not in
/// `pipeline::input_semantics`) so both `io::decode` (which produces it) and the
/// resolver (which consumes it) can share it without an io→pipeline dependency.
#[derive(Clone, Debug, PartialEq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GammaFact {
    /// No gamma tag present in the metadata.
    #[default]
    Absent,
    /// A gamma tag parsed to this numeric value.
    Value(f64),
    /// A gamma tag was present but could not be interpreted as a number (carries
    /// the offending raw string for the diagnostic). Ambiguous, never linear.
    Malformed(String),
}

/// Input / decode knobs (design-spec §9, stage 1).
///
/// Transfer and meaning are **two independent axes** (not a single combined
/// `input.color` choice, which conflated them): the resolver
/// (`pipeline::input_semantics`) resolves each from separate evidence. There is
/// deliberately no `input.color` field — the old combined key is rejected with a
/// migration error at recipe load (see `cli::load_recipe`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct InputParams {
    /// Transfer-encoding assertion (default `auto`).
    pub transfer: TransferAssertion,
    /// Measurement-meaning assertion (default `auto`).
    pub meaning: MeaningAssertion,
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
///
/// The acquisition-ladder tier 3 **content-based source**
/// (`film_base.source = "content"` / `--base-content`) is owned by the separate
/// `film-base-content-fallback` task and is deliberately **not** a variant here —
/// the auto detector only *suggests* it on refusal, never falls back to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FilmBaseSource {
    /// Estimate the base from the detected unexposed rebate band behind the
    /// film holder (the inward-scan detector; fails loudly on low confidence).
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

/// Where the density render's display-white anchor (`Dmax`) comes from
/// (design-spec §7.2/§9, `density.dmax`).
///
/// A single mutually-exclusive choice, like [`FilmBaseSource`] — not independent
/// flags. `Dmax` is the corrected density that the render maps to display white
/// (`1.0`) so the default u16 encode fills the display range instead of leaving
/// all detail above `1.0`.
///
/// Like `Dmin`, `Dmax` is a **roll-fixed calibration** (a property of the film
/// stock + development + scanner), so the default is a *fixed* anchor reused
/// across the roll — not a per-frame measurement. The `dmax-reference` task
/// (design-spec §7.2/§12) established this: anchoring each frame's densest pixel
/// to display white is per-frame *exposure normalization* (it brightens
/// underexposed frames and forces an overcast grey to white), which conflicts
/// with NC's "convert faithfully, grade in Lightroom" purpose. The roll-fixed
/// anchor is resolved reference → per-stock constant → nominal: a value measured
/// once from a fully-exposed reference frame (or a known per-stock constant) is
/// carried here as [`Explicit`](Self::Explicit); with no calibration the default
/// [`Fixed`](Self::Fixed) nominal anchor applies. Serializes as `"fixed"` /
/// `{ "explicit": <d> }` / `"auto"` / `"none"`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DmaxSource {
    /// Fixed nominal anchor: a scene-independent corrected-density placement
    /// (`algo::density::NOMINAL_DMAX`), reused across every frame. The default
    /// when none of the `--d-max` / `--fixed-d-max` / `--auto-d-max` /
    /// `--no-d-max` flags is given — the roll-fixed behavior when no reference /
    /// per-stock value has been measured. Because it is a constant, darker frames
    /// render darker (faithful relative exposure), unlike the per-frame `Auto`.
    #[default]
    Fixed,
    /// Explicit scalar anchor density — the roll-fixed calibration value. Carries
    /// a `Dmax` measured once from a fully-exposed reference frame
    /// (`estimate --d-max-region`) or a known per-stock constant, reused across
    /// the roll exactly like an explicit `Dmin` base. Frozen into a roll recipe as
    /// `density.dmax = { "explicit": <d> }`.
    Explicit(f32),
    /// Measure the anchor per frame from the corrected-density distribution
    /// (a high percentile). **Per-frame exposure normalization** — an explicit
    /// opt-in (`--auto-d-max`), *demoted* from the former default: it silently
    /// brightens underexposed frames and breaks roll-to-roll consistency, so it
    /// is a grading convenience, not the faithful-conversion default.
    Auto,
    /// No anchor: scene-referred output (base → `1.0`, exposed detail above it).
    /// Reproduces the pre-anchor render bit-for-bit — HDR f32 workflows rely on it.
    None,
}

/// Where the regional (shadow/highlight) balance's tone-ramp anchors come from
/// (design-spec §7.2/§9, `density.balance_range`).
///
/// A single mutually-exclusive choice, like [`DmaxSource`] — not independent
/// flags. The ramps span the corrected-density range `[lo, hi]`: `lo` is the
/// positive's deepest shadow tone, `hi` its brightest highlight tone. `Auto`
/// (default) measures the range per frame from the pre-regional corrected
/// densities (robust percentiles of the per-pixel scalar tone) and reports the
/// measured `[lo, hi]`; `Explicit` fixes it. Roll reuse is measure-once-replay:
/// run one frame under `Auto`, read its reported range, then pass it as
/// `Explicit` on the rest for deterministic, frame-independent toning.
/// Serializes as `"auto"` / `{ "explicit": [lo, hi] }`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BalanceRange {
    /// Measure `[lo, hi]` per frame from the corrected-density distribution.
    #[default]
    Auto,
    /// Explicit `[lo, hi]` corrected-density anchors (e.g. a reused measured
    /// range for roll consistency). Requires `lo < hi`, both finite.
    Explicit([f32; 2]),
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
    /// Display-white anchor source (default `fixed`). Applied in the render
    /// sub-stage at the density→linear boundary, beside `density_gamma`.
    pub dmax: DmaxSource,
    /// Per-channel density offset `[r, g, b]` applied to the **positive's
    /// shadows** (low scalar tone density; the region near the film base).
    /// A positive value brightens that channel there — pushes the region toward
    /// that channel's color. `[0, 0, 0]` (default) is identity/off.
    pub shadow_balance: [f32; 3],
    /// Per-channel density offset `[r, g, b]` applied to the **positive's
    /// highlights** (high scalar tone density; the dense negative areas).
    /// Same sign convention as `shadow_balance`. `[0, 0, 0]` (default) is off.
    pub highlight_balance: [f32; 3],
    /// Tone-ramp anchor source for the regional balance (default `auto`).
    /// Only consulted when a balance is non-zero — the neutral default skips
    /// the regional pass entirely (bit-exact with the unbalanced output).
    pub balance_range: BalanceRange,
}

impl Default for DensityParams {
    fn default() -> Self {
        Self {
            density_scale: [1.0, 1.0, 1.0],
            density_offset: [0.0, 0.0, 0.0],
            density_gamma: 1.0,
            dmax: DmaxSource::Fixed,
            shadow_balance: [0.0, 0.0, 0.0],
            highlight_balance: [0.0, 0.0, 0.0],
            balance_range: BalanceRange::Auto,
        }
    }
}

/// Where the print white-balance gains come from (design-spec §9,
/// `print.white_balance`).
///
/// A single mutually-exclusive choice, like [`FilmBaseSource`] / [`DmaxSource`] —
/// not parallel fields. Modeling the source as **one enum** is what makes the
/// precedence rule sound: an explicit `--white-balance 1,1,1` replaces a recipe's
/// auto mode *by source*, because the variant itself records where the gains came
/// from (explicit vs auto *provenance*), so precedence is decided by source, not
/// by value — a separate bool/Option pair would carry the value but not that
/// provenance. Serializes as
/// `{ "explicit": [r, g, b] }` / `"gray-world"` / `"percentile"`.
///
/// The auto modes are **deterministic statistics** over the rendered positive
/// (no ML, per the project's "AI-friendly ≠ ML" rule): same input + params ⇒
/// identical gains. The resolved gains ride into the convert JSON report so a
/// roll can freeze one frame's estimate into a recipe (measure once, reuse).
///
/// **Wire compatibility:** it *writes* the tagged form above, but its custom
/// [`Deserialize`] also accepts a legacy **bare `[r, g, b]` array**
/// (`"white_balance": [1, 1, 1]`) as `Explicit` gains — before this feature
/// `print.white_balance` was a plain `[f32; 3]`, so recipes/sidecars written by
/// older `nc` must still parse (reproducibility). See design-spec §9.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WbSource {
    /// Fixed per-channel gains `[r, g, b]`. The default (`[1, 1, 1]` = neutral,
    /// i.e. auto white balance off).
    Explicit([f32; 3]),
    /// Gray-world estimate (≈ NLP Auto-AVG): equalize the trimmed per-channel
    /// means. Simple, but a dominant scene color (a green lawn, a red wall)
    /// biases it — the whole frame is assumed to average to neutral.
    GrayWorld,
    /// Neutral-percentile estimate (≈ NLP Auto-Neutral): equalize the channels
    /// at a matched high percentile (near-white). More robust to dominant
    /// colors than gray-world — highlights are where neutrality matters most.
    Percentile,
}

impl Default for WbSource {
    fn default() -> Self {
        WbSource::Explicit([1.0, 1.0, 1.0])
    }
}

impl<'de> Deserialize<'de> for WbSource {
    /// Accepts both the current tagged form (`{ "explicit": [r, g, b] }` /
    /// `"gray-world"` / `"percentile"`) and the legacy **bare `[r, g, b]`** array
    /// that pre-`WbSource` recipes/sidecars wrote (when `print.white_balance` was
    /// a plain `[f32; 3]`), mapping the bare array to `Explicit`. Keeps old
    /// recipes reproducible; `Serialize` still emits only the tagged form.
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // A tagged copy of the variants (the derived shape), plus an untagged
        // wrapper that tries the bare array first, then the tagged form.
        #[derive(Deserialize)]
        #[serde(rename_all = "kebab-case")]
        enum Tagged {
            Explicit([f32; 3]),
            GrayWorld,
            Percentile,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bare([f32; 3]),
            Tagged(Tagged),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Bare(gains) | Repr::Tagged(Tagged::Explicit(gains)) => WbSource::Explicit(gains),
            Repr::Tagged(Tagged::GrayWorld) => WbSource::GrayWorld,
            Repr::Tagged(Tagged::Percentile) => WbSource::Percentile,
        })
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
    /// Highlight/neutral white-balance gain source (default explicit `[1, 1, 1]`
    /// = neutral). Auto modes estimate the gains per frame; see [`WbSource`].
    pub white_balance: WbSource,
    /// Highlight roll-off amount.
    pub highlight_compress: f32,
}

impl Default for PrintParams {
    fn default() -> Self {
        Self {
            print_exposure: 0.0,
            black_point: 0.0,
            white_balance: WbSource::default(),
            highlight_compress: 0.0,
        }
    }
}

/// Sigmoid / H&D-curve algorithm knobs (design-spec §7.3/§9,
/// `algorithm = sigmoid`).
///
/// The sigmoid algorithm shares stages 1–2 (and [`DensityParams`]'s
/// `density_scale` / `density_offset` / `dmax`) and stage 4 ([`PrintParams`])
/// with `density`; these knobs parameterize only its replacement stage 3, the
/// S-curve mapping corrected density to positive linear. `density_gamma` is the
/// straight-line curve's contrast and is **ignored** under `sigmoid` —
/// `contrast` here is the analogous knob.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SigmoidParams {
    /// Mid-density slope of the curve in log-output space (the `density_gamma`
    /// analogue). Must be finite and > 0.
    pub contrast: f32,
    /// Toe (shadow) knee width in log10 density units: how softly the curve
    /// approaches paper black. `0` disables the toe (hard straight-line black).
    pub toe: f32,
    /// Shoulder (highlight) knee width in log10 density units: how softly the
    /// curve approaches display white. `0` disables the shoulder.
    pub shoulder: f32,
}

impl Default for SigmoidParams {
    fn default() -> Self {
        Self {
            contrast: 1.0,
            toe: 0.2,
            shoulder: 0.2,
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

/// What the encode stage observed while writing — fed into the JSON report by
/// the orchestrator. Records two kinds of trouble the output samples can carry,
/// since `pipeline::color::to_output` does not clamp and the density-domain
/// algorithm can produce non-finite values from log/division math:
///
/// - **clipping** (`clipped_low`/`clipped_high`): finite samples outside `[0, 1]`
///   clamped into range. Only the u16 path clamps, so these are u16-only.
/// - **non-finite** (`non_finite`): `NaN`/`±inf` samples — a pipeline numerical
///   fault. Counted for *both* depths (u16 forces them to 0; f32 writes them
///   verbatim), so the fault surfaces regardless of output depth.
///
/// This rides back on the value path rather than down `Result` because it is a
/// quality warning, not a write failure (`--strict` can promote it to an error).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[must_use]
pub struct EncodeReport {
    /// Samples examined (`width * height * channels`). The denominator that makes
    /// the clip / non-finite counts interpretable as a fraction.
    pub total_samples: u64,
    /// Finite samples below 0.0 clamped up to 0 (u16 output only).
    pub clipped_low: u64,
    /// Finite samples above 1.0 clamped down to 65535 (u16 output only).
    pub clipped_high: u64,
    /// Non-finite (`NaN`/`±inf`) samples. Counted separately because they signal
    /// a numerical fault rather than mere out-of-gamut clipping.
    pub non_finite: u64,
}

impl EncodeReport {
    /// Total finite samples clamped at a range end (excludes non-finite).
    pub fn clipped_total(&self) -> u64 {
        self.clipped_low + self.clipped_high
    }

    /// Whether any sample is problematic — clamped at a range end or non-finite.
    /// The condition a normal run surfaces as a warning and `--strict` promotes
    /// to an error.
    pub fn any_loss(&self) -> bool {
        self.clipped_total() > 0 || self.non_finite > 0
    }

    /// Fraction of examined samples that were clipped or non-finite, in `[0, 1]`.
    /// Returns 0.0 when no samples were examined.
    pub fn loss_fraction(&self) -> f64 {
        if self.total_samples == 0 {
            0.0
        } else {
            (self.clipped_total() + self.non_finite) as f64 / self.total_samples as f64
        }
    }
}

/// Output / encode knobs (design-spec §9, stage 5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct OutputParams {
    /// HDR output switch (default `false`): `false` → 16-bit integer TIFF,
    /// `true` → 32-bit float TIFF (full HDR, no precision loss).
    pub hdr: bool,
    /// Output ICC profile selector (`sRGB`/`prophoto`/`acescg`/path). `None`
    /// means the depth-aware default (sRGB for the 16-bit default, wide-gamut
    /// linear for `hdr`).
    pub output_profile: Option<String>,
    /// BigTIFF promotion policy (default `auto`).
    pub bigtiff: BigTiff,
}

impl OutputParams {
    /// The encoder bit depth implied by the HDR switch: `hdr = false` →
    /// [`OutDepth::U16`], `true` → [`OutDepth::F32`]. The single place the
    /// recipe bool becomes a depth, so encode and color can't disagree.
    pub fn depth(&self) -> OutDepth {
        if self.hdr {
            OutDepth::F32
        } else {
            OutDepth::U16
        }
    }
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
            dmax: DmaxSource::Explicit(1.8),
            shadow_balance: [0.05, 0.0, -0.02],
            highlight_balance: [-0.05, 0.01, 0.0],
            balance_range: BalanceRange::Explicit([0.25, 1.75]),
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: DensityParams = serde_json::from_str(&json).unwrap();
        assert_eq!(params, back);
    }

    #[test]
    fn dmax_source_serializes_like_film_base_source() {
        // Unit variants are bare lowercase strings; the newtype variant is a
        // tagged object — the same shape convention as `FilmBaseSource`.
        assert_eq!(
            serde_json::to_string(&DmaxSource::Fixed).unwrap(),
            "\"fixed\""
        );
        assert_eq!(
            serde_json::to_string(&DmaxSource::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&DmaxSource::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&DmaxSource::Explicit(1.5)).unwrap(),
            r#"{"explicit":1.5}"#
        );
        for src in [
            DmaxSource::Fixed,
            DmaxSource::Auto,
            DmaxSource::None,
            DmaxSource::Explicit(2.25),
        ] {
            let json = serde_json::to_string(&src).unwrap();
            assert_eq!(serde_json::from_str::<DmaxSource>(&json).unwrap(), src);
        }
    }

    #[test]
    fn film_base_source_serializes_all_variants() {
        // Unit variants are bare lowercase strings; data variants are tagged
        // objects.
        assert_eq!(
            serde_json::to_string(&FilmBaseSource::Auto).unwrap(),
            "\"auto\""
        );
        for src in [
            FilmBaseSource::Auto,
            FilmBaseSource::Region([1, 2, 3, 4]),
            FilmBaseSource::Explicit([0.9, 0.5, 0.4]),
        ] {
            let json = serde_json::to_string(&src).unwrap();
            assert_eq!(serde_json::from_str::<FilmBaseSource>(&json).unwrap(), src);
        }
    }

    #[test]
    fn density_params_default_dmax_is_fixed() {
        // The default anchor is the roll-fixed nominal `Fixed`, not the demoted
        // per-frame `Auto` (dmax-reference): the faithful-conversion default must
        // not normalize exposure per frame.
        assert_eq!(DensityParams::default().dmax, DmaxSource::Fixed);
    }

    #[test]
    fn output_hdr_bool_drives_depth() {
        assert_eq!(OutputParams::default().depth(), OutDepth::U16);
        let hdr = OutputParams {
            hdr: true,
            ..OutputParams::default()
        };
        assert_eq!(hdr.depth(), OutDepth::F32);
    }

    #[test]
    fn sigmoid_params_json_round_trip_and_partial_defaults() {
        let params = SigmoidParams {
            contrast: 1.3,
            toe: 0.1,
            shoulder: 0.4,
        };
        let json = serde_json::to_string(&params).unwrap();
        assert_eq!(
            serde_json::from_str::<SigmoidParams>(&json).unwrap(),
            params
        );
        // A partial section fills the remaining defaults.
        let p: SigmoidParams = serde_json::from_str(r#"{"contrast":2.0}"#).unwrap();
        assert_eq!(p.contrast, 2.0);
        assert_eq!(p.toe, SigmoidParams::default().toe);
        assert_eq!(p.shoulder, SigmoidParams::default().shoulder);
    }

    #[test]
    fn algorithm_serializes_sigmoid_lowercase() {
        assert_eq!(
            serde_json::to_string(&Algorithm::Sigmoid).unwrap(),
            "\"sigmoid\""
        );
        assert_eq!(
            serde_json::from_str::<Algorithm>("\"sigmoid\"").unwrap(),
            Algorithm::Sigmoid
        );
    }

    #[test]
    fn density_params_default_regional_balance_is_neutral() {
        // The identity defaults the bit-exact-default guarantee rests on.
        let d = DensityParams::default();
        assert_eq!(d.shadow_balance, [0.0, 0.0, 0.0]);
        assert_eq!(d.highlight_balance, [0.0, 0.0, 0.0]);
        assert_eq!(d.balance_range, BalanceRange::Auto);
    }

    #[test]
    fn balance_range_serializes_like_dmax_source() {
        // Unit variant is a bare lowercase string; the newtype variant is a
        // tagged object — the same shape convention as `DmaxSource`.
        assert_eq!(
            serde_json::to_string(&BalanceRange::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&BalanceRange::Explicit([0.25, 2.5])).unwrap(),
            r#"{"explicit":[0.25,2.5]}"#
        );
        for src in [BalanceRange::Auto, BalanceRange::Explicit([0.1, 1.9])] {
            let json = serde_json::to_string(&src).unwrap();
            assert_eq!(serde_json::from_str::<BalanceRange>(&json).unwrap(), src);
        }
    }

    #[test]
    fn partial_recipe_fills_defaults() {
        // A recipe that sets only one knob should leave the rest at defaults.
        let params: PrintParams = serde_json::from_str(r#"{"print_exposure": 2.0}"#).unwrap();
        assert_eq!(params.print_exposure, 2.0);
        assert_eq!(params.white_balance, WbSource::Explicit([1.0, 1.0, 1.0]));
    }

    #[test]
    fn wb_source_serializes_like_the_other_source_enums() {
        // Unit variants are bare kebab-case strings; the payload variant is a
        // tagged object — the same shape convention as `FilmBaseSource` /
        // `DmaxSource`.
        assert_eq!(
            serde_json::to_string(&WbSource::GrayWorld).unwrap(),
            "\"gray-world\""
        );
        assert_eq!(
            serde_json::to_string(&WbSource::Percentile).unwrap(),
            "\"percentile\""
        );
        assert_eq!(
            serde_json::to_string(&WbSource::Explicit([1.1, 1.0, 0.9])).unwrap(),
            r#"{"explicit":[1.1,1.0,0.9]}"#
        );
        for src in [
            WbSource::GrayWorld,
            WbSource::Percentile,
            WbSource::Explicit([2.0, 1.0, 0.5]),
        ] {
            let json = serde_json::to_string(&src).unwrap();
            assert_eq!(serde_json::from_str::<WbSource>(&json).unwrap(), src);
        }
    }

    #[test]
    fn wb_source_deserializes_legacy_bare_array_as_explicit() {
        // Before `WbSource`, `print.white_balance` was a plain `[f32; 3]`, so
        // existing recipes/sidecars serialize the bare array. The custom
        // `Deserialize` must still accept it (→ `Explicit`) for reproducibility,
        // alongside the tagged forms.
        assert_eq!(
            serde_json::from_str::<WbSource>("[1.1,1.0,0.9]").unwrap(),
            WbSource::Explicit([1.1, 1.0, 0.9])
        );
        // The same, nested in a recipe's `print` section (defaults fill the rest).
        let print: PrintParams =
            serde_json::from_str(r#"{"white_balance":[1.1,1.0,0.9]}"#).unwrap();
        assert_eq!(print.white_balance, WbSource::Explicit([1.1, 1.0, 0.9]));
        // The tagged forms still parse (the bare array is an *addition*).
        assert_eq!(
            serde_json::from_str::<WbSource>(r#"{"explicit":[1.1,1.0,0.9]}"#).unwrap(),
            WbSource::Explicit([1.1, 1.0, 0.9])
        );
        assert_eq!(
            serde_json::from_str::<WbSource>("\"gray-world\"").unwrap(),
            WbSource::GrayWorld
        );
    }

    #[test]
    fn input_axes_default_to_auto_and_round_trip() {
        // The two independent input axes default to `auto` and serialize in their
        // documented wire forms.
        let p = InputParams::default();
        assert_eq!(p.transfer, TransferAssertion::Auto);
        assert_eq!(p.meaning, MeaningAssertion::Auto);

        assert_eq!(
            serde_json::to_string(&TransferAssertion::Linear).unwrap(),
            "\"linear\""
        );
        assert_eq!(
            serde_json::to_string(&MeaningAssertion::ScannerDevice).unwrap(),
            "\"scanner-device\""
        );
        assert_eq!(
            serde_json::to_string(&MeaningAssertion::Colorimetric).unwrap(),
            "\"colorimetric\""
        );

        // A partial `input` section fills the untouched axis with its default.
        let p: InputParams = serde_json::from_str(r#"{"transfer":"linear"}"#).unwrap();
        assert_eq!(p.transfer, TransferAssertion::Linear);
        assert_eq!(p.meaning, MeaningAssertion::Auto);
    }

    #[test]
    fn input_params_rejects_unknown_and_legacy_color_key() {
        // `deny_unknown_fields`: the removed combined `color` key is not a field,
        // so it is rejected at the struct level (the friendlier migration message
        // is emitted earlier, by `cli::load_recipe`).
        assert!(serde_json::from_str::<InputParams>(r#"{"color":"linear"}"#).is_err());
    }

    #[test]
    fn wb_source_default_is_neutral_explicit_gains() {
        // The default must be *explicit* neutral gains, not an auto mode — auto
        // white balance is opt-in, and the default output stays bit-identical to
        // the pre-auto-WB render.
        assert_eq!(WbSource::default(), WbSource::Explicit([1.0, 1.0, 1.0]));
        assert_eq!(PrintParams::default().white_balance, WbSource::default());
    }
}
