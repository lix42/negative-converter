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
/// (`--export-ir`) and, since `ir-holder-detection`, consumed by the film-base
/// holder mask — but **only when [`ir_verified`](Self::ir_verified) is true**, and
/// only when the plane measures able to separate holder from film on that frame
/// (`pipeline::film_base::ir_separability`; design-spec §6.1).
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

/// The migration message for the retired `sigmoid` curve, in a recipe.
/// (`--density-curve sigmoid` gets the removed-selector message instead.)
pub const REMOVED_SIGMOID_CURVE: &str = "the `sigmoid` density curve was removed. The \
     `exponential` is its knee-less shape, placed with mid-grey 0.62 density above the \
     film base (`--anchor-mid-offset`, recipe `\"anchor\": {\"mid-at-base-offset\": \
     0.62}`); the sigmoid's reference-based placement retired with the reference density. \
     Highlight roll-off belongs to the display tone (`--display-tone-headroom`). Use \
     `exponential`, or leave the curve unset for the default";

/// Output bit depth for the TIFF paths. Always *resolved* from the preset, by
/// [`OutputParams::depth`], and never stated: the `--out-depth` / `output.depth`
/// knob retired with the `legacy` and `custom` presets, the only two that read it.
///
/// [`Display`](std::fmt::Display) gives the spelling (`u16` / `f32`) diagnostics and
/// the telemetry record use; `{:?}` would print `U16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutDepth {
    /// 16-bit integer. Clamped and rounded at encode.
    #[default]
    U16,
    /// 32-bit float, written verbatim: values above 1.0 survive, and so do
    /// non-finite samples (counted, never laundered).
    F32,
}

impl std::fmt::Display for OutDepth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OutDepth::U16 => "u16",
            OutDepth::F32 => "f32",
        })
    }
}

/// BigTIFF promotion policy for the encoder. Every written image uses `Auto` since
/// `--bigtiff` retired with the `legacy` and `custom` presets; `On` / `Off` remain
/// as the resolved decision handed to the writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
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
    /// A resource limit would be exceeded — today, the memory preflight's
    /// estimated peak allocation against the run's budget
    /// (`pipeline::memory`). Exit 6.
    ///
    /// Distinct from [`Unsupported`](Self::Unsupported) on purpose: the input is
    /// perfectly supported, it is *this run on this budget* that cannot proceed,
    /// and an agent that catches exit 6 knows to retry with `--max-memory` (or on
    /// a bigger machine) rather than give up on the file.
    Resource(String),
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
            NcError::Resource(_) => 6,
        }
    }
}

impl NcError {
    /// The message without the `kind:` prefix [`Display`](std::fmt::Display) adds.
    ///
    /// For composing one error's text into another message: `Display` prefixes the
    /// kind, so `format!("{e} …")` inside a warning carries a stray `usage:`, and
    /// inside a new `NcError` prints it twice.
    ///
    /// **Not a second rendering of the error** — it is the one `Display` is built
    /// from (below), so the two cannot drift. Print an error with `Display`; use
    /// this only to compose its text into a longer message.
    pub fn message(&self) -> &str {
        match self {
            NcError::Usage(m)
            | NcError::Decode(m)
            | NcError::Unsupported(m)
            | NcError::Write(m)
            | NcError::Resource(m)
            | NcError::Other(m) => m,
        }
    }

    /// The `kind:` label [`Display`](std::fmt::Display) prefixes, which is also the
    /// exit-code family (design-spec §11).
    fn kind(&self) -> &'static str {
        match self {
            NcError::Usage(_) => "usage",
            NcError::Decode(_) => "decode",
            NcError::Unsupported(_) => "unsupported",
            NcError::Write(_) => "write",
            NcError::Resource(_) => "resource",
            NcError::Other(_) => "error",
        }
    }
}

impl std::fmt::Display for NcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind(), self.message())
    }
}

impl std::error::Error for NcError {}

#[cfg(test)]
mod error_tests {
    use super::NcError;

    /// `Display` is `kind: message`, and `message()` is exactly the second half —
    /// so composing one error's text into another cannot pick up a stray `usage:`
    /// and the two spellings cannot drift apart.
    #[test]
    fn message_is_display_without_the_kind_prefix() {
        for e in [
            NcError::Usage("u".into()),
            NcError::Decode("d".into()),
            NcError::Unsupported("n".into()),
            NcError::Write("w".into()),
            NcError::Resource("r".into()),
            NcError::Other("o".into()),
        ] {
            let shown = e.to_string();
            let (kind, msg) = shown.split_once(": ").expect("`kind: message`");
            assert_eq!(msg, e.message(), "{shown}");
            assert!(!kind.is_empty() && !kind.contains(' '), "{shown}");
            // Falsifiability: the prefix is real, so this is not a tautology.
            assert_ne!(shown, e.message());
        }
    }
}

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

/// Declared film chemistry (design-spec §9 `input.film_type`, §6.1) — a
/// **provenance declaration that gates nothing today**. **Chromogenic** dyes (C-41
/// colour *and* C-41-process B&W) are transparent to infrared; **silver** halide
/// B&W blocks IR in proportion to accumulated density.
///
/// It used to gate IR-assisted film-holder detection. It no longer does
/// (`ir-usability-detection`): chemistry is the wrong predictor, because
/// separability is a property of the *frame's* density, not the stock's — an
/// unexposed silver frame is IR-transparent against an opaque holder (measured
/// ~20:1) while its own fully-exposed leader is opaque throughout. The two
/// disagree on exactly the frames the calibration workflow uses, so
/// `film_base::ir_separability` measures the plane instead and this declaration
/// takes no part in the decision.
///
/// It is kept as a **shared input-medium declaration** the roadmap still needs:
/// the black & white `bw-support` task (roadmap item 3) for its B&W handling, and
/// the separate IR dust-removal task (roadmap item 1), which gates its defect map
/// on chemistry (silver blocks IR like dust). Whether *that* gate should also be a
/// measurement is an open question for those tasks — dust separability is not the
/// same question as holder separability. Serializes kebab-case (`"unknown"` /
/// `"silver"` / `"chromogenic"`); parsed the same on the CLI via `ValueEnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum FilmType {
    /// Film chemistry not declared (default).
    #[default]
    Unknown,
    /// Silver-halide B&W — silver blocks IR in proportion to accumulated density,
    /// so an *unexposed* frame is still IR-transparent while a leader is opaque.
    Silver,
    /// Chromogenic dye film (C-41 colour or C-41-process B&W) — IR-transparent at
    /// any exposure (measured 0.58-0.73 interior IR transmission over 25 frames,
    /// 9 rolls, leaders included).
    Chromogenic,
}

/// Measurement knobs — where a statistic read off the frame may be read from
/// (design-spec §9, `measure`).
///
/// Its own section rather than a key under `film_base`, because the effective area
/// governs **every** measurement path (`Dmin`, a roll's white balance, content exposure and
/// contrast, tiling uniformity), not just the film base. Filing it under one
/// consumer would misplace it permanently: every recipe struct carries
/// `deny_unknown_fields`, so a key's section is part of its identity.
///
/// Operational-only flags do not belong here — a `measure` key is a conversion
/// knob and must be both a CLI flag and a recipe key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeasureParams {
    /// The static border inset, as a fraction of the **original** frame's shorter
    /// dimension — the second of the effective area's two cuts.
    ///
    /// The holder is cut first, from IR, so this only has to clear a rebate; the
    /// default is [`DEFAULT_MEASURE_INSET`] and the bound is [`MAX_MEASURE_INSET`],
    /// both checked by [`check_measure_inset`]. Where the holder
    /// could **not** be measured (no IR plane, or film too IR-opaque to separate —
    /// routine for silver stock) this is the *only* cut, and the default will
    /// under-clear a real holder. Raising it is then the user's call, deliberately:
    /// nc declines to guess a depth it could not measure, and says in the report
    /// which case a run was in.
    pub inset: f32,
}

impl Default for MeasureParams {
    fn default() -> Self {
        Self {
            inset: DEFAULT_MEASURE_INSET,
        }
    }
}

/// Default static border inset, as a fraction of the **original** frame's shorter
/// dimension — the second of the effective area's two cuts.
///
/// It is sized for a rebate, because the holder is cut first from IR. Where the
/// holder could *not* be measured this same 5% is the only cut, and it may then
/// under-clear the holder and its rebate together: the only directly measured
/// holder depth nc has is **2.5-4% of the shorter edge** (the IR march across 31
/// real frames, `film-base/holder-depth-mask`). The 10-15% figure quoted elsewhere
/// is *not* a depth — it is the holder's share of a rendered frame's top codes
/// (`analysis/conversion-metrics`) — so it does not bound this. Under-clearing is
/// deliberate: nc declines to guess a depth it could not measure, and the user
/// raises the fraction instead (user decision 2026-09-16).
pub const DEFAULT_MEASURE_INSET: f32 = 0.05;

