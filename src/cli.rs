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
use crate::io::decode::{DecodeInfo, decode_within, probe};
use crate::io::{avif, encode, staged, ultra_hdr};
use crate::pipeline::input_semantics::{
    self, ContainerColorFacts, InputAssertions, InputColorReport, RawMode,
};
use crate::pipeline::memory::{self, MemoryReport, RunProfile, SamplePlan};
use crate::pipeline::{color, film_base, gain_map, hdr, stages, working_space};
use crate::telemetry;
use crate::types::{
    AnchorPlacement, BalanceRange, BigTiff, DensityCurve, DensityCurveType, DensityParams,
    DmaxSource, EncodeReport, FilmBase, FilmBaseParams, FilmBaseSource, FilmType, InputParams,
    MeaningAssertion, NcError, OutputParams, OutputPreset, OutputStats, PrintParams,
    Reconstruction, ReconstructionType, Result, TransferAssertion, WbSource,
};
use crate::version::{self, Identity};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// `nc` — film-negative → positive converter.
//
// `--version` prints the full build identity (semver + behavioral
// `pipeline_version` + commit + target), not just the crate version, so an output
// can be attributed to a build — see `version::version_string`.
#[derive(Parser, Debug)]
#[command(
    name = "nc",
    version = version::version_string(),
    about = "Film-negative → positive converter"
)]
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

/// The memory preflight's budget — **operational, not a conversion knob**, so it
/// lives here on the arg structs like `--report`/`--strict`/`--telemetry` and is
/// deliberately *not* a recipe key: it never enters the recipe/sidecar and can
/// never perturb a pixel (design-spec §9 Global). Shared by every subcommand that
/// decodes a scan; each gates on its own pipeline profile (`pipeline::memory`).
#[derive(Args, Debug, Default)]
pub struct MemoryArgs {
    /// Fail before decoding if this run's estimated peak memory would exceed this
    /// budget (e.g. `8GiB`, `4096MB`, raw bytes). Defaults to 6 GiB — a fixed
    /// value, so the pass/fail decision is the same on every machine. Operational
    /// flag — not a recipe key; never affects the output image.
    #[arg(long = "max-memory", value_name = "BYTES", value_parser = parse_max_memory_arg)]
    pub max_memory: Option<u64>,
}

impl MemoryArgs {
    /// The run's resolved budget (the flag, else the fixed default).
    fn budget(&self) -> memory::Budget {
        memory::Budget::resolve(self.max_memory)
    }
}

/// clap adapter for [`memory::parse_max_memory`] — clap wants a `String` error.
fn parse_max_memory_arg(s: &str) -> std::result::Result<u64, String> {
    memory::parse_max_memory(s).map_err(|e| e.to_string())
}

/// `inspect`: an input scan plus reporting controls.
#[derive(Args, Debug)]
pub struct IoArgs {
    /// Input negative scan (SilverFast HDR/HDRi TIFF).
    pub input: PathBuf,
    /// Declared film chemistry (`silver` | `chromogenic`). When `chromogenic` and
    /// the scan carries an IR plane, `inspect` reports the IR film-holder mask and
    /// the candidate rebate search runs only on film segments. `silver` / the
    /// unknown default keep the IR path off. See `convert --film-type`.
    #[arg(long = "film-type", value_enum, value_name = "TYPE")]
    pub film_type: Option<FilmType>,
    #[command(flatten)]
    pub memory: MemoryArgs,
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
    /// Declared film chemistry (`silver` | `chromogenic`). When `chromogenic` and
    /// the scan carries an IR plane, auto film-base detection uses the IR holder
    /// mask to exclude holder-occluded edge segments from the rebate search;
    /// `silver` / the unknown default keep the IR path off. See
    /// `convert --film-type`.
    #[arg(long = "film-type", value_enum, value_name = "TYPE")]
    pub film_type: Option<FilmType>,
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
    pub memory: MemoryArgs,
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
    /// Output positive path (TIFF for legacy/master, JPEG for Ultra HDR v1).
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: PathBuf,
    /// Reconstruction type (default `density`).
    #[arg(long, value_enum)]
    pub reconstruction: Option<ReconstructionType>,
    /// Density-to-positive curve (with `--reconstruction density`; default
    /// `sigmoid`).
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
    pub memory: MemoryArgs,
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
    pub memory: MemoryArgs,
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
    /// Declared film chemistry (`silver` | `chromogenic` | `unknown`). Enables
    /// IR-assisted film-holder detection: chromogenic dyes (C-41 colour and
    /// C-41-process B&W) are IR-transparent, so the holder is separable from film;
    /// `silver` (IR-opaque) and the `unknown` default keep the IR path off. Recipe
    /// key `input.film_type`. This is a shared input-medium declaration — the black
    /// & white `bw-support` task and the separate IR dust-removal task reuse the
    /// same knob.
    #[arg(long = "film-type", value_enum, value_name = "TYPE")]
    pub film_type: Option<FilmType>,
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
    /// Detect the unexposed rebate band behind the film holder. Best-effort and
    /// fails loudly when no confident band exists — real scans put a thin inset
    /// rebate *behind* the holder, not at the outer margin. **No longer the
    /// default**: `convert` requires one of these three flags **or** the
    /// `film_base.source` recipe key, because `Dmin` is a per-roll calibration
    /// that sets black point and colour balance together, and arriving at it by
    /// omission decided that for you. `roll` requires the same choice but takes
    /// **none of these flags** — it accepts only the recipe key, in the shared
    /// `--params` file. The measurement commands are unaffected,
    /// since they exist to produce a base: `estimate` resolves an unstated source
    /// to this, and `inspect` always runs the detector (it takes no film-base
    /// flags at all).
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
    /// Pin mid-grey (18%) at fraction F of the reference density, letting display
    /// white fall above it — the default anchoring rule, F 0.5. Raising F renders
    /// the roll darker, lowering it brighter. Mutually exclusive with
    /// `--sigmoid-white-at-d-max`.
    #[arg(
        long = "sigmoid-mid-fraction",
        value_name = "F",
        conflicts_with = "sigmoid_white_at_d_max"
    )]
    pub sigmoid_mid_fraction: Option<f32>,
    /// Pin display white *at* the reference density instead of placing mid-grey
    /// (the pre-2026-08 rule). Kept as an explicit diagnostic: at a photographic
    /// contrast it renders midtones 2.5–3.6 stops dark, because steepening the
    /// slope pivots the line about white. Sensible only when the reference is
    /// itself a diffuse white — and measuring that off frame content is
    /// per-frame exposure correction, which the default must not do.
    #[arg(long = "sigmoid-white-at-d-max")]
    pub sigmoid_white_at_d_max: bool,
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
    /// Highlight roll-off amount; named SDR/HDR branches resolve their own knee.
    #[arg(long)]
    pub highlight_compress: Option<f32>,
    /// Black/white-range placement endpoints for the shared display stage: the
    /// exact affine `(x - LOW)/(HIGH - LOW)` applied last, after white balance,
    /// exposure, and the black point (recipe key `print.linear_range`, default
    /// `0,1` = identity). This is the replacement home for `simple`
    /// reconstruction's removed `--clip-low`/`--clip-high` endpoints. A negative
    /// `LOW` is legal, so a leading `-` is accepted. Only a named display preset
    /// consumes it, so a non-default value is currently a loud usage error rather
    /// than a silently-ignored knob.
    #[arg(long = "linear-range", value_name = "LOW,HIGH", value_parser = parse_lo_hi,
          allow_hyphen_values = true)]
    pub linear_range: Option<[f32; 2]>,
}

/// Removed simple-reconstruction controls (design-spec §7.1/§9). Simple
/// reconstruction now ends at the direct unclamped positive `1 − scan/Dmin`;
/// its old inversion white balance and clip-range remap are **not**
/// reconstruction parameters — they return downstream (as explicit
/// `print.white_balance` and `print.linear_range`). The replacements are already
/// consumed by `ultra-hdr-v1`, but warned alias acceptance remains deliberately
/// deferred to the complete `output/presets` migration. Since their defaults were the exact identity, the default simple
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
///
/// `--output-preset` is the atomic output *policy* choice; the three legacy
/// selectors below (`--output-hdr`/`--output-sdr`, `--output-profile`,
/// `--bigtiff`) are the pre-preset depth/profile/container knobs a **named**
/// preset resolves itself, so a non-default one alongside it is a usage error —
/// checked on the *resolved value* in [`validate_output_preset`], identically for
/// a flag and for a recipe key. `--output-sdr` is the one exception: it *forces*
/// 16-bit integer output, which a named preset cannot produce, so it is rejected by
/// flag **presence** ([`reject_output_sdr_with_named_preset`]) even though its
/// resolved value is the default. (None of this can be a clap `conflicts_with`: the
/// conflict depends on the preset's *value* — `--output-preset legacy` is the
/// no-preset state and stays compatible with all three — and, for the other
/// selectors, on the selector's own value, since `--bigtiff auto` resolves the
/// documented default and is therefore accepted.)
#[derive(Args, Debug, Default)]
pub struct OutputOverrides {
    /// Named output policy: `legacy` (default — the transitional TIFF path, where
    /// the print controls run before the output ICC transform), `film-master`
    /// (unclamped 32-bit float linear ACEScg TIFF taken straight from the NC film
    /// RGB v1 mapping, bypassing every print/display control), `ultra-hdr-v1`
    /// (legacy gain-map JPEG through the shared display stages), `hdr-pq` /
    /// `hdr-hlg` (10-bit 4:4:4 Rec.2100 PQ/HLG AVIF, requires `.avif`), or
    /// `hdr-linear-tiff` (32-bit float display-linear BT.2020 interchange TIFF with
    /// no transfer applied), or `hdr-pq-tiff` / `hdr-hlg-tiff` (the same Rec.2100
    /// PQ/HLG signal as the AVIF presets, stored as full-range 16-bit TIFF code
    /// values) — the three TIFF HDR presets all require `.tif`/`.tiff`. Recipe key
    /// `output.preset`. A named preset is atomic: it resolves container, depth, and
    /// profile itself, so it rejects a non-default `--output-hdr` /
    /// `--output-profile` / `--bigtiff` (from a flag or the recipe alike; a value that
    /// already equals the documented default, like `--bigtiff auto`, is accepted), and
    /// rejects `--output-sdr` outright — it forces 16-bit integer output a named preset
    /// cannot produce. On top
    /// of that, `film-master` rejects the frame-local measurements
    /// `--auto-d-max`/`--auto-balance-range` plus every non-default
    /// downstream control; every display preset consumes those controls instead.
    /// `--output-hdr` is a *rendered* float TIFF: it is never an alias for
    /// `film-master`, nor for `hdr-linear-tiff` (which is display-linear BT.2020,
    /// not the selected output space). The remaining planned preset names
    /// (`gain-map-hdr`, `display-p3`, `compatibility`, `custom`) are not accepted
    /// yet.
    #[arg(long = "output-preset", value_name = "PRESET")]
    pub output_preset: Option<String>,
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
    /// The resolved **reference** density — the roll calibration (`curve.dmax`). Since
    /// `algo/reference-anchored-sigmoid` this is not necessarily the density that rendered
    /// to `1.0`; see `anchor` / `anchor_value`.
    pub dmax: DmaxResolution,
    /// The sigmoid's anchor **placement rule** — which tone the reference pins
    /// (design-spec §7.3). `null` for the exponential curve, which has no such rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorPlacement>,
    /// The **derived** anchor: the corrected density this render mapped to `1.0`, which
    /// sets the black floor at `10^(−contrast·anchor_value)`. Equal to `dmax.value` for the
    /// exponential curve and for the sigmoid's `white-at-dmax` placement; larger under the
    /// default mid-grey placement. Always emitted so the block is self-contained.
    pub anchor_value: Option<f32>,
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
    curve_anchor: Option<f32>,
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
                    // The placement rule and the anchor it derived. Both are needed for
                    // this block to be self-contained: `dmax.value` is the reference, and
                    // for the default sigmoid placement the density that actually rendered
                    // to 1.0 is a *different* number. Without them a consumer has to
                    // re-derive the anchor from the echoed recipe to know what the render
                    // did — which is what "diagnostics" is supposed to spare them.
                    anchor: match curve {
                        DensityCurve::Sigmoid(sig) => Some(sig.anchor),
                        DensityCurve::Exponential(_) => None,
                    },
                    anchor_value: curve_anchor,
                },
            }
        }
    }
}

/// What the AVIF encoder coded, for the resolved report. Serialize-only.
///
/// Every field is read back out of the produced file rather than restated from the
/// request, so the report is evidence about the artifact and not an echo of the
/// configuration. In particular `profile` records whether the file may claim the
/// AVIF v1.2 Advanced Profile, and `profile_reason` says why not when it may not —
/// a general-brand-only file is a legitimate output, but never a silent one.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AvifResult {
    /// `"advanced"` when the `MA1A` brand was written, else `"general-brand-only"`.
    pub profile: &'static str,
    /// Which published limit put the file outside the Advanced Profile. Absent when
    /// `profile` is `"advanced"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_reason: Option<String>,
    /// Coded bit depth (always 10 in this build).
    pub bit_depth: u8,
    /// AV1 `seq_profile` parsed from the codestream (1 = High, required for 4:4:4).
    pub seq_profile: u8,
    /// AV1 `seq_level_idx` parsed from the codestream. 16 is level 6.0, the
    /// Advanced Profile ceiling.
    pub seq_level_idx: u8,
    /// Human-readable level, e.g. `"2.0"`, derived from `seq_level_idx`.
    pub level: String,
    /// CICP colour primaries / transfer / matrix coefficients as coded.
    pub cicp: [u8; 3],
    /// Whether full-range coding was signalled.
    pub full_range: bool,
    /// Size of the AV1 codestream in bytes, excluding container boxes.
    pub codestream_bytes: usize,
}

/// What the `hdr-linear-tiff` encoder wrote, and the luminance semantics the file
/// cannot state for itself. Serialize-only.
///
/// **This block is authoritative for the HDR semantics, and deliberately so.** The
/// embedded ICC profile describes the colorimetry (BT.2020 primaries, D65, a linear
/// TRC) but its PCS stops at the media white, so no v4 profile can express that
/// `1.0` is 203 cd/m² and that highlights legitimately run to
/// `linear_headroom`. Anything consuming these files for luminance must read this,
/// not the profile — `interoperability` says so in the artifact itself rather than
/// leaving it to documentation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HdrLinearTiffResult {
    /// Stable identifier of the pixel contract
    /// ([`encode::HDR_LINEAR_PIXEL_CONTRACT`]).
    pub pixel_contract: &'static str,
    /// Bits per sample as written (32).
    pub bits_per_sample: u16,
    /// TIFF `SampleFormat` as written (3 = IEEE float).
    pub sample_format: u16,
    /// Whether the file was written as BigTIFF.
    pub bigtiff: bool,
    /// Size of the embedded linear-BT.2020 ICC profile, in bytes.
    pub icc_bytes: usize,
    /// The sample value that represents diffuse reference white, always `1.0`.
    pub reference_white_sample: f32,
    /// Reference white in cd/m² (the binding 203).
    pub reference_white_nits: f32,
    /// Mastering target peak in cd/m² (the binding 1000).
    pub target_peak_nits: f32,
    /// The sample value that represents `target_peak_nits` (≈4.926108) — the
    /// largest value the renderer will produce, and the reason this output cannot
    /// be a 16-bit integer TIFF.
    pub linear_headroom: f32,
    /// Resolved highlight-shoulder control and where the shoulder begins.
    pub highlight_compress: f32,
    pub shoulder_start: f32,
    /// Pinned tone-curve / gamut-mapping / linear-domain identifiers, straight from
    /// the renderer's own metadata rather than restated here.
    pub tone_curve: &'static str,
    pub gamut_mapping: &'static str,
    pub linear_domain: &'static str,
    /// This frame's **measured** light levels in cd/m² — peak and frame-average
    /// pixel luminance, not the mastering policy above.
    pub max_cll_nits: u16,
    pub max_fall_nits: u16,
    /// Plain statement of what the file alone does and does not communicate.
    pub interoperability: &'static str,
}

/// What the `hdr-pq-tiff` / `hdr-hlg-tiff` encoder wrote: the signalling contract,
/// the one quantization step's measured cost, and the honest limits of the file.
/// Serialize-only.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HdrCodedTiffResult {
    /// Stable identifier of the pixel contract (see `io::encode`).
    pub pixel_contract: &'static str,
    /// Bits per sample as written (16).
    pub bits_per_sample: u16,
    /// TIFF `SampleFormat` as written (1 = unsigned integer).
    pub sample_format: u16,
    /// Whether the file was written as BigTIFF.
    pub bigtiff: bool,
    /// Size of the embedded ICC profile in bytes.
    pub icc_bytes: usize,
    /// The CICP triple the embedded profile's `cicp` tag declares, as
    /// `[ColourPrimaries, TransferCharacteristics, MatrixCoefficients]`.
    ///
    /// **MatrixCoefficients is 0 here and 9 in the `avif` block for the same
    /// rendition**, and that is required rather than inconsistent:
    /// ICC.1:2022 §10.3 mandates 0 for an RGB data space, while AVIF stores
    /// Y'CbCr.
    pub cicp: [u8; 3],
    /// Whether full-range coding is signalled (always `true`).
    pub full_range: bool,
    /// Largest quantization error over the frame, in code units. At most `0.5` by
    /// construction — rounding cannot be worse than half a step.
    pub max_quantization_error_codes: f32,
    /// Root-mean-square quantization error over the frame, in code units.
    pub rms_quantization_error_codes: f32,
    /// Reference white in cd/m² (203) and the mastering peak (1000).
    pub reference_white_nits: f32,
    pub target_peak_nits: f32,
    /// This frame's **measured** peak and average light levels in cd/m², for PQ.
    ///
    /// Present only for PQ, mirroring the `clli` box `io::avif` writes for the same
    /// rendition: the values are absolute luminance, which HLG — being
    /// display-referred — cannot state. TIFF has no `clli` equivalent, so without
    /// these fields the measurement `pipeline::hdr::render_linear` took would be lost
    /// from both the file and the report, leaving a consumer tone-mapping this image
    /// with no way to learn its actual peak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cll_nits: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fall_nits: Option<u16>,
    /// HLG's reference-display assumptions, absent for PQ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hlg_system_gamma: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hlg_reference_display_peak_nits: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hlg_reference_display_black_nits: Option<f32>,
    /// What the file does and does not establish, stated in the artifact.
    pub interoperability: &'static str,
}

/// Which branch out of the NC film RGB v1 ACEScg boundary this conversion took,
/// and what that branch did (design-spec §5/§8). Serialize-only.
///
/// Exists so a consumer can tell — without re-deriving it from the recipe —
/// whether the pixels are the unclamped linear film master or a rendered image,
/// and so the master's content claim is explicit: it is NC's *intentional film
/// rendering*, never a physical scene-linear recovery.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct OutputRenderResult {
    /// The resolved output preset (`"legacy"` / `"film-master"`).
    pub preset: OutputPreset,
    /// Whether the print / tone-render sub-stage (white balance, exposure, black
    /// point, range placement, highlight roll-off) **ran at all** — not whether its
    /// values were non-default. Always `false` for `film-master` (the branch never
    /// reaches the stage), and also `false` for legacy `simple`, whose positive
    /// passes through untouched (design-spec §7.1).
    pub print_controls: bool,
    /// Whether any display rendering — tone mapping, destination gamut mapping, or
    /// a transfer/display encoding — ran. Always `false` for `film-master` (its
    /// ACEScg profile is a *linear* tag on already-ACEScg values, not a transform).
    pub display_render: bool,
    /// The encoding the preset resolved to, as a stable identifier.
    pub encoding: &'static str,
    /// What the pixels contain. For `film-master` this states the intentional-film
    /// content and explicitly disclaims physical scene recovery; it names the
    /// roll-fixed `Dmax` placement only when the run actually made one (`simple` and
    /// exponential `dmax = none` make none).
    pub content: &'static str,
    /// The pinned working-space mapping identifier the pixels crossed
    /// (`"nc-film-rgb-v1"`), repeated inside this block so a master's provenance
    /// is self-contained. Mirrors the top-level `working_mapping`.
    pub working_mapping: &'static str,
    /// Wire-schema version of the tagged `reconstruction` object the master was
    /// built from (`reconstruction.schema_version`). Recorded so a master names
    /// every version it depends on. The **behavioural** `pipeline_version` is a
    /// separate field owned by `core/conversion-versioning`; this build does not
    /// stamp one yet, so it is deliberately absent rather than guessed.
    pub reconstruction_schema_version: u32,
}

/// Whether this reconstruction places a display-white anchor the master's report
/// may claim. `simple` has no curve stage, and the exponential curve's `dmax =
/// none` is the scene-referred unity placement — neither anchors anything.
/// (`auto` cannot appear here: `validate_output_preset` rejects it under the
/// preset.)
fn master_places_dmax(reconstruction: &Reconstruction) -> bool {
    matches!(
        reconstruction,
        Reconstruction::Density { curve, .. } if curve.dmax() != DmaxSource::None
    )
}

/// Build the report's `output_render` from the resolved config. Pure derivation
/// (like [`reconstruction_result`]): the branch and what it applies are fully
/// determined by the preset plus the reconstruction, so there is nothing to thread
/// back from the render.
fn output_render_result(cfg: &ResolvedConfig) -> OutputRenderResult {
    let (print_controls, display_render, encoding, content) = match cfg.output.preset {
        OutputPreset::FilmMaster => (
            false,
            false,
            "unclamped-linear-acescg-float-tiff",
            // A `Dmax` placement is *supported*, not guaranteed: validation
            // deliberately accepts `dmax = none` for the exponential curve (the
            // scene-referred unity placement) and `simple` has no anchor at all, so
            // claiming one unconditionally would be a false provenance statement on
            // exactly the runs a consumer most needs to distinguish.
            if master_places_dmax(&cfg.reconstruction) {
                "intentional film rendering (film, lens, development, scanner, \
                 reconstruction, density curve, and the resolved roll-fixed Dmax \
                 placement); not a physical scene-linear recovery"
            } else {
                "intentional film rendering (film, lens, development, scanner, and \
                 reconstruction; this run placed no Dmax anchor); not a physical \
                 scene-linear recovery"
            },
        ),
        OutputPreset::Legacy => (
            // The legacy print render only runs for a density reconstruction;
            // `simple`'s positive passes through it untouched (design-spec §7.1).
            matches!(cfg.reconstruction, Reconstruction::Density { .. }),
            // The legacy path always runs the working→output ICC transform.
            true,
            match cfg.output.depth() {
                crate::types::OutDepth::F32 => "transitional-rendered-float-tiff",
                crate::types::OutDepth::U16 => "rendered-u16-tiff",
            },
            "print-rendered positive in the selected output colour space; the \
             transitional float form is already print-rendered and is not a film master",
        ),
        OutputPreset::UltraHdrV1 => (
            true,
            true,
            "legacy-ultra-hdr-v1-xmp-mpf-jpeg",
            "independently rendered SDR Display P3 base and HDR rendition, paired \
             by a single-channel luminance legacy Ultra HDR v1 gain map; not ISO 21496-1",
        ),
        OutputPreset::HdrPq => (
            true,
            true,
            "rec2100-pq-10bit-444-avif",
            "single-rendition display HDR: BT.2020 primaries with the ST 2084 (PQ) \
             transfer, 203 cd/m² reference white and a 1000 cd/m² mastering peak; \
             a rendered display image, not a film master",
        ),
        OutputPreset::HdrLinearTiff => (
            true,
            true,
            "display-linear-bt2020-float-tiff",
            "display-linear HDR interchange: BT.2020/D65 primaries with **no \
             transfer function applied**, samples relative to the 203 cd/m² \
             reference white and running to the 1000 cd/m² peak at ≈4.926108. \
             Print controls, the reference-white-preserving shoulder and BT.2020 \
             gamut mapping have all run, so this is a rendered display image and \
             not a film master; and being linear it is not a Rec.2100 PQ/HLG signal \
             either",
        ),
        OutputPreset::HdrPqTiff => (
            true,
            true,
            "rec2100-pq-u16-tiff",
            "single-rendition display HDR: BT.2020 primaries with the ST 2084 (PQ) \
             transfer, 203 cd/m² reference white and a 1000 cd/m² mastering peak, \
             stored as full-range 16-bit code values. 16 bits is TIFF's \
             quantization, not one of BT.2100's own bit depths (it specifies 10 and \
             12); the stored codes are exact and the one quantization step is \
             reported. A rendered display image, not a film master",
        ),
        OutputPreset::HdrHlgTiff => (
            true,
            true,
            "rec2100-hlg-u16-tiff",
            "single-rendition display HDR: BT.2020 primaries with the HLG transfer \
             under the reference 1000-nit zero-black OOTF at system gamma 1.2, \
             stored as full-range 16-bit code values (TIFF's quantization, not a \
             BT.2100 bit depth). The embedded ICC profile is scene-referred because \
             HLG's OOTF is not per-channel separable; this block and the CICP tag \
             carry the display-referred contract. A rendered display image, not a \
             film master",
        ),
        OutputPreset::HdrHlg => (
            true,
            true,
            "rec2100-hlg-10bit-444-avif",
            "single-rendition display HDR: BT.2020 primaries with the HLG transfer \
             under the reference 1000-nit zero-black OOTF at system gamma 1.2; \
             a rendered display image, not a film master",
        ),
    };
    OutputRenderResult {
        preset: cfg.output.preset,
        print_controls,
        display_render,
        encoding,
        content,
        working_mapping: working_space::WORKING_MAPPING_ID,
        reconstruction_schema_version: crate::types::RECONSTRUCTION_SCHEMA_VERSION,
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
    /// What produced this output: build identity (`nc_version`, `git_commit`,
    /// `git_dirty`, `target`), the behavioral `pipeline_version`, and — for
    /// `convert`/`roll` — the `params_hash` of the effective recipe
    /// (`core/conversion-versioning`). Purely operational provenance: it has no CLI
    /// flag and no recipe key, and never perturbs an output pixel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
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
    /// Which branch out of the ACEScg boundary the conversion took and what that
    /// branch applied (`convert`): the resolved preset, whether print controls or
    /// display rendering ran, the resolved encoding, and the master's explicit
    /// content claim (design-spec §5/§8). See [`OutputRenderResult`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_render: Option<OutputRenderResult>,
    /// What the AVIF encoder actually coded (`convert` with `hdr-pq` / `hdr-hlg`):
    /// the profile the file may claim and why, the AV1 profile/level read back out
    /// of the codestream, the CICP triple, and the coded size. Absent for every
    /// other preset. Provenance for the conformance claim — an agent can check the
    /// brand decision without re-parsing the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avif: Option<AvifResult>,
    /// What the `hdr-linear-tiff` encoder wrote (`convert` with that preset), and
    /// the reference-white / peak / headroom semantics the embedded ICC cannot
    /// carry. Absent for every other preset. **Authoritative** for this output's
    /// luminance meaning — see [`HdrLinearTiffResult`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr_linear_tiff: Option<HdrLinearTiffResult>,
    /// What the `hdr-pq-tiff` / `hdr-hlg-tiff` encoder wrote (`convert` with either
    /// preset): the CICP signalling, the measured quantization cost, and the
    /// documented interoperability limits. Absent for every other preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr_coded_tiff: Option<HdrCodedTiffResult>,
    /// What the decoder found (`inspect`): format, dimensions, channels, bit
    /// depth, IR presence, scanner metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode: Option<DecodeInfo>,
    /// What the memory preflight decided before this run decoded anything: the
    /// estimated peak with its per-phase breakdown, the budget and where it came
    /// from, the verdict, and the detected RAM the warn tier used
    /// (`pipeline::memory`). Present on every command that decodes a scan.
    /// Operational provenance — the budget is not a recipe key and never
    /// influences the pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryReport>,
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
    /// IR film-holder classification per edge (`inspect`, only with
    /// `--film-type chromogenic` on a scan carrying an IR plane): which along-edge
    /// segments the opaque holder occludes (dark in IR) vs actual film (bright).
    /// Holder segments are excluded from the rebate search; a fully-film or
    /// fully-holder edge is the all-segments-agree case. RGB alone cannot make
    /// this call — holder and dense film are both dark in RGB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_mask: Option<Vec<film_base::EdgeHolderMask>>,
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
    /// Per-channel mean of the samples as written (`convert`) — the comparison
    /// basis `nctool compare` diffs across two builds (per-channel mean ΔRGB is
    /// the difference of these means). Report-only, like `loss`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_stats: Option<OutputStats>,
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

/// A loaded recipe plus the load-time facts the report needs: whether the file
/// explicitly set `reconstruction.curve.dmax` (after defaults fill in, a recipe
/// that *wrote* `"fixed"` is indistinguishable from one that omitted it — the raw
/// JSON is the only witness), and the `pipeline_version` an enveloped sidecar
/// records for the build that produced it.
#[derive(Debug)]
struct LoadedRecipe {
    cfg: ResolvedConfig,
    curve_dmax_present: bool,
    /// `meta.pipeline_version` from a sidecar envelope, when the loaded file
    /// carried one. Provenance only — never applied, only compared (see
    /// [`pipeline_version_warning`]).
    meta_pipeline_version: Option<u32>,
    /// How far the recipe left the density curve unpinned (an omitted `curve`, or a
    /// sigmoid without `reconstruction.curve.anchor`) — the witness behind
    /// [`curve_default_warning`].
    unpinned_curve: Option<UnpinnedCurve>,
}

/// The sidecar document written beside every converted frame:
/// `{ "meta": {…identity…}, "params": {…recipe…} }`.
///
/// The envelope exists so a conversion's identity (`nc_version`, commit,
/// `pipeline_version`, `params_hash`) can ride with its recipe **without** becoming
/// recipe keys: every recipe struct is `deny_unknown_fields`, so bare identity keys
/// would make each new sidecar fail to reload through `--params`
/// (`core/conversion-versioning`). `meta` is provenance about the run that produced
/// the file and is never applied on load; `params` is the byte-for-byte recipe body
/// (`--dump-params`'s exact shape).
#[derive(Debug, Serialize)]
struct SidecarEnvelope<'a> {
    meta: SidecarMeta<'a>,
    params: &'a ResolvedConfig,
}

/// The sidecar's `meta` block: run identity, plus the output artifact's own
/// contract when its container cannot carry one.
///
/// **Why the contract has to live here and not beside `params`.** The HDR TIFFs'
/// luminance semantics — reference white, peak, headroom, tone/gamut identifiers,
/// the measured quantization cost — are deliberately *not* recipe keys, and the
/// embedded ICC provably cannot express them (its PCS stops at the media white). The
/// task requires the **sidecar** to be authoritative for them, so putting them only
/// in the stdout `Report` loses them on any run that discards it (`--report none` is
/// exactly how a batch script would call this).
///
/// It cannot be a third sibling key either: [`SidecarEnvelopeIn`] is
/// `deny_unknown_fields`, so `{meta, params, output}` would make **every** new
/// sidecar fail to reload through `--params`. Inside `meta` is safe because the read
/// side keeps `meta` as an ignored raw `Value`.
///
/// The blocks are the *same types* the report serializes, so the sidecar and the
/// report cannot drift apart.
#[derive(Debug, Serialize)]
struct SidecarMeta<'a> {
    #[serde(flatten)]
    identity: &'a Identity,
    #[serde(skip_serializing_if = "Option::is_none")]
    hdr_linear_tiff: Option<HdrLinearTiffResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hdr_coded_tiff: Option<HdrCodedTiffResult>,
}

