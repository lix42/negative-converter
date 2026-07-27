//! CLI orchestration — the agent-facing command surface.
//!
//! This is the scriptable contract an agent drives: clap argument parsing for
//! every subcommand and flag (design-spec §8–9), JSON recipe load/merge (flags
//! override a loaded recipe), `--dump-params` / `params` for discovery, a JSON
//! report, and stable exit codes via [`NcError`]. The conversion runs here:
//! `convert` drives the full decode → film-base → algorithm → output color
//! transform → encode pipeline (delegating the pure stages to `pipeline`/`algo`/
//! `io`); `inspect` and `estimate` decode and report without writing an image.
//!
//! Determinism rule: stdout carries *only* the JSON report / params; all logs and
//! warnings go to stderr, so an agent can pipe stdout straight into a parser.

use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::algo::density;
use crate::io::decode::{DecodeInfo, decode};
use crate::io::encode;
use crate::pipeline::input_semantics::{
    self, ContainerColorFacts, InputAssertions, InputColorReport, RawMode,
};
use crate::pipeline::{film_base, stages, working_space};
use crate::telemetry;
use crate::types::{
    BalanceRange, BigTiff, DensityCurve, DensityCurveType, DensityParams, DmaxSource, EncodeReport,
    FilmBase, FilmBaseParams, FilmBaseSource, InputParams, MeaningAssertion, NcError, OutputParams,
    PrintParams, Reconstruction, ReconstructionType, Result, TransferAssertion, WbSource,
};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// `nc` — film-negative → positive converter.
#[derive(Parser, Debug)]
#[command(name = "nc", version, about = "Film-negative → positive converter")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
// `convert` legitimately carries the full parameter surface; boxing it would
// only fight clap's derive for a one-shot CLI enum that's never stored en masse.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Convert a negative scan to a positive TIFF.
    Convert(ConvertArgs),
    /// Convert a roll (batch of frames) from one shared, frozen recipe.
    Roll(RollArgs),
    /// Inspect a scan and emit a JSON report (no output image).
    Inspect(IoArgs),
    /// Run only film-base / Dmin estimation; emit JSON.
    Estimate(EstimateArgs),
    /// Print the full default parameter set as JSON (recipe scaffolding).
    Params,
}

/// Report format on stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
#[allow(clippy::enum_variant_names)]
pub enum ReportFormat {
    /// Machine-readable JSON report.
    #[default]
    Json,
    /// No report.
    None,
}

/// Reporting / verbosity controls shared by every subcommand.
#[derive(Args, Debug, Default)]
pub struct ReportArgs {
    /// Report format emitted on stdout.
    #[arg(long, value_enum, default_value_t = ReportFormat::Json)]
    pub report: ReportFormat,
    /// Write the report here instead of stdout.
    #[arg(long, value_name = "PATH")]
    pub report_file: Option<PathBuf>,
    /// Increase stderr logging (-v, -vv). Never pollutes stdout.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Suppress non-error stderr logging.
    #[arg(long)]
    pub quiet: bool,
}

/// `inspect`: an input scan plus reporting controls.
#[derive(Args, Debug)]
pub struct IoArgs {
    /// Input negative scan (SilverFast HDR/HDRi TIFF).
    pub input: PathBuf,
    #[command(flatten)]
    pub report: ReportArgs,
}

/// `estimate`: an input scan, the film-base source flags (so the
/// calibrate-once-from-a-reference workflow works, design-spec §8), the grid
/// calibration mode, and reporting controls.
#[derive(Args, Debug)]
pub struct EstimateArgs {
    /// Input negative scan (SilverFast HDR/HDRi TIFF).
    pub input: PathBuf,
    /// Sample a fixed 5-cell grid (corners + center) over the frame — or over
    /// `--base-region` — instead of a single measurement. For unexposed
    /// reference frames (design-spec §9 ladder tier 1): the per-cell spread is
    /// reported and disagreement warns loudly (it diagnoses light leaks,
    /// illumination falloff, or dust). Incompatible with an explicit
    /// `--film-base` (nothing to sample) and with `--auto-base` (grid replaces
    /// border detection).
    #[arg(long, conflicts_with_all = ["film_base", "auto_base"])]
    pub grid: bool,
    /// Measure the roll-fixed display-white anchor `Dmax` from this region of a
    /// **fully-exposed reference frame** (the light-struck roll leader), using the
    /// resolved film base — the plan-phase mirror of `--base-region` for `Dmax`
    /// (design-spec §8). Reports the measured scalar plus reuse-ready `--d-max` /
    /// `density.dmax` forms to freeze into a roll recipe. Typically paired with an
    /// explicit `--film-base` (the `Dmin` measured from the unexposed frame). The
    /// region is recorded as provenance only, never re-read at apply time.
    #[arg(long = "d-max-region", value_name = "X,Y,W,H", value_parser = parse_region)]
    pub d_max_region: Option<[u32; 4]>,
    #[command(flatten)]
    pub film_base: FilmBaseOverrides,
    /// Treat estimation warnings (a non-uniform `--base-region`, grid
    /// disagreement, decode notes, …) as a hard error. `estimate` produces the
    /// `Dmin` a roll is calibrated on, so a script baking the result into a
    /// recipe wants a plausible-looking-but-bad base to fail loudly rather than
    /// be echoed back.
    #[arg(long)]
    pub strict: bool,
    #[command(flatten)]
    pub report: ReportArgs,
}

/// `convert`: input, output, and every conversion knob (design-spec §9).
///
/// Stage knobs are grouped into flattened `*Overrides` structs; each field is an
/// `Option` (or a presence flag) so [`merge`] can tell "explicitly passed" from
/// "left at the recipe / default value".
#[derive(Args, Debug)]
pub struct ConvertArgs {
    /// Input negative scan (SilverFast HDR/HDRi TIFF).
    pub input: PathBuf,
    /// Output positive TIFF.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: PathBuf,
    /// Reconstruction type (default `density`).
    #[arg(long, value_enum)]
    pub reconstruction: Option<ReconstructionType>,
    /// Density-to-positive curve (with `--reconstruction density`; default
    /// `exponential`).
    #[arg(long = "density-curve", value_enum)]
    pub density_curve: Option<DensityCurveType>,
    /// Removed: the pre-reconstruction algorithm selector. Kept hidden only to
    /// emit a migration error pointing at `--reconstruction`/`--density-curve`
    /// (nc is unreleased — no aliases).
    #[arg(long, hide = true, value_name = "NAME")]
    pub algorithm: Option<String>,

    #[command(flatten)]
    pub input_opts: InputOverrides,
    #[command(flatten)]
    pub film_base: FilmBaseOverrides,
    #[command(flatten)]
    pub density: DensityOverrides,
    #[command(flatten)]
    pub dmax: DmaxOverrides,
    #[command(flatten)]
    pub sigmoid: SigmoidOverrides,
    #[command(flatten)]
    pub print: PrintOverrides,
    #[command(flatten)]
    pub simple: SimpleOverrides,
    #[command(flatten)]
    pub output_opts: OutputOverrides,

    /// Load a JSON recipe; individual `--flag`s override its values.
    #[arg(long = "params", value_name = "JSON")]
    pub recipe_in: Option<PathBuf>,
    /// Write the effective (resolved) parameters to JSON and continue.
    #[arg(long, value_name = "JSON")]
    pub dump_params: Option<PathBuf>,
    /// Treat warnings (clipping, IR-ignored, …) as hard errors.
    #[arg(long)]
    pub strict: bool,
    /// Fix any stochastic step for reproducibility (none in Step 1; reserved).
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Append a telemetry record for this run to the local JSONL log (under the
    /// platform data dir, e.g. `$XDG_DATA_HOME/nc/telemetry.jsonl` or
    /// `~/.local/share/nc/telemetry.jsonl`; override with `NC_TELEMETRY_LOG`).
    /// Operational flag — not a recipe key; never affects the output image.
    #[arg(long)]
    pub telemetry: bool,
    /// Also write this run's telemetry record to `<path>` (`-` = stdout). May be
    /// combined with `--telemetry`. Operational flag — not a recipe key.
    #[arg(long, value_name = "PATH")]
    pub telemetry_file: Option<String>,

    #[command(flatten)]
    pub report: ReportArgs,
}

/// `nc roll`: convert a batch of frames from ONE shared, frozen recipe so the
/// whole roll is color-consistent and reproducible (design-spec §8, §12 item 6).
///
/// This is the batch-**apply** half of plan→recipe→apply: it replays a *provided*
/// frozen recipe (hand-authored or `nc params`/`--dump-params`-produced) over N
/// frames. It deliberately owns no auto-cascade that *generates* the recipe —
/// that is the separate `base-acquisition-planner` task. Roll-fixed params (the
/// film base, `density.dmax`) live in the shared `--params` recipe and appear
/// once in the roll report; frame-local params can be overridden per frame via a
/// `--frames` manifest.
///
/// Unlike `convert`'s single `-o <file>`, roll writes per-frame outputs into an
/// `--out-dir` (named `<stem>_positive.tiff`) plus a roll-level JSON report on
/// stdout, so single-frame `convert` stays byte-for-byte unchanged.
#[derive(Args, Debug)]
pub struct RollArgs {
    /// Input scans: files, directories (expanded to their `.tif`/`.tiff` files),
    /// or shell globs (expanded by the shell). Collected and sorted for a
    /// deterministic frame order. Mutually exclusive with `--frames`.
    #[arg(required_unless_present = "frames", conflicts_with = "frames")]
    pub inputs: Vec<PathBuf>,
    /// A JSON manifest naming the frames explicitly, each with an optional output
    /// path and an optional partial-recipe `params` override applied on top of the
    /// shared recipe for that frame only. Mutually exclusive with positional
    /// `inputs`. Shape: `{ "frames": [ { "input": "…", "output"?: "…",
    /// "params"?: { …partial recipe… } }, … ] }`.
    #[arg(long, value_name = "JSON")]
    pub frames: Option<PathBuf>,
    /// Output directory (created if missing). Per-frame outputs are written here
    /// as `<input-stem>_positive.tiff` unless the manifest gives an explicit
    /// output path.
    #[arg(short = 'o', long = "out-dir", value_name = "DIR")]
    pub out_dir: PathBuf,
    /// Shared frozen recipe applied to every frame (the roll-fixed film base,
    /// `density.dmax`, …). Same JSON shape as `convert --params`.
    #[arg(long = "params", value_name = "JSON")]
    pub recipe_in: Option<PathBuf>,
    /// Treat any frame's warnings as a hard error (after the roll report is
    /// emitted), like `convert --strict`.
    #[arg(long)]
    pub strict: bool,
    #[command(flatten)]
    pub report: ReportArgs,
}

// --- per-stage override groups (all-Option; presence flags for booleans) ----

/// Input / decode overrides (design-spec §9, stage 1).
///
/// `--input-transfer` and `--input-meaning` are the two **independent** input
/// assertions; each replaces the recipe's value on its own axis (they do not
/// conflict — they describe different facts). The legacy combined
/// `--assume-linear` is kept only to emit a migration error (it asserted both
/// axes at once), and `--input-profile` stays rejected for normal conversion.
#[derive(Args, Debug, Default)]
pub struct InputOverrides {
    /// Transfer-encoding assertion (`auto` | `linear`). Independent of
    /// `--input-meaning`: asserts how samples are encoded, not what they measure.
    #[arg(long = "input-transfer", value_enum, value_name = "TRANSFER")]
    pub input_transfer: Option<TransferAssertion>,
    /// Measurement-meaning assertion (`auto` | `scanner-device` | `colorimetric`).
    /// Only `scanner-device` + a linear transfer enters density; `colorimetric`
    /// is recognized but unsupported.
    #[arg(long = "input-meaning", value_enum, value_name = "MEANING")]
    pub input_meaning: Option<MeaningAssertion>,
    /// Deprecated: the old combined assertion. Kept only to emit a migration error
    /// — it conflated transfer and meaning. Use `--input-transfer` /
    /// `--input-meaning`.
    #[arg(long, hide = true)]
    pub assume_linear: bool,
    /// Reserved for the deferred scanner-profile-before-density experiment; not
    /// supported for normal conversion (rejected loudly). Input-side ICC
    /// application has no validated placement yet.
    #[arg(long, value_name = "ICC")]
    pub input_profile: Option<String>,
    /// Write the decoded IR plane to this path (HDRi only).
    #[arg(long, value_name = "PATH")]
    pub export_ir: Option<String>,
}

/// Film-base / Dmin overrides (design-spec §9, stage 2).
///
/// The three source flags are mutually exclusive (clap rejects passing more than
/// one); whichever is given replaces the recipe's `film_base.source` entirely.
#[derive(Args, Debug, Default)]
pub struct FilmBaseOverrides {
    /// Explicit per-channel base transmission.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb,
          conflicts_with_all = ["base_region", "auto_base"])]
    pub film_base: Option<[f32; 3]>,
    /// Region of the unexposed border to sample.
    #[arg(long, value_name = "X,Y,W,H", value_parser = parse_region,
          conflicts_with = "auto_base")]
    pub base_region: Option<[u32; 4]>,
    /// Detect the unexposed rebate band behind the film holder (the default
    /// behavior; fails loudly when no confident band exists).
    #[arg(long)]
    pub auto_base: bool,
}

/// Density-reconstruction overrides (design-spec §9,
/// `reconstruction = density`). Every flag here maps into the tagged
/// `reconstruction` object: `--density-scale`/`--density-offset` ⇒
/// `reconstruction.density.scale`/`.offset`, the regional-balance flags ⇒ the
/// same-named `reconstruction.density` fields, and `--density-gamma` ⇒
/// `reconstruction.curve.gamma` (exponential curve only — a merge-time usage
/// error under sigmoid, never ignored).
///
/// The two `balance_range` flags are mutually exclusive (clap rejects passing
/// both), like the [`DmaxOverrides`] quartet: whichever is given replaces the
/// recipe's `reconstruction.density.balance_range` entirely.
#[derive(Args, Debug, Default)]
pub struct DensityOverrides {
    /// Per-channel density gain.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub density_scale: Option<[f32; 3]>,
    /// Per-channel density offset (orange-mask compensation).
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub density_offset: Option<[f32; 3]>,
    /// Exponential-curve gamma (the straight line's slope).
    #[arg(long)]
    pub density_gamma: Option<f32>,
    /// Regional balance: per-channel density offset for the positive's shadows.
    /// Negative values are typical, so a leading `-` is accepted
    /// (`allow_hyphen_values`); the comma-list parser still rejects non-numbers.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb, allow_hyphen_values = true)]
    pub shadow_balance: Option<[f32; 3]>,
    /// Regional balance: per-channel density offset for the positive's highlights.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb, allow_hyphen_values = true)]
    pub highlight_balance: Option<[f32; 3]>,
    /// Explicit tone-ramp anchors for the regional balance (corrected density;
    /// reuse a frame's reported range across a roll). A negative `LO` is legal
    /// (`density_offset` can shift densities below zero).
    #[arg(long, value_name = "LO,HI", value_parser = parse_lo_hi, allow_hyphen_values = true,
          conflicts_with = "auto_balance_range")]
    pub balance_range: Option<[f32; 2]>,
    /// Measure the regional-balance tone range per frame (the default behavior).
    #[arg(long)]
    pub auto_balance_range: bool,
}

/// Display-white anchor (`Dmax`) overrides (design-spec §9,
/// `reconstruction.curve.dmax` — the curve stage owns the anchor).
///
/// One mutually-exclusive choice, like [`FilmBaseOverrides`]: the four flags
/// conflict (clap rejects passing more than one) and whichever is given replaces
/// the recipe curve's `dmax` entirely, whichever curve variant is resolved.
#[derive(Args, Debug, Default)]
pub struct DmaxOverrides {
    /// Explicit roll-fixed display-white anchor density (`Dmax`); a scalar,
    /// applied to all channels. The roll calibration: the value measured once from
    /// a fully-exposed reference frame (`estimate --d-max-region`) or a known
    /// per-stock constant, reused across the roll like an explicit `--film-base`.
    #[arg(long = "d-max", value_name = "D",
          conflicts_with_all = ["fixed_d_max", "auto_d_max", "no_d_max"])]
    pub d_max: Option<f32>,
    /// Use the fixed nominal roll anchor (the default behavior) — a
    /// scene-independent corrected-density placement reused across the roll.
    #[arg(long = "fixed-d-max", conflicts_with_all = ["auto_d_max", "no_d_max"])]
    pub fixed_d_max: bool,
    /// Measure the anchor per frame (opt-in exposure normalization; brightens
    /// underexposed frames and breaks roll consistency — grading, not conversion).
    #[arg(long = "auto-d-max", conflicts_with = "no_d_max")]
    pub auto_d_max: bool,
    /// Disable the anchor — scene-referred output (base → 1.0, detail above).
    #[arg(long = "no-d-max")]
    pub no_d_max: bool,
}

/// Sigmoid-curve overrides (design-spec §7.3/§9, `density-curve = sigmoid`).
/// The flags are `--sigmoid-*`-prefixed for namespacing; the recipe keys drop
/// the prefix (`reconstruction.curve.contrast` etc., like `--d-max` ⇒
/// `reconstruction.curve.dmax`). Each is a merge-time usage error when the
/// resolved curve is not sigmoid — never silently ignored.
#[derive(Args, Debug, Default)]
pub struct SigmoidOverrides {
    /// Mid-density slope of the S-curve (the `--density-gamma` analogue).
    #[arg(long)]
    pub sigmoid_contrast: Option<f32>,
    /// Toe (shadow) knee width in log10 density units; 0 disables the toe.
    #[arg(long)]
    pub sigmoid_toe: Option<f32>,
    /// Shoulder (highlight) knee width in log10 density units; 0 disables it.
    #[arg(long)]
    pub sigmoid_shoulder: Option<f32>,
}

/// Auto white-balance modes for `--auto-wb` — the CLI face of the two
/// estimating [`WbSource`] variants (the explicit variant is `--white-balance`).
/// clap's `ValueEnum` derives the kebab-case values `gray-world` / `percentile`,
/// matching the recipe wire form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AutoWb {
    /// Equalize the trimmed per-channel means (≈ NLP Auto-AVG). Simple; a
    /// dominant scene color biases it.
    GrayWorld,
    /// Equalize the channels at a matched near-white percentile (≈ NLP
    /// Auto-Neutral). More robust to dominant colors.
    Percentile,
}

impl From<AutoWb> for WbSource {
    fn from(mode: AutoWb) -> Self {
        match mode {
            AutoWb::GrayWorld => WbSource::GrayWorld,
            AutoWb::Percentile => WbSource::Percentile,
        }
    }
}

/// Print / tone-render overrides (design-spec §9).
///
/// `--white-balance` and `--auto-wb` are the two faces of the single
/// `print.white_balance` source (mutually exclusive; clap rejects passing both);
/// whichever is given replaces the recipe's choice entirely. Precedence is by
/// **source**, not value: an explicit `--white-balance 1,1,1` over a recipe's
/// auto mode means neutral gains, not re-estimation.
#[derive(Args, Debug, Default)]
pub struct PrintOverrides {
    /// Overall positive exposure.
    #[arg(long)]
    pub print_exposure: Option<f32>,
    /// Paper black / shadow floor.
    #[arg(long)]
    pub black_point: Option<f32>,
    /// Explicit highlight / neutral white-balance gains.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb,
          conflicts_with = "auto_wb")]
    pub white_balance: Option<[f32; 3]>,
    /// Estimate the white-balance gains per frame from image statistics.
    #[arg(long = "auto-wb", value_enum, value_name = "MODE")]
    pub auto_wb: Option<AutoWb>,
    /// Highlight roll-off amount.
    #[arg(long)]
    pub highlight_compress: Option<f32>,
}

/// Removed simple-reconstruction controls (design-spec §7.1/§9). Simple
/// reconstruction now ends at the direct unclamped positive `1 − scan/Dmin`;
/// its old inversion white balance and clip-range remap are **not**
/// reconstruction parameters — they return downstream (as explicit
/// `print.white_balance` and `print.linear_range`) when named output presets
/// land. Since their defaults were the exact identity, the default simple
/// output is unchanged; a *customized* value can no longer be expressed, so the
/// flags are kept hidden solely to emit a migration error (nc is unreleased —
/// no aliases, no silent behavior change).
#[derive(Args, Debug, Default)]
pub struct SimpleOverrides {
    /// Removed: inversion white-balance gains (returns as `print.white_balance`
    /// at preset migration).
    #[arg(long, hide = true, value_name = "R,G,B")]
    pub invert_white_balance: Option<String>,
    /// Removed: low clip point (returns as `print.linear_range`).
    #[arg(long, hide = true, value_name = "F")]
    pub clip_low: Option<String>,
    /// Removed: high clip point (returns as `print.linear_range`).
    #[arg(long, hide = true, value_name = "F")]
    pub clip_high: Option<String>,
}

/// Output / encode overrides (design-spec §9, stage 5).
#[derive(Args, Debug, Default)]
pub struct OutputOverrides {
    /// Write a 32-bit float TIFF (full HDR, no precision loss) instead of the
    /// default 16-bit integer TIFF.
    #[arg(long, conflicts_with = "output_sdr")]
    pub output_hdr: bool,
    /// Force the default 16-bit integer TIFF, overriding a recipe's
    /// `output.hdr = true` (the flags-win escape hatch; without it a bool
    /// presence flag could set HDR but never clear it).
    #[arg(long)]
    pub output_sdr: bool,
    /// Output ICC profile (`sRGB` / `prophoto` / `acescg` / `display-p3` / path).
    #[arg(long, value_name = "PROFILE")]
    pub output_profile: Option<String>,
    /// BigTIFF promotion policy (default `auto`).
    #[arg(long, value_enum)]
    pub bigtiff: Option<BigTiff>,
}

// ---------------------------------------------------------------------------
// Resolved configuration (= the recipe shape)
// ---------------------------------------------------------------------------

/// The fully-resolved parameter set the pipeline runs on. This is *also* the
/// recipe shape: `--params` deserializes a (partial) recipe into it with serde
/// defaults filling the gaps, and `--dump-params` / `nc params` serialize it.
///
/// Nested per-stage objects (not a flat bag) so `deny_unknown_fields` can reject
/// typo'd keys at every level — `serde(flatten)` would defeat that. The
/// algorithm selection is the one tagged `reconstruction` object
/// (`schema_version` 1, design-spec §8): there are no sibling top-level
/// `algorithm`/`density`/`sigmoid`/`simple` sections — the removed legacy forms
/// are rejected with a migration error at recipe load (`load_recipe`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ResolvedConfig {
    pub reconstruction: Reconstruction,
    pub input: InputParams,
    pub film_base: FilmBaseParams,
    pub print: PrintParams,
    pub output: OutputParams,
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// The reuse-ready forms of a measured film base, kept as one unit so the flag
/// and the recipe fragment are both-present-or-both-absent — the illegal
/// flag-without-recipe (or recipe-without-flag) state two parallel `Option`s
/// would permit is unrepresentable (the parallel-`Option` anti-pattern in
/// `CLAUDE.md`). Serialize-only; the field renames keep the two forms as the
/// flat top-level report keys `film_base_flag` / `film_base_recipe` when this is
/// `#[serde(flatten)]`ed into [`Report`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReuseReady {
    /// Ready-to-paste `--film-base R,G,B` flag for the measured base; the values
    /// round-trip to the exact measured `f32`s.
    #[serde(rename = "film_base_flag")]
    pub flag: String,
    /// The same measurement as a minimal recipe fragment for the `film_base`
    /// section — `{"source":{"explicit":[r,g,b]}}` — ready to merge into a roll
    /// recipe.
    #[serde(rename = "film_base_recipe")]
    pub recipe: FilmBaseParams,
}

/// A minimal curve-section recipe fragment carrying only the resolved
/// roll-fixed `Dmax` (`{ "dmax": { "explicit": <d> } }`), so `estimate`'s
/// reuse-ready output drops into a roll recipe's `reconstruction.curve` object
/// without pulling in the other curve defaults. Serialize-only.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DmaxRecipeFragment {
    /// The `reconstruction.curve.dmax` value — always the tagged
    /// `{ "explicit": <d> }` form.
    pub dmax: DmaxSource,
}

/// Reuse-ready forms of a measured roll-fixed `Dmax` (`estimate --d-max-region`),
/// mirroring [`ReuseReady`]: a paste-ready `--d-max <d>` flag and the matching
/// `density` recipe fragment. Both present together, so the calibrate-once → reuse
/// workflow (design-spec §8) is copy-paste smooth. Flattened into [`Report`], so
/// the two forms are the flat top-level keys `d_max_flag` / `d_max_recipe`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DmaxReuseReady {
    /// Ready-to-paste `--d-max <d>` flag; the value round-trips to the measured `f32`.
    #[serde(rename = "d_max_flag")]
    pub flag: String,
    /// The same measurement as a `density`-section recipe fragment —
    /// `{ "dmax": { "explicit": <d> } }` — ready to merge into a roll recipe.
    #[serde(rename = "d_max_recipe")]
    pub recipe: DmaxRecipeFragment,
}

/// Resolution diagnostics for the reconstruction that ran (design-spec §8's
/// report shape): `{"type":"simple"}`, or `{"type":"density","curve":{…}}`
/// with the resolved curve type and `dmax` resolution. Serialize-only.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ReconstructionResult {
    Simple,
    Density { curve: CurveResult },
}

/// The resolved curve inside a density [`ReconstructionResult`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CurveResult {
    /// The curve type that ran (`"exponential"` / `"sigmoid"`).
    #[serde(rename = "type")]
    pub curve_type: DensityCurveType,
    /// The resolved display-white anchor.
    pub dmax: DmaxResolution,
}

/// The resolved `Dmax` triple: which policy was configured, the value the curve
/// actually used this run (`null` for `none`), and where the resolution came
/// from. A reference-measured scalar frozen into a recipe reports
/// `explicit`/`recipe`; its capture region stays in the `estimate` record
/// (provenance, never a re-read directive).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DmaxResolution {
    pub policy: DmaxPolicy,
    /// Always emitted — `null` for `policy = "none"` (fixed report shape).
    pub value: Option<f32>,
    pub provenance: DmaxProvenance,
}

/// The configured `Dmax` policy (`reconstruction.curve.dmax`'s discriminator).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DmaxPolicy {
    Fixed,
    Explicit,
    Auto,
    None,
}

impl From<DmaxSource> for DmaxPolicy {
    fn from(source: DmaxSource) -> Self {
        match source {
            DmaxSource::Fixed => DmaxPolicy::Fixed,
            DmaxSource::Explicit(_) => DmaxPolicy::Explicit,
            DmaxSource::Auto => DmaxPolicy::Auto,
            DmaxSource::None => DmaxPolicy::None,
        }
    }
}

/// Where the resolved `Dmax` came from: the built-in default, the recipe file
/// (or a roll per-frame override), a CLI flag — or, for the `auto` policy, the
/// per-frame measurement itself (`"auto-frame"`), which is what makes that
/// policy display-oriented and unsuitable for a film master.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DmaxProvenance {
    Default,
    Recipe,
    Cli,
    AutoFrame,
}

/// Where the `reconstruction.curve.dmax` *setting* came from, tracked by the
/// orchestrators (recipe-key presence / flag presence) and combined with the
/// resolved policy into the report's [`DmaxProvenance`] — an `auto` policy
/// always reports `auto-frame`, because the *value* is a per-frame measurement
/// regardless of who selected the policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DmaxSetting {
    Default,
    Recipe,
    Cli,
}