/// Largest accepted [`MeasureParams::inset`]. Insetting half the shorter dimension
/// from *each* side would leave nothing, so the bound sits strictly below 0.5 — a
/// fraction this large is a mistake, not a measurement choice. The bound itself is
/// accepted ([`check_measure_inset`] refuses only `frac > MAX_MEASURE_INSET`).
pub const MAX_MEASURE_INSET: f32 = 0.4;

/// Check a measurement inset fraction, or refuse it.
///
/// The **single** definition of the rule, called from both places that need it:
/// `cli::validate` (so `convert`, `roll` and every per-frame override refuse a bad
/// value before a frame is decoded) and `pipeline::film_base::effective_area` (so a
/// programmatic caller cannot bypass it). Two gates bounding one knob differently
/// is the defect this shape exists to prevent.
pub fn check_measure_inset(frac: f32) -> Result<()> {
    if !frac.is_finite() || frac < 0.0 {
        return Err(NcError::Usage(format!(
            "--measure-inset / measure.inset must be finite and non-negative (got \
             {frac}). It is the fraction of the shorter edge inset from each side \
             after the holder cut; `0` asks for no inset (floored at one \
             holder-probe step on a frame where the holder was measured — see \
             `pipeline::film_base::effective_area`)."
        )));
    }
    if frac > MAX_MEASURE_INSET {
        return Err(NcError::Usage(format!(
            "--measure-inset / measure.inset is {frac}, beyond the supported maximum \
             of {MAX_MEASURE_INSET}. It is inset from *each* side, so {frac} would \
             remove {:.0}% of the shorter dimension; the default is \
             {DEFAULT_MEASURE_INSET}. Measured holder depth is 2.5-4% of the shorter \
             edge (IR march, 31 real frames), so lower the fraction. There is no \
             explicit measurement region to point at instead — `--base-region` sets \
             the film-base source, not the measured area. A conversion measures \
             nothing over the area; `hanten measure-roll` reads it to pool a roll's \
             white balance.",
            (frac * 2.0 * 100.0).min(100.0)
        )));
    }
    Ok(())
}

/// Input / decode knobs (design-spec §9, stage 1).
///
/// Transfer and meaning are **two independent axes** (not a single combined
/// `input.color` choice, which conflated them): the resolver
/// (`pipeline::input_semantics`) resolves each from separate evidence. There is
/// deliberately no `input.color` field — the old combined key is rejected with a
/// migration error at recipe load (see `cli::load_recipe_for`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct InputParams {
    /// Transfer-encoding assertion (default `auto`).
    pub transfer: TransferAssertion,
    /// Measurement-meaning assertion (default `auto`).
    pub meaning: MeaningAssertion,
    /// Declared film chemistry (default `unknown`). Provenance only — it no longer
    /// gates IR-assisted film-holder detection, which measures the IR plane instead
    /// (`ir-usability-detection`). Reserved for the roadmap tasks that still need a
    /// chemistry axis (`bw-support`; IR dust removal). See [`FilmType`].
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
/// (`calibration.film_base = "content"` / `--base-content`) is owned by the separate
/// `film-base/content-fallback` task and is deliberately **not** a variant here —
/// the auto detector only *suggests* it on refusal, never falls back to it.
/// **Deliberately has no `Default`.** `Dmin` is a roll calibration, and picking
/// one silently is the difference between a measured conversion and a guessed
/// one — so `convert` requires the choice to be stated (see
/// [`CalibrationParams::film_base`]). `Auto` remains a perfectly good *stated* answer;
/// what is gone is arriving at it by omission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilmBaseSource {
    /// Estimate the base from the detected unexposed rebate band behind the
    /// film holder (the inward-scan detector; fails loudly on low confidence).
    Auto,
    /// Sample the base from this border region `[x, y, w, h]`.
    Region([u32; 4]),
    /// Explicit per-channel base transmission `[r, g, b]`.
    Explicit([f32; 3]),
}

/// The roll's measured values (design-spec §8, the recipe's `calibration`
/// section) — what was read off *this* film, as opposed to the look that is
/// chosen and reused across rolls. A pipeline profile is a recipe with no
/// `calibration` section; a roll calibration is a recipe with nothing else.
///
/// **Deliberately an open section, not a fixed set.** A key belongs here when it
/// is (a) measured from the film, (b) fixed across the roll, and (c) consumed by
/// a rule that lives elsewhere — the rule stays a look knob, the measurement it
/// reads lives here. The film base is its one member today; the roll reference
/// density (`dmax`) retired with the placements that read it
/// (`nf-retire/dmax-machinery`). The section is expected to grow:
/// `nf-calibration/anchor-comparison` chose a content-referenced roll **white**, and
/// `nf-calibration/roll-white-rule` decides whether it joins this section or only the
/// contrast solved from it is carried.
/// So nothing here may assume a closed pair, and every member carries its own
/// optionality and its own default rather than the section carrying one for all
/// of them.
///
/// **Producing a calibration is not "one frame in, one calibration out".**
/// `film_base` comes from a single reference frame, but a roll content white is read
/// from every frame: the brightest frame's own white under a cap, never a percentile
/// across frames (`nf-calibration/roll-white-rule`). The acquisition
/// cascade that resolves a complete calibration is
/// `core/base-acquisition-planner`; this struct is only its shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CalibrationParams {
    /// Where the film base comes from — **required, with no default**.
    ///
    /// `None` means the user has not chosen, and `cli::validate` rejects it for
    /// `convert`/`roll` rather than silently estimating. The measurement commands
    /// exist to *produce* a base, so requiring one first would be circular:
    /// `estimate` resolves an unstated source to [`FilmBaseSource::Auto`], and
    /// `inspect` never reads this struct at all — it always runs the detector
    /// (`rebate_candidates` + `select_auto_base` directly).
    ///
    /// Why: `Dmin` is a per-roll calibration that sets both the black point and
    /// the colour balance (it is the divisor of the density conversion). Auto
    /// detection is best-effort on real scans — the rebate is a thin inset band,
    /// not the outer margin — so falling into it by omission produced conversions
    /// whose most important parameter nobody had decided. Stating `--auto-base`
    /// is still one flag; the point is that it is now a decision.
    pub film_base: Option<FilmBaseSource>,
}

/// Density-reconstruction knobs (design-spec §9, `reconstruction.density`) —
/// everything that shapes the corrected density `D′` (stages 1–2). The
/// density→positive curve (gamma and the anchor placement) is deliberately **not**
/// here: it belongs to [`ExponentialParams`], the separate curve stage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DensityParams {
    /// Per-channel density gain `[r, g, b]`.
    ///
    /// **Default `[1, 0.84, 0.73]`** — the fixed decode's `algo::fixed::DENSITY_SCALE`.
    /// Both non-unity values are calibrations rather than film constants. The scalar path
    /// leaves `contrast · (D'_c − D'_R)`, so a channel whose density rises faster than red
    /// drifts against it across the tone scale; this gain is what cancels that.
    ///
    /// **Calibrated from 31 hand-marked neutral patches** over five rolls (2026-09-16): each
    /// roll's median nulling scale, averaged with **equal weight per roll**, gives green
    /// 0.837 and blue 0.733. Weighting by roll rather than by patch is the point — 21 of the
    /// 31 patches are September, so a plain corpus median lands at 0.773 / 0.708 and lets one
    /// scan date set the default. `docs/progress/algo.md` (2026-09-16) carries the reasoning.
    ///
    /// Three caveats a caller should know. **Blue is the solid half and green is not**: every
    /// roll measured wants blue 0.68–0.78, so the previous `0.860` — taken from the
    /// manufacturers' published per-channel structure — overcorrects on this scanner, while
    /// green **splits by scan date** (July rolls 0.86–0.90, September ~0.77, consistent with
    /// a change of developer). `0.84` therefore fits neither group exactly and can overshoot
    /// toward green-yellow on a July roll. It nulls per-roll medians, not frames: patch-level
    /// scales span 0.64–0.95 green and 0.40–1.04 blue. And it is calibrated on **one
    /// scanner**, which is why the residual belongs to `io/scanner-density-calibration`
    /// rather than here.
    pub scale: [f32; 3],
    /// Per-channel density offset `[r, g, b]` (orange-mask compensation).
    pub offset: [f32; 3],
}

impl Default for DensityParams {
    fn default() -> Self {
        Self {
            // Read from the fixed decode rather than restated, so the two chains cannot
            // drift apart.
            scale: crate::algo::fixed::DENSITY_SCALE,
            offset: [0.0, 0.0, 0.0],
        }
    }
}

