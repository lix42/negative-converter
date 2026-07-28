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
/// present (HDRi input), is `len == width * height`. It is exported verbatim
/// (`--export-ir`) and, since `ir-holder-detection`, consumed by the chromogenic
/// film-base holder mask — but **only when [`ir_verified`](Self::ir_verified) is
/// true** (design-spec §6.1).
#[derive(Clone, Debug)]
pub struct LinearImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<f32>,
    pub ir: Option<Vec<f32>>,
    /// Whether the IR plane's provenance is **marker-verified** — the decoder
    /// found the SilverFast IR IFD's `NewSubfileType=4` marker, not merely a
    /// same-dimension 16-bit grayscale page identified by shape alone. A
    /// shape-only IR plane is still carried and exportable, but it must **not** be
    /// trusted by a conversion consumer (a stray grayscale page could otherwise be
    /// thresholded as IR and corrupt the film base), so the holder mask is skipped
    /// for it. Meaningful only when `ir.is_some()`; [`new`](Self::new) defaults it
    /// `false` and `io::decode` sets it from the marker.
    pub ir_verified: bool,
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
            // Provenance is not known at this boundary; `io::decode` sets it from
            // the IR IFD's `NewSubfileType=4` marker. A shape-only IR plane stays
            // `false` (carried/exportable but not trusted by consumers).
            ir_verified: false,
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

/// Reconstruction-type selector (design-spec §9, `--reconstruction` /
/// `reconstruction.type`).
///
/// A neutral selector that mirrors the CLI/recipe key, like the param structs —
/// it does not depend on the `algo` implementations. Serializes lowercase
/// (`"simple"` / `"density"`) and parses the same on the CLI via `ValueEnum`.
/// The full tagged configuration is [`Reconstruction`]; this is only its `type`
/// discriminator (the wire tag, the CLI value, and the telemetry summary).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReconstructionType {
    /// Channel-inversion baseline (debug / B&W).
    Simple,
    /// Density-domain reconstruction (Cineon / negadoctor) — the default.
    #[default]
    Density,
}

/// Density-curve selector (design-spec §9, `--density-curve` /
/// `reconstruction.curve.type`). Like [`ReconstructionType`], only the `type`
/// discriminator of the tagged [`DensityCurve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DensityCurveType {
    /// The straight-line `10^(gamma·(D′ − Dmax))` curve — the default.
    #[default]
    Exponential,
    /// The S-curve (photographic H&D / paper-response) with toe/shoulder knees.
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

/// Declared film chemistry (design-spec §9 `input.film_type`, §6.1) — the axis
/// that governs whether the IR plane can be trusted to separate film from the
/// opaque scanner holder (and, later, dust). **Chromogenic** dyes (C-41 colour
/// *and* C-41-process B&W) are transparent to infrared, so all film — base,
/// rebate, picture, even fully-exposed leader — reads bright in IR while the
/// opaque holder reads dark; the IR-assisted paths work. **Silver** halide B&W
/// blocks IR (dense silver reads dark, indistinguishable from the holder), so the
/// IR path must stay off. `Unknown` (default) is also off: the decoded scan
/// carries no reliable film-chemistry signal, and an IR plane's mere presence does
/// **not** imply chromogenic (a silver B&W scan can be HDRi with an IR plane) —
/// declare it explicitly with `--film-type`.
///
/// This is a **shared input-medium declaration**, not an `ir-holder-detection`
/// knob: other roadmap tasks reuse this same film-type axis — the black & white
/// `bw-support` task (roadmap item 3) for its B&W handling and the auto-`Dmax`
/// holder-border exclusion, and the separate IR dust-removal task (roadmap item 1)
/// gates its defect map on it (silver blocks IR like dust). Keep it cleanly named
/// and consumer-agnostic. Serializes kebab-case (`"unknown"` / `"silver"` /
/// `"chromogenic"`); parsed the same on the CLI via `ValueEnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum FilmType {
    /// Film chemistry not declared — the IR-assisted paths stay off (safe default).
    #[default]
    Unknown,
    /// Silver-halide B&W — silver blocks IR, so IR cannot separate holder from
    /// film; the IR-assisted paths stay off.
    Silver,
    /// Chromogenic dye film (C-41 colour or C-41-process B&W) — IR-transparent, so
    /// the IR-assisted holder mask (and later dust map) is usable.
    Chromogenic,
}