/// Build the report's `reconstruction_result` from the resolved config, the
/// render's resolved anchor value, and the tracked `dmax` setting provenance.
fn reconstruction_result(
    reconstruction: &Reconstruction,
    resolved_dmax: Option<f32>,
    setting: DmaxSetting,
) -> ReconstructionResult {
    match reconstruction {
        Reconstruction::Simple => ReconstructionResult::Simple,
        Reconstruction::Density { curve, .. } => {
            let policy = DmaxPolicy::from(curve.dmax());
            let provenance = match (policy, setting) {
                (DmaxPolicy::Auto, _) => DmaxProvenance::AutoFrame,
                (_, DmaxSetting::Cli) => DmaxProvenance::Cli,
                (_, DmaxSetting::Recipe) => DmaxProvenance::Recipe,
                (_, DmaxSetting::Default) => DmaxProvenance::Default,
            };
            ReconstructionResult::Density {
                curve: CurveResult {
                    curve_type: curve.curve_type(),
                    dmax: DmaxResolution {
                        policy,
                        value: resolved_dmax,
                        provenance,
                    },
                },
            }
        }
    }
}

/// Machine-readable result emitted on stdout (or `--report-file`). One shape
/// serves all three commands; irrelevant fields are `None`/empty and omitted
/// from the JSON (`skip_serializing_if`), so an agent gets a clean object per
/// command. Serialize-only — it embeds the serialize-only `DecodeInfo` /
/// `EncodeReport`, and nothing deserializes a report.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// The subcommand that produced this report (`convert`/`inspect`/`estimate`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<&'static str>,
    /// Input scan path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<PathBuf>,
    /// Output image path, when one was written (`convert`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    /// The effective (resolved) recipe the frame ran on (`convert`) — the same
    /// object written to the sidecar, so `recipe.reconstruction` is the exact
    /// tagged reconstruction schema (design-spec §8's report shape).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipe: Option<ResolvedConfig>,
    /// Resolution diagnostics for the reconstruction that ran (`convert`):
    /// `{"type":"simple"}`, or a density object carrying the resolved curve
    /// type and its `dmax = {policy, value, provenance}` (design-spec §8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconstruction_result: Option<ReconstructionResult>,
    /// The pinned working-space mapping this conversion interprets the
    /// reconstructed film RGB under (`convert`): always `"nc-film-rgb-v1"`
    /// (linear Rec.709/D65 → linear ACEScg/D60; see
    /// `pipeline::working_space`). Provenance only — the mapping is a fixed
    /// constant, not a tunable knob, so it has no CLI flag / recipe key
    /// (design-spec §8). A future mapping is a *new* identifier under
    /// `conversion-versioning`, never a silent change to v1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_mapping: Option<&'static str>,
    /// What the decoder found (`inspect`): format, dimensions, channels, bit
    /// depth, IR presence, scanner metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode: Option<DecodeInfo>,
    /// Resolved input color semantics (`convert`/`inspect`): the two independent
    /// axes (transfer encoding + measurement meaning) with per-axis evidence,
    /// whether an ICC is embedded plus a safe summary, and whether any transfer
    /// decoding was performed. `convert` only reaches the render once this
    /// resolves to a supported linear + scanner-device input; `inspect` reports it
    /// even when the input is ambiguous or unsupported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_color: Option<InputColorReport>,
    /// Estimated / resolved film base (the `Dmin` anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_base: Option<FilmBase>,
    /// Resolved display-white anchor density (`Dmax`): for `convert`, the value
    /// the density curve used (fixed nominal / explicit / auto-measured), absent
    /// for `dmax = none` or simple reconstruction; for `estimate
    /// --d-max-region`, the scalar measured from the fully-exposed reference
    /// frame. Reported so a roll can freeze one calibration into `--d-max` /
    /// `reconstruction.curve.dmax` (design-spec §8/§9). The structured
    /// `reconstruction_result` carries the same value with policy + provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dmax: Option<f32>,
    /// Reference region `[x, y, w, h]` sampled for the roll-fixed `Dmax`
    /// (`estimate --d-max-region`) — **provenance only**, recorded so the
    /// calibration is auditable, never a re-read directive baked into a recipe
    /// (that would break the deterministic-apply contract; design-spec §8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dmax_region: Option<[u32; 4]>,
    /// Reuse-ready forms of the measured roll-fixed `Dmax` (`estimate
    /// --d-max-region`): a paste-ready `--d-max <d>` flag and the matching
    /// `density` recipe fragment. Flattened, so the two forms are the flat
    /// top-level keys `d_max_flag` / `d_max_recipe`; `None` emits neither.
    #[serde(flatten)]
    pub dmax_reuse: Option<DmaxReuseReady>,
    /// Resolved stage-4 white-balance gains `[r, g, b]` the density print render
    /// applied (`convert`): the auto-estimated (`--auto-wb`) or explicit value,
    /// absent for the `simple` algorithm. Reported so a roll can freeze one
    /// frame's estimate into `--white-balance R,G,B` / a recipe's
    /// `print.white_balance = {"explicit": […]}` — measure once, reuse
    /// (design-spec §8/§9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<[f32; 3]>,
    /// Resolved regional-balance tone-ramp range `[lo, hi]` (corrected density)
    /// the density conversion used (`convert`): the auto-measured or explicit
    /// anchors, absent when both balances are neutral or for the `simple`
    /// algorithm. Reported so a roll can reuse one frame's measured range via
    /// `--balance-range` (design-spec §9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance_range: Option<[f32; 2]>,
    /// How the film base was chosen, as the structured [`FilmBaseSource`]
    /// (`"auto"` / `{"region":[…]}` / `{"explicit":[…]}`) so an agent gets the
    /// sampled rectangle / explicit values without string-parsing a label.
    /// For `estimate --grid` this is the overall rectangle the grid sampled
    /// (`{"region":[…]}`); the `grid` field documents the per-cell method.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_base_source: Option<FilmBaseSource>,
    /// Candidate unexposed-rebate bands from the inward-scan detector
    /// (`inspect` only): edge, a rectangle usable verbatim as `--base-region`,
    /// the proposed base, and the measured spread (lower = more uniform). Lets
    /// a user confirm a region instead of measuring one in an image viewer —
    /// and a future UI draws its highlight rectangles from the same data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_candidates: Option<Vec<film_base::RebateCandidate>>,
    /// Reuse-ready forms of the measured base (`estimate`): a ready-to-paste
    /// `--film-base R,G,B` flag and the matching `film_base` recipe fragment, so
    /// the calibrate-once → reuse workflow (design-spec §8) is copy-paste smooth.
    /// Both forms are present together or both absent — the pair only exists when
    /// the measurement is usable as an explicit base (each channel in `(0, 1]`),
    /// so a single [`ReuseReady`] (both-or-neither) replaces two parallel
    /// `Option`s that could encode the illegal flag-without-recipe state. Flattened
    /// so the two forms stay flat top-level keys (`film_base_flag` /
    /// `film_base_recipe`) on the wire; `None` emits neither.
    #[serde(flatten)]
    pub reuse: Option<ReuseReady>,
    /// Grid-sampling result (`estimate --grid`): the per-cell values, their
    /// per-channel spread, the agreement tolerance and verdict. Disagreement
    /// additionally lands in `warnings` (and fails under `--strict`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<film_base::GridEstimate>,
    /// Path the IR plane was exported to, when `--export-ir` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ir_exported: Option<PathBuf>,
    /// Encode-time sample loss (clipped / non-finite counts), for `convert`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss: Option<EncodeReport>,
    /// Non-fatal warnings (clipping, IR-ignored, BigTIFF auto-promote, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Wall-clock time in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Value parsers (comma lists)
// ---------------------------------------------------------------------------

/// Parse `R,G,B` into three `f32`s.
fn parse_rgb(s: &str) -> std::result::Result<[f32; 3], String> {
    let v = parse_floats::<3>(s)?;
    Ok(v)
}

/// Parse `LO,HI` into two `f32`s.
fn parse_lo_hi(s: &str) -> std::result::Result<[f32; 2], String> {
    parse_floats::<2>(s)
}

/// Parse `X,Y,W,H` into four `u32`s.
fn parse_region(s: &str) -> std::result::Result<[u32; 4], String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(format!(
            "expected X,Y,W,H (4 comma-separated integers), got `{s}`"
        ));
    }
    let mut out = [0u32; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .trim()
            .parse()
            .map_err(|_| format!("`{}` is not a non-negative integer in `{s}`", p.trim()))?;
    }
    Ok(out)
}

/// Parse exactly `N` comma-separated floats.
fn parse_floats<const N: usize>(s: &str) -> std::result::Result<[f32; N], String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != N {
        return Err(format!("expected {N} comma-separated numbers, got `{s}`"));
    }
    let mut out = [0f32; N];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .trim()
            .parse()
            .map_err(|_| format!("`{}` is not a number in `{s}`", p.trim()))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Recipe load / merge / validate (pure, unit-tested without the pipeline)
// ---------------------------------------------------------------------------

/// A loaded recipe plus the load-time fact the report's `Dmax` provenance
/// needs: whether the file explicitly set `reconstruction.curve.dmax` (after
/// defaults fill in, a recipe that *wrote* `"fixed"` is indistinguishable from
/// one that omitted it — the raw JSON is the only witness).
struct LoadedRecipe {
    cfg: ResolvedConfig,
    curve_dmax_present: bool,
}

/// Load a recipe file into a [`ResolvedConfig`], or the defaults when no recipe
/// is given. A read failure or invalid/unknown-key JSON is a usage error;
/// removed legacy keys get pinned migration errors first
/// ([`reject_legacy_recipe_keys`]) rather than opaque serde messages.
fn load_recipe(path: Option<&Path>) -> Result<LoadedRecipe> {
    match path {
        None => Ok(LoadedRecipe {
            cfg: ResolvedConfig::default(),
            curve_dmax_present: false,
        }),
        Some(p) => {
            let txt = std::fs::read_to_string(p)
                .map_err(|e| NcError::Usage(format!("cannot read recipe {}: {e}", p.display())))?;
            // Parse to a raw Value first for the migration checks and the
            // dmax-presence fact; the typed parse below still owns shape and
            // unknown-key validation. Unparseable JSON falls through to the
            // typed parse's error (its message names the recipe).
            let value: Option<serde_json::Value> = serde_json::from_str(&txt).ok();
            if let Some(v) = &value {
                reject_legacy_recipe_keys(v, &format!("recipe {}", p.display()))?;
            }
            let cfg = serde_json::from_str(&txt)
                .map_err(|e| NcError::Usage(format!("invalid recipe {}: {e}", p.display())))?;
            let curve_dmax_present = value.as_ref().is_some_and(sets_curve_dmax);
            Ok(LoadedRecipe {
                cfg,
                curve_dmax_present,
            })
        }
    }
}

/// Pinned migration errors for removed recipe keys, shared by the whole-recipe
/// load and the roll per-frame override path so a legacy key gets the same
/// actionable guidance wherever it appears (deny_unknown_fields would reject
/// them anyway, but with an opaque serde message):
///
/// - the combined `input.color` (conflated transfer with meaning;
///   input-data-semantics split it into `input.transfer` / `input.meaning`);
/// - the top-level `algorithm`/`density`/`sigmoid`/`simple` selection forms,
///   replaced by the one tagged `reconstruction` object (design-spec §8). nc is
///   unreleased, so these are rejected, never aliased.
fn reject_legacy_recipe_keys(v: &serde_json::Value, context: &str) -> Result<()> {
    if v.get("input")
        .and_then(|input| input.get("color"))
        .is_some()
    {
        return Err(NcError::Usage(format!(
            "{context}: `input.color` is no longer supported — it conflated transfer \
             encoding with measurement meaning. Replace it with the independent keys \
             `input.transfer` (auto|linear) and `input.meaning` \
             (auto|scanner-device|colorimetric)."
        )));
    }
    if let Some(key) = ["algorithm", "density", "sigmoid", "simple"]
        .into_iter()
        .find(|k| v.get(k).is_some())
    {
        return Err(NcError::Usage(format!(
            "{context}: top-level `{key}` is no longer supported — the algorithm \
             selection moved into one tagged `reconstruction` object \
             (schema_version 1). Use `reconstruction.type = \"simple\"` (no other \
             fields) or `reconstruction.type = \"density\"` with density \
             correction under `reconstruction.density` ({{scale, offset, \
             shadow_balance, highlight_balance, balance_range}}) and exactly one \
             tagged curve under `reconstruction.curve` \
             ({{\"type\":\"exponential\", gamma, dmax}} or \
             {{\"type\":\"sigmoid\", contrast, toe, shoulder, dmax}}). \
             See design-spec §8."
        )));
    }
    Ok(())
}

/// Whether a recipe/override JSON object explicitly carries
/// `reconstruction.curve.dmax` — the raw-JSON witness behind the report's
/// `recipe` Dmax provenance.
fn sets_curve_dmax(v: &serde_json::Value) -> bool {
    v.get("reconstruction")
        .and_then(|r| r.get("curve"))
        .and_then(|c| c.get("dmax"))
        .is_some()
}

/// Whether any of the four (clap-mutually-exclusive) `Dmax` flags was passed —
/// the CLI witness for the report's `Dmax` provenance, and the merge's "replace
/// the recipe curve's `dmax`" trigger.
fn dmax_flag_given(o: &DmaxOverrides) -> bool {
    o.d_max.is_some() || o.fixed_d_max || o.auto_d_max || o.no_d_max
}

/// The first density-reconstruction / curve / `Dmax` flag present on the
/// command line, by name — for the merge's invalid-combination errors (e.g. a
/// curve flag with `--reconstruction simple` must name the offending flag).
fn active_density_domain_flag(args: &ConvertArgs) -> Option<&'static str> {
    let d = &args.density;
    let s = &args.sigmoid;
    let x = &args.dmax;
    [
        ("--density-scale", d.density_scale.is_some()),
        ("--density-offset", d.density_offset.is_some()),
        ("--density-gamma", d.density_gamma.is_some()),
        ("--shadow-balance", d.shadow_balance.is_some()),
        ("--highlight-balance", d.highlight_balance.is_some()),
        ("--balance-range", d.balance_range.is_some()),
        ("--auto-balance-range", d.auto_balance_range),
        ("--sigmoid-contrast", s.sigmoid_contrast.is_some()),
        ("--sigmoid-toe", s.sigmoid_toe.is_some()),
        ("--sigmoid-shoulder", s.sigmoid_shoulder.is_some()),
        ("--d-max", x.d_max.is_some()),
        ("--fixed-d-max", x.fixed_d_max),
        ("--auto-d-max", x.auto_d_max),
        ("--no-d-max", x.no_d_max),
    ]
    .into_iter()
    .find_map(|(name, present)| present.then_some(name))
}

/// Apply CLI overrides on top of a (recipe or default) config; flags win.
///
/// `Option` overrides replace when `Some`, presence-flag booleans
/// (`--auto-base`) replace only when set — a `false` flag never clobbers a
/// recipe `true`, since you disable auto-base by supplying an explicit base, not
/// by passing `false`. (The removed `--algorithm`/simple-control and deprecated
/// input flags are rejected before `merge`, so they never reach here.)
///
/// Fallible where the old flat merge was total: the tagged `reconstruction`
/// makes some flag/config combinations *invalid* rather than inert, and the
/// design pins them as post-merge usage errors (exit 2), never ignored —
/// a density/curve/`Dmax` flag with a resolved `simple` reconstruction, a
/// sigmoid flag with a resolved `exponential` curve, `--density-gamma` with a
/// resolved `sigmoid` curve (a customized gamma is a loud error, not the old
/// warning), and `--density-curve` with `simple`.
pub fn merge(mut cfg: ResolvedConfig, args: &ConvertArgs) -> Result<ResolvedConfig> {
    let usage = |m: String| NcError::Usage(m);

    // --reconstruction: switch the type first, so the per-field flags below land
    // on the switched-to config. `density` over an existing density recipe keeps
    // its blocks (the flag is then a no-op assertion); switching from simple
    // starts from the density defaults; `simple` always resolves to Simple
    // (there is nothing to carry).
    if let Some(t) = args.reconstruction {
        cfg.reconstruction = match (t, cfg.reconstruction) {
            (ReconstructionType::Simple, _) => Reconstruction::Simple,
            (ReconstructionType::Density, d @ Reconstruction::Density { .. }) => d,
            (ReconstructionType::Density, Reconstruction::Simple) => Reconstruction::default(),
        };
    }

    // --density-curve: switch the curve variant inside a density reconstruction.
    // Same-type is a no-op (keeps the recipe's curve knobs); a switch carries
    // the roll-fixed `dmax` calibration over (it is curve-independent, exactly
    // as it was algorithm-independent before) and takes the new variant's
    // defaults for the curve-specific knobs.
    if let Some(c) = args.density_curve {
        match &mut cfg.reconstruction {
            Reconstruction::Simple => {
                return Err(usage(
                    "--density-curve selects the density-to-positive curve, but the \
                     resolved reconstruction is `simple` (no curve stage); pass \
                     --reconstruction density"
                        .into(),
                ));
            }
            Reconstruction::Density { curve, .. } => {
                if curve.curve_type() != c {
                    let dmax = curve.dmax();
                    *curve = match c {
                        DensityCurveType::Exponential => {
                            DensityCurve::Exponential(crate::types::ExponentialParams {
                                dmax,
                                ..Default::default()
                            })
                        }
                        DensityCurveType::Sigmoid => {
                            DensityCurve::Sigmoid(crate::types::SigmoidParams {
                                dmax,
                                ..Default::default()
                            })
                        }
                    };
                }
            }
        }
    }

    // input color: transfer and meaning are independent axes — each override
    // replaces the recipe's value on its own axis (flags win). The deprecated
    // `--assume-linear` / `--input-profile` flags are handled (rejected) outside
    // `merge`, in `reject_deprecated_input_flags`, before this runs.
    if let Some(t) = args.input_opts.input_transfer {
        cfg.input.transfer = t;
    }
    if let Some(m) = args.input_opts.input_meaning {
        cfg.input.meaning = m;
    }
    if let Some(p) = &args.input_opts.export_ir {
        cfg.input.export_ir = Some(p.clone());
    }

    // film base: the four source flags are mutually exclusive (clap-enforced);
    // whichever is given replaces the recipe's source entirely.
    if let Some(src) = film_base_source_override(&args.film_base) {
        cfg.film_base.source = src;
    }

    // Density-reconstruction, curve, and Dmax flags — all live inside the tagged
    // `reconstruction`, so with a resolved `simple` any of them is an invalid
    // combination (fail loudly, never a silent no-op).
    match &mut cfg.reconstruction {
        Reconstruction::Simple => {
            if let Some(flag) = active_density_domain_flag(args) {
                return Err(usage(format!(
                    "{flag} configures density reconstruction, but the resolved \
                     reconstruction is `simple` (the direct inversion has no density \
                     correction, curve, or Dmax); pass --reconstruction density"
                )));
            }
        }
        Reconstruction::Density { density, curve } => {
            // density block: `--density-scale`/`--density-offset` ⇒
            // `reconstruction.density.scale`/`.offset`; regional-balance flags ⇒
            // the same-named density fields.
            if let Some(v) = args.density.density_scale {
                density.scale = v;
            }
            if let Some(v) = args.density.density_offset {
                density.offset = v;
            }
            if let Some(v) = args.density.shadow_balance {
                density.shadow_balance = v;
            }
            if let Some(v) = args.density.highlight_balance {
                density.highlight_balance = v;
            }
            // balance range: the two flags are mutually exclusive (clap-enforced);
            // whichever is given replaces the recipe's
            // `reconstruction.density.balance_range` entirely.
            if let Some(v) = args.density.balance_range {
                density.balance_range = BalanceRange::Explicit(v);
            } else if args.density.auto_balance_range {
                density.balance_range = BalanceRange::Auto;
            }

            // `--density-gamma` ⇒ `reconstruction.curve.gamma` — exponential
            // only. A customized gamma under a resolved sigmoid curve is a loud
            // usage error (the pre-reconstruction code warned-and-ignored; the
            // tagged schema makes the invalid combination unrepresentable).
            if let Some(g) = args.density.density_gamma {
                match curve {
                    DensityCurve::Exponential(e) => e.gamma = g,
                    DensityCurve::Sigmoid(_) => {
                        return Err(usage(format!(
                            "--density-gamma ({g}) sets the exponential curve's gamma, \
                             but the resolved curve is sigmoid — its mid-density slope \
                             is --sigmoid-contrast (or pass --density-curve exponential)"
                        )));
                    }
                }
            }

            // sigmoid flags ⇒ `reconstruction.curve.{contrast, toe, shoulder}` —
            // sigmoid only; under exponential they are invalid, not inert.
            let sig = &args.sigmoid;
            let sigmoid_flag = [
                ("--sigmoid-contrast", sig.sigmoid_contrast.is_some()),
                ("--sigmoid-toe", sig.sigmoid_toe.is_some()),
                ("--sigmoid-shoulder", sig.sigmoid_shoulder.is_some()),
            ]
            .into_iter()
            .find_map(|(name, present)| present.then_some(name));
            match curve {
                DensityCurve::Sigmoid(s) => {
                    if let Some(v) = sig.sigmoid_contrast {
                        s.contrast = v;
                    }
                    if let Some(v) = sig.sigmoid_toe {
                        s.toe = v;
                    }
                    if let Some(v) = sig.sigmoid_shoulder {
                        s.shoulder = v;
                    }
                }
                DensityCurve::Exponential(_) => {
                    if let Some(flag) = sigmoid_flag {
                        return Err(usage(format!(
                            "{flag} configures the sigmoid curve, but the resolved \
                             curve is exponential; pass --density-curve sigmoid \
                             (its slope analogue for exponential is --density-gamma)"
                        )));
                    }
                }
            }

            // Dmax anchor ⇒ `reconstruction.curve.dmax`: the four flags are
            // mutually exclusive (clap-enforced); whichever is given replaces
            // the resolved curve's `dmax` entirely, whichever variant it is.
            if let Some(v) = args.dmax.d_max {
                *curve.dmax_mut() = DmaxSource::Explicit(v);
            } else if args.dmax.fixed_d_max {
                *curve.dmax_mut() = DmaxSource::Fixed;
            } else if args.dmax.auto_d_max {
                *curve.dmax_mut() = DmaxSource::Auto;
            } else if args.dmax.no_d_max {
                *curve.dmax_mut() = DmaxSource::None;
            }
        }
    }

    // print
    if let Some(v) = args.print.print_exposure {
        cfg.print.print_exposure = v;
    }
    if let Some(v) = args.print.black_point {
        cfg.print.black_point = v;
    }
    // white balance: `--white-balance` / `--auto-wb` are mutually exclusive
    // (clap-enforced); whichever is given replaces the recipe's source entirely.
    // Precedence is by *source*: explicit `--white-balance 1,1,1` still beats a
    // recipe's auto mode (the variant records where the gains came from).
    if let Some(v) = args.print.white_balance {
        cfg.print.white_balance = WbSource::Explicit(v);
    } else if let Some(mode) = args.print.auto_wb {
        cfg.print.white_balance = mode.into();
    }
    if let Some(v) = args.print.highlight_compress {
        cfg.print.highlight_compress = v;
    }

    // output: `--output-hdr` is a presence flag — passing it switches the output
    // to 32-bit float; when absent it must not clobber a recipe's `hdr: true`
    // (same convention as `--auto-base`), so only a set flag merges.
    // output depth: the two flags are mutually exclusive (clap-enforced);
    // whichever is given replaces the recipe's choice — `--output-sdr` exists
    // so a recipe `hdr: true` stays CLI-overridable (flags win), since an
    // absent presence flag never clobbers a recipe value.
    if args.output_opts.output_hdr {
        cfg.output.hdr = true;
    } else if args.output_opts.output_sdr {
        cfg.output.hdr = false;
    }
    if let Some(v) = &args.output_opts.output_profile {
        cfg.output.output_profile = Some(v.clone());
    }
    if let Some(v) = args.output_opts.bigtiff {
        cfg.output.bigtiff = v;
    }

    Ok(cfg)
}

/// Map the (clap-mutually-exclusive) film-base flags to a [`FilmBaseSource`],
/// or `None` when none was passed. Shared by `convert`'s [`merge`] and
/// `estimate`, so the two resolve the source identically.
fn film_base_source_override(o: &FilmBaseOverrides) -> Option<FilmBaseSource> {
    if let Some(v) = o.film_base {
        Some(FilmBaseSource::Explicit(v))
    } else if let Some(v) = o.base_region {
        Some(FilmBaseSource::Region(v))
    } else if o.auto_base {
        Some(FilmBaseSource::Auto)
    } else {
        None
    }
}

/// Validate that an explicit film base is a per-channel transmission in `(0, 1]`
/// — the one invariant that must hold wherever an explicit base enters (a recipe
/// via [`validate`], or the `--film-base` flag on `estimate`). Non-positive /
/// non-finite would divide into inf/NaN downstream; a value above 1.0 (e.g. a
/// "90" typo for "0.90") would render every real sample above white.
fn validate_explicit_film_base(base: &[f32; 3]) -> Result<()> {
    if base.iter().any(|v| !v.is_finite() || *v <= 0.0 || *v > 1.0) {
        return Err(NcError::Usage(format!(
            "--film-base channels are transmissions in (0, 1] (got {base:?})"
        )));
    }
    Ok(())
}