/// Where the print white-balance gains come from (design-spec §9,
/// `print.white_balance`).
///
/// A single mutually-exclusive choice, like [`FilmBaseSource`] —
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
/// The display tone is **not** one of them: it is fit range's, keyed under
/// `fit_range` (`ResolvedConfig::fit_range`), and each named display renderer applies
/// it after the shared stage. Every preset except
/// `film-master` (which bypasses print and display entirely) goes through the shared
/// stage.
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
    /// Black/white-range placement endpoints `[low, high]` in the rendered
    /// positive's linear domain — the exact affine `(x − low)/(high − low)` the
    /// shared display stage applies last (design-spec §6/§9,
    /// `print.linear_range` / `--linear-range LOW,HIGH`). The default `[0, 1]` is
    /// the exact identity. Requires finite `low < high`.
    ///
    /// This is the replacement home for `simple` reconstruction's removed
    /// `clip_low`/`clip_high` endpoints (design-spec §7.1) and is distinct from
    /// the density print `black_point`. Only the shared display stage consumes it:
    /// every display preset accepts a non-default value, while `film-master`
    /// rejects it loudly rather than silently ignoring it.
    pub linear_range: [f32; 2],
}

impl Default for PrintParams {
    fn default() -> Self {
        Self {
            print_exposure: 0.0,
            black_point: 0.0,
            white_balance: WbSource::default(),
            linear_range: [0.0, 1.0],
        }
    }
}

/// Default specular headroom for the display tone and fit range
/// (`fit_range.headroom_stops`), in stops.
///
/// `6` stops is `W = 64`, the value measured to beat the shipped sigmoid on both
/// highlight metrics on all seven fixture frames at matched brightness. `W = 256`
/// scored better on clipped fraction alone but leaves a pre-clamp peak of 1.016 —
/// nothing above diffuse white — which is the condition that makes a gain map inert, so
/// it is deliberately not the default.
pub const DEFAULT_HEADROOM_STOPS: f32 = 6.0;

/// The largest accepted specular headroom, in stops.
///
/// Beyond this the operator is indistinguishable from plain `v/(1 + v)` and the number
/// only looks like a setting. The measured useful range is 4–8 stops.
///
/// Lives here beside the default and [`headroom_white_point`] so the CLI gate and the
/// renderer's `Headroom` cannot bound the knob differently — the failure that pattern
/// produces is a config `cli::validate` accepts and the render then refuses, at exit 1
/// after a whole roll has decoded.
pub const MAX_HEADROOM_STOPS: f32 = 24.0;

/// The white point a specular headroom asks for: `2^stops`.
///
/// The **single** definition. `pipeline::display_tone::Headroom::new` (the current
/// chain's renderers) and `pipeline::fit_range` (the new chain's) both call it, so a
/// change to the stops→white-point meaning cannot move one and leave the other.
pub fn headroom_white_point(stops: f32) -> f32 {
    stops.exp2()
}

/// Which part of the headroom rule a value breaks — see [`headroom_fault`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HeadroomFault {
    /// Negative or non-finite.
    Negative(f32),
    /// Above [`MAX_HEADROOM_STOPS`].
    TooLarge(f32),
}

/// The specular-headroom rule, as data: the **single** definition both chains check.
/// Both refuse through [`headroom_fault_message`], naming the knob as its provenance
/// spells it — the key is `fit_range.headroom_stops` on both.
///
/// A negative headroom is not loud on its own: `2^-40` is a white point of ~9e-13, which
/// maps essentially every sample past the ceiling and turns the render into a solid
/// white field at exit 0 with the clip merely *counted*.
pub fn headroom_fault(stops: f32) -> Option<HeadroomFault> {
    if !stops.is_finite() || stops < 0.0 {
        Some(HeadroomFault::Negative(stops))
    } else if stops > MAX_HEADROOM_STOPS {
        Some(HeadroomFault::TooLarge(stops))
    } else {
        None
    }
}

/// The refusal for a [`HeadroomFault`], with `name` spelling the knob — one wording, so
/// the same bad value is explained the same way on both chains.
pub fn headroom_fault_message(fault: HeadroomFault, name: &str) -> String {
    match fault {
        HeadroomFault::Negative(stops) => format!(
            "{name} must be finite and non-negative (got {stops}). It is the specular \
             headroom above reference white the display tone keeps distinguishable, in \
             stops; `0` is the identity."
        ),
        HeadroomFault::TooLarge(stops) => format!(
            "{name} is {stops} stops, beyond the supported maximum of {MAX_HEADROOM_STOPS}. \
             Above ~8 stops the operator converges on plain Reinhard and the extra headroom \
             buys nothing; the measured useful range is 4–8 (the default \
             {DEFAULT_HEADROOM_STOPS} is a white point of {}).",
            headroom_white_point(DEFAULT_HEADROOM_STOPS)
        ),
    }
}

/// Check the current chain's specular headroom in stops, or refuse it.
///
/// [`headroom_fault`]'s rule, called from both gates that need it: `cli::validate` (so
/// `roll` and every per-frame override inherit it *before* a decode) and
/// `pipeline::display_tone::Headroom::new` (so a stage caller cannot skip it). The
/// stage check is deliberately a duplicate, not a fallback — see that constructor.
pub fn check_headroom_stops(stops: f32) -> Result<()> {
    match headroom_fault(stops) {
        None => Ok(()),
        Some(fault) => Err(NcError::Usage(headroom_fault_message(
            fault,
            "--display-tone-headroom / fit_range.headroom_stops",
        ))),
    }
}

/// The density→positive curve (design-spec §7.2/§9, `reconstruction.curve`): the
/// straight-line `10^(gamma·(D′ − A))` mapping, with the anchor `A` placed by
/// [`AnchorPlacement`].
///
/// The only curve since `nf-retire/characteristic`, so it is a plain struct rather than
/// a tagged enum and the `type` key is no longer written. Every earlier sidecar carries
/// `"type": "exponential"`, which the deserializer accepts and drops; the retired curves
/// (`sigmoid`, `characteristic`) and their keys are refused by name.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ExponentialParams {
    /// Film/print curve gamma (the straight line's slope).
    pub gamma: f32,
    /// Which tone is pinned, and at what density.
    pub anchor: AnchorPlacement,
}

impl Default for ExponentialParams {
    /// **The fixed decode's configuration** (`pipeline_version` 6): contrast 2.0 and
    /// mid-grey pinned 0.62 density above the film base, read from `algo::fixed` so the
    /// two chains cannot drift apart. `algo::fixed`'s
    /// `the_fixed_decode_matches_the_equivalent_legacy_configuration` pins that this
    /// curve renders it bit-identically.
    ///
    /// The slope is the **bundled** `gamma` (`algo::fixed::BUNDLED_CONTRAST`), not the
    /// new chain's linearization: this chain has no look stage to carry the print
    /// contrast `nf-reconstruction/gamma-split` moved there, so it keeps both halves in
    /// one number until `nf-core/default-flip` retires it.
    fn default() -> Self {
        Self {
            gamma: crate::algo::fixed::BUNDLED_CONTRAST,
            anchor: AnchorPlacement::MidAtBaseOffset(crate::algo::fixed::MID_ABOVE_BASE),
        }
    }
}

/// Output decades between mid-grey and display white: `−log10(0.18)`.
///
/// A mid-grey card reflects ~18% of the light falling on it, so on a correctly exposed
/// display-referred image it belongs at 0.18 of white — about 2.5 stops down. This is a
/// property of what "18% reflectance" means, not a tunable.
pub const MID_GREY_OUTPUT_DECADES: f32 = 0.744_727_5;

/// Density between a mid-grey card and a diffuse white on a correctly exposed colour
/// negative, from the manufacturers' own aim tables.
///
/// Every Kodak colour-negative datasheet carries the same *Judging Negative Exposures*
/// table (Status M, red channel, for "a normally exposed and processed color negative").
/// The absolute values differ per stock, but their **difference is essentially constant**:
/// Ektar 100, Portra 160 and Portra 400 all give 0.36 (Gold 200, a consumer stock, 0.40).
/// Sources: Kodak E-4046, E-4051, E-4050, E-7022.
///
/// A per-stock value belongs to `algo/film-stock-profiles`; until that registry exists this
/// single professional-film figure is the reference.
#[cfg(test)]
pub const REFERENCE_MID_TO_WHITE_DELTA: f32 = 0.36;