impl FilmType {
    /// Whether the film's dye chemistry is transparent to infrared, i.e. IR can be
    /// trusted to separate film from the opaque holder. True only for
    /// [`Chromogenic`](Self::Chromogenic); silver blocks IR and unknown is off by
    /// default. The IR plane must *also* be present — this is only the
    /// film-chemistry half of the gate. Shared with later roadmap consumers — the
    /// `bw-support` B&W task and the separate IR dust-removal task.
    pub fn ir_transparent(self) -> bool {
        matches!(self, FilmType::Chromogenic)
    }
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
    /// Declared film chemistry (default `unknown`). Gates the IR-assisted
    /// film-holder detection (`ir-holder-detection`) and is reused by later roadmap
    /// tasks (`bw-support` B&W handling; the separate IR dust-removal task): only
    /// `chromogenic` enables the IR path; `silver` / `unknown` keep it off. See
    /// [`FilmType`].
    pub film_type: FilmType,
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
/// `film-base/content-fallback` task and is deliberately **not** a variant here —
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

/// Density-reconstruction knobs (design-spec §9, `reconstruction.density`) —
/// everything that shapes the corrected density `D′` (stages 1–2). The
/// density→positive curve (gamma / contrast / `Dmax`) is deliberately **not**
/// here: it belongs to the tagged [`DensityCurve`], the separate curve stage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DensityParams {
    /// Per-channel density gain `[r, g, b]`.
    pub scale: [f32; 3],
    /// Per-channel density offset `[r, g, b]` (orange-mask compensation).
    pub offset: [f32; 3],
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
            scale: [1.0, 1.0, 1.0],
            offset: [0.0, 0.0, 0.0],
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
///
/// Three of these knobs (`white_balance`, `print_exposure`, `black_point`) plus
/// [`linear_range`](Self::linear_range) are the **shared** print controls the
/// named-output split resolves once for both display branches
/// (`pipeline::render_split`, design-spec §6): the pinned order is
/// `white balance → exposure → black point → linear_range placement`.
/// `highlight_compress` is deliberately **not** shared — highlight roll-off is
/// branch-specific SDR tone policy (`output/sdr-display-rendering`), and the
/// legacy no-preset render is the only consumer today.
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
    /// Black/white-range placement endpoints `[low, high]` in the rendered
    /// positive's linear domain — the exact affine `(x − low)/(high − low)` the
    /// shared display stage applies last (design-spec §6/§9,
    /// `print.linear_range` / `--linear-range LOW,HIGH`). The default `[0, 1]` is
    /// the exact identity. Requires finite `low < high`.
    ///
    /// This is the replacement home for `simple` reconstruction's removed
    /// `clip_low`/`clip_high` endpoints (design-spec §7.1) and is distinct from
    /// the density print `black_point`. Only the shared display stage consumes it,
    /// so until a named display preset lands a non-default value is rejected
    /// loudly by `cli::validate` rather than silently ignored.
    pub linear_range: [f32; 2],
}

impl Default for PrintParams {
    fn default() -> Self {
        Self {
            print_exposure: 0.0,
            black_point: 0.0,
            white_balance: WbSource::default(),
            highlight_compress: 0.0,
            linear_range: [0.0, 1.0],
        }
    }
}

/// Exponential-curve knobs (design-spec §7.2/§9,
/// `reconstruction.curve.type = "exponential"`): the straight-line
/// `10^(gamma·(D′ − Dmax))` density→positive mapping. The curve owns the
/// display-white placement — for the exponential curve `Dmax` is the scalar
/// exponent placement (`"none"` = unity placement, base → `1.0`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExponentialParams {
    /// Film/print curve gamma (the straight line's slope).
    pub gamma: f32,
    /// Display-white anchor source (default `fixed`). `"none"` (unity
    /// placement) is valid only for this curve — the sigmoid is anchored on
    /// `[0, Dmax]` and cannot run without one.
    pub dmax: DmaxSource,
}

impl Default for ExponentialParams {
    fn default() -> Self {
        Self {
            gamma: 1.0,
            dmax: DmaxSource::Fixed,
        }
    }
}

/// Sigmoid-curve knobs (design-spec §7.3/§9,
/// `reconstruction.curve.type = "sigmoid"`): the S-curve mapping corrected
/// density to positive linear. It shares the density-reconstruction stage
/// ([`DensityParams`]) with the exponential curve; `contrast` is the `gamma`
/// analogue (gamma itself exists only in the exponential variant), and `Dmax`
/// here is a curve-shaping input — both knees derive from it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SigmoidParams {
    /// Mid-density slope of the curve in log-output space (the exponential
    /// `gamma` analogue). Must be finite and > 0.
    pub contrast: f32,
    /// Toe (shadow) knee width in log10 density units: how softly the curve
    /// approaches paper black. `0` disables the toe (hard straight-line black).
    pub toe: f32,
    /// Shoulder (highlight) knee width in log10 density units: how softly the
    /// curve approaches display white. `0` disables the shoulder.
    pub shoulder: f32,
    /// Display-white anchor source (default `fixed`). `"none"` is rejected for
    /// this curve (`cli::validate`) — the S-curve is anchored on `[0, Dmax]`.
    pub dmax: DmaxSource,
}

impl Default for SigmoidParams {
    fn default() -> Self {
        Self {
            contrast: 1.0,
            toe: 0.2,
            shoulder: 0.2,
            dmax: DmaxSource::Fixed,
        }
    }
}

/// The tagged density→positive curve (design-spec §8/§9,
/// `reconstruction.curve`): exactly one of the two curve variants, each carrying
/// its own knobs plus the `Dmax` placement it owns. Serializes internally tagged
/// (`{"type":"exponential","gamma":…,"dmax":…}`); the custom `Deserialize`
/// mirrors that while rejecting cross-variant keys **by name** (e.g. `contrast`
/// under `exponential`) with a loud usage message instead of serde's generic
/// unknown-field error, and fills each variant's defaults for omitted fields.
/// The `type` tag is required whenever a `curve` object is present; only a fully
/// *omitted* `reconstruction.curve` defaults (to this enum's default —
/// exponential with its defaults).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DensityCurve {
    Exponential(ExponentialParams),
    Sigmoid(SigmoidParams),
}

impl Default for DensityCurve {
    fn default() -> Self {
        DensityCurve::Exponential(ExponentialParams::default())
    }
}

impl DensityCurve {
    /// This curve's `type` discriminator (wire tag / CLI value).
    pub fn curve_type(&self) -> DensityCurveType {
        match self {
            DensityCurve::Exponential(_) => DensityCurveType::Exponential,
            DensityCurve::Sigmoid(_) => DensityCurveType::Sigmoid,
        }
    }

    /// The display-white anchor source this curve carries. One accessor instead
    /// of per-variant field reads, so `Dmax` policy code (merge, validate,
    /// reports) can stay variant-agnostic.
    pub fn dmax(&self) -> DmaxSource {
        match self {
            DensityCurve::Exponential(e) => e.dmax,
            DensityCurve::Sigmoid(s) => s.dmax,
        }
    }