/// The read side of [`SidecarEnvelope`]. `meta` is kept as a raw `Value` on
/// purpose: it is provenance, so an older build must not reject a newer build's
/// extra `meta` fields, and nothing in it may influence the conversion. `params`
/// is likewise raw here so the *identical* body checks (migration errors, the
/// `curve.dmax` witness, the typed `deny_unknown_fields` parse) apply to an
/// enveloped and a bare recipe alike. `deny_unknown_fields` at this level keeps a
/// third sibling key from being silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarEnvelopeIn {
    #[serde(default)]
    meta: Option<serde_json::Value>,
    params: serde_json::Value,
}

/// Load a recipe file into a [`ResolvedConfig`], or the defaults when no recipe
/// is given. A read failure or invalid/unknown-key JSON is a usage error;
/// removed legacy keys get pinned migration errors first
/// ([`reject_legacy_recipe_keys`]) rather than opaque serde messages.
///
/// Accepts **both** shapes, so the established round-trip keeps working:
/// - the sidecar envelope `{ "meta": …, "params": {…recipe…} }` — identity is read
///   for provenance and otherwise ignored;
/// - a bare recipe object (a hand-written recipe, `--dump-params` output, or a
///   pre-envelope sidecar).
///
/// The two are told apart by the presence of a top-level `params` key, which is not
/// (and must never become) a recipe key.
fn load_recipe(path: Option<&Path>) -> Result<LoadedRecipe> {
    match path {
        None => Ok(LoadedRecipe {
            cfg: ResolvedConfig::default(),
            curve_dmax_present: false,
            meta_pipeline_version: None,
            // No recipe file means nothing was archived and nothing is being
            // reinterpreted — the run simply *is* this build's defaults. Only a
            // loaded document can carry the "written when the defaults were
            // different" problem the warning describes.
            unpinned_curve: None,
        }),
        Some(p) => {
            let txt = std::fs::read_to_string(p)
                .map_err(|e| NcError::Usage(format!("cannot read recipe {}: {e}", p.display())))?;
            // Parse to a raw Value first to pick the shape and to run the
            // migration checks / dmax-presence witness on the recipe *body*; the
            // typed parse below still owns shape and unknown-key validation.
            // Unparseable JSON falls through to the typed parse's error (its
            // message names the recipe).
            let value: Option<serde_json::Value> = serde_json::from_str(&txt).ok();
            let context = format!("recipe {}", p.display());
            // A recipe (or an envelope) is an OBJECT. serde's derived visitor accepts
            // a sequence for a struct and every `ResolvedConfig` field has a default,
            // so a bare `[]` would otherwise convert with all-default parameters and
            // exit 0 — the same silent-defaults trap as `{"params": []}`, one level up.
            if let Some(v) = &value
                && !v.is_object()
            {
                return Err(NcError::Usage(format!(
                    "{context}: a recipe must be a JSON object, got {}. A non-object \
                     document would convert with all-default parameters",
                    json_kind(v)
                )));
            }
            let (envelope_body, meta_pipeline_version) =
                match split_envelope(value.as_ref(), &context)? {
                    Some((body, meta_version)) => (Some(body), meta_version),
                    None => (None, None),
                };
            // The recipe *body*: an envelope's `params`, else the whole document.
            let body = envelope_body.as_ref().or(value.as_ref());
            if let Some(v) = body {
                reject_legacy_recipe_keys(v, &context)?;
            }
            let usage = |e| NcError::Usage(format!("invalid recipe {}: {e}", p.display()));
            // Bare recipes keep parsing straight from the file text, so their
            // (line/column-bearing) serde diagnostics are unchanged.
            let cfg = match &envelope_body {
                Some(v) => serde_json::from_value(v.clone()).map_err(usage)?,
                None => serde_json::from_str(&txt).map_err(usage)?,
            };
            let curve_dmax_present = body.is_some_and(sets_curve_dmax);
            Ok(LoadedRecipe {
                cfg,
                curve_dmax_present,
                meta_pipeline_version,
                unpinned_curve: body.and_then(unpinned_curve),
            })
        }
    }
}

/// Split a loaded document into `(recipe body JSON, meta.pipeline_version)` when it
/// is a sidecar envelope; `None` when it is a bare recipe (the legacy / hand-written
/// shape) and the caller should use the file text as-is.
///
/// A document carrying `meta` but no `params` is a *malformed* envelope, not a bare
/// recipe: it gets a pointed error rather than the opaque `unknown field 'meta'`
/// serde would produce.
///
/// `params` must be a JSON **object**. serde's derived visitor happily accepts a
/// *sequence* for a struct, and every [`ResolvedConfig`] field has a default, so
/// `{"params": []}` would otherwise convert with all-default parameters and a
/// `params_hash` byte-identical to the default recipe's — a truncated or
/// mis-generated sidecar silently converting with defaults instead of the recipe the
/// operator believes is applied, which is exactly the round-trip contract the
/// envelope exists to keep.
fn split_envelope(
    value: Option<&serde_json::Value>,
    context: &str,
) -> Result<Option<(serde_json::Value, Option<u32>)>> {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return Ok(None);
    };
    if !obj.contains_key("params") {
        if obj.contains_key("meta") {
            return Err(NcError::Usage(format!(
                "{context}: has a `meta` block but no `params` — a sidecar envelope \
                 is `{{\"meta\": {{…}}, \"params\": {{…recipe…}}}}`; a bare recipe \
                 object must not contain `meta`"
            )));
        }
        return Ok(None);
    }
    let envelope: SidecarEnvelopeIn = serde_json::from_value(value.unwrap().clone())
        .map_err(|e| NcError::Usage(format!("{context}: invalid sidecar envelope: {e}")))?;
    if !envelope.params.is_object() {
        return Err(NcError::Usage(format!(
            "{context}: sidecar `params` must be a recipe OBJECT, got {}. A non-object \
             `params` would convert with all-default parameters instead of the recipe \
             this file claims to carry",
            json_kind(&envelope.params)
        )));
    }
    // `meta`, when the document has the key at all, must be an OBJECT. Checked
    // against the raw JSON rather than `envelope.meta`, because serde folds
    // `"meta": null` into the same `None` an omitted key produces — and an omitted
    // `meta` is legal (a bare `--dump-params` recipe wrapped by hand).
    //
    // Without this, a corrupt *container* is silently softer than a corrupt *field*:
    // `Value::get` on a non-object returns `None`, which this path reads as "records
    // no pipeline_version", so `"meta": null` / `"x"` / `[]` replayed with **no skew
    // check at all**, while `{"pipeline_version": "1"}` inside a well-formed `meta`
    // is a loud exit 2. Malformed provenance must be as loud as an unreadable field.
    // (Unknown *fields* inside a well-formed `meta` stay lenient on purpose — that is
    // the forward-compatibility contract: an older build must tolerate a newer
    // build's extra provenance.)
    if let Some(meta) = obj.get("meta")
        && !meta.is_object()
    {
        return Err(NcError::Usage(format!(
            "{context}: sidecar `meta` must be an object, got {}. A malformed `meta` \
             carries no readable provenance, and treating it as absent would silently \
             skip the pipeline_version skew check this envelope exists to enable — \
             omit `meta` entirely if the recipe has no provenance to record",
            json_kind(meta)
        )));
    }
    let meta_pipeline_version = meta_pipeline_version(envelope.meta.as_ref(), context)?;
    Ok(Some((envelope.params, meta_pipeline_version)))
}

/// The `pipeline_version` recorded in a sidecar's `meta`, when present.
///
/// Present-but-unreadable is a **loud error**, not `None`. `None` means "this file
/// records no version" and suppresses the skew check entirely, so silently mapping a
/// `1.0`, a `"1"`, or a negative number onto it would disable the very warning the
/// label exists to raise — a sidecar round-tripped through a tool that emits `1.0`
/// would then replay on a later build and produce different pixels in silence. The
/// range check matters for the same reason in the other direction: `as u32`
/// truncation turns `4294967297` into `1`, which *matches* this build and suppresses
/// the warning by pretending to agree with it.
fn meta_pipeline_version(meta: Option<&serde_json::Value>, context: &str) -> Result<Option<u32>> {
    let Some(raw) = meta.and_then(|m| m.get("pipeline_version")) else {
        return Ok(None);
    };
    let bad = || {
        NcError::Usage(format!(
            "{context}: `meta.pipeline_version` is {raw}, which is not a pipeline version — it \
             must be a non-negative integer no larger than {}. A value nc cannot read would be \
             indistinguishable from an absent one and would silently disable the \
             pipeline_version skew warning",
            u32::MAX
        ))
    };
    let n = raw.as_u64().ok_or_else(bad)?;
    Ok(Some(u32::try_from(n).map_err(|_| bad())?))
}

/// A JSON value's kind, for error messages that need to name what was found.
fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// How far a loaded recipe left the density curve unpinned. The two shapes need
/// different warnings, because a different amount moved underneath each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnpinnedCurve {
    /// The recipe tags `curve.type = "sigmoid"` but omits `anchor`: the curve
    /// itself is pinned and only its **placement rule** floats.
    AnchorOnly,
    /// The recipe omits `reconstruction.curve` entirely, so the **curve itself**
    /// floats — and it moved on 2026-08-08.
    WholeCurve,
    /// The recipe pins the curve *type* but omits a scalar whose default moved in
    /// `pipeline_version` 2: the exponential's `gamma` (1.0 → 2.0) or either
    /// curve's `dmax` (nominal 2.0 → 1.3).
    ///
    /// Narrower than [`WholeCurve`](Self::WholeCurve) — the curve is the one the
    /// author chose — and easy to miss for exactly that reason: the recipe *looks*
    /// pinned. A recipe reading `{"curve":{"type":"exponential"}}` used to mean
    /// gamma 1.0 at anchor 2.0 and now means gamma 2.0 at anchor 1.3, which is a
    /// substantially different render with nothing in the file to show it.
    MovedDefaults,
}

/// Whether a loaded recipe resolves to a sigmoid curve it never fully pinned — the
/// witness behind [`curve_default_warning`].
///
/// Probed on the raw JSON because that is the only witness: once serde has filled
/// defaults, a recipe that omitted the curve is indistinguishable from one that
/// wrote today's default out in full.
///
/// **An omitted `curve` counts, since 2026-08-08.** The default curve is now the
/// sigmoid, so a `reconstruction` block with no `curve` resolves to a sigmoid at
/// this build's default anchor — a different curve, a different placement rule *and*
/// a different `Dmax` than the same file got before that date. That is exactly the
/// silent reinterpretation design-spec §7.3 promises not to do, and nothing else
/// catches it: a bare `--params` recipe carries no `meta.pipeline_version`, so
/// [`pipeline_version_warning`] never sees it. Answering "no warning" for a
/// curve-less `reconstruction` block was right while the curve-less resolution was
/// the anchorless exponential; it stopped being right when the default moved.
///
/// **The boundary is the presence of a `reconstruction` object, and that is a
/// judgement call worth stating.** A recipe carrying no `reconstruction` key at all
/// resolves to exactly the same render as `{"reconstruction": {"type": "density"}}`,
/// so warning about one and not the other is a seam. It is drawn there anyway,
/// because the two say different things: a recipe with a `reconstruction` block is
/// stating a reconstruction configuration, and the curve is a **hole in that
/// statement** that used to be filled differently. A recipe silent on the stage is
/// stating nothing about it — the same position as passing no recipe, where nc
/// warns nothing at all. Erasing the seam the other way would fire on essentially
/// every partial recipe and make `--strict` fail for all of them, permanently, in
/// exchange for a one-time migration aid.
///
/// Otherwise `None` only where the recipe rules the case out: `simple`
/// reconstruction (no curve stage exists), a tagged non-sigmoid curve, or an
/// explicit `anchor`.
fn unpinned_curve(v: &serde_json::Value) -> Option<UnpinnedCurve> {
    let reconstruction = v.get("reconstruction")?;
    // `simple` runs no curve stage, so no curve default can reach it. An absent
    // `type` is `density` (the serde default), which does.
    if reconstruction.get("type").and_then(|t| t.as_str()) == Some("simple") {
        return None;
    }
    let Some(curve) = reconstruction.get("curve") else {
        return Some(UnpinnedCurve::WholeCurve);
    };
    // `dmax`'s nominal moved for *both* curves, so it is checked once here — and
    // an **absent** key is not the only way it floats. `"dmax":"fixed"` names the
    // *policy*, not a value: it resolves through `NOMINAL_DMAX`, which moved
    // 2.0 → 1.3. That is the spelling `--dump-params` writes, so treating a present
    // key as pinned would have excused exactly the archived recipes people keep.
    // `"auto"` is per-frame (a different thing, not a moved default), and `"none"`
    // and `{"explicit":…}` are genuinely pinned.
    let dmax_floats = match curve.get("dmax") {
        None => true,
        Some(d) => d.as_str() == Some("fixed"),
    };
    match curve.get("type").and_then(|t| t.as_str()) {
        Some("sigmoid") => {
            if curve.get("anchor").is_none() {
                // The bigger of the two: report the placement move, which subsumes
                // a floating `dmax` in the explanation.
                Some(UnpinnedCurve::AnchorOnly)
            } else {
                dmax_floats.then_some(UnpinnedCurve::MovedDefaults)
            }
        }
        Some("exponential") => {
            (curve.get("gamma").is_none() || dmax_floats).then_some(UnpinnedCurve::MovedDefaults)
        }
        // An untagged or unknown curve object never parses, so it cannot reach a
        // render to be warned about.
        _ => None,
    }
}

/// Warn that an archived recipe will not reproduce its original render, because a
/// curve default moved underneath it.
///
/// The same situation as [`pipeline_version_warning`], one level down: the parameters
/// still apply, but a **default changed underneath them**. Two such moves are covered,
/// and they are reported separately because the remedy differs:
///
/// - [`UnpinnedCurve::AnchorOnly`] — `reconstruction.curve.anchor` was introduced by
///   `algo/reference-anchored-sigmoid` (2026-08-03) with a default of mid-grey
///   placement, replacing the previous white-at-Dmax behavior, so a recipe frozen
///   before that date renders differently even with `contrast`, `toe`, `shoulder`
///   and `dmax` all pinned.
/// - [`UnpinnedCurve::WholeCurve`] — `algo/negative-reconstruction-density-curves`
///   (2026-08-08, `pipeline_version` 2) made the sigmoid the default curve and moved
///   the nominal `Dmax` to 1.3, so a curve-less recipe that used to mean "the
///   exponential straight line at gamma 1.0, anchor 2.0" now means something else
///   entirely. Bigger than the anchor case, and previously unwarned.
///
/// Why a warning and not a `reconstruction.schema_version` bump: that constant versions the
/// **schema shape** and is checked for *exact* equality, so bumping it would reject every
/// archived recipe outright — including the many that pin a curve explicitly and are wholly
/// unaffected. Preserving the old semantics
/// per schema version is the other option, and it is a real design (a per-version default
/// table), but it would have to cover `contrast` and `shoulder` too — they moved in the same
/// commit and have the identical property — and that policy belongs with
/// `core/conversion-versioning`, not improvised here. What is not acceptable is silence, so
/// this says so loudly and `--strict` promotes it.
fn curve_default_warning(unpinned: Option<UnpinnedCurve>) -> Option<String> {
    Some(match unpinned? {
        UnpinnedCurve::AnchorOnly => "the loaded recipe selects the sigmoid curve without \
         `reconstruction.curve.anchor`, so it takes this build's default placement \
         (mid-grey at half the reference density). Recipes frozen before 2026-08-03 were \
         produced when the anchor pinned display *white* at the reference, so this render \
         will not match the original even with contrast/toe/shoulder/dmax pinned. Add an \
         explicit `anchor` (`\"white-at-dmax\"` to reproduce the old placement) to pin it."
            .to_string(),
        UnpinnedCurve::WholeCurve => "the loaded recipe omits `reconstruction.curve`, so the \
         curve is whichever one this build defaults to — since 2026-08-08 that is the \
         mid-grey-anchored sigmoid at the nominal Dmax of 1.3. The same file written before \
         that date resolved to the exponential straight line at gamma 1.0 and Dmax 2.0, so \
         this render will not match the original. Write an explicit tagged \
         `reconstruction.curve` to pin the curve, its anchor and its Dmax."
            .to_string(),
        UnpinnedCurve::MovedDefaults => "the loaded recipe pins `reconstruction.curve.type` \
         but leaves a value to this build's default that moved on 2026-08-08 \
         (`pipeline_version` 2): the exponential's `gamma` went 1.0 → 2.0 and the nominal \
         `dmax` went 2.0 → 1.3. A recipe that pins only the curve type therefore looks \
         pinned and is not — the same file renders differently than it did. Write the \
         curve's `gamma` (exponential) and its `dmax` explicitly to pin them."
            .to_string(),
    })
}

/// The warning for replaying a recipe captured under a **different** behavioral
/// `pipeline_version` than this build implements: the recipe still applies, but the
/// default render it was captured under has changed, so the pixels will not match
/// the original output. Loud (and `--strict`-promotable) rather than silent — that
/// mismatch is exactly what `pipeline_version` exists to make visible.
fn pipeline_version_warning(loaded_version: Option<u32>) -> Option<String> {
    let recorded = loaded_version?;
    (recorded != version::PIPELINE_VERSION).then(|| {
        format!(
            "the loaded recipe was produced by pipeline_version {recorded}, but this build is \
             pipeline_version {} — the parameters still apply, but the default conversion \
             behavior changed between them, so the output will not match the original",
            version::PIPELINE_VERSION
        )
    })
}