/// Which tone the exponential pins, and at what density (design-spec §7.2/§9,
/// `reconstruction.curve.anchor`).
///
/// The curve is an affine map in log space: a slope ([`ExponentialParams::gamma`]) plus
/// **one** pinned `(density, output)` pair, from which everything else follows.
///
/// One rule is left, and it is **reference-free**: it reads the film base (which stage 1
/// divides out, so the base *is* `D′ = 0`) and never a leader-measured reference
/// density. That is what keeps a leader's roll-to-roll error — two rolls of one stock
/// measured 0.295 apart while their bases agreed to 0.0005 — out of the render. The
/// three placements that read a reference, or pinned black instead of mid, retired with
/// the reference (`nf-retire/dmax-machinery`); a recipe naming one is refused by
/// [`REMOVED_ANCHOR_PLACEMENTS`].
///
/// Still an enum, and still serialized as a tagged object — the form a content-referenced
/// placement would need to carry its own measured value. `nf-calibration/anchor-comparison`
/// placed the roll's white through the look's contrast instead, so none is planned.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnchorPlacement {
    /// Pin **mid-grey** (output 0.18) at `offset` density *above the film base*,
    /// letting white fall where the contrast puts it — `A = offset + 0.745/contrast`.
    MidAtBaseOffset(f32),
}

/// The placements `reconstruction.curve.anchor` no longer accepts, by wire name, with
/// what each did — so a recipe naming one gets a migration error rather than serde's
/// bare "unknown variant".
pub const REMOVED_ANCHOR_PLACEMENTS: [(&str, &str); 3] = [
    (
        "white-at-dmax",
        "pinned display white at the reference density",
    ),
    (
        "mid-at-dmax-fraction",
        "pinned mid-grey at a fraction of the reference density",
    ),
    ("black-at-base", "pinned the film base to an output floor"),
];

impl AnchorPlacement {
    /// The curve's anchor `A` — the corrected density that renders to `1.0` — at this
    /// contrast. Pinning mid-grey at `M` solves `10^(contrast·(M − A)) = 0.18`, i.e.
    /// `A = M + 0.745/contrast`.
    pub fn anchor(self, contrast: f32) -> f32 {
        match self {
            AnchorPlacement::MidAtBaseOffset(offset) => offset + MID_GREY_OUTPUT_DECADES / contrast,
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

/// The migration message for the retired `characteristic` curve and everything that
/// selected it (`--film-stock`, the `--preset` names), so every provenance says the same
/// thing.
///
/// The `density.scale` clause is what makes the remedy work: that curve defaulted the
/// gain to `[1, 1, 1]`, and a sidecar writes every key, so dropping only the curve would
/// replay the exponential at the identity gain rather than at its calibrated default.
pub const REMOVED_CHARACTERISTIC_CURVE: &str = "the `characteristic` density curve (inverting \
     a film stock's published curve) was removed: the fixed decode is the only \
     reconstruction, and it is stock-agnostic. Remove `reconstruction.curve` for the \
     default curve, and also remove the `reconstruction.density.scale` of `[1, 1, 1]` a \
     sidecar carries — that was the characteristic curve's own default, where the \
     decode's is `[1, 0.84, 0.73]`. The old \
     render is reproducible only from the reference build (`scripts/reference-snapshot/`)";

impl<'de> Deserialize<'de> for ExponentialParams {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        // Capture the raw object so a retired key is detected by *presence*, not by its
        // value: a plain `Option<f32>` field turns an explicit `null` into `None`,
        // indistinguishable from an absent key.
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("reconstruction.curve must be a JSON object"))?;

        // Before the unknown-field scan: `dmax` used to live here, so "unknown field" is
        // true and useless. A recipe written against the old schema needs the path it
        // moved to (the more specific diagnosis goes first). No aliases.
        if obj.contains_key("dmax") {
            return Err(D::Error::custom(
                "`dmax` is no longer a `reconstruction.curve` key: the roll reference \
                 density retired with the placements that read it. The curve's one \
                 placement, `\"anchor\": {\"mid-at-base-offset\": <d>}`, reads only the \
                 film base. Remove the key.",
            ));
        }

        const KNOWN: [&str; 7] = [
            "type", "gamma", "anchor", "contrast", "toe", "shoulder", "stock",
        ];
        if let Some(k) = obj.keys().find(|k| !KNOWN.contains(&k.as_str())) {
            return Err(D::Error::custom(format!(
                "unknown field `{k}` in reconstruction.curve"
            )));
        }

        // A retired curve before its retired keys: a sigmoid recipe carries both, and the
        // curve is the more specific diagnosis.
        let curve_type = obj.get("type");
        if curve_type.and_then(serde_json::Value::as_str) == Some("sigmoid") {
            return Err(D::Error::custom(format!(
                "reconstruction.curve: {REMOVED_SIGMOID_CURVE}"
            )));
        }
        if curve_type.and_then(serde_json::Value::as_str) == Some("characteristic")
            || obj.contains_key("stock")
        {
            return Err(D::Error::custom(format!(
                "reconstruction.curve: {REMOVED_CHARACTERISTIC_CURVE}"
            )));
        }
        if let Some(key) = ["contrast", "toe", "shoulder"]
            .into_iter()
            .find(|k| obj.contains_key(*k))
        {
            return Err(D::Error::custom(format!(
                "`{key}` was a sigmoid-curve key and was removed with the sigmoid. The \
                 exponential's slope is `gamma`; highlight roll-off belongs to fit range \
                 (`fit_range.headroom_stops`)"
            )));
        }
        // The retired selector: its one surviving value is what every earlier sidecar
        // wrote, so it asks for nothing and is dropped; anything else is refused.
        match curve_type {
            None => {}
            Some(serde_json::Value::String(t)) if t == "exponential" => {}
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "reconstruction.curve.type is no longer a selector (the exponential is \
                     the only curve); remove the key — got {other}"
                )));
            }
        }

        if let Some(message) = obj.get("anchor").and_then(removed_anchor_message) {
            return Err(D::Error::custom(message));
        }
        let d = ExponentialParams::default();
        Ok(ExponentialParams {
            gamma: take_recipe_field(obj, "gamma")
                .map_err(D::Error::custom)?
                .unwrap_or(d.gamma),
            anchor: take_recipe_field(obj, "anchor")
                .map_err(D::Error::custom)?
                .unwrap_or(d.anchor),
        })
    }
}

/// The migration error for a `reconstruction.curve.anchor` naming a retired
/// placement, spelled either as a bare name or as a `{name: value}` object.
fn removed_anchor_message(anchor: &serde_json::Value) -> Option<String> {
    let name = match anchor {
        serde_json::Value::String(name) => name.as_str(),
        serde_json::Value::Object(obj) if obj.len() == 1 => obj.keys().next()?.as_str(),
        _ => return None,
    };
    let (name, what) = REMOVED_ANCHOR_PLACEMENTS
        .iter()
        .find(|(removed, _)| *removed == name)?;
    Some(format!(
        "reconstruction.curve.anchor `{name}` was removed: it {what}. The one placement \
         left is `{{\"mid-at-base-offset\": <d>}}` (flag `--anchor-mid-offset`), mid-grey \
         pinned `d` density above the film base"
    ))
}

/// Wire schema version of the tagged `reconstruction` recipe/report object
/// (design-spec §8). Versions the **schema shape only** — it is not the
/// behavioral `pipeline_version` (owned by the `conversion-versioning` task,
/// bumped only when default pixels change). Every resolved recipe/report emits
/// it; partial input may omit it (defaults to this value); any other value is
/// rejected loudly.
pub const RECONSTRUCTION_SCHEMA_VERSION: u32 = 1;

/// The reconstruction configuration (design-spec §8/§9, the recipe's one
/// `reconstruction` object): density correction plus the curve.
///
/// Until `nf-retire/sigmoid-and-simple` this was an enum selecting `simple` (the
/// direct inversion) or `density`; with `simple` gone there is one reconstruction, so
/// the `type` key is no longer written. Every earlier sidecar carries
/// `"type": "density"`, so that value is accepted and ignored on load; `"simple"` is
/// refused with a migration error. Omitted sections fill their defaults, so omission
/// never survives into a resolved recipe or report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reconstruction {
    /// Density correction (stages 1–2).
    pub density: DensityParams,
    /// The density→positive curve.
    pub curve: ExponentialParams,
}