/// Validate a resolved config at the CLI boundary so the pure stages can trust
/// their inputs. Every failure is a [`NcError::Usage`] (exit 2) — bad recipes and
/// impossible parameters fail loudly, never producing a quietly wrong image.
pub fn validate(cfg: &ResolvedConfig) -> Result<()> {
    let usage = |m: String| NcError::Usage(m);

    let finite = |label: &str, vals: &[f32]| -> Result<()> {
        for v in vals {
            if !v.is_finite() {
                return Err(usage(format!("{label} must be finite (got {v})")));
            }
        }
        Ok(())
    };
    let positive = |label: &str, vals: &[f32]| -> Result<()> {
        for v in vals {
            if !v.is_finite() || *v <= 0.0 {
                return Err(usage(format!("{label} must be finite and > 0 (got {v})")));
            }
        }
        Ok(())
    };

    // Film base: an explicit base is a per-channel transmission in (0, 1] — the
    // decoded scan is [0, 1]-normalized, so a value above 1 (e.g. a "90" typo for
    // "0.90") would silently render every real sample denser than the base; a
    // sampled region must have non-zero extent; auto needs nothing.
    match cfg.film_base.source {
        FilmBaseSource::Explicit(b) => validate_explicit_film_base(&b)?,
        FilmBaseSource::Region([_, _, w, h]) if w == 0 || h == 0 => {
            return Err(usage("--base-region width and height must be > 0".into()));
        }
        FilmBaseSource::Region(_) | FilmBaseSource::Auto => {}
    }

    // Reconstruction: the tagged config's value checks, per variant. `simple`
    // carries no further knobs (its old WB/clip controls were removed — see
    // `SimpleOverrides`), so only `density` has anything to check.
    if let Reconstruction::Density { density, curve } = &cfg.reconstruction {
        // Density block: per-channel gain must be positive; offset just finite.
        positive("--density-scale", &density.scale)?;
        finite("--density-offset", &density.offset)?;

        // Regional balance: the offsets are density deltas — any finite value
        // (including negative) is meaningful. An explicit ramp range must be finite
        // and ordered `lo < hi`: equal anchors would make the ramp divide by zero,
        // and a recipe can smuggle values the CLI parser never saw.
        finite("--shadow-balance", &density.shadow_balance)?;
        finite("--highlight-balance", &density.highlight_balance)?;
        if let BalanceRange::Explicit([lo, hi]) = density.balance_range {
            finite("--balance-range", &[lo, hi])?;
            if lo >= hi {
                return Err(usage(format!(
                    "--balance-range low ({lo}) must be < high ({hi})"
                )));
            }
            // The span `hi - lo` divides the ramp; two individually-finite anchors
            // can still overflow it to `+inf` (e.g. `-3e38,3e38`), which silently
            // collapses `w_hi` to 0 for every pixel — the highlight balance would
            // then never apply while the report claims the range was honored. A
            // representable span is a hard requirement, not just `lo < hi`.
            if !(hi - lo).is_finite() {
                return Err(usage(format!(
                    "--balance-range span (high {hi} − low {lo}) overflows f32; \
                     use anchors whose difference is representable"
                )));
            }
        }

        // Dmax anchor: an explicit anchor is a corrected density — scene white
        // sits at a positive density above the base's `D = 0`, so a non-positive
        // / non-finite value (e.g. a sign typo) would brighten past white or blow
        // out. Reject it loudly; `Auto`/`None` need no value check.
        if let DmaxSource::Explicit(d) = curve.dmax() {
            positive("--d-max", &[d])?;
        }

        match curve {
            DensityCurve::Exponential(e) => {
                positive("--density-gamma", &[e.gamma])?;
            }
            DensityCurve::Sigmoid(s) => {
                // The S-curve is anchored on `[0, Dmax]` — both its white knee
                // and its black floor derive from the anchor — so `dmax = none`
                // (unity placement, no anchor) cannot drive it (design-spec §7.3).
                if s.dmax == DmaxSource::None {
                    return Err(usage(
                        "the sigmoid curve needs a display-white anchor (the default \
                         fixed anchor, --d-max <d>, or --auto-d-max); --no-d-max / \
                         `curve.dmax = none` is only supported by the exponential \
                         curve"
                            .into(),
                    ));
                }
                // Contrast (mid-density slope) must be positive AND bounded above:
                // an extreme slope collapses the S-curve into a hard black/white
                // threshold whose knees silently launder the blow-out into a
                // finite two-level image (highlights → exactly 1.0, shadows → the
                // floor) that trips *neither* the clip nor the non-finite counter
                // — a silent destruction the exponential curve avoids (it
                // overflows to +inf, which is counted). Cap it; use
                // `--density-curve exponential` for genuinely extreme contrast.
                positive("--sigmoid-contrast", &[s.contrast])?;
                if s.contrast > crate::algo::sigmoid::SIGMOID_CONTRAST_MAX {
                    return Err(usage(format!(
                        "--sigmoid-contrast ({}) must be <= {} (beyond this the \
                         S-curve is a hard threshold that silently destroys tonal \
                         detail; use --density-curve exponential)",
                        s.contrast,
                        crate::algo::sigmoid::SIGMOID_CONTRAST_MAX
                    )));
                }
                // Knee widths non-negative AND bounded above: 0 disables a knee, a
                // negative width would silently be treated as "off" by the curve,
                // and a huge *finite* width flattens the image into near-uniform
                // tone (giant shoulder → all-black, giant toe → all-white) with
                // samples that stay finite and in range — the same
                // silent-destruction class the contrast cap closes. Reject both
                // loudly.
                finite("--sigmoid-toe/--sigmoid-shoulder", &[s.toe, s.shoulder])?;
                let knee_max = crate::algo::sigmoid::SIGMOID_KNEE_MAX;
                if s.toe < 0.0 || s.shoulder < 0.0 || s.toe > knee_max || s.shoulder > knee_max {
                    return Err(usage(format!(
                        "--sigmoid-toe ({}) and --sigmoid-shoulder ({}) must be in \
                         [0, {knee_max}] (0 disables the knee; a larger width flattens \
                         the image into near-uniform tone without tripping the \
                         clip/non-finite counters)",
                        s.toe, s.shoulder
                    )));
                }
            }
        }
    }

    // Print: exposure / black point finite; gains positive. Highlight roll-off is a
    // non-negative amount — 0 disables it, and a negative value would be silently
    // ignored by the density render's soft-clip, so reject it loudly here.
    finite("--print-exposure", &[cfg.print.print_exposure])?;
    finite("--black-point", &[cfg.print.black_point])?;
    finite("--highlight-compress", &[cfg.print.highlight_compress])?;
    if cfg.print.highlight_compress < 0.0 {
        return Err(usage(format!(
            "--highlight-compress must be >= 0 (got {})",
            cfg.print.highlight_compress
        )));
    }
    // Explicit gains must be positive; the auto modes carry no value to check
    // here (estimated gains are guarded at the estimation point, exit 1). An auto
    // mode only has an effect through a print white-balance stage — density
    // reconstruction has one (both curves route through `finish_print`'s WB
    // slot); `simple` does not (its positive passes through untouched).
    // Whitelist the reconstruction that consumes the gains rather than blacklist
    // `simple`: a future reconstruction that also skips the print stage must
    // fail loudly here by default, not silently drop the requested estimation
    // (exit 0, no gains) — the "forgotten coupled spot" trap.
    match cfg.print.white_balance {
        WbSource::Explicit(gains) => positive("--white-balance", &gains)?,
        WbSource::GrayWorld | WbSource::Percentile
            if !matches!(cfg.reconstruction, Reconstruction::Density { .. }) =>
        {
            return Err(usage(
                "--auto-wb needs --reconstruction density (simple reconstruction \
                 has no print white-balance stage); pass explicit --white-balance \
                 gains instead, or switch reconstruction"
                    .into(),
            ));
        }
        WbSource::GrayWorld | WbSource::Percentile => {}
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Serialize a value as pretty JSON to a file; an I/O failure is a write error.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| NcError::Other(format!("serializing JSON: {e}")))?;
    std::fs::write(path, json)
        .map_err(|e| NcError::Write(format!("cannot write {}: {e}", path.display())))
}

/// Emit a report as JSON to stdout (kept clean) or `--report-file`. `none`
/// suppresses it entirely.
pub fn emit_report(report: &Report, format: ReportFormat, file: Option<&Path>) -> Result<()> {
    emit_json(report, format, file)
}