    /// Mutable access to the anchor source — the single write point the merge
    /// uses for the four `--*d-max` flags, whichever variant is resolved.
    pub fn dmax_mut(&mut self) -> &mut DmaxSource {
        match self {
            DensityCurve::Exponential(e) => &mut e.dmax,
            DensityCurve::Sigmoid(s) => &mut s.dmax,
        }
    }
}

/// Extract an optional field from a raw recipe object, distinguishing an **absent**
/// key (`Ok(None)`) from a **present** one (`Ok(Some(v))`) — including a present
/// explicit `null`, which is not a valid value for any of these typed fields and
/// so errors loudly rather than reading as "absent". A plain `Option<T>` struct
/// field cannot make this distinction (serde collapses JSON `null` to `None`), so
/// the tagged deserializers below capture the raw object and use key *presence* to
/// reject a cross-variant key or a null discriminator that would otherwise be
/// silently ignored / defaulted.
fn take_recipe_field<T: serde::de::DeserializeOwned>(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> std::result::Result<Option<T>, String> {
    match obj.get(key) {
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

impl<'de> Deserialize<'de> for DensityCurve {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        // Capture the raw object so a cross-variant key is detected by *presence*,
        // not by its value: a plain `Option<f32>` field turns an explicit `null`
        // into `None`, indistinguishable from an absent key, and would silently
        // accept e.g. `{"type":"exponential","contrast":null}` instead of rejecting
        // the invalid tagged combination. `type` is still required — a
        // present-but-untagged curve object must not guess a variant.
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("reconstruction.curve must be a JSON object"))?;

        const KNOWN: [&str; 6] = ["type", "gamma", "contrast", "toe", "shoulder", "dmax"];
        if let Some(k) = obj.keys().find(|k| !KNOWN.contains(&k.as_str())) {
            return Err(D::Error::custom(format!(
                "unknown field `{k}` in reconstruction.curve"
            )));
        }

        let curve_type: DensityCurveType = take_recipe_field(obj, "type")
            .map_err(D::Error::custom)?
            .ok_or_else(|| {
                D::Error::custom(
                    "reconstruction.curve is missing `type` (\"exponential\" or \"sigmoid\")",
                )
            })?;
        let dmax: Option<DmaxSource> = take_recipe_field(obj, "dmax").map_err(D::Error::custom)?;

        match curve_type {
            DensityCurveType::Exponential => {
                if let Some(key) = ["contrast", "toe", "shoulder"]
                    .into_iter()
                    .find(|k| obj.contains_key(*k))
                {
                    return Err(D::Error::custom(format!(
                        "`{key}` is a sigmoid-curve key, but the curve type is \
                         \"exponential\" (its knobs are `gamma` and `dmax`)"
                    )));
                }
                let d = ExponentialParams::default();
                Ok(DensityCurve::Exponential(ExponentialParams {
                    gamma: take_recipe_field(obj, "gamma")
                        .map_err(D::Error::custom)?
                        .unwrap_or(d.gamma),
                    dmax: dmax.unwrap_or(d.dmax),
                }))
            }
            DensityCurveType::Sigmoid => {
                if obj.contains_key("gamma") {
                    return Err(D::Error::custom(
                        "`gamma` is an exponential-curve key, but the curve type is \
                         \"sigmoid\" (the mid-density slope analogue is `contrast`)",
                    ));
                }
                let d = SigmoidParams::default();
                Ok(DensityCurve::Sigmoid(SigmoidParams {
                    contrast: take_recipe_field(obj, "contrast")
                        .map_err(D::Error::custom)?
                        .unwrap_or(d.contrast),
                    toe: take_recipe_field(obj, "toe")
                        .map_err(D::Error::custom)?
                        .unwrap_or(d.toe),
                    shoulder: take_recipe_field(obj, "shoulder")
                        .map_err(D::Error::custom)?
                        .unwrap_or(d.shoulder),
                    dmax: dmax.unwrap_or(d.dmax),
                }))
            }
        }
    }
}

/// Wire schema version of the tagged `reconstruction` recipe/report object
/// (design-spec §8). Versions the **schema shape only** — it is not the
/// behavioral `pipeline_version` (owned by the `conversion-versioning` task,
/// bumped only when default pixels change). Every resolved recipe/report emits
/// it; partial input may omit it (defaults to this value); any other value is
/// rejected loudly.
pub const RECONSTRUCTION_SCHEMA_VERSION: u32 = 1;

/// The tagged reconstruction configuration (design-spec §8/§9, the recipe's one
/// `reconstruction` object): `simple` (direct inversion, no further knobs) or
/// `density` (density correction plus exactly one tagged [`DensityCurve`]).
///
/// One enum, not parallel section fields, so an illegal combination (simple
/// with a curve, two algorithms at once) is unrepresentable — the tagged-enum
/// convention `FilmBaseSource`/`DmaxSource` established, applied to the
/// algorithm selection itself. The custom serde keeps the documented wire shape
/// exactly: `schema_version` + `type` always emitted; `density`/`curve` emitted
/// for (and accepted only with) `type = "density"`; omitted sections fill their
/// defaults (an omitted `curve` normalizes to tagged exponential defaults, so
/// omission never survives into a resolved recipe or report).
#[derive(Clone, Debug, PartialEq)]
pub enum Reconstruction {
    /// Channel-inversion baseline (debug / B&W): the direct unclamped positive
    /// `1 − scan/Dmin`. No density or curve configuration.
    Simple,
    /// Density-domain reconstruction (the default): corrected density `D′`
    /// (stages 1–2, `density`) mapped through the tagged `curve`.
    Density {
        density: DensityParams,
        curve: DensityCurve,
    },
}

impl Default for Reconstruction {
    fn default() -> Self {
        Reconstruction::Density {
            density: DensityParams::default(),
            curve: DensityCurve::default(),
        }
    }
}

impl Reconstruction {
    /// This configuration's `type` discriminator (wire tag / CLI value).
    pub fn reconstruction_type(&self) -> ReconstructionType {
        match self {
            Reconstruction::Simple => ReconstructionType::Simple,
            Reconstruction::Density { .. } => ReconstructionType::Density,
        }
    }

    /// The resolved curve's type, when this is a density reconstruction
    /// (`None` for `simple` — it has no curve stage).
    pub fn curve_type(&self) -> Option<DensityCurveType> {
        match self {
            Reconstruction::Simple => None,
            Reconstruction::Density { curve, .. } => Some(curve.curve_type()),
        }
    }
}

impl Serialize for Reconstruction {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        match self {
            Reconstruction::Simple => {
                let mut st = serializer.serialize_struct("Reconstruction", 2)?;
                st.serialize_field("schema_version", &RECONSTRUCTION_SCHEMA_VERSION)?;
                st.serialize_field("type", "simple")?;
                st.end()
            }
            Reconstruction::Density { density, curve } => {
                let mut st = serializer.serialize_struct("Reconstruction", 4)?;
                st.serialize_field("schema_version", &RECONSTRUCTION_SCHEMA_VERSION)?;
                st.serialize_field("type", "density")?;
                st.serialize_field("density", density)?;
                st.serialize_field("curve", curve)?;
                st.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Reconstruction {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        // Capture the raw object so `type` presence is distinguished from a present
        // explicit `null`: with `Option<ReconstructionType>`, `{"type":null}` would
        // collapse to `None` and silently default to density, and `type:"simple"`
        // would accept a null `density`/`curve` section. Presence-checking rejects
        // those malformed recipes loudly instead.
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("reconstruction must be a JSON object"))?;

        const KNOWN: [&str; 4] = ["schema_version", "type", "density", "curve"];
        if let Some(k) = obj.keys().find(|k| !KNOWN.contains(&k.as_str())) {
            return Err(D::Error::custom(format!(
                "unknown field `{k}` in reconstruction"
            )));
        }

        if let Some(v) =
            take_recipe_field::<u32>(obj, "schema_version").map_err(D::Error::custom)?
            && v != RECONSTRUCTION_SCHEMA_VERSION
        {
            return Err(D::Error::custom(format!(
                "unsupported reconstruction.schema_version {v} \
                 (this build reads schema_version {RECONSTRUCTION_SCHEMA_VERSION})"
            )));
        }

        // `type` present-but-null errors here (null is not a valid tag); absent
        // defaults to density.
        let reconstruction_type: ReconstructionType = take_recipe_field(obj, "type")
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        match reconstruction_type {
            ReconstructionType::Simple => {
                if let Some(key) = ["density", "curve"]
                    .into_iter()
                    .find(|k| obj.contains_key(*k))
                {
                    return Err(D::Error::custom(format!(
                        "reconstruction.type = \"simple\" takes no `{key}` section — \
                         density correction and density curves belong to \
                         `type = \"density\"`"
                    )));
                }
                Ok(Reconstruction::Simple)
            }
            ReconstructionType::Density => Ok(Reconstruction::Density {
                density: take_recipe_field(obj, "density")
                    .map_err(D::Error::custom)?
                    .unwrap_or_default(),
                // An omitted curve normalizes to tagged exponential defaults —
                // omission never survives into a resolved recipe.
                curve: take_recipe_field(obj, "curve")
                    .map_err(D::Error::custom)?
                    .unwrap_or_default(),
            }),
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

/// Named output preset (design-spec §5/§9, `output.preset` /
/// `--output-preset`) — the atomic output *policy* choice: which branch the
/// render takes out of the NC film RGB v1 ACEScg boundary, and the container /
/// depth / profile that branch resolves.
///
/// One mutually-exclusive enum field, like [`FilmBaseSource`] / [`DmaxSource`]:
/// a preset resolves a whole coherent policy, so it can never be a bag of
/// independent bools. Serializes kebab-case (`"legacy"` / `"film-master"`).
///
/// **Only two variants are accepted today.** The remaining planned preset names
/// (`gain-map-hdr`, `display-p3`, `compatibility`, `hdr-pq`, `hdr-hlg`,
/// `custom`) need the SDR/HDR display renderers and the container work owned by
/// `output/presets`; [`parse`](Self::parse) rejects them with a pinned
/// "does not accept yet" message rather than a generic unknown-value error, and
/// rejects the pre-release name `scene-master` as an unreleased-schema break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OutputPreset {
    /// **No named preset** (default) — the transitional legacy TIFF path: the
    /// print controls still run *before* the working→output ICC transform, and
    /// the `output.hdr` / `output.output_profile` / `output.bigtiff` flags select
    /// depth and profile exactly as they did before presets existed. Its
    /// *pre-colour-transform* pixels are frozen bit-for-bit by
    /// `pipeline::stages::golden` (which calls `reconstruct_and_print` directly);
    /// `stages`' `legacy_preset_render_is_the_frozen_reconstruct_print_colour_sequence`
    /// separately pins that this branch of `render` is still that whole sequence.
    /// `output/presets` replaces this default with `gain-map-hdr`.
    #[default]
    Legacy,
    /// **`film-master`** — an unclamped 32-bit float linear ACEScg TIFF taken
    /// **directly** from the NC film RGB v1 mapping. It preserves the intentional
    /// film, lens, development, scanner, reconstruction, and density-curve
    /// rendering (including supported fixed/roll `Dmax` placement) and bypasses
    /// every later white-balance, exposure, black/range-placement, highlight,
    /// display tone, gamut, and transfer operation. It is **not** a physical
    /// scene-linear recovery.
    ///
    /// The bypass is strict, not silent: `cli::validate` rejects frame-local
    /// `auto` `Dmax` and every non-default downstream control, whatever their
    /// source. A linear export that *wants* a creative / print / display
    /// adjustment is the (not-yet-accepted) `custom` workflow.
    FilmMaster,
}

impl OutputPreset {
    /// Parse the `--output-preset` value / `output.preset` recipe key. Shared by
    /// the CLI merge and the custom [`Deserialize`] below so a name gets the same
    /// diagnosis wherever it appears (the `OutputSpace::parse` precedent).
    pub fn parse(s: &str) -> Result<Self> {
        // Case-insensitive like `OutputSpace::parse`: these are keywords, not paths.
        match s.trim().to_ascii_lowercase().as_str() {
            "legacy" => Ok(OutputPreset::Legacy),
            "film-master" => Ok(OutputPreset::FilmMaster),
            // The pre-release name for the same branch. It was renamed *before*
            // release because "scene" wrongly implied physical scene-linear
            // recovery; nc is unreleased, so this is a schema break, not an alias.
            "scene-master" => Err(NcError::Usage(
                "output preset `scene-master` does not exist — it was renamed \
                 `film-master` before release (the master carries NC's intentional \
                 film/lens/development/scanner rendering, not a physical \
                 scene-linear recovery). Use `film-master`; there is no alias."
                    .into(),
            )),
            planned @ ("gain-map-hdr" | "display-p3" | "compatibility" | "hdr-pq" | "hdr-hlg"
            | "custom") => Err(NcError::Usage(format!(
                "output preset `{planned}` is a planned name that this build does not \
                 accept yet (it needs the display renderers / container work owned by \
                 `output/presets`). Accepted today: `legacy` (the transitional TIFF \
                 path, the default) and `film-master` (unclamped linear ACEScg float \
                 TIFF)."
            ))),
            other => Err(NcError::Usage(format!(
                "unknown output preset `{other}` — accepted: `legacy`, `film-master`"
            ))),
        }
    }

    /// Whether this preset is a *named* output (i.e. not the legacy no-preset
    /// path). Named presets are atomic: the legacy depth/profile/container
    /// selectors cannot accompany them.
    pub fn is_named(self) -> bool {
        !matches!(self, OutputPreset::Legacy)
    }

    /// The preset's stable wire / CLI name — the same string [`parse`](Self::parse)
    /// accepts and `Serialize` emits. Diagnostics take the name from here rather
    /// than hardcoding a literal, so a message about the *next* named preset can
    /// never end up describing `film-master`.
    pub fn name(self) -> &'static str {
        match self {
            OutputPreset::Legacy => "legacy",
            OutputPreset::FilmMaster => "film-master",
        }
    }
}

impl<'de> Deserialize<'de> for OutputPreset {
    /// Delegates to [`OutputPreset::parse`], so a recipe's `output.preset` gets the
    /// same pinned migration / not-yet-accepted diagnostics as the CLI flag
    /// (serde's derived enum error would only list the two accepted variants).
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let s = String::deserialize(deserializer)?;
        OutputPreset::parse(&s).map_err(|e| D::Error::custom(e.to_string()))
    }
}

/// Per-channel statistics of the samples **as written** to the output file —
/// report-only, and the numeric basis for cross-version comparison
/// (`core/conversion-versioning`).
///
/// Only the mean is recorded, deliberately: for a fixed scan + recipe, the
/// per-channel *mean ΔRGB* between two builds is exactly the difference of the two
/// runs' per-channel means (`mean(a) - mean(b) = mean(a - b)`), so `nctool compare`
/// derives that metric from two run records without ever re-reading, registering,
/// or shipping pixels. Richer metrics (ΔE2000, SSIM) need real pixel access and
/// belong to the QA harness (design-spec §12 item 7), not here.
///
/// Units are the written sample's own domain: the u16 path reports the quantized
/// value scaled back to `[0, 1]` (so it is exact integer arithmetic, identical on
/// every target given identical pixels); the f32 path reports the verbatim
/// (unclamped, possibly > 1.0) float mean over the **finite** samples, with
/// non-finite samples excluded so one `NaN` cannot swallow the whole statistic —
/// `EncodeReport::non_finite` is where that fault is reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct OutputStats {
    /// Mean written sample value per channel `[r, g, b]`. Zero for an empty image.
    pub mean: [f64; 3],
}

/// What the encode stage produced: the loss accounting the orchestrator turns into
/// report warnings, plus the report-only per-channel statistics of the written
/// samples.
///
/// Bundled so the caller never has to re-read the output file to get the statistics.
/// The means are a **second** pass over the sample buffer (after `quantize_u16` /
/// the non-finite scan), not a free by-product of the first, and it is paid
/// unconditionally — including under `--report none`, where nothing consumes it.
/// Deliberate for now: making it conditional would push the report mode down into
/// `io::encode`, coupling the encoder to an orchestration concern for one linear scan
/// of already-hot memory. Revisit if `telemetry/perf-instrumentation` ever shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[must_use]
pub struct EncodeOutcome {
    pub loss: EncodeReport,
    pub stats: OutputStats,
}

/// Output / encode knobs (design-spec §9, stage 5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct OutputParams {
    /// Named output preset (default `legacy` = no preset). Selects the branch out
    /// of the ACEScg boundary and, for a named preset, the container/depth/profile
    /// policy — so under `film-master` the three legacy selectors below must stay
    /// at their defaults (a named preset is atomic; `cli::validate` rejects a
    /// non-default one loudly instead of silently overriding it).
    pub preset: OutputPreset,
    /// HDR output switch (default `false`): `false` → 16-bit integer TIFF,
    /// `true` → 32-bit float TIFF (full HDR, no precision loss). Legacy path only
    /// — it is the *transitional rendered* float TIFF (print controls already
    /// applied) and is **never** an alias for the `film-master` preset.
    pub hdr: bool,
    /// Output ICC profile selector (`sRGB`/`prophoto`/`acescg`/path). `None`
    /// means the depth-aware default (sRGB for the 16-bit default, wide-gamut
    /// linear for `hdr`). Legacy path only — `film-master` resolves linear ACEScg
    /// itself.
    pub output_profile: Option<String>,
    /// BigTIFF promotion policy (default `auto`).
    pub bigtiff: BigTiff,
}