impl Serialize for Reconstruction {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("Reconstruction", 3)?;
        st.serialize_field("schema_version", &RECONSTRUCTION_SCHEMA_VERSION)?;
        st.serialize_field("density", &self.density)?;
        st.serialize_field("curve", &self.curve)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for Reconstruction {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

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

        // The retired selector: its old default is what every earlier sidecar wrote,
        // so it asked for nothing and is dropped; anything else is refused by name.
        match obj.get("type") {
            None => {}
            Some(serde_json::Value::String(t)) if t == "density" => {}
            Some(serde_json::Value::String(t)) if t == "simple" => {
                return Err(D::Error::custom(REMOVED_SIMPLE_RECONSTRUCTION));
            }
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "reconstruction.type is no longer a selector (density is the only \
                     reconstruction); remove the key — got {other}"
                )));
            }
        }

        // `DensityParams` is a plain derive, so serde would also accept a positional
        // array; the recipe's sections are objects (design-spec §9), so refuse the shape
        // rather than read an unlabelled triple pair.
        if let Some(v) = obj.get("density")
            && !v.is_object()
        {
            return Err(D::Error::custom(
                "reconstruction.density must be a JSON object",
            ));
        }
        let density: DensityParams = take_recipe_field(obj, "density")
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        let curve: ExponentialParams = take_recipe_field(obj, "curve")
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        Ok(Reconstruction { density, curve })
    }
}

/// The migration message for the retired `simple` reconstruction, shared by the
/// recipe deserializer and the `--reconstruction` flag.
pub const REMOVED_SIMPLE_RECONSTRUCTION: &str = "`simple` reconstruction (the direct \
     `1 − scan/Dmin` inversion) was removed: it is an affine inversion of the scan rather \
     than a decode of the film, and density is now the only reconstruction. Remove \
     `reconstruction.type` / `--reconstruction`; the default density reconstruction \
     applies";

/// What the encode stage observed while writing — fed into the JSON report by
/// the orchestrator. Records two kinds of trouble the output samples can carry,
/// since no colour stage clamps and the density-domain
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
/// One mutually-exclusive enum field, like [`FilmBaseSource`]:
/// a preset resolves a whole coherent policy, so it can never be a bag of
/// independent bools. Serializes kebab-case (`"gain-map-hdr"` / `"film-master"`).
///
/// **Ten variants are accepted** — `film-master`, `gain-map-hdr`, `ultra-hdr-v1`,
/// `display-p3`, `compatibility`, `hdr-pq`, `hdr-hlg`, `hdr-linear-tiff`,
/// `hdr-pq-tiff` and `hdr-hlg-tiff`, enumerated once in [`ALL`](Self::ALL). Every
/// one is atomic: it resolves container, depth and profile itself.
/// [`parse`](Self::parse) rejects the removed `legacy` and `custom` and the
/// pre-release `scene-master` with migration messages rather than a generic
/// unknown-value error.
///
/// Keep this list in step with `parse`, [`ALL`](Self::ALL), and
/// `OutputOverrides::output_preset`'s help text, which is what `--help` shows. The
/// diagnostics do not restate it: they are generated from `ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OutputPreset {
    /// **`film-master`** — an unclamped 32-bit float linear ACEScg TIFF taken
    /// **directly** from the NC film RGB v1 mapping. It preserves the intentional
    /// film, lens, development, scanner, reconstruction, and density-curve
    /// rendering (including its anchor placement) and bypasses
    /// every later white-balance, exposure, black/range-placement, highlight,
    /// display tone, gamut, and transfer operation. It is **not** a physical
    /// scene-linear recovery.
    ///
    /// The bypass is strict, not silent: `cli::validate` rejects every
    /// non-default downstream control, whatever its source. A linear float export that *wants* the print and display controls
    /// applied is [`HdrLinearTiff`](Self::HdrLinearTiff).
    FilmMaster,
    /// **`ultra-hdr-v1`** — an explicitly legacy Ultra HDR v1 JPEG: an
    /// SDR Display P3 base image plus a luminance gain-map JPEG and XMP/MPF
    /// metadata. This name deliberately does not claim ISO 21496-1 conformance.
    ///
    /// Retained as the *compatibility* gain-map output beside
    /// [`GainMapHdr`](Self::GainMapHdr), not superseded by it: this is the file to
    /// write for a decoder that reads only Google's dialect. Its bytes are frozen —
    /// a test asserts they contain no `21496`.
    ///
    /// **It is not HDR on Apple platforms** (measured 2026-08-06): ImageIO ignores
    /// the legacy XMP entirely and opens the file as an ordinary SDR JPEG.
    UltraHdrV1,
    /// **`gain-map-hdr`** — the same gain-map JPEG carrying **both** metadata
    /// dialects: Google's legacy Ultra HDR v1 XMP/MPF *and* ISO 21496-1 segments in
    /// both images, describing the one shared luminance gain map
    /// (`io::ultra_hdr::Dialects::LegacyPlusIso`).
    ///
    /// Dual-dialect is the whole point of the name rather than a refinement of
    /// [`UltraHdrV1`](Self::UltraHdrV1): Apple reads only the ISO dialect and
    /// Android 15+ reads both, so this is the one gain-map file that is actually
    /// HDR on Apple platforms. The pixels are identical to `ultra-hdr-v1`'s — the
    /// two presets differ **only** in the metadata segments attached.
    ///
    /// ISO 21496-1 is silent on coexistence with the legacy XMP, so which dialect a
    /// dual-aware decoder prefers is *observed behaviour*, never a conformance
    /// claim.
    ///
    /// **The product default** since `pipeline_version` 3. Being a named preset it is
    /// atomic and requires a `.jpg`/`.jpeg` output path — so `hanten convert -o out.tif`
    /// with no preset is now a usage error naming the accepted suffixes, where it
    /// previously wrote a 16-bit TIFF. That is the documented cost of the migration,
    /// not an oversight: an unnamed output policy silently changing container would
    /// be worse.
    #[default]
    GainMapHdr,
    /// **`hdr-pq`** — a single-rendition 10-bit 4:4:4 AVIF carrying Rec.2100 PQ
    /// (CICP 9/16/9, full range) with a 203 cd/m² reference white and 1000 cd/m²
    /// mastering peak. Written by `io::avif`; requires an `.avif` output path.
    HdrPq,
    /// **`hdr-hlg`** — the same container and coding as [`HdrPq`](Self::HdrPq) but
    /// Rec.2100 HLG (CICP 9/18/9) with the reference 1000-nit, zero-black OOTF at
    /// system gamma 1.2. Being display-referred, it carries no absolute
    /// content-light metadata.
    HdrHlg,
    /// **`hdr-linear-tiff`** — the display-linear HDR *interchange* master: an
    /// unclamped 32-bit float TIFF holding the HDR renderer's pre-transfer
    /// BT.2020/D65 samples verbatim, with a synthesized linear-BT.2020 ICC
    /// profile. Requires a `.tif`/`.tiff` output path.
    ///
    /// Distinct from all three neighbours, and the distinctions are the point:
    /// - **not** [`FilmMaster`](Self::FilmMaster) — that is linear ACEScg *before*
    ///   any display rendering, whereas this has been through the shared print
    ///   controls, the display tone, and BT.2020 gamut mapping;
    /// - **not** [`HdrPq`](Self::HdrPq)/[`HdrHlg`](Self::HdrHlg) — no transfer
    ///   function has been applied, so these are linear luminance values, not
    ///   Rec.2100 code values.
    ///
    /// Samples are reference-white-relative: `1.0` is the 203 cd/m² reference
    /// white and highlights legitimately reach the 1000 cd/m² peak at
    /// `pipeline::hdr::LINEAR_HEADROOM` (≈4.926108). Nothing is clamped, so the
    /// embedded ICC — whose PCS stops at 1.0 — cannot by itself convey those
    /// luminance semantics; the report and sidecar are authoritative for them.
    HdrLinearTiff,
    /// **`hdr-pq-tiff`** — the Rec.2100 PQ signal stored as full-range 16-bit TIFF
    /// code values, with an extended-range BT.2020 ICC profile carrying the
    /// `cicp` 9-16-0-1 tag. Requires a `.tif`/`.tiff` output path.
    ///
    /// Lossless *relative to the quantized signal*: the renderer's normalized
    /// output is quantized once with one pinned rounding rule and TIFF stores every
    /// resulting code exactly, with the measured max/RMS quantization error
    /// reported. **16 bits is TIFF's quantization, not one of BT.2100's own bit
    /// depths** (it specifies 10 and 12), so the file carries BT.2100's transfer
    /// function at TIFF's precision — the report says exactly that rather than
    /// implying a Rec.2100 system claim.
    HdrPqTiff,
    /// **`hdr-hlg-tiff`** — as [`HdrPqTiff`](Self::HdrPqTiff) but the HLG transfer,
    /// with `cicp` 9-18-0-1. Its ICC profile is deliberately **scene-referred**
    /// (HLG's OOTF is not per-channel separable, so no 1D curve set can express
    /// it); the display-referred contract lives in the report.
    HdrHlgTiff,
    /// **`display-p3`** — a 16-bit integer SDR TIFF in Display P3, rendered through
    /// the modern display stage (NC film RGB v1 → linear ACEScg → the shared print
    /// controls → `pipeline::sdr`, including its display tone and gamut mapping).
    /// Requires `.tif`/`.tiff`.
    DisplayP3,
    /// **`compatibility`** — the same modern SDR render as
    /// [`DisplayP3`](Self::DisplayP3), in **sRGB**: the widest-support output nc
    /// writes, and lossless (16-bit integer, no lossy codec). Requires
    /// `.tif`/`.tiff`.
    Compatibility,
}