/// Emit any serializable report as JSON to stdout (kept clean) or a file. `none`
/// suppresses it entirely. Shared by the per-command [`Report`] and the roll-level
/// [`RollReport`].
fn emit_json<T: Serialize>(value: &T, format: ReportFormat, file: Option<&Path>) -> Result<()> {
    if format == ReportFormat::None {
        return Ok(());
    }
    match file {
        Some(p) => write_json(p, value),
        None => {
            let json = serde_json::to_string_pretty(value)
                .map_err(|e| NcError::Other(format!("serializing report: {e}")))?;
            println!("{json}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// lcms2 runtime-error handler (see CLAUDE.md's lcms2 gotcha)
// ---------------------------------------------------------------------------

/// Set when lcms2 reports a runtime error through the process-global handler.
static CMS_ERROR: AtomicBool = AtomicBool::new(false);

/// lcms2 error callback. Records that a color-management error occurred and
/// echoes it to stderr (stdout stays report-only). `cmsDoTransform` (under
/// `Transform::transform_in_place`) is infallible and Little CMS's *default*
/// handler silently discards errors, so this hook is the only way a runtime
/// transform/profile fault in `pipeline::color` becomes visible.
unsafe extern "C" fn cms_error_handler(
    _ctx: lcms2_sys::Context,
    code: u32,
    text: *const std::os::raw::c_char,
) {
    CMS_ERROR.store(true, Ordering::SeqCst);
    let msg = if text.is_null() {
        std::borrow::Cow::Borrowed("(no message)")
    } else {
        // SAFETY: lcms2 passes a NUL-terminated C string for the message text.
        unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy()
    };
    eprintln!("nc: lcms2 error [{code}]: {msg}");
}

/// Install the process-global lcms2 error handler at startup. `pipeline::color`
/// builds its profiles/transforms on lcms2's global context, and the safe `lcms2`
/// wrapper exposes the handler only per-`ThreadContext`, so we set the global one
/// through the `lcms2-sys` FFI directly.
fn install_cms_error_handler() {
    // SAFETY: `cms_error_handler` matches lcms2's LogErrorHandlerFunction ABI and
    // only touches an atomic + stderr, so it is sound to call from C on any thread.
    unsafe { lcms2_sys::cmsSetLogErrorHandler(Some(cms_error_handler)) }
}

/// Take and clear the "lcms2 logged an error" flag. The orchestrator checks it
/// right after the color transform runs, which the infallible
/// `transform_in_place` cannot report through its return value.
fn cms_error_occurred() -> bool {
    CMS_ERROR.swap(false, Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// stderr logging (never touches stdout — that stays report-only)
// ---------------------------------------------------------------------------

/// Verbosity-gated stderr logger. `--quiet` silences everything below an error;
/// `-v`/`-vv` enable progress `info` lines. Warnings always go to the JSON
/// report (via [`push_warning`]); this only controls the stderr echo.
struct Log {
    verbose: u8,
    quiet: bool,
}

impl Log {
    fn new(args: &ReportArgs) -> Self {
        Self {
            verbose: args.verbose,
            quiet: args.quiet,
        }
    }

    /// Progress line — only shown with `-v` (and never when `--quiet`).
    fn info(&self, msg: impl Display) {
        if !self.quiet && self.verbose >= 1 {
            eprintln!("nc: {msg}");
        }
    }

    /// Warning line — shown unless `--quiet` (the report keeps it either way).
    fn warn(&self, msg: &str) {
        if !self.quiet {
            eprintln!("nc: warning: {msg}");
        }
    }

    /// Warning line shown *regardless* of `--quiet`. For fail-soft telemetry
    /// failures, which are deliberately kept out of the JSON report (so `--strict`
    /// can't promote them) and would otherwise vanish entirely under `--quiet` —
    /// an opted-in feature failing must never be silent. Ordinary warnings use
    /// [`warn`](Self::warn), which `--quiet` suppresses since the report still
    /// records them.
    fn warn_always(&self, msg: &str) {
        eprintln!("nc: warning: {msg}");
    }
}

/// Record a warning into the report and echo it to stderr in one step, so the
/// two never drift.
fn push_warning(report: &mut Report, log: &Log, msg: String) {
    log.warn(&msg);
    report.warnings.push(msg);
}

/// Like [`push_warning`], but into a caller-owned buffer instead of a [`Report`].
/// [`convert_frame`] accumulates here so a frame that warns and *then* fails still
/// hands its warnings back to the caller (the report only rides out on success).
fn push_warning_buf(warnings: &mut Vec<String>, log: &Log, msg: String) {
    log.warn(&msg);
    warnings.push(msg);
}

// ---------------------------------------------------------------------------
// Entry point + dispatch
// ---------------------------------------------------------------------------

/// Parse arguments and run the requested subcommand. The single entry point the
/// binary's `main` calls. clap handles `--help`/`--version` and usage errors with
/// its own (exit-2-compatible) codes; everything else flows through [`NcError`].
pub fn run() -> Result<()> {
    // Install once at startup so any lcms2 runtime fault in `pipeline::color`
    // surfaces instead of being silently swallowed by the default no-op handler.
    install_cms_error_handler();
    let cli = Cli::parse();
    match cli.command {
        Command::Params => run_params(),
        Command::Convert(args) => run_convert(args),
        Command::Roll(args) => run_roll(args),
        Command::Inspect(args) => run_inspect(args),
        Command::Estimate(args) => run_estimate(args),
    }
}

/// `nc params` — print the full default parameter set as JSON to stdout.
fn run_params() -> Result<()> {
    let json = serde_json::to_string_pretty(&ResolvedConfig::default())
        .map_err(|e| NcError::Other(format!("serializing params: {e}")))?;
    println!("{json}");
    Ok(())
}

/// Best-effort stable key for path-collision checks. Canonicalize the path when
/// it exists (resolves symlinks and `..`); for a not-yet-created write target,
/// canonicalize its parent directory instead (`tmp/sub/../out.tiff` and
/// `tmp/out.tiff` must compare equal — `std::path::absolute` alone keeps the
/// `..` and would let them slip past the check), re-attaching the file name.
/// When even the parent doesn't exist, fall back to a lexical normalization of
/// the absolute form. A guard against accidental self-clobbering, not
/// adversarial links. Casing is preserved here; [`keys_collide`] applies the
/// case-insensitive comparison so a not-yet-created `out.tiff`/`OUT.TIFF` pair
/// (which can't be canonicalized to a shared casing) still collides.
fn collision_key(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(p) = std::fs::canonicalize(parent) {
            return p.join(name);
        }
    }
    lexical_absolute(path)
}

/// Absolute form with `.`/`..` components removed lexically (no filesystem
/// access). Last-resort key for paths whose parent doesn't exist yet; lexical
/// `..` removal can disagree with the filesystem across symlinked directories,
/// which is acceptable for an accident guard.
fn lexical_absolute(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether two collision keys refer to the same write target. Compares exactly
/// **or** ignoring ASCII case: on a case-insensitive filesystem (macOS/Windows
/// default) `out.tiff` and `OUT.TIFF` are the same file, but when neither exists
/// yet [`collision_key`] can't canonicalize them to a shared casing, so a
/// case-sensitive `==` would wrongly let one write clobber the other. Detecting
/// per-volume case sensitivity portably isn't cheap, so we **conservatively
/// over-reject**: this is an accident guard, and false-rejecting `out.tiff` vs
/// `OUT.TIFF` in a single invocation (a harmless annoyance) is the right trade
/// against false-accepting and silently overwriting the just-written output.
fn keys_collide(a: &Path, b: &Path) -> bool {
    a == b
        || a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Reject write targets that would clobber the input scan or one another —
/// e.g. `-o` equal to the input (destroys the negative), or `--report-file`
/// equal to the output/sidecar (truncates a just-written artifact) — all of
/// which would otherwise "succeed" with exit 0. Fail loudly up front instead.
/// Comparison is case-insensitivity-aware (see [`keys_collide`]) so a
/// case-only difference can't slip a second write onto the same file on a
/// case-insensitive filesystem.
fn ensure_write_targets_distinct(input: &Path, targets: &[(&str, &Path)]) -> Result<()> {
    let input_key = collision_key(input);
    let mut seen: Vec<(&str, PathBuf)> = Vec::with_capacity(targets.len());
    for (label, path) in targets {
        let key = collision_key(path);
        if keys_collide(&key, &input_key) {
            return Err(NcError::Usage(format!(
                "{label} ({}) would overwrite the input scan",
                path.display()
            )));
        }
        if let Some((other, _)) = seen.iter().find(|(_, k)| keys_collide(k, &key)) {
            return Err(NcError::Usage(format!(
                "{label} ({}) collides with {other}",
                path.display()
            )));
        }
        seen.push((label, key));
    }
    Ok(())
}

/// Reject the deprecated input-color CLI flags loudly before merge/convert.
///
/// `--assume-linear` (the old *combined* assertion) is a hard usage error with
/// migration guidance — it must never silently assert both axes. `--input-profile`
/// stays rejected for normal conversion (input-side ICC application has no
/// validated placement; it is reserved for the deferred
/// scanner-profile-before-density experiment). `convert`-only; `roll` takes its
/// input axes from the shared recipe, whose legacy `input.color` key is rejected
/// at load by [`reject_legacy_input_color`].
fn reject_deprecated_input_flags(o: &InputOverrides) -> Result<()> {
    if o.assume_linear {
        return Err(NcError::Usage(
            "--assume-linear was removed: it asserted transfer encoding AND measurement \
             meaning at once. Assert them independently — `--input-transfer linear` (transfer) \
             and, for raw scanner data, `--input-meaning scanner-device` (meaning)."
                .into(),
        ));
    }
    if let Some(p) = &o.input_profile {
        return Err(NcError::Unsupported(format!(
            "--input-profile {p}: input-side ICC application is not supported for normal \
             conversion; it is reserved for the deferred scanner-profile-before-density \
             experiment. SilverFast scans are decoded as linear scanner measurements."
        )));
    }
    Ok(())
}

/// Migration errors for the removed algorithm selector and simple-reconstruction
/// controls. nc is unreleased, so these flags survive only as hidden args that
/// emit actionable guidance — never as aliases (`reject_legacy_recipe_keys` is
/// the recipe-side mirror).
fn reject_removed_flags(args: &ConvertArgs) -> Result<()> {
    if let Some(name) = &args.algorithm {
        return Err(NcError::Usage(format!(
            "--algorithm {name} was removed: the selection is now the tagged \
             reconstruction — use `--reconstruction simple|density`, plus \
             `--density-curve exponential|sigmoid` for the density curve (the old \
             `--algorithm sigmoid` is `--reconstruction density --density-curve \
             sigmoid`). Recipes select it via the one `reconstruction` object \
             (design-spec §8)."
        )));
    }
    for (flag, present) in [
        (
            "--invert-white-balance",
            args.simple.invert_white_balance.is_some(),
        ),
        ("--clip-low", args.simple.clip_low.is_some()),
        ("--clip-high", args.simple.clip_high.is_some()),
    ] {
        if present {
            return Err(NcError::Usage(format!(
                "{flag} was removed: it is not a simple-reconstruction parameter — \
                 simple reconstruction ends at the direct unclamped positive \
                 `1 - scan/Dmin` (design-spec §7.1). Inversion white balance and the \
                 clip range return as print controls (print.white_balance / \
                 print.linear_range) when named output presets land."
            )));
        }
    }
    Ok(())
}

/// Which input axes were asserted via a **CLI flag** (vs the recipe) — threaded
/// into [`convert_frame`] so the resolver records literal CLI-vs-recipe
/// provenance. `roll` has no per-frame input flags, so it passes
/// [`InputFromCli::none`].
#[derive(Clone, Copy, Debug, Default)]
struct InputFromCli {
    transfer: bool,
    meaning: bool,
}

impl InputFromCli {
    /// No CLI input assertions (the recipe-driven `roll` case).
    fn none() -> Self {
        Self::default()
    }
}

/// Build the resolver's [`ContainerColorFacts`] from what the decoder parsed.
///
/// `io::decode` accepts *any* 3-channel 16-bit chunky RGB TIFF, not only genuine
/// SilverFast scans, so raw-mode provenance is derived from the authoritative
/// SilverFast **XMP mode metadata** ([`DecodeInfo::is_silverfast_raw_mode`],
/// `Company=LaserSoft Imaging` + `HDRScan=Yes`) rather than assumed or keyed on a
/// spoofable `Software` string / IR-plane presence: a generic / colorimetric /
/// processed RGB16 TIFF gets `raw_mode: None`, so its meaning resolves `Unknown`
/// and `convert` rejects it (unless the user explicitly asserts the axes). The
/// XMP `Gamma` feeds the descriptive-transfer axis — `Gamma≈1` corroborates
/// linear; a non-linear gamma on a raw-mode scan makes the transfer ambiguous
/// (contradiction → `Unknown` → rejected). `embedded_icc` is passed through for
/// inspection.
fn container_color_facts(info: &DecodeInfo) -> ContainerColorFacts {
    ContainerColorFacts {
        raw_mode: info
            .is_silverfast_raw_mode()
            .then_some(RawMode::SilverFastHdr),
        gamma: info
            .silverfast_xmp
            .as_ref()
            .map(|x| x.gamma.clone())
            .unwrap_or_default(),
        embedded_icc: info.embedded_icc.clone(),
    }
}

/// Reject a SilverFast **positive-mode** scan (`Negative=No`) loudly. Such a scan
/// is still raw linear scanner data, so it passes the transfer/meaning gate — but
/// converting it as a *negative* is silently wrong. This is a small,
/// clearly-scoped check (distinct from the transfer/meaning resolution) so it is
/// easy to lift when positive-mode support lands. `inspect` never calls it (it
/// reports the `Negative` flag via `decode.silverfast_xmp` instead).
fn reject_positive_mode(info: &DecodeInfo) -> Result<()> {
    if info.is_silverfast_positive_mode() {
        return Err(NcError::Unsupported(
            "input is a SilverFast positive-mode scan (XMP Negative=No); converting it as a \
             negative would be silently wrong. Positive-mode scans are not yet supported \
             (follow-up); scan in negative mode, or convert a negative scan."
                .into(),
        ));
    }
    Ok(())
}

/// The merged input assertions plus their CLI/recipe provenance, for the resolver.
fn input_assertions(cfg: &ResolvedConfig, from_cli: InputFromCli) -> InputAssertions {
    InputAssertions {
        transfer: cfg.input.transfer,
        meaning: cfg.input.meaning,
        transfer_from_cli: from_cli.transfer,
        meaning_from_cli: from_cli.meaning,
    }
}

/// Everything one frame's pipeline produced, for the orchestrator to emit or
/// aggregate. `convert` (single frame) reads all of it — the report to emit, and
/// `info` / `recipe_json` / `timings` / `loss` for its optional telemetry record;
/// `roll` reads only `report` (telemetry is `convert`-only, design-spec §9).
struct ConvertedFrame {
    report: Report,
    info: DecodeInfo,
    recipe_json: String,
    /// Per-stage wall clocks; `total` is left `0.0` for the orchestrator to fill
    /// from its own whole-run clock (this struct times only the stages here).
    timings: telemetry::TimingInfo,
    loss: EncodeReport,
}

/// The per-frame conversion core: decode → film-base estimate → render → optional
/// IR export → encode + effective-recipe sidecar. Pure of the operational
/// concerns (`--strict` gating, report emission, telemetry) the callers layer on
/// top, so `convert` and `roll` share one byte-for-byte identical frame path.
///
/// The caller must have already validated `cfg` ([`validate`]), rejected the
/// deprecated input flags ([`reject_deprecated_input_flags`]), and checked
/// write-target collisions; `convert_frame` assumes a sound config and a safe
/// `output` path. It resolves and gates the input color semantics itself
/// (transfer + meaning, [`input_semantics`]) after decode, before the render. It
/// never
/// writes to stdout (the report rides back in [`ConvertedFrame`]); progress and
/// warnings go to stderr via `log`.
///
/// Warnings are accumulated into the caller-owned `warnings` buffer (echoed to
/// stderr as they occur) so they survive an early failure: on success they are
/// also moved into the returned report, but on the `Err` path they stay in the
/// caller's buffer — the roll orchestrator attaches them to a failed frame's
/// report. The caller decides whether `--strict` promotes them.
// One over clippy's argument cap: the orchestration core legitimately threads
// the frame identity, config, and the two report-provenance facts; a params
// struct for two one-off values would only obscure the call sites.
#[allow(clippy::too_many_arguments)]
fn convert_frame(
    command: &'static str,
    input: &Path,
    output: &Path,
    cfg: &ResolvedConfig,
    input_from_cli: InputFromCli,
    dmax_setting: DmaxSetting,
    log: &Log,
    warnings: &mut Vec<String>,
) -> Result<ConvertedFrame> {
    let mut report = Report {
        command: Some(command),
        input: Some(input.to_path_buf()),
        output: Some(output.to_path_buf()),
        // The effective recipe (the sidecar's exact object), so
        // `recipe.reconstruction` is the tagged reconstruction schema.
        recipe: Some(cfg.clone()),
        film_base_source: Some(cfg.film_base.source.clone()),
        ..Report::default()
    };

    // (A customized `--density-gamma` under a resolved sigmoid curve is now a
    // merge-time usage error — the tagged curve made the old warned-and-ignored
    // combination unrepresentable, so no warning lives here anymore.)

    // Domain guard for an explicit / reference-derived `Dmax` — see
    // `explicit_dmax_domain_warning`. Fires the (`--strict`-promotable) warning when
    // the anchor's density domain no longer matches what the render subtracts it from.
    if let Some(msg) = explicit_dmax_domain_warning(cfg) {
        push_warning_buf(warnings, log, msg);
    }

    // Stage 1 — decode. Per-stage wall clocks feed the telemetry record only
    // (they never touch the image/sidecar); measure them regardless of whether
    // telemetry is enabled so the render path is uniform.
    let stage_started = Instant::now();
    let (image, info) = decode(input)?;
    let decode_ms = elapsed_ms(stage_started);
    log.info(format_args!(
        "decoded {:?} {}x{} (ir={})",
        info.format, info.width, info.height, info.ir_present
    ));
    for w in &info.warnings {
        push_warning_buf(warnings, log, w.clone());
    }

    // Stage 1b — resolve input color semantics (transfer + measurement meaning as
    // independent axes) and gate: only a supported linear transfer + scanner-device
    // meaning may enter Dmin/density. An explicit assertion contradicting container
    // structure is a usage error here; an ambiguous/unsupported input is a loud
    // unsupported error — never a quietly-wrong image. The resolution rides into
    // the report (with evidence + a safe ICC summary) regardless.
    let input_meta = input_semantics::resolve(
        &container_color_facts(&info),
        &input_assertions(cfg, input_from_cli),
    )?;
    let input_report = InputColorReport::from_metadata(&input_meta);
    if input_report.icc_unparsable() {
        push_warning_buf(
            warnings,
            log,
            "embedded ICC profile present but could not be parsed for a summary".into(),
        );
    }
    input_semantics::require_convertible(&input_meta)?;
    report.input_color = Some(input_report);

    // A SilverFast positive-mode scan passes the transfer/meaning gate (it is raw
    // linear scanner data) but must not be converted as a negative — reject it
    // loudly with a distinct message rather than silently misconvert.
    reject_positive_mode(&info)?;

    // `--export-ir` on a scan with no IR plane can't be honored: fail fast,
    // before writing any output, rather than after the main encode.
    let export_ir = cfg.input.export_ir.as_deref().map(PathBuf::from);
    if export_ir.is_some() && !info.ir_present {
        return Err(NcError::Unsupported(
            "--export-ir requested but the input has no IR plane (HDRi input only)".into(),
        ));
    }
    // Note an IR plane that's carried but not consumed — but only when it isn't
    // being exported: `--export-ir` is the user handling it, so warning (and
    // failing under `--strict`) would be wrong. This keeps `--strict --export-ir`
    // a usable workflow on the primary HDRi format.
    if info.ir_present && export_ir.is_none() {
        push_warning_buf(
            warnings,
            log,
            "input carries an IR plane; it is preserved but not used in Step 1 \
             (use --export-ir to write it out)"
                .into(),
        );
    }

    // Stage 2 — film-base estimate. Resolved before the render so its quality
    // warnings (non-uniform region, cross-edge disagreement) are pushed — and so
    // echoed to stderr — *before* the fallible render runs, and ride out in the
    // JSON report on a successful run. (A hard render failure propagates its error
    // and exit code like every other error path and emits no report; the stderr
    // warnings still stand.)
    let stage_started = Instant::now();
    let base = film_base::estimate(&image, &cfg.film_base)?;
    let film_base_ms = elapsed_ms(stage_started);
    report.film_base = Some(base.base);
    for w in base.warnings {
        push_warning_buf(warnings, log, w);
    }

    // Clear any stale lcms2 flag so only errors from *this* render are counted.
    let _ = cms_error_occurred();
    // Stages 3–4 — reconstruction → legacy print → output color transform.
    let rendered = stages::render(
        &image,
        &base.base,
        &cfg.reconstruction,
        &cfg.print,
        &cfg.output,
    )?;
    // lcms2 transform/profile failures reach us only through the global handler
    // (`transform_in_place` is infallible), so check the flag it sets.
    if cms_error_occurred() {
        return Err(NcError::Other(
            "color management (lcms2) reported a runtime error; see stderr".into(),
        ));
    }
    report.dmax = rendered.convert.dmax;
    report.white_balance = rendered.convert.white_balance;
    report.balance_range = rendered.convert.balance_range;
    report.reconstruction_result = Some(reconstruction_result(
        &cfg.reconstruction,
        rendered.convert.dmax,
        dmax_setting,
    ));
    // Stamp the pinned working-space interpretation (design-spec §8). NC film RGB
    // v1 is the fixed rule "reconstructed film RGB is linear Rec.709/D65", applied
    // on every path (`pipeline::working_space::WORKING_MAPPING_ID`); the typed
    // ACEScg mapper realizes it for the named presets that consume `AcesCgImage`.
    report.working_mapping = Some(working_space::WORKING_MAPPING_ID);

    // Report an `auto` BigTIFF promotion (an automatic decision the user didn't
    // explicitly request).
    if cfg.output.bigtiff == BigTiff::Auto
        && encode::plans_bigtiff(&cfg.output, &rendered.image, rendered.icc.len())
    {
        push_warning_buf(
            warnings,
            log,
            "output promoted to BigTIFF (would exceed the classic 4 GiB TIFF limit)".into(),
        );
    }

    // Optional IR export — before the main encode, so a failing IR write fails
    // the run without first writing the primary output/sidecar.
    let mut ir_export_ms = None;
    if let Some(path) = &export_ir {
        let stage_started = Instant::now();
        encode::export_ir(&image, cfg.output.depth(), path)?;
        ir_export_ms = Some(elapsed_ms(stage_started));
        log.info(format_args!("wrote IR plane {}", path.display()));
        report.ir_exported = Some(path.clone());
    }

    // Stage 5 — encode + effective-recipe sidecar.
    let stage_started = Instant::now();
    let loss = encode::encode(&rendered.image, &cfg.output, Some(&rendered.icc), output)?;
    let encode_ms = elapsed_ms(stage_started);
    report.loss = Some(loss);
    if loss.any_loss() {
        push_warning_buf(
            warnings,
            log,
            format!(
                "output lost {} clipped and {} non-finite of {} samples ({:.2}%)",
                loss.clipped_total(),
                loss.non_finite,
                loss.total_samples,
                loss.loss_fraction() * 100.0,
            ),
        );
    }
    // A non-finite sample is a numerical fault, not routine gamut clipping — make
    // sure it is never fully silenced (the `--quiet --report none` combination
    // would otherwise suppress both channels of the warning above).
    if loss.non_finite > 0 && log.quiet {
        eprintln!(
            "nc: warning: {} non-finite (NaN/inf) output sample(s) — numerical fault",
            loss.non_finite
        );
    }

    let recipe_json = serde_json::to_string_pretty(cfg)
        .map_err(|e| NcError::Other(format!("serializing recipe for sidecar: {e}")))?;
    encode::write_sidecar(output, &recipe_json)?;
    log.info(format_args!("wrote {}", output.display()));

    // Success: hand the accumulated warnings to the report (the buffer is the
    // caller's; taking them keeps the two from double-counting). On the `Err`
    // paths above the buffer is left populated for the caller instead.
    report.warnings = std::mem::take(warnings);

    Ok(ConvertedFrame {
        report,
        info,
        recipe_json,
        timings: telemetry::TimingInfo {
            total: 0.0,
            decode: decode_ms,
            film_base: film_base_ms,
            algorithm: rendered.timings.algorithm_ms,
            color: rendered.timings.color_ms,
            encode: encode_ms,
            ir_export: ir_export_ms,
        },
        loss,
    })
}

/// Domain guard for an explicit / reference-derived `Dmax`: the warning message when
/// the anchor's density domain no longer matches what the render subtracts it from,
/// else `None`.
///
/// An explicit anchor is a base-relative density `D` measured under the *default*
/// density correction (`estimate --d-max-region`, or a hand-set `--d-max`), but the
/// render subtracts it from the corrected density
/// `D′ = scale·D + offset + regional-balance ramps`. Anything that moves `D′` off that
/// default domain lands the anchor in a different density domain than the render
/// subtracts it from, uniformly mis-anchoring every frame — silently. Two such knobs:
/// non-default density-scale/offset, and a non-neutral regional (shadow/highlight)
/// balance — the balance ramps add into `D′`
/// (`D′_c = B_c + shadow_c·w_lo + highlight_c·w_hi`) before the `− Dmax`. The caller
/// warns loudly (`--strict`-promotable) so the user re-measures the anchor under these
/// density params (or resets them).
///
/// `Fixed`/`Auto` are already in the corrected domain (the nominal is defined there;
/// `Auto` measures the post-correction, post-balance buffer), and `simple` has no
/// density domain at all, so the guard is scoped to an explicit anchor on a
/// density reconstruction — either curve; both subtract the anchor from `D′`.
/// (Regional balance varies per-tone, so it cannot be *folded into* a scalar
/// anchor — but a non-neutral balance still shifts `D′`, so a fixed anchor still
/// mis-anchors; hence it belongs in this guard.)
fn explicit_dmax_domain_warning(cfg: &ResolvedConfig) -> Option<String> {
    let Reconstruction::Density { density, curve } = &cfg.reconstruction else {
        return None;
    };
    if !matches!(curve.dmax(), DmaxSource::Explicit(_)) {
        return None;
    }
    let default_density = DensityParams::default();
    let nondefault_correction =
        density.scale != default_density.scale || density.offset != default_density.offset;
    let nonneutral_balance = density.shadow_balance != default_density.shadow_balance
        || density.highlight_balance != default_density.highlight_balance;
    if nondefault_correction || nonneutral_balance {
        Some(format!(
            "explicit --d-max is a base-relative density measured under default \
             density correction, but density-scale ({:?}) / density-offset ({:?}) / \
             regional balance (shadow {:?}, highlight {:?}) are non-default — the \
             anchor is in a different density domain than the curve subtracts it \
             from, uniformly mis-anchoring the frame; re-measure --d-max under these \
             density params or reset them to defaults",
            density.scale, density.offset, density.shadow_balance, density.highlight_balance
        ))
    } else {
        None
    }
}

/// Plausibility warning for a measured reference `Dmax` (`estimate --d-max-region`), or
/// `None` when it is a credible fully-exposed leader. Never a hard error (thin/unusual
/// stock varies) — a `--strict`-promotable warning for the user's manual review, since
/// a too-low anchor silently blows the roll too bright. Two distinct failure shapes, so
/// at most one fires:
///
/// - (a) the gray mean itself is below the leader floor — the whole frame is thin (the
///   weakest channel is necessarily low too, so this subsumes shape (b); report it as
///   the frame-wide diagnosis);
/// - (b) the gray mean is plausible, but the weakest channel sits barely above the base
///   (essentially unexposed) — a colored / wrong region, which the scalar mean alone
///   hides. A genuine leader is near-opaque in *every* channel, so the check is
///   per-channel on the minimum, not just the average.
fn reference_dmax_plausibility_warning(measured: &density::ReferenceDmax) -> Option<String> {
    let dmax = measured.scalar;
    let min_channel = measured
        .per_channel
        .iter()
        .copied()
        .fold(f32::INFINITY, f32::min);
    if dmax < density::MIN_PLAUSIBLE_REFERENCE_DMAX {
        Some(format!(
            "measured reference Dmax {dmax} is implausibly low for a fully-exposed \
             leader (expected ≳ {:.1} density) — the region may not be a fully-exposed \
             leader; verify --d-max-region before freezing this anchor",
            density::MIN_PLAUSIBLE_REFERENCE_DMAX
        ))
    } else if min_channel < density::MIN_PLAUSIBLE_REFERENCE_DMAX {
        Some(format!(
            "measured reference Dmax {dmax} is plausible on the gray average, but its \
             weakest channel density ({min_channel}, per-channel {:?}) is implausibly \
             low (expected ≳ {:.1}) — the region is colored or not a fully-exposed \
             leader (a genuine leader is near-opaque in every channel); verify \
             --d-max-region before freezing this anchor",
            measured.per_channel,
            density::MIN_PLAUSIBLE_REFERENCE_DMAX
        ))
    } else {
        None
    }
}

/// `nc convert` — the full pipeline: decode → film-base → algorithm → output
/// color transform → encode (+ sidecar, + optional IR export). Warnings are
/// collected into the report and echoed to stderr; `--strict` promotes any of
/// them to a non-zero exit.
fn run_convert(args: ConvertArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    reject_deprecated_input_flags(&args.input_opts)?;
    reject_removed_flags(&args)?;
    let loaded = load_recipe(args.recipe_in.as_deref())?;
    // Dmax provenance for the report: a CLI flag beats the recipe key beats the
    // default — the same precedence the merge applies to the value itself.
    let dmax_setting = if dmax_flag_given(&args.dmax) {
        DmaxSetting::Cli
    } else if loaded.curve_dmax_present {
        DmaxSetting::Recipe
    } else {
        DmaxSetting::Default
    };
    let cfg = merge(loaded.cfg, &args)?;
    validate(&cfg)?;

    // Guard every write target against the input and against each other before
    // anything is decoded or written.
    let sidecar = encode::sidecar_path(&args.output);
    // The persistent `--telemetry` log is also a write target: a
    // `NC_TELEMETRY_LOG` / default path that collides with the input or an
    // artifact is rejected up front like `--telemetry-file`, so an odd log path
    // can't silently append into (and corrupt) the input scan or the output.
    // Resolved here so the borrow outlives `targets`.
    let telemetry_log = if args.telemetry {
        telemetry::default_log_path()
    } else {
        None
    };
    let mut targets: Vec<(&str, &Path)> =
        vec![("--output", &args.output), ("the sidecar", &sidecar)];
    if let Some(p) = &args.dump_params {
        targets.push(("--dump-params", p));
    }
    if let Some(p) = args.report.report_file.as_deref() {
        targets.push(("--report-file", p));
    }
    if let Some(p) = cfg.input.export_ir.as_deref() {
        targets.push(("--export-ir", Path::new(p)));
    }
    // A `--telemetry-file` pointing at a real artifact would clobber it (the
    // record is written last, after the output). A path collision is a config
    // error, so it fails loudly up front like the other targets — distinct from a
    // telemetry *write* failure, which is fail-soft (handled after the conversion).
    // `-` (stdout) is not a filesystem target, so it's excluded from the check.
    if let Some(p) = telemetry_file_target(&args) {
        targets.push(("--telemetry-file", p));
    }
    if let Some(p) = &telemetry_log {
        targets.push(("the telemetry log", p));
    }
    ensure_write_targets_distinct(&args.input, &targets)?;

    if let Some(path) = &args.dump_params {
        write_json(path, &cfg)?;
    }
    // `--seed` is reserved (no stochastic step in Step 1) but accepted so the
    // documented flag isn't rejected; nothing consumes it yet.
    let _ = args.seed;

    // The per-frame pipeline core (decode → film-base → render → encode +
    // sidecar), shared byte-for-byte with `roll`. Operational concerns the two
    // orchestrators layer differently — report emission, `--strict` gating,
    // telemetry — stay out here.
    let mut warnings = Vec::new();
    let ConvertedFrame {
        mut report,
        info,
        recipe_json,
        timings: stage_timings,
        loss,
    } = convert_frame(
        "convert",
        &args.input,
        &args.output,
        &cfg,
        InputFromCli {
            transfer: args.input_opts.input_transfer.is_some(),
            meaning: args.input_opts.input_meaning.is_some(),
        },
        dmax_setting,
        &log,
        &mut warnings,
    )?;

    let total_ms = elapsed_ms(started);
    report.elapsed_ms = Some(total_ms);

    // Emit the report before the `--strict` gate so the machine-readable record
    // lands even when a warning then fails the run. (A hard I/O error above
    // returns earlier — its exit code and stderr message are the signal there.)
    emit_report(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
    )?;

    // `--strict` promotes any present warning to a non-zero exit. Decide it here,
    // *before* telemetry: a telemetry record's existence is the success signal
    // (there is no `outcome.success` field — see telemetry-strategy), so a run
    // that is about to exit non-zero must not leave a record that would read as a
    // successful run. The report emitted above already carries the warning detail
    // either way.
    let strict_failure = args.strict && !report.warnings.is_empty();

    // Telemetry (opt-in) is emitted after the deterministic output + sidecar are
    // written and only reads their facts, so it can't perturb them. It is
    // best-effort: a write failure is warned on stderr and never fails the run
    // (and `--strict` does not promote it), so it runs *after* the report and is
    // kept out of `report.warnings` — see `emit_telemetry`. Skipped on a
    // `--strict` failure so the log stays "one record per successful run".
    if telemetry_requested(&args) && !strict_failure {
        // `convert_frame` measured the per-stage wall clocks; the total is this
        // orchestrator's whole-run clock.
        let mut timings = stage_timings;
        timings.total = total_ms;
        emit_telemetry(
            &args,
            &cfg,
            &info,
            timings,
            loss,
            &recipe_json,
            &report,
            &log,
            telemetry_log.as_deref(),
        );
    }

    if strict_failure {
        return Err(NcError::Other(format!(
            "--strict: {} warning(s) present (see report)",
            report.warnings.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Roll (batch) — plan → recipe → apply, the batch-apply scaffold
// ---------------------------------------------------------------------------

/// A `--frames` manifest: an explicit list of frames to convert, each optionally
/// carrying its own output path and a partial-recipe override. `deny_unknown_fields`
/// so a typo'd top-level key is a loud error, not a silently-ignored frame list.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollManifest {
    frames: Vec<ManifestFrame>,
}

/// One frame in a `--frames` manifest. `params` is a *partial* recipe (any subset
/// of the [`ResolvedConfig`] shape) deep-merged onto the shared recipe for this
/// frame only — the frame-local override mechanism. `deny_unknown_fields` guards
/// the entry keys; the merged `params` are validated when deserialized back to a
/// `ResolvedConfig`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFrame {
    input: PathBuf,
    #[serde(default)]
    output: Option<PathBuf>,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

/// A frame resolved for conversion: where to read and write, and the effective
/// config (the shared recipe with any per-frame manifest override merged on top).
#[derive(Debug)]
struct PlannedFrame {
    input: PathBuf,
    output: PathBuf,
    cfg: ResolvedConfig,
    /// The per-frame override applied (manifest `params`), echoed into the roll
    /// report so a reader sees exactly what differed for this frame; `None` when
    /// the frame ran the shared recipe unchanged.
    overrides: Option<serde_json::Value>,
    /// Where this frame's `reconstruction.curve.dmax` setting came from (shared
    /// recipe / per-frame override ⇒ `Recipe`, else `Default`; roll has no
    /// per-frame flags) — the report's `Dmax` provenance.
    dmax_setting: DmaxSetting,
}

/// The roll-level JSON report emitted on stdout (or `--report-file`): the shared
/// frozen recipe *configuration* once, any roll-level warnings, the per-frame
/// status list, and a summary. The shared recipe here is the config every frame
/// was converted from; each frame additionally reports the *resolved* base/`Dmax`
/// it used (a redundant echo when the recipe pins an explicit base, meaningful
/// under an `auto`/`region` base that resolves per frame).
#[derive(Debug, Serialize)]
struct RollReport {
    command: &'static str,
    /// The shared frozen recipe configuration every frame was converted from —
    /// where the roll-fixed `film_base` / `density.dmax` config lives, once.
    recipe: ResolvedConfig,
    /// Roll-level warnings not tied to a single frame (e.g. the film base is not
    /// frozen because the shared recipe's `film_base.source` is not `explicit`).
    /// Echoed to stderr and, like per-frame warnings, promoted to a failing exit
    /// by `--strict`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    frames: Vec<FrameReport>,
    summary: RollSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<f64>,
}

/// Per-frame entry inside a [`RollReport`]. The per-frame *identity*
/// (`input`/`output`/`warnings`/`overrides`) lives here; the ok-vs-failed
/// *payload* is the data-carrying [`FrameStatus`] enum, so an "ok" frame can't
/// carry an `error` and a "failed" frame can't carry a film base — states the old
/// `status: &str` + all-`Option` layout could encode. `warnings` is common to
/// both outcomes: a frame that warns and *then* fails still reports its warnings
/// (they are echoed to stderr as they occur and preserved here regardless).
#[derive(Debug, Serialize)]
struct FrameReport {
    input: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<PathBuf>,
    /// The outcome payload, flattened so its `status` discriminator and fields
    /// serialize as flat sibling keys (`"status":"ok"`, `film_base`, … / `error`).
    #[serde(flatten)]
    status: FrameStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    /// The per-frame recipe override applied (manifest `params`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<serde_json::Value>,
}

/// The ok-vs-failed payload of a [`FrameReport`], each variant carrying only the
/// fields legal for that outcome. Internally tagged (`#[serde(tag = "status")]`)
/// and flattened into `FrameReport`, so it serializes the flat
/// `"status":"ok"`/`"failed"` discriminator with the payload as sibling keys —
/// the same wire shape the old `status: &str` + `error`/payload `Option`s
/// produced, minus the illegal combinations.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum FrameStatus {
    /// A converted frame: the resolved anchors it used (mirrors the relevant
    /// single-frame [`Report`] fields). Each is `None`/omitted when the algorithm
    /// or settings didn't produce it (e.g. `simple` has no `dmax`).
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        film_base: Option<FilmBase>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dmax: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        white_balance: Option<[f32; 3]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        balance_range: Option<[f32; 2]>,
        /// Resolved input color semantics (transfer + meaning + evidence + ICC
        /// summary) the frame ran on — mirrors the single-frame `Report` field so a
        /// roll frame reports the same input semantics `convert` does. Boxed: this
        /// is the one large field, and unboxed it makes `Ok` dwarf `Failed`
        /// (`clippy::large_enum_variant`); `Box` serializes transparently.
        #[serde(skip_serializing_if = "Option::is_none")]
        input_color: Option<Box<InputColorReport>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        loss: Option<EncodeReport>,
    },
    /// A frame that failed to convert: the failure message. The roll records it
    /// and continues (the loud non-zero exit is the batch-level signal).
    Failed { error: String },
}

/// Roll totals — a quick machine-readable tally alongside the per-frame list.
#[derive(Debug, Serialize)]
struct RollSummary {
    total: usize,
    succeeded: usize,
    failed: usize,
}

/// Whether a path has a `.tif`/`.tiff` extension (case-insensitive) — the filter
/// for expanding a directory argument into frames.
fn has_tiff_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
        .unwrap_or(false)
}

/// Expand one positional input into frame paths: a directory yields its
/// `.tif`/`.tiff` files (sorted for determinism); anything else passes through
/// verbatim (a missing file surfaces later as a per-frame decode error).
fn expand_input(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        let read_dir = std::fs::read_dir(path).map_err(|e| {
            NcError::Usage(format!(
                "cannot read input directory {}: {e}",
                path.display()
            ))
        })?;
        // Propagate a per-entry read error rather than dropping it: a silently
        // skipped entry would shorten the batch without a word (fail-loud
        // violation). Same usage-error class (exit 2) as failing to open the dir.
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|e| {
                NcError::Usage(format!(
                    "cannot read an entry in input directory {}: {e}",
                    path.display()
                ))
            })?;
            let p = entry.path();
            if p.is_file() && has_tiff_ext(&p) {
                entries.push(p);
            }
        }
        entries.sort();
        out.extend(entries);
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}

/// Default per-frame output name in the out-dir: `<input-stem>_positive.tiff`.
fn default_output_name(input: &Path, out_dir: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".to_string());
    out_dir.join(format!("{stem}_positive.tiff"))
}

/// Resolve a frame's output path: a manifest's explicit path (absolute used
/// verbatim, relative joined onto the out-dir) or the default `<stem>_positive.tiff`.
fn resolve_frame_output(explicit: Option<&Path>, input: &Path, out_dir: &Path) -> PathBuf {
    match explicit {
        Some(o) if o.is_absolute() => o.to_path_buf(),
        Some(o) => out_dir.join(o),
        None => default_output_name(input, out_dir),
    }
}

/// Deep-merge `overlay` into `base`: JSON objects merge key-by-key (recursively),
/// any other value replaces. Layers a per-frame partial-recipe override onto the
/// shared recipe's JSON before it is deserialized back to a validated
/// [`ResolvedConfig`] — a partial override keeps the shared values it doesn't
/// mention (a plain `serde` deserialize of the partial would reset them to
/// defaults instead).
///
/// Switching a multi-variant enum via an override is safe, not silent: the merged
/// value must still deserialize as that enum. Two tagged shapes need a
/// variant-switch rule instead of the key-by-key merge:
///
/// - **Externally tagged** (e.g. [`FilmBaseSource`]): a one-key map
///   (`{"region":[…]}`). Flipping it to another variant (`{"explicit":[…]}`)
///   must *replace* the whole map — a key-by-key merge would union the tags
///   into `{"region":…, "explicit":…}`, which no externally-tagged enum can
///   deserialize, turning an override that should apply into a confusing
///   `from_value` rejection. [`is_variant_switch`] catches exactly that
///   signature (both sides single-key objects with *different* keys).
/// - **Internally tagged** (the `reconstruction` object and its tagged
///   `curve`): a `"type"` field alongside the variant's own fields. Flipping
///   the `type` must not deep-merge either — the base's stale variant-specific
///   fields would survive (`density`/`curve` under a switch to `simple`,
///   `gamma` under a switch to `sigmoid`) and the fail-loud deserializer would
///   reject the union. [`internally_tagged_switch`] replaces the object with
///   the overlay, carrying the base's `dmax` when the overlay doesn't set it —
///   the one field the curve variants deliberately share (a roll-fixed anchor
///   survives an exponential↔sigmoid switch), mirroring the CLI [`merge`],
///   which carries `curve.dmax()` across a `--density-curve` switch.
///
/// A malformed override is still rejected loudly by the `from_value` in
/// [`resolve_frames`], never applied half-merged.
fn merge_json(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    if is_variant_switch(base, overlay) {
        *base = overlay.clone();
        return;
    }
    if let Some(switched) = internally_tagged_switch(base, overlay) {
        *base = switched;
        return;
    }
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                merge_json(b.entry(k.clone()).or_insert(serde_json::Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// The externally-tagged-enum-variant-switch signature: `base` and `overlay` are
/// both single-key objects with *different* keys (e.g. `{"region":[…]}` vs
/// `{"explicit":[…]}`). Deep-merging such a pair would leave a two-tag object that
/// no externally-tagged enum deserializes, so [`merge_json`] replaces it wholesale
/// instead. A unit variant serializes as a bare string (`"auto"`), not an object,
/// so switching to/from it never reaches here — the plain replace arm handles it.
fn is_variant_switch(base: &serde_json::Value, overlay: &serde_json::Value) -> bool {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            b.len() == 1 && o.len() == 1 && b.keys().next() != o.keys().next()
        }
        _ => false,
    }
}

/// The internally-tagged variant switch: `base` and `overlay` are both objects
/// carrying a `"type"` string discriminator with *different* values — in this
/// recipe schema that is the `reconstruction` object (`simple`/`density`) and
/// its tagged `curve` (`exponential`/`sigmoid`); nothing else uses an internal
/// tag. Returns the replacement object, or `None` when this isn't a type
/// switch (same/absent tags fall through to the ordinary deep merge, so a
/// same-variant partial override still keeps its siblings).
///
/// The replacement is the overlay itself, plus the base's `dmax` when the
/// overlay doesn't set one: `dmax` is the single field the curve variants
/// share by design — a roll-fixed display-white anchor is curve-independent
/// calibration, so a per-frame `{"curve":{"type":"sigmoid"}}` override keeps
/// the roll's frozen anchor exactly as the CLI's `--density-curve` switch does
/// ([`merge`]). Every other base field is variant-specific and must not
/// survive (the deserializer would loudly reject the union). The
/// `reconstruction`-level switch has no `dmax` key, so the carry is simply
/// inert there (`schema_version` needs no carry — omitted input defaults to
/// the one supported version).
fn internally_tagged_switch(
    base: &serde_json::Value,
    overlay: &serde_json::Value,
) -> Option<serde_json::Value> {
    let (b, o) = (base.as_object()?, overlay.as_object()?);
    let (base_type, overlay_type) = (b.get("type")?.as_str()?, o.get("type")?.as_str()?);
    if base_type == overlay_type {
        return None;
    }
    let mut switched = o.clone();
    if !switched.contains_key("dmax")
        && let Some(dmax) = b.get("dmax")
    {
        switched.insert("dmax".to_string(), dmax.clone());
    }
    Some(serde_json::Value::Object(switched))
}

/// Load a `--frames` manifest. A read failure or invalid/unknown-key JSON is a
/// usage error (a config mistake), like [`load_recipe`].
fn load_manifest(path: &Path) -> Result<RollManifest> {
    let txt = std::fs::read_to_string(path).map_err(|e| {
        NcError::Usage(format!(
            "cannot read --frames manifest {}: {e}",
            path.display()
        ))
    })?;
    serde_json::from_str(&txt)
        .map_err(|e| NcError::Usage(format!("invalid --frames manifest {}: {e}", path.display())))
}

/// Roll mode writes one output per frame into a shared directory, so a single
/// `input.export_ir` path — which every frame would overwrite — is nonsensical.
/// Reject it loudly rather than silently clobbering one IR file N times.
fn reject_roll_unsupported(cfg: &ResolvedConfig) -> Result<()> {
    if cfg.input.export_ir.is_some() {
        return Err(NcError::Usage(
            "input.export_ir (--export-ir) is not supported in roll mode: it names a \
             single path that every frame would overwrite; export the IR plane per \
             frame with `nc convert` instead"
                .into(),
        ));
    }
    Ok(())
}

/// Roll pre-flight: reject an input assertion that can never yield a convertible
/// frame **before** decoding the first (100+ MB) scan — restoring the up-front
/// fail-fast the old `reject_unsupported_input_color` gave (the per-file gate now
/// lives inside `convert_frame`, after the decode).
///
/// Only `input.meaning = colorimetric` is unconditionally unsupported regardless
/// of the file (colorimetric/encoded negatives have no inverse-transfer /
/// reconstruction path, so `require_convertible` rejects them for every frame).
/// The other axes' convertibility depends on per-file structural evidence
/// (unknown until decode), so they stay gated per frame. Applied to both the
/// shared recipe and each resolved per-frame override.
fn reject_roll_unsupported_input(cfg: &ResolvedConfig) -> Result<()> {
    if cfg.input.meaning == MeaningAssertion::Colorimetric {
        return Err(NcError::Unsupported(
            "input.meaning = colorimetric is unsupported for every frame: colorimetric / \
             encoded negatives have no inverse-transfer/reconstruction path yet. Remove it \
             or assert a scanner-device meaning."
                .into(),
        ));
    }
    Ok(())
}

/// Build the per-frame plan from the `--frames` manifest or the positional inputs,
/// resolving each frame's effective config (shared recipe + any per-frame
/// override) and output path. Config errors (a bad override, an unsupported knob)
/// fail loudly here, before any frame is converted; runtime errors (a bad decode,
/// a degenerate base) surface per frame during conversion. A per-frame override
/// that touches a roll-fixed calibration (`film_base` or `density.dmax`) is not
/// rejected — it is applied, with a loud roll-level warning pushed to
/// `roll_warnings` (like the not-frozen warning), so a deliberate per-frame value
/// stays possible while the color-consistency break is surfaced and
/// `--strict`-promotable.
fn resolve_frames(
    args: &RollArgs,
    shared: &ResolvedConfig,
    shared_dmax_present: bool,
    roll_warnings: &mut Vec<String>,
    log: &Log,
) -> Result<Vec<PlannedFrame>> {
    let out_dir = args.out_dir.as_path();
    let shared_setting = if shared_dmax_present {
        DmaxSetting::Recipe
    } else {
        DmaxSetting::Default
    };
    let mut planned = Vec::new();
    match &args.frames {
        Some(manifest_path) => {
            let manifest = load_manifest(manifest_path)?;
            if manifest.frames.is_empty() {
                return Err(NcError::Usage(format!(
                    "--frames manifest {} lists no frames",
                    manifest_path.display()
                )));
            }
            // The shared recipe as JSON, so a per-frame partial override can be
            // deep-merged onto it and deserialized back with `deny_unknown_fields`.
            let shared_value = serde_json::to_value(shared)
                .map_err(|e| NcError::Other(format!("serializing shared recipe: {e}")))?;
            for mf in manifest.frames {
                let (cfg, overrides, dmax_setting) = match mf.params {
                    Some(ov) => {
                        // A per-frame override carrying a removed legacy key gets
                        // the same pinned migration guidance as the shared recipe,
                        // not an opaque `deny_unknown_fields` serde error.
                        reject_legacy_recipe_keys(
                            &ov,
                            &format!("frame {}: per-frame `params` override", mf.input.display()),
                        )?;
                        // `film_base` and `density.dmax` are both roll-fixed
                        // calibrations: the whole batch is meant to share one frozen
                        // base (Dmin) and one display-white anchor (Dmax). A per-frame
                        // override *may* still set either (a deliberate per-frame value
                        // stays possible), but doing so gives this frame a different
                        // Dmin / Dmax from the rest of the roll and breaks color
                        // consistency — so warn loudly (roll-level,
                        // `--strict`-promotable) and continue, applying the override,
                        // rather than rejecting.
                        if ov.get("film_base").is_some() {
                            let msg = format!(
                                "frame {}: a per-frame `params` override sets `film_base`, \
                                 overriding the roll-fixed base — this frame's Dmin differs \
                                 from the rest of the roll, breaking color consistency. Set \
                                 the base once in the shared --params recipe (and drop the \
                                 per-frame `film_base`) if you want a frozen, consistent roll.",
                                mf.input.display()
                            );
                            log.warn(&msg);
                            roll_warnings.push(msg);
                        }
                        // `reconstruction.curve.dmax` became a roll-fixed calibration
                        // in the `dmax-reference` task (default `Fixed`, or an
                        // `Explicit` measured/per-stock anchor frozen into the recipe).
                        // A per-frame override of it breaks roll consistency exactly
                        // like `film_base`.
                        if sets_curve_dmax(&ov) {
                            let msg = format!(
                                "frame {}: a per-frame `params` override sets \
                                 `reconstruction.curve.dmax`, overriding the roll-fixed \
                                 display-white anchor — this frame's Dmax differs from the \
                                 rest of the roll, breaking color consistency. Set Dmax once \
                                 in the shared --params recipe (and drop the per-frame \
                                 `reconstruction.curve.dmax`) if you want a frozen, \
                                 consistent roll.",
                                mf.input.display()
                            );
                            log.warn(&msg);
                            roll_warnings.push(msg);
                        }
                        let setting = if sets_curve_dmax(&ov) {
                            DmaxSetting::Recipe
                        } else {
                            shared_setting
                        };
                        let mut v = shared_value.clone();
                        merge_json(&mut v, &ov);
                        let cfg: ResolvedConfig = serde_json::from_value(v).map_err(|e| {
                            NcError::Usage(format!(
                                "frame {}: invalid params override: {e}",
                                mf.input.display()
                            ))
                        })?;
                        validate(&cfg)?;
                        reject_roll_unsupported(&cfg)?;
                        reject_roll_unsupported_input(&cfg)?;
                        (cfg, Some(ov), setting)
                    }
                    None => (shared.clone(), None, shared_setting),
                };
                let output = resolve_frame_output(mf.output.as_deref(), &mf.input, out_dir);
                planned.push(PlannedFrame {
                    input: mf.input,
                    output,
                    cfg,
                    overrides,
                    dmax_setting,
                });
            }
        }
        None => {
            let mut inputs = Vec::new();
            for p in &args.inputs {
                expand_input(p, &mut inputs)?;
            }
            inputs.sort();
            inputs.dedup();
            if inputs.is_empty() {
                return Err(NcError::Usage(
                    "no input frames to convert (the inputs matched no files)".into(),
                ));
            }
            for input in inputs {
                let output = default_output_name(&input, out_dir);
                planned.push(PlannedFrame {
                    input,
                    output,
                    cfg: shared.clone(),
                    overrides: None,
                    dmax_setting: shared_setting,
                });
            }
        }
    }
    Ok(planned)
}

/// Guard every roll write target (per-frame outputs + sidecars, `--report-file`)
/// against every input scan and against one another — so a same-stem collision or
/// a target aimed at an input fails loudly up front rather than clobbering a scan
/// or a just-written sibling. The roll-input analogue of
/// [`ensure_write_targets_distinct`] (multiple inputs, case-insensitivity-aware).
fn ensure_roll_targets_distinct(inputs: &[&Path], targets: &[(String, PathBuf)]) -> Result<()> {
    let input_keys: Vec<PathBuf> = inputs.iter().map(|p| collision_key(p)).collect();
    let mut seen: Vec<(&str, PathBuf)> = Vec::with_capacity(targets.len());
    for (label, path) in targets {
        let key = collision_key(path);
        if input_keys.iter().any(|ik| keys_collide(ik, &key)) {
            return Err(NcError::Usage(format!(
                "{label} ({}) would overwrite an input scan",
                path.display()
            )));
        }
        if let Some((other, _)) = seen.iter().find(|(_, k)| keys_collide(k, &key)) {
            return Err(NcError::Usage(format!(
                "{label} ({}) collides with {other}",
                path.display()
            )));
        }
        seen.push((label.as_str(), key));
    }
    Ok(())
}

/// Map a successfully-converted frame's [`Report`] to its [`FrameReport`] entry.
fn frame_report_ok(pf: &PlannedFrame, report: Report) -> FrameReport {
    FrameReport {
        input: pf.input.clone(),
        output: Some(pf.output.clone()),
        status: FrameStatus::Ok {
            film_base: report.film_base,
            dmax: report.dmax,
            white_balance: report.white_balance,
            balance_range: report.balance_range,
            input_color: report.input_color.map(Box::new),
            loss: report.loss,
        },
        warnings: report.warnings,
        overrides: pf.overrides.clone(),
    }
}

/// A failed frame's [`FrameReport`] entry — the error message plus any warnings
/// accumulated before the failure point (decode/IR/film-base notices), so a frame
/// that warns and then fails still reports them (and they aren't lost to `--quiet`).
fn frame_report_err(pf: &PlannedFrame, err: &NcError, warnings: Vec<String>) -> FrameReport {
    FrameReport {
        input: pf.input.clone(),
        output: Some(pf.output.clone()),
        status: FrameStatus::Failed {
            error: err.to_string(),
        },
        warnings,
        overrides: pf.overrides.clone(),
    }
}

/// `nc roll` — convert a batch of frames from one shared, frozen recipe (the
/// batch-apply scaffold, design-spec §8/§12 item 6). Resolves the plan (frames +
/// per-frame configs), guards write targets, then converts each frame through the
/// same [`convert_frame`] core `convert` uses — so per-frame output is
/// byte-identical to a single `convert` with the same effective recipe. A frame's
/// failure is recorded and the roll continues; the loud non-zero exit + per-frame
/// `error` in the roll report are the signal.
fn run_roll(args: RollArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    // Shared frozen recipe — validated once up front so a broken recipe fails
    // loudly before any frame is touched.
    let LoadedRecipe {
        cfg: shared,
        curve_dmax_present: shared_dmax_present,
    } = load_recipe(args.recipe_in.as_deref())?;
    validate(&shared)?;
    reject_roll_unsupported(&shared)?;
    reject_roll_unsupported_input(&shared)?;

    // A roll's headline guarantee is one frozen, roll-fixed film base shared by
    // every frame. Only an *explicit* base delivers that: `auto`/`region` (and the
    // default, `auto`) re-estimate `Dmin` from each frame's own pixels, so the roll
    // is neither frozen nor color-consistent even though the report still prints
    // "one shared recipe". Warn loudly (report + stderr, `--strict`-promotable)
    // rather than hard-failing, so a best-effort batch stays usable.
    let mut roll_warnings: Vec<String> = Vec::new();
    if !matches!(shared.film_base.source, FilmBaseSource::Explicit(_)) {
        let kind = match shared.film_base.source {
            FilmBaseSource::Auto => "auto",
            FilmBaseSource::Region(_) => "region",
            FilmBaseSource::Explicit(_) => unreachable!(),
        };
        let msg = format!(
            "roll film base is NOT frozen: film_base.source is `{kind}`, so every frame \
             estimates its own Dmin — the roll is not color-consistent and the shared \
             recipe is not truly shared. Calibrate the base once (e.g. `nc estimate \
             --base-region X,Y,W,H <reference-scan>`), then pass the reported explicit \
             base via `--film-base R,G,B` or a recipe with `film_base.source.explicit`."
        );
        log.warn(&msg);
        roll_warnings.push(msg);
    }

    // `reconstruction.curve.dmax` is likewise a roll-fixed calibration by default:
    // `Fixed` (the nominal constant), `Explicit` (a frozen scalar), and `None`
    // (the bit-exact scene-referred escape hatch) all treat every frame
    // identically. Only `Auto` (`--auto-d-max`) re-measures the display-white
    // anchor from each frame's own pixels, so a shared recipe carrying it is not
    // truly frozen — same warn-and-continue treatment as the base.
    if let Reconstruction::Density { curve, .. } = &shared.reconstruction
        && curve.dmax() == DmaxSource::Auto
    {
        let msg = "roll Dmax is NOT frozen: reconstruction.curve.dmax is `auto`, so every \
             frame measures its own display-white anchor — the roll is not \
             color-consistent and the shared recipe is not truly shared. Freeze Dmax \
             once (e.g. `nc estimate --d-max-region X,Y,W,H <reference-scan>`), then \
             pass the reported anchor via `--d-max <d>` or a recipe with \
             `reconstruction.curve.dmax.explicit`, or accept the default fixed \
             nominal anchor."
            .to_string();
        log.warn(&msg);
        roll_warnings.push(msg);
    }

    // Resolve the plan. A per-frame override that touches a roll-fixed calibration
    // (`film_base` / `reconstruction.curve.dmax`) appends its own roll-level
    // warning here (warn-and-continue, like the not-frozen warnings above), so
    // `roll_warnings` is passed in to collect it.
    let planned = resolve_frames(
        &args,
        &shared,
        shared_dmax_present,
        &mut roll_warnings,
        &log,
    )?;

    // Guard every write target (per-frame outputs + sidecars, and the report
    // file) against every input and against one another before writing anything.
    // The `--frames` manifest is a read input too — a write target aimed at it
    // (e.g. `--report-file` equal to the manifest path) must be rejected, not
    // silently clobbered — so include it in the protected read set.
    let mut inputs: Vec<&Path> = planned.iter().map(|p| p.input.as_path()).collect();
    if let Some(frames) = args.frames.as_deref() {
        inputs.push(frames);
    }
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    for pf in &planned {
        targets.push((
            format!("output for {}", pf.input.display()),
            pf.output.clone(),
        ));
        targets.push((
            format!("sidecar for {}", pf.input.display()),
            encode::sidecar_path(&pf.output),
        ));
    }
    if let Some(rf) = args.report.report_file.as_deref() {
        targets.push(("--report-file".to_string(), rf.to_path_buf()));
    }
    ensure_roll_targets_distinct(&inputs, &targets)?;

    // Create the output directory now that the plan is known-good. A manifest may
    // name a per-frame output in a subdirectory (`sub/x.tiff`), so create each
    // frame's output parent too — otherwise the encode fails on a missing dir.
    // (The sidecar is written beside the output, so the same parent covers it.)
    std::fs::create_dir_all(&args.out_dir).map_err(|e| {
        NcError::Write(format!(
            "cannot create --out-dir {}: {e}",
            args.out_dir.display()
        ))
    })?;
    for pf in &planned {
        if let Some(parent) = pf.output.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                NcError::Write(format!(
                    "cannot create output directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
    }

    let mut frames = Vec::with_capacity(planned.len());
    let (mut succeeded, mut failed) = (0usize, 0usize);
    for pf in &planned {
        log.info(format_args!("converting {}", pf.input.display()));
        // Per-frame warnings accumulate here so a frame that warns and then fails
        // still hands them back (the report only rides out on success).
        let mut warnings = Vec::new();
        match convert_frame(
            "roll",
            &pf.input,
            &pf.output,
            &pf.cfg,
            InputFromCli::none(),
            pf.dmax_setting,
            &log,
            &mut warnings,
        ) {
            Ok(frame) => {
                succeeded += 1;
                frames.push(frame_report_ok(pf, frame.report));
            }
            Err(e) => {
                failed += 1;
                // Batch resilience: one frame's failure is recorded and the roll
                // continues (the loud non-zero exit + per-frame `error` are the
                // signal). Echo to stderr too; stdout stays the JSON report.
                log.warn(&format!("frame {} failed: {e}", pf.input.display()));
                frames.push(frame_report_err(pf, &e, warnings));
            }
        }
    }

    // `--strict` promotes any warning to a failing exit (convert's gate,
    // aggregated across the roll): both the roll-level warnings (e.g. the base is
    // not frozen) and any per-frame warning. Decided before the report is emitted.
    let strict_failure =
        args.strict && (!roll_warnings.is_empty() || frames.iter().any(|f| !f.warnings.is_empty()));

    let total = frames.len();
    let roll = RollReport {
        command: "roll",
        recipe: shared,
        warnings: roll_warnings,
        frames,
        summary: RollSummary {
            total,
            succeeded,
            failed,
        },
        elapsed_ms: Some(elapsed_ms(started)),
    };
    // Emit the report before the failure gates so the machine-readable per-frame
    // record still lands even when the roll then exits non-zero (convert/estimate
    // contract).
    emit_json(
        &roll,
        args.report.report,
        args.report.report_file.as_deref(),
    )?;

    if failed > 0 {
        return Err(NcError::Other(format!(
            "roll: {failed} of {total} frame(s) failed to convert (see report)"
        )));
    }
    if strict_failure {
        return Err(NcError::Other(
            "--strict: the roll produced warnings (see report)".into(),
        ));
    }
    Ok(())
}

/// `nc inspect` — decode a scan and report what was found (format, dimensions,
/// channels, bit depth, IR presence, scanner metadata) plus a best-effort
/// suggested `Dmin`. No output image is written.
fn run_inspect(args: IoArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    if let Some(rf) = args.report.report_file.as_deref() {
        ensure_write_targets_distinct(&args.input, &[("--report-file", rf)])?;
    }
    let (image, info) = decode(&args.input)?;
    log.info(format_args!(
        "decoded {:?} {}x{} (ir={})",
        info.format, info.width, info.height, info.ir_present
    ));

    let mut report = Report {
        command: Some("inspect"),
        input: Some(args.input.clone()),
        ..Report::default()
    };
    for w in &info.warnings {
        push_warning(&mut report, &log, w.clone());
    }

    // Resolve the input color semantics with no user assertions (auto/auto) so the
    // report shows the file's *intrinsic* evidence — transfer + measurement meaning
    // with per-axis evidence and a safe ICC summary. `inspect` is diagnostic: it
    // reports even ambiguous/unsupported inputs (it never gates like `convert`),
    // so `resolve` cannot error here (auto assertions never contradict structure).
    let input_meta =
        input_semantics::resolve(&container_color_facts(&info), &InputAssertions::auto())
            .expect("auto/auto resolution never fails");
    let input_report = InputColorReport::from_metadata(&input_meta);
    if input_report.icc_unparsable() {
        push_warning(
            &mut report,
            &log,
            "embedded ICC profile present but could not be parsed for a summary".into(),
        );
    }
    report.input_color = Some(input_report);

    if info.ir_present {
        push_warning(
            &mut report,
            &log,
            "input carries an IR plane; preserved but not used in Step 1 \
             (use `convert --export-ir` to write it out)"
                .into(),
        );
    }

    // Candidate rebate bands + suggested Dmin via the inward-scan detector. For
    // inspect this is informational — a refusal is a note, not fatal — and the
    // candidates are reported even when selection refuses, so the user can
    // confirm a rectangle for `--base-region` instead of measuring one.
    match film_base::rebate_candidates(&image) {
        Ok(candidates) => {
            match film_base::select_auto_base(&image, &candidates) {
                Ok(est) => {
                    report.film_base = Some(est.base);
                    report.film_base_source = Some(FilmBaseSource::Auto);
                    for w in est.warnings {
                        push_warning(&mut report, &log, w);
                    }
                }
                // The selection error already carries actionable advice (pass
                // --base-region/--film-base, or --base-content per the
                // film-base-content-fallback task); short lead-in only.
                Err(e) => push_warning(
                    &mut report,
                    &log,
                    format!("suggested Dmin unavailable — {e}"),
                ),
            }
            if !candidates.is_empty() {
                report.base_candidates = Some(candidates);
            }
        }
        Err(e) => push_warning(
            &mut report,
            &log,
            format!("film-base detection skipped — {e}"),
        ),
    }

    report.decode = Some(info);
    report.elapsed_ms = Some(elapsed_ms(started));
    emit_report(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
    )
}

/// Reuse-ready forms of a measured base: a paste-ready `--film-base R,G,B`
/// flag string and the matching `film_base` recipe fragment — or `None` when
/// the measurement fails the explicit-base validation `convert` applies (each
/// channel in `(0, 1]`), so a degenerate base is never advertised as reusable.
/// `f32`'s `Display` prints the shortest round-tripping decimal, so both forms
/// reproduce the exact measured value when fed back to `convert`.
fn reuse_ready(rgb: [f32; 3]) -> Option<(String, FilmBaseParams)> {
    validate_explicit_film_base(&rgb).ok()?;
    Some((
        format!("--film-base {},{},{}", rgb[0], rgb[1], rgb[2]),
        FilmBaseParams {
            source: FilmBaseSource::Explicit(rgb),
        },
    ))
}

/// `nc estimate` — run only film-base / `Dmin` estimation from the selected
/// source (default `auto`, or `--base-region`/`--film-base`; `--grid` samples
/// a 5-cell grid for unexposed-frame calibration) and emit the resolved
/// [`FilmBase`] as JSON — together with reuse-ready forms of it (a
/// `--film-base` flag string and a `film_base` recipe fragment) when the
/// measurement is usable as an explicit base (each channel in `(0, 1]`;
/// otherwise a warning explains why not) — so the measured value drops
/// straight into a `convert` call or a roll recipe (design-spec §8). Auto
/// detection may fail loudly on real scans; that propagates as an error (the
/// user asked for an estimate we can't give). `--strict` promotes warnings
/// (e.g. grid disagreement) to a failing exit after the report is emitted.
fn run_estimate(args: EstimateArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    if let Some(rf) = args.report.report_file.as_deref() {
        ensure_write_targets_distinct(&args.input, &[("--report-file", rf)])?;
    }
    let source = film_base_source_override(&args.film_base).unwrap_or_default();
    // Guard an explicit base with the same check `convert` applies (a recipe
    // never reaches estimate, but a bad `--film-base` must fail loudly rather
    // than be echoed back). Region bounds are checked by `film_base::estimate`.
    if let FilmBaseSource::Explicit(b) = &source {
        validate_explicit_film_base(b)?;
    }

    let (image, info) = decode(&args.input)?;
    log.info(format_args!(
        "decoded {:?} {}x{} (ir={})",
        info.format, info.width, info.height, info.ir_present
    ));

    let mut report = Report {
        command: Some("estimate"),
        input: Some(args.input.clone()),
        ..Report::default()
    };
    for w in &info.warnings {
        push_warning(&mut report, &log, w.clone());
    }

    let base = if args.grid {
        // Grid calibration: clap rejects `--grid` with `--film-base` /
        // `--auto-base`, so the rectangle is `--base-region` or the full frame.
        let rect = args
            .film_base
            .base_region
            .unwrap_or([0, 0, image.width, image.height]);
        let grid = film_base::estimate_grid(&image, rect)?;
        if !grid.agreement {
            // The 1.0 spread sentinel also fires when a channel's cells all
            // measure ~0 (a degenerate sample, not a light leak); diagnose by
            // the combined base so the warning names the actual problem.
            let msg = if <[f32; 3]>::from(grid.base).iter().any(|v| *v <= 0.0) {
                format!(
                    "grid measured non-positive transmission (combined base \
                     [{}, {}, {}]) — degenerate sample, not film base; was the \
                     sampled area unexposed film? See the report's grid.cells",
                    grid.base.r, grid.base.g, grid.base.b
                )
            } else {
                format!(
                    "grid cells disagree: per-channel relative spread \
                     [{:.4}, {:.4}, {:.4}] exceeds tolerance {} — possible light \
                     leak, scanner illumination falloff, or dust; see the \
                     report's grid.cells for the per-region values",
                    grid.spread[0], grid.spread[1], grid.spread[2], grid.tolerance
                )
            };
            push_warning(&mut report, &log, msg);
        }
        // The source records the overall rectangle the grid sampled; the
        // `grid` report field documents the per-cell method.
        report.film_base_source = Some(FilmBaseSource::Region(rect));
        let base = grid.base;
        report.grid = Some(grid);
        base
    } else {
        // Single-measurement path: `film_base::estimate` guards the base
        // finite-and-positive at birth (auto-base-redesign) and may attach
        // quality warnings (non-uniform region, cross-edge disagreement).
        let est = film_base::estimate(
            &image,
            &FilmBaseParams {
                source: source.clone(),
            },
        )?;
        report.film_base_source = Some(source);
        for w in est.warnings {
            push_warning(&mut report, &log, w);
        }
        est.base
    };
    report.film_base = Some(base);

    // Optional roll-fixed `Dmax` measurement from a fully-exposed reference region
    // (the plan-phase mirror of `--base-region` for `Dmax`, design-spec §8). Needs
    // a usable base to compute base-relative density; a degenerate base (the grid
    // path can produce one) is left to the existing degenerate-base handling below
    // — measuring here would only mask that with a confusing secondary error.
    if let Some(region) = args.d_max_region {
        let base_arr = <[f32; 3]>::from(base);
        // The density divide only needs a finite-positive base; a base outside
        // `(0, 1]` still yields a (diagnostic) `Dmax`, but is *not* a valid explicit
        // `--film-base` — see the reuse gating below.
        let base_divisible = base_arr.iter().all(|v| v.is_finite() && *v > 0.0);
        if base_divisible {
            // Median transmission of the reference region (robust to dust on a
            // near-opaque frame; see `film_base::sample_region_at`), reduced to the
            // scalar `Dmax` — a base-relative density `D = -log10(t/base)` (raw `D`
            // per §4; the render's corrected-density domain only under default
            // density-scale/offset). A degenerate / non-opaque region errors loudly
            // inside `reference_dmax`.
            let reference = film_base::sample_region_at(&image, region, 0.5)?;
            let measured = density::reference_dmax(<[f32; 3]>::from(reference), &base)?;
            let dmax = measured.scalar;
            report.dmax = Some(dmax);
            report.dmax_region = Some(region);
            log.info(format_args!(
                "measured roll-fixed Dmax {dmax} from {region:?}"
            ));
            // Plausibility for a fully-exposed leader — a loud, `--strict`-promotable
            // warning (never a hard error: thin/unusual stock varies). See
            // `reference_dmax_plausibility_warning`.
            if let Some(msg) = reference_dmax_plausibility_warning(&measured) {
                push_warning(&mut report, &log, msg);
            }
            // Reuse-ready `--d-max` / `density.dmax` forms are gated on the SAME
            // base-usability check the film-base reuse uses (each channel in
            // `(0, 1]`), not merely `base_divisible`: a base in `(1, ∞)` divides
            // fine but is not a valid explicit `--film-base`, so advertising a
            // `--d-max` measured against it as "reuse-ready" — while the film-base
            // reuse is withheld — would be a footgun. The diagnostic `dmax` /
            // `dmax_region` above still emit either way.
            if validate_explicit_film_base(&base_arr).is_ok() {
                report.dmax_reuse = Some(DmaxReuseReady {
                    flag: format!("--d-max {dmax}"),
                    recipe: DmaxRecipeFragment {
                        dmax: DmaxSource::Explicit(dmax),
                    },
                });
            }
        }
    }

    // Reuse-ready forms — attached only when the measurement passes the
    // explicit-base validation `convert` applies: a base outside `(0, 1]` on any
    // channel is still reported as the measurement, but never as "reuse-ready".
    // The single-measurement path already errors on a degenerate base via
    // `estimate`'s guard; the grid path's degenerate (`<= 0` / non-finite)
    // combined base is hard-errored below, *after* the report is emitted — so
    // this suppression keeps that emitted report from advertising the degenerate
    // value as reusable, and still stands alone for a non-degenerate but
    // out-of-range base (a channel `> 1`).
    //
    // Deliberately independent of grid *agreement*: a `--grid` run whose cells
    // disagree (light leak / falloff / dust) still emits reuse-ready output when
    // the combined median base is in range — the median resists a single bad
    // cell, and the disagreement already rides `warnings`. A consumer treating
    // the base as authoritative must check `warnings` (or run `--strict`, which
    // promotes the disagreement to a hard failure); only a *degenerate* base
    // withholds the reuse forms. (Design-spec §8.)
    match reuse_ready(<[f32; 3]>::from(base)) {
        Some((flag, recipe)) => {
            report.reuse = Some(ReuseReady { flag, recipe });
        }
        None => push_warning(
            &mut report,
            &log,
            format!(
                "measured base {:?} is not usable as an explicit --film-base \
                 (channels must be in (0, 1]) — was the sampled area unexposed \
                 film base? No reuse-ready output emitted",
                <[f32; 3]>::from(base)
            ),
        ),
    }

    report.elapsed_ms = Some(elapsed_ms(started));
    // Emit the report before the `--strict` gate so the machine-readable record
    // (the measured base) lands even when a warning then fails the run (same
    // contract as `convert`).
    emit_report(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
    )?;
    // A degenerate grid combined base (non-finite or <= 0 on any channel — e.g.
    // `--grid --base-region` on the dark holder) cannot anchor the density
    // divide, so hard-error **regardless of `--strict`**, mirroring the
    // single-measurement path where `film_base::estimate`'s finite-and-positive
    // guard rejects the same condition at birth. Same `NcError::Other` (exit 1)
    // as that guard, so both estimate paths map a degenerate base to one exit
    // code. The diagnostic report (with `grid.cells` and the per-cell warning) is
    // emitted above first, so the evidence lands before this gate.
    if args.grid
        && <[f32; 3]>::from(base)
            .iter()
            .any(|v| !v.is_finite() || *v <= 0.0)
    {
        return Err(NcError::Other(format!(
            "grid combined film base {:?} is not finite and positive on every \
             channel; it cannot anchor the density divide — was the sampled area \
             unexposed film base? See the report's grid.cells",
            <[f32; 3]>::from(base)
        )));
    }
    if args.strict && !report.warnings.is_empty() {
        return Err(NcError::Other(format!(
            "--strict: {} warning(s) present (see report)",
            report.warnings.len()
        )));
    }
    Ok(())
}

/// Whether this run should collect telemetry — opt-in via either flag.
fn telemetry_requested(args: &ConvertArgs) -> bool {
    args.telemetry || args.telemetry_file.is_some()
}

/// The `--telemetry-file` value as a filesystem write target, or `None` when it's
/// absent or `-` (stdout, which is not a file and needs no collision check).
fn telemetry_file_target(args: &ConvertArgs) -> Option<&Path> {
    match args.telemetry_file.as_deref() {
        Some(p) if p != "-" => Some(Path::new(p)),
        _ => None,
    }
}

/// Build the telemetry record for a finished conversion and write it to the
/// requested sink(s): the persistent JSONL log (`--telemetry`) and/or a one-off
/// file or stdout (`--telemetry-file`). `telemetry_log` is the pre-resolved log
/// path the caller already collision-checked, so the guarded and written paths
/// are the same by construction (and the env is read only once). Best-effort —
/// every failure is warned on stderr and swallowed (the conversion already
/// succeeded), and nothing here enters `report.warnings`, so `--strict` cannot
/// turn a telemetry write failure into a conversion failure. This is the one
/// documented deviation from the house fail-loudly rule (telemetry is
/// non-critical observability).
#[allow(clippy::too_many_arguments)]
fn emit_telemetry(
    args: &ConvertArgs,
    cfg: &ResolvedConfig,
    info: &DecodeInfo,
    timings: telemetry::TimingInfo,
    loss: EncodeReport,
    recipe_json: &str,
    report: &Report,
    log: &Log,
    telemetry_log: Option<&Path>,
) {
    let record = telemetry::build_record(telemetry::RecordInputs {
        info,
        // The ambient reads live here in the orchestrator; `build_record` stays a
        // pure function of its inputs (mirrors `default_log_path`/`resolve_log_path`).
        timestamp_ms: telemetry::now_unix_millis(),
        cpu_count: telemetry::cpu_count(),
        timings,
        loss,
        input_bytes: file_len(&args.input),
        output_bytes: file_len(&args.output),
        reconstruction: cfg.reconstruction.reconstruction_type(),
        curve: cfg.reconstruction.curve_type(),
        params_hash: telemetry::params_hash(recipe_json),
        film_base_source: cfg.film_base.source.clone(),
        dmax: report.dmax,
        output_hdr: cfg.output.hdr,
        warnings: report.warnings.len(),
    });

    // A telemetry write failure warns but never fails the run. Unlike ordinary
    // warnings, these are deliberately kept out of `report.warnings` (so
    // `--strict` can't promote them), which means the report can't carry them
    // either — so they must show even under `--quiet` (the `non_finite` precedent
    // above): an opted-in feature failing silently would defeat the opt-in.
    // `warn_always` is the one-liner for exactly this. The successful-write
    // notices stay `log.info` (visible only under `-v`).
    let warn = |msg: String| log.warn_always(&msg);

    // One compact JSON object (one line for the JSONL log).
    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(e) => {
            warn(format!("telemetry: could not serialize record: {e}"));
            return;
        }
    };

    if args.telemetry {
        match telemetry_log {
            Some(path) => {
                if let Err(e) = telemetry::append_jsonl(path, &line) {
                    warn(format!(
                        "telemetry: could not append to {}: {e}",
                        path.display()
                    ));
                } else {
                    log.info(format_args!("telemetry: appended to {}", path.display()));
                }
            }
            None => warn(
                "telemetry: could not locate a data dir for the log \
                 (set NC_TELEMETRY_LOG)"
                    .into(),
            ),
        }
    }

    if let Some(target) = args.telemetry_file.as_deref() {
        if target == "-" {
            // `-` = stdout. Written fail-soft with `writeln!` (not `println!`,
            // which panics on a broken pipe) so a closed stdout reader can't turn
            // a succeeded conversion into a panic. Note: if the JSON report is
            // also on stdout (the default), stdout then carries the report plus
            // this one line — pair `--telemetry-file -` with
            // `--report none`/`--report-file` when a parser consumes stdout.
            if let Err(e) = writeln!(std::io::stdout(), "{line}") {
                warn(format!("telemetry: could not write to stdout: {e}"));
            }
        } else if let Err(e) = telemetry::write_oneoff(Path::new(target), &line) {
            warn(format!("telemetry: could not write {target}: {e}"));
        } else {
            log.info(format_args!("telemetry: wrote {target}"));
        }
    }
}

/// Best-effort file size in bytes for the telemetry record; `None` if the file
/// can't be stat'd (never fails the run).
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

/// Milliseconds elapsed since `started`, as an `f64` for the report.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExponentialParams, SigmoidParams};

    /// Parse a `convert` invocation (with the required input/output already set)
    /// and return its args, so merge can be tested against the real parser.
    fn parse_convert(extra: &[&str]) -> ConvertArgs {
        let mut argv = vec!["nc", "convert", "in.tiff", "-o", "out.tiff"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Convert(a) => a,
            _ => unreachable!("expected convert"),
        }
    }

    /// A density-reconstruction config from its two blocks (the common test
    /// constructor — the tagged enum makes field-poking verbose otherwise).
    fn density_cfg(density: DensityParams, curve: DensityCurve) -> ResolvedConfig {
        ResolvedConfig {
            reconstruction: Reconstruction::Density { density, curve },
            ..ResolvedConfig::default()
        }
    }

    fn exponential_cfg(e: ExponentialParams) -> ResolvedConfig {
        density_cfg(DensityParams::default(), DensityCurve::Exponential(e))
    }

    fn sigmoid_cfg(s: SigmoidParams) -> ResolvedConfig {
        density_cfg(DensityParams::default(), DensityCurve::Sigmoid(s))
    }

    fn simple_cfg() -> ResolvedConfig {
        ResolvedConfig {
            reconstruction: Reconstruction::Simple,
            ..ResolvedConfig::default()
        }
    }

    /// The density block of a resolved config (panics on `simple` — the tests
    /// using it assert a density reconstruction).
    fn density_of(cfg: &ResolvedConfig) -> &DensityParams {
        match &cfg.reconstruction {
            Reconstruction::Density { density, .. } => density,
            Reconstruction::Simple => panic!("expected a density reconstruction"),
        }
    }

    /// The resolved curve of a density config (panics on `simple`).
    fn curve_of(cfg: &ResolvedConfig) -> &DensityCurve {
        match &cfg.reconstruction {
            Reconstruction::Density { curve, .. } => curve,
            Reconstruction::Simple => panic!("expected a density reconstruction"),
        }
    }

    /// The exponential curve's gamma (panics on sigmoid/simple).
    fn gamma_of(cfg: &ResolvedConfig) -> f32 {
        match curve_of(cfg) {
            DensityCurve::Exponential(e) => e.gamma,
            DensityCurve::Sigmoid(_) => panic!("expected the exponential curve"),
        }
    }

    /// The sigmoid curve's knobs (panics on exponential/simple).
    fn sigmoid_of(cfg: &ResolvedConfig) -> SigmoidParams {
        match curve_of(cfg) {
            DensityCurve::Sigmoid(s) => *s,
            DensityCurve::Exponential(_) => panic!("expected the sigmoid curve"),
        }
    }

    #[test]
    fn cli_parser_is_valid() {
        // Catches clap derive mistakes (duplicate flags, bad value parsers).
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_rgb_and_region() {
        assert_eq!(parse_rgb("0.9, 0.5,0.4").unwrap(), [0.9, 0.5, 0.4]);
        assert!(parse_rgb("0.9,0.5").is_err()); // too few
        assert!(parse_rgb("a,b,c").is_err()); // not numbers
        assert_eq!(parse_region("0,1,2,3").unwrap(), [0, 1, 2, 3]);
        assert!(parse_region("0,1,2").is_err()); // too few
        assert!(parse_region("0,1,2,-3").is_err()); // negative
    }

    #[test]
    fn merge_flag_overrides_recipe_else_keeps_recipe_else_default() {
        let recipe: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":2.0}}}"#,
        )
        .unwrap();

        // recipe value, no flag → recipe kept
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(gamma_of(&cfg), 2.0);

        // matching flag → flag wins
        let cfg = merge(recipe, &parse_convert(&["--density-gamma", "1.5"])).unwrap();
        assert_eq!(gamma_of(&cfg), 1.5);

        // unspecified everywhere → default
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&[])).unwrap();
        assert_eq!(gamma_of(&cfg), 1.0);
    }

    #[test]
    fn merge_handles_reconstruction_and_array_flags() {
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&[
                "--reconstruction",
                "simple",
                "--white-balance",
                "1.1,1.0,0.9",
            ]),
        )
        .unwrap();
        assert_eq!(cfg.reconstruction, Reconstruction::Simple);
        assert_eq!(cfg.print.white_balance, WbSource::Explicit([1.1, 1.0, 0.9]));
    }

    #[test]
    fn merge_switches_reconstruction_and_curve_variants() {
        // `--reconstruction density` over a simple recipe starts from the
        // density defaults (there is nothing to carry).
        let cfg = merge(
            simple_cfg(),
            &parse_convert(&["--reconstruction", "density"]),
        )
        .unwrap();
        assert_eq!(cfg.reconstruction, Reconstruction::default());

        // ...and over a density recipe keeps its blocks (a no-op assertion).
        let recipe = exponential_cfg(ExponentialParams {
            gamma: 1.8,
            dmax: DmaxSource::Explicit(1.6),
        });
        let cfg = merge(
            recipe.clone(),
            &parse_convert(&["--reconstruction", "density"]),
        )
        .unwrap();
        assert_eq!(cfg.reconstruction, recipe.reconstruction);

        // `--density-curve sigmoid` over an exponential recipe carries the
        // roll-fixed dmax calibration over and takes sigmoid defaults otherwise.
        let cfg = merge(recipe, &parse_convert(&["--density-curve", "sigmoid"])).unwrap();
        assert_eq!(
            sigmoid_of(&cfg),
            SigmoidParams {
                dmax: DmaxSource::Explicit(1.6),
                ..SigmoidParams::default()
            }
        );

        // The reverse switch carries dmax back and takes exponential defaults.
        let recipe = sigmoid_cfg(SigmoidParams {
            contrast: 2.0,
            dmax: DmaxSource::Auto,
            ..SigmoidParams::default()
        });
        let cfg = merge(recipe, &parse_convert(&["--density-curve", "exponential"])).unwrap();
        assert_eq!(
            *curve_of(&cfg),
            DensityCurve::Exponential(ExponentialParams {
                gamma: 1.0,
                dmax: DmaxSource::Auto,
            })
        );

        // Same-type `--density-curve` is a no-op that keeps the recipe's knobs.
        let recipe = sigmoid_cfg(SigmoidParams {
            contrast: 2.0,
            toe: 0.05,
            ..SigmoidParams::default()
        });
        let cfg = merge(
            recipe.clone(),
            &parse_convert(&["--density-curve", "sigmoid"]),
        )
        .unwrap();
        assert_eq!(cfg.reconstruction, recipe.reconstruction);
    }

    #[test]
    fn merge_rejects_invalid_tagged_combinations() {
        // Every density/curve/Dmax flag with a resolved `simple` reconstruction
        // is a post-merge usage error naming the flag — never a silent no-op.
        for flags in [
            ["--reconstruction", "simple", "--density-scale", "1,1,1"].as_slice(),
            ["--reconstruction", "simple", "--density-gamma", "1.4"].as_slice(),
            ["--reconstruction", "simple", "--shadow-balance", "0.1,0,0"].as_slice(),
            ["--reconstruction", "simple", "--balance-range", "0.2,1.8"].as_slice(),
            ["--reconstruction", "simple", "--auto-balance-range"].as_slice(),
            ["--reconstruction", "simple", "--sigmoid-contrast", "1.2"].as_slice(),
            ["--reconstruction", "simple", "--d-max", "1.5"].as_slice(),
            ["--reconstruction", "simple", "--no-d-max"].as_slice(),
            [
                "--reconstruction",
                "simple",
                "--density-curve",
                "exponential",
            ]
            .as_slice(),
        ] {
            let err = merge(ResolvedConfig::default(), &parse_convert(flags)).unwrap_err();
            assert!(
                matches!(err, NcError::Usage(_)),
                "{flags:?} must be a usage error, got {err}"
            );
        }

        // A sigmoid flag with a resolved exponential curve (the default) is
        // invalid, not inert.
        let err = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--sigmoid-contrast", "1.2"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--sigmoid-contrast"), "{err}");
        assert!(err.to_string().contains("exponential"), "{err}");

        // A customized gamma with a resolved sigmoid curve is a loud usage
        // error, never ignored or downgraded to a warning.
        let err = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--density-curve", "sigmoid", "--density-gamma", "1.4"]),
        )
        .unwrap_err();
        assert!(matches!(err, NcError::Usage(_)));
        assert!(err.to_string().contains("--sigmoid-contrast"), "{err}");
        // ...including when the sigmoid curve comes from the recipe.
        let recipe = sigmoid_cfg(SigmoidParams::default());
        let err = merge(recipe, &parse_convert(&["--density-gamma", "1.4"])).unwrap_err();
        assert!(matches!(err, NcError::Usage(_)));
    }

    #[test]
    fn removed_algorithm_and_simple_flags_are_migration_errors() {
        // `--algorithm` is rejected with guidance naming the replacement pair.
        let err = reject_removed_flags(&parse_convert(&["--algorithm", "sigmoid"])).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--reconstruction"), "{err}");
        assert!(err.to_string().contains("--density-curve"), "{err}");

        // The removed simple controls are rejected, pointing downstream.
        for flags in [
            ["--invert-white-balance", "1.1,1.0,0.9"].as_slice(),
            ["--clip-low", "0.1"].as_slice(),
            ["--clip-high", "0.9"].as_slice(),
        ] {
            let err = reject_removed_flags(&parse_convert(flags)).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{flags:?}");
            assert!(err.to_string().contains("print"), "{flags:?}: {err}");
        }

        // A clean invocation passes.
        assert!(reject_removed_flags(&parse_convert(&[])).is_ok());
    }

    #[test]
    fn legacy_recipe_forms_are_migration_errors() {
        // Each removed top-level selection form fails with guidance naming the
        // tagged `reconstruction` object — including the old sibling sections.
        for body in [
            r#"{"algorithm":"density"}"#,
            r#"{"density":{"density_gamma":1.8}}"#,
            r#"{"sigmoid":{"contrast":1.2}}"#,
            r#"{"simple":{"clip_low":0.1}}"#,
        ] {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let err = reject_legacy_recipe_keys(&v, "recipe r.json").unwrap_err();
            assert_eq!(err.exit_code(), 2, "{body}");
            assert!(err.to_string().contains("reconstruction"), "{body}: {err}");
        }
        // ...and through `load_recipe` on a real file.
        let p = std::env::temp_dir().join(format!("nc-legacy-{}.json", std::process::id()));
        std::fs::write(&p, r#"{"algorithm":"density"}"#).unwrap();
        let got = load_recipe(Some(&p));
        std::fs::remove_file(&p).ok();
        assert!(matches!(got, Err(NcError::Usage(_))));

        // The new tagged form passes the migration check.
        let v: serde_json::Value =
            serde_json::from_str(r#"{"reconstruction":{"type":"density"}}"#).unwrap();
        assert!(reject_legacy_recipe_keys(&v, "recipe r.json").is_ok());
    }

    #[test]
    fn merge_wb_flags_map_to_the_source_enum() {
        // Each flag maps to its variant; a forgotten merge arm would leave the
        // default and silently make the flag a no-op (the four-spot-wiring trap).
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--auto-wb", "gray-world"]),
        )
        .unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::GrayWorld);
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--auto-wb", "percentile"]),
        )
        .unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::Percentile);

        // No flag keeps the recipe's auto mode; a flag replaces it (flags win).
        let mut recipe = ResolvedConfig::default();
        recipe.print.white_balance = WbSource::GrayWorld;
        assert_eq!(
            merge(recipe.clone(), &parse_convert(&[]))
                .unwrap()
                .print
                .white_balance,
            WbSource::GrayWorld
        );
        assert_eq!(
            merge(recipe.clone(), &parse_convert(&["--auto-wb", "percentile"]))
                .unwrap()
                .print
                .white_balance,
            WbSource::Percentile
        );
        // Explicit beats auto BY SOURCE: `--white-balance 1,1,1` over a recipe
        // auto mode means neutral gains, not re-estimation — even though the
        // value equals the default (the variant carries the provenance).
        assert_eq!(
            merge(recipe, &parse_convert(&["--white-balance", "1,1,1"]))
                .unwrap()
                .print
                .white_balance,
            WbSource::Explicit([1.0, 1.0, 1.0])
        );
    }

    #[test]
    fn mutually_exclusive_wb_flags_are_rejected() {
        let argv = [
            "nc",
            "convert",
            "i",
            "-o",
            "o",
            "--white-balance",
            "1,1,1",
            "--auto-wb",
            "percentile",
        ];
        assert!(
            Cli::try_parse_from(argv).is_err(),
            "--white-balance and --auto-wb should conflict"
        );
    }

    #[test]
    fn recipe_parses_nested_print_white_balance_key() {
        // The recipe key lives under `print.white_balance`; pin the documented
        // (§9) nesting and all three variant wire-forms through `ResolvedConfig`.
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"print":{"white_balance":"gray-world"}}"#).unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::GrayWorld);
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"print":{"white_balance":"percentile"}}"#).unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::Percentile);
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"print":{"white_balance":{"explicit":[1.2,1.0,0.8]}}}"#)
                .unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::Explicit([1.2, 1.0, 0.8]));
        // The auto modes validate under density reconstruction (no value to
        // range-check).
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            let mut cfg = ResolvedConfig::default();
            cfg.print.white_balance = mode;
            validate(&cfg).unwrap();
        }
    }

    #[test]
    fn validate_rejects_auto_wb_with_simple_reconstruction() {
        // `simple` never reads `print.white_balance`, so an auto mode would be a
        // silent no-op (exit 0, no estimation, no gains). A requested action must
        // fail loudly instead — exit 2 (usage).
        let mut cfg = simple_cfg();
        for mode in [WbSource::GrayWorld, WbSource::Percentile] {
            cfg.print.white_balance = mode;
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "{mode:?} with simple must be rejected"
            );
        }
        // Explicit gains under simple are fine (`print.white_balance` is inert
        // there, but not an *action* silently dropped).
        cfg.print.white_balance = WbSource::Explicit([1.1, 1.0, 0.9]);
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_accepts_auto_wb_with_both_density_curves() {
        // The whitelist's arm: density reconstruction under either curve must
        // accept an auto mode (the counterpart to the simple rejection above).
        for curve in [
            DensityCurve::Exponential(ExponentialParams::default()),
            DensityCurve::Sigmoid(SigmoidParams::default()),
        ] {
            let mut cfg = density_cfg(DensityParams::default(), curve);
            for mode in [WbSource::GrayWorld, WbSource::Percentile] {
                cfg.print.white_balance = mode;
                validate(&cfg)
                    .unwrap_or_else(|e| panic!("{curve:?} + {mode:?} must validate: {e}"));
            }
        }
    }

    #[test]
    fn every_auto_wb_source_has_a_cli_flag() {
        // Guard against a future `WbSource` auto mode shipping recipe-only (it
        // must be reachable from `--auto-wb`, per "every knob is a CLI flag").
        // `WbSource::Explicit` is `--white-balance`; every other variant must map
        // back from an `AutoWb`. Uses an exhaustive match so adding a variant
        // fails to compile until it is wired here (and thus to the flag).
        for mode in [AutoWb::GrayWorld, AutoWb::Percentile] {
            let src: WbSource = mode.into();
            let round_trip = match src {
                WbSource::Explicit(_) => panic!("an AutoWb must not map to Explicit"),
                WbSource::GrayWorld => AutoWb::GrayWorld,
                WbSource::Percentile => AutoWb::Percentile,
            };
            assert_eq!(round_trip, mode);
        }
    }

    #[test]
    fn merge_dmax_flags_map_to_the_source_enum() {
        // Each flag maps to its variant on the resolved curve; a forgotten merge
        // arm would leave the default and silently make the flag a no-op (the
        // four-spot-wiring trap).
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--d-max", "1.75"]),
        )
        .unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Explicit(1.75));
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--no-d-max"])).unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::None);
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--auto-d-max"])).unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Auto);
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--fixed-d-max"]),
        )
        .unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Fixed);

        // The same flags land on a resolved *sigmoid* curve too — the anchor is
        // curve-stage state, written through whichever variant is resolved.
        let cfg = merge(
            sigmoid_cfg(SigmoidParams::default()),
            &parse_convert(&["--d-max", "1.4"]),
        )
        .unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Explicit(1.4));

        // No flag keeps the recipe's choice; a flag replaces it (flags win).
        let recipe = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::Explicit(2.0),
        });
        assert_eq!(
            curve_of(&merge(recipe.clone(), &parse_convert(&[])).unwrap()).dmax(),
            DmaxSource::Explicit(2.0)
        );
        assert_eq!(
            curve_of(&merge(recipe.clone(), &parse_convert(&["--no-d-max"])).unwrap()).dmax(),
            DmaxSource::None
        );
        // `--fixed-d-max` overrides a recipe's explicit/auto back to the default
        // fixed anchor (the flags-win escape hatch, since the default is Fixed and
        // an absent flag never clobbers a recipe value).
        assert_eq!(
            curve_of(&merge(recipe, &parse_convert(&["--fixed-d-max"])).unwrap()).dmax(),
            DmaxSource::Fixed
        );
    }

    #[test]
    fn merge_sigmoid_flags_override_recipe_else_keep_recipe() {
        // Each flag maps to its field; a forgotten merge arm would leave the
        // default and silently make the flag a no-op (the four-spot-wiring trap).
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&[
                "--density-curve",
                "sigmoid",
                "--sigmoid-contrast",
                "1.6",
                "--sigmoid-toe",
                "0.1",
                "--sigmoid-shoulder",
                "0.35",
            ]),
        )
        .unwrap();
        let s = sigmoid_of(&cfg);
        assert_eq!(s.contrast, 1.6);
        assert_eq!(s.toe, 0.1);
        assert_eq!(s.shoulder, 0.35);

        // No flag keeps the recipe's values; a flag replaces only its own knob.
        let recipe: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"sigmoid","contrast":2.0,"toe":0.05}}}"#,
        )
        .unwrap();
        let cfg = merge(recipe, &parse_convert(&["--sigmoid-shoulder", "0.4"])).unwrap();
        let s = sigmoid_of(&cfg);
        assert_eq!(s.contrast, 2.0);
        assert_eq!(s.toe, 0.05);
        assert_eq!(s.shoulder, 0.4);
    }

    #[test]
    fn validate_rejects_bad_sigmoid_params() {
        // Contrast must be finite, positive, AND bounded above (an extreme slope
        // silently collapses the curve into a hard threshold — see the const doc).
        for bad in [
            0.0,
            -1.0,
            f32::NAN,
            f32::INFINITY,
            crate::algo::sigmoid::SIGMOID_CONTRAST_MAX + 1.0,
            1e30,
        ] {
            let cfg = sigmoid_cfg(SigmoidParams {
                contrast: bad,
                ..SigmoidParams::default()
            });
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "contrast {bad} should fail"
            );
        }
        // The cap itself is accepted (boundary is inclusive).
        validate(&sigmoid_cfg(SigmoidParams {
            contrast: crate::algo::sigmoid::SIGMOID_CONTRAST_MAX,
            ..SigmoidParams::default()
        }))
        .unwrap();
        // Knee widths must be finite, >= 0, AND <= the cap (a negative width would
        // silently read as "knee off"; a huge finite width flattens the image
        // without tripping any counter). Both ends fail loudly.
        let knee_max = crate::algo::sigmoid::SIGMOID_KNEE_MAX;
        for (toe, shoulder) in [
            (-0.1, 0.2),
            (0.2, f32::NAN),
            (0.2, f32::INFINITY),
            (knee_max + 1.0, 0.2),
            (0.2, knee_max + 1.0),
            (10_000.0, 0.2),
            (0.2, 10_000.0),
        ] {
            let cfg = sigmoid_cfg(SigmoidParams {
                toe,
                shoulder,
                ..SigmoidParams::default()
            });
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "toe={toe} shoulder={shoulder} should fail"
            );
        }
        // Zero widths (both knees off = the straight line) and the cap itself are
        // valid (boundary inclusive).
        validate(&sigmoid_cfg(SigmoidParams {
            toe: 0.0,
            shoulder: 0.0,
            ..SigmoidParams::default()
        }))
        .unwrap();
        validate(&sigmoid_cfg(SigmoidParams {
            toe: knee_max,
            shoulder: knee_max,
            ..SigmoidParams::default()
        }))
        .unwrap();
    }

    #[test]
    fn validate_rejects_sigmoid_without_a_dmax_anchor() {
        // The S-curve is anchored on [0, Dmax]; `dmax = none` only works for
        // the exponential curve's scene-referred output.
        let cfg = sigmoid_cfg(SigmoidParams {
            dmax: DmaxSource::None,
            ..SigmoidParams::default()
        });
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        // Auto and Explicit anchors are fine under sigmoid...
        validate(&sigmoid_cfg(SigmoidParams {
            dmax: DmaxSource::Auto,
            ..SigmoidParams::default()
        }))
        .unwrap();
        validate(&sigmoid_cfg(SigmoidParams {
            dmax: DmaxSource::Explicit(1.4),
            ..SigmoidParams::default()
        }))
        .unwrap();
        // ...and `none` stays valid for the exponential curve.
        validate(&exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::None,
        }))
        .unwrap();
    }

    #[test]
    fn recipe_parses_tagged_curve_keys() {
        // §9 places the curve knobs under the tagged `reconstruction.curve`; with
        // `deny_unknown_fields` a misplaced key would silently reject the recipe,
        // so pin the documented nesting for both variants.
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"type":"density",
                "curve":{"type":"sigmoid","contrast":1.4,"toe":0.15,"shoulder":0.3}}}"#,
        )
        .unwrap();
        let s = sigmoid_of(&cfg);
        assert_eq!(s.contrast, 1.4);
        assert_eq!(s.toe, 0.15);
        assert_eq!(s.shoulder, 0.3);
        // A tagged-but-partial curve fills that variant's defaults.
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"reconstruction":{"curve":{"type":"sigmoid","toe":0.0}}}"#)
                .unwrap();
        let s = sigmoid_of(&cfg);
        assert_eq!(s.toe, 0.0);
        assert_eq!(s.contrast, SigmoidParams::default().contrast);
    }

    #[test]
    fn merge_regional_balance_flags() {
        // Each knob maps through merge into `reconstruction.density`; a forgotten
        // arm would silently make the flag a no-op (the four-spot-wiring trap).
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&[
                "--shadow-balance",
                "0.1,0,-0.05",
                "--highlight-balance",
                "-0.1,0.02,0",
                "--balance-range",
                "0.25,1.75",
            ]),
        )
        .unwrap();
        let d = density_of(&cfg);
        assert_eq!(d.shadow_balance, [0.1, 0.0, -0.05]);
        assert_eq!(d.highlight_balance, [-0.1, 0.02, 0.0]);
        assert_eq!(d.balance_range, BalanceRange::Explicit([0.25, 1.75]));

        // No flag keeps the recipe's values; a flag replaces them (flags win),
        // and `--auto-balance-range` overrides a recipe's explicit range.
        let recipe: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"density":{"shadow_balance":[0.2,0.0,0.0],
                                             "balance_range":{"explicit":[0.5,2.5]}}}}"#,
        )
        .unwrap();
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(density_of(&cfg).shadow_balance, [0.2, 0.0, 0.0]);
        assert_eq!(
            density_of(&cfg).balance_range,
            BalanceRange::Explicit([0.5, 2.5])
        );
        let cfg = merge(
            recipe,
            &parse_convert(&["--shadow-balance", "0,0,0", "--auto-balance-range"]),
        )
        .unwrap();
        assert_eq!(density_of(&cfg).shadow_balance, [0.0, 0.0, 0.0]);
        assert_eq!(density_of(&cfg).balance_range, BalanceRange::Auto);
    }

    #[test]
    fn mutually_exclusive_balance_range_flags_are_rejected() {
        assert!(
            Cli::try_parse_from([
                "nc",
                "convert",
                "i",
                "-o",
                "o",
                "--balance-range",
                "0.2,1.8",
                "--auto-balance-range"
            ])
            .is_err()
        );
    }

    #[test]
    fn validate_rejects_bad_regional_balance() {
        // Non-finite balance offsets (recipe-smuggleable) fail loudly.
        let cfg = density_cfg(
            DensityParams {
                shadow_balance: [0.1, f32::NAN, 0.0],
                ..DensityParams::default()
            },
            DensityCurve::default(),
        );
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        let cfg = density_cfg(
            DensityParams {
                highlight_balance: [f32::INFINITY, 0.0, 0.0],
                ..DensityParams::default()
            },
            DensityCurve::default(),
        );
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // An explicit range must be finite, ordered lo < hi (equal anchors would
        // make the ramp divide by zero), and have a *representable* span — two
        // individually-finite anchors can still overflow `hi - lo` to +inf,
        // which would silently flatten the ramp.
        for bad in [
            [1.0, 1.0],
            [2.0, 1.0],
            [f32::NAN, 1.0],
            [0.0, f32::INFINITY],
            [-3.0e38, 3.0e38], // finite anchors, span overflows to +inf
        ] {
            let cfg = density_cfg(
                DensityParams {
                    balance_range: BalanceRange::Explicit(bad),
                    ..DensityParams::default()
                },
                DensityCurve::default(),
            );
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "balance range {bad:?} should fail"
            );
        }

        // Negative-density anchors are legal (`offset` can shift D' below zero),
        // and Auto plus finite balances validate.
        let cfg = density_cfg(
            DensityParams {
                shadow_balance: [0.1, -0.1, 0.0],
                balance_range: BalanceRange::Explicit([-0.5, 1.5]),
                ..DensityParams::default()
            },
            DensityCurve::default(),
        );
        validate(&cfg).unwrap();
    }

    #[test]
    fn recipe_parses_regional_balance_keys() {
        // The keys live under `reconstruction.density` (§9);
        // `deny_unknown_fields` would silently reject a docs-shaped recipe if the
        // structs drifted.
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"density":{"shadow_balance":[0.1,0.0,-0.05],
                                             "highlight_balance":[-0.1,0.0,0.05],
                                             "balance_range":{"explicit":[0.25,1.75]}}}}"#,
        )
        .unwrap();
        let d = density_of(&cfg);
        assert_eq!(d.shadow_balance, [0.1, 0.0, -0.05]);
        assert_eq!(d.highlight_balance, [-0.1, 0.0, 0.05]);
        assert_eq!(d.balance_range, BalanceRange::Explicit([0.25, 1.75]));
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"reconstruction":{"density":{"balance_range":"auto"}}}"#)
                .unwrap();
        assert_eq!(density_of(&cfg).balance_range, BalanceRange::Auto);
    }

    #[test]
    fn recipe_parses_curve_dmax_key() {
        // The recipe key lives under `reconstruction.curve.dmax` (the curve owns
        // the anchor); with `deny_unknown_fields` at every level a misplaced key
        // would silently reject, so pin the documented (§9) nesting and all four
        // wire-forms through `ResolvedConfig` — on both curve variants.
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"exponential","dmax":{"explicit":1.5}}}}"#,
        )
        .unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Explicit(1.5));
        for (wire, expected) in [
            ("\"none\"", DmaxSource::None),
            ("\"auto\"", DmaxSource::Auto),
            ("\"fixed\"", DmaxSource::Fixed),
        ] {
            let cfg: ResolvedConfig = serde_json::from_str(&format!(
                r#"{{"reconstruction":{{"curve":{{"type":"exponential","dmax":{wire}}}}}}}"#
            ))
            .unwrap();
            assert_eq!(curve_of(&cfg).dmax(), expected, "{wire}");
        }
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"sigmoid","dmax":{"explicit":1.4}}}}"#,
        )
        .unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Explicit(1.4));
    }

    #[test]
    fn mutually_exclusive_dmax_flags_are_rejected() {
        for pair in [
            ["--d-max", "1.5", "--no-d-max"].as_slice(),
            ["--d-max", "1.5", "--auto-d-max"].as_slice(),
            ["--d-max", "1.5", "--fixed-d-max"].as_slice(),
            ["--fixed-d-max", "--auto-d-max"].as_slice(),
            ["--fixed-d-max", "--no-d-max"].as_slice(),
            ["--auto-d-max", "--no-d-max"].as_slice(),
        ] {
            let mut argv = vec!["nc", "convert", "i", "-o", "o"];
            argv.extend_from_slice(pair);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{pair:?} should conflict"
            );
        }
    }

    #[test]
    fn validate_rejects_bad_explicit_dmax() {
        // A recipe can smuggle a non-positive / non-finite anchor past clap's
        // value parser, so validate is the only guard once it's in the config.
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let cfg = exponential_cfg(ExponentialParams {
                gamma: 1.0,
                dmax: DmaxSource::Explicit(bad),
            });
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "explicit d-max {bad} should fail (exponential)"
            );
            let cfg = sigmoid_cfg(SigmoidParams {
                dmax: DmaxSource::Explicit(bad),
                ..SigmoidParams::default()
            });
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "explicit d-max {bad} should fail (sigmoid)"
            );
        }
        // A positive explicit anchor, and Fixed / Auto / None, all validate on
        // the exponential curve.
        for src in [
            DmaxSource::Explicit(1.8),
            DmaxSource::None,
            DmaxSource::Auto,
            DmaxSource::Fixed,
        ] {
            validate(&exponential_cfg(ExponentialParams {
                gamma: 1.0,
                dmax: src,
            }))
            .unwrap();
        }
    }

    #[test]
    fn dump_params_round_trips_through_params() {
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--density-gamma", "1.8", "--output-hdr"]),
        )
        .unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        // The sigmoid form and the simple form round-trip too.
        for cfg in [
            merge(
                ResolvedConfig::default(),
                &parse_convert(&["--density-curve", "sigmoid", "--sigmoid-toe", "0.1"]),
            )
            .unwrap(),
            merge(
                ResolvedConfig::default(),
                &parse_convert(&["--reconstruction", "simple"]),
            )
            .unwrap(),
        ] {
            let json = serde_json::to_string(&cfg).unwrap();
            assert_eq!(serde_json::from_str::<ResolvedConfig>(&json).unwrap(), cfg);
        }
    }

    #[test]
    fn resolved_recipe_emits_the_documented_reconstruction_schema() {
        // Schema fixtures (design-spec §8): every resolved recipe emits
        // `reconstruction.schema_version = 1` and exactly one tagged curve — an
        // omitted input curve never survives normalization.
        let v = serde_json::to_value(ResolvedConfig::default()).unwrap();
        assert_eq!(v["reconstruction"]["schema_version"], 1);
        assert_eq!(v["reconstruction"]["type"], "density");
        assert_eq!(v["reconstruction"]["curve"]["type"], "exponential");
        assert_eq!(v["reconstruction"]["curve"]["dmax"], "fixed");
        assert_eq!(
            v["reconstruction"]["density"]["scale"],
            serde_json::json!([1.0, 1.0, 1.0])
        );

        // Partial input: omitted curve normalizes to tagged exponential defaults.
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"reconstruction":{"schema_version":1,"type":"density"}}"#)
                .unwrap();
        assert_eq!(*curve_of(&cfg), DensityCurve::default());
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["reconstruction"]["curve"]["type"], "exponential");

        // Simple emits schema_version + type and nothing else.
        let v = serde_json::to_value(simple_cfg()).unwrap();
        assert_eq!(
            v["reconstruction"],
            serde_json::json!({"schema_version": 1, "type": "simple"})
        );

        // An unsupported schema_version is rejected loudly through the recipe.
        assert!(
            serde_json::from_str::<ResolvedConfig>(r#"{"reconstruction":{"schema_version":2}}"#)
                .is_err()
        );
    }

    #[test]
    fn reconstruction_result_serializes_the_documented_shapes() {
        // The report's resolution diagnostics (design-spec §8): simple is exactly
        // {"type":"simple"}; density carries curve type + the resolved dmax
        // triple, with `value` always present (null for `none`).
        let v = serde_json::to_value(reconstruction_result(
            &Reconstruction::Simple,
            None,
            DmaxSetting::Default,
        ))
        .unwrap();
        assert_eq!(v, serde_json::json!({"type": "simple"}));

        let v = serde_json::to_value(reconstruction_result(
            &Reconstruction::default(),
            Some(2.0),
            DmaxSetting::Default,
        ))
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "type": "density",
                "curve": {
                    "type": "exponential",
                    "dmax": {"policy": "fixed", "value": 2.0, "provenance": "default"}
                }
            })
        );

        // `none` reports a null value; the recipe provenance rides through.
        let cfg = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::None,
        });
        let v = serde_json::to_value(reconstruction_result(
            &cfg.reconstruction,
            None,
            DmaxSetting::Recipe,
        ))
        .unwrap();
        assert_eq!(
            v["curve"]["dmax"],
            serde_json::json!({"policy": "none", "value": null, "provenance": "recipe"})
        );

        // The auto policy always reports `auto-frame` — the value is a per-frame
        // measurement regardless of who selected the policy (this is the marker
        // that makes it master-incompatible).
        let cfg = sigmoid_cfg(SigmoidParams {
            dmax: DmaxSource::Auto,
            ..SigmoidParams::default()
        });
        for setting in [DmaxSetting::Default, DmaxSetting::Recipe, DmaxSetting::Cli] {
            let v = serde_json::to_value(reconstruction_result(
                &cfg.reconstruction,
                Some(1.37),
                setting,
            ))
            .unwrap();
            assert_eq!(v["curve"]["type"], "sigmoid");
            assert_eq!(v["curve"]["dmax"]["policy"], "auto");
            assert_eq!(
                v["curve"]["dmax"]["provenance"], "auto-frame",
                "{setting:?}"
            );
        }

        // A reference-measured scalar frozen into a recipe: explicit / recipe.
        let cfg = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::Explicit(1.64),
        });
        let v = serde_json::to_value(reconstruction_result(
            &cfg.reconstruction,
            Some(1.64),
            DmaxSetting::Recipe,
        ))
        .unwrap();
        assert_eq!(v["curve"]["dmax"]["policy"], "explicit");
        assert_eq!(v["curve"]["dmax"]["provenance"], "recipe");
        // ...and a CLI-passed one reports `cli`.
        let v = serde_json::to_value(reconstruction_result(
            &cfg.reconstruction,
            Some(1.64),
            DmaxSetting::Cli,
        ))
        .unwrap();
        assert_eq!(v["curve"]["dmax"]["provenance"], "cli");
    }

    #[test]
    fn report_recipe_echo_carries_the_tagged_reconstruction() {
        // The convert report's `recipe` is the effective config, so
        // `recipe.reconstruction` is the exact tagged schema (design-spec §8).
        let report = Report {
            recipe: Some(ResolvedConfig::default()),
            ..Report::default()
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["recipe"]["reconstruction"]["schema_version"], 1);
        assert_eq!(v["recipe"]["reconstruction"]["type"], "density");
        // Absent for non-convert reports.
        let v = serde_json::to_value(Report::default()).unwrap();
        assert!(v.get("recipe").is_none());
        assert!(v.get("reconstruction_result").is_none());
    }

    #[test]
    fn merge_output_hdr_flag_sets_but_never_clears() {
        // Flag present → hdr on (a forgotten merge arm would silently make the
        // flag a no-op — the four-spot-wiring trap).
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--output-hdr"])).unwrap();
        assert!(cfg.output.hdr);
        // No flag → the default stays off.
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&[])).unwrap();
        assert!(!cfg.output.hdr);
        // An absent (false) presence flag must not clobber a recipe `true`.
        let recipe: ResolvedConfig = serde_json::from_str(r#"{"output":{"hdr":true}}"#).unwrap();
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert!(cfg.output.hdr);
        // `--output-sdr` is the explicit escape hatch: it forces a recipe
        // `hdr: true` back to 16-bit (flags win by presence, not value).
        let cfg = merge(recipe, &parse_convert(&["--output-sdr"])).unwrap();
        assert!(!cfg.output.hdr);
        // ...and is a no-op on an already-SDR config.
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--output-sdr"])).unwrap();
        assert!(!cfg.output.hdr);
    }

    #[test]
    fn mutually_exclusive_output_depth_flags_are_rejected() {
        // clap must reject the conflicting pair rather than silently pick one.
        assert!(
            Cli::try_parse_from([
                "nc",
                "convert",
                "i",
                "-o",
                "o",
                "--output-hdr",
                "--output-sdr"
            ])
            .is_err()
        );
    }

    #[test]
    fn recipe_rejects_removed_out_depth_key() {
        // Breaking recipe change (pre-release): the old `output.out_depth` key
        // must be rejected loudly by `deny_unknown_fields`, never silently
        // ignored — an old recipe would otherwise quietly encode at 16-bit.
        assert!(
            serde_json::from_str::<ResolvedConfig>(r#"{"output":{"out_depth":"f32"}}"#).is_err()
        );
    }

    #[test]
    fn recipe_rejects_unknown_keys() {
        // Unknown top-level section.
        assert!(serde_json::from_str::<ResolvedConfig>(r#"{"reconstructon":{}}"#).is_err());
        // Typo'd key inside the reconstruction density block.
        assert!(
            serde_json::from_str::<ResolvedConfig>(
                r#"{"reconstruction":{"density":{"scal":[1,1,1]}}}"#
            )
            .is_err()
        );
        // Typo'd key inside the tagged curve.
        assert!(
            serde_json::from_str::<ResolvedConfig>(
                r#"{"reconstruction":{"curve":{"type":"exponential","gama":1.0}}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn params_default_is_valid_parseable_json() {
        let json = serde_json::to_string_pretty(&ResolvedConfig::default()).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ResolvedConfig::default());
        validate(&back).unwrap();
    }

    #[test]
    fn validate_rejects_bad_params() {
        // Exponential gamma must be positive.
        let cfg = exponential_cfg(ExponentialParams {
            gamma: 0.0,
            dmax: DmaxSource::Fixed,
        });
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = ResolvedConfig::default();
        cfg.print.white_balance = WbSource::Explicit([1.0, f32::NAN, 1.0]);
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // Non-positive explicit gains are rejected too (a recipe can smuggle
        // them past the CLI value parser).
        let mut cfg = ResolvedConfig::default();
        cfg.print.white_balance = WbSource::Explicit([1.0, 0.0, 1.0]);
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // Negative highlight compression is rejected (the print render silently
        // treats it as "off", so a wrong-sign value must fail loudly, not no-op).
        let mut cfg = ResolvedConfig::default();
        cfg.print.highlight_compress = -0.3;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        // Zero is valid (disables the roll-off).
        let mut cfg = ResolvedConfig::default();
        cfg.print.highlight_compress = 0.0;
        validate(&cfg).unwrap();

        // Non-positive density scale is rejected.
        let cfg = density_cfg(
            DensityParams {
                scale: [1.0, 0.0, 1.0],
                ..DensityParams::default()
            },
            DensityCurve::default(),
        );
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // A clean default (and the simple config) pass.
        validate(&ResolvedConfig::default()).unwrap();
        validate(&simple_cfg()).unwrap();
    }

    #[test]
    fn validate_rejects_recipe_smuggled_bad_values() {
        // A recipe can carry values the CLI value-parsers would have rejected,
        // so validate is the only guard for these once they're in the config.
        let mut cfg = ResolvedConfig::default();
        cfg.film_base.source = FilmBaseSource::Explicit([0.9, 0.0, 0.4]); // zero transmission
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = ResolvedConfig::default();
        cfg.film_base.source = FilmBaseSource::Explicit([0.9, 90.0, 0.4]); // "90" typo for "0.90"
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        let mut cfg = ResolvedConfig::default();
        cfg.film_base.source = FilmBaseSource::Explicit([1.0, 1.0, 1.0]); // 1.0 exactly is valid
        validate(&cfg).unwrap();

        let mut cfg = ResolvedConfig::default();
        cfg.film_base.source = FilmBaseSource::Region([0, 0, 0, 0]); // zero-area region
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
    }

    #[test]
    fn export_ir_and_seed_parse_into_the_right_homes() {
        // `--export-ir` is an input/decode key (design-spec §9), not output.
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--export-ir", "ir.tiff"]),
        )
        .unwrap();
        assert_eq!(cfg.input.export_ir.as_deref(), Some("ir.tiff"));

        // The reserved `--seed` flag parses rather than being rejected by clap.
        let args = parse_convert(&["--seed", "42"]);
        assert_eq!(args.seed, Some(42));
    }

    #[test]
    fn merge_keeps_recipe_source_until_a_flag_replaces_it() {
        // No flag → the recipe's mutually-exclusive choice survives.
        let mut recipe = ResolvedConfig::default();
        recipe.film_base.source = FilmBaseSource::Explicit([0.9, 0.5, 0.4]);
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(
            cfg.film_base.source,
            FilmBaseSource::Explicit([0.9, 0.5, 0.4])
        );

        // A flag replaces the whole source — no field is left behind to win on
        // precedence (the #5/#6 fix). `--base-region` beats a recipe explicit base.
        let cfg = merge(recipe, &parse_convert(&["--base-region", "0,0,100,40"])).unwrap();
        assert_eq!(
            cfg.film_base.source,
            FilmBaseSource::Region([0, 0, 100, 40])
        );
    }

    #[test]
    fn input_axes_merge_independently_and_flags_win() {
        // transfer and meaning are independent axes: a flag on one axis replaces
        // that axis and leaves the other at the recipe value (flags win per axis).
        let mut recipe = ResolvedConfig::default();
        recipe.input.transfer = TransferAssertion::Auto;
        recipe.input.meaning = MeaningAssertion::ScannerDevice;

        // No flags → both recipe values survive.
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(cfg.input.transfer, TransferAssertion::Auto);
        assert_eq!(cfg.input.meaning, MeaningAssertion::ScannerDevice);

        // `--input-transfer` replaces only the transfer axis.
        let cfg = merge(
            recipe.clone(),
            &parse_convert(&["--input-transfer", "linear"]),
        )
        .unwrap();
        assert_eq!(cfg.input.transfer, TransferAssertion::Linear);
        assert_eq!(cfg.input.meaning, MeaningAssertion::ScannerDevice);

        // `--input-meaning` replaces only the meaning axis (over a recipe value).
        let cfg = merge(recipe, &parse_convert(&["--input-meaning", "colorimetric"])).unwrap();
        assert_eq!(cfg.input.transfer, TransferAssertion::Auto);
        assert_eq!(cfg.input.meaning, MeaningAssertion::Colorimetric);
    }

    #[test]
    fn deprecated_assume_linear_is_a_migration_error() {
        // The old combined assertion must never silently assert both axes — it is a
        // loud usage error (exit 2) pointing at the two independent flags.
        let args = parse_convert(&["--assume-linear"]);
        let err = reject_deprecated_input_flags(&args.input_opts).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--input-transfer"));
    }

    #[test]
    fn input_profile_stays_rejected_for_convert() {
        // `--input-profile` is reserved (deferred experiment) — rejected loudly
        // (exit 4) rather than silently ignored.
        let args = parse_convert(&["--input-profile", "scanner.icc"]);
        let err = reject_deprecated_input_flags(&args.input_opts).unwrap_err();
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn legacy_input_color_recipe_key_is_a_migration_error() {
        // A recipe carrying the removed combined key fails to load with actionable
        // migration guidance (not an opaque unknown-field message).
        let v: serde_json::Value = serde_json::from_str(r#"{"input":{"color":"linear"}}"#).unwrap();
        let err = reject_legacy_recipe_keys(&v, "recipe r.json").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("input.transfer"));
        // A recipe using the new keys passes this migration check.
        let v: serde_json::Value =
            serde_json::from_str(r#"{"input":{"transfer":"linear","meaning":"scanner-device"}}"#)
                .unwrap();
        assert!(reject_legacy_recipe_keys(&v, "recipe r.json").is_ok());
    }

    #[test]
    fn mutually_exclusive_source_flags_are_rejected() {
        // clap must reject conflicting source flags rather than silently picking one.
        assert!(
            Cli::try_parse_from([
                "nc",
                "convert",
                "i",
                "-o",
                "o",
                "--auto-base",
                "--film-base",
                "0.9,0.5,0.4"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "nc",
                "convert",
                "i",
                "-o",
                "o",
                "--base-region",
                "0,0,1,1",
                "--film-base",
                "0.9,0.5,0.4"
            ])
            .is_err()
        );
    }

    #[test]
    fn estimate_grid_conflicts_with_explicit_and_auto_base() {
        // Grid replaces sampling/detection, so an explicit base or auto-base
        // alongside it is contradictory — clap must reject, not silently pick.
        for bad in [
            ["--grid", "--film-base", "0.9,0.5,0.4"].as_slice(),
            ["--grid", "--auto-base"].as_slice(),
        ] {
            let mut argv = vec!["nc", "estimate", "in.tiff"];
            argv.extend_from_slice(bad);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{bad:?} should conflict"
            );
        }
        // `--grid` with `--base-region` is the documented sub-rectangle mode.
        let cli = Cli::try_parse_from([
            "nc",
            "estimate",
            "in.tiff",
            "--grid",
            "--base-region",
            "0,0,9,9",
        ])
        .unwrap();
        match cli.command {
            Command::Estimate(a) => {
                assert!(a.grid);
                assert_eq!(a.film_base.base_region, Some([0, 0, 9, 9]));
            }
            _ => unreachable!("expected estimate"),
        }
    }

    #[test]
    fn reuse_ready_fragment_round_trips_as_a_recipe() {
        // The `film_base_recipe` report fragment must parse back both as the
        // `film_base` section value and inside a full recipe — otherwise the
        // advertised paste-into-a-roll-recipe workflow is broken.
        let fragment = FilmBaseParams {
            source: FilmBaseSource::Explicit([0.553, 0.271, 0.159]),
        };
        let json = serde_json::to_string(&fragment).unwrap();
        assert_eq!(json, r#"{"source":{"explicit":[0.553,0.271,0.159]}}"#);
        let back: FilmBaseParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fragment);
        let recipe: ResolvedConfig =
            serde_json::from_str(&format!(r#"{{"film_base":{json}}}"#)).unwrap();
        assert_eq!(recipe.film_base, fragment);
        validate(&recipe).unwrap();
    }

    #[test]
    fn film_base_flag_string_round_trips_exact_f32s() {
        // `Display` for f32 prints the shortest decimal that parses back to the
        // same bits, so the emitted `--film-base` string reproduces the exact
        // measured base — including awkward values with no short decimal form.
        let rgb = [0.553_712_3_f32, 1.0 / 3.0, f32::MIN_POSITIVE];
        let (flag, fragment) = reuse_ready(rgb).expect("a valid base is reuse-ready");
        let value = flag.strip_prefix("--film-base ").unwrap();
        assert_eq!(parse_rgb(value).unwrap(), rgb);
        // The two forms carry the same value — never allowed to drift.
        assert_eq!(fragment.source, FilmBaseSource::Explicit(rgb));
    }

    #[test]
    fn dmax_reuse_fragment_round_trips_as_a_recipe() {
        // `estimate --d-max-region`'s `d_max_recipe` fragment must serialize as
        // the documented `{"dmax":{"explicit":<d>}}` and, merged into a recipe's
        // tagged `reconstruction.curve`, parse back to the frozen anchor —
        // otherwise the freeze-into-a-roll-recipe workflow is broken. (Mirrors
        // the film-base fragment round-trip.)
        let fragment = DmaxRecipeFragment {
            dmax: DmaxSource::Explicit(1.2734),
        };
        let json = serde_json::to_string(&fragment).unwrap();
        assert_eq!(json, r#"{"dmax":{"explicit":1.2734}}"#);
        // Merged into a curve object (the documented destination), it parses on
        // both variants and validates.
        for curve_type in ["exponential", "sigmoid"] {
            let recipe: ResolvedConfig = serde_json::from_str(&format!(
                r#"{{"reconstruction":{{"curve":{{"type":"{curve_type}","dmax":{{"explicit":1.2734}}}}}}}}"#
            ))
            .unwrap();
            assert_eq!(
                curve_of(&recipe).dmax(),
                DmaxSource::Explicit(1.2734),
                "{curve_type}"
            );
            validate(&recipe).unwrap();
        }
    }

    #[test]
    fn estimate_parses_d_max_region() {
        // The plan-phase `--d-max-region` mirror of `--base-region` parses into an
        // [x,y,w,h] rectangle and coexists with an explicit `--film-base`.
        let cli = Cli::try_parse_from([
            "nc",
            "estimate",
            "leader.tiff",
            "--film-base",
            "0.9,0.55,0.42",
            "--d-max-region",
            "10,20,30,40",
        ])
        .unwrap();
        match cli.command {
            Command::Estimate(a) => {
                assert_eq!(a.d_max_region, Some([10, 20, 30, 40]));
                assert_eq!(a.film_base.film_base, Some([0.9, 0.55, 0.42]));
            }
            _ => unreachable!("expected estimate"),
        }
    }

    #[test]
    fn explicit_dmax_domain_warning_fires_on_nonneutral_regional_balance() {
        // Baseline: an explicit anchor with default density correction and neutral
        // balance is already in the curve's domain — no warning.
        let explicit = |density: DensityParams| {
            density_cfg(
                density,
                DensityCurve::Exponential(ExponentialParams {
                    gamma: 1.0,
                    dmax: DmaxSource::Explicit(2.0),
                }),
            )
        };
        assert!(explicit_dmax_domain_warning(&explicit(DensityParams::default())).is_none());

        // B1: a non-neutral regional balance shifts D′ (the corrected density the
        // curve subtracts the anchor from: D′_c = B_c + shadow·w_lo + highlight·w_hi),
        // so a reused explicit anchor mis-anchors even with default scale/offset. Warn,
        // and name regional balance in the message.
        let cfg = explicit(DensityParams {
            shadow_balance: [0.05, 0.0, -0.02],
            ..DensityParams::default()
        });
        let msg = explicit_dmax_domain_warning(&cfg).expect("non-neutral shadow balance must warn");
        assert!(
            msg.contains("regional balance"),
            "message must name regional balance: {msg}"
        );

        // A non-neutral highlight balance alone (scale/offset default) also warns,
        // and it warns under the sigmoid curve too (both curves subtract Dmax).
        let cfg = explicit(DensityParams {
            highlight_balance: [0.0, 0.01, 0.0],
            ..DensityParams::default()
        });
        assert!(explicit_dmax_domain_warning(&cfg).is_some());
        let cfg = density_cfg(
            DensityParams {
                highlight_balance: [0.0, 0.01, 0.0],
                ..DensityParams::default()
            },
            DensityCurve::Sigmoid(SigmoidParams {
                dmax: DmaxSource::Explicit(2.0),
                ..SigmoidParams::default()
            }),
        );
        assert!(explicit_dmax_domain_warning(&cfg).is_some());

        // `simple` has no density domain — no warning.
        assert!(explicit_dmax_domain_warning(&simple_cfg()).is_none());

        // A `Fixed`/`Auto` anchor is already in the corrected domain — no warning
        // even with a non-neutral balance on a density reconstruction.
        let cfg = density_cfg(
            DensityParams {
                shadow_balance: [0.05, 0.0, -0.02],
                ..DensityParams::default()
            },
            DensityCurve::default(),
        );
        assert!(explicit_dmax_domain_warning(&cfg).is_none());
    }

    #[test]
    fn reference_dmax_plausibility_warns_on_a_weak_channel_a_plausible_scalar_hides() {
        // B2, colored-region example: base [1,1,1], transmissions ≈ [0.001,0.99,0.99]
        // → per-channel densities ≈ [3.0, 0.004, 0.004]. The gray mean ≈ 1.0 clears
        // MIN_PLAUSIBLE_REFERENCE_DMAX, so the scalar-only check passes, yet green and
        // blue are essentially unexposed base — not a leader. The per-channel minimum
        // check must fire the (weak-channel) warning.
        let base = FilmBase::from([1.0, 1.0, 1.0]);
        let measured = density::reference_dmax([0.001, 0.99, 0.99], &base).unwrap();
        assert!(
            measured.scalar >= density::MIN_PLAUSIBLE_REFERENCE_DMAX,
            "the gray average alone must pass the scalar check ({})",
            measured.scalar
        );
        let msg = reference_dmax_plausibility_warning(&measured)
            .expect("a plausible scalar hiding a weak channel must warn");
        assert!(
            msg.contains("weakest channel"),
            "the weak-channel warning must fire, not the thin-frame one: {msg}"
        );

        // A genuine near-opaque leader (dense in every channel) → no warning.
        let measured = density::reference_dmax([0.01, 0.01, 0.01], &base).unwrap();
        assert!(reference_dmax_plausibility_warning(&measured).is_none());

        // A uniformly-thin frame (scalar below the floor) → the frame-wide warning.
        let measured = density::reference_dmax([0.3, 0.3, 0.3], &base).unwrap();
        let msg = reference_dmax_plausibility_warning(&measured)
            .expect("a sub-floor gray mean must warn");
        assert!(
            msg.contains("implausibly low for a fully-exposed leader"),
            "the thin-frame warning must fire: {msg}"
        );
    }

    #[test]
    fn report_reuse_flattens_to_flat_keys_or_nothing() {
        // The wire contract: the reuse pair serializes as two flat top-level keys
        // (`film_base_flag` / `film_base_recipe`), both present together, and the
        // `ReuseReady` wrapper / `reuse` field name never leaks. `None` emits
        // neither key. Locks the `#[serde(flatten)]` + rename shape so a refactor
        // can't silently change the agent-facing JSON.
        // Values exactly representable in f32 (halves/quarters/eighths) so the
        // JSON literals match without precision noise — the shape is the point.
        let with = Report {
            reuse: Some(ReuseReady {
                flag: "--film-base 0.5,0.25,0.125".to_string(),
                recipe: FilmBaseParams {
                    source: FilmBaseSource::Explicit([0.5, 0.25, 0.125]),
                },
            }),
            ..Report::default()
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["film_base_flag"], "--film-base 0.5,0.25,0.125");
        assert_eq!(
            v["film_base_recipe"],
            serde_json::json!({ "source": { "explicit": [0.5, 0.25, 0.125] } })
        );
        assert!(v.get("reuse").is_none(), "the wrapper name must not leak");

        let without = Report::default();
        let v = serde_json::to_value(&without).unwrap();
        assert!(v.get("film_base_flag").is_none());
        assert!(v.get("film_base_recipe").is_none());
        assert!(v.get("reuse").is_none());
    }

    #[test]
    fn reuse_ready_suppresses_degenerate_bases() {
        // The safety contract of the reuse output: a measurement `convert`
        // would reject (dark-holder zero, non-finite, >1 typo-scale) must never
        // be advertised as a paste-ready --film-base.
        assert!(reuse_ready([0.0, 0.5, 0.5]).is_none()); // dark holder channel
        assert!(reuse_ready([f32::NAN, 0.5, 0.5]).is_none()); // numerical fault
        assert!(reuse_ready([0.9, 90.0, 0.4]).is_none()); // "90" typo for "0.90"
        assert!(reuse_ready([-0.1, 0.5, 0.5]).is_none()); // negative
        // A valid base produces the exact flag string and matching fragment.
        let (flag, fragment) = reuse_ready([0.553, 0.271, 0.159]).unwrap();
        assert_eq!(flag, "--film-base 0.553,0.271,0.159");
        assert_eq!(
            fragment.source,
            FilmBaseSource::Explicit([0.553, 0.271, 0.159])
        );
    }

    #[test]
    fn load_recipe_maps_failures_to_usage() {
        // No path → defaults, infallibly, with no dmax provenance.
        let loaded = load_recipe(None).unwrap();
        assert_eq!(loaded.cfg, ResolvedConfig::default());
        assert!(!loaded.curve_dmax_present);

        // Missing file → Usage (exit 2), not Other.
        let missing = std::env::temp_dir().join("nc-no-such-recipe-xyz.json");
        assert!(matches!(
            load_recipe(Some(&missing)),
            Err(NcError::Usage(_))
        ));

        // Malformed JSON and unknown keys both map to Usage.
        for (tag, body) in [
            ("malformed", "{ not json"),
            (
                "unknown-key",
                r#"{"reconstruction":{"density":{"scal":[1,1,1]}}}"#,
            ),
        ] {
            let p =
                std::env::temp_dir().join(format!("nc-recipe-{tag}-{}.json", std::process::id()));
            std::fs::write(&p, body).unwrap();
            let got = load_recipe(Some(&p));
            std::fs::remove_file(&p).ok();
            assert!(
                matches!(got, Err(NcError::Usage(_))),
                "{tag} should be Usage"
            );
        }

        // A valid partial recipe loads, fills defaults, and records whether the
        // file set `reconstruction.curve.dmax` (the report's provenance witness).
        let p = std::env::temp_dir().join(format!("nc-recipe-ok-{}.json", std::process::id()));
        std::fs::write(
            &p,
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.8}}}"#,
        )
        .unwrap();
        let got = load_recipe(Some(&p)).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(gamma_of(&got.cfg), 1.8);
        assert_eq!(got.cfg.print, PrintParams::default());
        assert!(!got.curve_dmax_present, "gamma alone sets no dmax");

        let p = std::env::temp_dir().join(format!("nc-recipe-dmax-{}.json", std::process::id()));
        std::fs::write(
            &p,
            r#"{"reconstruction":{"curve":{"type":"exponential","dmax":{"explicit":1.6}}}}"#,
        )
        .unwrap();
        let got = load_recipe(Some(&p)).unwrap();
        std::fs::remove_file(&p).ok();
        assert!(got.curve_dmax_present);
    }

    #[test]
    fn keys_collide_is_case_insensitivity_aware() {
        assert!(keys_collide(
            Path::new("/d/out.tiff"),
            Path::new("/d/out.tiff")
        ));
        // Case-only difference must collide (conservative over-reject).
        assert!(keys_collide(
            Path::new("/d/out.tiff"),
            Path::new("/d/OUT.TIFF")
        ));
        // Genuinely different names must not.
        assert!(!keys_collide(
            Path::new("/d/out.tiff"),
            Path::new("/d/other.tiff")
        ));
    }

    #[test]
    fn write_targets_reject_case_only_collision_before_creation() {
        // `-o out.tiff --telemetry-file OUT.TIFF` on a case-insensitive FS is the
        // same file; with neither pre-existing, `collision_key` can't canonicalize
        // to a shared casing, so the guard must catch it via the case-insensitive
        // comparison. Use a real (existing) parent dir with non-existent children.
        let dir = std::env::temp_dir().join(format!("nc-case-collide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.tiff");
        let tel = dir.join("OUT.TIFF");
        let input = dir.join("in.tiff");
        let got = ensure_write_targets_distinct(
            &input,
            &[("--output", &out), ("--telemetry-file", &tel)],
        );
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(got, Err(NcError::Usage(_))),
            "a case-only telemetry-file/output collision must be a usage error: {got:?}"
        );
    }

    // --- roll (batch) --------------------------------------------------------

    #[test]
    fn roll_requires_input_or_frames_and_they_conflict() {
        // Neither positional inputs nor --frames → usage error.
        assert!(Cli::try_parse_from(["nc", "roll", "-o", "out"]).is_err());
        // Both → mutually exclusive.
        assert!(
            Cli::try_parse_from(["nc", "roll", "a.tif", "--frames", "m.json", "-o", "out"])
                .is_err()
        );
        // Either alone (with --out-dir) is fine.
        assert!(Cli::try_parse_from(["nc", "roll", "a.tif", "b.tif", "-o", "out"]).is_ok());
        assert!(Cli::try_parse_from(["nc", "roll", "--frames", "m.json", "-o", "out"]).is_ok());
        // --out-dir is required.
        assert!(Cli::try_parse_from(["nc", "roll", "a.tif"]).is_err());
    }

    #[test]
    fn merge_json_deep_merges_objects_and_replaces_other_values() {
        // Objects merge key-by-key (recursively); scalars/arrays replace wholesale.
        let mut base = serde_json::json!({"a": {"x": 1, "y": 2}, "b": 3});
        let overlay = serde_json::json!({"a": {"y": 20, "z": 30}, "b": [1, 2]});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!({"a": {"x": 1, "y": 20, "z": 30}, "b": [1, 2]})
        );
    }

    #[test]
    fn merge_json_replaces_enum_variant_switch_but_deep_merges_same_tag() {
        // An externally-tagged enum variant switch (`region` → `explicit`) must
        // REPLACE the one-key map, not union the tags — a `{"region":…,
        // "explicit":…}` object deserializes as no enum variant. Regression guard
        // for the per-frame `film_base.source` override path.
        let mut base = serde_json::json!({"film_base": {"source": {"region": [1, 2, 3, 4]}}});
        let overlay = serde_json::json!({"film_base": {"source": {"explicit": [0.9, 0.5, 0.4]}}});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!({"film_base": {"source": {"explicit": [0.9, 0.5, 0.4]}}})
        );
        // The SAME tag on both sides is not a variant switch: recurse into it so a
        // partial override of one sub-field keeps its siblings.
        let mut base = serde_json::json!({"curve": {"dmax": {"auto": {"p": 0.5, "q": 1}}}});
        let overlay = serde_json::json!({"curve": {"dmax": {"auto": {"p": 0.9}}}});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!({"curve": {"dmax": {"auto": {"p": 0.9, "q": 1}}}})
        );
    }

    #[test]
    fn merge_json_switches_internally_tagged_type_and_carries_dmax() {
        // The internally-tagged twins of the externally-tagged rule above: the
        // `reconstruction` object and its `curve` carry a `type` discriminator
        // beside variant-specific fields, so a per-frame type switch must
        // replace those fields (a deep merge would leave a union the fail-loud
        // deserializer rejects) while carrying the one deliberately-shared
        // field, the roll-fixed `dmax` — the same semantics the CLI merge gives
        // `--density-curve`.

        // Curve exponential → sigmoid: `gamma` dropped, `dmax` carried.
        let mut base = serde_json::json!({"reconstruction": {"curve":
            {"type": "exponential", "gamma": 1.8, "dmax": {"explicit": 1.6}}}});
        let overlay =
            serde_json::json!({"reconstruction": {"curve": {"type": "sigmoid", "contrast": 1.4}}});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!({"reconstruction": {"curve":
                {"type": "sigmoid", "contrast": 1.4, "dmax": {"explicit": 1.6}}}})
        );

        // An overlay that sets its own `dmax` wins over the carried one.
        let mut base = serde_json::json!(
            {"curve": {"type": "sigmoid", "contrast": 2.0, "dmax": {"explicit": 1.6}}});
        let overlay = serde_json::json!({"curve": {"type": "exponential", "dmax": "auto"}});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!({"curve": {"type": "exponential", "dmax": "auto"}})
        );

        // A SAME-type curve override is not a switch: deep merge keeps siblings.
        let mut base = serde_json::json!(
            {"curve": {"type": "sigmoid", "contrast": 2.0, "toe": 0.05}});
        let overlay = serde_json::json!({"curve": {"type": "sigmoid", "shoulder": 0.4}});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!(
                {"curve": {"type": "sigmoid", "contrast": 2.0, "toe": 0.05, "shoulder": 0.4}})
        );

        // Reconstruction density → simple: the stale `density`/`curve` blocks
        // must not survive (simple takes neither) — and the merged JSON must
        // deserialize back to a valid config.
        let mut base = serde_json::to_value(ResolvedConfig::default()).unwrap();
        let overlay = serde_json::json!({"reconstruction": {"type": "simple"}});
        merge_json(&mut base, &overlay);
        assert_eq!(
            base["reconstruction"],
            serde_json::json!({"type": "simple"})
        );
        let cfg: ResolvedConfig = serde_json::from_value(base).unwrap();
        assert_eq!(cfg.reconstruction, Reconstruction::Simple);
    }

    #[test]
    fn per_frame_override_switches_variants_and_keeps_the_roll_fixed_dmax() {
        // Through `resolve_frames`: a per-frame reconstruction/curve type switch
        // is a legitimate override — it must APPLY (deserialize cleanly), and a
        // curve switch must keep the shared recipe's roll-fixed anchor.
        let dir = std::env::temp_dir().join(format!("nc-roll-typeswitch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("frames.json");
        std::fs::write(
            &manifest,
            r#"{"frames":[
                 {"input":"a.tif","params":{"reconstruction":{"type":"simple"}}},
                 {"input":"b.tif",
                  "params":{"reconstruction":{"curve":{"type":"sigmoid","contrast":1.4}}}}
               ]}"#,
        )
        .unwrap();
        let args = RollArgs {
            inputs: vec![],
            frames: Some(manifest.clone()),
            out_dir: dir.clone(),
            recipe_in: None,
            strict: false,
            report: ReportArgs::default(),
        };
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.8,
            dmax: DmaxSource::Explicit(1.6),
        });
        shared.film_base.source = FilmBaseSource::Explicit([0.9, 0.55, 0.42]);
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let planned = resolve_frames(&args, &shared, true, &mut warnings, &log);
        std::fs::remove_dir_all(&dir).ok();
        let planned = planned.expect("per-frame type switches must apply, not error");
        assert_eq!(planned.len(), 2);

        // Frame 1: density → simple (the stale density/curve blocks are gone).
        assert_eq!(planned[0].cfg.reconstruction, Reconstruction::Simple);

        // Frame 2: exponential → sigmoid, keeping the roll-fixed dmax exactly as
        // the CLI's `--density-curve sigmoid` would; the stale `gamma` is gone
        // and unset sigmoid knobs take their defaults.
        assert_eq!(
            planned[1].cfg.reconstruction,
            Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Sigmoid(SigmoidParams {
                    contrast: 1.4,
                    dmax: DmaxSource::Explicit(1.6),
                    ..SigmoidParams::default()
                }),
            }
        );
        // No dmax override was written, so no roll-fixed-anchor warning fires.
        assert!(
            !warnings.iter().any(|w| w.contains("display-white anchor")),
            "a curve switch that keeps the shared anchor must not warn: {warnings:?}"
        );
    }

    #[test]
    fn per_frame_override_can_switch_film_base_variant_and_still_warns() {
        // A per-frame `params` override that flips the roll-fixed `film_base.source`
        // from `region` to `explicit` must APPLY (the merged JSON deserializes) and
        // still raise the roll-level "base overridden" warning. Before the
        // variant-switch fix the merge unioned the tags and `from_value` rejected
        // it, turning a valid override into a confusing error.
        let dir = std::env::temp_dir().join(format!("nc-roll-varswitch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("frames.json");
        std::fs::write(
            &manifest,
            r#"{"frames":[{"input":"a.tif",
                          "params":{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}}}}]}"#,
        )
        .unwrap();
        let args = RollArgs {
            inputs: vec![],
            frames: Some(manifest.clone()),
            out_dir: dir.clone(),
            recipe_in: None,
            strict: false,
            report: ReportArgs::default(),
        };
        let shared = ResolvedConfig {
            film_base: FilmBaseParams {
                source: FilmBaseSource::Region([10, 10, 20, 20]),
            },
            ..ResolvedConfig::default()
        };
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let planned = resolve_frames(&args, &shared, false, &mut warnings, &log);
        std::fs::remove_dir_all(&dir).ok();
        let planned = planned.expect("region→explicit override should apply, not error");
        assert_eq!(planned.len(), 1);
        assert_eq!(
            planned[0].cfg.film_base.source,
            FilmBaseSource::Explicit([0.9, 0.55, 0.42])
        );
        assert!(
            !warnings.is_empty(),
            "overriding the roll-fixed film base must still warn"
        );
    }

    #[test]
    fn per_frame_override_keeps_shared_roll_fixed_params() {
        // The manifest per-frame merge path: a partial override changes only its
        // own knob and keeps the shared roll-fixed params (film base, Dmax) — the
        // "frame-local override applies to just that frame" guarantee at the
        // config level. Mirrors `resolve_frames`' merge.
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::Explicit(1.6),
        });
        shared.film_base.source = FilmBaseSource::Explicit([0.9, 0.55, 0.42]);
        let mut v = serde_json::to_value(&shared).unwrap();
        let ov: serde_json::Value =
            serde_json::from_str(r#"{"print":{"print_exposure":0.15}}"#).unwrap();
        merge_json(&mut v, &ov);
        let cfg: ResolvedConfig = serde_json::from_value(v).unwrap();
        assert_eq!(cfg.print.print_exposure, 0.15);
        assert_eq!(
            cfg.film_base.source,
            FilmBaseSource::Explicit([0.9, 0.55, 0.42])
        );
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Explicit(1.6));
    }

    #[test]
    fn per_frame_override_rejects_legacy_reconstruction_keys() {
        // A per-frame `params` override using the removed sibling sections gets
        // the same migration guidance as a whole recipe, not an opaque
        // deny-unknown-fields error from the merged deserialize.
        let dir = std::env::temp_dir().join(format!("nc-roll-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("frames.json");
        std::fs::write(
            &manifest,
            r#"{"frames":[{"input":"a.tif","params":{"density":{"density_gamma":1.8}}}]}"#,
        )
        .unwrap();
        let args = RollArgs {
            inputs: vec![],
            frames: Some(manifest.clone()),
            out_dir: dir.clone(),
            recipe_in: None,
            strict: false,
            report: ReportArgs::default(),
        };
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let got = resolve_frames(
            &args,
            &ResolvedConfig::default(),
            false,
            &mut warnings,
            &log,
        );
        std::fs::remove_dir_all(&dir).ok();
        let err = got.expect_err("a legacy per-frame override must be rejected");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("reconstruction"), "{err}");
    }

    #[test]
    fn manifest_rejects_unknown_keys_and_parses_overrides() {
        // `deny_unknown_fields` at both levels catches a typo'd manifest.
        assert!(serde_json::from_str::<RollManifest>(r#"{"framez":[]}"#).is_err());
        assert!(
            serde_json::from_str::<RollManifest>(r#"{"frames":[{"input":"a.tif","bogus":1}]}"#)
                .is_err()
        );
        // A well-formed manifest with a per-frame override + output parses.
        let m: RollManifest = serde_json::from_str(
            r#"{"frames":[{"input":"a.tif","output":"a_out.tiff",
                           "params":{"print":{"print_exposure":0.2}}}]}"#,
        )
        .unwrap();
        assert_eq!(m.frames.len(), 1);
        assert_eq!(m.frames[0].input, PathBuf::from("a.tif"));
        assert_eq!(m.frames[0].output, Some(PathBuf::from("a_out.tiff")));
        assert!(m.frames[0].params.is_some());
    }

    #[test]
    fn tiff_ext_and_output_naming() {
        assert!(has_tiff_ext(Path::new("a.tif")));
        assert!(has_tiff_ext(Path::new("a.TIFF")));
        assert!(!has_tiff_ext(Path::new("a.png")));
        assert!(!has_tiff_ext(Path::new("a")));
        assert_eq!(
            default_output_name(Path::new("/scans/frame01.tif"), Path::new("/out")),
            PathBuf::from("/out/frame01_positive.tiff")
        );
        // A manifest output: relative joins the out-dir, absolute is used verbatim,
        // and `None` falls back to the default name.
        assert_eq!(
            resolve_frame_output(
                Some(Path::new("custom.tiff")),
                Path::new("/s/f.tif"),
                Path::new("/out")
            ),
            PathBuf::from("/out/custom.tiff")
        );
        assert_eq!(
            resolve_frame_output(
                Some(Path::new("/abs/c.tiff")),
                Path::new("/s/f.tif"),
                Path::new("/out")
            ),
            PathBuf::from("/abs/c.tiff")
        );
        assert_eq!(
            resolve_frame_output(None, Path::new("/s/f.tif"), Path::new("/out")),
            PathBuf::from("/out/f_positive.tiff")
        );
    }

    #[test]
    fn expand_input_lists_sorted_tiffs_and_skips_others() {
        // Directory expansion after the fail-loud rewrite: `.tif`/`.tiff` files
        // (case-insensitive) in sorted order, non-TIFF and extension-less entries
        // skipped. (A per-entry `read_dir` error is not portably reproducible in a
        // test, so only the happy path is exercised here.)
        let dir = std::env::temp_dir().join(format!("nc-expand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["b.tif", "a.TIFF", "c.png", "d"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let mut out = Vec::new();
        let got = expand_input(&dir, &mut out);
        std::fs::remove_dir_all(&dir).ok();
        got.expect("expanding a readable directory should succeed");
        assert_eq!(out, vec![dir.join("a.TIFF"), dir.join("b.tif")]);
    }

    #[test]
    fn reject_roll_unsupported_rejects_export_ir() {
        let mut cfg = ResolvedConfig::default();
        assert!(reject_roll_unsupported(&cfg).is_ok());
        cfg.input.export_ir = Some("ir.tiff".into());
        assert!(matches!(
            reject_roll_unsupported(&cfg),
            Err(NcError::Usage(_))
        ));
    }

    #[test]
    fn ensure_roll_targets_distinct_catches_input_and_sibling_collisions() {
        // A target aimed at an input scan, and two frames colliding on one output
        // (e.g. same stem from different dirs), both fail loudly.
        let inputs = [Path::new("/scans/a.tif"), Path::new("/scans/b.tif")];
        let clobber_input = vec![("output for a".to_string(), PathBuf::from("/scans/a.tif"))];
        assert!(matches!(
            ensure_roll_targets_distinct(&inputs, &clobber_input),
            Err(NcError::Usage(_))
        ));
        let sibling_collision = vec![
            (
                "output for a".to_string(),
                PathBuf::from("/out/img_positive.tiff"),
            ),
            (
                "output for b".to_string(),
                PathBuf::from("/out/img_positive.tiff"),
            ),
        ];
        assert!(matches!(
            ensure_roll_targets_distinct(&inputs, &sibling_collision),
            Err(NcError::Usage(_))
        ));
        // Distinct outputs not touching any input are fine.
        let ok = vec![
            (
                "output for a".to_string(),
                PathBuf::from("/out/a_positive.tiff"),
            ),
            (
                "output for b".to_string(),
                PathBuf::from("/out/b_positive.tiff"),
            ),
        ];
        assert!(ensure_roll_targets_distinct(&inputs, &ok).is_ok());
    }

    #[test]
    fn ensure_roll_targets_distinct_protects_the_frames_manifest() {
        // `run_roll` adds the `--frames` manifest to the protected read set, so a
        // write target aimed at it (e.g. `--report-file` equal to the manifest
        // path) is rejected up front rather than clobbering the manifest.
        let manifest = Path::new("/rolls/frames.json");
        let inputs = [Path::new("/scans/a.tif"), manifest];
        let clobber_manifest = vec![(
            "--report-file".to_string(),
            PathBuf::from("/rolls/frames.json"),
        )];
        assert!(matches!(
            ensure_roll_targets_distinct(&inputs, &clobber_manifest),
            Err(NcError::Usage(_))
        ));
    }

    #[test]
    fn roll_report_puts_the_shared_recipe_once() {
        // The shared recipe *configuration* appears once at the top of the roll
        // report — carrying the tagged reconstruction — and each frame
        // additionally echoes the *resolved* base/Dmax it used (a redundant echo
        // here since the recipe pins an explicit base). The per-frame entry is
        // the data-carrying `FrameStatus` — an "ok" frame serializes the flat
        // `"status":"ok"` with its payload as sibling keys.
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::Explicit(1.6),
        });
        shared.film_base.source = FilmBaseSource::Explicit([0.9, 0.55, 0.42]);
        let roll = RollReport {
            command: "roll",
            recipe: shared,
            warnings: vec![],
            frames: vec![FrameReport {
                input: PathBuf::from("f1.tif"),
                output: Some(PathBuf::from("out/f1_positive.tiff")),
                status: FrameStatus::Ok {
                    film_base: Some(FilmBase::from([0.9, 0.55, 0.42])),
                    dmax: Some(1.6),
                    white_balance: None,
                    balance_range: None,
                    input_color: None,
                    loss: None,
                },
                warnings: vec![],
                overrides: None,
            }],
            summary: RollSummary {
                total: 1,
                succeeded: 1,
                failed: 0,
            },
            elapsed_ms: Some(1.0),
        };
        let v = serde_json::to_value(&roll).unwrap();
        assert_eq!(v["command"], "roll");
        // f32 round-trips through JSON as f64, so compare the roll-fixed anchors
        // approximately rather than bit-exactly.
        let fb: Vec<f64> = v["recipe"]["film_base"]["source"]["explicit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect();
        assert!(
            (fb[0] - 0.9).abs() < 1e-6
                && (fb[1] - 0.55).abs() < 1e-6
                && (fb[2] - 0.42).abs() < 1e-6
        );
        assert_eq!(v["recipe"]["reconstruction"]["schema_version"], 1);
        assert!(
            (v["recipe"]["reconstruction"]["curve"]["dmax"]["explicit"]
                .as_f64()
                .unwrap()
                - 1.6)
                .abs()
                < 1e-6
        );
        assert_eq!(v["summary"]["succeeded"], 1);
        // The flattened `FrameStatus::Ok` still serializes the flat `status`
        // discriminator and its payload as sibling keys of the frame entry.
        assert_eq!(v["frames"][0]["status"], "ok");
        assert_eq!(v["frames"][0]["input"], "f1.tif");
        let ffb: Vec<f64> = v["frames"][0]["film_base"]
            .as_object()
            .expect("per-frame resolved film base is a sibling key of status")
            .values()
            .map(|x| x.as_f64().unwrap())
            .collect();
        assert_eq!(ffb.len(), 3);
        assert!((v["frames"][0]["dmax"].as_f64().unwrap() - 1.6).abs() < 1e-6);
    }

    #[test]
    fn failed_frame_report_keeps_accumulated_warnings() {
        // A frame that warned before failing still carries those warnings in its
        // report entry (they aren't reset to empty on the failure path).
        let pf = PlannedFrame {
            input: PathBuf::from("bad.tif"),
            output: PathBuf::from("out/bad_positive.tiff"),
            cfg: ResolvedConfig::default(),
            overrides: None,
            dmax_setting: DmaxSetting::Default,
        };
        let warnings = vec!["a warning raised before the failure".to_string()];
        let fr = frame_report_err(&pf, &NcError::Decode("boom".into()), warnings);
        let v = serde_json::to_value(&fr).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error"], "decode: boom");
        assert_eq!(
            v["warnings"][0], "a warning raised before the failure",
            "a failed frame must keep the warnings accumulated before it failed: {v}"
        );
    }
}