impl OutputParams {
    /// The encoder bit depth this output resolves to. The single place a recipe
    /// value becomes a depth, so encode, IR export, and color can't disagree:
    ///
    /// - `film-master` is **always** [`OutDepth::F32`] — the master is unclamped
    ///   float linear ACEScg by definition, not by an `output.hdr` switch (which
    ///   must stay at its default under the preset).
    /// - legacy: `hdr = false` → [`OutDepth::U16`], `true` → [`OutDepth::F32`].
    pub fn depth(&self) -> OutDepth {
        match self.preset {
            OutputPreset::FilmMaster => OutDepth::F32,
            OutputPreset::Legacy => {
                if self.hdr {
                    OutDepth::F32
                } else {
                    OutDepth::U16
                }
            }
        }
    }

    /// The first legacy depth/profile/container selector that is **not** at its
    /// documented default, as `(name, value)` for a diagnostic — or `None` when all
    /// three are default (the atomicity precondition for a named preset).
    ///
    /// Destructured, not field-accessed: adding an output selector makes this
    /// binding fail to compile, forcing the author to decide whether a named preset
    /// resolves it. A field-access sweep would silently omit the new knob and
    /// reintroduce exactly the silent-override this check exists to prevent — the
    /// same reason `cli::validate_output_preset` destructures [`PrintParams`].
    ///
    /// Each name lists the flag(s) *and* the recipe key, because the check runs on
    /// the **resolved** value: a selector is rejected identically whether it came
    /// from a flag or from the recipe, and the message must not guess which.
    pub fn non_default_legacy_selector(&self) -> Option<(&'static str, String)> {
        let d = Self::default();
        let Self {
            preset: _,
            hdr,
            output_profile,
            bigtiff,
        } = self;
        [
            (
                "--output-hdr / --output-sdr / output.hdr",
                *hdr != d.hdr,
                format!("{hdr}"),
            ),
            (
                "--output-profile / output.output_profile",
                *output_profile != d.output_profile,
                format!("{output_profile:?}"),
            ),
            (
                "--bigtiff / output.bigtiff",
                *bigtiff != d.bigtiff,
                format!("{bigtiff:?}"),
            ),
        ]
        .into_iter()
        .find_map(|(name, non_default, value)| non_default.then_some((name, value)))
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
            scale: [1.2, 1.0, 0.8],
            offset: [0.1, 0.0, -0.05],
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
    fn curve_default_dmax_is_fixed() {
        // The default anchor is the roll-fixed nominal `Fixed`, not the demoted
        // per-frame `Auto` (dmax-reference): the faithful-conversion default must
        // not normalize exposure per frame. Both curve variants own a `dmax` and
        // both default it to `Fixed`.
        assert_eq!(ExponentialParams::default().dmax, DmaxSource::Fixed);
        assert_eq!(SigmoidParams::default().dmax, DmaxSource::Fixed);
        assert_eq!(DensityCurve::default().dmax(), DmaxSource::Fixed);
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
    fn curve_json_round_trips_both_tagged_variants() {
        let exponential = DensityCurve::Exponential(ExponentialParams {
            gamma: 1.4,
            dmax: DmaxSource::Explicit(1.8),
        });
        let sigmoid = DensityCurve::Sigmoid(SigmoidParams {
            contrast: 1.3,
            toe: 0.1,
            shoulder: 0.4,
            dmax: DmaxSource::Auto,
        });
        for curve in [exponential, sigmoid] {
            let json = serde_json::to_string(&curve).unwrap();
            assert_eq!(serde_json::from_str::<DensityCurve>(&json).unwrap(), curve);
        }
        // The wire form is internally tagged with the documented key names.
        assert_eq!(
            serde_json::to_string(&DensityCurve::default()).unwrap(),
            r#"{"type":"exponential","gamma":1.0,"dmax":"fixed"}"#
        );
    }

    #[test]
    fn curve_partial_input_fills_variant_defaults_but_requires_the_tag() {
        // A tagged-but-partial curve fills that variant's defaults.
        let c: DensityCurve = serde_json::from_str(r#"{"type":"sigmoid","contrast":2.0}"#).unwrap();
        assert_eq!(
            c,
            DensityCurve::Sigmoid(SigmoidParams {
                contrast: 2.0,
                ..SigmoidParams::default()
            })
        );
        let c: DensityCurve = serde_json::from_str(r#"{"type":"exponential"}"#).unwrap();
        assert_eq!(c, DensityCurve::default());
        // A present-but-untagged curve object must not guess a variant.
        assert!(serde_json::from_str::<DensityCurve>(r#"{"gamma":1.2}"#).is_err());
    }

    #[test]
    fn curve_rejects_cross_variant_keys_by_name() {
        // Sigmoid-only keys under exponential are named in the error, not
        // silently ignored and not a generic unknown-field message.
        for json in [
            r#"{"type":"exponential","contrast":2.0}"#,
            r#"{"type":"exponential","toe":0.1}"#,
            r#"{"type":"exponential","shoulder":0.1}"#,
        ] {
            let err = serde_json::from_str::<DensityCurve>(json).unwrap_err();
            assert!(
                err.to_string().contains("sigmoid-curve key"),
                "unexpected error for {json}: {err}"
            );
        }
        // …and gamma under sigmoid points at `contrast`.
        let err =
            serde_json::from_str::<DensityCurve>(r#"{"type":"sigmoid","gamma":1.2}"#).unwrap_err();
        assert!(err.to_string().contains("contrast"), "{err}");
        // A key belonging to neither variant is still an unknown field.
        assert!(
            serde_json::from_str::<DensityCurve>(r#"{"type":"exponential","gama":1.2}"#).is_err()
        );
    }

    #[test]
    fn tagged_recipes_reject_null_valued_cross_variant_and_tag_keys() {
        // A present-but-null cross-variant key must be rejected by *presence*, not
        // silently read as absent (the trap a plain `Option<f32>`/`Option<T>` field
        // would fall into: serde collapses JSON `null` to `None`).
        let err = serde_json::from_str::<DensityCurve>(r#"{"type":"exponential","contrast":null}"#)
            .unwrap_err();
        assert!(err.to_string().contains("sigmoid-curve key"), "{err}");
        assert!(
            serde_json::from_str::<DensityCurve>(r#"{"type":"sigmoid","gamma":null}"#)
                .unwrap_err()
                .to_string()
                .contains("exponential-curve key")
        );
        // A null discriminator must not silently default to density…
        assert!(serde_json::from_str::<Reconstruction>(r#"{"type":null}"#).is_err());
        // …and a null forbidden section under `simple` must still be rejected.
        let err = serde_json::from_str::<Reconstruction>(r#"{"type":"simple","density":null}"#)
            .unwrap_err();
        assert!(err.to_string().contains("takes no `density`"), "{err}");
        assert!(
            serde_json::from_str::<Reconstruction>(r#"{"type":"simple","curve":null}"#).is_err()
        );
    }

    #[test]
    fn reconstruction_serializes_the_documented_tagged_shapes() {
        // `schema_version` + `type` always emitted; simple carries nothing else.
        assert_eq!(
            serde_json::to_string(&Reconstruction::Simple).unwrap(),
            r#"{"schema_version":1,"type":"simple"}"#
        );
        // Density always emits its blocks and exactly one tagged curve — an
        // omitted input curve never survives normalization.
        let json = serde_json::to_value(Reconstruction::default()).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["type"], "density");
        assert_eq!(json["density"]["scale"], serde_json::json!([1.0, 1.0, 1.0]));
        assert_eq!(json["curve"]["type"], "exponential");
    }

    #[test]
    fn reconstruction_partial_input_normalizes() {
        // Empty object = all defaults: density with exponential defaults.
        let r: Reconstruction = serde_json::from_str("{}").unwrap();
        assert_eq!(r, Reconstruction::default());
        // Omitted schema_version defaults to 1; an explicit 1 also parses.
        let r: Reconstruction = serde_json::from_str(r#"{"schema_version":1}"#).unwrap();
        assert_eq!(r, Reconstruction::default());
        // Omitted curve under density normalizes to tagged exponential defaults.
        let r: Reconstruction =
            serde_json::from_str(r#"{"type":"density","density":{"scale":[1.1,1.0,0.9]}}"#)
                .unwrap();
        assert_eq!(
            r,
            Reconstruction::Density {
                density: DensityParams {
                    scale: [1.1, 1.0, 0.9],
                    ..DensityParams::default()
                },
                curve: DensityCurve::default(),
            }
        );
        // Round trip: resolved → JSON → resolved is identity for both types.
        for r in [
            Reconstruction::Simple,
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Sigmoid(SigmoidParams::default()),
            },
        ] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Reconstruction>(&json).unwrap(), r);
        }
    }

    #[test]
    fn reconstruction_rejects_bad_schema_version_and_illegal_combinations() {
        // Any schema_version other than 1 is rejected loudly.
        let err = serde_json::from_str::<Reconstruction>(r#"{"schema_version":2}"#).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "{err}");
        // Simple takes no density/curve section.
        for json in [
            r#"{"type":"simple","density":{}}"#,
            r#"{"type":"simple","curve":{"type":"exponential"}}"#,
        ] {
            let err = serde_json::from_str::<Reconstruction>(json).unwrap_err();
            assert!(err.to_string().contains("simple"), "{json}: {err}");
        }
        // Unknown fields are rejected at the reconstruction level too.
        assert!(serde_json::from_str::<Reconstruction>(r#"{"algorithm":"density"}"#).is_err());
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
    fn film_type_defaults_to_unknown_and_gates_the_ir_path() {
        // The default is the safe off state, and only chromogenic reports the film
        // as IR-transparent (the shared gate `ir-holder-detection` / `bw-support`
        // key on). Silver and unknown keep the IR path off.
        assert_eq!(FilmType::default(), FilmType::Unknown);
        assert_eq!(InputParams::default().film_type, FilmType::Unknown);
        assert!(FilmType::Chromogenic.ir_transparent());
        assert!(!FilmType::Silver.ir_transparent());
        assert!(!FilmType::Unknown.ir_transparent());
    }

    #[test]
    fn film_type_round_trips_kebab_case() {
        assert_eq!(
            serde_json::to_string(&FilmType::Chromogenic).unwrap(),
            "\"chromogenic\""
        );
        for t in [FilmType::Unknown, FilmType::Silver, FilmType::Chromogenic] {
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<FilmType>(&json).unwrap(), t);
        }
        // A partial `input` section fills the untouched film_type with its default.
        let p: InputParams = serde_json::from_str(r#"{"film_type":"silver"}"#).unwrap();
        assert_eq!(p.film_type, FilmType::Silver);
        assert_eq!(p.transfer, TransferAssertion::Auto);
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

    #[test]
    fn linear_range_default_is_the_exact_identity_and_lives_under_print() {
        // `(x − 0)/(1 − 0)` is the exact identity, so adding the knob cannot perturb
        // any existing output. Its recipe home is `print.linear_range` (design-spec
        // §9's Print / tone render section) — a misplaced key would be silently
        // rejected by `deny_unknown_fields` on docs-shaped recipes.
        assert_eq!(PrintParams::default().linear_range, [0.0, 1.0]);
        let p: PrintParams = serde_json::from_str(r#"{"linear_range":[0.02,0.97]}"#).unwrap();
        assert_eq!(p.linear_range, [0.02, 0.97]);
        // Round-trips, and the untouched siblings keep their defaults.
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"linear_range\":[0.02,0.97]"), "{json}");
        assert_eq!(serde_json::from_str::<PrintParams>(&json).unwrap(), p);
        assert_eq!(p.print_exposure, 0.0);
        // The removed simple-reconstruction spelling is not a recipe key here.
        assert!(serde_json::from_str::<PrintParams>(r#"{"clip_low":0.02}"#).is_err());
    }

    #[test]
    fn output_preset_parses_accepted_names_and_diagnoses_the_rest() {
        assert_eq!(OutputPreset::parse("legacy").unwrap(), OutputPreset::Legacy);
        assert_eq!(
            OutputPreset::parse("film-master").unwrap(),
            OutputPreset::FilmMaster
        );
        // Keywords, so case/whitespace-insensitive like `OutputSpace::parse`.
        assert_eq!(
            OutputPreset::parse(" Film-Master ").unwrap(),
            OutputPreset::FilmMaster
        );
        assert_eq!(OutputPreset::default(), OutputPreset::Legacy);
        assert!(OutputPreset::FilmMaster.is_named());
        assert!(!OutputPreset::Legacy.is_named());

        // The pre-release name is an unreleased-schema break, NOT an alias: the
        // message must name the rename and the reason, and must not silently accept.
        let err = OutputPreset::parse("scene-master").unwrap_err();
        assert!(matches!(err, NcError::Usage(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("scene-master"), "{msg}");
        assert!(msg.contains("film-master"), "{msg}");
        assert!(msg.contains("no alias"), "{msg}");

        // A planned-but-unimplemented name gets its own diagnosis rather than a bare
        // "unknown", so an agent can tell "not yet" from "typo".
        for planned in [
            "gain-map-hdr",
            "display-p3",
            "compatibility",
            "hdr-pq",
            "hdr-hlg",
            "custom",
        ] {
            let msg = OutputPreset::parse(planned).unwrap_err().to_string();
            assert!(msg.contains("does not accept yet"), "{planned}: {msg}");
            assert!(msg.contains("film-master"), "{planned}: {msg}");
        }
        let msg = OutputPreset::parse("filmmaster").unwrap_err().to_string();
        assert!(msg.contains("unknown output preset"), "{msg}");
    }

    #[test]
    fn output_preset_recipe_key_round_trips_and_shares_the_parse_diagnostics() {
        // Recipe home is `output.preset` (design-spec §9's Output / encode section).
        let o: OutputParams = serde_json::from_str(r#"{"preset":"film-master"}"#).unwrap();
        assert_eq!(o.preset, OutputPreset::FilmMaster);
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"preset\":\"film-master\""), "{json}");
        assert_eq!(serde_json::from_str::<OutputParams>(&json).unwrap(), o);
        assert_eq!(
            serde_json::from_str::<OutputParams>(r#"{}"#)
                .unwrap()
                .preset,
            OutputPreset::Legacy
        );
        // The custom `Deserialize` delegates to `parse`, so the recipe key gets the
        // same pinned rename diagnosis as the flag (serde's derived error would only
        // list the accepted variants and never mention the rename).
        let err = serde_json::from_str::<OutputParams>(r#"{"preset":"scene-master"}"#).unwrap_err();
        assert!(err.to_string().contains("renamed"), "{err}");
        assert!(
            serde_json::from_str::<OutputParams>(r#"{"preset":"gain-map-hdr"}"#)
                .unwrap_err()
                .to_string()
                .contains("does not accept yet")
        );
    }

    #[test]
    fn film_master_resolves_f32_independently_of_the_hdr_switch() {
        // The master is unclamped float linear ACEScg *by definition*, so its depth
        // must not depend on `output.hdr` (which stays at its default under the
        // preset — `cli::validate` rejects a non-default one).
        let master = OutputParams {
            preset: OutputPreset::FilmMaster,
            ..OutputParams::default()
        };
        assert_eq!(master.depth(), OutDepth::F32);
        assert_eq!(master.non_default_legacy_selector(), None);
        // Legacy keeps the pre-preset switch semantics exactly.
        assert_eq!(OutputParams::default().depth(), OutDepth::U16);
        assert_eq!(
            OutputParams {
                hdr: true,
                ..OutputParams::default()
            }
            .depth(),
            OutDepth::F32
        );
        // Each legacy selector is individually detected as non-default *and named*
        // — the sweep must blame the offender, not list all three, or a diagnostic
        // test could pass while the wrong selector is reported.
        for (key, value, non_default) in [
            (
                "output.hdr",
                "true",
                OutputParams {
                    hdr: true,
                    ..OutputParams::default()
                },
            ),
            (
                "output.output_profile",
                "srgb",
                OutputParams {
                    output_profile: Some("srgb".into()),
                    ..OutputParams::default()
                },
            ),
            (
                "output.bigtiff",
                "On",
                OutputParams {
                    bigtiff: BigTiff::On,
                    ..OutputParams::default()
                },
            ),
        ] {
            let (name, reported) = non_default
                .non_default_legacy_selector()
                .unwrap_or_else(|| panic!("{non_default:?} must name an offender"));
            assert!(name.contains(key), "{name} should name {key}");
            assert!(reported.contains(value), "{reported} should show {value}");
            // …and only that one: the other two keys must not appear.
            for other in ["output.hdr", "output.output_profile", "output.bigtiff"] {
                assert_eq!(
                    other == key,
                    name.contains(other),
                    "{name} must name {key} and nothing else"
                );
            }
        }
        // All three at their defaults → nothing to blame.
        assert_eq!(OutputParams::default().non_default_legacy_selector(), None);
    }
}