impl OutputPreset {
    /// Parse the `--output-preset` value / `output.preset` recipe key. Shared by
    /// the CLI merge and the custom [`Deserialize`] below so a name gets the same
    /// diagnosis wherever it appears (the `OutputSpace::parse` precedent).
    pub fn parse(s: &str) -> Result<Self> {
        // Case-insensitive like `OutputSpace::parse`: these are keywords, not paths.
        match s.trim().to_ascii_lowercase().as_str() {
            "film-master" => Ok(OutputPreset::FilmMaster),
            "ultra-hdr-v1" => Ok(OutputPreset::UltraHdrV1),
            "gain-map-hdr" => Ok(OutputPreset::GainMapHdr),
            "hdr-pq" => Ok(OutputPreset::HdrPq),
            "hdr-hlg" => Ok(OutputPreset::HdrHlg),
            "hdr-linear-tiff" => Ok(OutputPreset::HdrLinearTiff),
            "hdr-pq-tiff" => Ok(OutputPreset::HdrPqTiff),
            "hdr-hlg-tiff" => Ok(OutputPreset::HdrHlgTiff),
            "display-p3" => Ok(OutputPreset::DisplayP3),
            "compatibility" => Ok(OutputPreset::Compatibility),
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
            // Retired with the legacy print path (`nf-retire/legacy-custom`). nc is
            // unreleased, so a recipe naming either is a schema break, not an alias.
            removed @ ("legacy" | "custom") => Err(NcError::Usage(format!(
                "output preset `{removed}` was removed together with the legacy print \
                 path{} — for a 16-bit TIFF use `display-p3` (or `compatibility` for \
                 sRGB); for a float TIFF, `film-master` (linear ACEScg before display \
                 rendering) or `hdr-linear-tiff` (display-linear BT.2020). The old \
                 rendering is reproducible only from the reference build \
                 (`scripts/reference-snapshot/`). There is no alias.",
                if removed == "custom" {
                    " and the depth/profile/container selectors it existed to accept"
                } else {
                    ""
                }
            ))),
            other => Err(NcError::Usage(format!(
                "unknown output preset `{other}` — accepted: {}",
                Self::accepted_list()
            ))),
        }
    }

    /// Every preset this build accepts, in help order.
    ///
    /// Diagnostics are generated from this list rather than restating it, because
    /// two hand-written "accepted: …" lists both went stale the moment a preset
    /// shipped — and a stale list hides exactly the name the user was reaching for.
    pub const ALL: [OutputPreset; 10] = [
        OutputPreset::FilmMaster,
        OutputPreset::GainMapHdr,
        OutputPreset::UltraHdrV1,
        OutputPreset::DisplayP3,
        OutputPreset::Compatibility,
        OutputPreset::HdrPq,
        OutputPreset::HdrHlg,
        OutputPreset::HdrLinearTiff,
        OutputPreset::HdrPqTiff,
        OutputPreset::HdrHlgTiff,
    ];