/// The canonical resolved-recipe JSON — the exact bytes `--dump-params` writes, and
/// the input to [`version::stable_hash`] for `identity.params_hash`.
///
/// The sidecar's `params` body is the same **document**, not the same bytes: nesting
/// it under `params` indents every line two extra spaces. So `params_hash` is
/// reproducible from a `--dump-params` file (`stable_hash` of its bytes) and from the
/// sidecar only as parsed JSON — the tests assert it both ways, and each in the form
/// that actually holds.
///
/// Pinning one function means the hash a report advertises always corresponds to a
/// recipe an agent can actually reproduce (`nc convert --dump-params …` then hash
/// it), and can never describe a different config than the sidecar's body.
fn canonical_params_json(cfg: &ResolvedConfig) -> Result<String> {
    serde_json::to_string_pretty(cfg)
        .map_err(|e| NcError::Other(format!("serializing effective recipe: {e}")))
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

/// Whether a recipe/override JSON object explicitly carries
/// `reconstruction.curve.anchor` — the witness behind `roll`'s fourth roll-consistency
/// warning. The anchor *placement* is a roll-level rule by design (design-spec §7.3): it
/// decides which tone the roll's reference density pins, so a per-frame override changes
/// that frame's tonal placement while every other frame keeps the roll's. A raw-JSON probe
/// like [`sets_curve_dmax`] for the same reason — a restating override is still a
/// per-frame assertion.
fn sets_curve_anchor(v: &serde_json::Value) -> bool {
    v.get("reconstruction")
        .and_then(|r| r.get("curve"))
        .and_then(|c| c.get("anchor"))
        .is_some()
}

/// Whether a recipe/override JSON object explicitly carries `output.preset` — the
/// witness behind `roll`'s roll-consistency warning for the output policy. A raw-JSON
/// probe like [`sets_curve_dmax`], not a resolved-value comparison, because an override
/// that *restates* the shared preset is still a per-frame assertion of the output policy
/// and the roll report has no other place to surface it.
fn sets_output_preset(v: &serde_json::Value) -> bool {
    v.get("output").and_then(|o| o.get("preset")).is_some()
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
        ("--sigmoid-mid-fraction", s.sigmoid_mid_fraction.is_some()),
        ("--sigmoid-white-at-d-max", s.sigmoid_white_at_d_max),
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
    if let Some(t) = args.input_opts.film_type {
        cfg.input.film_type = t;
    }
    if let Some(p) = &args.input_opts.export_ir {
        cfg.input.export_ir = Some(p.clone());
    }

    // film base: the three source flags are mutually exclusive (clap-enforced);
    // whichever is given replaces the recipe's source entirely.
    if let Some(src) = film_base_source_override(&args.film_base) {
        cfg.film_base.source = Some(src);
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

            // sigmoid flags ⇒ `reconstruction.curve.{contrast, toe, shoulder,
            // anchor}` — sigmoid only; under exponential they are invalid, not
            // inert.
            let sig = &args.sigmoid;
            let sigmoid_flag = [
                ("--sigmoid-contrast", sig.sigmoid_contrast.is_some()),
                ("--sigmoid-toe", sig.sigmoid_toe.is_some()),
                ("--sigmoid-shoulder", sig.sigmoid_shoulder.is_some()),
                ("--sigmoid-mid-fraction", sig.sigmoid_mid_fraction.is_some()),
                ("--sigmoid-white-at-d-max", sig.sigmoid_white_at_d_max),
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
                    // Anchor placement is one mutually-exclusive rule (like
                    // `dmax` below), so whichever flag is given replaces the
                    // resolved placement entirely rather than editing a field of
                    // it. clap enforces the exclusivity.
                    if let Some(f) = sig.sigmoid_mid_fraction {
                        s.anchor = AnchorPlacement::MidAtDmaxFraction(f);
                    } else if sig.sigmoid_white_at_d_max {
                        s.anchor = AnchorPlacement::WhiteAtDmax;
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
    // `--linear-range LOW,HIGH` ⇒ `print.linear_range`: an atomic pair (both
    // endpoints at once), so it replaces the recipe's pair entirely. Passing the
    // documented default `0,1` is the flags-win reset of a recipe's non-default
    // pair — allowed, and it is what makes a recipe usable under `film-master`.
    if let Some(v) = args.print.linear_range {
        cfg.print.linear_range = v;
    }

    // output preset: an atomic policy choice, so the flag replaces the recipe's
    // preset entirely (including resetting a recipe's `film-master` back to
    // `legacy`). Parsed with the shared `OutputPreset::parse`, so an unknown /
    // not-yet-accepted / renamed name gets the same pinned diagnosis as the recipe
    // key.
    if let Some(name) = &args.output_opts.output_preset {
        cfg.output.preset = OutputPreset::parse(name)?;
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

/// The **complete** `convert` parameter gate: everything [`validate`] checks, plus the
/// one rule that needs the raw flags rather than the resolved config
/// ([`reject_output_sdr_with_named_preset`]).
///
/// `convert` orchestrators must call **this**, not `validate` — a `merge` + `validate`
/// pair silently omits the flag-presence rule and reinstates the bug where
/// `--output-sdr` next to a named preset writes an f32 master. `roll` legitimately
/// calls `validate` directly: it has no output flags at all, so there is nothing for
/// the extra rule to see. `output/presets` must preserve the same rule when it
/// adds roll-aware activation for the remaining named policies.
pub fn validate_convert(cfg: &ResolvedConfig, args: &ConvertArgs) -> Result<()> {
    // Flag-shape first: "these two requests contradict each other" is a clearer
    // diagnosis than whatever value rule the same config might also trip.
    reject_output_sdr_with_named_preset(cfg, args)?;
    // The output path's suffix is likewise a property of *this invocation*, so it
    // outranks `validate`'s value rules — and specifically outranks the
    // missing-base rule, which `validate` deliberately reports last because an
    // omission is the least specific diagnosis available. Without this ordering,
    // `-o out.jpg --output-preset hdr-pq` with no base demands a base first and
    // only then mentions the suffix, making the user fix two things in series.
    reject_output_suffix_mismatch(cfg, args)?;
    validate(cfg)?;
    Ok(())
}

/// The resolved container's suffix rule: the output path is never rewritten, so a
/// mismatch is a usage error naming what the container accepts (design-spec §5).
fn reject_output_suffix_mismatch(cfg: &ResolvedConfig, args: &ConvertArgs) -> Result<()> {
    let Some(extensions) = required_extensions(cfg.output.preset) else {
        return Ok(());
    };
    if extensions.iter().any(|want| {
        args.output
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(want))
    }) {
        return Ok(());
    }
    let list = extensions
        .iter()
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>()
        .join(" or ");
    Err(NcError::Usage(format!(
        "--output-preset {} requires an output path ending in {list}",
        cfg.output.preset.name()
    )))
}

/// Output-path extensions a preset's resolved container accepts, or `None` when
/// the preset imposes no rule (the legacy TIFF path).
///
/// One table so a new container cannot acquire a suffix rule in one place and miss
/// it in another. `output/presets` extends this with the remaining presets and is
/// what makes it roll-aware.
fn required_extensions(preset: OutputPreset) -> Option<&'static [&'static str]> {
    match preset {
        OutputPreset::UltraHdrV1 => Some(&["jpg", "jpeg"]),
        OutputPreset::HdrPq | OutputPreset::HdrHlg => Some(&["avif"]),
        // A TIFF like the legacy path and `film-master` — but unlike them this
        // preset states the rule, because a `.jpg` path under an f32 BT.2020 master
        // is a mistake worth catching at the CLI boundary rather than writing a
        // TIFF with a misleading name.
        OutputPreset::HdrLinearTiff | OutputPreset::HdrPqTiff | OutputPreset::HdrHlgTiff => {
            Some(&["tif", "tiff"])
        }
        // `film-master` is a TIFF like the legacy path; its suffix policy arrives
        // with the rest of the table in `output/presets`.
        OutputPreset::Legacy | OutputPreset::FilmMaster => None,
    }
}

/// How the calling command can state a film base — the one thing the
/// missing-base diagnosis must vary on, because the remedies are disjoint.
///
/// `convert` has all three film-base flags; `roll` has **none** of them
/// (`RollArgs` flattens only `MemoryArgs`/`ReportArgs`), so telling a `roll` user
/// to "pass `--auto-base`" is advice they cannot follow — the flag exits 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilmBaseRemedy {
    /// `convert` — `--film-base` / `--base-region` / `--auto-base`, or the recipe.
    Flags,
    /// `roll` — the shared `--params` recipe only.
    SharedRecipe,
}

impl FilmBaseRemedy {
    /// The remedy available to the command named in a report's `command` field.
    fn for_command(command: &str) -> Self {
        match command {
            "roll" => Self::SharedRecipe,
            _ => Self::Flags,
        }
    }
}

/// The **one** spelling of "no film base was stated", so the two places that can
/// report it ([`validate`] and [`convert_frame`]'s totality guard) cannot drift
/// into two differently-worded diagnoses of the same condition.
///
/// Command-aware by [`FilmBaseRemedy`]: the requirement is identical, but what the
/// user can do about it is not.
pub fn missing_film_base_message(remedy: FilmBaseRemedy) -> String {
    match remedy {
        FilmBaseRemedy::Flags => "no film base selected: pass --film-base R,G,B (a Dmin measured \
             once per roll, e.g. with `nc estimate`), --base-region X,Y,W,H to sample an \
             unexposed border, or --auto-base to detect the rebate band (best-effort: real scans \
             put a thin inset rebate behind the holder, so it can fail). Recipe key: \
             `film_base.source`."
            .to_string(),
        // `roll` deliberately does not repeat the flag names as an option: it has
        // none of them, and the first version of this message sent users to flags
        // that exit 2.
        FilmBaseRemedy::SharedRecipe => "no film base selected: `roll` takes no film-base flags, \
             so set `film_base.source` in the shared --params recipe. Measuring once per roll is \
             the intended workflow: run `nc estimate --base-region X,Y,W,H <reference-scan>` on \
             one frame and paste the reported `film_base` fragment \
             (`\"film_base\": {\"source\": {\"explicit\": [R, G, B]}}`) into the recipe — that is \
             also the only source that keeps every frame on one frozen Dmin. \
             `{\"source\": \"auto\"}` and `{\"source\": {\"region\": [X, Y, W, H]}}` are accepted \
             too, but re-estimate per frame, so the roll is not colour-consistent."
            .to_string(),
    }
}

/// Validate a resolved config at the CLI boundary so the pure stages can trust
/// their inputs. Every failure is a [`NcError::Usage`] (exit 2) — bad recipes and
/// impossible parameters fail loudly, never producing a quietly wrong image.
///
/// **Not the whole `convert` gate.** Every rule here reads only the resolved config, so
/// it is shared verbatim by `convert` and `roll` (and by each `roll` per-frame
/// override). `convert` has one additional rule that inspects flag *presence* and
/// therefore cannot live here; [`validate_convert`] composes the two and is what a
/// `convert` orchestrator must call.
///
/// This spelling reports the missing film base with [`FilmBaseRemedy::Flags`];
/// `roll` calls [`validate_with_remedy`] so its users are pointed at the shared
/// recipe instead of flags `RollArgs` does not accept.
pub fn validate(cfg: &ResolvedConfig) -> Result<()> {
    validate_with_remedy(cfg, FilmBaseRemedy::Flags)
}

/// [`validate`], with the caller stating which remedy its users actually have for
/// an unstated film base. Only the wording of that one diagnosis differs; every
/// rule is identical, which is what keeps `roll` and `convert` on one gate.
pub fn validate_with_remedy(cfg: &ResolvedConfig, remedy: FilmBaseRemedy) -> Result<()> {
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
    // The *unstated* case is deliberately not handled here — it is the last rule in
    // this function. "You have not chosen a film base" is the least specific
    // diagnosis there is, so letting it run first would pre-empt every
    // contradiction rule below (and `reject_roll_unsupported*`) on a config that
    // has both problems, reporting the vaguer one. Flag-shape first.
    match cfg.film_base.source {
        Some(FilmBaseSource::Explicit(b)) => validate_explicit_film_base(&b)?,
        Some(FilmBaseSource::Region([_, _, w, h])) if w == 0 || h == 0 => {
            return Err(usage("--base-region width and height must be > 0".into()));
        }
        Some(FilmBaseSource::Region(_)) | Some(FilmBaseSource::Auto) | None => {}
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
                // Mid-grey's placement fraction: finite and in (0, 1]. At 0 the
                // anchor stops depending on the reference at all (mid-grey pinned
                // at the density origin — the film base — rendering the whole
                // frame above mid-grey); negative pushes it below the base, where
                // no sample exists. Above 1 mid-grey sits *past* the roll's
                // display-white reference, which is not a photographic rendering
                // of anything. F = 1 is the legal edge: mid-grey lands on the
                // reference and white above it.
                if let AnchorPlacement::MidAtDmaxFraction(f) = s.anchor {
                    finite("--sigmoid-mid-fraction", &[f])?;
                    // The placement adds `MID_GREY_OUTPUT_DECADES / contrast` to the
                    // reference, so a positive-but-tiny contrast overflows that quotient to
                    // +inf and the derived anchor is non-finite — which `apply_curve` can
                    // only report at runtime, and which a debug build turned into a panic
                    // (exit 101) rather than a usage error. Reject the slope here, where the
                    // message can name the flag. The bound is far below any photographic
                    // slope (the shipped default is ≈2.07) and exists only to keep the
                    // derivation finite.
                    if !(crate::types::MID_GREY_OUTPUT_DECADES / s.contrast).is_finite() {
                        return Err(usage(format!(
                            "--sigmoid-contrast ({}) is too small to place the mid-grey \
                             anchor: the placement adds {}/contrast to the reference \
                             density, and that quotient overflows to a non-finite anchor. \
                             Use a photographic slope (the default is {:.4}), or \
                             --sigmoid-white-at-d-max, which needs no such division",
                            s.contrast,
                            crate::types::MID_GREY_OUTPUT_DECADES,
                            crate::types::REFERENCE_CONTRAST
                        )));
                    }
                    if f <= 0.0 || f > 1.0 {
                        return Err(usage(format!(
                            "--sigmoid-mid-fraction ({f}) must be in (0, 1] — it \
                             places mid-grey at that fraction of the roll's \
                             reference density (0.5 renders mid-grey halfway up \
                             the roll's range; 1 puts it on the reference itself)"
                        )));
                    }
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

    // Range placement: the endpoints divide the affine, so they must be finite,
    // ordered `lo < hi`, and have a representable span (the same three checks
    // `--balance-range` needs — two individually-finite anchors can still overflow
    // their difference to `+inf`, which would silently collapse every sample).
    let [lo, hi] = cfg.print.linear_range;
    finite("--linear-range", &[lo, hi])?;
    if lo >= hi {
        return Err(usage(format!(
            "--linear-range low ({lo}) must be < high ({hi})"
        )));
    }
    if !(hi - lo).is_finite() {
        return Err(usage(format!(
            "--linear-range span (high {hi} − low {lo}) overflows f32; use endpoints \
             whose difference is representable"
        )));
    }

    validate_output_preset(cfg)?;

    // Last, deliberately: `film_base.source` has no default, and `Dmin` is the
    // divisor of the density conversion, so falling into auto-detection by
    // omission decided the most consequential parameter for the user. `--auto-base`
    // is still one flag away — the requirement is that the choice be *stated*, not
    // that it be explicit.
    //
    // It runs after every value and shape rule because it is the least specific
    // diagnosis in the function: a config that both contradicts itself and states
    // no base should be told about the contradiction, which names the two things
    // the user actually typed.
    if cfg.film_base.source.is_none() {
        return Err(usage(missing_film_base_message(remedy)));
    }

    Ok(())
}

/// The neutral (identity) range placement — the documented `print.linear_range`
/// default. Named here so the "is this at its default?" checks below and the
/// struct default cannot drift.
fn linear_range_is_default(cfg: &ResolvedConfig) -> bool {
    cfg.print.linear_range == PrintParams::default().linear_range
}

/// Output-preset validation (design-spec §5/§9) — the strict, never-silent half of
/// the named-output split.
///
/// **Every rule here is checked on the resolved *value*, and value semantics are
/// the whole rule** — there is deliberately no second check by flag *presence*.
/// A knob is rejected identically whether it came from the recipe, a flag, or a
/// migrated simple-control alias, and a flag that resets a recipe value *back* to
/// its documented default is legitimately accepted under flags-win semantics
/// (that is how a roll recipe carrying print controls is re-exported as a master
/// with `--print-exposure 0`, and it is why `--bigtiff auto` and a recipe
/// `"hdr": false` — which ask for nothing the preset does not already do — are
/// accepted next to a named preset).
///
/// A *general* presence rule would have to be mirrored for recipe keys to behave the
/// same, and mirroring it means probing raw JSON per key (the
/// `LoadedRecipe::curve_dmax_present` machinery) for no gain: the two provenances
/// must be indistinguishable here, and only the resolved value is. The one flag that
/// escapes this reasoning is `--output-sdr`, which has *no* recipe spelling and whose
/// documented meaning a named preset contradicts rather than subsumes; it is rejected
/// by presence in [`reject_output_sdr_with_named_preset`], which explains why.
///
/// Three rules:
///
/// 1. **A named preset is atomic** — it resolves the container, bit depth, and
///    colour profile itself, so a non-default legacy selector is a loud error
///    rather than a silent override. Applies to *every* named preset (gated on
///    [`OutputPreset::is_named`], not on `film-master`), so the next preset cannot
///    silently lose the protection.
/// 2. **`film-master` bypasses every downstream control, so it rejects a
///    non-default one** rather than silently ignoring it, and rejects the two
///    *frame-local measurements* — `auto` `Dmax` and an actually-consulted `auto`
///    `balance_range` — which normalize per frame and break the cross-frame
///    consistency a master exists to preserve.
/// 3. **A non-default `print.linear_range` is display-only.** Every display preset
///    consumes it through the shared display stage (`pipeline::render_split`) —
///    `ultra-hdr-v1`, `hdr-pq`/`hdr-hlg`, `hdr-linear-tiff`, and
///    `hdr-pq-tiff`/`hdr-hlg-tiff` — while the legacy path's frozen ordering does
///    not include it. A knob that legacy would silently ignore is a loud error
///    there. The check below keys on `preset == Legacy` and so is
///    preset-agnostic: a new display preset inherits acceptance automatically, and
///    `film-master` rejects the knob separately under rule 2.
fn validate_output_preset(cfg: &ResolvedConfig) -> Result<()> {
    let usage = NcError::Usage;
    let preset = cfg.output.preset;

    if !linear_range_is_default(cfg) && preset == OutputPreset::Legacy {
        let [lo, hi] = cfg.print.linear_range;
        return Err(usage(format!(
            "--linear-range / print.linear_range ({lo},{hi}) is applied only by the \
             shared display stage of a named display preset. The legacy \
             no-preset TIFF path keeps its frozen ordering and does not apply it, so \
             the value would be silently ignored — it is rejected instead. Leave it at \
             the default `0,1`."
        )));
    }

    // Rule 1 — a named preset is atomic: it resolves the container *format*, depth, and
    // profile itself, so a non-default legacy selector must not be silently overridden.
    // Gated on `is_named()` (not on `FilmMaster`) so the next named preset inherits
    // the protection instead of silently losing it, and the offender is named
    // individually — a message listing all three would let a "did we blame the right
    // selector?" test pass vacuously.
    //
    // "Atomic" is deliberately not total: `bigtiff` stays at its `auto` default, which
    // means the **classic-vs-BigTIFF promotion decision is still delegated** to the
    // size-based `resolve_bigtiff` policy rather than pinned by the preset. That is why
    // `--bigtiff auto` is accepted here and why a master over ~4 GiB legitimately comes
    // out as BigTIFF. Only `--bigtiff on|off`, which would *override* that policy, is
    // the atomicity violation.
    if preset.is_named()
        && let Some((name, value)) = cfg.output.non_default_legacy_selector()
    {
        return Err(usage(format!(
            "--output-preset {} is an atomic output policy — it resolves the container \
             format, bit depth, and colour profile itself (for film-master: an unclamped \
             32-bit float linear ACEScg TIFF with the ACEScg profile) — but {name} is set \
             to a non-default value ({value}). Remove that value — the check runs on the \
             resolved config, so it makes no difference whether it came from the recipe \
             or from a flag — or drop the preset. A value that already equals the \
             documented default (`--bigtiff auto`, a recipe `\"hdr\": false`) asks for \
             nothing the preset does not do and is accepted; `--output-sdr` is the \
             exception, rejected separately because it *forces* 16-bit integer output the \
             preset cannot produce. Note --output-hdr is the *transitional rendered* \
             float TIFF — the print controls have already run — and is never an alias for \
             film-master.",
            preset.name()
        )));
    }

    if preset != OutputPreset::FilmMaster {
        return Ok(());
    }

    // Rule 2a — frame-local auto Dmax. Checked before the control sweep because it
    // is the master-specific reason, not a generic "non-default" complaint.
    if let Reconstruction::Density { curve, .. } = &cfg.reconstruction
        && curve.dmax() == DmaxSource::Auto
    {
        return Err(usage(
            "--output-preset film-master rejects a frame-local auto display-white \
             anchor (--auto-d-max / reconstruction.curve.dmax = \"auto\"): it measures \
             the anchor per frame, which normalizes exposure frame-by-frame and breaks \
             the cross-frame consistency the master exists to preserve. Use the \
             roll-fixed anchor — the default --fixed-d-max, an explicit --d-max <d> \
             measured once with `nc estimate --d-max-region`, or (exponential curve \
             only) --no-d-max for the scene-referred unity placement."
                .into(),
        ));
    }

    // Rule 2b — the *other* frame-local measurement, and the same hazard verbatim:
    // an `auto` regional-balance range measures this frame's 0.5/99.5 corrected-density
    // percentiles (`algo::density::measure_balance_range`), so two frames of one roll
    // get different tone-ramp anchors and their masters are not mutually consistent.
    //
    // Rejected only when the range is genuinely consulted: `regional_balance`
    // short-circuits before measuring whenever the two balances are equal — including
    // the neutral default — so the default `BalanceRange::Auto` is inert and must stay
    // accepted. `density::consults_balance_range` is that predicate, kept beside the
    // short-circuits it mirrors so the two cannot drift.
    if let Reconstruction::Density { density, .. } = &cfg.reconstruction
        && density.balance_range == BalanceRange::Auto
        && crate::algo::density::consults_balance_range(density)
    {
        return Err(usage(format!(
            "--output-preset film-master rejects a frame-local auto regional-balance \
             range (--auto-balance-range / reconstruction.density.balance_range = \
             \"auto\") when a balance is actually applied (shadow_balance {:?} vs \
             highlight_balance {:?}): the ramp anchors are measured from this frame's \
             density percentiles, so two frames of a roll would be corrected against \
             different anchors and their masters would not be mutually consistent — the \
             same reason the master rejects auto Dmax. Measure the range once with `nc \
             convert` on a representative frame and reuse it via --balance-range LO,HI, \
             or leave the balances equal (an equal pair is a tone-independent offset and \
             consults no range).",
            density.shadow_balance, density.highlight_balance
        )));
    }

    // Rule 2c — every non-default downstream control, named individually so the
    // error says which one and where it came from. `film-master` encodes stage 4
    // directly, so each of these would otherwise be silently dropped.
    let d = PrintParams::default();
    // Destructured, not field-accessed: adding a print control makes this binding
    // fail to compile, forcing the author to decide whether `film-master` bypasses
    // it. A field-access sweep would silently omit the new knob and reintroduce
    // exactly the silent-ignore this rule exists to prevent.
    let PrintParams {
        print_exposure,
        black_point,
        white_balance,
        highlight_compress,
        linear_range,
    } = &cfg.print;
    let offender = [
        (
            "--print-exposure / print.print_exposure",
            *print_exposure != d.print_exposure,
            format!("{print_exposure}"),
        ),
        (
            "--black-point / print.black_point",
            *black_point != d.black_point,
            format!("{black_point}"),
        ),
        (
            "--white-balance / --auto-wb / print.white_balance",
            *white_balance != d.white_balance,
            format!("{white_balance:?}"),
        ),
        (
            "--highlight-compress / print.highlight_compress",
            *highlight_compress != d.highlight_compress,
            format!("{highlight_compress}"),
        ),
        (
            "--linear-range / print.linear_range",
            *linear_range != d.linear_range,
            format!("{linear_range:?}"),
        ),
    ]
    .into_iter()
    .find_map(|(name, non_default, value)| non_default.then_some((name, value)));
    if let Some((name, value)) = offender {
        return Err(usage(format!(
            "--output-preset film-master bypasses all print and display controls, but \
             {name} is set to a non-default value ({value}). The master is the \
             unclamped linear ACEScg film rendering — a linear export that also wants \
             a creative / print / display adjustment is the `custom` workflow (not \
             accepted by this build yet; owned by `output/presets`). Reset the control \
             to its default (a flag may reset a recipe value), or drop the preset. \
             There is no ignore-conflicting-controls mode."
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Serialize a value as pretty JSON to a file; an I/O failure is a write error.
///
/// Staged and committed immediately, so a failure mid-write cannot leave a truncated
/// document at `path` — but *not* held back to join a conversion's artifact set
/// (`io/transactional-output-writes`). Both callers are deliberately outside it:
/// `--dump-params` is written before anything is decoded, and `--report-file` must
/// land even when `--strict` subsequently fails the run — and in `roll` it is a
/// roll-level artifact that no single frame's set could hold.
fn write_json<T: Serialize>(path: &Path, value: &T, log: &Log) -> Result<()> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| NcError::Other(format!("serializing JSON: {e}")))?;
    // Promotion notes (currently: a hard-linked target whose aliases keep the old bytes)
    // go to stderr here rather than into `report.warnings`. These are *operational*
    // artifacts — `--dump-params`, `--report-file` — and folding them into the conversion's
    // warning set would let a hard-linked report file fail a `--strict` render, which is not
    // what `--strict` is about.
    //
    // `warn_always`, not `warn`: kept out of the JSON report *and* quiet-gated would mean
    // no channel carries it under `--quiet`, leaving the stranded alias entirely silent —
    // exactly the defect this reporting exists to close. That combination is what
    // `warn_always` is for (see its doc comment; fail-soft telemetry uses it for the same
    // reason).
    for note in staged::stage_bytes(path, json.as_bytes())?.commit()? {
        log.warn_always(&note);
    }
    Ok(())
}

/// Emit a report as JSON to stdout (kept clean) or `--report-file`. `none`
/// suppresses it entirely.
fn emit_report(
    report: &Report,
    format: ReportFormat,
    file: Option<&Path>,
    log: &Log,
) -> Result<()> {
    emit_json(report, format, file, log)
}

/// Emit any serializable report as JSON to stdout (kept clean) or a file. `none`
/// suppresses it entirely. Shared by the per-command [`Report`] and the roll-level
/// [`RollReport`].
fn emit_json<T: Serialize>(
    value: &T,
    format: ReportFormat,
    file: Option<&Path>,
    log: &Log,
) -> Result<()> {
    if format == ReportFormat::None {
        return Ok(());
    }
    match file {
        Some(p) => write_json(p, value, log),
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
/// emit actionable guidance — not as aliases *in this build*; design-spec §7.1/§9
/// tie alias activation to the complete `output/presets` migration (see the
/// comment on the simple controls below). `reject_legacy_recipe_keys` is
/// the recipe-side mirror.
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
    // The removed simple-reconstruction controls. Their replacement print controls
    // now *exist* (`print.white_balance` shipped with auto-WB; `print.linear_range`
    // shipped with the shared display stage), so the migration error names the
    // concrete replacement flag instead of promising a future one.
    //
    // They stay **rejections in this build**. Design-spec §7.1/§9 do specify them as
    // *warned aliases* — but only under the complete `output/presets` migration.
    // `ultra-hdr-v1` now consumes both replacements; deferral instead keeps help,
    // warnings, recipe provenance, roll handling, and the version boundary atomic.
    // `output/presets` owns the switch to alias behaviour; when it lands, the alias must still warn that it
    // preserves the requested *numbers* and not the legacy pixels (per-channel gains do
    // not commute with the working-space matrix).
    for (flag, present, replacement) in [
        (
            "--invert-white-balance",
            args.simple.invert_white_balance.is_some(),
            "--white-balance R,G,B (recipe `print.white_balance = {\"explicit\": [r, g, b]}`)",
        ),
        (
            "--clip-low",
            args.simple.clip_low.is_some(),
            "--linear-range LOW,HIGH (recipe `print.linear_range`) — an atomic pair, \
             so pass both endpoints",
        ),
        (
            "--clip-high",
            args.simple.clip_high.is_some(),
            "--linear-range LOW,HIGH (recipe `print.linear_range`) — an atomic pair, \
             so pass both endpoints",
        ),
    ] {
        if present {
            return Err(NcError::Usage(format!(
                "{flag} was removed: it is not a simple-reconstruction parameter — \
                 simple reconstruction ends at the direct unclamped positive \
                 `1 - scan/Dmin` (design-spec §7.1). It is a print control that now \
                 runs *after* the NC film RGB v1 working-space mapping, so use \
                 {replacement}. The value carries over; the pixels do not — \
                 per-channel gains and an affine range placement do not commute with \
                 the working-space matrix, so the result is not bit-identical to the \
                 pre-mapping behaviour."
            )));
        }
    }
    Ok(())
}

/// `--output-sdr` next to a **named** output preset is a contradictory request, and
/// the one deliberate exception to [`validate_output_preset`]'s resolved-value rule:
/// it is checked by flag **presence**.
///
/// Why this flag and not `--bigtiff auto` or a recipe `"hdr": false`:
///
/// - **Its documented meaning is contradicted, not merely redundant.** design-spec §9
///   defines `--output-sdr` as "*force* the default 16-bit integer output". A named
///   preset does not write 16-bit integer output, so honouring the preset silently
///   discards an explicit request. `--bigtiff auto` means "decide for me" and a recipe
///   `"hdr": false` asserts nothing at all — those two genuinely ask for nothing the
///   preset does not already do, so they stay accepted.
/// - **There is no recipe spelling to mirror.** The recipe carries only `hdr: bool`,
///   and `false` is the `#[serde(default)]` — indistinguishable from omission, so it
///   can never encode this request. That is what makes a presence check cheap and
///   provenance-symmetric here: one field read, no raw-JSON probing of the kind
///   [`LoadedRecipe`]`::curve_dmax_present` needs, and no recipe form left behaving
///   differently.
///
/// A hard error (exit 2), not a warning: the user asked for a container the preset
/// cannot produce, and resolving that silently is exactly what "fail loudly" forbids.
/// Note `--output-hdr` needs no entry here — it resolves a *non-default* `output.hdr`,
/// so the value rule already rejects it from either provenance.
fn reject_output_sdr_with_named_preset(cfg: &ResolvedConfig, args: &ConvertArgs) -> Result<()> {
    if args.output_opts.output_sdr && cfg.output.preset.is_named() {
        return Err(NcError::Usage(format!(
            "--output-sdr forces the default 16-bit integer TIFF, but --output-preset {} \
             resolves its own container format, bit depth, and colour profile (film-master, \
             for example, is an unclamped 32-bit float linear ACEScg TIFF), so the two \
             requests contradict each other and nc will not silently honour one of them. \
             Drop --output-sdr, or drop the preset. (This one is checked by flag presence \
             rather than resolved value, because `--output-sdr` has no recipe spelling: \
             `output.hdr = false` is the default and asserts nothing, so there is no \
             recipe form of the request to reject in step with it.)",
            cfg.output.preset.name()
        )));
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

/// Run the memory preflight for one input and fold its outcome into the run:
/// probe the file's shape from headers alone, size the run for `profile` +
/// `sampling`, reject loudly when it would exceed `budget` (exit 6), and push the
/// RAM-pressure warning when it fits the budget but not the machine.
///
/// Shared by `convert`/`roll` and by `inspect`/`estimate` so the four commands
/// gate identically — each with its own profile, since `inspect`/`estimate` stop
/// after decode and must not be judged on a render they never run, and its own
/// [`SamplePlan`], since the film-base phase's cost depends on which rectangles the
/// run samples.
///
/// `total_ram` is a parameter rather than a `detect_total_ram()` call inside, so
/// the warn tier — the one piece of this gate that is environment-dependent, and
/// therefore the one that can make `--strict` exit differently on two machines — is
/// reachable from a test through the real wiring. Production callers pass
/// [`memory::detect_total_ram`].
fn preflight_memory(
    input: &Path,
    profile: RunProfile,
    sampling: SamplePlan,
    budget: memory::Budget,
    total_ram: Option<u64>,
    log: &Log,
    warnings: &mut Vec<String>,
) -> Result<MemoryReport> {
    let shape = probe(input)?;
    let mem = memory::preflight(&shape, profile, sampling, budget, total_ram)?;
    log.info(format_args!(
        "memory preflight: estimated peak {} bytes, budget {} bytes ({:?})",
        mem.estimate.estimated_peak_bytes, mem.budget_bytes, mem.budget_source
    ));
    if let Some(msg) = memory::warn_message(&mem) {
        push_warning_buf(warnings, log, msg);
    }
    Ok(mem)
}

/// The film-base sampling a resolved [`FilmBaseSource`] will perform, for the
/// memory model's film-base phase: an explicit base reads no pixels, a region
/// materializes exactly its rectangle, and `auto` materializes the frame interior
/// (`film_base::auto_interior_pixels`, resolved inside the model against the probed
/// shape). `estimate` adds its `--grid` / `--d-max-region` rectangles on top.
fn sample_plan(source: &FilmBaseSource) -> SamplePlan {
    match source {
        FilmBaseSource::Explicit(_) => SamplePlan::none(),
        FilmBaseSource::Region([_, _, w, h]) => SamplePlan::rect(*w as u64 * *h as u64),
        FilmBaseSource::Auto => SamplePlan::auto(),
    }
}

/// The per-frame conversion core: **stage-0 memory preflight** → decode →
/// film-base estimate → render → optional IR export → encode + effective-recipe
/// sidecar. Pure of the operational concerns the callers layer on top (`--strict`
/// gating, report emission, telemetry), so `convert` and `roll` share one
/// byte-for-byte identical frame path.
///
/// The one operational concern it *does* own is the memory gate
/// ([`preflight_memory`]), and deliberately: it must run per frame and
/// immediately before this function's own `decode_within`, which needs the same
/// budget anyway. Do not "tidy" it up into the orchestrators — that would split
/// the run's validation across two layers and leave the budget threaded here
/// regardless.
///
/// The caller must have already validated `cfg` ([`validate`]), rejected the
/// deprecated input flags ([`reject_deprecated_input_flags`]), and checked
/// write-target collisions; apart from the memory gate above, `convert_frame`
/// assumes a sound config and a safe `output` path. It resolves and gates the input color semantics itself
/// (transfer + meaning, [`input_semantics`]) after decode, before the render. It
/// never
/// writes to stdout (the report rides back in [`ConvertedFrame`]); progress and
/// warnings go to stderr via `log`.
///
/// Warnings are accumulated into the caller-owned `warnings` buffer (echoed to
/// stderr as they occur) so they survive an early failure: on success they are
/// also moved into the returned report, but on the `Err` path they stay in the
/// caller's buffer — the roll orchestrator attaches them to a failed frame's
/// report. The caller decides whether `--strict` promotes them. `memory_out`
/// carries the preflight's decision back the same way and for the same reason: a
/// frame that passed the gate and then failed later is exactly where a reader wants
/// the estimate, so it must not be lost with the returned report.
// Two over clippy's argument cap: the orchestration core legitimately threads the
// frame identity, the config, the run's memory budget, and the two
// report-provenance out-params; a struct wrapping a handful of one-off values
// would only obscure the call sites.
enum FrameRender {
    Tiff(stages::Rendered),
    UltraHdr {
        render: Box<gain_map::GainMapRender>,
        convert: stages::ConvertReport,
        timings: stages::StageTimings,
    },
    HdrAvif {
        render: Box<hdr::RenderedHdr>,
        convert: stages::ConvertReport,
        timings: stages::StageTimings,
    },
    /// `hdr-linear-tiff`: the pre-transfer BT.2020 rendition, written verbatim.
    HdrLinearTiff {
        render: Box<hdr::LinearBt2020Hdr>,
        convert: stages::ConvertReport,
        timings: stages::StageTimings,
    },
    /// `hdr-pq-tiff` / `hdr-hlg-tiff`: the same rendition `HdrAvif` carries, coded
    /// as 16-bit TIFF instead.
    HdrCodedTiff {
        render: Box<hdr::RenderedHdr>,
        convert: stages::ConvertReport,
        timings: stages::StageTimings,
    },
}

impl FrameRender {
    fn convert(&self) -> stages::ConvertReport {
        match self {
            Self::Tiff(rendered) => rendered.convert,
            Self::UltraHdr { convert, .. }
            | Self::HdrAvif { convert, .. }
            | Self::HdrLinearTiff { convert, .. }
            | Self::HdrCodedTiff { convert, .. } => *convert,
        }
    }

    fn timings(&self) -> stages::StageTimings {
        match self {
            Self::Tiff(rendered) => rendered.timings,
            Self::UltraHdr { timings, .. }
            | Self::HdrAvif { timings, .. }
            | Self::HdrLinearTiff { timings, .. }
            | Self::HdrCodedTiff { timings, .. } => *timings,
        }
    }

    /// The measured content light of a **single-rendition HDR** render, for the
    /// SDR-range check ([`hdr::sdr_range_warning`]).
    ///
    /// `None` for the two branches the check does not apply to, for different
    /// reasons: `Tiff` renders no HDR signal at all, and `UltraHdr` is dual-rendition
    /// — its deliverable is an SDR base image plus a gain map, so an HDR rendition
    /// that stays near reference white produces an inert gain map rather than a
    /// mislabelled container. That is a real observation (`GainMapMax` measures
    /// ≈1.0027x today) but a different warning about a different artifact, and it
    /// belongs with the gain-map stage that can measure it.
    fn hdr_content_light(&self) -> Option<hdr::ContentLightLevel> {
        match self {
            Self::HdrAvif { render, .. } | Self::HdrCodedTiff { render, .. } => {
                Some(render.metadata().content_light)
            }
            Self::HdrLinearTiff { render, .. } => Some(render.content_light()),
            Self::Tiff(_) | Self::UltraHdr { .. } => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn convert_frame(
    command: &'static str,
    input: &Path,
    output: &Path,
    cfg: &ResolvedConfig,
    input_from_cli: InputFromCli,
    dmax_setting: DmaxSetting,
    budget: memory::Budget,
    memory_out: &mut Option<MemoryReport>,
    log: &Log,
    warnings: &mut Vec<String>,
) -> Result<ConvertedFrame> {
    // The canonical resolved-recipe JSON, resolved up front: it is both the
    // sidecar's `params` body and the input to the identity `params_hash`, so the
    // hash a report advertises is provably the hash of the recipe that ran.
    let recipe_json = canonical_params_json(cfg)?;
    let identity = Identity::with_params_hash(version::stable_hash(&recipe_json));

    // `film_base.source` has no default, and the gate rejects `None` before any
    // frame runs (`validate_convert` for `convert`, `validate_with_remedy` directly
    // for each `roll` frame). Restating it here keeps this function total rather
    // than relying on an `unwrap` whose safety lives in another module — sharing
    // `missing_film_base_message` so this unreachable spelling cannot drift into a
    // second, thinner diagnosis of the same condition.
    let base_source = cfg.film_base.source.clone().ok_or_else(|| {
        NcError::Usage(missing_film_base_message(FilmBaseRemedy::for_command(
            command,
        )))
    })?;

    let mut report = Report {
        command: Some(command),
        identity: Some(identity.clone()),
        input: Some(input.to_path_buf()),
        output: Some(output.to_path_buf()),
        // The effective recipe (the sidecar's exact object), so
        // `recipe.reconstruction` is the tagged reconstruction schema.
        recipe: Some(cfg.clone()),
        film_base_source: Some(base_source.clone()),
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

    // Stage 0 — memory preflight, on a metadata-only header probe. This must run
    // *before* decode allocates: the whole point is to reject an oversized frame
    // while the heap is still empty (a check after decode would OOM on exactly the
    // inputs it exists to catch). Over budget ⇒ loud exit 6; within budget but
    // most of the machine's RAM ⇒ a `--strict`-promotable warning. Operational
    // gate — it never touches a pixel, so the output stays deterministic.
    let export_ir_planned = cfg.input.export_ir.is_some();
    let mem = preflight_memory(
        input,
        match cfg.output.preset {
            OutputPreset::UltraHdrV1 => RunProfile::UltraHdrV1 {
                export_ir: export_ir_planned,
            },
            OutputPreset::HdrPq | OutputPreset::HdrHlg => RunProfile::HdrAvif {
                export_ir: export_ir_planned,
            },
            OutputPreset::HdrLinearTiff => RunProfile::HdrLinearTiff {
                export_ir: export_ir_planned,
            },
            OutputPreset::HdrPqTiff | OutputPreset::HdrHlgTiff => RunProfile::HdrCodedTiff {
                export_ir: export_ir_planned,
            },
            OutputPreset::Legacy | OutputPreset::FilmMaster => RunProfile::Convert {
                depth: cfg.output.depth(),
                export_ir: export_ir_planned,
            },
        },
        sample_plan(&base_source),
        budget,
        memory::detect_total_ram(),
        log,
        warnings,
    )?;
    // Out-param first, so the diagnostic survives a later failure on this frame:
    // the roll orchestrator attaches it to the frame report either way (a frame that
    // passed the gate and then failed is exactly where a reader wants the estimate).
    *memory_out = Some(mem);
    report.memory = Some(mem);

    // Stage 1 — decode. Per-stage wall clocks feed the telemetry record only
    // (they never touch the image/sidecar); measure them regardless of whether
    // telemetry is enabled so the render path is uniform.
    let stage_started = Instant::now();
    let (image, info) = decode_within(input, budget.bytes())?;
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
    // Whether film-base estimation will actually consume the IR plane for holder
    // detection: only under the chromogenic gate, on a scan that carries a
    // **marker-verified** IR plane, and only when the base is being auto-detected
    // (an explicit `--film-base` / `--base-region` runs no detection at all). A
    // shape-only IR plane (unverified provenance) is not trusted, so it degrades to
    // RGB-only. Governs the "IR carried but unused" note (false when we consume it)
    // and the chromogenic-without-IR note below.
    let auto_base = matches!(base_source, FilmBaseSource::Auto);
    let chromogenic = cfg.input.film_type.ir_transparent();
    let ir_shape_only = info.ir_present && !image.ir_verified;
    let ir_used_for_holder = chromogenic && info.ir_present && image.ir_verified && auto_base;

    // When the auto chromogenic path wanted the IR mask but the plane is shape-only,
    // it silently degraded to RGB-only — say so (a `--strict`-promotable warning,
    // like the no-IR note). Emitting it here means the generic "carried but unused"
    // note below is skipped for the same plane, so only one IR note fires.
    let shape_only_holder_note = auto_base && chromogenic && ir_shape_only;
    if shape_only_holder_note {
        push_warning_buf(
            warnings,
            log,
            "--film-type chromogenic declared and an IR plane is present, but it is \
             identified by shape alone (no NewSubfileType=4 marker) and not trusted \
             for holder detection; using RGB-only film-holder detection for the film base"
                .into(),
        );
    }

    // Note an IR plane that's carried but not consumed — but not when it's being
    // exported (`--export-ir` is the user handling it, so warning — and failing
    // under `--strict` — would be wrong; keeps `--strict --export-ir` usable on the
    // primary HDRi format), not when the chromogenic film-base path is consuming it
    // for holder detection (the "not used in Step 1" claim is then false), and not
    // when the shape-only note above already covered this plane.
    if info.ir_present && export_ir.is_none() && !ir_used_for_holder && !shape_only_holder_note {
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
    // A `--film-type chromogenic` declaration only does anything when the auto base
    // detector runs on a scan that actually carries an IR plane; on an HDR 48-bit
    // scan there is nothing to mask with. Note only the case where detection *runs*
    // but has no IR plane (auto base) — an explicit `--film-base`/`--base-region`
    // takes no detection path, so warning there would fail a valid `--strict` run
    // over a path it never exercises.
    if auto_base && cfg.input.film_type == FilmType::Chromogenic && !info.ir_present {
        push_warning_buf(
            warnings,
            log,
            "--film-type chromogenic declared but the scan has no IR plane; \
             using RGB-only film-holder detection for the film base"
                .into(),
        );
    }

    let stage_started = Instant::now();
    let base = film_base::estimate(&image, &base_source, cfg.input.film_type)?;
    let film_base_ms = elapsed_ms(stage_started);
    report.film_base = Some(base.base);
    for w in base.warnings {
        push_warning_buf(warnings, log, w);
    }

    // Clear any stale lcms2 flag so only errors from *this* render are counted.
    let _ = cms_error_occurred();
    // Stages 3–4 — reconstruction → legacy print → output color transform.
    // Exhaustive on the preset, deliberately: the *container* is what selects the
    // branch, and two presets can share a transfer (`hdr-pq` and `hdr-pq-tiff` render
    // an identical rendition into different containers). An `if let Some(transfer) =
    // transfer_for(..)` chain would silently hand the TIFF presets to the AVIF
    // encoder, so the compiler is made to enumerate the cases instead.
    let rendered = match cfg.output.preset {
        OutputPreset::UltraHdrV1 => {
            let source =
                stages::render_display_source(&image, &base.base, &cfg.reconstruction, &cfg.print)?;
            let convert = source.convert;
            let mut timings = source.timings;
            let display_started = Instant::now();
            let render = gain_map::render(
                &source.shared,
                gain_map::GainMapConfig::ultra_hdr_v1(cfg.print.highlight_compress),
            )?;
            // Both independent SDR/HDR display renders plus the common-domain gain
            // construction are color work. Keep them out of encode_ms so stage
            // totals account for every pixel operation even when telemetry is off.
            timings.color_ms += elapsed_ms(display_started);
            FrameRender::UltraHdr {
                render: Box::new(render),
                convert,
                timings,
            }
        }
        OutputPreset::HdrLinearTiff => {
            // The same shared display source as every other display preset, stopped
            // one stage earlier — `render_linear` without `encode_transfer`, so the
            // samples stay display-linear BT.2020 and no transfer is ever applied.
            let source =
                stages::render_display_source(&image, &base.base, &cfg.reconstruction, &cfg.print)?;
            let convert = source.convert;
            let mut timings = source.timings;
            let display_started = Instant::now();
            let render = hdr::render_linear(&source.shared, cfg.print.highlight_compress)?;
            // The linear display render is colour work, like the PQ/HLG and gain-map
            // branches. Only the TIFF write belongs to encode_ms.
            timings.color_ms += elapsed_ms(display_started);
            FrameRender::HdrLinearTiff {
                render: Box::new(render),
                convert,
                timings,
            }
        }
        OutputPreset::HdrPq
        | OutputPreset::HdrHlg
        | OutputPreset::HdrPqTiff
        | OutputPreset::HdrHlgTiff => {
            // One rendition off the same shared display source the gain-map path
            // uses, so every display preset consumes an identically resolved
            // reconstruction and print stage. The transfer comes from the preset;
            // only the container below differs.
            let transfer = hdr::transfer_for(cfg.output.preset).ok_or_else(|| {
                NcError::Other(format!(
                    "`{}` reached the HDR render branch without a Rec.2100 transfer",
                    cfg.output.preset.name()
                ))
            })?;
            let source =
                stages::render_display_source(&image, &base.base, &cfg.reconstruction, &cfg.print)?;
            let convert = source.convert;
            let mut timings = source.timings;
            let display_started = Instant::now();
            let render = hdr::render(&source.shared, transfer, cfg.print.highlight_compress)?;
            // The display render and its PQ/HLG transfer are colour work, like the
            // gain-map branch above — coding alone belongs to encode_ms.
            timings.color_ms += elapsed_ms(display_started);
            let render = Box::new(render);
            // Exhaustive, like the outer match and for the same reason: a `_` arm
            // here would hand a future coded preset the AVIF container silently.
            // `unreachable` is not used — the outer arm's pattern is the only way in,
            // but stating the remaining presets keeps the compiler as the guard.
            match cfg.output.preset {
                OutputPreset::HdrPqTiff | OutputPreset::HdrHlgTiff => FrameRender::HdrCodedTiff {
                    render,
                    convert,
                    timings,
                },
                OutputPreset::HdrPq | OutputPreset::HdrHlg => FrameRender::HdrAvif {
                    render,
                    convert,
                    timings,
                },
                OutputPreset::Legacy
                | OutputPreset::FilmMaster
                | OutputPreset::UltraHdrV1
                | OutputPreset::HdrLinearTiff => {
                    return Err(NcError::Other(format!(
                        "`{}` reached the shared Rec.2100 render arm, which only the \
                         AVIF and coded-TIFF presets may enter",
                        cfg.output.preset.name()
                    )));
                }
            }
        }
        OutputPreset::Legacy | OutputPreset::FilmMaster => FrameRender::Tiff(stages::render(
            &image,
            &base.base,
            &cfg.reconstruction,
            &cfg.print,
            &cfg.output,
        )?),
    };
    // lcms2 transform/profile failures reach us only through the global handler
    // (`transform_in_place` is infallible), so check the flag it sets.
    if cms_error_occurred() {
        return Err(NcError::Other(
            "color management (lcms2) reported a runtime error; see stderr".into(),
        ));
    }
    let convert = rendered.convert();
    report.dmax = convert.dmax;
    report.white_balance = convert.white_balance;
    report.balance_range = convert.balance_range;
    report.reconstruction_result = Some(reconstruction_result(
        &cfg.reconstruction,
        convert.dmax,
        convert.curve_anchor,
        dmax_setting,
    ));
    // Stamp the pinned working-space interpretation (design-spec §8). NC film RGB
    // v1 is the fixed rule "reconstructed film RGB is linear Rec.709/D65", applied
    // on every path (`pipeline::working_space::WORKING_MAPPING_ID`); the typed
    // ACEScg mapper realizes it for the named presets that consume `AcesCgImage`.
    report.working_mapping = Some(working_space::WORKING_MAPPING_ID);
    // Which branch ran out of that boundary, and what it applied — so a
    // `film-master` consumer can see "no print controls, no display render,
    // unclamped linear ACEScg" without inferring it from the recipe.
    report.output_render = Some(output_render_result(cfg));

    // Report an `auto` BigTIFF promotion (an automatic decision the user didn't
    // explicitly request).
    if cfg.output.bigtiff == BigTiff::Auto
        && let FrameRender::Tiff(rendered) = &rendered
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
    // Staged artifacts, committed together after every fallible step succeeds. The
    // set is IR + primary + sidecar: all three belong to one conversion, and it is
    // the orphaned-primary case (`encode` ok → sidecar fails) that motivated this.
    // `--report-file` / `--dump-params` are staged individually elsewhere: the report
    // must land even when `--strict` then fails the run, and in `roll` it is a
    // roll-level artifact that no single frame's set could hold.
    let mut pending: Vec<staged::Staged> = Vec::new();
    let mut ir_export_ms = None;
    if let Some(path) = &export_ir {
        let stage_started = Instant::now();
        // Use the preset's resolved `depth()` for the IR TIFF. u16 for the legacy
        // default, `ultra-hdr-v1` (fixed 8-bit JPEG primary), `hdr-pq`/`hdr-hlg`
        // (10-bit AVIF primary) and `hdr-pq-tiff`/`hdr-hlg-tiff` (whose primary is
        // itself u16); f32 for legacy `--output-hdr`, `film-master` and
        // `hdr-linear-tiff` — the last two resolve f32 without touching
        // `output.hdr`, and `hdr-linear-tiff`'s f32 IR is why
        // `RunProfile::HdrLinearTiff` charges nothing for the export. The IR
        // *samples* are unchanged either way — the plane is carried, never converted —
        // so the only difference is quantization headroom. Documented in design-spec §9.
        pending.push(encode::export_ir(&image, cfg.output.depth(), path)?);
        ir_export_ms = Some(elapsed_ms(stage_started));
        report.ir_exported = Some(path.clone());
    }

    // An HDR container whose signal never rises above SDR reference white is a
    // wrapper around an SDR picture — reported loudly (and `--strict`-promotable)
    // rather than left to be discovered by inspecting the file's `clli` box. Checked
    // here, before the encode consumes the render, because the measurement rides on
    // the rendition and only two of the four HDR arms surface it in their summary.
    if let Some(content_light) = rendered.hdr_content_light()
        && let Some(message) = hdr::sdr_range_warning(content_light)
    {
        push_warning_buf(warnings, log, message);
    }

    // Stage 5 — encode + effective-recipe sidecar.
    let stage_started = Instant::now();
    let render_timings = rendered.timings();
    let mut avif_summary = None;
    let mut hdr_tiff_summary = None;
    let mut hdr_coded_summary = None;
    let (primary, outcome) = match rendered {
        FrameRender::Tiff(rendered) => {
            encode::encode(&rendered.image, &cfg.output, Some(&rendered.icc), output)?
        }
        FrameRender::UltraHdr { render, .. } => ultra_hdr::encode(*render, output)?,
        FrameRender::HdrAvif { render, .. } => {
            let (staged, outcome, summary) = avif::encode(*render, output)?;
            avif_summary = Some(summary);
            (staged, outcome)
        }
        FrameRender::HdrCodedTiff { render, .. } => {
            // Keyed off the transfer the render *actually applied*, not off the
            // preset: that is the value the stored code values were produced with
            // (and the one `pixel_contract` is derived from), so the profile and the
            // codes cannot disagree. Matching on the preset here would let a future
            // coded preset silently inherit the PQ profile.
            let icc = match render.metadata().transfer {
                hdr::HdrTransfer::Pq => color::hdr_pq_tiff_icc()?,
                hdr::HdrTransfer::Hlg => color::hdr_hlg_tiff_icc()?,
            };
            let (staged, outcome, summary) =
                encode::encode_hdr_coded(*render, &cfg.output, &icc, output)?;
            hdr_coded_summary = Some(Box::new(summary));
            (staged, outcome)
        }
        FrameRender::HdrLinearTiff { render, .. } => {
            // The profile is resolved here, not inside the encoder, so the embedded
            // blob is provably the one the orchestrator chose — the same rule the
            // legacy arm follows with `rendered.icc`.
            let icc = color::hdr_linear_bt2020_icc()?;
            let (staged, outcome, summary) =
                encode::encode_hdr_linear(*render, &cfg.output, &icc, output)?;
            hdr_tiff_summary = Some(summary);
            (staged, outcome)
        }
    };
    if cms_error_occurred() {
        return Err(NcError::Other(
            "color management (lcms2) reported a runtime error; see stderr".into(),
        ));
    }
    let encode_ms = elapsed_ms(stage_started);
    if let Some(summary) = avif_summary {
        // A general-brand-only file is valid but is never advertised as Advanced
        // Profile, and the downgrade is surfaced (and `--strict`-promotable) rather
        // than left for someone to discover by inspecting brands.
        let profile_reason = match &summary.profile {
            avif::AvifProfile::Advanced => None,
            avif::AvifProfile::GeneralOnly { reason } => {
                push_warning_buf(
                    warnings,
                    log,
                    format!(
                        "AVIF written without the MA1A brand (not AVIF v1.2 Advanced \
                         Profile): {reason}"
                    ),
                );
                Some(reason.clone())
            }
        };
        report.avif = Some(AvifResult {
            profile: match summary.profile {
                avif::AvifProfile::Advanced => "advanced",
                avif::AvifProfile::GeneralOnly { .. } => "general-brand-only",
            },
            profile_reason,
            bit_depth: summary.bit_depth,
            seq_profile: summary.seq_profile,
            seq_level_idx: summary.seq_level_idx,
            level: avif::level_name(summary.seq_level_idx),
            cicp: [summary.cicp.0, summary.cicp.1, summary.cicp.2],
            full_range: summary.full_range,
            codestream_bytes: summary.codestream_bytes,
        });
    }
    if let Some(summary) = hdr_coded_summary {
        // Same reason as the linear TIFF below: the profile is built inside the
        // encode arm, so the `auto` BigTIFF promotion is reported from what the
        // encoder resolved rather than predicted before it.
        if cfg.output.bigtiff == BigTiff::Auto && summary.bigtiff {
            push_warning_buf(
                warnings,
                log,
                "output promoted to BigTIFF (would exceed the classic 4 GiB TIFF limit)".into(),
            );
        }
        let metadata = summary.metadata;
        report.hdr_coded_tiff = Some(HdrCodedTiffResult {
            pixel_contract: summary.pixel_contract,
            bits_per_sample: summary.bits_per_sample,
            sample_format: summary.sample_format,
            bigtiff: summary.bigtiff,
            icc_bytes: summary.icc_bytes,
            // Deliberately **not** `metadata.cicp_matrix_coefficients`: that is the
            // AVIF value (9, Y'CbCr). An RGB ICC profile requires 0, and the profile
            // this file embeds writes 0 — so the report states what the artifact
            // carries, not what the renderer declared for a different container.
            cicp: [metadata.cicp_color_primaries, metadata.cicp_transfer, 0],
            full_range: metadata.full_range,
            max_quantization_error_codes: summary.max_quantization_error_codes,
            rms_quantization_error_codes: summary.rms_quantization_error_codes,
            reference_white_nits: metadata.linear.reference_white_nits,
            target_peak_nits: metadata.linear.target_peak_nits,
            // PQ only, for the same reason `io::avif` omits `clli` on HLG: HLG is
            // display-referred, so absolute content-light values would be a false
            // claim rather than a missing one.
            max_cll_nits: match metadata.transfer {
                hdr::HdrTransfer::Pq => Some(metadata.content_light.max_cll_nits),
                hdr::HdrTransfer::Hlg => None,
            },
            max_fall_nits: match metadata.transfer {
                hdr::HdrTransfer::Pq => Some(metadata.content_light.max_fall_nits),
                hdr::HdrTransfer::Hlg => None,
            },
            hlg_system_gamma: metadata.hlg_system_gamma,
            hlg_reference_display_peak_nits: metadata.hlg_reference_display_peak_nits,
            hlg_reference_display_black_nits: metadata.hlg_reference_display_black_nits,
            interoperability: "16-bit is TIFF's quantization, not one of BT.2100's specified bit \
                 depths (10 and 12): the file carries BT.2100's transfer function at \
                 TIFF's precision. The stored code values are exact and the single \
                 quantization step is reported above. Automatic HDR presentation is \
                 not claimed — TIFF has no CICP tag of its own, so the signalling \
                 lives in the embedded ICC profile's `cicp` tag, which only a \
                 CICP-aware colour-managed reader honours; treat this as \
                 limited-interoperability interchange rather than a display-ready \
                 deliverable, and see the AVIF or gain-map presets for delivery",
        });
    }
    if let Some(summary) = hdr_tiff_summary {
        // Reported after the write, from what the encoder resolved — the `auto`
        // BigTIFF promotion above cannot cover this preset, because its ICC is built
        // inside the encode arm and `plans_bigtiff` would need the length first.
        if cfg.output.bigtiff == BigTiff::Auto && summary.bigtiff {
            push_warning_buf(
                warnings,
                log,
                "output promoted to BigTIFF (would exceed the classic 4 GiB TIFF limit)".into(),
            );
        }
        let linear = summary.linear;
        report.hdr_linear_tiff = Some(HdrLinearTiffResult {
            pixel_contract: summary.pixel_contract,
            bits_per_sample: summary.bits_per_sample,
            sample_format: summary.sample_format,
            bigtiff: summary.bigtiff,
            icc_bytes: summary.icc_bytes,
            reference_white_sample: 1.0,
            reference_white_nits: linear.reference_white_nits,
            target_peak_nits: linear.target_peak_nits,
            linear_headroom: linear.linear_headroom,
            highlight_compress: linear.highlight_compress,
            shoulder_start: linear.shoulder_start,
            tone_curve: linear.tone_curve,
            gamut_mapping: linear.gamut_mapping,
            linear_domain: linear.linear_domain,
            max_cll_nits: summary.content_light.max_cll_nits,
            max_fall_nits: summary.content_light.max_fall_nits,
            interoperability: "the embedded ICC profile states the BT.2020/D65 \
                               primaries and the linear transfer only; its PCS stops \
                               at the media white, so the reference-white, peak and \
                               headroom values in this block — not the profile — \
                               define the luminance semantics of these samples",
        });
    }
    let loss = outcome.loss;
    report.loss = Some(loss);
    // Report-only statistics of the samples as written — the numeric basis a
    // cross-version `compare` diffs (per-channel mean ΔRGB). Measured *after* the
    // pixels are final, from the same data the encoder wrote.
    report.output_stats = Some(outcome.stats);
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

    // The sidecar is the identity-stamped envelope `{ meta, params }` — identity
    // beside the recipe, never inside it, so `--params <sidecar>` still reloads
    // (`deny_unknown_fields` would reject bare identity keys). `params` is the
    // canonical recipe body computed above, so the sidecar and the advertised
    // `params_hash` can't disagree.
    let sidecar_json = serde_json::to_string_pretty(&SidecarEnvelope {
        meta: SidecarMeta {
            identity: &identity,
            // Taken from the report blocks rather than rebuilt, so an HDR TIFF's
            // companion metadata states exactly what its report does even when the
            // report itself is discarded.
            hdr_linear_tiff: report.hdr_linear_tiff,
            hdr_coded_tiff: report.hdr_coded_tiff,
        },
        params: cfg,
    })
    .map_err(|e| NcError::Other(format!("serializing sidecar: {e}")))?;
    pending.push(encode::write_sidecar(output, &sidecar_json)?);
    // The primary goes LAST, deliberately. Its presence at the final path is what
    // reads as "this conversion succeeded", so it must be the last thing to appear:
    // if some other rename fails, the run leaves no output rather than an output with
    // a missing companion. The reverse order would reintroduce the orphaned-primary
    // case for any commit-phase failure.
    pending.push(primary);

    // Everything is written and fsynced; only the renames are left, and `commit_all`
    // pre-checks every target first so a predictable blocker fails before anything is
    // promoted. This is the narrow window the scope note describes — a crash between
    // two renames, or a rename failure no check predicts, can still leave one final
    // path updated and another not. POSIX cannot fix that (rename is atomic per file,
    // not across a set). What can no longer happen: a truncated artifact at a final
    // path, or a complete primary output orphaned because a later step failed.
    // Any facts the promotion surfaced (currently: a target with other hard links, whose
    // aliases keep the old bytes because the replace is atomic) ride the normal warning
    // channel, so they reach the report and `--strict` promotes them.
    for note in staged::commit_all(std::mem::take(&mut pending))? {
        push_warning_buf(warnings, log, note);
    }
    // Logged only after the renames, so the message describes what is actually on
    // disk under that name rather than what was staged.
    if let Some(path) = &export_ir {
        log.info(format_args!("wrote IR plane {}", path.display()));
    }
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
            algorithm: render_timings.algorithm_ms,
            color: render_timings.color_ms,
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
    // The *complete* convert gate: `validate`'s resolved-config rules plus the one
    // flag-presence rule that cannot live there (see `validate_convert`).
    validate_convert(&cfg, &args)?;

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
        write_json(path, &cfg, &log)?;
    }
    // `--seed` is reserved (no stochastic step in Step 1) but accepted so the
    // documented flag isn't rejected; nothing consumes it yet.
    let _ = args.seed;

    // The per-frame pipeline core (decode → film-base → render → encode +
    // sidecar), shared byte-for-byte with `roll`. Operational concerns the two
    // orchestrators layer differently — report emission, `--strict` gating,
    // telemetry — stay out here.
    let mut warnings = Vec::new();
    // Replaying a sidecar captured under a *different* behavioral
    // `pipeline_version` still applies its parameters, but the default render has
    // changed underneath them — so the pixels won't match the original. Loud and
    // `--strict`-promotable rather than a silently-different image: exposing exactly
    // that mismatch is why `pipeline_version` exists. Pushed before the conversion
    // so it is on stderr before any work happens; note that on a *failed* frame
    // `convert_frame(…)?` propagates and no report is emitted, so stderr is the only
    // place it appears there. (`roll` differs: it records per-frame failures and
    // still emits its report, so the roll-level warning survives a bad frame.)
    if let Some(msg) = pipeline_version_warning(loaded.meta_pipeline_version) {
        push_warning_buf(&mut warnings, &log, msg);
    }
    if let Some(msg) = curve_default_warning(loaded.unpinned_curve) {
        push_warning_buf(&mut warnings, &log, msg);
    }
    let frame = convert_frame(
        "convert",
        &args.input,
        &args.output,
        &cfg,
        InputFromCli {
            transfer: args.input_opts.input_transfer.is_some(),
            meaning: args.input_opts.input_meaning.is_some(),
        },
        dmax_setting,
        args.memory.budget(),
        // `convert` reads the preflight decision off the returned report; the
        // out-param exists for `roll`'s failed frames.
        &mut None,
        &log,
        &mut warnings,
    );

    // A failure here drops the report, and with it every warning accumulated
    // before the failure point — including the memory preflight's RAM-pressure
    // note, whose whole point is to explain a run the OS may kill. `log.warn`
    // already echoed them, but `--quiet` suppresses that, so under `--quiet` they
    // would be lost on *both* channels. Re-emit unconditionally (the `warn_always`
    // treatment clipping already gets) before propagating. `roll` has always
    // honoured this via `frame_report_err`; `convert` did not.
    let frame = match frame {
        Ok(frame) => frame,
        Err(e) => {
            // Only the warnings `--quiet` swallowed: `push_warning_buf` already
            // echoed each one through `log.warn` as it was raised, so re-emitting
            // unconditionally would double-print them on a normal run.
            if log.quiet {
                for w in &warnings {
                    log.warn_always(w);
                }
            }
            return Err(e);
        }
    };
    let ConvertedFrame {
        mut report,
        info,
        recipe_json,
        timings: stage_timings,
        loss,
    } = frame;

    let total_ms = elapsed_ms(started);
    report.elapsed_ms = Some(total_ms);

    // Emit the report before the `--strict` gate so the machine-readable record
    // lands even when a warning then fails the run. (A hard I/O error above
    // returns earlier — its exit code and stderr message are the signal there.)
    emit_report(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
        &log,
    )?;

    // `--strict` promotes any present warning to a non-zero exit. Decide it here,
    // *before* telemetry: a telemetry record's existence is the success signal
    // (there is no `outcome.success` field — see telemetry/strategy), so a run
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
    /// What produced this batch: build identity + the behavioral
    /// `pipeline_version` + the `params_hash` of the **shared** frozen recipe
    /// (`core/conversion-versioning`). Unconditional here (unlike `Report.identity`)
    /// because a roll always resolves a full recipe. Operational provenance only —
    /// no CLI flag, no recipe key, no effect on a single output pixel.
    identity: Identity,
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
    /// What the memory preflight decided for *this* frame — mirrors the
    /// single-frame `Report` field. Per-frame rather than roll-level because
    /// frames may differ in dimensions (and so in estimated peak) even though
    /// they share one budget; the gate runs per frame too.
    ///
    /// Common to both outcomes, like `warnings`: the gate runs before anything
    /// else, so a frame that passed it and then failed still has a decision to
    /// report — and that is precisely the frame whose estimate a reader wants.
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<MemoryReport>,
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
        /// Per-channel mean of the samples as written — mirrors the single-frame
        /// `Report` field. Without it a roll's frames carry no comparison basis, so
        /// `nctool compare` (and the docs' claim that a roll is comparable) would
        /// have nothing to diff frame-to-frame.
        #[serde(skip_serializing_if = "Option::is_none")]
        output_stats: Option<OutputStats>,
        /// This frame's own identity. The roll report stamps the **shared** frozen
        /// recipe's `params_hash`; a per-frame `params` override genuinely changes
        /// that frame's effective recipe and therefore its hash, so the per-frame
        /// value is the only place that difference is visible in the report (it also
        /// rides that frame's sidecar `meta`).
        #[serde(skip_serializing_if = "Option::is_none")]
        identity: Option<Identity>,
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
    // Every preset that pins an output suffix is `convert`-only until
    // `output/presets` makes roll naming/manifests container-aware.
    //
    // The gate is suffix *contract*, not "is it a TIFF": `hdr-linear-tiff` writes a
    // TIFF and is still refused here, because roll derives each frame's output name
    // itself and nothing yet makes that derivation honour a preset's required
    // extension — so a roll would produce paths the equivalent `convert` run would
    // have rejected. (`film-master` pins no suffix, so it stays roll-capable.)
    if required_extensions(cfg.output.preset).is_some() {
        return Err(NcError::Usage(format!(
            "output.preset = \"{}\" is currently supported only by `nc convert`; it \
             pins a required output extension, and roll naming and collision handling \
             for preset-resolved containers are owned by the later output/presets task",
            cfg.output.preset.name()
        )));
    }
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
/// that touches a roll-fixed choice (`film_base`, `reconstruction.curve.dmax`, or
/// `output.preset`) is not rejected — it is applied, with a loud roll-level warning
/// pushed to `roll_warnings` (like the not-frozen warning), so a deliberate
/// per-frame value stays possible while the consistency break is surfaced and
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
                        // `reconstruction.curve.anchor` is roll-fixed for the same
                        // reason, one level up: `curve.dmax` is the roll's reference
                        // density and the anchor placement decides which *tone* that
                        // reference pins (design-spec §7.3). Overriding it per frame is
                        // therefore a tonal-placement break even when every frame shares
                        // one Dmax — the frame renders on a different rule, which is
                        // subtler than a different number and would otherwise be silent.
                        if sets_curve_anchor(&ov) {
                            let msg = format!(
                                "frame {}: a per-frame `params` override sets \
                                 `reconstruction.curve.anchor`, overriding the roll-fixed \
                                 anchor placement — this frame pins a different tone to the \
                                 roll's reference density, breaking tonal consistency. Set \
                                 the placement once in the shared --params recipe (and drop \
                                 the per-frame `reconstruction.curve.anchor`) if you want a \
                                 frozen, consistent roll.",
                                mf.input.display()
                            );
                            log.warn(&msg);
                            roll_warnings.push(msg);
                        }
                        // `output.preset` is the fourth roll-fixed choice, and the
                        // coarsest: it selects which branch out of the ACEScg boundary
                        // runs, so overriding it per frame emits a frame of a different
                        // *image class* — an unclamped linear ACEScg master among
                        // rendered u16 positives, or vice versa. Worse than a Dmin/Dmax
                        // break, and until now the only silent one of the three. Same
                        // shape as its siblings: apply it, warn loudly at roll level
                        // (`--strict`-promotable), never reject. It is worth warning
                        // even for a *matching* override, because `FrameStatus::Ok`
                        // carries no `output_render` block (that field is convert-only),
                        // so `frames[].overrides` is the only other place the change is
                        // visible.
                        if sets_output_preset(&ov) {
                            let msg = format!(
                                "frame {}: a per-frame `params` override sets \
                                 `output.preset`, overriding the roll's output policy — \
                                 this frame takes a different branch out of the ACEScg \
                                 boundary from the rest of the roll, so its pixels are a \
                                 different image class (unclamped linear ACEScg master vs \
                                 rendered TIFF), not merely a different rendering. Set the \
                                 preset once in the shared --params recipe (and drop the \
                                 per-frame `output.preset`) if you want one consistent \
                                 roll.",
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
                        // Same ordering as the shared gate above: roll-specific
                        // rejections first, the least-specific missing-base last.
                        reject_roll_unsupported(&cfg)?;
                        reject_roll_unsupported_input(&cfg)?;
                        validate_with_remedy(&cfg, FilmBaseRemedy::SharedRecipe)?;
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
            output_stats: report.output_stats,
            identity: report.identity,
        },
        memory: report.memory,
        warnings: report.warnings,
        overrides: pf.overrides.clone(),
    }
}

/// A failed frame's [`FrameReport`] entry — the error message plus any warnings
/// accumulated before the failure point (decode/IR/film-base notices), so a frame
/// that warns and then fails still reports them (and they aren't lost to `--quiet`).
/// `memory` is whatever the preflight decided before the failure (it runs first, so
/// only a frame rejected *by* the gate has none).
fn frame_report_err(
    pf: &PlannedFrame,
    err: &NcError,
    memory: Option<MemoryReport>,
    warnings: Vec<String>,
) -> FrameReport {
    FrameReport {
        input: pf.input.clone(),
        output: Some(pf.output.clone()),
        status: FrameStatus::Failed {
            error: err.to_string(),
        },
        memory,
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
        meta_pipeline_version,
        unpinned_curve: shared_unpinned_curve,
    } = load_recipe(args.recipe_in.as_deref())?;
    // Roll-specific rejections run **before** the shared `validate`, and the order
    // is the same least-specific-diagnosis-last policy `validate` itself now
    // follows: "this setting cannot work in roll mode" names the offending key,
    // while "no film base selected" is the least specific diagnosis available. A
    // recipe that is both baseless and roll-invalid should surface the roll problem
    // first, or the user adds a base only to meet a second error.
    reject_roll_unsupported(&shared)?;
    reject_roll_unsupported_input(&shared)?;
    // `roll`'s remedy for an unstated film base is the shared recipe, never a flag:
    // `RollArgs` accepts none of the three film-base flags.
    validate_with_remedy(&shared, FilmBaseRemedy::SharedRecipe)?;

    // A roll's headline guarantee is one frozen, roll-fixed film base shared by
    // every frame. Only an *explicit* base delivers that: `auto`/`region`
    // re-estimate `Dmin` from each frame's own pixels, so the roll is neither
    // frozen nor color-consistent even though the report still prints "one shared
    // recipe". Warn loudly (report + stderr, `--strict`-promotable) rather than
    // hard-failing, so a best-effort batch stays usable.
    let mut roll_warnings: Vec<String> = Vec::new();
    // A frozen recipe replayed under a different behavioral `pipeline_version` than
    // it was captured under is a roll-level fact (one shared recipe, N frames), so
    // it rides `roll_warnings` rather than any single frame's list.
    if let Some(msg) = pipeline_version_warning(meta_pipeline_version) {
        log.warn(&msg);
        roll_warnings.push(msg);
    }
    // Same reasoning for an unpinned curve / anchor default: one shared recipe, N frames,
    // so it is a roll-level fact rather than any single frame's.
    if let Some(msg) = curve_default_warning(shared_unpinned_curve) {
        log.warn(&msg);
        roll_warnings.push(msg);
    }
    if !matches!(shared.film_base.source, Some(FilmBaseSource::Explicit(_))) {
        // `validate` above already rejected `None`, so only the two estimating
        // sources reach here.
        let kind = match shared.film_base.source {
            Some(FilmBaseSource::Auto) => "auto",
            Some(FilmBaseSource::Region(_)) => "region",
            Some(FilmBaseSource::Explicit(_)) | None => unreachable!("validate rejects both"),
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
        // Per-frame warnings and the preflight decision accumulate here so a frame
        // that warns / gets sized and *then* fails still hands them back (the report
        // only rides out on success).
        let mut warnings = Vec::new();
        let mut memory = None;
        match convert_frame(
            "roll",
            &pf.input,
            &pf.output,
            &pf.cfg,
            InputFromCli::none(),
            pf.dmax_setting,
            args.memory.budget(),
            &mut memory,
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
                frames.push(frame_report_err(pf, &e, memory, warnings));
            }
        }
    }

    // `--strict` promotes any warning to a failing exit (convert's gate,
    // aggregated across the roll): both the roll-level warnings (e.g. the base is
    // not frozen) and any per-frame warning. Decided before the report is emitted.
    let strict_failure =
        args.strict && (!roll_warnings.is_empty() || frames.iter().any(|f| !f.warnings.is_empty()));

    let total = frames.len();
    // Roll-level identity: the build that ran, plus the `params_hash` of the
    // **shared** frozen recipe (a per-frame override changes that frame's effective
    // recipe, and that frame's own sidecar/report carries its own hash).
    let identity =
        Identity::with_params_hash(version::stable_hash(&canonical_params_json(&shared)?));
    let roll = RollReport {
        command: "roll",
        identity,
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
        &log,
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
    let mut report = Report {
        command: Some("inspect"),
        // `identity` is on EVERY report (design-spec §9), not just conversions —
        // an `inspect` result is an artifact someone files, and "which build read
        // this scan" is exactly as attributable a question. `Identity::new` is the
        // no-recipe constructor: `inspect` resolves no recipe, so `params_hash` is
        // genuinely absent rather than a construction artifact.
        identity: Some(Identity::new()),
        input: Some(args.input.clone()),
        ..Report::default()
    };

    // Memory preflight before decode, on the decode-only profile: `inspect` never
    // renders or encodes, so gating it on the full-pipeline peak would reject
    // scans it can diagnose comfortably. It always runs the auto detector below
    // (the suggested `Dmin`), so the film-base phase counts its interior sample.
    let budget = args.memory.budget();
    report.memory = Some(preflight_memory(
        &args.input,
        RunProfile::DecodeOnly,
        SamplePlan::auto(),
        budget,
        memory::detect_total_ram(),
        &log,
        &mut report.warnings,
    )?);

    let (image, info) = decode_within(&args.input, budget.bytes())?;
    log.info(format_args!(
        "decoded {:?} {}x{} (ir={})",
        info.format, info.width, info.height, info.ir_present
    ));

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

    // IR film-holder mask (only with `--film-type chromogenic` on a scan carrying a
    // *marker-verified* IR plane). Diagnostic on `inspect`: it shows which along-edge
    // segments the opaque holder occludes, and drives the film-segment restriction
    // the candidate search below uses.
    let film_type = args.film_type.unwrap_or_default();
    let chromogenic = film_type.ir_transparent();
    // Whether the holder mask actually consumes the IR plane here — the one Step-1
    // consumer. Tailor the IR-status note to what happens so it never contradicts
    // the `holder_mask` output below (the old unconditional "preserved but not
    // used" note fired even when the mask was using it).
    let ir_consumed = chromogenic && info.ir_present && image.ir_verified;
    if chromogenic && !info.ir_present {
        // Mirror `convert`'s note: chromogenic declared but nothing to mask with.
        push_warning(
            &mut report,
            &log,
            "--film-type chromogenic declared but the scan has no IR plane; \
             film-holder detection is RGB-only"
                .into(),
        );
    } else if chromogenic && info.ir_present && !image.ir_verified {
        // Shape-only IR plane: carried/exportable but not trusted for detection.
        push_warning(
            &mut report,
            &log,
            "--film-type chromogenic declared and an IR plane is present, but it is \
             identified by shape alone (no NewSubfileType=4 marker) and not trusted \
             for holder detection; film-holder detection is RGB-only"
                .into(),
        );
    } else if info.ir_present && !ir_consumed {
        // IR present but no consumer engaged it (non-chromogenic, or unknown/silver).
        push_warning(
            &mut report,
            &log,
            "input carries an IR plane; preserved but not used in Step 1 \
             (use `convert --export-ir` to write it out)"
                .into(),
        );
    }
    // Best-effort, like the candidate search below: a too-small chromogenic image
    // makes `ir_holder_mask` error on `scan_depth`, but `inspect` is informational
    // and must not abort only because `--film-type` was passed — report it as a note.
    match film_base::ir_holder_mask(&image, film_type) {
        Ok(Some(mask)) => report.holder_mask = Some(mask),
        Ok(None) => {}
        Err(e) => push_warning(
            &mut report,
            &log,
            format!("holder-mask detection skipped — {e}"),
        ),
    }

    // Candidate rebate bands + suggested Dmin via the inward-scan detector. For
    // inspect this is informational — a refusal is a note, not fatal — and the
    // candidates are reported even when selection refuses, so the user can
    // confirm a rectangle for `--base-region` instead of measuring one.
    match film_base::rebate_candidates(&image, film_type) {
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
                // film-base/content-fallback task); short lead-in only.
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
        &log,
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
            source: Some(FilmBaseSource::Explicit(rgb)),
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
///
/// **The `unwrap_or(FilmBaseSource::Auto)` below is the only surviving default
/// film-base choice in the crate, and no fingerprint watches it.** Since
/// `film_base.source` lost its default, `version::PIPELINE_FINGERPRINTS` no
/// longer covers this decision from either side: `base` pins the detector by
/// naming `Auto` explicitly, and `recipe` sees the resolved config's `null`.
/// Changing what an unstated `estimate` resolves to would therefore move every
/// `nc estimate` result with the whole drift gate green — verify such a change by
/// hand, and do not assume the gate is watching.
fn run_estimate(args: EstimateArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    if let Some(rf) = args.report.report_file.as_deref() {
        ensure_write_targets_distinct(&args.input, &[("--report-file", rf)])?;
    }
    // `estimate` exists to *produce* a base, so requiring one first would be
    // circular: here — and only here — an unstated source still means `auto`.
    let source = film_base_source_override(&args.film_base).unwrap_or(FilmBaseSource::Auto);
    // Guard an explicit base with the same check `convert` applies (a recipe
    // never reaches estimate, but a bad `--film-base` must fail loudly rather
    // than be echoed back). Region bounds are checked by `film_base::estimate`.
    if let FilmBaseSource::Explicit(b) = &source {
        validate_explicit_film_base(b)?;
    }

    let mut report = Report {
        command: Some("estimate"),
        // Same contract as `inspect`: build identity on every report, no
        // `params_hash` because no recipe was resolved. An estimated `Dmin` is
        // routinely frozen into a roll recipe, so which build measured it matters.
        identity: Some(Identity::new()),
        input: Some(args.input.clone()),
        ..Report::default()
    };

    // Memory preflight before decode (decode-only profile — `estimate` samples the
    // decoded image and stops). Its film-base phase is the largest rectangle this
    // invocation will gather: the base source's own sample (`--grid` samples cells
    // of `--base-region`, or of the whole frame when it is absent — counted
    // conservatively as the whole rectangle), plus any `--d-max-region`. They are
    // gathered one at a time, so the model takes the largest.
    let budget = args.memory.budget();
    let mut sampling = if args.grid {
        match args.film_base.base_region {
            // `--grid` samples five cells of the rectangle, one at a time, so the
            // phase peaks at one cell — not at the whole rectangle.
            Some([_, _, w, h]) => SamplePlan::rect(film_base::grid_cell_pixels(w, h)),
            None => SamplePlan::none().with_whole_frame_grid(),
        }
    } else {
        sample_plan(&source)
    };
    if let Some([_, _, w, h]) = args.d_max_region {
        sampling = sampling.with_rect(w as u64 * h as u64);
    }
    report.memory = Some(preflight_memory(
        &args.input,
        RunProfile::DecodeOnly,
        sampling,
        budget,
        memory::detect_total_ram(),
        &log,
        &mut report.warnings,
    )?);

    let (image, info) = decode_within(&args.input, budget.bytes())?;
    log.info(format_args!(
        "decoded {:?} {}x{} (ir={})",
        info.format, info.width, info.height, info.ir_present
    ));

    for w in &info.warnings {
        push_warning(&mut report, &log, w.clone());
    }

    // Mirror `convert`'s notes: only the `auto` single-measurement path consults the
    // IR holder mask, so a chromogenic declaration there degrades to RGB-only when
    // there is no IR plane, or when the IR plane is shape-only (unverified
    // provenance). The `--grid` and explicit/region paths never touch IR, so the
    // declaration is a genuine no-op for them and needs no note.
    if !args.grid
        && matches!(source, FilmBaseSource::Auto)
        && args.film_type.unwrap_or_default().ir_transparent()
    {
        if !info.ir_present {
            push_warning(
                &mut report,
                &log,
                "--film-type chromogenic declared but the scan has no IR plane; \
                 using RGB-only film-holder detection for the film base"
                    .into(),
            );
        } else if !image.ir_verified {
            push_warning(
                &mut report,
                &log,
                "--film-type chromogenic declared and an IR plane is present, but it is \
                 identified by shape alone (no NewSubfileType=4 marker) and not trusted \
                 for holder detection; using RGB-only film-holder detection for the film base"
                    .into(),
            );
        }
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
        // quality warnings (non-uniform region, cross-edge disagreement). The
        // declared film type lets the `auto` source use the IR holder mask when
        // the scan is chromogenic and carries an IR plane.
        let est = film_base::estimate(&image, &source, args.film_type.unwrap_or_default())?;
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
        &log,
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
    // Falsifiable in a debug build: the `unwrap_or` below is unreachable, and if it
    // ever *were* reached it would silently record a plausible-but-wrong `auto` for
    // a run that resolved something else — the one failure mode a telemetry field
    // cannot recover from after the fact. A schema bump to make the field optional
    // is not worth it for an arm `validate` already forecloses; this assertion is.
    debug_assert!(
        report.film_base_source.is_some(),
        "telemetry runs only after a successful conversion, which means `validate` \
         accepted a stated film base"
    );
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
        // The report's copy is the source `convert_frame` actually resolved and
        // ran, so it cannot disagree with the conversion. It is always `Some`
        // here — telemetry is emitted only after a conversion succeeded, which
        // means `validate` accepted the source — and telemetry must never fail a
        // run, so the unreachable arm degrades instead of panicking.
        film_base_source: report
            .film_base_source
            .clone()
            .unwrap_or(FilmBaseSource::Auto),
        dmax: report.dmax,
        preset: cfg.output.preset,
        // Via `depth()` — the single place a recipe value becomes a depth — so the
        // record cannot disagree with what `io::encode` actually wrote. Reading
        // `cfg.output.hdr` here would report `false` for a `film-master` run, whose
        // switch is pinned at its default while the branch resolves f32.
        output_hdr: cfg.output.depth() == crate::types::OutDepth::F32,
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

    /// A resolved config whose film base is **stated**.
    ///
    /// `film_base.source` has no default (see `types::FilmBaseParams::source`),
    /// so `validate` rejects an unstated one. Tests that are not *about* the film
    /// base use this, otherwise every unrelated assertion would trip that rule
    /// instead of the one under test. `Auto` is the value this used to default to,
    /// so these tests exercise exactly what they did before.
    fn base_cfg() -> ResolvedConfig {
        ResolvedConfig {
            film_base: FilmBaseParams {
                source: Some(FilmBaseSource::Auto),
            },
            ..ResolvedConfig::default()
        }
    }
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
            ..base_cfg()
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
            ..base_cfg()
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

        // unspecified everywhere → that curve's own default. Selected explicitly
        // because the *default* curve is the sigmoid, and this test's subject is
        // the exponential's gamma-merge precedence, not which curve is default.
        let cfg = merge(
            base_cfg(),
            &parse_convert(&["--density-curve", "exponential"]),
        )
        .unwrap();
        assert_eq!(gamma_of(&cfg), 2.0);
    }

    #[test]
    fn merge_handles_reconstruction_and_array_flags() {
        let cfg = merge(
            base_cfg(),
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
                gamma: 2.0,
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
            let err = merge(base_cfg(), &parse_convert(flags)).unwrap_err();
            assert!(
                matches!(err, NcError::Usage(_)),
                "{flags:?} must be a usage error, got {err}"
            );
        }

        // A sigmoid flag with a resolved exponential curve is invalid, not inert.
        // The curve is named explicitly since the sigmoid is now the default.
        let err = merge(
            base_cfg(),
            &parse_convert(&[
                "--density-curve",
                "exponential",
                "--sigmoid-contrast",
                "1.2",
            ]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--sigmoid-contrast"), "{err}");
        assert!(err.to_string().contains("exponential"), "{err}");

        // A customized gamma with a resolved sigmoid curve is a loud usage
        // error, never ignored or downgraded to a warning.
        let err = merge(
            base_cfg(),
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
        let cfg = merge(base_cfg(), &parse_convert(&["--auto-wb", "gray-world"])).unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::GrayWorld);
        let cfg = merge(base_cfg(), &parse_convert(&["--auto-wb", "percentile"])).unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::Percentile);

        // No flag keeps the recipe's auto mode; a flag replaces it (flags win).
        let mut recipe = base_cfg();
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
            let mut cfg = base_cfg();
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
        let cfg = merge(base_cfg(), &parse_convert(&["--d-max", "1.75"])).unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Explicit(1.75));
        let cfg = merge(base_cfg(), &parse_convert(&["--no-d-max"])).unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::None);
        let cfg = merge(base_cfg(), &parse_convert(&["--auto-d-max"])).unwrap();
        assert_eq!(curve_of(&cfg).dmax(), DmaxSource::Auto);
        let cfg = merge(base_cfg(), &parse_convert(&["--fixed-d-max"])).unwrap();
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
            base_cfg(),
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
    fn merge_sigmoid_anchor_placement_flags() {
        // The placement is one rule, so each flag replaces the whole variant. A
        // missing merge arm here would be invisible: the default placement is
        // already `MidAtDmaxFraction(0.5)`, so `--sigmoid-mid-fraction 0.5` would
        // "work" by accident — hence asserting a *different* fraction.
        let cfg = merge(
            base_cfg(),
            &parse_convert(&[
                "--density-curve",
                "sigmoid",
                "--sigmoid-mid-fraction",
                "0.65",
            ]),
        )
        .unwrap();
        assert_eq!(
            sigmoid_of(&cfg).anchor,
            AnchorPlacement::MidAtDmaxFraction(0.65)
        );

        let cfg = merge(
            base_cfg(),
            &parse_convert(&["--density-curve", "sigmoid", "--sigmoid-white-at-d-max"]),
        )
        .unwrap();
        assert_eq!(sigmoid_of(&cfg).anchor, AnchorPlacement::WhiteAtDmax);

        // Absent flags keep the recipe's placement (the flags-win merge never
        // clobbers with a default), and `--sigmoid-white-at-d-max` is the escape
        // hatch back from a recipe fraction.
        let recipe: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"sigmoid","anchor":{"mid-at-dmax-fraction":0.6}}}}"#,
        )
        .unwrap();
        assert_eq!(
            sigmoid_of(&merge(recipe, &parse_convert(&["--sigmoid-contrast", "2.0"])).unwrap())
                .anchor,
            AnchorPlacement::MidAtDmaxFraction(0.6)
        );
        let recipe: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"sigmoid","anchor":{"mid-at-dmax-fraction":0.6}}}}"#,
        )
        .unwrap();
        assert_eq!(
            sigmoid_of(&merge(recipe, &parse_convert(&["--sigmoid-white-at-d-max"])).unwrap())
                .anchor,
            AnchorPlacement::WhiteAtDmax
        );

        // Both placement flags are sigmoid-only: under a resolved exponential
        // curve they are a usage error naming the offending flag, not inert.
        for flag in [
            ["--sigmoid-mid-fraction", "0.6"].as_slice(),
            ["--sigmoid-white-at-d-max"].as_slice(),
        ] {
            // The exponential curve is selected explicitly: it is no longer the
            // default, so without this the flags would resolve against a sigmoid
            // and be perfectly valid — the rejection under test would vanish.
            let argv = [&["--density-curve", "exponential"][..], flag].concat();
            let err = merge(base_cfg(), &parse_convert(&argv)).unwrap_err();
            assert!(err.to_string().contains(flag[0]), "{err}");
        }
        // And they are mutually exclusive at the clap layer.
        assert!(
            Cli::try_parse_from([
                "nc",
                "convert",
                "in.tiff",
                "-o",
                "out.tiff",
                "--density-curve",
                "sigmoid",
                "--sigmoid-mid-fraction",
                "0.6",
                "--sigmoid-white-at-d-max",
            ])
            .is_err()
        );
    }

    #[test]
    fn validate_rejects_a_contrast_too_small_to_place_the_anchor() {
        // The placement adds MID_GREY_OUTPUT_DECADES / contrast, so a positive-but-tiny
        // slope overflows that quotient. Before this check it passed `validate` (positive
        // and under the cap), then panicked on a debug assertion — exit 101 for what is a
        // usage error.
        // The quotient overflows below ~2.2e-39, so those are the values under test.
        for bad in [1e-40, 1e-44, 2.1e-39] {
            let cfg = sigmoid_cfg(SigmoidParams {
                contrast: bad,
                anchor: AnchorPlacement::MidAtDmaxFraction(0.5),
                ..SigmoidParams::default()
            });
            let Err(err) = validate(&cfg) else {
                panic!("contrast {bad} must be rejected")
            };
            assert!(matches!(err, NcError::Usage(_)), "{bad}: {err}");
            assert!(err.to_string().contains("--sigmoid-contrast"), "{err}");
        }
        // Just above the threshold the derivation stays finite and is therefore accepted —
        // the check guards the *overflow*, not "small contrast" in general. `f32::MIN_POSITIVE`
        // gives a finite anchor of ~6.3e37, and since `contrast * anchor` is then exactly
        // MID_GREY_OUTPUT_DECADES the render is a flat mid-grey rather than a broken one.
        validate(&sigmoid_cfg(SigmoidParams {
            contrast: f32::MIN_POSITIVE,
            anchor: AnchorPlacement::MidAtDmaxFraction(0.5),
            ..SigmoidParams::default()
        }))
        .unwrap();
        // `WhiteAtDmax` performs no division, so an overflowing slope is none of this rule's
        // business — it stays accepted, which is what makes the check targeted rather than
        // a blanket lower bound on contrast.
        validate(&sigmoid_cfg(SigmoidParams {
            contrast: 1e-40,
            anchor: AnchorPlacement::WhiteAtDmax,
            ..SigmoidParams::default()
        }))
        .unwrap();
    }

    #[test]
    fn curve_default_warning_fires_for_every_recipe_that_leaves_the_curve_unpinned() {
        // The witness is a raw-JSON probe, so test it that way.
        let probe =
            |json: &str| unpinned_curve(&serde_json::from_str::<serde_json::Value>(json).unwrap());
        // An archived sigmoid recipe with no `anchor` — the case that silently changed
        // meaning on 2026-08-03. The curve itself is pinned, so only the placement moved.
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{"type":"sigmoid","contrast":1.0}}}"#),
            Some(UnpinnedCurve::AnchorOnly)
        );
        // Pinned explicitly either way ⇒ nothing changed underneath it, no noise.
        assert_eq!(
            probe(
                r#"{"reconstruction":{"curve":{"type":"sigmoid","anchor":"white-at-dmax","dmax":{"explicit":2.0}}}}"#
            ),
            None
        );
        assert_eq!(
            probe(
                r#"{"reconstruction":{"curve":{"type":"sigmoid","anchor":{"mid-at-dmax-fraction":0.5},"dmax":{"explicit":1.3}}}}"#
            ),
            None
        );
        // A curve pinned by *type only* looks pinned and is not: on 2026-08-08 the
        // exponential's gamma moved 1.0 → 2.0 and the nominal dmax 2.0 → 1.3, so this
        // file renders differently than it did with nothing in it to show that.
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{"type":"exponential"}}}"#),
            Some(UnpinnedCurve::MovedDefaults)
        );
        // Pinning both moved scalars silences it — the falsifiable half.
        assert_eq!(
            probe(
                r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.0,"dmax":{"explicit":2.0}}}}"#
            ),
            None
        );
        // Either one alone still floats the other.
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.0}}}"#),
            Some(UnpinnedCurve::MovedDefaults)
        );
        // A sigmoid that pins its placement but not its dmax: the anchor rule is
        // settled, the reference density it is measured against is not.
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{"type":"sigmoid","anchor":"white-at-dmax"}}}"#),
            Some(UnpinnedCurve::MovedDefaults)
        );
        // A recipe with no `curve` section used to resolve to the exponential, and this
        // probe used to stay silent for it. Since 2026-08-08 it resolves to the sigmoid
        // at a different anchor rule and a different Dmax, so silence would be exactly
        // the "archived recipe silently reinterpreted" case design-spec §7.3 forbids —
        // and no `meta.pipeline_version` rides a bare recipe to catch it instead.
        assert_eq!(
            probe(r#"{"reconstruction":{"type":"density"}}"#),
            Some(UnpinnedCurve::WholeCurve)
        );
        // A recipe silent on `reconstruction` resolves identically to the line above, yet
        // deliberately does *not* warn: it states no reconstruction configuration to
        // reinterpret, which is the same position as passing no recipe at all. Warning
        // here would fire on nearly every partial recipe and make `--strict` fail for all
        // of them permanently — see `unpinned_curve` for the full argument.
        assert_eq!(probe(r#"{"film_base":{"source":"auto"}}"#), None);
        // `simple` runs no curve stage, so no curve default can reach it.
        assert_eq!(probe(r#"{"reconstruction":{"type":"simple"}}"#), None);

        assert_eq!(curve_default_warning(None), None);
        let msg = curve_default_warning(Some(UnpinnedCurve::AnchorOnly)).expect("must warn");
        assert!(msg.contains("white-at-dmax"), "{msg}");
        let msg = curve_default_warning(Some(UnpinnedCurve::WholeCurve)).expect("must warn");
        assert!(msg.contains("reconstruction.curve"), "{msg}");
        assert!(msg.contains("exponential"), "{msg}");
    }

    #[test]
    fn sets_curve_anchor_probes_the_roll_fixed_placement() {
        let probe = |json: &str| {
            sets_curve_anchor(&serde_json::from_str::<serde_json::Value>(json).unwrap())
        };
        assert!(probe(
            r#"{"reconstruction":{"curve":{"anchor":"white-at-dmax"}}}"#
        ));
        assert!(probe(
            r#"{"reconstruction":{"curve":{"anchor":{"mid-at-dmax-fraction":0.6}}}}"#
        ));
        // A restating override still counts (same rule as `sets_curve_dmax`), and a frame
        // that touches only non-placement keys does not.
        assert!(!probe(r#"{"reconstruction":{"curve":{"contrast":2.0}}}"#));
        assert!(!probe(r#"{"print":{"print_exposure":0.5}}"#));
    }

    #[test]
    fn validate_rejects_bad_sigmoid_mid_fraction() {
        // (0, 1]: 0 detaches the anchor from the reference entirely (mid-grey on
        // the film base), negative places it below the base, above 1 places
        // mid-grey past display white.
        for bad in [0.0, -0.5, 1.01, 2.0, f32::NAN, f32::INFINITY] {
            let cfg = sigmoid_cfg(SigmoidParams {
                anchor: AnchorPlacement::MidAtDmaxFraction(bad),
                ..SigmoidParams::default()
            });
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "fraction {bad} should fail"
            );
        }
        // The default, the inclusive upper edge, and a very small positive
        // fraction are all accepted.
        for good in [0.5, 1.0, 1e-3] {
            validate(&sigmoid_cfg(SigmoidParams {
                anchor: AnchorPlacement::MidAtDmaxFraction(good),
                ..SigmoidParams::default()
            }))
            .unwrap();
        }
        // `WhiteAtDmax` carries no value, so nothing to range-check.
        validate(&sigmoid_cfg(SigmoidParams {
            anchor: AnchorPlacement::WhiteAtDmax,
            ..SigmoidParams::default()
        }))
        .unwrap();
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
            base_cfg(),
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
            base_cfg(),
            // `--density-gamma` is the exponential curve's knob, and the default
            // curve is the sigmoid, so the curve must be selected explicitly —
            // otherwise `validate` correctly rejects the pair as a contradiction.
            &parse_convert(&[
                "--density-curve",
                "exponential",
                "--density-gamma",
                "1.8",
                "--output-hdr",
            ]),
        )
        .unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        // The sigmoid form and the simple form round-trip too.
        for cfg in [
            merge(
                base_cfg(),
                &parse_convert(&["--density-curve", "sigmoid", "--sigmoid-toe", "0.1"]),
            )
            .unwrap(),
            merge(base_cfg(), &parse_convert(&["--reconstruction", "simple"])).unwrap(),
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
        let v = serde_json::to_value(base_cfg()).unwrap();
        assert_eq!(v["reconstruction"]["schema_version"], 1);
        assert_eq!(v["reconstruction"]["type"], "density");
        assert_eq!(v["reconstruction"]["curve"]["type"], "sigmoid");
        assert_eq!(v["reconstruction"]["curve"]["dmax"], "fixed");
        assert_eq!(
            v["reconstruction"]["density"]["scale"],
            serde_json::json!([1.0, 1.0, 1.0])
        );

        // Partial input: omitted curve normalizes to the tagged default curve.
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"reconstruction":{"schema_version":1,"type":"density"}}"#)
                .unwrap();
        assert_eq!(*curve_of(&cfg), DensityCurve::default());
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["reconstruction"]["curve"]["type"], "sigmoid");

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
            None,
            DmaxSetting::Default,
        ))
        .unwrap();
        assert_eq!(v, serde_json::json!({"type": "simple"}));

        // The exponential curve, named explicitly since it is no longer the
        // default: no placement rule exists for it, so `anchor` is omitted
        // entirely rather than reported as a null rule; the reference *is* the
        // anchor, so `anchor_value` mirrors it.
        let v = serde_json::to_value(reconstruction_result(
            &exponential_cfg(ExponentialParams::default()).reconstruction,
            Some(2.0),
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
                    "dmax": {"policy": "fixed", "value": 2.0, "provenance": "default"},
                    "anchor_value": 2.0
                }
            })
        );

        // The default curve (sigmoid) additionally reports its placement rule,
        // because it *has* one — that is the shape a default report now carries.
        let v = serde_json::to_value(reconstruction_result(
            &Reconstruction::default(),
            Some(2.0),
            Some(2.0),
            DmaxSetting::Default,
        ))
        .unwrap();
        assert_eq!(v["curve"]["type"], "sigmoid");
        assert_eq!(
            v["curve"]["anchor"],
            serde_json::json!({"mid-at-dmax-fraction": 0.5})
        );

        // `none` reports a null value; the recipe provenance rides through.
        let cfg = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            dmax: DmaxSource::None,
        });
        let v = serde_json::to_value(reconstruction_result(
            &cfg.reconstruction,
            None,
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
                // Default mid-grey placement: A = 0.5*1.37 + 0.745/2.0687 ≈ 1.045, so the
                // derived anchor is deliberately NOT the reference here.
                Some(1.045),
                setting,
            ))
            .unwrap();
            assert_eq!(v["curve"]["type"], "sigmoid");
            // The placement rule and the derived anchor make this block self-contained:
            // `dmax.value` is the reference, `anchor_value` is what rendered to 1.0.
            assert_eq!(
                v["curve"]["anchor"],
                serde_json::json!({"mid-at-dmax-fraction": 0.5})
            );
            // Approximate: an f32 widens to f64 in JSON (1.045f32 is 1.0449999570846558),
            // so an exact literal comparison would be asserting f32 representation.
            let got = v["curve"]["anchor_value"].as_f64().expect("anchor_value");
            assert!((got - 1.045).abs() < 1e-6, "{got}");
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
            recipe: Some(base_cfg()),
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
        let cfg = merge(base_cfg(), &parse_convert(&["--output-hdr"])).unwrap();
        assert!(cfg.output.hdr);
        // No flag → the default stays off.
        let cfg = merge(base_cfg(), &parse_convert(&[])).unwrap();
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
        let cfg = merge(base_cfg(), &parse_convert(&["--output-sdr"])).unwrap();
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

    // --- named output presets: film-master + the shared display controls -------

    /// A resolved config on the `film-master` branch, otherwise all defaults.
    fn film_master_cfg() -> ResolvedConfig {
        ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::FilmMaster,
                ..OutputParams::default()
            },
            ..base_cfg()
        }
    }

    /// The `Usage` message from a config that must fail validation.
    fn validate_err(cfg: &ResolvedConfig) -> String {
        match validate(cfg) {
            Err(NcError::Usage(m)) => m,
            other => panic!("expected a Usage error, got {other:?}"),
        }
    }

    #[test]
    fn merge_output_preset_flag_replaces_the_recipe_preset() {
        // The merge arm — a forgotten one silently makes `--output-preset` a no-op
        // (the four-spot-wiring trap).
        let cfg = merge(
            base_cfg(),
            &parse_convert(&["--output-preset", "film-master"]),
        )
        .unwrap();
        assert_eq!(cfg.output.preset, OutputPreset::FilmMaster);
        // Absent flag → the recipe's preset survives.
        let recipe: ResolvedConfig =
            serde_json::from_str(r#"{"output":{"preset":"film-master"}}"#).unwrap();
        assert_eq!(
            merge(recipe.clone(), &parse_convert(&[]))
                .unwrap()
                .output
                .preset,
            OutputPreset::FilmMaster
        );
        // An atomic policy choice, so the flag also resets a recipe's named preset
        // back to the no-preset legacy path (flags win in both directions).
        assert_eq!(
            merge(recipe, &parse_convert(&["--output-preset", "legacy"]))
                .unwrap()
                .output
                .preset,
            OutputPreset::Legacy
        );
        // No flag, no recipe key → the default.
        assert_eq!(
            merge(base_cfg(), &parse_convert(&[]))
                .unwrap()
                .output
                .preset,
            OutputPreset::Legacy
        );
        // The flag shares `OutputPreset::parse`, so a renamed/unknown value is a
        // usage error at merge, not a silent fallback to legacy.
        let err = merge(
            base_cfg(),
            &parse_convert(&["--output-preset", "scene-master"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("film-master"), "{err}");
    }

    #[test]
    fn merge_linear_range_flag_replaces_the_recipe_pair() {
        // The merge arm for the atomic `[low, high]` pair.
        let cfg = merge(base_cfg(), &parse_convert(&["--linear-range", "0.02,0.97"])).unwrap();
        assert_eq!(cfg.print.linear_range, [0.02, 0.97]);
        // A negative low parses (a leading `-` must not be read as a flag).
        let cfg = merge(base_cfg(), &parse_convert(&["--linear-range", "-0.1,1.2"])).unwrap();
        assert_eq!(cfg.print.linear_range, [-0.1, 1.2]);
        // Absent flag → the recipe pair survives.
        let recipe: ResolvedConfig =
            serde_json::from_str(r#"{"print":{"linear_range":[0.05,0.9]}}"#).unwrap();
        assert_eq!(
            merge(recipe.clone(), &parse_convert(&[]))
                .unwrap()
                .print
                .linear_range,
            [0.05, 0.9]
        );
        // Passing the documented default is the flags-win *reset* of a recipe's
        // non-default pair — this is what makes such a recipe usable under
        // `film-master`, so it must not be treated as "no override given".
        assert_eq!(
            merge(recipe, &parse_convert(&["--linear-range", "0,1"]))
                .unwrap()
                .print
                .linear_range,
            [0.0, 1.0]
        );
    }

    #[test]
    fn validate_rejects_a_bad_linear_range() {
        // The endpoints divide the affine, so all three failure modes must be loud:
        // non-finite, mis-ordered/degenerate, and an unrepresentable span (two finite
        // endpoints whose difference overflows would silently collapse every sample).
        for bad in [
            [f32::NAN, 1.0],
            [0.0, f32::INFINITY],
            [1.0, 0.0],
            [0.5, 0.5],
            [-f32::MAX, f32::MAX],
        ] {
            let cfg = ResolvedConfig {
                print: PrintParams {
                    linear_range: bad,
                    ..PrintParams::default()
                },
                ..film_master_cfg()
            };
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn linear_range_is_consumed_only_by_the_display_preset() {
        // The legacy path keeps its frozen ordering and `film-master` bypasses
        // print controls entirely. Ultra HDR reaches the shared display stage,
        // so it is the first preset that legitimately consumes this value.
        //
        // The two branches are rejected by *different* rules, so assert each rule's own
        // distinctive wording: both messages name `linear_range`, which means a
        // `contains("linear_range")` check would stay green even if one rule vanished.
        let cfg_with = |preset: OutputPreset| ResolvedConfig {
            print: PrintParams {
                linear_range: [0.02, 0.97],
                ..PrintParams::default()
            },
            output: OutputParams {
                preset,
                ..OutputParams::default()
            },
            ..base_cfg()
        };
        // Legacy → the "no consumer" rule.
        let msg = validate_err(&cfg_with(OutputPreset::Legacy));
        assert!(msg.contains("linear_range"), "{msg}");
        assert!(msg.contains("legacy no-preset TIFF"), "{msg}");
        // film-master → the print-control bypass sweep, which is a different reason.
        let msg = validate_err(&cfg_with(OutputPreset::FilmMaster));
        assert!(msg.contains("linear_range"), "{msg}");
        assert!(
            msg.contains("bypasses all print and display controls"),
            "{msg}"
        );
        validate(&cfg_with(OutputPreset::UltraHdrV1)).unwrap();
        // The default pair is of course fine on both branches.
        validate(&base_cfg()).unwrap();
        validate(&film_master_cfg()).unwrap();
    }

    #[test]
    fn ultra_hdr_preset_is_convert_only_and_requires_a_jpeg_suffix() {
        let cfg = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::UltraHdrV1,
                ..OutputParams::default()
            },
            ..base_cfg()
        };
        let mut args = parse_convert(&["--output-preset", "ultra-hdr-v1"]);
        assert!(validate_convert(&cfg, &args).is_err());
        args.output = PathBuf::from("out.JPEG");
        validate_convert(&cfg, &args).unwrap();
        let err = reject_roll_unsupported(&cfg).unwrap_err().to_string();
        assert!(err.contains("convert"), "{err}");
    }

    #[test]
    fn hdr_avif_presets_are_convert_only_and_require_an_avif_suffix() {
        for (preset, name) in [
            (OutputPreset::HdrPq, "hdr-pq"),
            (OutputPreset::HdrHlg, "hdr-hlg"),
        ] {
            let cfg = ResolvedConfig {
                output: OutputParams {
                    preset,
                    ..OutputParams::default()
                },
                ..base_cfg()
            };
            // A `.tiff` (or the default) path is rejected; `.avif` in any case passes.
            let mut args = parse_convert(&["--output-preset", name]);
            let err = validate_convert(&cfg, &args).unwrap_err().to_string();
            assert!(err.contains(".avif"), "{name}: {err}");
            assert!(err.contains(name), "{name}: {err}");
            args.output = PathBuf::from("out.AVIF");
            validate_convert(&cfg, &args).unwrap();

            // Roll mode names the preset it is refusing.
            let err = reject_roll_unsupported(&cfg).unwrap_err().to_string();
            assert!(err.contains("convert"), "{name}: {err}");
            assert!(err.contains(name), "{name}: {err}");

            // Atomic like every named preset: a legacy depth selector cannot ride
            // along, by flag presence *or* resolved value.
            let mut sdr = parse_convert(&["--output-preset", name, "--output-sdr"]);
            sdr.output = PathBuf::from("out.avif");
            assert!(
                validate_convert(&cfg, &sdr).is_err(),
                "{name} must reject --output-sdr"
            );
            let hdr_flag = ResolvedConfig {
                output: OutputParams {
                    preset,
                    hdr: true,
                    ..OutputParams::default()
                },
                ..base_cfg()
            };
            assert!(
                validate(&hdr_flag).is_err(),
                "{name} must reject a non-default output.hdr"
            );

            // The IR TIFF export resolves u16; the primary is fixed 10-bit AVIF.
            assert_eq!(cfg.output.depth(), crate::types::OutDepth::U16);
        }
    }

    #[test]
    fn coded_hdr_tiff_presets_are_convert_only_and_require_a_tiff_suffix() {
        for (preset, name) in [
            (OutputPreset::HdrPqTiff, "hdr-pq-tiff"),
            (OutputPreset::HdrHlgTiff, "hdr-hlg-tiff"),
        ] {
            let cfg = ResolvedConfig {
                output: OutputParams {
                    preset,
                    ..OutputParams::default()
                },
                ..base_cfg()
            };
            let mut args = parse_convert(&["--output-preset", name]);
            args.output = PathBuf::from("out.avif");
            let err = validate_convert(&cfg, &args).unwrap_err().to_string();
            assert!(err.contains(".tif"), "{name}: {err}");
            assert!(err.contains(name), "{name}: {err}");
            args.output = PathBuf::from("out.TIF");
            validate_convert(&cfg, &args).unwrap();

            // Convert-only, like every suffix-pinning preset.
            let err = reject_roll_unsupported(&cfg).unwrap_err().to_string();
            assert!(err.contains(name), "{name}: {err}");

            // Atomic: the legacy selectors cannot ride along.
            let mut sdr = parse_convert(&["--output-preset", name, "--output-sdr"]);
            sdr.output = PathBuf::from("out.tif");
            assert!(
                validate_convert(&cfg, &sdr).is_err(),
                "{name} must reject --output-sdr"
            );
            let hdr_flag = ResolvedConfig {
                output: OutputParams {
                    preset,
                    hdr: true,
                    ..OutputParams::default()
                },
                ..base_cfg()
            };
            assert!(
                validate(&hdr_flag).is_err(),
                "{name} must reject a non-default output.hdr"
            );
            // `--bigtiff on` is the third selector the atomic rule covers, and it is
            // stated here at the CLI level (through `merge`) rather than by poking a
            // resolved value, so the flag spelling is what the assertion pins.
            let big = merge(
                base_cfg(),
                &parse_convert(&["--output-preset", name, "--bigtiff", "on"]),
            )
            .unwrap();
            let err = validate(&big).unwrap_err().to_string();
            assert!(err.contains("output.bigtiff"), "{name}: {err}");
            // Control: `--bigtiff auto` is the documented default and asks the preset
            // for nothing, so it must still pass — otherwise the assertion above
            // would also hold for a rule that rejected the flag outright.
            let auto = merge(
                base_cfg(),
                &parse_convert(&["--output-preset", name, "--bigtiff", "auto"]),
            )
            .unwrap();
            validate(&auto).unwrap_or_else(|e| panic!("{name}: --bigtiff auto: {e}"));

            // These resolve u16 for the primary *and* the IR plane.
            assert_eq!(cfg.output.depth(), crate::types::OutDepth::U16);
            // And they render a Rec.2100 transfer — the property that makes them
            // share a rendition with the AVIF presets.
            assert!(hdr::transfer_for(preset).is_some(), "{name}");
        }

        // The distinction the suffix hides: `hdr-pq` and `hdr-pq-tiff` render the
        // same transfer into different containers, so the *container* rules must
        // differ while the transfer agrees.
        assert_eq!(
            hdr::transfer_for(OutputPreset::HdrPq),
            hdr::transfer_for(OutputPreset::HdrPqTiff)
        );
        assert_eq!(
            required_extensions(OutputPreset::HdrPq),
            Some(&["avif"][..])
        );
        assert_eq!(
            required_extensions(OutputPreset::HdrPqTiff),
            Some(&["tif", "tiff"][..])
        );
    }

    #[test]
    fn hdr_linear_tiff_is_convert_only_and_requires_a_tiff_suffix() {
        let cfg = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::HdrLinearTiff,
                ..OutputParams::default()
            },
            ..base_cfg()
        };

        // `.jpg` is rejected and the message names both the preset and what it wants.
        let mut args = parse_convert(&["--output-preset", "hdr-linear-tiff"]);
        args.output = PathBuf::from("out.jpg");
        let err = validate_convert(&cfg, &args).unwrap_err().to_string();
        assert!(err.contains(".tif"), "{err}");
        assert!(err.contains("hdr-linear-tiff"), "{err}");

        // Both spellings pass, in any case.
        for name in ["out.tif", "out.TIFF", "out.tiff"] {
            args.output = PathBuf::from(name);
            validate_convert(&cfg, &args)
                .unwrap_or_else(|e| panic!("{name} should be accepted: {e}"));
        }

        // Roll refuses it — and the reason must be the suffix contract, not a claim
        // that the container is not a TIFF (it is one).
        let err = reject_roll_unsupported(&cfg).unwrap_err().to_string();
        assert!(err.contains("convert"), "{err}");
        assert!(err.contains("hdr-linear-tiff"), "{err}");
        assert!(
            !err.contains("non-TIFF"),
            "the refusal must not claim this preset is not a TIFF: {err}"
        );

        // Atomic like every named preset. `--output-sdr` is rejected by presence
        // (it forces 16-bit integer output this preset cannot produce)...
        let mut sdr = parse_convert(&["--output-preset", "hdr-linear-tiff", "--output-sdr"]);
        sdr.output = PathBuf::from("out.tiff");
        assert!(
            validate_convert(&cfg, &sdr).is_err(),
            "must reject --output-sdr"
        );
        // ...and a non-default `output.hdr` / `output_profile` by resolved value.
        // `--output-hdr` is the *rendered* float TIFF in the selected output space,
        // which is a different image from display-linear BT.2020 — accepting it
        // would silently promise one and deliver the other.
        for offender in [
            OutputParams {
                preset: OutputPreset::HdrLinearTiff,
                hdr: true,
                ..OutputParams::default()
            },
            OutputParams {
                preset: OutputPreset::HdrLinearTiff,
                output_profile: Some("acescg".into()),
                ..OutputParams::default()
            },
        ] {
            let bad = ResolvedConfig {
                output: offender,
                ..base_cfg()
            };
            assert!(
                validate(&bad).is_err(),
                "must reject a non-default legacy selector: {:?}",
                bad.output
            );
        }
        // `--bigtiff on` is the third selector of that rule, stated at the CLI level
        // (through `merge`) so the flag spelling is what is pinned — with
        // `--bigtiff auto`, the documented default, as the falsifiable control.
        let big = merge(
            base_cfg(),
            &parse_convert(&["--output-preset", "hdr-linear-tiff", "--bigtiff", "on"]),
        )
        .unwrap();
        let err = validate(&big).unwrap_err().to_string();
        assert!(err.contains("output.bigtiff"), "{err}");
        let auto = merge(
            base_cfg(),
            &parse_convert(&["--output-preset", "hdr-linear-tiff", "--bigtiff", "auto"]),
        )
        .unwrap();
        validate(&auto).unwrap();

        // But a non-default `--linear-range` **is** accepted: unlike the legacy path
        // (whose frozen ordering never applies it) this preset genuinely consumes it
        // through the shared display stage, so rejecting it would refuse a knob that
        // works. This is the falsifiable half of that rule.
        let ranged = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::HdrLinearTiff,
                ..OutputParams::default()
            },
            print: PrintParams {
                linear_range: [0.05, 0.95],
                ..PrintParams::default()
            },
            ..base_cfg()
        };
        validate(&ranged).unwrap();
        // Control: the same range under the legacy path is still an error, so the
        // assertion above is proving preset-specific behaviour and not that the rule
        // stopped working altogether.
        let legacy_ranged = ResolvedConfig {
            print: ranged.print.clone(),
            ..base_cfg()
        };
        assert!(validate(&legacy_ranged).is_err());

        // The primary *and* the IR export are f32 here — the preset resolves depth
        // without consulting `output.hdr`.
        assert_eq!(cfg.output.depth(), crate::types::OutDepth::F32);

        // The transfer mapping is the single place the preset becomes a transfer.
        assert_eq!(
            hdr::transfer_for(OutputPreset::HdrPq),
            Some(hdr::HdrTransfer::Pq)
        );
        assert_eq!(
            hdr::transfer_for(OutputPreset::HdrHlg),
            Some(hdr::HdrTransfer::Hlg)
        );
        // `hdr-linear-tiff` belongs in this list and is the subtle member:
        // it *is* an HDR rendition and answers `None` only because it applies no
        // transfer at all, where the other three answer `None` for the opposite
        // reason — they are not HDR renditions. Both readings are "no transfer", so
        // the mapping must be pinned for it too or the interesting case is the one
        // nothing guards.
        for other in [
            OutputPreset::HdrLinearTiff,
            OutputPreset::Legacy,
            OutputPreset::FilmMaster,
            OutputPreset::UltraHdrV1,
        ] {
            assert_eq!(hdr::transfer_for(other), None, "{other:?}");
        }
    }

    #[test]
    fn film_master_rejects_frame_local_auto_dmax_and_pins_the_supported_anchors() {
        // Frame-local `auto` normalizes exposure per frame, which is exactly the
        // cross-frame consistency the master exists to preserve — so it is rejected
        // for *either* curve, from either source.
        for curve in [
            DensityCurve::Exponential(ExponentialParams {
                dmax: DmaxSource::Auto,
                ..ExponentialParams::default()
            }),
            DensityCurve::Sigmoid(SigmoidParams {
                dmax: DmaxSource::Auto,
                ..SigmoidParams::default()
            }),
        ] {
            let cfg = ResolvedConfig {
                reconstruction: Reconstruction::Density {
                    density: DensityParams::default(),
                    curve,
                },
                ..film_master_cfg()
            };
            let msg = validate_err(&cfg);
            assert!(msg.contains("auto"), "{curve:?}: {msg}");
            assert!(msg.contains("film-master"), "{curve:?}: {msg}");
            // …and the message points at the roll-fixed alternatives.
            assert!(msg.contains("--d-max"), "{curve:?}: {msg}");
        }

        // Supported placements, pinned by curve type (design-spec §5):
        //   exponential — fixed (default), explicit/roll, and `none` (unity);
        //   sigmoid     — fixed (default) and explicit/roll; `none` is rejected for
        //                 the S-curve regardless of preset, so it is not listed.
        for curve in [
            DensityCurve::Exponential(ExponentialParams::default()),
            DensityCurve::Exponential(ExponentialParams {
                dmax: DmaxSource::Explicit(1.64),
                ..ExponentialParams::default()
            }),
            DensityCurve::Exponential(ExponentialParams {
                dmax: DmaxSource::None,
                ..ExponentialParams::default()
            }),
            DensityCurve::Sigmoid(SigmoidParams::default()),
            DensityCurve::Sigmoid(SigmoidParams {
                dmax: DmaxSource::Explicit(1.64),
                ..SigmoidParams::default()
            }),
        ] {
            let cfg = ResolvedConfig {
                reconstruction: Reconstruction::Density {
                    density: DensityParams::default(),
                    curve,
                },
                ..film_master_cfg()
            };
            validate(&cfg).unwrap_or_else(|e| panic!("{curve:?} must be accepted: {e}"));
        }
        // …and the *un*supported placement stays rejected under the preset too:
        // `dmax = none` is invalid for the S-curve regardless of preset, so
        // "supported anchors pinned by curve type" is only half-pinned without it.
        // The rejection comes from the curve rule, which runs *before*
        // `validate_output_preset` — pin it here so a reordering that let the preset
        // path return first could not silently start accepting it.
        let msg = validate_err(&ResolvedConfig {
            reconstruction: Reconstruction::Density {
                density: DensityParams::default(),
                curve: DensityCurve::Sigmoid(SigmoidParams {
                    dmax: DmaxSource::None,
                    ..SigmoidParams::default()
                }),
            },
            ..film_master_cfg()
        });
        assert!(msg.contains("--no-d-max"), "{msg}");
        assert!(msg.contains("sigmoid"), "{msg}");

        // `simple` has no Dmax at all, so the master accepts it unconditionally.
        validate(&ResolvedConfig {
            reconstruction: Reconstruction::Simple,
            ..film_master_cfg()
        })
        .unwrap();
    }

    #[test]
    fn film_master_rejects_a_measured_balance_range_only_when_it_is_consulted() {
        // The other frame-local measurement, and the same cross-frame hazard as auto
        // Dmax: an `auto` regional-balance range is measured from *this* frame's density
        // percentiles, so two frames of a roll get different ramp anchors.
        //
        // But `regional_balance` short-circuits before measuring whenever the two
        // balances are equal — including the neutral default — so the default
        // `BalanceRange::Auto` is genuinely inert and must stay accepted, or every
        // default master would break.
        let master_with = |density: DensityParams| ResolvedConfig {
            reconstruction: Reconstruction::Density {
                density,
                curve: DensityCurve::default(),
            },
            ..film_master_cfg()
        };

        // Accepted: the default (Auto range, neutral balances) — the case that must not
        // regress — and an equal-but-non-neutral pair, which is a tone-independent
        // offset that consults no range.
        validate(&master_with(DensityParams::default())).unwrap();
        validate(&master_with(DensityParams {
            shadow_balance: [0.05, 0.0, -0.02],
            highlight_balance: [0.05, 0.0, -0.02],
            balance_range: BalanceRange::Auto,
            ..DensityParams::default()
        }))
        .unwrap();
        // Accepted: unequal balances with an *explicit* roll range — the recovery the
        // error message points at.
        validate(&master_with(DensityParams {
            shadow_balance: [0.1, 0.0, 0.0],
            highlight_balance: [0.0; 3],
            balance_range: BalanceRange::Explicit([0.2, 1.6]),
            ..DensityParams::default()
        }))
        .unwrap();

        // Rejected: unequal balances with the measured `Auto` range, whichever side is
        // set — this is the combination that was silently accepted before.
        for (name, shadow, highlight) in [
            ("shadow only", [0.1, 0.0, 0.0], [0.0; 3]),
            ("highlight only", [0.0; 3], [-0.05, 0.01, 0.0]),
            ("both unequal", [0.05, 0.0, -0.02], [-0.05, 0.01, 0.0]),
        ] {
            let msg = validate_err(&master_with(DensityParams {
                shadow_balance: shadow,
                highlight_balance: highlight,
                balance_range: BalanceRange::Auto,
                ..DensityParams::default()
            }));
            assert!(msg.contains("film-master"), "{name}: {msg}");
            assert!(msg.contains("--balance-range"), "{name}: {msg}");
            assert!(msg.contains("frame-local"), "{name}: {msg}");
        }
        // …and the same params without the preset are legal on the legacy path.
        validate(&ResolvedConfig {
            reconstruction: Reconstruction::Density {
                density: DensityParams {
                    shadow_balance: [0.1, 0.0, 0.0],
                    highlight_balance: [0.0; 3],
                    balance_range: BalanceRange::Auto,
                    ..DensityParams::default()
                },
                curve: DensityCurve::default(),
            },
            ..base_cfg()
        })
        .unwrap();
    }

    #[test]
    fn film_master_rejects_every_non_default_print_control() {
        // The master bypasses stage 4, so a requested adjustment must be a loud
        // error, never silently dropped. Exhaustive over the print struct — the
        // destructuring in `validate_output_preset` makes a newly added control fail
        // to compile there, and this test pins the behaviour for each existing one.
        let cases: [(&str, PrintParams); 5] = [
            (
                "print_exposure",
                PrintParams {
                    print_exposure: 0.5,
                    ..PrintParams::default()
                },
            ),
            (
                "black_point",
                PrintParams {
                    black_point: 0.01,
                    ..PrintParams::default()
                },
            ),
            (
                "white_balance",
                PrintParams {
                    white_balance: WbSource::Explicit([1.05, 1.0, 0.93]),
                    ..PrintParams::default()
                },
            ),
            (
                "white_balance",
                PrintParams {
                    white_balance: WbSource::Percentile,
                    ..PrintParams::default()
                },
            ),
            (
                "highlight_compress",
                PrintParams {
                    highlight_compress: 0.2,
                    ..PrintParams::default()
                },
            ),
        ];
        for (name, print) in cases {
            let cfg = ResolvedConfig {
                print,
                ..film_master_cfg()
            };
            let msg = validate_err(&cfg);
            assert!(msg.contains(name), "{name}: {msg}");
            assert!(msg.contains("film-master"), "{name}: {msg}");
            // The error must offer no ignore-conflicting-controls escape.
            assert!(msg.contains("custom"), "{name}: {msg}");
        }
        // The all-default print block is what the master requires.
        validate(&film_master_cfg()).unwrap();
    }

    #[test]
    fn named_preset_rejects_a_non_default_legacy_selector_from_either_provenance() {
        // A named preset is atomic: it resolves container/depth/profile itself, so a
        // non-default legacy selector is a loud error rather than a silent override.
        //
        // The rule is **resolved-value only** — there is deliberately no second check
        // by flag presence — so a flag and a recipe key must reach the *same* outcome
        // for the *same* resolved value. Every case below is therefore run through
        // `merge` + `validate` twice: once flag-sourced, once recipe-sourced.
        let outcome = |argv: &[&str], recipe_json: &str| -> std::result::Result<(), String> {
            let mut recipe: ResolvedConfig = serde_json::from_str(recipe_json).unwrap();
            // State a base so `validate` reaches the atomicity rule under test.
            recipe.film_base.source = Some(FilmBaseSource::Auto);
            let mut full = vec!["--output-preset", "film-master"];
            full.extend_from_slice(argv);
            let cfg = merge(recipe, &parse_convert(&full)).unwrap();
            validate(&cfg).map_err(|e| e.to_string())
        };

        // Non-default → rejected, from a flag and from the recipe alike, and the
        // message must blame the offending selector by name (a message listing all
        // three would make this assertion vacuous).
        for (key, flag, recipe) in [
            (
                "output.hdr",
                vec!["--output-hdr"],
                r#"{"output":{"hdr":true}}"#,
            ),
            (
                "output.output_profile",
                vec!["--output-profile", "srgb"],
                r#"{"output":{"output_profile":"srgb"}}"#,
            ),
            (
                "output.bigtiff",
                vec!["--bigtiff", "on"],
                r#"{"output":{"bigtiff":"on"}}"#,
            ),
        ] {
            for (provenance, msg) in [
                ("flag", outcome(&flag, "{}").unwrap_err()),
                ("recipe", outcome(&[], recipe).unwrap_err()),
            ] {
                assert!(msg.contains(key), "{key} via {provenance}: {msg}");
                assert!(msg.contains("atomic output policy"), "{provenance}: {msg}");
                // `--output-hdr` is a *rendered* float TIFF; the message must say so
                // rather than let a reader assume it is the same thing as the master.
                assert!(msg.contains("never an alias"), "{provenance}: {msg}");
                // Only the offender is blamed.
                for other in ["output.hdr", "output.output_profile", "output.bigtiff"] {
                    assert_eq!(
                        other == key,
                        msg.contains(other),
                        "{provenance}: {msg} must blame only {key}"
                    );
                }
            }
        }

        // A value that *already equals* the documented default is accepted, from both
        // provenances: `--bigtiff auto` means "decide for me" and a recipe `hdr: false`
        // is the serde default asserting nothing, so neither asks the preset for
        // anything it does not already do.
        //
        // `--output-sdr` is deliberately NOT in this list even though it also resolves
        // `hdr = false`: it *forces* 16-bit integer output, so it is rejected by
        // presence in `reject_output_sdr_with_named_preset` (which `validate` cannot
        // see, hence the separate test below).
        for (name, flag, recipe) in [
            (
                "bigtiff auto",
                vec!["--bigtiff", "auto"],
                r#"{"output":{"bigtiff":"auto"}}"#,
            ),
            ("hdr false", vec![], r#"{"output":{"hdr":false}}"#),
        ] {
            outcome(&flag, "{}").unwrap_or_else(|e| panic!("{name} flag must be accepted: {e}"));
            outcome(&[], recipe)
                .unwrap_or_else(|e| panic!("{name} recipe key must be accepted: {e}"));
        }

        // The same recipes without the preset stay perfectly legal (legacy path).
        let mut legacy: ResolvedConfig =
            serde_json::from_str(r#"{"output":{"hdr":true,"bigtiff":"on"}}"#).unwrap();
        legacy.film_base.source = Some(FilmBaseSource::Auto);
        validate(&legacy).unwrap();
        validate(&merge(base_cfg(), &parse_convert(&["--output-hdr"])).unwrap()).unwrap();
    }

    #[test]
    fn output_sdr_is_rejected_by_presence_next_to_a_named_preset() {
        // The one deliberate presence check, and the reason it is not a value check:
        // `--output-sdr` *forces* the default 16-bit integer TIFF (design-spec §9), which
        // a named preset cannot produce — so it is a contradicted request, not a
        // redundant one, even though its resolved value (`hdr = false`) IS the default.
        // Under the value rule alone it exited 0 and silently wrote an f32 master.
        let check = |argv: &[&str], recipe_json: &str| -> std::result::Result<(), String> {
            let recipe: ResolvedConfig = serde_json::from_str(recipe_json).unwrap();
            let args = parse_convert(argv);
            let cfg = merge(recipe, &args).unwrap();
            reject_output_sdr_with_named_preset(&cfg, &args).map_err(|e| e.to_string())
        };

        // Rejected next to a named preset, from either preset provenance — the flag is
        // the only thing checked by presence, so the *preset*'s source must not matter.
        for (name, argv, recipe) in [
            (
                "flag preset",
                vec!["--output-preset", "film-master", "--output-sdr"],
                "{}",
            ),
            (
                "recipe preset",
                vec!["--output-sdr"],
                r#"{"output":{"preset":"film-master"}}"#,
            ),
            (
                // The case that motivated the reversal: the user asked for 16-bit
                // twice and previously received f32 with no diagnostic.
                "recipe preset + recipe hdr:true",
                vec!["--output-sdr"],
                r#"{"output":{"preset":"film-master","hdr":true}}"#,
            ),
        ] {
            let msg = check(&argv, recipe).unwrap_err();
            assert!(msg.contains("--output-sdr"), "{name}: {msg}");
            assert!(msg.contains("film-master"), "{name}: {msg}");
            assert!(msg.contains("16-bit integer"), "{name}: {msg}");
        }

        // Accepted without a named preset — `--output-sdr` keeps its whole legacy job,
        // including resetting a recipe `hdr: true`.
        check(&["--output-sdr"], "{}").unwrap();
        check(&["--output-sdr"], r#"{"output":{"hdr":true}}"#).unwrap();
        check(
            &["--output-preset", "legacy", "--output-sdr"],
            r#"{"output":{"hdr":true}}"#,
        )
        .unwrap();
        // …and the flag's merge behaviour is untouched by the new rejection.
        assert!(
            !merge(
                serde_json::from_str(r#"{"output":{"hdr":true}}"#).unwrap(),
                &parse_convert(&["--output-sdr"])
            )
            .unwrap()
            .output
            .hdr
        );
        // Absent flag → nothing to reject, even under the preset.
        reject_output_sdr_with_named_preset(&film_master_cfg(), &parse_convert(&[])).unwrap();
    }

    #[test]
    fn roll_frame_override_of_output_preset_is_flagged_as_a_consistency_break() {
        // `output.preset` is the third roll-fixed choice, alongside `film_base` and
        // `reconstruction.curve.dmax`, and the coarsest: it changes which branch out of
        // the ACEScg boundary a frame takes, so the frame is a different image class.
        // It was the only one of the three that warned about nothing.
        let probe = |json: &str| sets_output_preset(&serde_json::from_str(json).unwrap());
        assert!(probe(r#"{"output":{"preset":"legacy"}}"#));
        assert!(probe(r#"{"output":{"preset":"film-master"}}"#));
        // A raw-JSON probe like `sets_curve_dmax`: an override that merely *restates*
        // the shared preset is still a per-frame assertion, and the roll report has
        // nowhere else to surface it (`FrameStatus` carries no `output_render`).
        assert!(!probe(r#"{"output":{"hdr":true}}"#));
        assert!(!probe(r#"{"print":{"print_exposure":0.5}}"#));
        assert!(!probe(r#"{}"#));
    }

    #[test]
    fn film_master_accepts_a_recipe_whose_controls_a_flag_resets_to_default() {
        // The rejection is on the *resolved* value, so flags-win semantics stay
        // usable: a roll recipe carrying print controls can be re-exported as a
        // master by resetting them on the command line, without editing the recipe.
        let mut recipe: ResolvedConfig = serde_json::from_str(
            r#"{"print":{"print_exposure":0.5,"white_balance":{"explicit":[1.05,1.0,0.93]}}}"#,
        )
        .unwrap();
        // State a base so the accepted case reaches the print-control rule under
        // test rather than the film-base requirement.
        recipe.film_base.source = Some(FilmBaseSource::Auto);
        // Without the resets the master rejects it…
        let cfg = merge(
            recipe.clone(),
            &parse_convert(&["--output-preset", "film-master"]),
        )
        .unwrap();
        assert!(validate(&cfg).is_err());
        // …and with them it is accepted.
        let cfg = merge(
            recipe,
            &parse_convert(&[
                "--output-preset",
                "film-master",
                "--print-exposure",
                "0",
                "--white-balance",
                "1,1,1",
            ]),
        )
        .unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.output.preset, OutputPreset::FilmMaster);
    }

    #[test]
    fn output_render_result_serializes_the_documented_shapes() {
        let value = |cfg: &ResolvedConfig| serde_json::to_value(output_render_result(cfg)).unwrap();

        // `film-master`: no print controls, no display render, unclamped linear
        // ACEScg, and an explicit disclaimer of physical scene recovery.
        let master = value(&film_master_cfg());
        assert_eq!(master["preset"], "film-master");
        assert_eq!(master["print_controls"], false);
        assert_eq!(master["display_render"], false);
        assert_eq!(master["encoding"], "unclamped-linear-acescg-float-tiff");
        assert_eq!(master["working_mapping"], "nc-film-rgb-v1");
        assert_eq!(master["reconstruction_schema_version"], 1);
        let content = master["content"].as_str().unwrap();
        assert!(content.contains("intentional film rendering"), "{content}");
        assert!(content.contains("not a physical scene-linear"), "{content}");
        // The unreleased pre-rename name must appear nowhere in the report.
        assert!(!master.to_string().contains("scene-master"));
        // The block's exact key set — which pins that `pipeline_version` is absent
        // (owned by `core/conversion-versioning`, deliberately not guessed here)
        // *and* fails if any other field is added without updating this contract.
        // Asserting `get("pipeline_version").is_none()` alone could never fail: the
        // struct does not declare the field.
        let mut keys: Vec<&str> = master
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "content",
                "display_render",
                "encoding",
                "preset",
                "print_controls",
                "reconstruction_schema_version",
                "working_mapping",
            ]
        );

        // The master's content claim must not invent a Dmax placement it did not make:
        // validation deliberately accepts exponential `dmax = none`, and `simple` has no
        // anchor at all.
        for (name, reconstruction) in [
            ("simple", Reconstruction::Simple),
            (
                "exponential dmax=none",
                Reconstruction::Density {
                    density: DensityParams::default(),
                    curve: DensityCurve::Exponential(ExponentialParams {
                        dmax: DmaxSource::None,
                        ..ExponentialParams::default()
                    }),
                },
            ),
        ] {
            let anchorless = value(&ResolvedConfig {
                reconstruction,
                ..film_master_cfg()
            });
            let content = anchorless["content"].as_str().unwrap();
            assert!(
                content.contains("placed no Dmax anchor"),
                "{name}: {content}"
            );
            assert!(!content.contains("roll-fixed Dmax"), "{name}: {content}");
            assert!(
                content.contains("not a physical scene-linear"),
                "{name}: {content}"
            );
        }
        // …and the anchored default does claim it.
        assert!(
            content.contains("resolved roll-fixed Dmax"),
            "the default fixed anchor must be claimed: {content}"
        );

        // legacy u16 (the default): the print stage runs for a density
        // reconstruction and the working→output ICC transform always runs.
        let legacy = value(&base_cfg());
        assert_eq!(legacy["preset"], "legacy");
        assert_eq!(legacy["print_controls"], true);
        assert_eq!(legacy["display_render"], true);
        assert_eq!(legacy["encoding"], "rendered-u16-tiff");

        // legacy `--output-hdr`: still a *rendered* float TIFF — the identifier must
        // not read like a master.
        let hdr = value(&ResolvedConfig {
            output: OutputParams {
                hdr: true,
                ..OutputParams::default()
            },
            ..base_cfg()
        });
        assert_eq!(hdr["encoding"], "transitional-rendered-float-tiff");
        assert!(
            hdr["content"]
                .as_str()
                .unwrap()
                .contains("not a film master")
        );

        // legacy `simple`: its positive passes through with no print stage.
        assert_eq!(value(&simple_cfg())["print_controls"], false);
    }

    #[test]
    fn removed_simple_flags_name_their_replacement_print_controls() {
        // In *this* build the removed simple controls are rejections, not the warned
        // aliases design-spec §7.1/§9 specify: `ultra-hdr-v1` consumes the replacement
        // controls, but alias activation remains tied to the complete output/presets
        // migration. The message must name the concrete
        // replacement, which exists, and must not promise identical pixels —
        // per-channel gains and an affine range placement do not commute with the
        // working-space matrix.
        for (flag, value, replacement) in [
            ("--invert-white-balance", "1.05,1,0.93", "--white-balance"),
            ("--clip-low", "0.02", "--linear-range"),
            ("--clip-high", "0.97", "--linear-range"),
        ] {
            let err = reject_removed_flags(&parse_convert(&[flag, value])).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(flag), "{flag}: {msg}");
            assert!(msg.contains(replacement), "{flag}: {msg}");
            // …and must not promise identical pixels.
            assert!(msg.contains("not bit-identical"), "{flag}: {msg}");
        }
    }

    #[test]
    fn help_uses_film_master_and_never_the_pre_release_name() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("convert")
            .expect("convert subcommand")
            .render_long_help()
            .to_string();
        assert!(help.contains("--output-preset"), "{help}");
        assert!(help.contains("film-master"), "{help}");
        assert!(help.contains("--linear-range"), "{help}");
        assert!(
            !help.contains("scene-master"),
            "the pre-release name must not appear in help"
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
    fn params_default_is_parseable_json_but_no_longer_runnable() {
        // The subject is the exact document `nc params` prints — `run_params`
        // serializes `ResolvedConfig::default()` — so this must stay on the real
        // default, not on a film-base-stated stand-in. Substituting `base_cfg()`
        // here would leave nothing asserting that the printed scaffold round-trips.
        let json = serde_json::to_string_pretty(&ResolvedConfig::default()).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ResolvedConfig::default());

        // ...and the scaffold is deliberately NOT runnable as printed: it states no
        // film base, so `validate` rejects it. That is the new contract — `nc
        // params` emits a template to edit, not a recipe to run — and pinning it
        // here is what stops a future default from quietly making it runnable again.
        let msg = match validate(&back) {
            Err(NcError::Usage(m)) => m,
            other => panic!(
                "the printed default scaffold must be rejected until a film base is \
                 stated, got {other:?}"
            ),
        };
        assert!(msg.contains("film_base.source"), "{msg}");

        // Falsifiable control: the same document with a base stated does validate,
        // so the rejection above is about the film base and nothing else.
        let mut runnable = back.clone();
        runnable.film_base.source = Some(FilmBaseSource::Auto);
        validate(&runnable).unwrap();
    }

    #[test]
    fn validate_requires_a_stated_film_base() {
        // `film_base.source` has no default: `Dmin` is the divisor of the density
        // conversion, so falling into auto-detection by omission decided the most
        // consequential parameter for the user. All three stated forms are fine —
        // including `auto`, which is what this used to default to. The rule is
        // that the choice is *made*, not that it is explicit.
        let unstated = ResolvedConfig::default();
        assert_eq!(unstated.film_base.source, None, "there must be no default");
        let msg = match validate(&unstated) {
            Err(NcError::Usage(m)) => m,
            other => panic!("an unstated film base must be a usage error, got {other:?}"),
        };
        // The message has to be actionable: name every way out, since a user who
        // hit this has no idea which of the three they wanted.
        for expected in [
            "--film-base",
            "--base-region",
            "--auto-base",
            "film_base.source",
        ] {
            assert!(msg.contains(expected), "{expected} missing from: {msg}");
        }

        for stated in [
            FilmBaseSource::Auto,
            FilmBaseSource::Region([0, 0, 100, 40]),
            FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
        ] {
            let mut cfg = ResolvedConfig::default();
            cfg.film_base.source = Some(stated.clone());
            validate(&cfg)
                .unwrap_or_else(|e| panic!("a stated {stated:?} base must be accepted: {e}"));
        }
    }

    #[test]
    fn roll_is_told_about_the_shared_recipe_rather_than_flags_it_rejects() {
        // Same rule, different remedy: `RollArgs` flattens only `MemoryArgs` /
        // `ReportArgs`, so every film-base flag exits 2 on `roll`. Naming them
        // would be advice the user cannot follow.
        let unstated = ResolvedConfig::default();
        let msg = match validate_with_remedy(&unstated, FilmBaseRemedy::SharedRecipe) {
            Err(NcError::Usage(m)) => m,
            other => panic!("an unstated film base must be a usage error, got {other:?}"),
        };
        assert!(msg.contains("--params"), "{msg}");
        assert!(msg.contains("film_base.source"), "{msg}");
        for absent in ["--auto-base", "--film-base"] {
            assert!(!msg.contains(absent), "{absent} must not be offered: {msg}");
        }
        // `--base-region` *does* appear — but only inside the `nc estimate`
        // invocation the message recommends, which is a different command and does
        // accept it. What must never appear is a `roll` flag.
        assert!(
            msg.matches("--base-region")
                .count()
                .eq(&msg.matches("nc estimate --base-region").count()),
            "--base-region may only appear as an argument of `nc estimate`: {msg}"
        );
        // The *requirement* is remedy-independent — only the wording moves.
        let mut stated = ResolvedConfig::default();
        stated.film_base.source = Some(FilmBaseSource::Auto);
        validate_with_remedy(&stated, FilmBaseRemedy::SharedRecipe).unwrap();
        // And `convert_frame`'s totality guard shares the same two spellings, so
        // the unreachable restatement cannot drift from the gate's.
        assert_eq!(
            missing_film_base_message(FilmBaseRemedy::for_command("roll")),
            missing_film_base_message(FilmBaseRemedy::SharedRecipe)
        );
        assert_eq!(
            missing_film_base_message(FilmBaseRemedy::for_command("convert")),
            missing_film_base_message(FilmBaseRemedy::Flags)
        );
    }

    #[test]
    fn the_missing_base_rule_never_pre_empts_a_more_specific_one() {
        // `validate`'s documented principle is flag-shape first: a config that both
        // contradicts itself and states no base must be told about the
        // contradiction, which names the two things the user actually typed.
        // Placing the `None` arm first (its original position) reversed that for
        // every rule in the function.
        let mut contradictory = ResolvedConfig::default();
        contradictory.output.preset = OutputPreset::FilmMaster;
        contradictory.print.linear_range = [0.05, 0.95];
        assert_eq!(contradictory.film_base.source, None);
        let msg = validate(&contradictory).unwrap_err().to_string();
        assert!(
            msg.contains("linear_range"),
            "the contradiction must be diagnosed ahead of the missing base: {msg}"
        );

        // Falsifiable control: with the contradiction removed, the same config does
        // report the missing base — so the assertion above is about ordering, not
        // about the missing-base rule having stopped working.
        let mut only_unstated = ResolvedConfig::default();
        only_unstated.output.preset = OutputPreset::FilmMaster;
        assert!(
            validate(&only_unstated)
                .unwrap_err()
                .to_string()
                .contains("no film base selected")
        );
        only_unstated.film_base.source = Some(FilmBaseSource::Auto);
        validate(&only_unstated).unwrap();
    }

    #[test]
    fn every_v1_film_base_spelling_still_deserializes() {
        // Back-compat for archived recipes and sidecars: `film_base.source` became
        // an `Option`, and every recipe ever written spells one of these three. If
        // any stopped deserializing to `Some(..)`, every archived recipe would exit
        // 2 instead of replaying. The `= Some(..)` pokes elsewhere in this module
        // bypass serde entirely, so nothing else pins the wire format.
        for (json, want) in [
            (r#"{"film_base":{"source":"auto"}}"#, FilmBaseSource::Auto),
            (
                r#"{"film_base":{"source":{"region":[10,20,30,40]}}}"#,
                FilmBaseSource::Region([10, 20, 30, 40]),
            ),
            (
                r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}}}"#,
                FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
            ),
        ] {
            let cfg: ResolvedConfig = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("a v1 recipe spelling must still parse: {json}: {e}"));
            assert_eq!(cfg.film_base.source, Some(want), "{json}");
            validate(&cfg).unwrap_or_else(|e| panic!("{json} must still validate: {e}"));
        }

        // Falsifiable control: omitting the key is the one thing that now yields
        // `None` — which is the whole point of the change.
        let omitted: ResolvedConfig = serde_json::from_str(r#"{"film_base":{}}"#).unwrap();
        assert_eq!(omitted.film_base.source, None);
        assert!(validate(&omitted).is_err());
    }

    #[test]
    fn auto_base_flag_states_the_source_rather_than_relying_on_a_default() {
        // The flag is the migration path for anyone who *wanted* detection: it
        // resolves to exactly the source that used to be implicit.
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--auto-base"])).unwrap();
        assert_eq!(cfg.film_base.source, Some(FilmBaseSource::Auto));
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_bad_params() {
        // Exponential gamma must be positive.
        let cfg = exponential_cfg(ExponentialParams {
            gamma: 0.0,
            dmax: DmaxSource::Fixed,
        });
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = base_cfg();
        cfg.print.white_balance = WbSource::Explicit([1.0, f32::NAN, 1.0]);
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // Non-positive explicit gains are rejected too (a recipe can smuggle
        // them past the CLI value parser).
        let mut cfg = base_cfg();
        cfg.print.white_balance = WbSource::Explicit([1.0, 0.0, 1.0]);
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // Negative highlight compression is rejected (the print render silently
        // treats it as "off", so a wrong-sign value must fail loudly, not no-op).
        let mut cfg = base_cfg();
        cfg.print.highlight_compress = -0.3;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        // Zero is valid (disables the roll-off).
        let mut cfg = base_cfg();
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
        validate(&base_cfg()).unwrap();
        validate(&simple_cfg()).unwrap();
    }

    #[test]
    fn validate_rejects_recipe_smuggled_bad_values() {
        // A recipe can carry values the CLI value-parsers would have rejected,
        // so validate is the only guard for these once they're in the config.
        let mut cfg = base_cfg();
        cfg.film_base.source = Some(FilmBaseSource::Explicit([0.9, 0.0, 0.4])); // zero transmission
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = base_cfg();
        cfg.film_base.source = Some(FilmBaseSource::Explicit([0.9, 90.0, 0.4])); // "90" typo for "0.90"
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        let mut cfg = base_cfg();
        cfg.film_base.source = Some(FilmBaseSource::Explicit([1.0, 1.0, 1.0])); // 1.0 exactly is valid
        validate(&cfg).unwrap();

        let mut cfg = base_cfg();
        cfg.film_base.source = Some(FilmBaseSource::Region([0, 0, 0, 0])); // zero-area region
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
    }

    #[test]
    fn export_ir_and_seed_parse_into_the_right_homes() {
        // `--export-ir` is an input/decode key (design-spec §9), not output.
        let cfg = merge(base_cfg(), &parse_convert(&["--export-ir", "ir.tiff"])).unwrap();
        assert_eq!(cfg.input.export_ir.as_deref(), Some("ir.tiff"));

        // The reserved `--seed` flag parses rather than being rejected by clap.
        let args = parse_convert(&["--seed", "42"]);
        assert_eq!(args.seed, Some(42));
    }

    #[test]
    fn merge_keeps_recipe_source_until_a_flag_replaces_it() {
        // No flag → the recipe's mutually-exclusive choice survives.
        let mut recipe = base_cfg();
        recipe.film_base.source = Some(FilmBaseSource::Explicit([0.9, 0.5, 0.4]));
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(
            cfg.film_base.source,
            Some(FilmBaseSource::Explicit([0.9, 0.5, 0.4]))
        );

        // A flag replaces the whole source — no field is left behind to win on
        // precedence (the #5/#6 fix). `--base-region` beats a recipe explicit base.
        let cfg = merge(recipe, &parse_convert(&["--base-region", "0,0,100,40"])).unwrap();
        assert_eq!(
            cfg.film_base.source,
            Some(FilmBaseSource::Region([0, 0, 100, 40]))
        );
    }

    #[test]
    fn input_axes_merge_independently_and_flags_win() {
        // transfer and meaning are independent axes: a flag on one axis replaces
        // that axis and leaves the other at the recipe value (flags win per axis).
        let mut recipe = base_cfg();
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
    fn merge_film_type_flag_overrides_recipe_else_keeps_recipe() {
        // `--film-type` maps to `input.film_type`; the flag replaces a recipe
        // value, and its absence never clobbers one (a forgotten merge arm would
        // silently make the flag a no-op — the four-spot knob rule).
        let mut recipe = base_cfg();
        recipe.input.film_type = FilmType::Silver;

        // No flag → the recipe value survives.
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(cfg.input.film_type, FilmType::Silver);

        // The flag wins over the recipe.
        let cfg = merge(recipe, &parse_convert(&["--film-type", "chromogenic"])).unwrap();
        assert_eq!(cfg.input.film_type, FilmType::Chromogenic);

        // Over the default recipe, the flag sets the declared type.
        let cfg = merge(base_cfg(), &parse_convert(&["--film-type", "chromogenic"])).unwrap();
        assert_eq!(cfg.input.film_type, FilmType::Chromogenic);
        // ...and the untouched default is `unknown` (the safe off state).
        let cfg = merge(base_cfg(), &parse_convert(&[])).unwrap();
        assert_eq!(cfg.input.film_type, FilmType::Unknown);
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
            source: Some(FilmBaseSource::Explicit([0.553, 0.271, 0.159])),
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
        assert_eq!(fragment.source, Some(FilmBaseSource::Explicit(rgb)));
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
            let mut recipe: ResolvedConfig = serde_json::from_str(&format!(
                r#"{{"reconstruction":{{"curve":{{"type":"{curve_type}","dmax":{{"explicit":1.2734}}}}}}}}"#
            ))
            .unwrap();
            // The fragment under test is the `dmax` one; state a base so
            // `validate` reaches it (`film_base.source` has no default).
            recipe.film_base.source = Some(FilmBaseSource::Auto);
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
                    source: Some(FilmBaseSource::Explicit([0.5, 0.25, 0.125])),
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
            Some(FilmBaseSource::Explicit([0.553, 0.271, 0.159]))
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

    /// Write `body` to a temp recipe, load it, clean up, return the result.
    fn load_recipe_body(tag: &str, body: &str) -> Result<LoadedRecipe> {
        let p = std::env::temp_dir().join(format!("nc-env-{tag}-{}.json", std::process::id()));
        std::fs::write(&p, body).unwrap();
        let got = load_recipe(Some(&p));
        std::fs::remove_file(&p).ok();
        got
    }

    #[test]
    fn load_recipe_accepts_the_envelope_and_the_bare_legacy_shape() {
        // The round-trip contract of `core/conversion-versioning`: identity lives in
        // a `meta` envelope beside the recipe, so BOTH the new sidecar and every
        // pre-existing bare recipe load — and to the *same* config.
        let bare = r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.8}}}"#;
        let enveloped = format!(
            r#"{{"meta":{{"nc_version":"0.1.0","pipeline_version":7,
                          "params_hash":"0123456789abcdef","git_commit":"abc"}},
                "params":{bare}}}"#
        );

        let flat = load_recipe_body("bare", bare).unwrap();
        let wrapped = load_recipe_body("env", &enveloped).unwrap();
        assert_eq!(flat.cfg, wrapped.cfg, "both shapes resolve to one config");
        assert_eq!(gamma_of(&wrapped.cfg), 1.8);
        // A bare recipe records no provenance; the envelope's is read but never
        // applied — only compared (see `pipeline_version_warning`).
        assert_eq!(flat.meta_pipeline_version, None);
        assert_eq!(wrapped.meta_pipeline_version, Some(7));
        // The `curve.dmax` witness is computed from the recipe *body* either way.
        assert!(!wrapped.curve_dmax_present);
        let with_dmax = load_recipe_body(
            "env-dmax",
            r#"{"meta":{},"params":{"reconstruction":{"curve":{"type":"exponential",
                "dmax":{"explicit":1.6}}}}}"#,
        )
        .unwrap();
        assert!(
            with_dmax.curve_dmax_present,
            "an enveloped recipe's dmax must still be witnessed"
        );
    }

    #[test]
    fn envelope_errors_are_loud_and_specific() {
        // `meta` with no `params` is a half-written envelope, not a bare recipe:
        // it must say so rather than emit serde's opaque `unknown field 'meta'`.
        let err = load_recipe_body("half", r#"{"meta":{"pipeline_version":1}}"#).unwrap_err();
        assert!(
            matches!(&err, NcError::Usage(m) if m.contains("`meta` block but no `params`")),
            "got {err}"
        );
        // A third sibling key alongside meta/params is rejected, not ignored — the
        // envelope is `deny_unknown_fields` too.
        assert!(matches!(
            load_recipe_body("extra", r#"{"meta":{},"params":{},"surprise":1}"#),
            Err(NcError::Usage(_))
        ));
        // Legacy-key migration errors still fire on an enveloped body.
        assert!(matches!(
            load_recipe_body("legacy", r#"{"meta":{},"params":{"algorithm":"density"}}"#),
            Err(NcError::Usage(_))
        ));
    }

    #[test]
    fn pipeline_version_warning_fires_only_on_a_real_mismatch() {
        // No recorded version (a bare/legacy recipe) ⇒ nothing to compare, no noise.
        assert_eq!(pipeline_version_warning(None), None);
        // The current version ⇒ no warning.
        assert_eq!(
            pipeline_version_warning(Some(version::PIPELINE_VERSION)),
            None
        );
        // Any other version ⇒ a warning naming both numbers, so the operator can
        // see which direction the skew runs.
        let other = version::PIPELINE_VERSION.wrapping_add(1);
        let msg = pipeline_version_warning(Some(other)).expect("mismatch must warn");
        assert!(msg.contains(&format!("pipeline_version {other}")), "{msg}");
        assert!(
            msg.contains(&format!("pipeline_version {}", version::PIPELINE_VERSION)),
            "{msg}"
        );
    }

    #[test]
    fn canonical_params_json_round_trips_back_to_the_same_config() {
        // The hash identifies a recipe an agent can *re-apply*, so the canonical
        // document must reload to the identical config — not merely be some
        // serialization of it. (The byte-level "this is what --dump-params writes"
        // claim is pinned end-to-end by
        // `params_hash_is_the_hash_of_the_dump_params_bytes`; asserting it here
        // against `to_string_pretty` would only restate this function's own body.)
        let cfg = base_cfg();
        let json = canonical_params_json(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<ResolvedConfig>(&json).unwrap(), cfg);
    }

    #[test]
    fn params_and_meta_are_not_recipe_keys() {
        // `params` is the reserved discriminator that tells an envelope from a bare
        // recipe, and `meta` its sibling. If a future stage section ever claimed
        // either name, every recipe carrying it would be silently reinterpreted as
        // an envelope (or rejected), so pin that the resolved recipe's own top level
        // never uses them.
        let value = serde_json::to_value(base_cfg()).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        for reserved in ["params", "meta"] {
            assert!(
                !keys.iter().any(|k| k.as_str() == reserved),
                "`{reserved}` is reserved for the sidecar envelope but is now a recipe key: \
                 {keys:?}"
            );
        }
    }

    #[test]
    fn a_malformed_meta_container_is_as_loud_as_a_malformed_field() {
        // The guard was on the *field* but not its *container*: `Value::get` on a
        // non-object returns `None`, which this path read as "records no
        // pipeline_version" — indistinguishable from a bare legacy recipe. So a
        // sidecar whose whole `meta` block was corrupt replayed with NO skew check,
        // while a corrupt field inside a well-formed `meta` was a loud exit 2.
        for body in [
            r#"{"meta":null,"params":{}}"#,
            r#"{"meta":"x","params":{}}"#,
            r#"{"meta":[],"params":{}}"#,
            r#"{"meta":123,"params":{}}"#,
            r#"{"meta":true,"params":{}}"#,
        ] {
            let err = load_recipe_body("bad-meta", body).unwrap_err();
            assert!(
                matches!(&err, NcError::Usage(m) if m.contains("`meta` must be an object")),
                "{body}: got {err}"
            );
        }
        // An OMITTED `meta` stays legal — a hand-wrapped `--dump-params` recipe has no
        // provenance to record, and that is not a malformed envelope.
        assert_eq!(
            load_recipe_body("no-meta", r#"{"params":{}}"#)
                .unwrap()
                .meta_pipeline_version,
            None
        );
        // An empty `meta` object is legal too, and records nothing.
        assert_eq!(
            load_recipe_body("empty-meta", r#"{"meta":{},"params":{}}"#)
                .unwrap()
                .meta_pipeline_version,
            None
        );
        // Unknown fields inside a well-formed `meta` stay lenient — that leniency is
        // the forward-compatibility contract, not an oversight.
        assert_eq!(
            load_recipe_body(
                "future-meta",
                r#"{"meta":{"invented":[1],"pipeline_version":7},"params":{}}"#
            )
            .unwrap()
            .meta_pipeline_version,
            Some(7)
        );
    }

    #[test]
    fn meta_pipeline_version_rejects_values_it_cannot_read() {
        // Present-but-unreadable must be LOUD. Mapped to `None` it would be
        // indistinguishable from "this file records no version" and would silently
        // disable the skew warning; truncated with `as u32` it can even land on this
        // build's version and pretend to agree.
        let ok = serde_json::json!({"pipeline_version": 7});
        assert_eq!(meta_pipeline_version(Some(&ok), "ctx").unwrap(), Some(7));
        // Absent (whole meta, or just the key) ⇒ genuinely nothing recorded.
        assert_eq!(meta_pipeline_version(None, "ctx").unwrap(), None);
        let empty = serde_json::json!({});
        assert_eq!(meta_pipeline_version(Some(&empty), "ctx").unwrap(), None);

        for bad in [
            serde_json::json!({"pipeline_version": 1.0}),
            serde_json::json!({"pipeline_version": "1"}),
            serde_json::json!({"pipeline_version": -1}),
            serde_json::json!({"pipeline_version": null}),
            // u32::MAX + 2 — `as u32` would truncate this to 1, matching a build at
            // pipeline_version 1 and suppressing the warning entirely.
            serde_json::json!({"pipeline_version": 4294967297u64}),
        ] {
            assert!(
                matches!(
                    meta_pipeline_version(Some(&bad), "ctx"),
                    Err(NcError::Usage(_))
                ),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_non_object_recipe_body_is_refused_instead_of_silently_defaulting() {
        // serde accepts a sequence for a struct and every `ResolvedConfig` field has
        // a default, so both of these used to convert with ALL-DEFAULT parameters and
        // a params_hash identical to the default recipe's — a truncated sidecar
        // quietly ignoring the recipe the operator thinks is applied.
        for (tag, body) in [
            ("arr-envelope", r#"{"params": []}"#),
            ("arr-bare", "[]"),
            ("num-envelope", r#"{"params": 3}"#),
            ("str-bare", r#""nope""#),
        ] {
            let err = load_recipe_body(tag, body).unwrap_err();
            assert!(
                matches!(&err, NcError::Usage(m) if m.contains("must be a")),
                "{tag}: got {err}"
            );
        }
        // An empty object stays valid on both levels — that is a recipe (or an
        // envelope body) that legitimately means "all defaults".
        // Note this compares against the *bare* default: a recipe that says
        // nothing leaves `film_base.source` unset (`None`), which `validate`
        // later rejects for `convert`. "All defaults" is not the same as "ready
        // to run" any more, and that is the point of the requirement.
        assert_eq!(
            load_recipe_body("obj-bare", "{}").unwrap().cfg,
            ResolvedConfig::default()
        );
        assert_eq!(
            load_recipe_body("obj-envelope", r#"{"params": {}}"#)
                .unwrap()
                .cfg,
            ResolvedConfig::default()
        );
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
        let mut base = serde_json::to_value(base_cfg()).unwrap();
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
            memory: MemoryArgs::default(),
            report: ReportArgs::default(),
        };
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.8,
            dmax: DmaxSource::Explicit(1.6),
        });
        shared.film_base.source = Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]));
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
            memory: MemoryArgs::default(),
            report: ReportArgs::default(),
        };
        let shared = ResolvedConfig {
            film_base: FilmBaseParams {
                source: Some(FilmBaseSource::Region([10, 10, 20, 20])),
            },
            ..base_cfg()
        };
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let planned = resolve_frames(&args, &shared, false, &mut warnings, &log);
        std::fs::remove_dir_all(&dir).ok();
        let planned = planned.expect("region→explicit override should apply, not error");
        assert_eq!(planned.len(), 1);
        assert_eq!(
            planned[0].cfg.film_base.source,
            Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]))
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
        shared.film_base.source = Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]));
        let mut v = serde_json::to_value(&shared).unwrap();
        let ov: serde_json::Value =
            serde_json::from_str(r#"{"print":{"print_exposure":0.15}}"#).unwrap();
        merge_json(&mut v, &ov);
        let cfg: ResolvedConfig = serde_json::from_value(v).unwrap();
        assert_eq!(cfg.print.print_exposure, 0.15);
        assert_eq!(
            cfg.film_base.source,
            Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]))
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
            memory: MemoryArgs::default(),
            report: ReportArgs::default(),
        };
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let got = resolve_frames(&args, &base_cfg(), false, &mut warnings, &log);
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
        let mut cfg = base_cfg();
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
        shared.film_base.source = Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]));
        let roll = RollReport {
            command: "roll",
            identity: Identity::with_params_hash(version::stable_hash("{}")),
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
                    output_stats: Some(OutputStats {
                        mean: [0.25, 0.5, 0.75],
                    }),
                    identity: Some(Identity::with_params_hash(version::stable_hash("frame"))),
                },
                memory: None,
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
    fn failed_frame_report_keeps_accumulated_warnings_and_the_preflight_decision() {
        // A frame that warned and got sized before failing still carries both in its
        // report entry (neither is reset on the failure path). The memory block lives
        // on `FrameReport`, not on the `Ok` payload, precisely so a frame that passed
        // the gate and then failed doesn't throw the estimate away.
        let pf = PlannedFrame {
            input: PathBuf::from("bad.tif"),
            output: PathBuf::from("out/bad_positive.tiff"),
            cfg: base_cfg(),
            overrides: None,
            dmax_setting: DmaxSetting::Default,
        };
        let warnings = vec!["a warning raised before the failure".to_string()];
        let mem = memory::preflight(
            &crate::io::decode::ImageShape::new(1000, 1000, 3, 16, true).unwrap(),
            RunProfile::DecodeOnly,
            SamplePlan::auto(),
            memory::Budget::resolve(None),
            None,
        )
        .unwrap();
        let fr = frame_report_err(&pf, &NcError::Decode("boom".into()), Some(mem), warnings);
        let v = serde_json::to_value(&fr).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["error"], "decode: boom");
        assert_eq!(
            v["warnings"][0], "a warning raised before the failure",
            "a failed frame must keep the warnings accumulated before it failed: {v}"
        );
        assert_eq!(
            v["memory"]["estimated_peak_bytes"], mem.estimate.estimated_peak_bytes,
            "a failed frame must keep the preflight decision: {v}"
        );
    }

    #[test]
    fn ram_pressure_warning_reaches_the_report_and_strict() {
        // The warn tier is the one environment-dependent piece of the gate — and,
        // via `--strict`, the one way "same input + params ⇒ same exit" can break.
        // Drive it through the real wiring by injecting the RAM figure (there is
        // deliberately no env override): a machine small enough that the fixture's
        // estimate exceeds 70% of RAM must produce a report warning, which is what
        // `--strict` promotes to a failing exit.
        let log = Log::new(&ReportArgs::default());
        let input = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hdri-64bit.tif");
        let budget = memory::Budget::resolve(None);

        // Enough RAM: no warning.
        let mut warnings = Vec::new();
        let quiet = preflight_memory(
            &input,
            RunProfile::DecodeOnly,
            SamplePlan::auto(),
            budget,
            Some(64 * 1024 * 1024 * 1024),
            &log,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(quiet.decision, memory::Verdict::Ok);
        assert!(warnings.is_empty(), "{warnings:?}");

        // A machine whose 70% line sits below the estimate: warn, but proceed.
        let tiny_ram = quiet.estimate.estimated_peak_bytes; // 70% of it is below the estimate
        let mut warnings = Vec::new();
        let warned = preflight_memory(
            &input,
            RunProfile::DecodeOnly,
            SamplePlan::auto(),
            budget,
            Some(tiny_ram),
            &log,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(warned.decision, memory::Verdict::Warn);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("70%"), "{warnings:?}");
        assert!(warnings[0].contains("may swap"), "{warnings:?}");
        // …and that is a report warning, so `--strict`'s gate (`args.strict &&
        // !report.warnings.is_empty()`) fails the run on it.
        let report = Report {
            warnings,
            ..Report::default()
        };
        assert!(
            !report.warnings.is_empty(),
            "the warning must be `--strict`-promotable"
        );
    }
}