    /// The accepted names as a comma-separated backticked list, for diagnostics.
    fn accepted_list() -> String {
        Self::ALL
            .iter()
            .map(|p| format!("`{}`", p.name()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The preset's stable wire / CLI name — the same string [`parse`](Self::parse)
    /// accepts and `Serialize` emits. Diagnostics take the name from here rather
    /// than hardcoding a literal, so a message about the *next* named preset can
    /// never end up describing `film-master`.
    pub fn name(self) -> &'static str {
        match self {
            OutputPreset::FilmMaster => "film-master",
            OutputPreset::UltraHdrV1 => "ultra-hdr-v1",
            OutputPreset::GainMapHdr => "gain-map-hdr",
            OutputPreset::HdrPq => "hdr-pq",
            OutputPreset::HdrHlg => "hdr-hlg",
            OutputPreset::HdrLinearTiff => "hdr-linear-tiff",
            OutputPreset::HdrPqTiff => "hdr-pq-tiff",
            OutputPreset::HdrHlgTiff => "hdr-hlg-tiff",
            OutputPreset::DisplayP3 => "display-p3",
            OutputPreset::Compatibility => "compatibility",
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
///
/// For a lossy JPEG output this is the normalized 8-bit primary-image buffer
/// handed to the compressor, not a decoder-dependent measurement after JPEG
/// reconstruction. That keeps the comparison basis deterministic without
/// pretending the codec preserves exact samples.
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
///
/// Only the preset: every preset is atomic, so container, depth and profile are
/// *resolved* from it ([`depth`](Self::depth)) rather than stated. The
/// `output.depth` / `output.output_profile` / `output.bigtiff` selectors retired with
/// the `legacy` and `custom` presets, and `cli::reject_legacy_recipe_keys` names that
/// migration for a recipe still carrying one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct OutputParams {
    /// Named output preset (default `gain-map-hdr`). Selects the branch out of the
    /// ACEScg boundary and the container/depth/profile policy.
    pub preset: OutputPreset,
}

impl OutputParams {
    /// The encoder bit depth this output resolves to. The single place a recipe
    /// value becomes a depth, so encode, IR export, and color can't disagree:
    ///
    /// - `film-master` is **always** [`OutDepth::F32`] — the master is unclamped
    ///   float linear ACEScg by definition.
    /// - the gain-map presets resolve [`OutDepth::U16`] only for optional IR TIFF
    ///   export; their primary image is fixed 8-bit JPEG.
    /// - `hdr-pq` / `hdr-hlg` likewise resolve [`OutDepth::U16`] only for the IR
    ///   TIFF; their primary image is fixed 10-bit AVIF.
    pub fn depth(&self) -> OutDepth {
        match self.preset {
            // Both are unclamped 32-bit float TIFFs.
            OutputPreset::FilmMaster | OutputPreset::HdrLinearTiff => OutDepth::F32,
            // Used only by the optional IR TIFF export. The primary image's depth
            // is fixed by the preset — 8-bit JPEG, or 10-bit AVIF.
            // `hdr-*-tiff` resolves u16 for the primary *and* the optional IR
            // plane; the AVIF/JPEG presets only use it for IR.
            OutputPreset::UltraHdrV1
            | OutputPreset::GainMapHdr
            | OutputPreset::HdrPq
            | OutputPreset::HdrHlg
            | OutputPreset::HdrPqTiff
            | OutputPreset::HdrHlgTiff
            // 16-bit integer for the primary image and any IR plane: "losslessly
            // stored SDR" is the point, and a float SDR TIFF is precision nothing
            // can display.
            | OutputPreset::DisplayP3
            | OutputPreset::Compatibility => OutDepth::U16,
        }
    }

    /// The **primary image's** bit depth, as a label for the telemetry record.
    ///
    /// Deliberately *not* [`depth`](Self::depth), which answers a different question:
    /// for the JPEG and AVIF presets that value is only the optional IR *TIFF*'s
    /// depth, so recording it verbatim labelled a gain-map run `u16` when its primary
    /// is a fixed 8-bit JPEG. The container fixes these, so they are constants rather
    /// than anything resolved.
    pub fn primary_depth_label(&self) -> &'static str {
        match self.preset {
            // Fixed by the container.
            OutputPreset::GainMapHdr | OutputPreset::UltraHdrV1 => "u8",
            OutputPreset::HdrPq | OutputPreset::HdrHlg => "u10",
            // TIFF presets: the primary really is what `depth()` resolves.
            OutputPreset::FilmMaster
            | OutputPreset::DisplayP3
            | OutputPreset::Compatibility
            | OutputPreset::HdrLinearTiff
            | OutputPreset::HdrPqTiff
            | OutputPreset::HdrHlgTiff => match self.depth() {
                OutDepth::U16 => "u16",
                OutDepth::F32 => "f32",
            },
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
        assert_eq!(NcError::Resource(String::new()).exit_code(), 6);
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
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: DensityParams = serde_json::from_str(&json).unwrap();
        assert_eq!(params, back);
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
    fn every_preset_resolves_its_own_depth() {
        // No knob states a depth any more: the two float TIFFs resolve f32 and every
        // other preset u16 (for its primary, or only for the optional IR TIFF).
        for preset in OutputPreset::ALL {
            let want = match preset {
                OutputPreset::FilmMaster | OutputPreset::HdrLinearTiff => OutDepth::F32,
                _ => OutDepth::U16,
            };
            assert_eq!(OutputParams { preset }.depth(), want, "{}", preset.name());
        }
    }

    #[test]
    fn the_anchor_pins_mid_grey_above_the_base() {
        let c = 2.0f32;
        let off = AnchorPlacement::MidAtBaseOffset(0.5).anchor(c);
        assert!(
            (off - (0.5 + MID_GREY_OUTPUT_DECADES / c)).abs() < 1e-6,
            "{off}"
        );
    }

    /// A recipe naming a retired placement gets the migration error, in both of the
    /// spellings serde wrote (a bare unit-variant name, a `{name: value}` object), and
    /// the error names the one placement left.
    #[test]
    fn a_retired_anchor_placement_is_a_migration_error() {
        for anchor in [
            r#""white-at-dmax""#,
            r#"{"mid-at-dmax-fraction": 0.5}"#,
            r#"{"black-at-base": 0.005}"#,
        ] {
            let json = format!(r#"{{"type": "exponential", "anchor": {anchor}}}"#);
            let err = serde_json::from_str::<ExponentialParams>(&json)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("was removed") && err.contains("mid-at-base-offset"),
                "{anchor}: {err}"
            );
        }
        let kept = serde_json::from_str::<ExponentialParams>(
            r#"{"type": "exponential", "anchor": {"mid-at-base-offset": 0.5}}"#,
        )
        .unwrap();
        assert_eq!(kept.anchor, AnchorPlacement::MidAtBaseOffset(0.5));
    }

    #[test]
    fn the_curve_round_trips_without_a_type_tag() {
        let curve = ExponentialParams {
            gamma: 1.4,
            anchor: AnchorPlacement::MidAtBaseOffset(0.5),
        };
        let json = serde_json::to_string(&curve).unwrap();
        assert_eq!(
            serde_json::from_str::<ExponentialParams>(&json).unwrap(),
            curve
        );
        // The default is the fixed decode's configuration, and the retired tag is not
        // written.
        assert_eq!(
            serde_json::to_string(&ExponentialParams::default()).unwrap(),
            r#"{"gamma":2.0,"anchor":{"mid-at-base-offset":0.62}}"#
        );
    }

    #[test]
    fn the_retired_exponential_tag_is_accepted_and_partial_input_fills_defaults() {
        // Every sidecar before the collapse wrote `"type": "exponential"`, so it asks for
        // nothing and must still replay — alone, or beside a partial curve.
        let want = ExponentialParams {
            gamma: 1.5,
            ..ExponentialParams::default()
        };
        for json in [r#"{"type":"exponential","gamma":1.5}"#, r#"{"gamma":1.5}"#] {
            assert_eq!(
                serde_json::from_str::<ExponentialParams>(json).unwrap(),
                want,
                "{json}"
            );
        }
        assert_eq!(
            serde_json::from_str::<ExponentialParams>(r#"{"type":"exponential"}"#).unwrap(),
            ExponentialParams::default()
        );
        // A null or unknown tag is refused, never read as absent.
        for json in [r#"{"type":null}"#, r#"{"type":"linear"}"#] {
            let err = serde_json::from_str::<ExponentialParams>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no longer a selector"), "{json}: {err}");
        }
    }

    /// The retired `characteristic` curve is refused by name — its tag or its one key,
    /// either of which a sidecar carries — and the message names the `density.scale`
    /// that its sidecars also carry at the curve's own identity default.
    #[test]
    fn the_retired_characteristic_curve_is_refused_by_name() {
        for json in [
            r#"{"type":"characteristic"}"#,
            r#"{"type":"characteristic","stock":"portra-400"}"#,
            r#"{"stock":"generic-c41"}"#,
            r#"{"type":"exponential","stock":"portra-400"}"#,
            r#"{"type":"characteristic","gamma":2.0}"#,
        ] {
            let err = serde_json::from_str::<ExponentialParams>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains(REMOVED_CHARACTERISTIC_CURVE), "{json}: {err}");
        }
        assert!(REMOVED_CHARACTERISTIC_CURVE.contains("reconstruction.density.scale"));
    }

    #[test]
    fn the_retired_sigmoid_is_refused_by_name() {
        // The curve itself, with or without its keys — the curve is the more specific
        // diagnosis, so it wins over the retired-key one.
        for json in [
            r#"{"type":"sigmoid"}"#,
            r#"{"type":"sigmoid","contrast":2.0,"toe":0.2}"#,
        ] {
            let err = serde_json::from_str::<ExponentialParams>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains(REMOVED_SIGMOID_CURVE), "{json}: {err}");
            assert!(
                err.contains("exponential"),
                "must name the replacement: {err}"
            );
        }
        // Its keys are named, not a generic unknown field — including a present-but-null
        // one, which must be caught by presence.
        for json in [
            r#"{"type":"exponential","contrast":2.0}"#,
            r#"{"toe":0.1}"#,
            r#"{"type":"exponential","shoulder":null}"#,
        ] {
            let err = serde_json::from_str::<ExponentialParams>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains("removed with the sigmoid"), "{json}: {err}");
        }
        // A key belonging to no curve, ever, is still an unknown field.
        assert!(serde_json::from_str::<ExponentialParams>(r#"{"gama":1.2}"#).is_err());
    }

    #[test]
    fn the_retired_simple_reconstruction_is_refused_by_name() {
        for json in [r#"{"type":"simple"}"#, r#"{"type":"simple","density":{}}"#] {
            let err = serde_json::from_str::<Reconstruction>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains(REMOVED_SIMPLE_RECONSTRUCTION), "{json}: {err}");
        }
        // A null or unknown selector is refused too, never read as absent.
        for json in [r#"{"type":null}"#, r#"{"type":"sigmoid"}"#] {
            assert!(
                serde_json::from_str::<Reconstruction>(json).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn the_retired_density_type_tag_is_accepted_at_its_old_value() {
        // Every sidecar before the collapse wrote `"type": "density"`, so it asked for
        // nothing and must still replay.
        let tagged: Reconstruction =
            serde_json::from_str(r#"{"schema_version":1,"type":"density"}"#).unwrap();
        assert_eq!(tagged, Reconstruction::default());
    }

    #[test]
    fn reconstruction_serializes_the_documented_shape() {
        // `schema_version`, one density block and the curve — an omitted input curve
        // never survives normalization, and neither retired `type` is written.
        let json = serde_json::to_value(Reconstruction::default()).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json.get("type").is_none(), "{json}");
        // The default gain is `[1, 0.84, 0.73]` (a scanner calibration; see
        // `DensityParams::scale`). Compared against the `f32` values rather than a JSON
        // literal, because `0.84f32` widens to `0.8399999737739563` as an `f64` — the
        // emitted *text* is still the round-trip-shortest `0.84`, which is what a user's
        // recipe carries.
        assert_eq!(
            json["density"]["scale"],
            serde_json::json!([1.0f32, 0.84f32, 0.73f32])
        );
        assert!(json["curve"].get("type").is_none(), "{json}");
        assert_eq!(json["curve"]["gamma"], 2.0);
    }

    /// A non-object `reconstruction.density` is refused, naming the section. `DensityParams`
    /// is a plain derive, so serde also accepts it in **positional-array** form, which
    /// no recipe spells.
    #[test]
    fn a_non_object_density_section_is_refused() {
        // The exact array a `DensityParams` derive accepts, field for field.
        let array = r#"{"density":[[1.2,1.0,0.8],[0,0,0]]}"#;
        let err = serde_json::from_str::<Reconstruction>(array)
            .expect_err("an array-shaped `density` must be refused")
            .to_string();
        assert!(
            err.contains("reconstruction.density"),
            "the message must name the section: {err}"
        );
        // Falsifiability: the same recipe with the object spelling parses *and* keeps the
        // gain, so the guard rejects the shape rather than the value.
        let object = r#"{"density":{"scale":[1.2,1.0,0.8]}}"#;
        let r: Reconstruction = serde_json::from_str(object).unwrap();
        assert_eq!(r.density.scale, [1.2, 1.0, 0.8]);

        // Every other non-object spelling is refused the same way, including a present
        // explicit `null` (which must never read as an absent key).
        for bad in [
            r#"{"density":null}"#,
            r#"{"density":1.0}"#,
            r#"{"density":"scale"}"#,
        ] {
            let err = serde_json::from_str::<Reconstruction>(bad)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("reconstruction.density"),
                "the message must name the section for {bad}: {err}"
            );
        }
    }

    #[test]
    fn reconstruction_partial_input_normalizes() {
        // Empty object = all defaults.
        let r: Reconstruction = serde_json::from_str("{}").unwrap();
        assert_eq!(r, Reconstruction::default());
        // Omitted schema_version defaults to 1; an explicit 1 also parses.
        let r: Reconstruction = serde_json::from_str(r#"{"schema_version":1}"#).unwrap();
        assert_eq!(r, Reconstruction::default());
        // An omitted gain takes the one default whatever the curve states.
        let r: Reconstruction =
            serde_json::from_str(r#"{"curve":{"type":"exponential","gamma":1.5}}"#).unwrap();
        assert_eq!(r.density, DensityParams::default());
        // Omitted curve normalizes to the default curve.
        let r: Reconstruction =
            serde_json::from_str(r#"{"density":{"scale":[1.1,1.0,0.9]}}"#).unwrap();
        assert_eq!(
            r,
            Reconstruction {
                density: DensityParams {
                    scale: [1.1, 1.0, 0.9],
                    ..DensityParams::default()
                },
                curve: ExponentialParams::default(),
            }
        );
        // Round trip: resolved → JSON → resolved is identity.
        let r = Reconstruction::default();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Reconstruction>(&json).unwrap(), r);
    }

    #[test]
    fn reconstruction_rejects_bad_schema_version_and_unknown_fields() {
        // Any schema_version other than 1 is rejected loudly.
        let err = serde_json::from_str::<Reconstruction>(r#"{"schema_version":2}"#).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "{err}");
        // Unknown fields are rejected at the reconstruction level too.
        assert!(serde_json::from_str::<Reconstruction>(r#"{"algorithm":"density"}"#).is_err());
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
        // tagged object — the same shape convention as `FilmBaseSource`.
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
    fn film_type_defaults_to_unknown_and_declares_nothing() {
        // Undeclared is the default, and the declaration is provenance only: since
        // `ir-usability-detection` it gates nothing, so there is no predicate here
        // to assert — `film_base::ir_separability` measures the plane instead.
        assert_eq!(FilmType::default(), FilmType::Unknown);
        assert_eq!(InputParams::default().film_type, FilmType::Unknown);
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
        // is emitted earlier, by `cli::load_recipe_for`).
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
        assert_eq!(
            OutputPreset::parse("film-master").unwrap(),
            OutputPreset::FilmMaster
        );
        assert_eq!(
            OutputPreset::parse("ultra-hdr-v1").unwrap(),
            OutputPreset::UltraHdrV1
        );
        // Keywords, so case/whitespace-insensitive like `OutputSpace::parse`.
        assert_eq!(
            OutputPreset::parse(" Film-Master ").unwrap(),
            OutputPreset::FilmMaster
        );
        // The product default since the `output/presets` migration.
        assert_eq!(OutputPreset::default(), OutputPreset::GainMapHdr);

        // The two retired names are removed-value errors naming the replacements and
        // the reference build — never a generic "unknown", and never an alias.
        for removed in ["legacy", "custom", " Legacy "] {
            let err = OutputPreset::parse(removed).unwrap_err();
            assert!(matches!(err, NcError::Usage(_)), "{err:?}");
            let msg = err.message();
            assert!(msg.contains("was removed"), "{msg}");
            assert!(!msg.contains("unknown output preset"), "{msg}");
            for replacement in ["display-p3", "film-master", "hdr-linear-tiff"] {
                assert!(msg.contains(replacement), "{msg}");
            }
            assert!(msg.contains("reference-snapshot"), "{msg}");
            assert!(msg.contains("no alias"), "{msg}");
        }
        // Only `custom`'s message explains the selectors it existed for.
        assert!(
            OutputPreset::parse("custom")
                .unwrap_err()
                .message()
                .contains("selectors")
        );
        assert!(
            !OutputPreset::parse("legacy")
                .unwrap_err()
                .message()
                .contains("selectors")
        );

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
        // There is no planned-but-unaccepted name left: `gain-map-hdr` and `custom`
        // were the last two, and `output/presets` shipped both. The "does not accept
        // yet" arm is gone with them — an unknown name now always means a typo, which
        // is why the diagnostic below is the only one left.
        // `hdr-pq` / `hdr-hlg` graduated out of that list when
        // `output/hdr-avif-output` activated them, and `hdr-linear-tiff`,
        // `hdr-pq-tiff` and `hdr-hlg-tiff` when `output/lossless-hdr-tiff` did — all
        // six are accepted now. They are asserted here name by name so the AVIF, the
        // linear-TIFF, and the coded-TIFF families cannot be confused back together.
        // `display-p3` and `compatibility` graduated when the SDR presets landed, and
        // `gain-map-hdr` when `output/presets` wired the dual-dialect container up.
        // It is asserted separately from `ultra-hdr-v1` on purpose: the two write the
        // same pixels and differ only in metadata dialect, which is exactly the pair
        // a future edit could collapse.
        assert_eq!(
            OutputPreset::parse("gain-map-hdr").unwrap(),
            OutputPreset::GainMapHdr
        );
        assert_eq!(
            OutputPreset::parse("display-p3").unwrap(),
            OutputPreset::DisplayP3
        );
        assert_eq!(
            OutputPreset::parse("compatibility").unwrap(),
            OutputPreset::Compatibility
        );
        assert_eq!(OutputPreset::parse("hdr-pq").unwrap(), OutputPreset::HdrPq);
        assert_eq!(
            OutputPreset::parse("hdr-hlg").unwrap(),
            OutputPreset::HdrHlg
        );
        assert_eq!(
            OutputPreset::parse("hdr-linear-tiff").unwrap(),
            OutputPreset::HdrLinearTiff
        );
        // The three TIFF HDR presets are distinct names, and `hdr-pq` vs
        // `hdr-pq-tiff` differing only by suffix is exactly the confusion worth
        // pinning: one writes AVIF, the other TIFF, from an identical rendition.
        assert_eq!(
            OutputPreset::parse("hdr-pq-tiff").unwrap(),
            OutputPreset::HdrPqTiff
        );
        assert_eq!(
            OutputPreset::parse("hdr-hlg-tiff").unwrap(),
            OutputPreset::HdrHlgTiff
        );
        assert_ne!(OutputPreset::HdrPq, OutputPreset::HdrPqTiff);
        let msg = OutputPreset::parse("filmmaster").unwrap_err().to_string();
        assert!(msg.contains("unknown output preset"), "{msg}");
    }

    #[test]
    fn every_accepted_preset_is_listed_in_the_parse_diagnostics() {
        // Both "accepted: …" lists used to be hand-written, and both went stale the
        // moment a preset shipped — so `--output-preset displayp3` listed eight names
        // and hid the one the user wanted. They are generated from `ALL` now, and this
        // is the test that keeps `ALL` itself honest.
        let unknown = OutputPreset::parse("displayp3").unwrap_err().to_string();
        for preset in OutputPreset::ALL {
            let name = preset.name();
            assert!(
                unknown.contains(name),
                "unknown-preset message omits {name}"
            );
            // Every entry is a name `parse` actually accepts (a typo in `ALL` would
            // otherwise advertise a name that then fails).
            assert_eq!(OutputPreset::parse(name).unwrap(), preset, "{name}");
        }
        // Completeness guard: adding a variant breaks this match, and the arm the
        // author has to write sits next to the reminder to extend `ALL`.
        for preset in OutputPreset::ALL {
            match preset {
                OutputPreset::FilmMaster
                | OutputPreset::UltraHdrV1
                | OutputPreset::GainMapHdr
                | OutputPreset::DisplayP3
                | OutputPreset::Compatibility
                | OutputPreset::HdrPq
                | OutputPreset::HdrHlg
                | OutputPreset::HdrLinearTiff
                | OutputPreset::HdrPqTiff
                | OutputPreset::HdrHlgTiff => {
                    assert!(OutputPreset::ALL.contains(&preset));
                }
            }
        }
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
            OutputPreset::GainMapHdr
        );
        // The custom `Deserialize` delegates to `parse`, so the recipe key gets the
        // same pinned rename diagnosis as the flag (serde's derived error would only
        // list the accepted variants and never mention the rename).
        let err = serde_json::from_str::<OutputParams>(r#"{"preset":"scene-master"}"#).unwrap_err();
        assert!(err.to_string().contains("renamed"), "{err}");
        let err = serde_json::from_str::<OutputParams>(r#"{"preset":"legacy"}"#).unwrap_err();
        assert!(err.to_string().contains("was removed"), "{err}");
        // Every name the flag accepts, the recipe key accepts too — a preset reachable
        // from one but not the other would be unusable in `hanten roll`, which has no
        // output flags at all.
        for preset in OutputPreset::ALL {
            let json = format!(r#"{{"preset":"{}"}}"#, preset.name());
            assert_eq!(
                serde_json::from_str::<OutputParams>(&json).unwrap().preset,
                preset,
                "{json}"
            );
        }
        assert_eq!(
            serde_json::from_str::<OutputParams>(r#"{"preset":"gain-map-hdr"}"#)
                .unwrap()
                .preset,
            OutputPreset::GainMapHdr
        );
    }
}
