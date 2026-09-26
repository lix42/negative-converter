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

use std::ffi::OsStr;
use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::algo::fixed;
use crate::flow::{self, Flow};
use crate::io::decode::{DecodeInfo, decode_within, probe};
use crate::io::{avif, encode, staged, ultra_hdr};
use crate::pipeline::chain;
use crate::pipeline::display_tone::Headroom;
use crate::pipeline::fit_gamut::DestinationGamut;
use crate::pipeline::fit_range::{self, DisplayPeak};
use crate::pipeline::input_semantics::{
    self, ContainerColorFacts, InputAssertions, InputColorReport, RawMode,
};
use crate::pipeline::memory::{self, MemoryReport, RunProfile, SamplePlan};
use crate::pipeline::working_space::AcesCgImage;
use crate::pipeline::{
    color, film_base, gain_map, hdr, look, roll_white, scene_correction, sdr, stages, working_space,
};
use crate::recipe::{self, KnobNames, Recipe};
use crate::telemetry;
use crate::types::{
    AnchorPlacement, CalibrationParams, DEFAULT_MEASURE_INSET, EncodeOutcome, EncodeReport,
    FilmBase, FilmBaseSource, FilmType, InputParams, LinearImage, MeaningAssertion, MeasureParams,
    NcError, OutDepth, OutputParams, OutputPreset, OutputStats, PrintParams,
    REMOVED_SIMPLE_RECONSTRUCTION, Reconstruction, Result, TransferAssertion, WbSource,
    check_measure_inset,
};
use crate::version::{self, Identity};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// `hanten` — film-negative → positive converter.
//
// `--version` prints the full build identity (semver + behavioral
// `pipeline_version` + commit + target), not just the crate version, so an output
// can be attributed to a build — see `version::version_string`.
#[derive(Parser, Debug)]
#[command(
    // `name` is what `--version` prints before the identity block and what every
    // usage/error line spells, so it is the product name. `nctool`'s `is_nc`
    // recognises this banner — it must accept the pre-rename `nc ` too, since the
    // reference rendition comes from the pre-rename reference build (CLAUDE.md's
    // boundary).
    name = "hanten",
    version = version::version_string(),
    about = "Hanten — film-negative → positive converter"
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
    /// Convert a negative scan to a positive image.
    Convert(ConvertArgs),
    /// Convert a roll (batch of frames) from one shared, frozen recipe.
    Roll(RollArgs),
    /// Inspect a scan and emit a JSON report (no output image).
    Inspect(IoArgs),
    /// Run only film-base / Dmin estimation; emit JSON.
    Estimate(EstimateArgs),
    /// Measure a roll's white balance once, for its recipe; emit JSON.
    MeasureRoll(MeasureRollArgs),
    /// Print the full default parameter set as JSON (recipe scaffolding).
    Params(ParamsArgs),
}

/// `hanten params` options.
#[derive(Args, Debug)]
pub struct ParamsArgs {
    /// Transitional: print the new rendering chain's recipe (recipe_version 2)
    /// instead of the shipped one's — the document `convert --new-flow` and
    /// `roll --new-flow` read. See `convert --new-flow`.
    #[arg(long = "new-flow")]
    pub new_flow: bool,
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
    /// Declared film chemistry (`silver` | `chromogenic`). Provenance only: it does
    /// **not** gate IR-assisted film-holder detection, which measures the IR plane
    /// itself. See `convert --film-type`.
    #[arg(long = "film-type", value_enum, value_name = "TYPE")]
    pub film_type: Option<FilmType>,
    #[command(flatten)]
    pub measure: MeasureOverrides,
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
    /// Retired with the roll reference density (`nf-retire/dmax-machinery`). Hidden,
    /// and kept only to emit a migration error.
    #[arg(long = "d-max-region", hide = true, value_name = "X,Y,W,H", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub d_max_region: Option<String>,
    /// Declared film chemistry (`silver` | `chromogenic`). Provenance only: it does
    /// **not** gate IR-assisted film-holder detection, which measures the IR plane
    /// itself. See `convert --film-type`.
    #[arg(long = "film-type", value_enum, value_name = "TYPE")]
    pub film_type: Option<FilmType>,
    #[command(flatten)]
    pub film_base: FilmBaseOverrides,
    #[command(flatten)]
    pub measure: MeasureOverrides,
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

/// `measure-roll`: the roll's picture frames, its leader, and the new chain's recipe
/// they are decoded under (`nf-scene-correction/roll-white-balance`).
#[derive(Args, Debug)]
pub struct MeasureRollArgs {
    /// The roll's picture frames (SilverFast HDR/HDRi TIFF). Leave out the unexposed
    /// base, the leader and any calibration frame: every input is pooled as picture.
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,
    /// The roll's leader — a fully exposed frame, decoded with the same base. Pixels
    /// within 0.1 density of it are left out, so a fully exposed frame mixed into the
    /// roll cannot become its white (measured: it would move the gains 0.4–1.3 stops).
    /// Without it the run warns, and `--strict` refuses before decoding anything.
    #[arg(long, value_name = "PATH")]
    pub leader: Option<PathBuf>,
    /// The roll's recipe for the new chain (`"recipe_version": 2`): the film base and
    /// the decode the gains are measured under. Its `scene_correction` values are not
    /// read — that is what this command measures — though the recipe must still load
    /// (a retired or unknown key there is refused).
    #[arg(long = "params", value_name = "JSON")]
    pub recipe_in: Option<PathBuf>,
    /// The roll's film base (Dmin) as `R,G,B`, over the recipe's. Required one way or
    /// the other, and explicit: a base estimated per frame would measure each frame
    /// under a different decode. Measure it once with `hanten estimate --grid` on the
    /// unexposed base frame.
    #[arg(long = "film-base", value_name = "R,G,B", value_parser = parse_rgb)]
    pub film_base: Option<[f32; 3]>,
    #[command(flatten)]
    pub measure: MeasureOverrides,
    /// Treat warnings (a capped holder march, decode notes) as a hard error, and
    /// refuse to measure without `--leader`.
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
    /// Output positive path. The suffix is optional: leave it off and Hanten
    /// appends the resolved preset's container (see --output-preset). A stated
    /// suffix is never rewritten, so it must be one that preset accepts.
    #[arg(short = 'o', long, value_name = "PATH")]
    pub output: PathBuf,
    /// Removed with `simple` reconstruction: density is the only reconstruction.
    /// Hidden, and kept only to emit a migration error — there is no alias.
    #[arg(long, hide = true, value_name = "TYPE")]
    pub reconstruction: Option<String>,
    /// Removed: the pre-reconstruction algorithm selector. Kept hidden only to
    /// emit a migration error (nc is unreleased — no aliases).
    #[arg(long, hide = true, value_name = "NAME")]
    pub algorithm: Option<String>,

    #[command(flatten)]
    pub input_opts: InputOverrides,
    #[command(flatten)]
    pub film_base: FilmBaseOverrides,
    #[command(flatten)]
    pub measure: MeasureOverrides,
    #[command(flatten)]
    pub density: DensityOverrides,
    #[command(flatten)]
    pub dmax: RemovedDmaxFlags,
    #[command(flatten)]
    pub balance: RemovedBalanceFlags,
    #[command(flatten)]
    pub sigmoid: RemovedSigmoidFlags,
    #[command(flatten)]
    pub characteristic: RemovedCharacteristicFlags,
    #[command(flatten)]
    pub anchor: AnchorOverrides,
    #[command(flatten)]
    pub print: PrintOverrides,
    #[command(flatten)]
    pub scene: SceneCorrectionOverrides,
    #[command(flatten)]
    pub look: LookOverrides,
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

    /// Transitional: resolve the new rendering chain (docs/design-update.md,
    /// docs/nf-migration.md) instead of the shipped one. Scaffolding rather than a
    /// feature: CLI-only — never a recipe key, since it selects which knobs exist
    /// rather than setting one — and removed when the default flips, as a migration
    /// error with no alias (nc is unreleased, so removal is cheap). A knob the new
    /// chain cannot honour is refused rather than accepted and ignored. A --params
    /// recipe must be the new chain's own document (`"recipe_version": 2`; `hanten
    /// params --new-flow` writes one) — each chain refuses the other's recipe by name.
    /// It writes one destination, a Display P3 16-bit TIFF, with no sidecar.
    // Plain prose on purpose: clap renders a doc comment verbatim as `--help` text,
    // so markdown emphasis would print as asterisks.
    #[arg(long = "new-flow")]
    pub new_flow: bool,

    #[command(flatten)]
    pub memory: MemoryArgs,
    #[command(flatten)]
    pub report: ReportArgs,
}

/// `hanten roll`: convert a batch of frames from ONE shared, frozen recipe so the
/// whole roll is color-consistent and reproducible (design-spec §8, §12 item 6).
///
/// This is the batch-**apply** half of plan→recipe→apply: it replays a *provided*
/// frozen recipe (hand-authored or `hanten params`/`--dump-params`-produced) over N
/// frames. It deliberately owns no auto-cascade that *generates* the recipe —
/// that is the separate `base-acquisition-planner` task. Roll-fixed params (the
/// film base) live in the shared `--params` recipe and appear
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
    /// …). Same JSON shape as `convert --params`.
    #[arg(long = "params", value_name = "JSON")]
    pub recipe_in: Option<PathBuf>,
    /// Treat any frame's warnings as a hard error (after the roll report is
    /// emitted), like `convert --strict`.
    #[arg(long)]
    pub strict: bool,
    /// Transitional: resolve the new rendering chain for every frame — see
    /// `convert --new-flow`. Roll accepts no conversion flags, so its knobs arrive in
    /// the shared recipe and the per-frame overrides: the shared recipe must be the
    /// new chain's document (`"recipe_version": 2`), and each override is merged onto
    /// it, so the current chain's keys are refused by name at either site.
    #[arg(long = "new-flow")]
    pub new_flow: bool,
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
    /// Declared film chemistry (`silver` | `chromogenic` | `unknown`). Provenance
    /// only — it does **not** gate IR-assisted film-holder detection: whether IR can
    /// separate the holder from film is measured from the plane itself, because
    /// separability tracks the frame's accumulated density rather than the stock's
    /// chemistry (an unexposed silver frame separates; its own leader does not).
    /// Recipe key `input.film_type`. Kept as a shared input-medium declaration the
    /// black & white `bw-support` task and the separate IR dust-removal task still
    /// need.
    #[arg(long = "film-type", value_enum, value_name = "TYPE")]
    pub film_type: Option<FilmType>,
    /// Write the decoded IR plane to this path (HDRi only).
    #[arg(long, value_name = "PATH")]
    pub export_ir: Option<String>,
}

/// Film-base / Dmin overrides (design-spec §9, stage 2).
///
/// The three source flags are mutually exclusive (clap rejects passing more than
/// one); whichever is given replaces the recipe's `calibration.film_base` entirely.
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
    /// `calibration.film_base` recipe key, because `Dmin` is a per-roll calibration
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

/// Measurement-region overrides (design-spec §9, `measure`).
#[derive(Args, Debug, Default)]
pub struct MeasureOverrides {
    /// Static border inset for measurements, as a fraction of the shorter edge
    /// (default 0.05). The effective area is two cuts in order: the film holder,
    /// measured from the IR plane where it separates, then this inset. The inset
    /// runs either way — where the holder could not be measured (no IR plane, or
    /// film too IR-opaque, which is routine for B&W) it is the only cut, and the
    /// default is sized for a rebate rather than a holder. Raise it for such a
    /// scan; the report says which case the run was in. Where the holder *was*
    /// measured the applied inset is floored at one holder-probe step (0.5% of the
    /// shorter edge), the cut's own resolution, so a stated value near 0 can report
    /// more than it asked for — `effective_area.inset` is the applied value. Never
    /// crops the image.
    #[arg(long = "measure-inset", value_name = "FRAC")]
    pub measure_inset: Option<f32>,
}

/// Density-reconstruction overrides (design-spec §9,
/// `reconstruction = density`). Every flag here maps into the tagged
/// `reconstruction` object: `--density-scale`/`--density-offset` ⇒
/// `reconstruction.density.scale`/`.offset`, and `--density-gamma` ⇒
/// `reconstruction.curve.gamma`.
#[derive(Args, Debug, Default)]
pub struct DensityOverrides {
    /// Per-channel density gain.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub density_scale: Option<[f32; 3]>,
    /// Per-channel density offset (orange-mask compensation).
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub density_offset: Option<[f32; 3]>,
    /// Exponential-curve gamma (the straight line's slope). Under `--new-flow` this
    /// is only the film's linearization (recipe key `reconstruction.linearization`,
    /// default 1.8) — a calibration; how contrasty the picture is is `--contrast`.
    /// Without it, the whole slope (default 2.0).
    #[arg(long)]
    pub density_gamma: Option<f32>,
}

/// The curve selector, the `characteristic` curve's stock and the named bundles that
/// selected it, removed with that curve (`nf-retire/characteristic`). Hidden, and kept
/// only to emit a migration error — there is no alias. Each takes any value, or none,
/// so an old spelling reaches that message instead of clap's generic one.
#[derive(Args, Debug, Default)]
pub struct RemovedCharacteristicFlags {
    #[arg(long = "density-curve", hide = true, value_name = "CURVE", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub density_curve: Option<String>,
    #[arg(long = "film-stock", hide = true, value_name = "NAME", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub film_stock: Option<String>,
    #[arg(long = "preset", hide = true, value_name = "NAME", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub preset: Option<String>,
}

/// The regional balance's flags, removed with it (`nf-retire/regional-balance`).
/// Hidden, and kept only to emit a migration error — there is no alias. The valued
/// ones take any value, or none, so the old spellings (a negative `--shadow-balance
/// -0.05,0,0`, a bare `--balance-range`) reach that message instead of clap's generic
/// one. (A bare one followed by another valued flag still swallows that flag and hits
/// clap's error — loud, exit 2, and never a valid spelling.)
#[derive(Args, Debug, Default)]
pub struct RemovedBalanceFlags {
    #[arg(long = "shadow-balance", hide = true, value_name = "R,G,B", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub shadow_balance: Option<String>,
    #[arg(long = "highlight-balance", hide = true, value_name = "R,G,B", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub highlight_balance: Option<String>,
    #[arg(long = "balance-range", hide = true, value_name = "LO,HI", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub balance_range: Option<String>,
    #[arg(long = "auto-balance-range", hide = true)]
    pub auto_balance_range: bool,
}

/// The reference density's flags and the three anchor placements that read it or
/// pinned black, removed together (`nf-retire/dmax-machinery`). Hidden, and kept only
/// to emit a migration error — there is no alias. The valued ones take any value, or
/// none, so the old spellings (`--d-max`, `--d-max -1.5`) reach that message instead
/// of clap's generic one. (A bare one followed by another valued flag still swallows
/// that flag and hits clap's error — loud, exit 2, and never a valid spelling.)
#[derive(Args, Debug, Default)]
pub struct RemovedDmaxFlags {
    #[arg(long = "d-max", hide = true, value_name = "D", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub d_max: Option<String>,
    #[arg(long = "fixed-d-max", hide = true)]
    pub fixed_d_max: bool,
    #[arg(long = "auto-d-max", hide = true)]
    pub auto_d_max: bool,
    #[arg(long = "no-d-max", hide = true)]
    pub no_d_max: bool,
    #[arg(long = "anchor-mid-fraction", hide = true, value_name = "F", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub anchor_mid_fraction: Option<String>,
    #[arg(long = "anchor-white-at-reference", hide = true)]
    pub anchor_white_at_reference: bool,
    #[arg(long = "anchor-black-floor", hide = true, value_name = "FLOOR", num_args = 0..=1, default_missing_value = "", allow_hyphen_values = true)]
    pub anchor_black_floor: Option<String>,
}

/// The sigmoid curve's flags, removed with it (`nf-retire/sigmoid-and-simple`),
/// including the two `--anchor-*` aliases that kept its prefix. Hidden, and kept only
/// to emit a migration error — there is no alias.
#[derive(Args, Debug, Default)]
pub struct RemovedSigmoidFlags {
    #[arg(long, hide = true, value_name = "F")]
    pub sigmoid_contrast: Option<String>,
    #[arg(long, hide = true, value_name = "W")]
    pub sigmoid_toe: Option<String>,
    #[arg(long, hide = true, value_name = "W")]
    pub sigmoid_shoulder: Option<String>,
    #[arg(long, hide = true, value_name = "F")]
    pub sigmoid_mid_fraction: Option<String>,
    #[arg(long, hide = true)]
    pub sigmoid_white_at_d_max: bool,
}

/// Anchor-placement override (design-spec §7.2/§9, `reconstruction.curve.anchor`) —
/// the exponential's [`AnchorPlacement`].
#[derive(Args, Debug, Default)]
pub struct AnchorOverrides {
    /// Pin mid-grey (18%) at density D above the film base, letting display white
    /// fall where the slope puts it — the default rule, D 0.62. Reads no roll
    /// reference density, so a leader's roll-to-roll error never reaches the render.
    #[arg(long = "anchor-mid-offset", value_name = "D")]
    pub anchor_mid_offset: Option<f32>,
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

/// The new chain's scene-correction overrides that have no current-chain flag
/// (recipe section `scene_correction`, `nf-scene-correction/stage`).
///
/// `--white-balance` keeps its spelling on both chains (in [`PrintOverrides`]), and
/// `--auto-wb` is the current chain's alone — the new chain measures white balance
/// once per roll (`hanten measure-roll`). Only exposure is renamed, because `--print-exposure`
/// names a print stage the new chain does not have. Refused without `--new-flow`
/// (`flow::reject_unavailable_flags`), where the exposure is `--print-exposure`.
#[derive(Args, Debug, Default)]
pub struct SceneCorrectionOverrides {
    /// Exposure in stops (EV) — a scene-referred gain of `2^EV` on every channel,
    /// before the look and the display fit (recipe key `scene_correction.exposure`).
    /// Stops of the reconstructed scene: the look's `--contrast` then expands them with
    /// the rest of the picture. `--new-flow` only; the current chain's exposure is
    /// `--print-exposure`.
    #[arg(long, allow_hyphen_values = true, conflicts_with = "print_exposure")]
    pub exposure: Option<f32>,
}

/// The new chain's look overrides (recipe section `look`, `nf-look`). `--new-flow`
/// only; refused without it (`flow::reject_unavailable_flags`).
#[derive(Args, Debug, Default)]
pub struct LookOverrides {
    /// Print contrast, pivoted at mid-grey: every ACEScg channel becomes
    /// `0.18 · (v / 0.18)^CONTRAST` (recipe key `look.contrast`, default 2.0/1.8 ≈ 1.11,
    /// which with the decode's linearization reproduces the pre-split contrast 2.0;
    /// 1 is the identity). Runs after scene correction, so an `--exposure` is expanded
    /// with the rest of the picture. `--new-flow` only.
    #[arg(long, value_name = "CONTRAST", allow_hyphen_values = true)]
    pub contrast: Option<f32>,
    /// The per-channel grade `R,B`: red and blue exponents pivoted at mid-grey, green
    /// fixed at 1, with each pixel's ACEScg luminance restored afterwards — a cast that
    /// grows away from mid in both directions without moving neutral contrast (recipe
    /// key `look.channel_grade`, default `1,1`, the identity). Both positive, with the
    /// spread over `R,1,B` under 1. Runs after `--contrast`. `--new-flow` only.
    #[arg(
        long = "channel-grade",
        value_name = "R,B",
        value_parser = parse_lo_hi,
        allow_hyphen_values = true
    )]
    pub channel_grade: Option<[f32; 2]>,
    /// Highlight desaturation's strength, in [0, 1]: how far a bright, near-neutral
    /// pixel is pulled toward neutral (recipe key
    /// `look.highlight_desaturation.strength`, default 0.8; 0 is off). It keys on
    /// brightness and on distance from the neutral axis, so coloured highlights keep
    /// their colour; it assumes the roll's white balance (`hanten measure-roll`).
    /// `--new-flow` only.
    #[arg(
        long = "highlight-desaturation",
        value_name = "STRENGTH",
        allow_hyphen_values = true
    )]
    pub highlight_desaturation: Option<f32>,
    /// Where highlight desaturation starts, in stops relative to diffuse white
    /// (negative; recipe key `look.highlight_desaturation.start_stops`, default -1).
    /// `--new-flow` only.
    #[arg(
        long = "highlight-desaturation-start",
        value_name = "STOPS",
        allow_hyphen_values = true
    )]
    pub highlight_desaturation_start: Option<f32>,
    /// Highlight desaturation's saturation band `S0,S1`: full pull at or below `S0`,
    /// none at or above `S1`, on `log10(max/min)` of the pixel's ACEScg channels over
    /// the whole contrast, `--density-gamma` times `--contrast` (recipe key `look.highlight_desaturation.band`, default
    /// `0.015,0.025`). `--new-flow` only.
    #[arg(
        long = "highlight-desaturation-band",
        value_name = "S0,S1",
        value_parser = parse_lo_hi,
        allow_hyphen_values = true
    )]
    pub highlight_desaturation_band: Option<[f32; 2]>,
}

impl LookOverrides {
    /// Whether any look flag was typed.
    pub(crate) fn any(&self) -> bool {
        self.contrast.is_some()
            || self.channel_grade.is_some()
            || self.highlight_desaturation.is_some()
            || self.highlight_desaturation_start.is_some()
            || self.highlight_desaturation_band.is_some()
    }
}

/// Print / tone-render overrides (design-spec §9).
///
/// `--white-balance` and `--auto-wb` are the two faces of the current chain's one
/// white-balance source, `print.white_balance` — mutually exclusive (clap rejects
/// passing both); whichever is given replaces the recipe's choice entirely.
/// Precedence is by **source**, not value: an explicit `--white-balance 1,1,1` over a
/// recipe's auto mode means neutral gains, not re-estimation. Under `--new-flow` only
/// `--white-balance` exists, setting `scene_correction.white_balance`
/// (`recipe::merge`); `--auto-wb` is refused there (`flow`).
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
    /// Estimate the white-balance gains per frame from image statistics. Not under
    /// `--new-flow`, which measures white balance once per roll instead (`hanten
    /// measure-roll`).
    #[arg(long = "auto-wb", value_enum, value_name = "MODE")]
    pub auto_wb: Option<AutoWb>,
    /// Removed with the `shoulder` and `none` tones (recipe key `print.display_tone`):
    /// the display tone is always extended Reinhard. Hidden, and kept only to emit a
    /// migration error.
    #[arg(long = "display-tone", hide = true, value_name = "MODE")]
    pub display_tone: Option<String>,
    /// Specular headroom above reference white for the display tone, in stops, default
    /// 6 (a white point of 64; recipe key `fit_range.headroom_stops`). The tone is
    /// extended Reinhard, which compresses the whole curve against that white point
    /// while holding scene mid-grey (18%) where it is, so raising the headroom changes
    /// the highlights without darkening the midtones. It deliberately overshoots
    /// reference white on the SDR presets, where the loss is counted at the encode
    /// boundary; the HDR presets hold it under their 1000-nit peak. `0` is the exact
    /// identity, and then anything above the render's own ceiling — reference white on
    /// SDR, the 1000-nit peak (≈4.93x reference white) on HDR — is a loud error rather
    /// than a clip: lower --print-exposure until the frame fits. Display presets only;
    /// `film-master` applies no display tone.
    // A negative headroom must reach `check_headroom_stops`, whose message names this
    // very flag: without this, clap refused `-1` as "unexpected argument" and the rule's
    // own negative branch was unreachable from the CLI. Same reason `--linear-range`
    // accepts a leading `-`.
    #[arg(
        long = "display-tone-headroom",
        value_name = "STOPS",
        allow_hyphen_values = true
    )]
    pub display_tone_headroom: Option<f32>,
    /// Removed with the `shoulder` tone, whose knee it placed (recipe key
    /// `print.highlight_compress`). Hidden, and kept only to emit a migration error.
    #[arg(long, hide = true, value_name = "AMOUNT", allow_hyphen_values = true)]
    pub highlight_compress: Option<String>,
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
/// `--output-preset` is the whole output *policy* choice: every preset is atomic,
/// resolving its own container, depth and profile. The depth/profile/container
/// selectors retired with the `legacy` and `custom` presets, the only ones that read
/// them; their flags survive hidden, only to emit the migration error
/// ([`REMOVED_OUTPUT_SELECTORS`]).
#[derive(Args, Debug, Default)]
pub struct OutputOverrides {
    /// Named output policy. **Default: `gain-map-hdr`** (since `pipeline_version` 3)
    /// — a gain-map JPEG carrying both the ISO 21496-1 and legacy Ultra HDR v1
    /// metadata dialects, and the one that decodes as HDR on Apple platforms. It
    /// requires a `.jpg`/`.jpeg` output path, so `-o out.tif` with no preset is a
    /// usage error rather than a TIFF.
    ///
    /// The rest:
    /// `ultra-hdr-v1` (the same file with only the legacy dialect — the
    /// compatibility form, which Apple opens as plain SDR);
    /// `display-p3` / `compatibility` (a modern-pipeline SDR render stored losslessly
    /// as 16-bit integer TIFF, in Display P3 and sRGB respectively);
    /// `film-master` (unclamped 32-bit float linear ACEScg TIFF taken straight from
    /// the NC film RGB v1 mapping, bypassing every print/display control);
    /// `hdr-pq` / `hdr-hlg` (10-bit 4:4:4 Rec.2100 PQ/HLG AVIF, requires `.avif`);
    /// `hdr-linear-tiff` (32-bit float display-linear BT.2020 interchange TIFF with
    /// no transfer applied);
    /// `hdr-pq-tiff` / `hdr-hlg-tiff` (the same Rec.2100 signal as the AVIF presets,
    /// stored as full-range 16-bit TIFF code values).
    /// Every TIFF preset requires `.tif`/`.tiff`. Recipe key `output.preset`.
    ///
    /// Every preset resolves its own depth, profile and container. `film-master`
    /// additionally rejects every non-default downstream control; every display preset consumes those controls
    /// instead.
    #[arg(long = "output-preset", value_name = "PRESET")]
    pub output_preset: Option<String>,
    /// Removed with the `legacy` and `custom` presets (recipe key `output.depth`).
    /// Hidden, and kept only to emit a migration error — there is no alias.
    #[arg(long = "out-depth", hide = true, value_name = "DEPTH")]
    pub out_depth: Option<String>,
    /// Removed: `--output-hdr` / `--output-sdr` (recipe key `output.hdr`), the
    /// earlier spelling of `--out-depth`. Hidden, and kept only to emit a migration
    /// error.
    #[arg(long, hide = true)]
    pub output_hdr: bool,
    /// Removed: see [`output_hdr`](Self::output_hdr).
    #[arg(long, hide = true)]
    pub output_sdr: bool,
    /// Removed with the `legacy` and `custom` presets (recipe key
    /// `output.output_profile`). Hidden, and kept only to emit a migration error.
    #[arg(long, hide = true, value_name = "PROFILE")]
    pub output_profile: Option<String>,
    /// Removed with the `legacy` and `custom` presets (recipe key `output.bigtiff`).
    /// Hidden, and kept only to emit a migration error.
    #[arg(long, hide = true, value_name = "POLICY")]
    pub bigtiff: Option<String>,
}

/// One output selector that retired with the `legacy` and `custom` presets.
struct RemovedOutputSelector {
    /// The recipe key, under `output`.
    key: &'static str,
    flag: &'static str,
    /// Whether the flag was passed.
    present: fn(&OutputOverrides) -> bool,
    /// Whether a recipe value is the one every earlier build wrote by default. Every
    /// sidecar and `--dump-params` document carried all three keys at these values, so
    /// they are dropped on load rather than refused — they asked for nothing, and
    /// refusing them would refuse every recipe written before the retirement.
    is_old_default: fn(&serde_json::Value) -> bool,
    /// What replaced it, for the migration message.
    replacement: &'static str,
}

/// The selectors that retired with the `legacy` and `custom` presets. One table, so the
/// flag stubs ([`reject_removed_flags`]) and the recipe migration
/// ([`reject_legacy_recipe_keys`]) say the same thing.
static REMOVED_OUTPUT_SELECTORS: [RemovedOutputSelector; 3] = [
    RemovedOutputSelector {
        key: "depth",
        flag: "--out-depth",
        present: |o| o.out_depth.is_some(),
        is_old_default: |v| v.as_str() == Some("u16"),
        replacement: "every preset resolves its own depth — for a 16-bit TIFF use the \
                      `display-p3` preset (or `compatibility` for sRGB); for a float TIFF, \
                      `film-master` (linear ACEScg before display rendering) or \
                      `hdr-linear-tiff` (display-linear BT.2020)",
    },
    RemovedOutputSelector {
        key: "output_profile",
        flag: "--output-profile",
        present: |o| o.output_profile.is_some(),
        is_old_default: serde_json::Value::is_null,
        replacement: "every preset embeds the profile its pixels are in — `display-p3` \
                      for Display P3, `compatibility` for sRGB, `film-master` for linear \
                      ACEScg; ProPhoto and a user-supplied ICC file have no replacement",
    },
    RemovedOutputSelector {
        key: "bigtiff",
        flag: "--bigtiff",
        present: |o| o.bigtiff.is_some(),
        is_old_default: |v| v.as_str() == Some("auto"),
        replacement: "BigTIFF is always decided automatically: a file too large for \
                      classic TIFF is written as BigTIFF, and the report says so",
    },
];

/// The [`REMOVED_OUTPUT_SELECTORS`] row for `key` — looked up by name, so no caller
/// depends on the table's order.
fn removed_output_selector(key: &str) -> &'static RemovedOutputSelector {
    REMOVED_OUTPUT_SELECTORS
        .iter()
        .find(|s| s.key == key)
        .expect("a retired output selector is named by its recipe key")
}

/// The remedy for a retired selector, on the chain the command line selects. The
/// preset advice holds only without `--new-flow`, which refuses every preset flag: there,
/// the one destination is already a Display P3 16-bit TIFF, so dropping the flag works.
fn removed_output_remedy(s: &RemovedOutputSelector, new_flow: bool) -> String {
    if new_flow {
        "drop it — under `--new-flow` the one destination is a Display P3 16-bit TIFF, and \
         there is no output policy to choose yet"
            .to_string()
    } else {
        s.replacement.to_string()
    }
}

/// The migration message for a retired selector passed as a **flag**.
fn removed_output_flag_message(s: &RemovedOutputSelector, new_flow: bool) -> String {
    format!(
        "{} (recipe key `output.{}`) was removed together with the `legacy` and `custom` \
         output presets, the only ones that read it: {}. There is no alias.",
        s.flag,
        s.key,
        removed_output_remedy(s, new_flow)
    )
}

/// Drop the retired keys a recipe carries at the value every earlier build wrote by
/// default — the output selectors, `print.highlight_compress`, `calibration.dmax`
/// at `"fixed"`, and the regional balance's `reconstruction.density.shadow_balance` /
/// `highlight_balance` at `[0, 0, 0]` and `balance_range` at `"auto"` — returning
/// whether anything was removed. A non-default value is left
/// for [`reject_legacy_recipe_keys`] to refuse.
///
/// The rule for retiring any recipe key: every sidecar and `--dump-params` document
/// serializes every key, so a retired key sits at its old default in every recipe on
/// disk — strip it there, or no old recipe replays. The exception is an old default
/// whose replay would now render differently: refuse it, since stripping it would
/// silently render the new default. `print.display_tone`'s `"shoulder"` is one, so it is
/// **not** stripped here. `calibration.dmax`'s `"fixed"` is not one: the one placement
/// left reads no reference, so it replays byte-identically; nor is the regional
/// balance's, whose neutral pair skipped the pass bit-exactly.
fn strip_retired_keys_at_old_defaults(v: &mut serde_json::Value) -> bool {
    let mut stripped = false;
    if let Some(output) = v.get_mut("output").and_then(|o| o.as_object_mut()) {
        for s in &REMOVED_OUTPUT_SELECTORS {
            if output.get(s.key).is_some_and(s.is_old_default) {
                output.remove(s.key);
                stripped = true;
            }
        }
    }
    if let Some(print) = v.get_mut("print").and_then(|p| p.as_object_mut())
        && print
            .get("highlight_compress")
            .is_some_and(|hc| hc.as_f64() == Some(0.0))
    {
        print.remove("highlight_compress");
        stripped = true;
    }
    if let Some(calibration) = v.get_mut("calibration").and_then(|c| c.as_object_mut())
        && calibration.get("dmax").and_then(serde_json::Value::as_str) == Some("fixed")
    {
        calibration.remove("dmax");
        stripped = true;
    }
    if let Some(density) = v
        .pointer_mut("/reconstruction/density")
        .and_then(|d| d.as_object_mut())
    {
        // Neutral as the old deserializer read it (f32), so `-0.0`, `0` and a value
        // underflowing to zero strip too — the old build skipped the pass for them all.
        let neutral = |key: &str, value: &serde_json::Value| match key {
            "balance_range" => value.as_str() == Some("auto"),
            _ => balance_triple(value) == Some([0.0; 3]),
        };
        for key in REGIONAL_BALANCE_KEYS {
            if density.get(key).is_some_and(|value| neutral(key, value)) {
                density.remove(key);
                stripped = true;
            }
        }
    }
    stripped
}

/// The regional balance's recipe keys under `reconstruction.density`, retired with it
/// (`nf-retire/regional-balance`).
const REGIONAL_BALANCE_KEYS: [&str; 3] = ["shadow_balance", "highlight_balance", "balance_range"];

/// A regional-balance triple as the old deserializer read it: a three-element numeric
/// array, each value as `f32`. `None` for anything else. Shared by the neutral-default
/// strip and the migration message, so the two agree on what equals what.
fn balance_triple(value: &serde_json::Value) -> Option<[f32; 3]> {
    let a = value.as_array()?;
    if a.len() != 3 {
        return None;
    }
    let mut t = [0.0f32; 3];
    for (slot, c) in t.iter_mut().zip(a) {
        *slot = c.as_f64()? as f32;
    }
    Some(t)
}

/// The migration error for a regional-balance key left after
/// [`strip_retired_keys_at_old_defaults`] — a non-neutral value.
///
/// The old balance compared its two f32 triples first and, when they were equal (an
/// absent key meaning `[0, 0, 0]`), applied a tone-independent offset without reading
/// the range. So an equal pair is reproduced by `reconstruction.density.offset` —
/// bit-for-bit over a zero offset, since `x + 0 + s` is `x + s`; otherwise to one f32
/// rounding of the sum — and a `balance_range` beside equal (or absent) balances never
/// affected the render. Both remedies say so; only a differing pair is a lost render.
fn removed_balance_recipe_message(density: &serde_json::Value, context: &str) -> Option<String> {
    let key = REGIONAL_BALANCE_KEYS
        .into_iter()
        .find(|k| density.get(k).is_some())?;
    let value = &density[key];
    // An absent key is its neutral default; a malformed value never counts as equal.
    let triple = |k: &str| density.get(k).map_or(Some([0.0; 3]), balance_triple);
    let equal_pair = match (triple("shadow_balance"), triple("highlight_balance")) {
        (Some(s), Some(h)) if s == h => Some(s),
        _ => None,
    };
    let remedy = match (key, equal_pair) {
        ("balance_range", Some(_)) => "Remove the key; the render is unchanged — the range \
                                       was consulted only when the two balances differed"
            .to_string(),
        (_, Some(offset)) => format!(
            "This recipe's equal pair {offset:?} was a tone-independent offset: remove the \
             balance keys and set `reconstruction.density.offset` to the offset this run \
             resolves plus the pair (the shared recipe's, for a roll per-frame override), \
             which replays the render (exactly over a zero offset, otherwise to float \
             rounding)"
        ),
        (_, None) => "Remove the key; the old rendering is reproducible only from the \
                      reference build"
            .to_string(),
    };
    Some(format!(
        "{context}: recipe key `reconstruction.density.{key}` ({value}) was removed with the \
         regional balance: {REGIONAL_BALANCE_RETIRED}. {remedy}. Its neutral default is \
         still accepted, so a sidecar written before the retirement replays."
    ))
}

/// The migration message for a retired `print.display_tone` recipe value.
///
/// Every value is refused, `reinhard` included: the tone is always applied now, and
/// carrying a stated headroom across to `fit_range.headroom_stops` would be an alias.
/// Only `shoulder` and `none` are lost renders; a `reinhard` recipe renders identically
/// once its headroom moves, so its message says so rather than pointing at the reference
/// build.
fn removed_display_tone_recipe_message(value: &serde_json::Value, context: &str) -> String {
    let headroom = value
        .get("reinhard")
        .and_then(|r| r.get("headroom_stops"))
        .and_then(serde_json::Value::as_f64);
    const LOST: &str = "The old rendering is reproducible only from the reference build.";
    const KEPT: &str = "The render is unchanged.";
    let (remedy, outcome) = match (value.as_str(), headroom) {
        (Some("shoulder"), _) => (
            "the recipe's `shoulder` has no replacement: remove the key to take the default \
             (6 stops of headroom)"
                .to_string(),
            LOST,
        ),
        (Some("none"), _) => (
            "the nearest to `none` on a display preset is the identity, \
             `\"fit_range\": {\"headroom_stops\": 0}`"
                .to_string(),
            LOST,
        ),
        (_, Some(stops)) => (
            format!("move the headroom to `\"fit_range\": {{\"headroom_stops\": {stops}}}`"),
            KEPT,
        ),
        _ => (
            "remove the key — the default headroom is what `reinhard` resolved".to_string(),
            KEPT,
        ),
    };
    format!(
        "{context}: recipe key `print.display_tone` ({value}) was removed with the \
         `shoulder` and `none` tones: the display tone is always extended Reinhard, whose \
         one parameter is `fit_range.headroom_stops` (`--display-tone-headroom`). So {remedy}. \
         There is no alias. {outcome}"
    )
}

// ---------------------------------------------------------------------------
// Resolved configuration (= the recipe shape)
// ---------------------------------------------------------------------------

/// The fully-resolved parameter set the pipeline runs on. This is *also* the
/// recipe shape: `--params` deserializes a (partial) recipe into it with serde
/// defaults filling the gaps, and `--dump-params` / `hanten params` serialize it.
///
/// Nested per-stage objects (not a flat bag) so `deny_unknown_fields` can reject
/// typo'd keys at every level — `serde(flatten)` would defeat that. The
/// algorithm selection is the one tagged `reconstruction` object
/// (`schema_version` 1, design-spec §8): there are no sibling top-level
/// `algorithm`/`density`/`sigmoid`/`simple` sections — the removed legacy forms
/// are rejected with a migration error at recipe load (`load_recipe_for`).
///
/// Sections and key placement follow design-spec §9; under `deny_unknown_fields` a key
/// placed differently from §9 rejects every docs-shaped recipe. `params` and `meta` are
/// reserved top-level names: [`split_envelope`] tells a sidecar from a bare recipe by
/// them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ResolvedConfig {
    pub reconstruction: Reconstruction,
    pub input: InputParams,
    pub calibration: CalibrationParams,
    pub measure: MeasureParams,
    pub print: PrintParams,
    /// The display tone's headroom — the new chain's `fit_range` section, shared so the
    /// key is not renamed again when the current chain retires. On the current chain the
    /// SDR presets apply fit range's operator bit-for-bit, while the HDR presets keep
    /// their own asymptotic-base form (`display_tone::highlight_lifted_reinhard`) until
    /// the flip.
    pub fit_range: crate::recipe::FitRange,
    pub output: OutputParams,
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// The reuse-ready forms of a measured film base, kept as one unit so the flag
/// and the recipe value are both-present-or-both-absent — the illegal
/// flag-without-recipe (or recipe-without-flag) state two parallel `Option`s
/// would permit is unrepresentable.
///
/// **The pairing is per measurement, not per section.** The flag half becomes the
/// flat report key `film_base_flag`; the recipe half is copied into the report's
/// [`CalibrationFragment`]. A future calibration value measured across many frames
/// (a roll content white) would have no flag form at all, which a section-wide
/// both-present rule would forbid. Serialize-only.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReuseReady {
    /// Ready-to-paste `--film-base R,G,B` flag for the measured base; the values
    /// round-trip to the exact measured `f32`s.
    #[serde(rename = "film_base_flag")]
    pub flag: String,
    /// The same measurement as the `calibration.film_base` value —
    /// `{"explicit":[r,g,b]}` — ready to merge into a roll recipe. Not serialized
    /// here: it is emitted once, inside [`Report::calibration`].
    #[serde(skip)]
    pub source: FilmBaseSource,
}

/// The report's `calibration` object: the calibration values this invocation
/// resolved, in exactly the recipe shape, so
/// `hanten estimate … | jq '{calibration}' > roll-cal.json` writes a reusable roll
/// calibration with nothing to hand-edit.
///
/// The pipe straight into `--params -` that design-spec §8 shows is the **target**:
/// `--params` takes a path today, is not repeatable, and has no `-` case. Both arrive
/// with `core/recipe-composition`; the shape emitted here is already what they take.
///
/// **Partial by construction, and that is load-bearing.** It is not
/// [`CalibrationParams`]: every member is skipped when absent, so a run that measured
/// nothing pins nothing over a later `--params` layer, and a member is added by adding
/// one field here — the section is open (see [`CalibrationParams`]), so nothing may
/// assume the set is exactly the one member it has today.
///
/// It reports *what was measured here*, never "the roll's calibration": a complete
/// one may need several invocations, and a future member is measured across many
/// frames rather than from one. Assembling it is `core/base-acquisition-planner`.
#[derive(Clone, Debug, PartialEq, Default, Serialize)]
pub struct CalibrationFragment {
    /// The measured base as `calibration.film_base` — present exactly when
    /// `film_base_flag` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_base: Option<FilmBaseSource>,
}

impl CalibrationFragment {
    /// Whether anything was measured; an empty fragment is omitted from the report
    /// rather than emitted as `{}`, which would pipe into `--params` as a no-op the
    /// user could mistake for a calibration.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Resolution diagnostics for the reconstruction that ran (design-spec §8's
/// report shape): `{"curve":{…}}` with the curve's anchor. Serialize-only. (The
/// `type` tags went with `simple` reconstruction and the `characteristic` curve.)
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ReconstructionResult {
    pub curve: CurveResult,
}

/// The resolved curve inside a [`ReconstructionResult`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CurveResult {
    /// The curve's anchor **placement rule** (design-spec §7.2).
    pub anchor: AnchorPlacement,
    /// The **derived** anchor: the corrected density this render mapped to `1.0`, which
    /// sets the black floor at `10^(−contrast·anchor_value)`.
    pub anchor_value: f32,
}

/// Build the report's `reconstruction_result` from the resolved config and the
/// render's resolved anchor value.
fn reconstruction_result(
    reconstruction: &Reconstruction,
    curve_anchor: f32,
) -> ReconstructionResult {
    ReconstructionResult {
        curve: CurveResult {
            // The placement rule and the anchor it derived, so a consumer need not
            // re-derive the anchor from the echoed recipe to know what the render did.
            anchor: reconstruction.curve.anchor,
            anchor_value: curve_anchor,
        },
    }
}

/// What the AVIF encoder coded, for the resolved report. Serialize-only.
///
/// Every field **except `rendering`** is read back out of the produced file rather
/// than restated from the request, so the report is evidence about the artifact and
/// not an echo of the configuration. In particular `profile` records whether the file
/// may claim the AVIF v1.2 Advanced Profile, and `profile_reason` says why not when
/// it may not — a general-brand-only file is a legitimate output, but never a silent
/// one.
///
/// `rendering` is the deliberate exception, and it is nested rather than flattened so
/// the distinction survives: those are the luminance semantics **no** AVIF box can
/// state, so they can only come from the renderer. Keeping them in their own object
/// means a reader can tell at a glance which half of this block is evidence about
/// bytes and which half is declared policy.
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
    /// The rendering policy behind the pixels — see the type's own note.
    pub rendering: AvifRenderingResult,
}

/// The luminance and tone semantics of an AVIF rendition, which the container cannot
/// express. Serialize-only.
///
/// CICP names the transfer function but not what diffuse white *is*: PQ's curve is
/// absolute, yet nothing in the file says nc anchors reference white at 203 cd/m² and
/// masters to a 1000 cd/m² peak, and for HLG — display-referred — no box could. So a
/// consumer deciding how to tone-map these files has the same problem the coded and
/// linear TIFF blocks already solve by stating it, and this states it the same way.
///
/// `tone_curve` is the renderer's **pinned identifier**, straight from its metadata;
/// `output_render.display_tone` states the same resolved operator in the block every
/// preset emits. This is the same pairing the coded- and
/// linear-TIFF blocks carry, and the AVIF block lacking it was the anomaly — two AVIFs
/// with byte-identical `cicp`, `profile` and `level` can hold materially different
/// renditions, and the artifact block should be self-sufficient about which.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AvifRenderingResult {
    /// Reference white in cd/m² (the binding 203).
    pub reference_white_nits: f32,
    /// Mastering target peak in cd/m² (the binding 1000).
    pub target_peak_nits: f32,
    /// The display-linear value that represents `target_peak_nits` (≈4.926108) — the
    /// largest the renderer produces, before the transfer function encodes it.
    pub linear_headroom: f32,
    /// Which display tone curve produced these pixels, straight from the renderer's
    /// metadata.
    pub tone_curve: &'static str,
    /// Pinned gamut-mapping and linear-domain identifiers, from the renderer.
    pub gamut_mapping: &'static str,
    pub linear_domain: &'static str,
    /// HLG's reference-display assumptions, absent for PQ. Mirrors the coded-TIFF
    /// block; unlike that one, the measured content-light values are omitted here
    /// because for AVIF they are in the file's own `clli` box for PQ, and omitted by
    /// design for HLG.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hlg_system_gamma: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hlg_reference_display_peak_nits: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hlg_reference_display_black_nits: Option<f32>,
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
    /// Which display tone curve produced these pixels, straight from the renderer's
    /// own metadata — the same identifier `hdr_linear_tiff` reports.
    pub tone_curve: &'static str,
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

/// What a `--new-flow` conversion ran: the fixed decode's resolved parameters, what
/// each stage of the new chain applied, and the destination. Serialize-only.
///
/// **Provisional.** It exists so the new flow's report says something true while
/// `nf-core/report-contract` decides the real shape; the legacy-chain sections
/// (`reconstruction_result`, `output_render`, `white_balance`, …) are
/// omitted under the flag rather than filled with values that describe a chain the
/// run did not take. Every field is a fact read off the resolved chain — no prose.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NewFlowResult {
    /// The fixed decode's resolved parameters, as the render used them.
    pub decode: fixed::DecodeReport,
    /// Each stage of the new chain in order, with what it applied.
    pub stages: [NewFlowStageResult; 4],
    /// Scene correction's values: the white-balance gains and the exposure applied.
    pub scene_correction: scene_correction::SceneCorrection,
    /// The look's controls as applied — the contrast, and highlight desaturation's
    /// strength, start and band.
    pub look: look::LookSection,
    /// Fit range's operator by name, with the headroom, white point and display peak
    /// it ran at — what a non-default `fit_range.headroom_stops` changes.
    pub fit_range: fit_range::FitRange,
    /// The destination written — one today (`nf-destinations/preset-set` owns the set).
    pub destination: &'static str,
    /// The gamut the chain rendered into, read off its exit.
    pub gamut: &'static str,
    /// Always `false`: no sidecar is written under `--new-flow` yet. The new chain's
    /// recipe exists (`crate::recipe`); writing it here is `nf-core/report-contract`'s.
    pub sidecar_written: bool,
    /// A sidecar an earlier run left beside this output, removed because it
    /// described the image this run replaced. Absent when there was none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_sidecar: Option<PathBuf>,
}

/// One stage of the new chain and what it applied under the run's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct NewFlowStageResult {
    pub stage: &'static str,
    pub applied: &'static str,
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
    /// The resolved output preset (`"gain-map-hdr"` / `"film-master"` / …).
    pub preset: OutputPreset,
    /// Whether the shared print controls (white balance, exposure, black point and
    /// `linear_range`) **ran at all** — not whether their values were non-default.
    /// `false` only for `film-master`, which never reaches the stage.
    ///
    /// The display tone is **not** one of these: it is applied inside each display
    /// renderer rather than in the shared stage, so it is reported by
    /// [`display_tone`](Self::display_tone).
    pub print_controls: bool,
    /// Whether any display rendering — tone mapping, destination gamut mapping, or
    /// a transfer/display encoding — ran. Always `false` for `film-master` (its
    /// ACEScg profile is a *linear* tag on already-ACEScg values, not a transform).
    pub display_render: bool,
    /// The encoding the preset resolved to, as a stable identifier.
    pub encoding: &'static str,
    /// The display tone the branch applied: the operator by name and its headroom.
    ///
    /// Absent when the branch has no display tone stage **at all** — `film-master`'s
    /// bypass. It is the only statement of tone policy the SDR presets and the AVIF
    /// pair emit (neither writes a per-preset contract block), so `content` states what
    /// the branch does *besides* tone and never names a curve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_tone: Option<DisplayToneResult>,
    /// What the pixels contain. For `film-master` this states the intentional-film
    /// content and explicitly disclaims physical scene recovery, and names what
    /// placed mid-grey — the film-base-derived anchor, or the stock's published curve.
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

/// The display tone a display preset applied, for [`OutputRenderResult::display_tone`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DisplayToneResult {
    /// The operator's pinned identifier, or `"identity"` at zero headroom — the new
    /// chain's `new_flow.fit_range.operator` rule, so both chains name identical pixels
    /// identically.
    pub operator: &'static str,
    /// `fit_range.headroom_stops`; `0` is the identity.
    pub headroom_stops: f32,
}

/// Build the report's `output_render` from the resolved config. Pure derivation
/// (like [`reconstruction_result`]): the branch and what it applies are fully
/// determined by the preset, the reconstruction, and the resolved headroom, so there
/// is nothing to thread back from the render.
///
/// Tone is reported as a field and kept out of the prose: deriving it from the preset
/// alone once let `content` assert a shoulder the run had skipped.
fn output_render_result(cfg: &ResolvedConfig) -> OutputRenderResult {
    let (print_controls, display_render, encoding, content) = match cfg.output.preset {
        OutputPreset::FilmMaster => (
            false,
            false,
            "unclamped-linear-acescg-float-tiff",
            "intentional film rendering (film, lens, development, scanner, \
             reconstruction, density curve, and a film-base-derived anchor \
             placement); not a physical scene-linear recovery",
        ),
        OutputPreset::UltraHdrV1 => (
            true,
            true,
            "legacy-ultra-hdr-v1-xmp-mpf-jpeg",
            "independently rendered SDR Display P3 base and HDR rendition, paired \
             by a single-channel luminance legacy Ultra HDR v1 gain map; not ISO 21496-1. \
             Apple platforms ignore this dialect and open the file as plain SDR",
        ),
        OutputPreset::GainMapHdr => (
            true,
            true,
            "dual-dialect-gain-map-jpeg",
            "the same rendition pair and single-channel luminance gain map as \
             `ultra-hdr-v1`, packaged with ISO 21496-1 segments in both images \
             alongside the legacy Ultra HDR v1 XMP/MPF metadata. Which dialect a \
             dual-aware decoder prefers is observed behaviour, not a conformance claim",
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
             Print controls and BT.2020 gamut mapping have both run, so this is a \
             rendered display image and not a film master; and being linear it is \
             not a Rec.2100 PQ/HLG signal either",
        ),
        OutputPreset::DisplayP3 => (
            true,
            true,
            "display-p3-u16-tiff",
            "single-rendition SDR: Display P3 primaries with the sRGB transfer, \
             203 cd/m² reference white, stored losslessly as 16-bit integer codes. \
             The shared print controls and gamut mapping into P3 have both run, so \
             this is a rendered display image, not a film master; it crosses the \
             NC film RGB v1 → linear ACEScg boundary first",
        ),
        OutputPreset::Compatibility => (
            true,
            true,
            "srgb-u16-tiff",
            "single-rendition SDR: sRGB primaries and transfer, 203 cd/m² reference \
             white, stored losslessly as 16-bit integer codes — the widest-support \
             output Hanten writes. The same render as `display-p3` with a smaller \
             destination gamut, so more saturated film colour is mapped in rather \
             than preserved; choose `display-p3` when the target display can show it",
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
    // Matched exhaustively so a new preset has to state whether it has a display tone
    // stage rather than inheriting one silently.
    let display_tone = match cfg.output.preset {
        OutputPreset::FilmMaster => None,
        OutputPreset::GainMapHdr
        | OutputPreset::UltraHdrV1
        | OutputPreset::DisplayP3
        | OutputPreset::Compatibility
        | OutputPreset::HdrPq
        | OutputPreset::HdrHlg
        | OutputPreset::HdrLinearTiff
        | OutputPreset::HdrPqTiff
        | OutputPreset::HdrHlgTiff => Some(DisplayToneResult {
            // `validate` has already checked the headroom; reference white is both
            // branches' identity crossover.
            operator: Headroom::new(cfg.fit_range.headroom_stops)
                .map_or(crate::pipeline::display_tone::EXTENDED_REINHARD, |h| {
                    h.operator(1.0)
                }),
            headroom_stops: cfg.fit_range.headroom_stops,
        }),
    };
    OutputRenderResult {
        preset: cfg.output.preset,
        print_controls,
        display_render,
        encoding,
        display_tone,
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
    /// Resolution diagnostics for the reconstruction that ran (`convert`): the
    /// anchor placement and the anchor it derived (design-spec §8).
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
    /// What a `--new-flow` conversion ran (`convert` and each `roll` frame under the
    /// flag). See [`NewFlowResult`]; absent on the legacy flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_flow: Option<NewFlowResult>,
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
    /// The calibration values this invocation measured, in recipe shape
    /// (`estimate`) — see [`CalibrationFragment`]. Absent when nothing measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationFragment>,
    /// Resolved stage-4 white-balance gains `[r, g, b]` the density print render
    /// applied (`convert`): the auto-estimated (`--auto-wb`) or explicit value,
    /// absent for the `simple` algorithm. Reported so a roll can freeze one
    /// frame's estimate into `--white-balance R,G,B` / a recipe's
    /// `print.white_balance = {"explicit": […]}` — measure once, reuse
    /// (design-spec §8/§9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<[f32; 3]>,
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
    /// The declared film chemistry, echoed back (`inspect` / `estimate`). Those two
    /// commands resolve no recipe, so without this the flag would be parsed and
    /// dropped — accepted-and-ignored, which this project treats as a bug. It gates
    /// nothing (`ir-usability-detection`); it is recorded so a declaration a user
    /// made is visible in the artifact that run produced. `convert`/`roll` carry it
    /// in the resolved recipe instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_type: Option<FilmType>,
    /// The measured IR usability verdict (`inspect` / `estimate`, only on a scan
    /// carrying an IR plane): the interior IR transmission and whether it clears the bar for
    /// telling the opaque holder from film on **this frame**. Reported so the
    /// verdict — and the threshold behind it — is falsifiable from a run rather
    /// than only from the source (`ir-usability-detection`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ir_separability: Option<film_base::IrSeparability>,
    /// IR film-holder classification per edge (`inspect`, on a scan whose IR plane
    /// is marker-verified and measures usable): which along-edge
    /// segments the opaque holder occludes (dark in IR) vs actual film (bright).
    /// Holder segments are excluded from the rebate search; a fully-film or
    /// fully-holder edge is the all-segments-agree case. RGB alone cannot make
    /// this call — holder and dense film are both dark in RGB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_mask: Option<Vec<film_base::EdgeHolderMask>>,
    /// The resolved **effective measurement area** (every command that decodes —
    /// `inspect`, `estimate`, `convert`, and each `roll` frame via
    /// `FrameStatus::Ok`): the rectangle a measurement may be read over, after the
    /// IR-measured holder cut and the static inset. Reported so both cuts are
    /// falsifiable from a run —
    /// `holder: null` means the holder was *not measured* (no IR, shape-only, or
    /// not separable here), while all-zero depths mean it was measured and there is
    /// none. `inset` is the **applied** inset, which the holder cut's resolution
    /// floors at one probe step wherever the march ran. The image is never cropped;
    /// this is where statistics are read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_area: Option<film_base::EffectiveArea>,
    /// Reuse-ready forms of the measured base (`estimate`): a ready-to-paste
    /// `--film-base R,G,B` flag and the matching `film_base` recipe fragment, so
    /// the calibrate-once → reuse workflow (design-spec §8) is copy-paste smooth.
    /// Both forms are present together or both absent — the pair only exists when
    /// the measurement is usable as an explicit base (each channel in `(0, 1]`),
    /// so a single [`ReuseReady`] (both-or-neither) replaces two parallel
    /// `Option`s that could encode the illegal flag-without-recipe state. Flattened
    /// so the flag stays a flat top-level key (`film_base_flag`) on the wire; the
    /// recipe half is emitted inside [`Self::calibration`]; `None` emits neither.
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

/// A loaded recipe plus the load-time facts the report needs, read off the raw JSON
/// (after defaults fill in, a stated default is indistinguishable from an omitted one),
/// and the `pipeline_version` an enveloped sidecar records for the build that produced
/// it.
#[derive(Debug)]
struct LoadedRecipe {
    doc: RecipeDoc,
    /// Whether the file explicitly set `output.preset`: once the default became a *named* preset,
    /// a resolved `gain-map-hdr` no longer says whether anyone chose it. The suffix
    /// diagnosis varies on that (see [`SuffixContext`]).
    output_preset_present: bool,
    /// `meta.pipeline_version` from a sidecar envelope, when the loaded file
    /// carried one. Provenance only — never applied, only compared (see
    /// [`pipeline_version_warning`]).
    meta_pipeline_version: Option<u32>,
    /// How far the recipe left the density curve unpinned (an omitted `curve`, or an
    /// exponential without its `gamma` or `anchor`) — the witness behind
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
/// presence witnesses, the typed `deny_unknown_fields` parse) apply to an
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
///
/// **Which schema is read depends on `flow`**, and each side refuses the other's
/// document by name before the typed parse: under `--new-flow` the body must be a
/// `recipe_version` 2 [`Recipe`] ([`recipe::check_body`]), and without it a body
/// stating `recipe_version` is refused ([`recipe::check_body_without_flag`]).
fn load_recipe_for(path: Option<&Path>, flow: Flow) -> Result<LoadedRecipe> {
    match path {
        None => Ok(LoadedRecipe {
            doc: match flow {
                Flow::Legacy => RecipeDoc::Current(ResolvedConfig::default()),
                Flow::New => RecipeDoc::New(Recipe::default()),
            },
            output_preset_present: false,
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
            // migration checks / presence witnesses on the recipe *body*; the
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
            let (mut envelope_body, meta_pipeline_version) =
                match split_envelope(value.as_ref(), &context)? {
                    Some((body, meta_version)) => (Some(body), meta_version),
                    None => (None, None),
                };
            let mut value = value;
            // Every document the current chain wrote before `legacy`/`custom` and the
            // reference density retired carries their keys at the old defaults (and
            // `print.highlight_compress` at 0); drop those before the body is checked or
            // parsed, so an old sidecar gets as far as its first real migration error.
            let stripped = flow == Flow::Legacy
                && envelope_body
                    .as_mut()
                    .or(value.as_mut())
                    .is_some_and(strip_retired_keys_at_old_defaults);
            // The recipe *body*: an envelope's `params`, else the whole document.
            let body = envelope_body.as_ref().or(value.as_ref());
            let usage = |e| NcError::Usage(format!("invalid recipe {}: {e}", p.display()));
            if flow == Flow::New {
                // The new chain's document. Its own migration checks replace the
                // current chain's: those point at `reconstruction.type` and the like,
                // homes this schema does not have.
                if let Some(v) = body {
                    recipe::check_body(v, true, &context)?;
                }
                let r: Recipe = match &envelope_body {
                    Some(v) => serde_json::from_value(v.clone()).map_err(usage)?,
                    None => serde_json::from_str(&txt).map_err(usage)?,
                };
                return Ok(LoadedRecipe {
                    doc: RecipeDoc::New(r),
                    // No `output` section exists in this schema.
                    output_preset_present: false,
                    meta_pipeline_version,
                    // No curve to leave unpinned: the decode is one fixed line.
                    unpinned_curve: None,
                });
            }
            if let Some(v) = body {
                recipe::check_body_without_flag(v, &context)?;
                reject_legacy_recipe_keys(v, &context)?;
            }
            // Bare recipes keep parsing straight from the file text, so their
            // (line/column-bearing) serde diagnostics are unchanged — unless a retired
            // selector was stripped, when the edited document is what must be parsed.
            let cfg = match (&envelope_body, &value) {
                (Some(v), _) => serde_json::from_value(v.clone()).map_err(usage)?,
                (None, Some(v)) if stripped => serde_json::from_value(v.clone()).map_err(usage)?,
                (None, _) => serde_json::from_str(&txt).map_err(usage)?,
            };
            let output_preset_present = body.is_some_and(sets_output_preset);
            Ok(LoadedRecipe {
                doc: RecipeDoc::Current(cfg),
                output_preset_present,
                meta_pipeline_version,
                unpinned_curve: body.and_then(unpinned_curve),
            })
        }
    }
}

/// [`load_recipe_for`] on the current chain — the spelling the unit tests use.
#[cfg(test)]
fn load_recipe(path: Option<&Path>) -> Result<LoadedRecipe> {
    load_recipe_for(path, Flow::Legacy)
}

/// A loaded recipe, in the schema of the chain the run selected.
///
/// One document, never two views of it: the new chain's projection onto the current
/// chain's config ([`Recipe::to_config`]) is derived where it is needed — after the
/// flags merged — rather than stored beside the recipe it would have to agree with.
#[derive(Debug)]
enum RecipeDoc {
    /// The current chain's recipe.
    Current(ResolvedConfig),
    /// The new chain's (`--new-flow`, `crate::recipe`).
    New(Recipe),
}

#[cfg(test)]
impl LoadedRecipe {
    /// The current chain's document; the unit tests load only that one.
    fn cfg(&self) -> &ResolvedConfig {
        match &self.doc {
            RecipeDoc::Current(cfg) => cfg,
            RecipeDoc::New(_) => panic!("a new-chain recipe has no current-chain config"),
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
             must be a non-negative integer no larger than {}. A value Hanten cannot read would be \
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

/// How far a loaded recipe left the density stage unpinned. Each shape gets its own
/// warning, because a different amount moved underneath each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnpinnedCurve {
    /// The recipe omits `reconstruction.curve` entirely, so the **curve itself**
    /// floats — and it moved on 2026-08-08 and again on 2026-09-23.
    WholeCurve,
    /// The recipe states the curve but leaves its `gamma` or `anchor` to this build's
    /// default — both of which have moved: `gamma` 1.0 → 2.0 in
    /// `pipeline_version` 2, and `anchor` `white-at-dmax` → `mid-at-base-offset` in 6.
    ///
    /// Narrower than [`WholeCurve`](Self::WholeCurve) — the curve is the one the
    /// author chose — and easy to miss for exactly that reason: the recipe *looks*
    /// pinned. A recipe reading `{"curve":{"type":"exponential"}}` (the retired tag) or
    /// `{"curve":{}}` has meant three different renders with nothing in the file to show
    /// it.
    MovedDefaults,
    /// The recipe states a `reconstruction` block but leaves `density.scale` unstated,
    /// so the **per-channel gain** floats — and it has moved twice for the
    /// scalar-contrast curves: `pipeline_version` 4 (2026-09-09) took it from
    /// `[1, 1, 1]` to `[1, 0.90, 0.86]`, and 5 (2026-09-16) to `[1, 0.84, 0.73]`.
    ///
    /// The one variant here that is not about the `curve` object, and the reason it is
    /// worth its own message: a recipe can pin every curve knob, *look* fully pinned,
    /// and still replay in a different **colour** rather than a different tone.
    DensityScale,
}

/// Whether a loaded recipe resolves to a density curve it never fully pinned — the
/// witness behind [`curve_default_warning`].
///
/// Probed on the raw JSON because that is the only witness: once serde has filled
/// defaults, a recipe that omitted the curve is indistinguishable from one that
/// wrote today's default out in full.
///
/// **An omitted `curve` counts, since 2026-08-08.** The default curve moved then (to
/// the sigmoid) and again in `pipeline_version` 6 (to the exponential at the fixed
/// decode's configuration), so a `reconstruction` block with no `curve` resolves to a
/// different curve and placement rule than the same file got when it was written. That is exactly the
/// silent reinterpretation design-spec §7.2 promises not to do, and nothing else
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
/// **`reconstruction.density.scale` is the second hole, since `pipeline_version` 4.**
/// The per-channel gain moved `[1, 1, 1]` → `[1, 0.90, 0.86]` (2026-09-09) for the
/// scalar-contrast curves, so a recipe that pins the whole curve object and stays silent
/// on the gain replays in a different **colour**. It is a hole in the same statement
/// the `curve` checks read — an absent `density` block and a `density` block without
/// `scale` are equally unproduceable by `--dump-params`, which writes every key — so
/// it is answered here rather than by a second warning. It is reported *after* the
/// curve findings, which are the more specific diagnosis; a recipe floating both is
/// told about the curve first and about the gain once the curve is pinned.
///
/// Otherwise `None` only for a curve that pins every value this build would otherwise
/// supply — including its `anchor` — beside a stated gain. (A `simple`, `sigmoid` or
/// `characteristic` recipe is refused at load, so it never reaches here.)
fn unpinned_curve(v: &serde_json::Value) -> Option<UnpinnedCurve> {
    let reconstruction = v.get("reconstruction")?;
    let Some(curve) = reconstruction.get("curve") else {
        return Some(UnpinnedCurve::WholeCurve);
    };
    // The rule every check below follows: **warn only on shapes this build cannot
    // produce.** An absent `gamma`, `anchor` or `curve` is never written by
    // `--dump-params`, so a recipe carrying one was written by some other build —
    // which is the whole population the warning is for, and it makes the predicate
    // structurally free of false positives instead of tuned.
    // `recipe_dumped_by_this_build_replays_clean_under_strict` is the gate.
    //
    // An absent reference density was once one of those shapes; the reference retired
    // (`nf-retire/dmax-machinery`), so there is nothing left to float.
    //
    // This build writes both `gamma` and `anchor`, so a curve without either is a shape
    // this build cannot produce. Its retired `type` tag decides nothing: every tag but
    // `exponential` is refused at load.
    let curve_finding = (curve.get("gamma").is_none() || curve.get("anchor").is_none())
        .then_some(UnpinnedCurve::MovedDefaults);
    // The gain is the second moved default (see this function's doc), and it lives
    // beside `curve` rather than inside it. Only an *unstated* one counts: an explicit
    // `[1, 0.84, 0.73]` is what this build writes, so warning on the value rather than
    // on the shape would fail a freshly dumped recipe's own `--strict` replay.
    curve_finding.or_else(|| {
        let scale_stated = reconstruction
            .get("density")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|d| d.contains_key("scale"));
        (!scale_stated).then_some(UnpinnedCurve::DensityScale)
    })
}

/// Warn that an archived recipe will not reproduce its original render, because a
/// curve default moved underneath it.
///
/// The same situation as [`pipeline_version_warning`], one level down: the parameters
/// still apply, but a **default changed underneath them**. Two such moves are covered,
/// and they are reported separately because the remedy differs:
///
/// - [`UnpinnedCurve::WholeCurve`] — the default curve moved twice:
///   `algo/negative-reconstruction-density-curves` (2026-08-08, `pipeline_version` 2)
///   made the sigmoid the default and moved the nominal `Dmax` to 1.3, and
///   `nf-retire/sigmoid-and-simple` (2026-09-23, `pipeline_version` 6) retired the
///   sigmoid for the exponential at the fixed decode's configuration.
/// - [`UnpinnedCurve::MovedDefaults`] — the exponential pinned by type only, whose
///   `gamma` and `anchor` defaults have both moved.
/// - [`UnpinnedCurve::DensityScale`] — `algo/film-stock-profiles` (2026-09-09,
///   `pipeline_version` 4) moved `reconstruction.density.scale` from `[1, 1, 1]` to
///   `[1, 0.90, 0.86]`, and `io/scanner-density-calibration` (2026-09-16,
///   `pipeline_version` 5) to `[1, 0.84, 0.73]` — so a recipe silent on the gain replays
///   in a different colour however completely it pins the curve, and now across two
///   moves rather than one. The second instance of the class
///   `docs/tasks/core/recipe-replay-fidelity.md` tracks.
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
fn curve_default_warning(
    unpinned: Option<UnpinnedCurve>,
    loaded_version: Option<u32>,
) -> Option<String> {
    // A recipe that records **this** build's `pipeline_version` was produced by a
    // build whose defaults are these defaults: nothing moved underneath it, and
    // saying otherwise breaks the documented reproducibility path — a sidecar
    // `--dump-params` just wrote would fail its own `--strict` replay. The warning
    // is about drift *between* versions, and `pipeline_version_warning` already
    // covers the case where they differ.
    //
    // An absent version (a hand-written or pre-envelope recipe) still warns: there
    // is no evidence about which defaults it was written against, which is exactly
    // the uncertainty worth surfacing.
    if loaded_version == Some(version::PIPELINE_VERSION) {
        return None;
    }
    Some(match unpinned? {
        UnpinnedCurve::WholeCurve => "the loaded recipe omits `reconstruction.curve`, so the \
         curve is whichever one this build defaults to — since 2026-09-23 \
         (`pipeline_version` 6) that is the exponential with mid-grey pinned 0.62 density \
         above the film base. The same file written earlier resolved to the \
         mid-grey-anchored sigmoid (from 2026-08-08) or the exponential at gamma 1.0 and \
         Dmax 2.0 (before that), so this render will not match the original. Write \
         `reconstruction.curve` with its `gamma` and `anchor` to pin it."
            .to_string(),
        UnpinnedCurve::MovedDefaults => "the loaded recipe states `reconstruction.curve` \
         but leaves a value to this build's default: the curve's `gamma` went 1.0 → 2.0 on \
         2026-08-08 (`pipeline_version` 2), and its `anchor` went `white-at-dmax` → \
         `{\"mid-at-base-offset\": 0.62}` on 2026-09-23 (`pipeline_version` 6). A recipe \
         that states the curve without both therefore looks pinned and is not — the same \
         file can render differently than it did. Write the curve's `gamma` and its `anchor` \
         explicitly to pin them."
            .to_string(),
        UnpinnedCurve::DensityScale => "the loaded recipe states a `reconstruction` \
         block but leaves `reconstruction.density.scale` to this build's default: the \
         per-channel density gain moved [1, 1, 1] → [1, 0.90, 0.86] on 2026-09-09 \
         (`pipeline_version` 4) and again → [1, 0.84, 0.73] on 2026-09-16 \
         (`pipeline_version` 5) for the scalar-contrast curves — so a recipe \
         silent on the gain now replays differently across two moves, not one. That moves \
         *colour*, not only tone, so this render will not match the original even with the \
         whole curve pinned. Write `reconstruction.density.scale` explicitly to pin it."
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
/// recipe an agent can actually reproduce (`hanten convert --dump-params …` then hash
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
///   replaced by the one tagged `reconstruction` object (design-spec §8);
/// - the top-level `film_base` section, whose contents moved into `calibration`
///   (`core/calibration-recipe-section`). The removed `reconstruction.curve.dmax` is
///   rejected by the curve's own deserializer, which is where its "unknown field" would
///   otherwise be raised;
/// - a stated `calibration.dmax`, the roll reference density that retired with the
///   placements reading it (`nf-retire/dmax-machinery`). Its old default `"fixed"` is
///   stripped before this runs;
/// - a non-neutral regional balance (`nf-retire/regional-balance`), whose neutral
///   defaults are likewise stripped first.
///
/// nc is unreleased, so all of these are rejected, never aliased.
fn reject_legacy_recipe_keys(v: &serde_json::Value, context: &str) -> Result<()> {
    if v.get("film_base").is_some() {
        return Err(NcError::Usage(format!(
            "{context}: top-level `film_base` is no longer supported — the roll's \
             measured values moved into their own `calibration` section, and the \
             `source` wrapper went with them. Replace \
             `\"film_base\": {{\"source\": {{\"explicit\": [r, g, b]}}}}` with \
             `\"calibration\": {{\"film_base\": {{\"explicit\": [r, g, b]}}}}` (also \
             `\"auto\"`, `{{\"region\": [x, y, w, h]}}`). The values and the render are \
             unchanged; only the path moved. A pipeline profile is a recipe with no \
             `calibration` section; a roll calibration is a recipe with nothing else. \
             See design-spec §8."
        )));
    }
    if let Some(value) = v.get("calibration").and_then(|c| c.get("dmax")) {
        return Err(NcError::Usage(format!(
            "{context}: `calibration.dmax` ({value}) was removed: the roll reference \
             density retired with the placements that read it, and the anchor is now \
             placed from the film base (`reconstruction.curve.anchor = \
             {{\"mid-at-base-offset\": <d>}}`). Remove the key — its old default \
             `\"fixed\"` is still accepted, so a sidecar written before the retirement \
             replays."
        )));
    }
    if let Some(message) = v
        .pointer("/reconstruction/density")
        .and_then(|density| removed_balance_recipe_message(density, context))
    {
        return Err(NcError::Usage(message));
    }
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
    if v.get("output").and_then(|o| o.get("hdr")).is_some() {
        return Err(NcError::Usage(format!(
            "{context}: `output.hdr` is no longer supported, and neither is \
             `output.depth`, which replaced it: {}. Remove the key.",
            removed_output_selector("depth").replacement
        )));
    }
    let print = v.get("print");
    if let Some(value) = print.and_then(|p| p.get("display_tone")) {
        return Err(NcError::Usage(removed_display_tone_recipe_message(
            value, context,
        )));
    }
    // Only a non-zero width reaches here; the load path strips the old default.
    if let Some(value) = print.and_then(|p| p.get("highlight_compress")) {
        return Err(NcError::Usage(format!(
            "{context}: recipe key `print.highlight_compress` ({value}) was removed with \
             the `shoulder` display tone, whose knee it placed. The remaining tone, \
             extended Reinhard, has no knee: its shape is `fit_range.headroom_stops` \
             (`--display-tone-headroom`). Remove the key — its old default `0` is still \
             accepted, so a sidecar written before the retirement replays that far."
        )));
    }
    // Only a non-default value reaches here: the load path strips the old defaults
    // first (`strip_retired_keys_at_old_defaults`), so what is left asked for something
    // no preset can do by that name.
    for s in &REMOVED_OUTPUT_SELECTORS {
        if let Some(value) = v.get("output").and_then(|o| o.get(s.key)) {
            return Err(NcError::Usage(format!(
                "{context}: recipe key `output.{}` ({value}) was removed together with the \
                 `legacy` and `custom` output presets, the only ones that read it: {}. \
                 Remove the key — its old default value is still accepted, so a sidecar \
                 written before the retirement replays.",
                s.key, s.replacement
            )));
        }
    }
    if let Some(key) = ["algorithm", "density", "sigmoid", "simple"]
        .into_iter()
        .find(|k| v.get(k).is_some())
    {
        return Err(NcError::Usage(format!(
            "{context}: top-level `{key}` is no longer supported — the reconstruction \
             is one `reconstruction` object (schema_version 1; the `simple` and \
             `sigmoid` algorithms were removed). Put density \
             correction under `reconstruction.density` ({{scale, offset}}) and the curve \
             under `reconstruction.curve` ({{gamma, anchor}}). See design-spec §8."
        )));
    }
    Ok(())
}

/// Whether a recipe/override JSON object explicitly carries
/// `calibration.film_base` — the raw-JSON witness behind `roll`'s per-frame
/// roll-consistency warning. A key probe, not a value comparison, for the same
/// reason as its siblings: an override that merely *restates* the shared base is
/// still a per-frame assertion of a roll calibration.
fn sets_calibration_film_base(v: &serde_json::Value) -> bool {
    v.get("calibration")
        .and_then(|c| c.get("film_base"))
        .is_some()
}

/// Whether a recipe/override JSON object explicitly carries
/// `reconstruction.curve.anchor` — the witness behind `roll`'s roll-consistency warning
/// for the anchor placement, which is a roll-level rule by design (design-spec §7.2): it
/// decides where mid-grey lands, so a per-frame override changes
/// that frame's tonal placement while every other frame keeps the roll's. A raw-JSON probe
/// like [`sets_calibration_film_base`] for the same reason — a restating override is still a
/// per-frame assertion.
fn sets_curve_anchor(v: &serde_json::Value) -> bool {
    v.get("reconstruction")
        .and_then(|r| r.get("curve"))
        .and_then(|c| c.get("anchor"))
        .is_some()
}

/// Whether a recipe/override JSON object explicitly carries `output.preset` — the
/// witness behind `roll`'s roll-consistency warning for the output policy, and behind
/// `convert`'s suffix diagnosis ([`SuffixContext`]). A raw-JSON probe like
/// [`sets_calibration_film_base`], not a resolved-value comparison, because an override that
/// *restates* the shared preset is still a per-frame assertion of the output policy
/// and the roll report has no other place to surface it.
fn sets_output_preset(v: &serde_json::Value) -> bool {
    v.get("output").and_then(|o| o.get("preset")).is_some()
}

/// Resolve `--anchor-mid-offset` into a placement, or `None` if it was not given.
fn anchor_flag_placement(a: &AnchorOverrides) -> Option<AnchorPlacement> {
    a.anchor_mid_offset.map(AnchorPlacement::MidAtBaseOffset)
}

/// Apply CLI overrides on top of a (recipe or default) config; flags win.
///
/// `Option` overrides replace when `Some`, presence-flag booleans
/// (`--auto-base`) replace only when set — a `false` flag never clobbers a
/// recipe `true`, since you disable auto-base by supplying an explicit base, not
/// by passing `false`. (The removed `--algorithm`/simple-control and deprecated
/// input flags are rejected before `merge`, so they never reach here.)
///
/// Fallible because a named value (`--output-preset`) is parsed here, with the
/// diagnosis the recipe key shares.
///
/// A knob with no arm here is a silent no-op flag; each new knob gets a merge test.
pub fn merge(mut cfg: ResolvedConfig, args: &ConvertArgs) -> Result<ResolvedConfig> {
    merge_shared_sections(
        &mut cfg.input,
        &mut cfg.calibration.film_base,
        &mut cfg.measure,
        args,
    );

    // density block: `--density-scale`/`--density-offset` ⇒
    // `reconstruction.density.scale`/`.offset`.
    let Reconstruction { density, curve } = &mut cfg.reconstruction;
    if let Some(v) = args.density.density_scale {
        density.scale = v;
    }
    if let Some(v) = args.density.density_offset {
        density.offset = v;
    }
    // `--density-gamma` and `--anchor-mid-offset` ⇒ the curve's `gamma` and `anchor`.
    if let Some(g) = args.density.density_gamma {
        curve.gamma = g;
    }
    if let Some(p) = anchor_flag_placement(&args.anchor) {
        curve.anchor = p;
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
    // `--display-tone-headroom` ⇒ `fit_range.headroom_stops`.
    if let Some(stops) = args.print.display_tone_headroom {
        cfg.fit_range.headroom_stops = stops;
    }
    // `--linear-range LOW,HIGH` ⇒ `print.linear_range`: an atomic pair (both
    // endpoints at once), so it replaces the recipe's pair entirely. Passing the
    // documented default `0,1` is the flags-win reset of a recipe's non-default
    // pair — allowed, and it is what makes a recipe usable under `film-master`.
    if let Some(v) = args.print.linear_range {
        cfg.print.linear_range = v;
    }

    // output preset: an atomic policy choice, so the flag replaces the recipe's
    // preset entirely. Parsed with the shared `OutputPreset::parse`, so an unknown /
    // removed / renamed name gets the same pinned diagnosis as the recipe key.
    if let Some(name) = &args.output_opts.output_preset {
        cfg.output.preset = OutputPreset::parse(name)?;
    }

    Ok(cfg)
}

/// The flag arms for the sections both chains read — `input`, the film base and
/// `measure` — shared by [`merge`] and [`recipe::merge`], so the two chains cannot
/// resolve the same flag differently.
///
/// The film base is passed as its field rather than the section, because the two
/// chains keep separate `calibration` types ([`CalibrationParams`] and
/// `recipe::Calibration`).
pub(crate) fn merge_shared_sections(
    input: &mut InputParams,
    film_base: &mut Option<FilmBaseSource>,
    measure: &mut MeasureParams,
    args: &ConvertArgs,
) {
    // input color: transfer and meaning are independent axes — each override
    // replaces the recipe's value on its own axis (flags win). The deprecated
    // `--assume-linear` / `--input-profile` flags are handled (rejected) outside
    // `merge`, in `reject_deprecated_input_flags`, before this runs.
    if let Some(t) = args.input_opts.input_transfer {
        input.transfer = t;
    }
    if let Some(m) = args.input_opts.input_meaning {
        input.meaning = m;
    }
    if let Some(t) = args.input_opts.film_type {
        input.film_type = t;
    }
    if let Some(p) = &args.input_opts.export_ir {
        input.export_ir = Some(p.clone());
    }

    // film base: the three source flags are mutually exclusive (clap-enforced);
    // whichever is given replaces the recipe's source entirely.
    if let Some(src) = film_base_source_override(&args.film_base) {
        *film_base = Some(src);
    }

    // measurement region: the static inset. A plain value override — the holder
    // half of the effective area is measured, never configured.
    if let Some(f) = args.measure.measure_inset {
        measure.inset = f;
    }
}

/// Map the (clap-mutually-exclusive) film-base flags to a [`FilmBaseSource`],
/// or `None` when none was passed. Shared by both chains' `convert` merge
/// ([`merge_shared_sections`]) and `estimate`, so they resolve the source identically.
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

/// The **complete** `convert` parameter gate: everything [`validate`] checks, plus every
/// rule that needs a knob's *provenance* rather than its resolved value — the suffix
/// diagnosis, which must know whether anyone actually selected the preset it is about
/// to name ([`reject_output_suffix_mismatch`]).
///
/// `convert` orchestrators must call **this**, not `validate` — a `merge` + `validate`
/// pair silently omits the flag-presence rules. `roll` calls
/// [`validate_with_remedy`]: it has no output flags at all, so there is nothing for the
/// provenance rules above to see.
///
/// **A presence rule rejects a flag only when it forces something the branch cannot
/// produce** (the retired `--out-depth u16` beside an f32-only master). An identity
/// value that renders byte-identically asks for nothing, and refusing it would break
/// the flags-win reset that lets one recipe serve several branches.
///
/// **This also runs under `--new-flow`**, on [`Recipe::to_config`]'s projection, whose
/// `print`/`output` are always defaults. A presence rule reading `cfg.print` or
/// `cfg.output` must be gated on `Flow::Legacy`, or it refuses a flag the new flow
/// keeps.
///
/// **One provenance rule is deliberately *not* here**, so "complete" above means
/// complete for everything reachable after `merge`:
/// [`flow::reject_unavailable_flags`] must run **before** `merge`, because a presence
/// rule placed after it is unreachable on every command line `merge` refuses first.
/// `run_convert` calls it there, and a future `convert` orchestrator owes that call as
/// well as this one. (There is no value half: under `--new-flow` a recipe is the new
/// chain's own document, whose schema refuses at load what a value rule would have.)
/// It is scaffolding: `nf-core/default-flip` deletes it along with `--new-flow`.
pub fn validate_convert(
    cfg: &ResolvedConfig,
    args: &ConvertArgs,
    recipe_preset: RecipePreset,
) -> Result<()> {
    // The output path's suffix is a property of *this invocation*, so it
    // outranks `validate`'s value rules — and specifically outranks the
    // missing-base rule, which `validate` deliberately reports last because an
    // omission is the least specific diagnosis available. Without this ordering,
    // `-o out.jpg --output-preset hdr-pq` with no base demands a base first and
    // only then mentions the suffix, making the user fix two things in series.
    // Under `--new-flow` the rule is judged against the new flow's one destination
    // rather than the (refused) output preset — see [`OutputTarget`].
    reject_output_suffix_mismatch(cfg, args, recipe_preset)?;
    validate(cfg)?;
    Ok(())
}

/// Whether the loaded `--params` recipe stated `output.preset`, as
/// [`validate_convert`] takes it.
///
/// A separate argument rather than something read off [`ResolvedConfig`]: the merge
/// has already erased the difference between "the recipe wrote `gain-map-hdr`" and
/// "nobody wrote anything", and only [`sets_output_preset`]'s raw-JSON probe still
/// knows. Spelled as an enum so the call sites say which they mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipePreset {
    /// The recipe carried an `output.preset` key (whatever value).
    Stated,
    /// No recipe, or a recipe that left `output.preset` out.
    Unstated,
}

/// The resolved container's suffix rule, as `convert`'s gate takes it: a stated
/// suffix is honoured verbatim or refused, an absent one is completed
/// (design-spec §5).
///
/// The gate discards the resolved path — [`run_convert`] calls
/// [`resolve_output_path`] itself for the value, because the completed path is
/// what the sidecar, the write-target guard and the report must all see.
fn reject_output_suffix_mismatch(
    cfg: &ResolvedConfig,
    args: &ConvertArgs,
    recipe_preset: RecipePreset,
) -> Result<()> {
    resolve_output_path(
        &args.output,
        OutputTarget::resolve(Flow::from_flag(args.new_flow), cfg.output.preset),
        convert_suffix_context(args, recipe_preset),
    )
    .map(|_| ())
}

/// Which diagnosis a `convert` suffix failure gets. One spelling, because the
/// gate and [`run_convert`] both need it and a second copy could disagree.
///
/// Blame the preset only when the user actually chose one — from *either*
/// provenance. Since the default became `gain-map-hdr`, "a named preset" no longer
/// implies "a preset was stated": pointing at one nobody selected sends the user
/// looking for a flag that is not in their command line, and telling someone whose
/// recipe says `display-p3` that "with no --output-preset, nc writes `display-p3`"
/// is simply false.
fn convert_suffix_context(args: &ConvertArgs, recipe_preset: RecipePreset) -> SuffixContext<'_> {
    if args.output_opts.output_preset.is_some() || recipe_preset == RecipePreset::Stated {
        SuffixContext::Chosen
    } else {
        SuffixContext::Default
    }
}

/// Where the resolved preset came from, which is what the suffix diagnosis must
/// vary on. Not derivable from the preset value since the default became a *named*
/// one: `gain-map-hdr` reaches [`resolve_output_path`] both ways.
#[derive(Clone, Copy, Debug)]
enum SuffixContext<'a> {
    /// `convert` with an explicit `--output-preset`, or a recipe that set one.
    Chosen,
    /// `convert` with no preset selected — the default resolved it.
    Default,
    /// A `roll` frame whose manifest entry supplied the path.
    RollFrame(&'a Path),
}

/// The path nc will actually write: the path as given when it states a suffix the
/// resolved container accepts, or that path with the container's canonical suffix
/// appended when it states none.
///
/// The rule in one line: **a suffix is never rewritten, only completed.** A
/// trailing dot-segment counts as a suffix only when it is a spelling *some*
/// preset accepts, so `out.tiff` under a JPEG preset is still the usage error it
/// has always been, while `out.v2` and `roll-1.2` are stems and keep their dot
/// (`out.v2.jpg`). **No byte that decides *which file* is named is ever altered or
/// dropped** — which is what keeps `output/presets`' "the output path is never
/// silently renamed" true after this change: nc *completes* a path, it never
/// renames one. Not quite byte-for-byte, and the gap is deliberate: [`Path::with_file_name`]
/// normalises redundant separators and interior `.` segments, so `out//x` resolves
/// to `out/x.jpg`. Those denote the same file, so the claim is about the file, not
/// the spelling. Where a dropped byte *would* change the file — a path naming a
/// directory — the answer is a refusal, never a quiet rewrite; see
/// [`Unappendable`].
///
/// Shared by `convert` and by `roll`'s **explicit** manifest paths, so the two
/// cannot grow parallel rules — which is exactly how the suffix rule and the
/// convert-only refusal drifted apart once before. Roll's **derived** names do not
/// come through here: they are built from [`Container::canonical`] and are correct
/// by construction.
fn resolve_output_path(
    given: &Path,
    target: impl Into<OutputTarget>,
    context: SuffixContext<'_>,
) -> Result<PathBuf> {
    let target = target.into();
    let container = target.container();
    match given.extension() {
        // A spelling some preset claims is a container request, so it is judged.
        Some(ext) if is_container_suffix(ext) => {
            if accepts(container, ext) {
                // Honoured exactly as typed, including a non-canonical spelling and
                // its case: `out.jpeg` stays `.jpeg`, `out.TIF` stays `.TIF`.
                Ok(given.to_path_buf())
            } else {
                Err(suffix_mismatch_error(target, given, context))
            }
        }
        // No dot-segment, or one no preset claims: the whole path is the stem.
        _ => append_suffix(given, container.canonical(), context),
    }
}

/// Whether `container` accepts this spelling, in any case.
fn accepts(container: Container, ext: &OsStr) -> bool {
    container
        .accepted()
        .iter()
        .any(|want| ext.eq_ignore_ascii_case(want))
}

/// Whether `ext` is an output-container spelling **any** preset accepts — the test
/// that tells a suffix from a dotted stem.
///
/// Derived from [`OutputPreset::ALL`] rather than restated: a second list of the
/// same spellings is the thing that would go stale when a container is added.
fn is_container_suffix(ext: &OsStr) -> bool {
    OutputPreset::ALL
        .iter()
        .any(|preset| accepts(container_for(*preset), ext))
}

/// Why a path has no file name to append a suffix to. Two shapes, because the
/// path helpers hide them in *different* ways and only one is self-announcing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unappendable {
    /// `.`, `..`, `/`, `dir/..` — [`Path::file_name`] itself returns `None`.
    NamesNoFile,
    /// `dir/`, `dir//`, `dir/.`, `dir/./` — the path's last *meaningful* component
    /// is a directory, but `file_name()` normalises the trailing separator or `.`
    /// away and hands back the directory's own name, so appending would silently
    /// write that directory's **sibling**.
    NamesADirectory,
}

impl Unappendable {
    /// The clause naming the fault, shared by every context's message.
    fn what(self) -> &'static str {
        match self {
            Self::NamesNoFile => "names no file",
            Self::NamesADirectory => "names a directory rather than a file",
        }
    }

    /// Which shape `given` is, or `None` when it has a file name to append to.
    ///
    /// **Syntactic and pure on purpose** — no `is_dir()` probe, because this runs
    /// inside `validate_convert` *and* roll's planner, where a filesystem stat would
    /// make the answer timing- and platform-dependent. A path that merely *happens*
    /// to name an existing directory (`-o positives`) is therefore still a stem:
    /// `positives.jpg` is unambiguous.
    ///
    /// The directory test reads the string form because `Path` has already thrown
    /// the evidence away by the time `file_name()` answers. Lossy conversion can
    /// neither add nor remove a trailing ASCII separator or `.`, so it is exact for
    /// non-UTF-8 paths too, and `is_separator` covers `\` on Windows. It must match
    /// a trailing `.` **only** when a separator precedes it: `out.` is the
    /// documented degenerate stem (`out..jpg`), not a directory.
    fn of(given: &Path) -> Option<Self> {
        if given.file_name().is_none() {
            return Some(Self::NamesNoFile);
        }
        let shown = given.to_string_lossy();
        let names_dir = shown.ends_with(std::path::is_separator)
            || shown
                .strip_suffix('.')
                .is_some_and(|head| head.ends_with(std::path::is_separator));
        names_dir.then_some(Self::NamesADirectory)
    }
}

/// The diagnosis for a path with nothing to append a suffix to. Like
/// [`suffix_mismatch_error`], every arm names a remedy the *reader's* command line
/// can actually reach — a bare message told a `roll` user to "use `hanten roll
/// --out-dir`" while they were running exactly that, and never said which of 40
/// manifest entries to fix.
fn unappendable_error(
    reason: Unappendable,
    given: &Path,
    ext: &str,
    context: SuffixContext<'_>,
) -> NcError {
    NcError::Usage(match context {
        // A manifest entry named this path, so attribute the frame and name the two
        // remedies *that entry* has. Never `--out-dir`: it is a whole-roll flag the
        // reader has already passed, and it cannot fix one entry.
        SuffixContext::RollFrame(input) => format!(
            "frame {}: the manifest's explicit output {} {}, so Hanten has nothing to \
             append a `.{ext}` suffix to — give the entry's `output` a file name, or drop \
             its `output` key to take the derived name inside the out-dir",
            input.display(),
            given.display(),
            reason.what()
        ),
        // `convert`, either provenance: the remedy is the path in `-o`. The directory
        // case also points at roll, because `--out-dir positives/` is where the
        // trailing separator comes from in the first place.
        _ => {
            let mut msg = format!(
                "the output path {} {}, so Hanten has nothing to append a `.{ext}` \
                 suffix to — give a path ending in a file name",
                given.display(),
                reason.what()
            );
            if reason == Unappendable::NamesADirectory {
                msg.push_str(
                    ", or use `hanten roll --out-dir` to write a whole roll into a directory",
                );
            }
            msg
        }
    })
}

/// `given` with `.<ext>` appended to its file name.
///
/// Appended, never [`PathBuf::set_extension`]: that *replaces*, so it would turn
/// `out.v2` into `out.jpg` and eat a stem the user typed.
///
/// Refused for either [`Unappendable`] shape rather than completed — completing a
/// directory path writes its sibling, which on `roll` puts the whole roll outside
/// the `--out-dir` the user named, at exit 0, with the report agreeing.
fn append_suffix(given: &Path, ext: &str, context: SuffixContext<'_>) -> Result<PathBuf> {
    if let Some(reason) = Unappendable::of(given) {
        return Err(unappendable_error(reason, given, ext, context));
    }
    let mut completed = given
        .file_name()
        .expect("Unappendable::of rejects a path with no file name")
        .to_os_string();
    completed.push(".");
    completed.push(ext);
    Ok(given.with_file_name(completed))
}

/// The diagnosis for a stated suffix the resolved container does not accept.
/// Every arm names a remedy the *reader's* command line can actually reach.
fn suffix_mismatch_error(
    target: OutputTarget,
    output: &Path,
    context: SuffixContext<'_>,
) -> NcError {
    let list = required_extensions(target)
        .iter()
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>()
        .join(" or ");
    let preset = match target {
        OutputTarget::Preset(preset) => preset,
        // One destination and no selector, so there is no preset to blame and no
        // flag to point at — the same sentence whichever command stated the path.
        OutputTarget::NewFlow => {
            let frame = match context {
                SuffixContext::RollFrame(input) => format!("frame {}: ", input.display()),
                SuffixContext::Chosen | SuffixContext::Default => String::new(),
            };
            return NcError::Usage(format!(
                "{frame}the output path {} does not end in {list}: under --new-flow, Hanten \
                 writes its one destination, a Display P3 16-bit TIFF. Hanten never renames \
                 a suffix you state — drop it and the path is completed for you",
                output.display()
            ));
        }
    };
    NcError::Usage(match context {
        // A manifest named this path, so point at the entry to fix and at both ways
        // out (the derived name always matches by construction).
        SuffixContext::RollFrame(input) => format!(
            "frame {}: the manifest's explicit output {} does not match output preset \
             `{}`, which requires {list} — Hanten never renames the path you gave it (drop \
             the entry's suffix to have it completed, or its `output` key to take the \
             derived name instead)",
            input.display(),
            output.display(),
            preset.name()
        ),
        // A preset the user selected — by flag or in the recipe — can be blamed by
        // name; neither wording points at a flag they may not have passed.
        SuffixContext::Chosen => format!(
            "output preset `{}` requires an output path ending in {list} — or no suffix \
             at all, which Hanten completes for you",
            preset.name()
        ),
        // Nothing was typed, so name the *default* explicitly rather than a flag the
        // user never passed — and say how to get a different container, since this is
        // the message someone meets the first time the new default surprises them.
        // An **extensionless** path no longer reaches here at all: it is completed
        // from the resolved container (design-spec §5).
        SuffixContext::Default => format!(
            "the output path {} does not end in {list}: with no --output-preset, Hanten \
             writes `{}` (see --help for the other presets, e.g. `display-p3` for a \
             16-bit TIFF). Hanten never renames a suffix you state — drop it and the path \
             is completed for you",
            output.display(),
            preset.name()
        ),
    })
}

/// The set of file containers nc writes. Which spellings a path may state, and
/// which one nc supplies when it completes or derives a name, both hang off this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Tiff,
    Jpeg,
    Avif,
}

/// The file container a preset's encoder writes.
///
/// **The only preset-shaped step in output-path handling** — but *not* the only
/// preset match that decides the file: the render dispatch in [`convert_frame`]
/// separately picks the encoder that writes the bytes, and the two must agree.
/// Nothing in the type system couples them; the magic-byte assertions in
/// `tests/pipeline.rs`'s `a_bare_output_stem_takes_the_presets_container_and_everything_names_it`
/// are what pin name to bytes.
///
/// Exhaustive on purpose, and it must stay that way: a new preset has to *fail to
/// compile* here rather than inherit a container from a `_` arm or a lookup map.
/// [`OutputTarget::container`] routes every legacy-flow path through this, and
/// [`required_extensions`] and [`derived_extension`] hang off that — so a future
/// destination set changes `OutputTarget` and this function and nothing under them
/// (`nf-destinations/preset-set`). The new flow's arm lives in `OutputTarget`, not
/// here, because it is not a preset.
fn container_for(preset: OutputPreset) -> Container {
    match preset {
        OutputPreset::UltraHdrV1 | OutputPreset::GainMapHdr => Container::Jpeg,
        OutputPreset::HdrPq | OutputPreset::HdrHlg => Container::Avif,
        OutputPreset::HdrLinearTiff
        | OutputPreset::HdrPqTiff
        | OutputPreset::HdrHlgTiff
        | OutputPreset::DisplayP3
        | OutputPreset::Compatibility
        | OutputPreset::FilmMaster => Container::Tiff,
    }
}

impl Container {
    /// Every spelling accepted on a *stated* output path, in any case.
    fn accepted(self) -> &'static [&'static str] {
        match self {
            Self::Tiff => &["tif", "tiff"],
            Self::Jpeg => &["jpg", "jpeg"],
            Self::Avif => &["avif"],
        }
    }

    /// The one spelling nc writes when it supplies the suffix itself — a completed
    /// `convert` path or a derived `roll` name.
    ///
    /// Deliberately **not** `accepted()[0]`: that lists `tif` first, and taking the
    /// head would have renamed every existing roll output from `_positive.tiff`.
    fn canonical(self) -> &'static str {
        match self {
            Self::Tiff => "tiff",
            Self::Jpeg => "jpg",
            Self::Avif => "avif",
        }
    }
}

/// What an output path is judged against: a legacy-flow output preset, or the new
/// flow's one destination.
///
/// Under `--new-flow` the output preset is refused, so judging a path against the
/// resolved (default) preset would blame a preset nobody selected and point at a flag
/// the flow rejects. The new flow's destination is a Display P3 16-bit TIFF
/// (`nf-core/minimal-end-to-end`); `nf-destinations/preset-set` replaces this with the
/// real destination set, whose selection rules are its to settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputTarget {
    Preset(OutputPreset),
    NewFlow,
}

impl OutputTarget {
    /// The target a run's output is judged against.
    fn resolve(flow: Flow, preset: OutputPreset) -> Self {
        match flow {
            Flow::Legacy => Self::Preset(preset),
            Flow::New => Self::NewFlow,
        }
    }

    /// The file container the target's encoder writes.
    fn container(self) -> Container {
        match self {
            Self::Preset(preset) => container_for(preset),
            Self::NewFlow => Container::Tiff,
        }
    }
}

impl From<OutputPreset> for OutputTarget {
    fn from(preset: OutputPreset) -> Self {
        Self::Preset(preset)
    }
}

/// Output-path extensions a preset's resolved container accepts.
///
/// Every preset states a rule — there is no "declines a suffix" case left, and the
/// `Option` this returned before is gone with it: since nc now *derives* a suffix,
/// a preset with no container could not be given a name at all.
fn required_extensions(target: impl Into<OutputTarget>) -> &'static [&'static str] {
    target.into().container().accepted()
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
             once per roll, e.g. with `hanten estimate`), --base-region X,Y,W,H to sample an \
             unexposed border, or --auto-base to detect the rebate band (best-effort: real scans \
             put a thin inset rebate behind the holder, so it can fail). Recipe key: \
             `calibration.film_base`."
            .to_string(),
        // `roll` deliberately does not repeat the flag names as an option: it has
        // none of them, and the first version of this message sent users to flags
        // that exit 2.
        FilmBaseRemedy::SharedRecipe => "no film base selected: `roll` takes no film-base flags, \
             so set `calibration.film_base` in the shared --params recipe. Measuring once per roll is \
             the intended workflow: run `hanten estimate --base-region X,Y,W,H <reference-scan>` on \
             one frame and paste the reported `calibration` object straight in \
             (`\"calibration\": {\"film_base\": {\"explicit\": [R, G, B]}}`) — that is \
             also the only source that keeps every frame on one frozen Dmin. \
             `\"auto\"` and `{\"region\": [X, Y, W, H]}` are accepted \
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
/// override). `convert` has further rules that inspect flag *presence* and therefore
/// cannot live here; [`validate_convert`] composes them with this and is what a
/// `convert` orchestrator must call. A rule that reads only a value belongs here, not
/// there, or `roll` and its per-frame overrides never see it.
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
    match cfg.calibration.film_base {
        Some(FilmBaseSource::Explicit(b)) => validate_explicit_film_base(&b)?,
        Some(FilmBaseSource::Region([_, _, w, h])) if w == 0 || h == 0 => {
            return Err(usage("--base-region width and height must be > 0".into()));
        }
        Some(FilmBaseSource::Region(_)) | Some(FilmBaseSource::Auto) | None => {}
    }

    // Measurement region: a *value* rule, so it lives here rather than in
    // `validate_convert` — `roll` and every per-frame override reach only this
    // gate, and a stage-only check would let a whole roll decode before failing
    // per frame. The bound itself is `types::check_measure_inset`, the one
    // definition `film_base::effective_area` also calls.
    check_measure_inset(cfg.measure.inset)?;

    // Reconstruction: the config's value checks.
    let Reconstruction { density, curve } = &cfg.reconstruction;
    // Density block: per-channel gain must be positive; offset just finite.
    positive("--density-scale", &density.scale)?;
    finite("--density-offset", &density.offset)?;

    positive("--density-gamma", &[curve.gamma])?;

    // Anchor placement. **Ordered after the slope check above, deliberately:** the
    // anchor divides by the slope, and diagnosing that division first reported a zero
    // (or `nan`) slope as "too small to place the anchor" — neither is small. Slope
    // positivity is the more specific diagnosis, so it wins.
    let slope = curve.gamma;
    match curve.anchor {
        // A density above the base, so strictly positive: at 0 mid-grey is pinned on
        // the base itself (the base renders as mid-grey — the whole frame above it),
        // and negative places it below the base where no sample exists.
        AnchorPlacement::MidAtBaseOffset(offset) => {
            positive("--anchor-mid-offset", &[offset])?;
        }
    }
    // The guard is on the **resolved** anchor, not on a proxy quotient, so it cannot
    // drift from what the render will do.
    let anchor = curve.anchor.anchor(slope);
    if !anchor.is_finite() {
        return Err(usage(format!(
            "the resolved anchor placement is not usable: it derives a non-finite \
             anchor ({anchor}) at --density-gamma {slope:e}. The placement divides by \
             the slope, and that quotient overflows f32 for a very small slope. Use \
             a photographic slope"
        )));
    }
    // A finite anchor is not enough. The curve evaluates `slope · (density − anchor)`,
    // and a large *finite* anchor overflows that **product** to −inf, whose `10^` is
    // exactly 0.0 — an all-black frame at exit 0 with no clip and no non-finite
    // count, the same laundering the check above exists to stop.
    // `--anchor-mid-offset 2e38` reaches it at the shipped default gamma, so it needs
    // no exotic slope. The bound is on the *overflow* only: a large offset whose
    // product stays finite (offset 3e38 at gamma 1e-37 is −3e1) is honest arithmetic
    // on absurd input and belongs to `algo/density-safety-bounds`, not here.
    if !(slope * anchor).is_finite() {
        return Err(usage(format!(
            "the resolved anchor placement is not usable: the anchor ({anchor:e}) is \
             finite, but the curve's exponent (slope × (density − anchor)) overflows \
             f32 at --density-gamma {slope:e}, so every sample would render as exactly \
             0.0 — a silently black frame. Use a smaller --anchor-mid-offset, or a \
             smaller slope"
        )));
    }

    // Print: exposure / black point finite; gains positive.
    finite("--print-exposure", &[cfg.print.print_exposure])?;
    finite("--black-point", &[cfg.print.black_point])?;
    // Explicit gains must be positive; the auto modes carry no value to check
    // here (estimated gains are guarded at the estimation point, exit 1).
    if let WbSource::Explicit(gains) = cfg.print.white_balance {
        positive("--white-balance", &gains)?;
    }

    // Range placement: the endpoints divide the affine, so they must be finite,
    // ordered `lo < hi`, and have a representable span — two individually-finite
    // anchors can still overflow their difference to `+inf`, which would silently
    // collapse every sample.
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

    // The display tone's headroom. A **value** rule and therefore here rather than in
    // `validate_convert`: `roll` and every per-frame override reach only `validate`, and
    // without this they decoded the whole roll before `Headroom::new` refused the same
    // number, once per frame. There is deliberately no presence rule beside it:
    // `film-master`'s value sweep refuses a non-default headroom from either provenance,
    // and a presence rule would refuse the flags-win reset to the default.
    crate::types::check_headroom_stops(cfg.fit_range.headroom_stops)?;

    // Last, deliberately: `calibration.film_base` has no default, and `Dmin` is the
    // divisor of the density conversion, so falling into auto-detection by
    // omission decided the most consequential parameter for the user. `--auto-base`
    // is still one flag away — the requirement is that the choice be *stated*, not
    // that it be explicit.
    //
    // It runs after every value and shape rule because it is the least specific
    // diagnosis in the function: a config that both contradicts itself and states
    // no base should be told about the contradiction, which names the two things
    // the user actually typed.
    if cfg.calibration.film_base.is_none() {
        return Err(usage(missing_film_base_message(remedy)));
    }

    Ok(())
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
/// with `--print-exposure 0`).
///
/// Every preset is atomic — container, depth and profile are resolved from it, and
/// no knob states them any more — so the one rule left is `film-master`'s: it bypasses
/// every downstream control, so it rejects a non-default one rather than silently
/// ignoring it. (A rule refusing the display tone
/// on presets that could not carry it retired with the other tones: every display preset
/// applies the one that is left.)
fn validate_output_preset(cfg: &ResolvedConfig) -> Result<()> {
    if cfg.output.preset == OutputPreset::FilmMaster {
        return validate_film_master(cfg);
    }
    Ok(())
}

/// Everything `film-master` refuses — [`validate_output_preset`]'s one rule.
fn validate_film_master(cfg: &ResolvedConfig) -> Result<()> {
    let usage = NcError::Usage;

    // Every non-default downstream control, named individually so the
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
            "--linear-range / print.linear_range",
            *linear_range != d.linear_range,
            format!("{linear_range:?}"),
        ),
        // Not a print control, but a display one the master bypasses just the same.
        (
            "--display-tone-headroom / fit_range.headroom_stops",
            cfg.fit_range.headroom_stops != crate::recipe::FitRange::default().headroom_stops,
            format!("{}", cfg.fit_range.headroom_stops),
        ),
    ]
    .into_iter()
    .find_map(|(name, non_default, value)| non_default.then_some((name, value)));
    if let Some((name, value)) = offender {
        return Err(usage(format!(
            "--output-preset film-master bypasses all print and display controls, but \
             {name} is set to a non-default value ({value}). The master is the \
             unclamped linear ACEScg film rendering, and there is no preset that is \
             the master *plus* a print adjustment — `hdr-linear-tiff` is the float TIFF \
             that applies them, after display rendering. Reset the control \
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
// lcms2 runtime-error handler
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
    eprintln!("hanten: lcms2 error [{code}]: {msg}");
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
            eprintln!("hanten: {msg}");
        }
    }

    /// Warning line — shown unless `--quiet` (the report keeps it either way).
    fn warn(&self, msg: &str) {
        if !self.quiet {
            eprintln!("hanten: warning: {msg}");
        }
    }

    /// Warning line shown *regardless* of `--quiet`. For fail-soft telemetry
    /// failures, which are deliberately kept out of the JSON report (so `--strict`
    /// can't promote them) and would otherwise vanish entirely under `--quiet` —
    /// an opted-in feature failing must never be silent. Ordinary warnings use
    /// [`warn`](Self::warn), which `--quiet` suppresses since the report still
    /// records them.
    fn warn_always(&self, msg: &str) {
        eprintln!("hanten: warning: {msg}");
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
        Command::Params(args) => run_params(&args),
        Command::Convert(args) => run_convert(args),
        Command::Roll(args) => run_roll(args),
        Command::Inspect(args) => run_inspect(args),
        Command::Estimate(args) => run_estimate(args),
        Command::MeasureRoll(args) => run_measure_roll(args),
    }
}

/// `hanten params` — print the full default parameter set as JSON to stdout, in
/// the schema of the chain `--new-flow` selects.
fn run_params(args: &ParamsArgs) -> Result<()> {
    let json = match Flow::from_flag(args.new_flow) {
        Flow::Legacy => serde_json::to_string_pretty(&ResolvedConfig::default()),
        Flow::New => serde_json::to_string_pretty(&Recipe::default()),
    }
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
/// at load by [`reject_legacy_recipe_keys`].
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
///
/// Runs before [`flow::reject_unavailable_flags`], so it fires on **both** chains: every
/// remedy here must name something that also works under `--new-flow`.
fn reject_removed_flags(args: &ConvertArgs) -> Result<()> {
    if let Some(name) = &args.algorithm {
        return Err(NcError::Usage(format!(
            "--algorithm {name} was removed: density is the only reconstruction, and the \
             exponential its only curve (recipe `reconstruction.curve`, design-spec §8). \
             Drop the flag. The `simple` and `sigmoid` algorithms were removed outright."
        )));
    }
    if let Some(message) = removed_characteristic_message(&args.characteristic) {
        return Err(NcError::Usage(message));
    }
    if let Some(value) = &args.reconstruction {
        return Err(NcError::Usage(format!(
            "--reconstruction {value}: {REMOVED_SIMPLE_RECONSTRUCTION}."
        )));
    }
    if let Some((flag, remedy)) = removed_sigmoid_flag(&args.sigmoid) {
        return Err(NcError::Usage(format!(
            "{flag} was removed with the sigmoid curve: {remedy}."
        )));
    }
    if let Some((flag, what)) = removed_dmax_flag(&args.dmax) {
        return Err(NcError::Usage(removed_dmax_message(flag, what)));
    }
    if let Some(flag) = removed_balance_flag(&args.balance) {
        return Err(NcError::Usage(format!(
            "{flag} was removed with the regional balance: {REGIONAL_BALANCE_RETIRED}. \
             Drop the flag. (An equal `--shadow-balance` and `--highlight-balance` was a \
             tone-independent offset: `--density-offset` set to the resolved offset plus \
             that value reproduces it — exactly over a zero offset, otherwise to float \
             rounding.)"
        )));
    }
    // The removed depth switch, and `--out-depth`, which replaced it and has since
    // retired too. The older pair is pointed straight at the preset, not at a flag
    // that no longer exists.
    for (flag, present) in [
        ("--output-hdr", args.output_opts.output_hdr),
        ("--output-sdr", args.output_opts.output_sdr),
    ] {
        if present {
            return Err(NcError::Usage(format!(
                "{flag} was removed, and so was `--out-depth`, which replaced it: {}.",
                removed_output_remedy(removed_output_selector("depth"), args.new_flow)
            )));
        }
    }
    // The retired display tones. The remedy holds on both chains: the headroom flag is
    // the one the new flow keeps, and `reinhard` is what both apply.
    if let Some(value) = &args.print.display_tone {
        let why = match value.as_str() {
            "reinhard" => "extended Reinhard is now the only display tone and is always \
                           applied, so there is nothing to select — drop the flag"
                .to_string(),
            "none" => "the nearest to `none` on a display preset is the identity, \
                 `--display-tone-headroom 0`"
                .to_string(),
            "shoulder" => "`shoulder` has no replacement — drop the flag for the default \
                           tone"
                .to_string(),
            other => format!("`{other}` was never a tone — drop the flag"),
        };
        return Err(NcError::Usage(format!(
            "--display-tone was removed with the `shoulder` and `none` tones (recipe key \
             `print.display_tone`): {why}. The tone's one parameter is \
             `--display-tone-headroom` (recipe `fit_range.headroom_stops`). There is no alias."
        )));
    }
    if args.print.highlight_compress.is_some() {
        return Err(NcError::Usage(
            "--highlight-compress was removed with the `shoulder` display tone, whose knee \
             it placed (recipe key `print.highlight_compress`). The remaining tone, extended \
             Reinhard, has no knee: its shape is `--display-tone-headroom` (recipe \
             `fit_range.headroom_stops`). There is no alias."
                .to_string(),
        ));
    }
    if let Some(s) = REMOVED_OUTPUT_SELECTORS
        .iter()
        .find(|s| (s.present)(&args.output_opts))
    {
        return Err(NcError::Usage(removed_output_flag_message(
            s,
            args.new_flow,
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
                "{flag} was removed: it was never a reconstruction parameter — \
                 the retired `simple` reconstruction ended at the direct unclamped \
                 positive `1 - scan/Dmin`. It is a print control that now \
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

/// The first retired sigmoid flag the user passed, with its remedy.
fn removed_sigmoid_flag(flags: &RemovedSigmoidFlags) -> Option<(&'static str, &'static str)> {
    // Each remedy must also hold under `--new-flow`, which refuses the anchor
    // placements and the display tone the current chain offers.
    const KNEE: &str = "the exponential has no knees, and highlight roll-off belongs to \
                        the display tone (`--display-tone-headroom`)";
    const PLACEMENT: &str = "reference-based placement retired with the reference \
                             density; the exponential's one placement is \
                             `--anchor-mid-offset`, mid-grey a stated density above the \
                             film base";
    [
        (
            "--sigmoid-contrast",
            flags.sigmoid_contrast.is_some(),
            "the exponential's slope is `--density-gamma`; under `--new-flow` that is \
             the film's linearization, and the picture's contrast is `--contrast`",
        ),
        ("--sigmoid-toe", flags.sigmoid_toe.is_some(), KNEE),
        ("--sigmoid-shoulder", flags.sigmoid_shoulder.is_some(), KNEE),
        (
            "--sigmoid-mid-fraction",
            flags.sigmoid_mid_fraction.is_some(),
            PLACEMENT,
        ),
        (
            "--sigmoid-white-at-d-max",
            flags.sigmoid_white_at_d_max,
            PLACEMENT,
        ),
    ]
    .into_iter()
    .find(|(_, present, _)| *present)
    .map(|(flag, _, remedy)| (flag, remedy))
}

/// The first retired reference-density or anchor flag the user passed, with what it
/// did.
fn removed_dmax_flag(flags: &RemovedDmaxFlags) -> Option<(&'static str, &'static str)> {
    const REFERENCE: &str = "set the roll reference density";
    [
        ("--d-max", flags.d_max.is_some(), REFERENCE),
        ("--fixed-d-max", flags.fixed_d_max, REFERENCE),
        ("--auto-d-max", flags.auto_d_max, REFERENCE),
        ("--no-d-max", flags.no_d_max, REFERENCE),
        (
            "--anchor-white-at-reference",
            flags.anchor_white_at_reference,
            "pinned display white at the reference density",
        ),
        (
            "--anchor-mid-fraction",
            flags.anchor_mid_fraction.is_some(),
            "pinned mid-grey at a fraction of the reference density",
        ),
        (
            "--anchor-black-floor",
            flags.anchor_black_floor.is_some(),
            "pinned the film base to an output floor",
        ),
    ]
    .into_iter()
    .find(|(_, present, _)| *present)
    .map(|(flag, _, what)| (flag, what))
}

/// The migration error for a retired reference-density or anchor flag — shared by
/// `convert` and `estimate --d-max-region` so they say the same thing. The remedy is
/// "drop the flag", which holds everywhere.
fn removed_dmax_message(flag: &str, what: &str) -> String {
    format!(
        "{flag} was removed: it {what}. The roll reference density and the placements \
         that read it are gone. Drop the flag: the anchor is placed from the film base, \
         mid-grey `--anchor-mid-offset D` density above it (default 0.62, slope \
         `--density-gamma`)."
    )
}

/// The migration error for a flag retired with the `characteristic` curve, or `None`
/// when none was passed. It fires on both chains, so every remedy is one both accept:
/// drop the flag.
fn removed_characteristic_message(flags: &RemovedCharacteristicFlags) -> Option<String> {
    const REFERENCE: &str = "The old render is reproducible only from the reference build \
                             (`scripts/reference-snapshot/`)";
    if let Some(value) = &flags.density_curve {
        let why = match value.trim().to_ascii_lowercase().as_str() {
            "characteristic" => format!(
                " The `characteristic` curve inverted a film stock's published curve; the \
                 decode is stock-agnostic now. {REFERENCE}."
            ),
            "sigmoid" => " The `sigmoid` retired before it; highlight roll-off belongs to \
                          the display tone (`--display-tone-headroom`)."
                .to_string(),
            _ => String::new(),
        };
        return Some(format!(
            "--density-curve was removed: the exponential is the only density curve (recipe \
             `reconstruction.curve`), so there is nothing to select — drop the flag.{why}"
        ));
    }
    if flags.film_stock.is_some() {
        return Some(format!(
            "--film-stock was removed with the `characteristic` curve it chose a stock for: \
             the decode is stock-agnostic. Drop the flag. {REFERENCE}."
        ));
    }
    if flags.preset.is_some() {
        return Some(format!(
            "--preset was removed: its bundles (`characteristic-generic`, `-stock`, `-aim`, \
             and earlier `sigmoid-knees` / `-flat`) set retired curves with an exposure \
             calibrated to them, and no bundle replaces them. Drop the flag, and set a knob \
             you want directly — `--print-exposure` on the current chain; `--contrast`, \
             `--channel-grade`, `--highlight-desaturation` under `--new-flow` — or collect \
             them in a `--params` recipe. {REFERENCE}."
        ));
    }
    None
}

/// The first removed regional-balance flag present, if any.
fn removed_balance_flag(flags: &RemovedBalanceFlags) -> Option<&'static str> {
    [
        ("--shadow-balance", flags.shadow_balance.is_some()),
        ("--highlight-balance", flags.highlight_balance.is_some()),
        ("--balance-range", flags.balance_range.is_some()),
        ("--auto-balance-range", flags.auto_balance_range),
    ]
    .into_iter()
    .find_map(|(flag, present)| present.then_some(flag))
}

/// Why the regional balance retired and what replaces it — shared by the flag and
/// recipe migration errors, which fire on both chains, so the replacement is named
/// with the chain that has it. The substantive difference is the measurement, not
/// the spelling: say so rather than presenting the grade as a rename.
const REGIONAL_BALANCE_RETIRED: &str = "per-channel density offsets ramped between a \
     shadow and a highlight density, whose `auto` range was measured on every frame (so \
     a roll stayed consistent only by measuring once and replaying the range), and \
     which nothing bounded, so a large enough difference between its ends folded two \
     densities onto one. Its successor is the look's per-channel grade, `--channel-grade \
     R,B` (recipe `look.channel_grade`) under `--new-flow`: pivoted at a fixed mid-grey, \
     so it measures nothing, and bounded so it stays monotone. It acts on the working \
     space's channels rather than on film density, so the old values do not carry over; \
     the current chain has no counterpart";

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
    /// The canonical resolved recipe — `None` under `--new-flow`, which computes none.
    recipe_json: Option<String>,
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
/// shape). `estimate --grid` substitutes its own rectangle.
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
    /// `ultra-hdr-v1` / `gain-map-hdr`: one gain-map render, packaged with the
    /// metadata dialects the preset selected. The render is identical for both —
    /// the presets differ only in `dialects`, which is why it is carried here
    /// rather than re-derived from the preset at the encode site.
    UltraHdr {
        render: Box<gain_map::GainMapRender>,
        dialects: ultra_hdr::Dialects,
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

/// Which chain renders a frame — and, for the new one, the recipe it reads.
///
/// Carried as one value rather than a [`Flow`] beside an `Option<&Recipe>`, so the
/// new chain cannot be selected without the document its decode and stages read.
#[derive(Clone, Copy, Debug)]
enum FrameChain<'a> {
    Legacy,
    New(&'a Recipe),
}

impl<'a> FrameChain<'a> {
    fn of(recipe: Option<&'a Recipe>) -> Self {
        recipe.map_or(FrameChain::Legacy, FrameChain::New)
    }

    fn flow(self) -> Flow {
        match self {
            FrameChain::Legacy => Flow::Legacy,
            FrameChain::New(_) => Flow::New,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn convert_frame(
    command: &'static str,
    input: &Path,
    output: &Path,
    cfg: &ResolvedConfig,
    chain: FrameChain<'_>,
    input_from_cli: InputFromCli,
    // Files this run *read* besides the scan (`--params`, a roll's `--frames`), so a
    // cleanup never removes one — see `render_new_flow_frame`.
    read_inputs: &[&Path],
    budget: memory::Budget,
    memory_out: &mut Option<MemoryReport>,
    log: &Log,
    warnings: &mut Vec<String>,
) -> Result<ConvertedFrame> {
    let flow = chain.flow();
    // The canonical resolved-recipe JSON, resolved up front: it is both the
    // sidecar's `params` body and the input to the identity `params_hash`, so the
    // hash a report advertises is provably the hash of the recipe that ran.
    //
    // Under `--new-flow` the resolved config describes the legacy chain (its
    // `reconstruction`, `print` and `output` sections are defaults nothing reads), so
    // neither its hash nor its echo would identify what ran. What would is the new
    // chain's `Recipe` — hashing and echoing it changes the report's shape, which is
    // `nf-core/report-contract`'s.
    let recipe_json = match flow {
        Flow::Legacy => Some(canonical_params_json(cfg)?),
        Flow::New => None,
    };
    let identity = match &recipe_json {
        Some(json) => Identity::with_params_hash(version::stable_hash(json)),
        None => Identity::new(),
    };

    // `calibration.film_base` has no default, and the gate rejects `None` before any
    // frame runs (`validate_convert` for `convert`, `validate_with_remedy` directly
    // for each `roll` frame). Restating it here keeps this function total rather
    // than relying on an `unwrap` whose safety lives in another module — sharing
    // `missing_film_base_message` so this unreachable spelling cannot drift into a
    // second, thinner diagnosis of the same condition.
    let base_source = cfg.calibration.film_base.clone().ok_or_else(|| {
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
        recipe: (flow == Flow::Legacy).then(|| cfg.clone()),
        film_base_source: Some(base_source.clone()),
        ..Report::default()
    };

    // Stage 0 — memory preflight, on a metadata-only header probe. This must run
    // *before* decode allocates: the whole point is to reject an oversized frame
    // while the heap is still empty (a check after decode would OOM on exactly the
    // inputs it exists to catch). Over budget ⇒ loud exit 6; within budget but
    // most of the machine's RAM ⇒ a `--strict`-promotable warning. Operational
    // gate — it never touches a pixel, so the output stays deterministic.
    let export_ir_planned = cfg.input.export_ir.is_some();
    let mem = preflight_memory(
        input,
        // The new flow renders into one destination whatever `output.preset`
        // resolves (that section is refused under the flag), so its profile is
        // chosen by the flow, not by the preset.
        if flow == Flow::New {
            RunProfile::NewFlowSdrTiff {
                export_ir: export_ir_planned,
            }
        } else {
            match cfg.output.preset {
                OutputPreset::UltraHdrV1 => RunProfile::UltraHdrV1 {
                    export_ir: export_ir_planned,
                },
                OutputPreset::GainMapHdr => RunProfile::GainMapHdr {
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
                OutputPreset::DisplayP3 | OutputPreset::Compatibility => RunProfile::SdrTiff {
                    export_ir: export_ir_planned,
                },
                OutputPreset::FilmMaster => RunProfile::Convert {
                    depth: cfg.output.depth(),
                    export_ir: export_ir_planned,
                },
            }
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
    // `decode_for_roll_white` (`measure-roll`) repeats this front half — preflight,
    // decode, input semantics, positive-mode refusal, effective area and its warnings —
    // up to the chain; a gate added here belongs there too.
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
    // detection: on a scan carrying a **marker-verified** IR plane that measures
    // usable on this frame, and only when the base is being auto-detected (an
    // explicit `--film-base` / `--base-region` runs no detection at all). A
    // shape-only IR plane (unverified provenance) is not trusted, so it degrades to
    // RGB-only. These govern the two fallback notes below; whether the plane was
    // *actually* consumed is read off stage 2 afterwards, not predicted here.
    let auto_base = matches!(base_source, FilmBaseSource::Auto);
    let ir_shape_only = info.ir_present && !image.ir_verified;
    // Whether the IR plane can separate holder from film **on this frame**. A
    // measurement, not the `--film-type chromogenic` declaration that used to gate
    // this (`ir-usability-detection`): chemistry mispredicts separability in both
    // directions, so the plane is asked directly. `None` when there is no IR plane.
    //
    // Measured here even though `ir_holder_mask` measures again inside stage 2: the
    // fallback note below must fire even when `estimate` then *errors* — auto
    // refusing to find a rebate is exactly when "the IR plane could not help" is
    // worth reading — and a failed stage 2 returns no `BaseEstimate` to carry it.
    // Both calls are bounded strided samples, so the duplication is ~100k reads.
    let ir_separability = film_base::ir_separability(&image);
    let ir_usable = ir_separability.is_some_and(|s| s.usable);

    // When holder detection wanted the IR mask but the plane is shape-only, it
    // silently degraded to RGB-only — say so (a `--strict`-promotable warning, like
    // the no-IR note). Emitting it here means the generic "carried but unused" note
    // below is skipped for the same plane, so only one IR note fires. Suppressed
    // under `--export-ir` for the same reason that note is: the user is taking the
    // plane themselves, and failing their `--strict` run over a detection path that
    // fell back to the one every 48-bit scan uses silently would be wrong.
    let shape_only_holder_note = auto_base && ir_shape_only && export_ir.is_none();
    if shape_only_holder_note {
        push_warning_buf(
            warnings,
            log,
            "an IR plane is present, but it is identified by shape alone (no \
             NewSubfileType=4 marker) and not trusted for holder detection; using \
             RGB-only film-holder detection for the film base"
                .into(),
        );
    }

    // Trusted IR that still cannot do the job: this frame's own film is too opaque
    // to tell from the holder (a fully-exposed frame on silver stock, say). Say so
    // with the measurement, so the fallback is diagnosable rather than silent.
    let ir_unusable_note =
        auto_base && info.ir_present && image.ir_verified && !ir_usable && export_ir.is_none();
    if ir_unusable_note {
        push_warning_buf(
            warnings,
            log,
            format!(
                "the IR plane cannot separate the film holder on this frame (interior \
                 IR transmission {:.4}); using RGB-only film-holder detection for the \
                 film base",
                ir_separability.map_or(0.0, |s| s.interior_median)
            ),
        );
    }

    // Stage 2 — film-base estimate. Resolved before the render so its quality
    // warnings (non-uniform region, cross-edge disagreement) are pushed — and so
    // echoed to stderr — *before* the fallible render runs, and ride out in the
    // JSON report on a successful run. (A hard render failure propagates its error
    // and exit code like every other error path and emits no report; the stderr
    // warnings still stand.)
    let stage_started = Instant::now();
    let base = film_base::estimate(&image, &base_source)?;
    let film_base_ms = elapsed_ms(stage_started);
    report.film_base = Some(base.base);
    for w in base.warnings {
        push_warning_buf(warnings, log, w);
    }

    // The effective measurement area, resolved **unconditionally** and always
    // reported: that is what makes `--measure-inset` observable rather than
    // accepted-and-ignored, which the project forbids. The march costs less than
    // run-to-run noise even on an 18.7 MP frame, so there is nothing to save by
    // skipping it.
    //
    // Nothing in a `convert` measures over it today — its one per-frame consumer, the
    // auto reference density, retired (`nf-retire/dmax-machinery`), and on the new flow
    // the roll's white balance is measured over it by `hanten measure-roll` instead —
    // so an **empty** region is a warning rather than a refusal, with no
    // `report.effective_area`, because there is no region to report. A consumer added
    // here must decide whether an empty region becomes fatal for it.
    match film_base::effective_area(&image, cfg.measure.inset) {
        Ok(area) => {
            report.effective_area = Some(area);
            for w in film_base::effective_area_warnings(&area) {
                push_warning_buf(warnings, log, w);
            }
        }
        // Rebuilt from the error's own text rather than `Display`, which prefixes the
        // kind (`usage: …`) — a warning must not carry it.
        Err(e) => {
            push_warning_buf(
                warnings,
                log,
                format!(
                    "{} Nothing in this conversion measures over the region, so the \
                     render is unaffected and the report omits `effective_area` \
                     (--measure-inset has no effect on this run).",
                    e.message()
                ),
            );
        }
    }

    // Note an IR plane that's carried but not consumed. Keyed on what each stage
    // actually did, never on a prediction from the inputs: a marker-verified plane
    // that measures usable can still produce no mask (the all-holder fallback), and
    // predicting consumption silently suppressed this warning — and so `--strict` —
    // on exactly that case.
    //
    // The measurement area never suppresses it: nothing in the render reads the region,
    // so a marched holder (`effective_area.holder_applied`) reaches no pixel. That is
    // also why the wording is about the **conversion** and names `effective_area`:
    // `convert` resolves the area unconditionally, so a run legitimately reports a
    // measured `holder_applied: true` beside this note.
    //
    // Not emitted when the plane is being exported
    // (`--export-ir` is the user handling it, so warning — and failing under
    // `--strict` — would be wrong; keeps `--strict --export-ir` usable on the
    // primary HDRi format), and not when one of the two fallback notes above
    // already covered this plane.
    if info.ir_present
        && export_ir.is_none()
        && !base.ir_mask_applied
        && !shape_only_holder_note
        && !ir_unusable_note
    {
        push_warning_buf(
            warnings,
            log,
            "input carries an IR plane; it is preserved but not used in the \
             conversion — no rendered pixel depends on it, whatever \
             `effective_area.holder` measured (use --export-ir to write it out)"
                .into(),
        );
    }

    // The migration seam. Decode and film base are shared by both flows — the new
    // design keeps them — so the branch belongs here, at the render.
    if let FrameChain::New(recipe) = chain {
        return render_new_flow_frame(
            NewFlowFrame {
                recipe,
                image,
                base: base.base,
                export_ir,
                output,
                report,
                info,
                decode_ms,
                film_base_ms,
                read_inputs,
            },
            log,
            warnings,
        );
    }
    // Clear any stale lcms2 flag so only errors from *this* render are counted.
    let _ = cms_error_occurred();
    // Stages 3–5 — reconstruction → the preset's branch.
    // Exhaustive on the preset, deliberately: the *container* is what selects the
    // branch, and two presets can share a transfer (`hdr-pq` and `hdr-pq-tiff` render
    // an identical rendition into different containers). An `if let Some(transfer) =
    // transfer_for(..)` chain would silently hand the TIFF presets to the AVIF
    // encoder, so the compiler is made to enumerate the cases instead.
    //
    // The display tone is resolved once, before any render: an unusable headroom then
    // fails without having paid for the reconstruction and print stage. `film-master`
    // applies none, and `validate` has held its headroom at the default.
    let tone = Headroom::new(cfg.fit_range.headroom_stops)?;
    let rendered = match cfg.output.preset {
        OutputPreset::UltraHdrV1 | OutputPreset::GainMapHdr => {
            // One render for both gain-map presets. They differ only in which
            // metadata dialects get attached during packaging, so resolving the
            // dialect here keeps the two from drifting into two renders.
            //
            // Exhaustive, like the SDR gamut and coded/AVIF splits below and for the
            // same reason: a `_` arm would silently hand a future gain-map preset
            // whichever dialect happened to be last, and the difference between the
            // two is exactly whether the file is HDR on Apple platforms.
            let dialects = match cfg.output.preset {
                OutputPreset::UltraHdrV1 => ultra_hdr::Dialects::LegacyUltraHdrV1,
                OutputPreset::GainMapHdr => ultra_hdr::Dialects::LegacyPlusIso,
                OutputPreset::FilmMaster
                | OutputPreset::HdrPq
                | OutputPreset::HdrHlg
                | OutputPreset::HdrLinearTiff
                | OutputPreset::HdrPqTiff
                | OutputPreset::HdrHlgTiff
                | OutputPreset::DisplayP3
                | OutputPreset::Compatibility => {
                    return Err(NcError::Other(format!(
                        "`{}` reached the gain-map render arm, which only the \
                         gain-map presets may enter",
                        cfg.output.preset.name()
                    )));
                }
            };
            let source =
                stages::render_display_source(&image, &base.base, &cfg.reconstruction, &cfg.print)?;
            let convert = source.convert;
            let mut timings = source.timings;
            let display_started = Instant::now();
            let render =
                gain_map::render(&source.shared, gain_map::GainMapConfig::ultra_hdr_v1(tone))?;
            // Both independent SDR/HDR display renders plus the common-domain gain
            // construction are color work. Keep them out of encode_ms so stage
            // totals account for every pixel operation even when telemetry is off.
            timings.color_ms += elapsed_ms(display_started);
            FrameRender::UltraHdr {
                render: Box::new(render),
                dialects,
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
            let render = hdr::render_linear(&source.shared, tone)?;
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
            let render = hdr::render(&source.shared, transfer, tone)?;
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
                OutputPreset::FilmMaster
                | OutputPreset::UltraHdrV1
                | OutputPreset::GainMapHdr
                | OutputPreset::HdrLinearTiff
                | OutputPreset::DisplayP3
                | OutputPreset::Compatibility => {
                    return Err(NcError::Other(format!(
                        "`{}` reached the shared Rec.2100 render arm, which only the \
                         AVIF and coded-TIFF presets may enter",
                        cfg.output.preset.name()
                    )));
                }
            }
        }
        OutputPreset::DisplayP3 | OutputPreset::Compatibility => {
            // Both SDR presets are one render differing only in destination gamut,
            // written through the general TIFF encode at 16 bits.
            //
            // Exhaustive, like the coded/AVIF split above and for the same reason: a
            // `_` arm would hand a future SDR preset the sRGB gamut silently, and it
            // would render sRGB while the report named its own space.
            let gamut = match cfg.output.preset {
                OutputPreset::DisplayP3 => sdr::SdrGamut::DisplayP3,
                OutputPreset::Compatibility => sdr::SdrGamut::SRgb,
                OutputPreset::FilmMaster
                | OutputPreset::UltraHdrV1
                | OutputPreset::GainMapHdr
                | OutputPreset::HdrPq
                | OutputPreset::HdrHlg
                | OutputPreset::HdrLinearTiff
                | OutputPreset::HdrPqTiff
                | OutputPreset::HdrHlgTiff => {
                    return Err(NcError::Other(format!(
                        "`{}` reached the SDR render arm, which only the SDR display \
                         presets may enter",
                        cfg.output.preset.name()
                    )));
                }
            };
            FrameRender::Tiff(stages::render_sdr_preset(
                &image,
                &base.base,
                &cfg.reconstruction,
                &cfg.print,
                tone,
                gamut,
            )?)
        }
        OutputPreset::FilmMaster => FrameRender::Tiff(stages::render_film_master(
            &image,
            &base.base,
            &cfg.reconstruction,
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
    report.white_balance = convert.white_balance;
    report.reconstruction_result = Some(reconstruction_result(
        &cfg.reconstruction,
        convert.curve_anchor,
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

    // Report a BigTIFF promotion — always an automatic decision now that no knob
    // requests one.
    if let FrameRender::Tiff(rendered) = &rendered
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
        // Use the preset's resolved `depth()` for the IR TIFF. u16 for the gain-map
        // presets (fixed 8-bit JPEG primary), `hdr-pq`/`hdr-hlg` (10-bit AVIF
        // primary), and the SDR and coded HDR TIFFs (whose primary is itself u16);
        // f32 for `film-master` and `hdr-linear-tiff` — and `hdr-linear-tiff`'s f32
        // IR is why `RunProfile::HdrLinearTiff` charges nothing for the export. The IR
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
        FrameRender::UltraHdr {
            render, dialects, ..
        } => ultra_hdr::encode_with(*render, output, dialects)?,
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
            let (staged, outcome, summary) = encode::encode_hdr_coded(*render, &icc, output)?;
            hdr_coded_summary = Some(Box::new(summary));
            (staged, outcome)
        }
        FrameRender::HdrLinearTiff { render, .. } => {
            // The profile is resolved here, not inside the encoder, so the embedded
            // blob is provably the one the orchestrator chose — the same rule the
            // general TIFF arm follows with `rendered.icc`.
            let icc = color::hdr_linear_bt2020_icc()?;
            let (staged, outcome, summary) = encode::encode_hdr_linear(*render, &icc, output)?;
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
            rendering: AvifRenderingResult {
                reference_white_nits: summary.metadata.linear.reference_white_nits,
                target_peak_nits: summary.metadata.linear.target_peak_nits,
                linear_headroom: summary.metadata.linear.linear_headroom,
                tone_curve: summary.metadata.linear.tone_curve,
                gamut_mapping: summary.metadata.linear.gamut_mapping,
                linear_domain: summary.metadata.linear.linear_domain,
                hlg_system_gamma: summary.metadata.hlg_system_gamma,
                hlg_reference_display_peak_nits: summary.metadata.hlg_reference_display_peak_nits,
                hlg_reference_display_black_nits: summary.metadata.hlg_reference_display_black_nits,
            },
        });
    }
    if let Some(summary) = hdr_coded_summary {
        // Same reason as the linear TIFF below: the profile is built inside the
        // encode arm, so the `auto` BigTIFF promotion is reported from what the
        // encoder resolved rather than predicted before it.
        if summary.bigtiff {
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
            tone_curve: metadata.linear.tone_curve,
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
        if summary.bigtiff {
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
    report_encode_outcome(&mut report, &outcome, log, warnings);

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

/// Fold an encode's loss and output statistics into the report, warning on any
/// loss. Shared by both flows' encode sites so the loss is described one way.
fn report_encode_outcome(
    report: &mut Report,
    outcome: &EncodeOutcome,
    log: &Log,
    warnings: &mut Vec<String>,
) {
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
            "hanten: warning: {} non-finite (NaN/inf) output sample(s) — numerical fault",
            loss.non_finite
        );
    }
}

/// Whether `path` holds one of nc's sidecars, recognised by its provenance rather
/// than by key names: the `{meta, params}` envelope with `params` an object and
/// `meta` carrying the identity every sidecar stamps (`nc_version`,
/// `pipeline_version`, `target`). Deleting is destructive, so anything short of that
/// — missing, unreadable, or a different file that merely shares the shape — is not
/// ours to remove.
fn is_nc_sidecar(path: &Path) -> bool {
    let Some(doc) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    let meta = &doc["meta"];
    doc.as_object().is_some_and(|o| o.len() == 2)
        && doc["params"].is_object()
        && meta["nc_version"].is_string()
        && meta["pipeline_version"].is_u64()
        && meta["target"].is_string()
}

/// The new flow's one destination, as the report names it. Provisional —
/// `nf-destinations/preset-set` owns the set and its names.
const NEW_FLOW_DESTINATION: &str = "display-p3-u16-tiff";

/// The gamut the new flow's one destination renders into.
const NEW_FLOW_GAMUT: DestinationGamut = DestinationGamut::DisplayP3;

/// The peak fit range compresses against for that destination: an SDR display.
const NEW_FLOW_PEAK: DisplayPeak = DisplayPeak::SDR;

/// What [`convert_frame`] has resolved by the time the new flow's render takes
/// over: everything up to and including the film base, which both flows share.
struct NewFlowFrame<'a> {
    /// The new chain's recipe: what the decode and the chain read.
    recipe: &'a Recipe,
    image: LinearImage,
    base: FilmBase,
    export_ir: Option<PathBuf>,
    output: &'a Path,
    report: Report,
    info: DecodeInfo,
    decode_ms: f64,
    film_base_ms: f64,
    read_inputs: &'a [&'a Path],
}

/// The new flow's render, encode and commit for one frame: the fixed decode
/// (`algo::fixed`) → NC film RGB v1 → `pipeline::chain` → its one destination, a
/// Display P3 16-bit TIFF (`nf-core/minimal-end-to-end`).
///
/// The same staging discipline as the legacy path — the optional IR export and the
/// primary are staged, then committed together with the primary last — but **no
/// sidecar** yet: the one it would write is the new chain's `Recipe`, and changing
/// what a sidecar holds is `nf-core/report-contract`'s. The report says so in
/// `new_flow`.
fn render_new_flow_frame(
    frame: NewFlowFrame<'_>,
    log: &Log,
    warnings: &mut Vec<String>,
) -> Result<ConvertedFrame> {
    let NewFlowFrame {
        recipe,
        image,
        base,
        export_ir,
        output,
        mut report,
        info,
        decode_ms,
        film_base_ms,
        read_inputs,
    } = frame;
    let decode_params = recipe.reconstruction;
    let chain_params = recipe.chain_params(NEW_FLOW_PEAK, NEW_FLOW_GAMUT);

    // Reconstruction: the fixed decode, then the pinned NC film RGB v1 3×3.
    let stage_started = Instant::now();
    let (film, decoded) = fixed::decode(&image, &base, &decode_params)?;
    let aces = working_space::map_nc_film_rgb_v1(film);
    let algorithm_ms = elapsed_ms(stage_started);

    // The chain, then the destination's transfer. Clear any stale lcms2 flag first so
    // only a fault from *this* transform is counted.
    let _ = cms_error_occurred();
    let stage_started = Instant::now();
    let rendered = chain::render(aces, &chain_params)?;
    let (linear, gamut) = rendered.image.into_parts();
    let (encoded, icc) = color::encode_display_linear(linear, gamut)?;
    let color_ms = elapsed_ms(stage_started);
    if cms_error_occurred() {
        return Err(NcError::Other(
            "color management (lcms2) reported a runtime error; see stderr".into(),
        ));
    }

    // The NC film RGB v1 3×3 into ACEScg runs on this flow too, so the pinned
    // interpretation is a fact about the run.
    report.working_mapping = Some(working_space::WORKING_MAPPING_ID);
    report.new_flow = Some(NewFlowResult {
        decode: decoded,
        stages: rendered
            .applied
            .map(|(stage, applied)| NewFlowStageResult { stage, applied }),
        scene_correction: rendered.scene_correction,
        look: rendered.look,
        fit_range: rendered.fit_range,
        destination: NEW_FLOW_DESTINATION,
        gamut: gamut.name(),
        sidecar_written: false,
        removed_sidecar: None,
    });

    // The IR export reads the *decoded* image and is staged before the primary, at
    // the destination's depth (u16), as on the legacy path.
    let mut pending: Vec<staged::Staged> = Vec::new();
    let mut ir_export_ms = None;
    if let Some(path) = &export_ir {
        let stage_started = Instant::now();
        pending.push(encode::export_ir(&image, OutDepth::U16, path)?);
        ir_export_ms = Some(elapsed_ms(stage_started));
        report.ir_exported = Some(path.clone());
    }

    let stage_started = Instant::now();
    let (primary, outcome, bigtiff) = encode::encode_u16(&encoded, &icc, output)?;
    let encode_ms = elapsed_ms(stage_started);
    if bigtiff {
        push_warning_buf(
            warnings,
            log,
            "output promoted to BigTIFF (would exceed the classic 4 GiB TIFF limit)".into(),
        );
    }
    report_encode_outcome(&mut report, &outcome, log, warnings);

    // The primary goes last, as on the legacy path: its presence is what reads as
    // success.
    pending.push(primary);
    for note in staged::commit_all(std::mem::take(&mut pending))? {
        push_warning_buf(warnings, log, note);
    }
    if let Some(path) = &export_ir {
        log.info(format_args!("wrote IR plane {}", path.display()));
    }
    log.info(format_args!("wrote {}", output.display()));

    // A sidecar an earlier run wrote beside this path now describes an image that no
    // longer exists — reloading it would reproduce a different picture. Removed only
    // after the new image is committed, and only when it is recognisably one of nc's
    // sidecars, so a user's own file that happens to share the name is left alone.
    let stale = encode::sidecar_path(output);
    let stale_key = collision_key(&stale);
    let is_read_input = read_inputs
        .iter()
        .any(|input| keys_collide(&collision_key(input), &stale_key));
    if is_read_input && is_nc_sidecar(&stale) {
        // This run's own recipe (or manifest) sits where the old sidecar would: it is
        // an input, not a stale artifact, so it stays — loudly, since it still pairs
        // by name with an image it no longer describes.
        push_warning_buf(
            warnings,
            log,
            format!(
                "{} pairs by name with this output but describes the image a legacy run \
                 wrote there; it was left in place because this run read it (--params / \
                 --frames)",
                stale.display()
            ),
        );
    } else if is_nc_sidecar(&stale) {
        // A warning, not an error: the new image is already committed, and failing
        // the run here would discard the report that describes it.
        match std::fs::remove_file(&stale) {
            Ok(()) => {
                log.info(format_args!("removed stale sidecar {}", stale.display()));
                if let Some(nf) = report.new_flow.as_mut() {
                    nf.removed_sidecar = Some(stale);
                }
            }
            Err(e) => push_warning_buf(
                warnings,
                log,
                format!(
                    "could not remove the stale sidecar {} (it describes the image this \
                     run replaced): {e}",
                    stale.display()
                ),
            ),
        }
    }

    report.warnings = std::mem::take(warnings);
    Ok(ConvertedFrame {
        report,
        info,
        recipe_json: None,
        timings: telemetry::TimingInfo {
            total: 0.0,
            decode: decode_ms,
            film_base: film_base_ms,
            algorithm: algorithm_ms,
            color: color_ms,
            encode: encode_ms,
            ir_export: ir_export_ms,
        },
        loss: outcome.loss,
    })
}

/// `hanten convert` — the full pipeline: decode → film-base → algorithm → output
/// color transform → encode (+ sidecar, + optional IR export). Warnings are
/// collected into the report and echoed to stderr; `--strict` promotes any of
/// them to a non-zero exit.
fn run_convert(args: ConvertArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    reject_deprecated_input_flags(&args.input_opts)?;
    reject_removed_flags(&args)?;
    let flow = Flow::from_flag(args.new_flow);
    // Presence-keyed availability runs **before** `merge`, deliberately: `merge`
    // refuses some command lines itself, and a rule placed after it is unreachable
    // on exactly those — handing the user a remedy that names a knob this flow
    // rejects.
    flow::reject_unavailable_flags(flow, &args)?;
    // The third provenance the two availability tables cannot see — a recipe
    // written for the other chain — is refused inside the load, before `merge`
    // reasons about values the selected chain never reads.
    let loaded = load_recipe_for(args.recipe_in.as_deref(), flow)?;
    let recipe_preset = if loaded.output_preset_present {
        RecipePreset::Stated
    } else {
        RecipePreset::Unstated
    };
    // Under `--new-flow` the flags merge into the new chain's recipe, whose decode
    // section the current chain's `merge` does not have; `cfg` is then its projection,
    // for the stages both chains run. The decode's value rules run here, ahead of
    // `validate_convert`, whose last rule — no film base chosen — is the least
    // specific diagnosis there is.
    let (cfg, new_recipe) = match loaded.doc {
        RecipeDoc::Current(doc) => (merge(doc, &args)?, None),
        RecipeDoc::New(r) => {
            let r = recipe::merge(r, &args);
            let cfg = r.to_config();
            recipe::validate(&r, KnobNames::FlagAndKey)?;
            (cfg, Some(r))
        }
    };
    // The *complete* convert gate: `validate`'s resolved-config rules plus the two
    // provenance-sensitive rules that cannot live there (see `validate_convert`).
    validate_convert(&cfg, &args, recipe_preset)?;

    // The path nc actually writes: `-o out` under the default becomes `out.jpg`.
    // Resolved **here**, before anything derives from it — the write-target guard,
    // the sidecar, the report's `output` and telemetry's `output_bytes` must all
    // see the completed path, never the stem. (`validate_convert` ran the same rule
    // and discarded the value; this is the one call that keeps it.) Under
    // `--new-flow` the container is the new flow's one destination's — a TIFF.
    let target = OutputTarget::resolve(flow, cfg.output.preset);
    let output = resolve_output_path(
        &args.output,
        target,
        convert_suffix_context(&args, recipe_preset),
    )?;
    if output != args.output {
        let from = match target {
            OutputTarget::Preset(preset) => format!("the resolved preset `{}`", preset.name()),
            OutputTarget::NewFlow => "the new flow's destination".to_string(),
        };
        log.info(format!(
            "output path completed from {from}: writing {}",
            output.display()
        ));
    }

    // Guard every write target against the input and against each other before
    // anything is decoded or written.
    let sidecar = encode::sidecar_path(&output);
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
    let mut targets: Vec<(&str, &Path)> = vec![("--output", &output)];
    // No sidecar is written under `--new-flow` (its `params` would describe a chain
    // the run did not select, and could not be reloaded — the `--dump-params`
    // reasoning), so it is no write target there.
    if flow == Flow::Legacy {
        targets.push(("the sidecar", &sidecar));
    }
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

    // The recipe of the chain the run selected — under `--new-flow`, the new chain's
    // document, which reloads under the same flag to the same recipe.
    if let Some(path) = &args.dump_params {
        match &new_recipe {
            Some(r) => write_json(path, r, &log)?,
            None => write_json(path, &cfg, &log)?,
        }
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
    if let Some(msg) = curve_default_warning(loaded.unpinned_curve, loaded.meta_pipeline_version) {
        push_warning_buf(&mut warnings, &log, msg);
    }
    let frame = convert_frame(
        "convert",
        &args.input,
        &output,
        &cfg,
        FrameChain::of(new_recipe.as_ref()),
        InputFromCli {
            transfer: args.input_opts.input_transfer.is_some(),
            meaning: args.input_opts.input_meaning.is_some(),
        },
        &args
            .recipe_in
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>(),
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
    // `recipe_json` is `None` only under `--new-flow`, which refuses the telemetry
    // flags before anything runs, so the pair below is the whole condition.
    if telemetry_requested(&args)
        && !strict_failure
        && let Some(recipe_json) = &recipe_json
    {
        // `convert_frame` measured the per-stage wall clocks; the total is this
        // orchestrator's whole-run clock.
        let mut timings = stage_timings;
        timings.total = total_ms;
        emit_telemetry(
            &args,
            &output,
            &cfg,
            &info,
            timings,
            loss,
            recipe_json,
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
    /// Under `--new-flow`, the frame's own recipe — the shared one with any per-frame
    /// override applied — which the decode and the chain read; `cfg` is then its
    /// projection, for the stages both chains run.
    recipe: Option<Recipe>,
    /// The per-frame override applied (manifest `params`), echoed into the roll
    /// report so a reader sees exactly what differed for this frame; `None` when
    /// the frame ran the shared recipe unchanged.
    overrides: Option<serde_json::Value>,
}

/// The roll-level JSON report emitted on stdout (or `--report-file`): the shared
/// frozen recipe *configuration* once, any roll-level warnings, the per-frame
/// status list, and a summary. The shared recipe here is the config every frame
/// was converted from; each frame additionally reports the *resolved* base
/// it used (a redundant echo when the recipe pins an explicit base, meaningful
/// under an `auto`/`region` base that resolves per frame).
#[derive(Debug, Serialize)]
struct RollReport {
    command: &'static str,
    /// What produced this batch: build identity + the behavioral
    /// `pipeline_version` + the `params_hash` of the **shared** frozen recipe
    /// (`core/conversion-versioning`). Unconditional here (unlike `Report.identity`)
    /// because a roll always resolves a full recipe — though under `--new-flow` the
    /// hash is omitted, for the reason `recipe` is. Operational provenance only —
    /// no CLI flag, no recipe key, no effect on a single output pixel.
    identity: Identity,
    /// The shared frozen recipe configuration every frame was converted from —
    /// where the roll-fixed `film_base` config lives, once.
    /// Omitted under `--new-flow`, as on `convert`: this field's type is the legacy
    /// chain's config, and echoing the new chain's `Recipe` instead is
    /// `nf-core/report-contract`'s.
    #[serde(skip_serializing_if = "Option::is_none")]
    recipe: Option<ResolvedConfig>,
    /// Roll-level warnings not tied to a single frame (e.g. the film base is not
    /// frozen because the shared recipe's `calibration.film_base` is not `explicit`).
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
    /// A converted frame: the resolved values it used (mirrors the relevant
    /// single-frame [`Report`] fields). Each is `None`/omitted when the settings
    /// didn't produce it.
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        film_base: Option<FilmBase>,
        #[serde(skip_serializing_if = "Option::is_none")]
        white_balance: Option<[f32; 3]>,
        /// The resolved effective measurement area — mirrors the single-frame
        /// `Report` field. Per-frame rather than roll-level: one shared
        /// `measure.inset` meets a holder depth that is genuinely this frame's
        /// own, and a per-frame `params` override can change the inset too. It is
        /// also what makes `measure.inset` observable on `roll` at all, the same
        /// reason `convert` reports it unconditionally.
        ///
        /// Boxed for the same reason as `input_color` below — between them they
        /// are what would otherwise make `Ok` dwarf `Failed`
        /// (`clippy::large_enum_variant`); `Box` serializes transparently.
        #[serde(skip_serializing_if = "Option::is_none")]
        effective_area: Option<Box<film_base::EffectiveArea>>,
        /// Resolved input color semantics (transfer + meaning + evidence + ICC
        /// summary) the frame ran on — mirrors the single-frame `Report` field so a
        /// roll frame reports the same input semantics `convert` does. Boxed like
        /// `effective_area` above: these are the two large fields, and unboxed they
        /// make `Ok` dwarf `Failed` (`clippy::large_enum_variant`); `Box`
        /// serializes transparently.
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
        /// Boxed with `new_flow` below: adding that block tipped `Ok` over
        /// `clippy::large_enum_variant`, and this is the largest remaining field.
        #[serde(skip_serializing_if = "Option::is_none")]
        identity: Option<Box<Identity>>,
        /// What a `--new-flow` frame ran — mirrors the single-frame `Report` field.
        /// Boxed like `effective_area` (`clippy::large_enum_variant`).
        #[serde(skip_serializing_if = "Option::is_none")]
        new_flow: Option<Box<NewFlowResult>>,
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

/// Default per-frame output name in the out-dir: `<input-stem>_positive.<ext>`,
/// where the suffix comes from the **frame's own resolved preset** rather than a
/// hardcoded `tiff`.
///
/// The preset is per frame, not per roll, because a manifest's per-frame `params`
/// override may change `output.preset` — that override already warns loudly (it
/// emits a different image class), but the name still has to describe the bytes
/// actually written.
///
/// The spelling comes from [`derived_extension`], **not** from the first entry of
/// [`required_extensions`] — that lists `tif` before `tiff`, so taking its head
/// would have silently renamed every existing roll output from `_positive.tiff` to
/// `_positive.tif`.
fn default_output_name(input: &Path, out_dir: &Path, target: impl Into<OutputTarget>) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".to_string());
    out_dir.join(format!("{stem}_positive.{}", derived_extension(target)))
}

/// The extension nc writes when it supplies the suffix itself — a `roll` frame's
/// derived name, or a `convert` path the user left uncompleted.
///
/// Separate from [`required_extensions`], which says what nc *accepts*: several
/// containers accept two spellings and a supplied one must pick exactly one. The
/// invariant tying them together is that this value is always **a member of** that
/// preset's accepted set, so a supplied suffix can never fail
/// [`resolve_output_path`]; a test asserts it for every preset in
/// `OutputPreset::ALL`, which is what keeps the two from drifting.
///
/// TIFF presets keep `tiff` because that is what roll wrote before containers
/// existed; changing it would rename every output of an unchanged recipe.
fn derived_extension(target: impl Into<OutputTarget>) -> &'static str {
    target.into().container().canonical()
}

/// Resolve a frame's output path: a manifest's explicit path (absolute used
/// verbatim, relative joined onto the out-dir) or the derived
/// `<stem>_positive.<ext>`.
///
/// An explicit manifest path goes through the **same** rule `convert` uses, so it
/// is judged when it states a suffix and *completed* when it does not (an entry
/// naming `chosen` under the default preset writes `chosen.jpg`). A derived name is
/// correct by construction and is not re-checked.
fn resolve_frame_output(
    explicit: Option<&Path>,
    input: &Path,
    out_dir: &Path,
    preset: OutputPreset,
    flow: Flow,
) -> Result<PathBuf> {
    let target = OutputTarget::resolve(flow, preset);
    let path = match explicit {
        Some(o) if o.is_absolute() => o.to_path_buf(),
        Some(o) => out_dir.join(o),
        None => return Ok(default_output_name(input, out_dir, target)),
    };
    resolve_output_path(&path, target, SuffixContext::RollFrame(input))
}

/// Deep-merge `overlay` into `base`: JSON objects merge key-by-key (recursively),
/// any other value replaces. Layers a per-frame partial-recipe override onto the
/// shared recipe's JSON before it is deserialized back to a validated
/// [`ResolvedConfig`] — a partial override keeps the shared values it doesn't
/// mention (a plain `serde` deserialize of the partial would reset them to
/// defaults instead).
///
/// Switching a multi-variant enum via an override is safe, not silent: the merged
/// value must still deserialize as that enum. An **externally tagged** one (e.g.
/// [`FilmBaseSource`]) is a one-key map (`{"region":[…]}`), and flipping it to another
/// variant (`{"explicit":[…]}`) must *replace* the whole map — a key-by-key merge would
/// union the tags into `{"region":…, "explicit":…}`, which no externally-tagged enum can
/// deserialize, turning an override that should apply into a confusing `from_value`
/// rejection. [`is_variant_switch`] catches exactly that signature (both sides
/// single-key objects with *different* keys). No recipe section is internally tagged
/// any more: the `reconstruction` selector went with `simple` and the curve's with
/// `characteristic`, so an overlay's retired `type` merges in as a key and the
/// deserializer strips or refuses it.
///
/// A malformed override is still rejected loudly by the `from_value` in
/// [`resolve_frames`], never applied half-merged.
fn merge_json(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    if is_variant_switch(base, overlay) {
        *base = overlay.clone();
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

/// Load a `--frames` manifest. A read failure or invalid/unknown-key JSON is a
/// usage error (a config mistake), like [`load_recipe_for`].
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
    // **Every preset is roll-capable now.** The blanket convert-only refusal that
    // used to live here was waiting on exactly one missing capability — roll derived
    // `<stem>_positive.tiff` for every frame regardless of container, so a JPEG or
    // AVIF preset would have written a file whose contents its name denied.
    // `default_output_name` now takes the suffix from `derived_extension` — **not**
    // from `required_extensions`, whose head is `tif` and would have renamed every
    // roll output; see that function for why the two are separate — and an explicit
    // manifest path goes through the same `resolve_output_path` rule `convert`
    // uses, so the gap is closed rather than guarded.
    //
    // Do not reintroduce a preset list here. Roll capability was once *derived* from
    // "pins a suffix", and completing the suffix table then refused every preset and
    // broke `hanten roll` outright — the two concepts had merely coincided. They are
    // separate axes, and the correct number of presets on this one is now zero.
    if cfg.input.export_ir.is_some() {
        return Err(NcError::Usage(
            "input.export_ir (--export-ir) is not supported in roll mode: it names a \
             single path that every frame would overwrite; export the IR plane per \
             frame with `hanten convert` instead"
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
/// that touches a roll-fixed choice (`film_base`,
/// `reconstruction.curve.anchor`, `reconstruction.curve.stock`, or `output.preset`) is
/// not rejected — it is applied, with a loud roll-level warning
/// pushed to `roll_warnings` (like the not-frozen warning), so a deliberate
/// per-frame value stays possible while the consistency break is surfaced and
/// `--strict`-promotable.
fn resolve_frames(
    args: &RollArgs,
    shared: &ResolvedConfig,
    shared_recipe: Option<&Recipe>,
    roll_warnings: &mut Vec<String>,
    log: &Log,
) -> Result<Vec<PlannedFrame>> {
    let out_dir = args.out_dir.as_path();
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
            // Under `--new-flow` that is the new chain's document, so an override's
            // sections are that schema's; serialized once, cloned per frame.
            let shared_value = match shared_recipe {
                Some(sr) => serde_json::to_value(sr),
                None => serde_json::to_value(shared),
            }
            .map_err(|e| NcError::Other(format!("serializing shared recipe: {e}")))?;
            for mf in manifest.frames {
                let (cfg, recipe, overrides) = match mf.params {
                    Some(mut ov) => {
                        // A per-frame override carrying a removed legacy key gets
                        // the same pinned migration guidance as the shared recipe,
                        // not an opaque `deny_unknown_fields` serde error — and under
                        // `--new-flow`, the same refusal of the current chain's keys.
                        let context =
                            format!("frame {}: per-frame `params` override", mf.input.display());
                        // Echoed in the report as the user wrote it: a retired key
                        // stripped below still states what the manifest said.
                        let written = ov.clone();
                        match shared_recipe {
                            Some(_) => recipe::check_body(&ov, false, &context)?,
                            None => {
                                strip_retired_keys_at_old_defaults(&mut ov);
                                recipe::check_body_without_flag(&ov, &context)?;
                                reject_legacy_recipe_keys(&ov, &context)?;
                            }
                        }
                        // `calibration.film_base` is a roll calibration: the whole batch
                        // is meant to share one frozen base (Dmin). A per-frame override
                        // *may* still set it (a deliberate per-frame value stays
                        // possible), but doing so gives this frame a different Dmin from
                        // the rest of the roll and breaks color consistency — so warn loudly (roll-level,
                        // `--strict`-promotable) and continue, applying the override,
                        // rather than rejecting.
                        if sets_calibration_film_base(&ov) {
                            let msg = format!(
                                "frame {}: a per-frame `params` override sets \
                                 `calibration.film_base`, overriding the roll-fixed base — \
                                 this frame's Dmin differs from the rest of the roll, \
                                 breaking color consistency. Set the base once in the shared \
                                 --params recipe (and drop the per-frame \
                                 `calibration.film_base`) if you want a frozen, consistent \
                                 roll.",
                                mf.input.display()
                            );
                            log.warn(&msg);
                            roll_warnings.push(msg);
                        }
                        // `reconstruction.curve.anchor` is roll-fixed for the same
                        // reason: the placement decides where mid-grey lands (design-spec
                        // §7.2), so overriding it per frame renders that frame brighter or
                        // darker than the rest of the roll.
                        if sets_curve_anchor(&ov) {
                            let msg = format!(
                                "frame {}: a per-frame `params` override sets \
                                 `reconstruction.curve.anchor`, overriding the roll-fixed \
                                 anchor placement — this frame places mid-grey differently \
                                 from the rest of the roll, breaking tonal consistency. Set \
                                 the placement once in the shared --params recipe (and drop \
                                 the per-frame `reconstruction.curve.anchor`) if you want a \
                                 frozen, consistent roll.",
                                mf.input.display()
                            );
                            log.warn(&msg);
                            roll_warnings.push(msg);
                        }
                        // `output.preset` is the coarsest roll-fixed choice of the set:
                        // it selects which branch out of the ACEScg boundary
                        // runs, so overriding it per frame emits a frame of a different
                        // *image class* — an unclamped linear ACEScg master among
                        // rendered u16 positives, or vice versa. Worse than a Dmin/Dmax
                        // break, and the last of these key probes to gain a warning. Same
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
                        let invalid = |e: serde_json::Error| {
                            NcError::Usage(format!(
                                "frame {}: invalid params override: {e}",
                                mf.input.display()
                            ))
                        };
                        let mut v = shared_value.clone();
                        merge_json(&mut v, &ov);
                        // Under `--new-flow` the frame keeps its own recipe as well as the
                        // projection: the decode and the chain read the recipe, so an
                        // override of `reconstruction.*` would otherwise be validated and
                        // then rendered with the shared value.
                        let (cfg, frame_recipe): (ResolvedConfig, Option<Recipe>) =
                            match shared_recipe {
                                Some(_) => {
                                    let r: Recipe = serde_json::from_value(v).map_err(invalid)?;
                                    (r.to_config(), Some(r))
                                }
                                None => (serde_json::from_value(v).map_err(invalid)?, None),
                            };
                        // Same ordering as the shared gate above: roll-specific
                        // rejections first, then the new chain's recipe gate, the
                        // least-specific missing-base last.
                        reject_roll_unsupported(&cfg)?;
                        reject_roll_unsupported_input(&cfg)?;
                        if let Some(r) = &frame_recipe {
                            recipe::validate(r, KnobNames::KeyOnly).map_err(|e| {
                                NcError::Usage(format!("{context}: {}", e.message()))
                            })?;
                        }
                        validate_with_remedy(&cfg, FilmBaseRemedy::SharedRecipe)?;
                        (cfg, frame_recipe, Some(written))
                    }
                    None => (shared.clone(), shared_recipe.cloned(), None),
                };
                let output = resolve_frame_output(
                    mf.output.as_deref(),
                    &mf.input,
                    out_dir,
                    cfg.output.preset,
                    Flow::from_flag(args.new_flow),
                )?;
                planned.push(PlannedFrame {
                    input: mf.input,
                    output,
                    cfg,
                    recipe,
                    overrides,
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
                let output = default_output_name(
                    &input,
                    out_dir,
                    OutputTarget::resolve(Flow::from_flag(args.new_flow), shared.output.preset),
                );
                planned.push(PlannedFrame {
                    input,
                    output,
                    cfg: shared.clone(),
                    recipe: shared_recipe.cloned(),
                    overrides: None,
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
            white_balance: report.white_balance,
            effective_area: report.effective_area.map(Box::new),
            input_color: report.input_color.map(Box::new),
            loss: report.loss,
            output_stats: report.output_stats,
            identity: report.identity.map(Box::new),
            new_flow: report.new_flow.map(Box::new),
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

/// `hanten roll` — convert a batch of frames from one shared, frozen recipe (the
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
        doc,
        meta_pipeline_version,
        unpinned_curve: shared_unpinned_curve,
        // Roll's suffix diagnosis never takes the convert arms: a manifest path is
        // blamed as `SuffixContext::RollFrame` and a derived name matches by
        // construction, so the shared recipe's provenance has nothing to vary.
        // (Roll's *per-frame* `output.preset` witness is probed separately, at the
        // override, for the roll-consistency warning.)
        output_preset_present: _,
    } = load_recipe_for(args.recipe_in.as_deref(), Flow::from_flag(args.new_flow))?;
    // `roll` merges no flags, so the new chain's projection can be taken once here.
    let (shared, shared_recipe) = match doc {
        RecipeDoc::Current(cfg) => (cfg, None),
        RecipeDoc::New(r) => (r.to_config(), Some(r)),
    };
    // Roll-specific rejections run **before** the shared `validate`, and the order
    // is the same least-specific-diagnosis-last policy `validate` itself now
    // follows: "this setting cannot work in roll mode" names the offending key,
    // while "no film base selected" is the least specific diagnosis available. A
    // recipe that is both baseless and roll-invalid should surface the roll problem
    // first, or the user adds a base only to meet a second error.
    reject_roll_unsupported(&shared)?;
    reject_roll_unsupported_input(&shared)?;
    // Under `--new-flow` the shared recipe is the new chain's document: its decode
    // values are checked here, ahead of the missing-base rule below for the same
    // reason as on `convert`, and its shared sections by `validate_with_remedy` on the
    // projection. Each per-frame overlay gets both in `resolve_frames`.
    if let Some(r) = &shared_recipe {
        recipe::validate(r, KnobNames::KeyOnly)
            .map_err(|e| NcError::Usage(format!("shared recipe: {}", e.message())))?;
    }
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
    if let Some(msg) = curve_default_warning(shared_unpinned_curve, meta_pipeline_version) {
        log.warn(&msg);
        roll_warnings.push(msg);
    }
    if !matches!(
        shared.calibration.film_base,
        Some(FilmBaseSource::Explicit(_))
    ) {
        // `validate` above already rejected `None`, so only the two estimating
        // sources reach here.
        let kind = match shared.calibration.film_base {
            Some(FilmBaseSource::Auto) => "auto",
            Some(FilmBaseSource::Region(_)) => "region",
            Some(FilmBaseSource::Explicit(_)) | None => unreachable!("validate rejects both"),
        };
        let msg = format!(
            "roll film base is NOT frozen: calibration.film_base is `{kind}`, so every frame \
             estimates its own Dmin — the roll is not color-consistent and the shared \
             recipe is not truly shared. Calibrate the base once (e.g. `hanten estimate \
             --base-region X,Y,W,H <reference-scan>`), then pass the reported explicit \
             base via `--film-base R,G,B` or a recipe with `calibration.film_base.explicit`."
        );
        log.warn(&msg);
        roll_warnings.push(msg);
    }

    // Resolve the plan. A per-frame override that touches a roll-fixed calibration
    // (`film_base`) appends its own roll-level
    // warning here (warn-and-continue, like the not-frozen warnings above), so
    // `roll_warnings` is passed in to collect it.
    let planned = resolve_frames(
        &args,
        &shared,
        shared_recipe.as_ref(),
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
        // No sidecar is written under `--new-flow` (see `render_new_flow_frame`).
        if Flow::from_flag(args.new_flow) == Flow::Legacy {
            targets.push((
                format!("sidecar for {}", pf.input.display()),
                encode::sidecar_path(&pf.output),
            ));
        }
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
            FrameChain::of(pf.recipe.as_ref()),
            InputFromCli::none(),
            &args
                .recipe_in
                .iter()
                .chain(args.frames.iter())
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
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
    let (identity, recipe) = match Flow::from_flag(args.new_flow) {
        Flow::Legacy => (
            Identity::with_params_hash(version::stable_hash(&canonical_params_json(&shared)?)),
            Some(shared),
        ),
        Flow::New => (Identity::new(), None),
    };
    let roll = RollReport {
        command: "roll",
        identity,
        recipe,
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

/// `hanten inspect` — decode a scan and report what was found (format, dimensions,
/// channels, bit depth, IR presence, scanner metadata) plus a best-effort
/// suggested `Dmin`. No output image is written.
fn run_inspect(args: IoArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    // A bad `--measure-inset` is a *usage* error (exit 2), not a diagnostic that
    // degrades to a warning: these commands resolve no recipe, so `validate` never
    // sees the flag and the best-effort `effective_area` call below would swallow
    // it at exit 0. Checked before the decode, so a 160 MB read is not wasted on a
    // typo.
    if let Some(f) = args.measure.measure_inset {
        check_measure_inset(f)?;
    }

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

    // IR film-holder mask (on a scan carrying a *marker-verified* IR plane that
    // measures usable). Diagnostic on `inspect`: it shows which along-edge segments
    // the opaque holder occludes, and drives the film-segment restriction the
    // candidate search below uses. The verdict is measured
    // (`ir-usability-detection`) — `--film-type` takes no part in it.
    let ir_separability = film_base::ir_separability(&image);
    let ir_usable = ir_separability.is_some_and(|s| s.usable);
    report.ir_separability = ir_separability;
    report.film_type = args.film_type;
    // Build the mask *before* the notes, so each one describes what actually
    // happened rather than predicting it: a usable plane can still produce no mask
    // (a too-small image errors on `scan_depth`; an all-holder mask falls back).
    // Best-effort, like the candidate search below — `inspect` is informational and
    // must not abort over a diagnostic.
    let mut mask_error = false;
    match film_base::ir_holder_mask(&image) {
        Ok(mask) => report.holder_mask = mask,
        Err(e) => {
            mask_error = true;
            push_warning(
                &mut report,
                &log,
                format!("holder-mask detection skipped — {e}"),
            );
        }
    }
    // The effective measurement area: the holder depth march plus the static
    // inset. Independent of the along-edge mask above — it deliberately does not
    // inherit `ir_holder_mask`'s all-holder decline, so a frame whose holder wraps
    // the whole border is measured here even when the mask above is `None`.
    // Best-effort like every other `inspect` diagnostic.
    match film_base::effective_area(
        &image,
        args.measure.measure_inset.unwrap_or(DEFAULT_MEASURE_INSET),
    ) {
        Ok(area) => {
            report.effective_area = Some(area);
            for w in film_base::effective_area_warnings(&area) {
                push_warning(&mut report, &log, w);
            }
        }
        Err(e) => push_warning(
            &mut report,
            &log,
            // `message()`, not `{e}`: `Display` prefixes the kind, so this
            // rendered as "skipped — usage: …" — a warning announcing an error
            // inside itself.
            format!("effective-area resolution skipped — {}", e.message()),
        ),
    }
    // Both IR consumers, each reporting what it did rather than what the inputs
    // predict. The holder march is the second one: on 22 of 25 real chromogenic
    // frames the mask above declines while the march measures a holder and moves
    // the reported rectangle, and keying the note on the mask alone made one report
    // carry both a measured `effective_area.holder` and "preserved but not used".
    let ir_consumed = report.holder_mask.is_some()
        || report
            .effective_area
            .is_some_and(|a: film_base::EffectiveArea| a.holder_applied);

    if info.ir_present && !image.ir_verified {
        // Shape-only IR plane: carried/exportable but not trusted for detection.
        push_warning(
            &mut report,
            &log,
            "an IR plane is present, but it is identified by shape alone (no \
             NewSubfileType=4 marker) and not trusted for holder detection; \
             film-holder detection is RGB-only"
                .into(),
        );
    } else if info.ir_present && !ir_usable {
        // Trusted, but this frame's own film is too opaque to tell from the holder.
        push_warning(
            &mut report,
            &log,
            format!(
                "the IR plane cannot separate the film holder on this frame (interior \
                 IR transmission {:.4}); film-holder detection is RGB-only",
                ir_separability.map_or(0.0, |s| s.interior_median)
            ),
        );
    } else if info.ir_present && !ir_consumed && !mask_error {
        // Marker-verified and measured usable, yet **neither** reader consumed it:
        // "both declined for a reason neither note above named". This is no longer
        // the all-holder fallback — that frame marches to a ring, so
        // `holder_applied` is true and `ir_consumed` short-circuits here. With a
        // usable plane the only combination left is an errored `effective_area`
        // beside an all-holder mask, and the "effective-area resolution skipped"
        // warning above already describes that event.
        //
        // Kept as a safety net all the same, and deliberately *not* gated on
        // `report.effective_area.is_some()`: if a third IR reader ever lands, this
        // is the branch that keeps saying so rather than going quiet.
        // (`mask_error` already said its piece.)
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
    match film_base::rebate_candidates(&image, report.holder_mask.as_deref()) {
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
/// flag string and the matching `calibration.film_base` value — or `None` when
/// the measurement fails the explicit-base validation `convert` applies (each
/// channel in `(0, 1]`), so a degenerate base is never advertised as reusable.
/// `f32`'s `Display` prints the shortest round-tripping decimal, so both forms
/// reproduce the exact measured value when fed back to `convert`.
fn reuse_ready(rgb: [f32; 3]) -> Option<(String, FilmBaseSource)> {
    validate_explicit_film_base(&rgb).ok()?;
    Some((
        format!("--film-base {},{},{}", rgb[0], rgb[1], rgb[2]),
        FilmBaseSource::Explicit(rgb),
    ))
}

/// `hanten estimate` — run only film-base / `Dmin` estimation from the selected
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
/// `calibration.film_base` lost its default, `version::PIPELINE_FINGERPRINTS` no
/// longer covers this decision from either side: `base` pins the detector by
/// naming `Auto` explicitly, and `recipe` sees the resolved config's `null`.
/// Changing what an unstated `estimate` resolves to would therefore move every
/// `hanten estimate` result with the whole drift gate green — verify such a change by
/// hand, and do not assume the gate is watching.
fn run_estimate(args: EstimateArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    if args.d_max_region.is_some() {
        return Err(NcError::Usage(removed_dmax_message(
            "--d-max-region",
            "measured the roll reference density off a light-struck leader",
        )));
    }

    // A bad `--measure-inset` is a *usage* error (exit 2), not a diagnostic that
    // degrades to a warning: these commands resolve no recipe, so `validate` never
    // sees the flag and the best-effort `effective_area` call below would swallow
    // it at exit 0. Checked before the decode, so a 160 MB read is not wasted on a
    // typo.
    if let Some(f) = args.measure.measure_inset {
        check_measure_inset(f)?;
    }

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
    // conservatively as the whole rectangle).
    let budget = args.memory.budget();
    let sampling = if args.grid {
        match args.film_base.base_region {
            // `--grid` samples five cells of the rectangle, one at a time, so the
            // phase peaks at one cell — not at the whole rectangle.
            Some([_, _, w, h]) => SamplePlan::rect(film_base::grid_cell_pixels(w, h)),
            None => SamplePlan::none().with_whole_frame_grid(),
        }
    } else {
        sample_plan(&source)
    };
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
    // IR holder mask, so it degrades to RGB-only when the IR plane is shape-only
    // (unverified provenance) or measures unable to separate holder from film. The
    // `--grid` and explicit/region paths never touch IR, so they need no note.
    report.film_type = args.film_type;
    // The calibration command reports the measurement itself, not just a warning
    // about it: `estimate` is where a user decides how to acquire a base, so the
    // number that drove the decision belongs in its artifact too.
    report.ir_separability = film_base::ir_separability(&image);
    // Same rationale for the effective area: `estimate` is where a user decides how
    // to acquire a base, so the region a measurement would be read over belongs in
    // its artifact. It does not (yet) drive this command's estimate — that is
    // `film-base/holder-masked-measurement`'s change.
    match film_base::effective_area(
        &image,
        args.measure.measure_inset.unwrap_or(DEFAULT_MEASURE_INSET),
    ) {
        Ok(area) => {
            report.effective_area = Some(area);
            for w in film_base::effective_area_warnings(&area) {
                push_warning(&mut report, &log, w);
            }
        }
        Err(e) => push_warning(
            &mut report,
            &log,
            // `message()`, not `{e}`: `Display` prefixes the kind, so this
            // rendered as "skipped — usage: …" — a warning announcing an error
            // inside itself.
            format!("effective-area resolution skipped — {}", e.message()),
        ),
    }
    let mut ir_note_pending = !args.grid && matches!(source, FilmBaseSource::Auto);
    if ir_note_pending && info.ir_present {
        let sep = report.ir_separability;
        if !image.ir_verified {
            ir_note_pending = false;
            push_warning(
                &mut report,
                &log,
                "an IR plane is present, but it is identified by shape alone (no \
                 NewSubfileType=4 marker) and not trusted for holder detection; \
                 using RGB-only film-holder detection for the film base"
                    .into(),
            );
        } else if !sep.is_some_and(|s| s.usable) {
            ir_note_pending = false;
            push_warning(
                &mut report,
                &log,
                format!(
                    "the IR plane cannot separate the film holder on this frame \
                     (interior IR transmission {:.4}); using RGB-only film-holder \
                     detection for the film base",
                    sep.map_or(0.0, |s| s.interior_median)
                ),
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
        // `auto` source uses the IR holder mask when the scan carries an IR plane
        // that measures able to separate holder from film.
        let est = film_base::estimate(&image, &source)?;
        report.film_base_source = Some(source);
        for w in est.warnings {
            push_warning(&mut report, &log, w);
        }
        // A plane that survived both notes above and still went unused **for the
        // film base**. Read off what stage 2 did, never predicted.
        //
        // The all-holder fallback is no longer the only route here: since
        // `holder-depth-mask`, `estimate` also resolves the effective area, whose
        // march can measure a holder on the same frame. This note deliberately does
        // **not** gain that disjunct — the scoping to "for the film base" is what
        // makes it honest, and it is load-bearing rather than incidental. The film
        // base really did not use the plane, which is the verdict a calibration
        // command owes; `inspect` (which reports on the whole Step-1 read) counts
        // both readers, and `convert` counts the region only when it reaches a
        // pixel. Three questions, three rules, each individually honest — adding the
        // march here would make *this* message wrong.
        if info.ir_present && image.ir_verified && !est.ir_mask_applied && ir_note_pending {
            push_warning(
                &mut report,
                &log,
                "input carries an IR plane; preserved but not used for the film base".into(),
            );
        }
        est.base
    };
    report.film_base = Some(base);

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
        Some((flag, source)) => {
            report.reuse = Some(ReuseReady { flag, source });
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

    // The recipe-shaped handoff, assembled from the reuse-ready pairs rather than
    // from the measurements directly: that is what keeps each key present exactly
    // when its flag is, and withholds a value the reuse gates declined to advertise.
    // Built here, after every gate above has decided.
    report.calibration = calibration_fragment(&report);

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

/// The report's `calibration` object, derived from the reuse-ready pairs.
///
/// **Derived, never assembled separately.** Each key is present exactly when its
/// own reuse-ready pair is, so the pairing invariant is enforced by construction
/// rather than by two call sites agreeing — and a measurement the reuse gates
/// declined to advertise (a base outside `(0, 1]`)
/// cannot leak into a fragment a user would pipe straight into `--params`.
///
/// `None` when nothing was measured: an empty `{}` would pipe into `--params` as a
/// no-op the user could mistake for a calibration.
fn calibration_fragment(report: &Report) -> Option<CalibrationFragment> {
    let fragment = CalibrationFragment {
        film_base: report.reuse.as_ref().map(|r| r.source.clone()),
    };
    (!fragment.is_empty()).then_some(fragment)
}

// ---------------------------------------------------------------------------
// measure-roll — the roll white balance (`nf-scene-correction/roll-white-balance`)
// ---------------------------------------------------------------------------

/// The `measure-roll` JSON report.
#[derive(Debug, Serialize)]
struct MeasureRollReport {
    command: &'static str,
    /// Which build measured the gains — they are frozen into a recipe and outlive it.
    identity: Identity,
    /// The film base every input was decoded with.
    film_base: FilmBase,
    /// The decode the gains belong to: they are measured at its output.
    decode: fixed::DecodeReport,
    /// The leader guard, when `--leader` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    leader: Option<MeasuredLeader>,
    frames: Vec<MeasuredFrame>,
    white_balance: RollWhiteBalance,
    /// The gains in the two forms a user freezes them in.
    reuse: WhiteBalanceReuse,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
struct MeasuredLeader {
    input: PathBuf,
    #[serde(flatten)]
    guard: roll_white::LeaderGuard,
    memory: MemoryReport,
}

#[derive(Debug, Serialize)]
struct MeasuredFrame {
    input: PathBuf,
    /// The effective area the frame was sampled over.
    region: [u32; 4],
    #[serde(flatten)]
    counts: roll_white::FrameCounts,
    memory: MemoryReport,
}

#[derive(Debug, Serialize)]
struct RollWhiteBalance {
    /// Green-anchored gains for `scene_correction.white_balance`.
    gains: [f32; 3],
    /// The per-channel percentile of the pooled pixels they equalize.
    percentile: f32,
    /// Pixels pooled over the whole roll.
    pooled: usize,
}

#[derive(Debug, Serialize)]
struct WhiteBalanceReuse {
    /// For `convert --new-flow`.
    flag: String,
    /// A partial recipe, to merge into the roll's.
    recipe: WhiteBalanceFragment,
}

/// `{"scene_correction": {"white_balance": {"explicit": [r, g, b]}}}`, typed rather than
/// built as a `serde_json::Value` so the gains print in their `f32` form — a `Value`
/// widens them to `f64` digits the flag form does not show.
#[derive(Debug, Serialize)]
struct WhiteBalanceFragment {
    scene_correction: WhiteBalanceSection,
}

#[derive(Debug, Serialize)]
struct WhiteBalanceSection {
    white_balance: scene_correction::WhiteBalance,
}

/// One input decoded into linear ACEScg at the new chain's decode, plus its effective
/// area: the front half of `convert_frame`'s new-flow path, gated the same way
/// (memory preflight, input semantics, positive-mode refusal) and stopping before
/// scene correction — the point the roll's gains will be applied at.
///
/// **A copy, and it must stay in step.** `convert_frame`'s front half is tangled with
/// its report and IR notes, so this repeats its gates rather than sharing them: a
/// refusal or measurement-region warning added there belongs here too, or gains get
/// frozen from frames `convert` would refuse.
fn decode_for_roll_white(
    input: &Path,
    recipe: &Recipe,
    base: &FilmBase,
    budget: memory::Budget,
    log: &Log,
    warnings: &mut Vec<String>,
) -> Result<(
    AcesCgImage,
    Result<film_base::EffectiveArea>,
    fixed::DecodeReport,
    MemoryReport,
)> {
    let memory = preflight_memory(
        input,
        RunProfile::MeasureRoll,
        SamplePlan::none(),
        budget,
        memory::detect_total_ram(),
        log,
        warnings,
    )?;
    let (image, info) = decode_within(input, budget.bytes())?;
    log.info(format_args!(
        "decoded {} {}x{}",
        input.display(),
        info.width,
        info.height
    ));
    for w in &info.warnings {
        push_warning_buf(warnings, log, format!("{}: {w}", input.display()));
    }
    let input_meta = input_semantics::resolve(
        &container_color_facts(&info),
        &input_assertions(&recipe.to_config(), InputFromCli::none()),
    )?;
    if InputColorReport::from_metadata(&input_meta).icc_unparsable() {
        push_warning_buf(
            warnings,
            log,
            format!(
                "{}: embedded ICC profile present but could not be parsed for a summary",
                input.display()
            ),
        );
    }
    input_semantics::require_convertible(&input_meta)?;
    reject_positive_mode(&info)?;
    let area = film_base::effective_area(&image, recipe.measure.inset);
    let (film, decoded) = fixed::decode(&image, base, &recipe.reconstruction)?;
    drop(image);
    Ok((
        working_space::map_nc_film_rgb_v1(film),
        area,
        decoded,
        memory,
    ))
}

/// `hanten measure-roll` — measure a roll's white balance once, over its picture
/// frames, and report the gains to freeze into its recipe.
fn run_measure_roll(args: MeasureRollArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    let mut recipe = match load_recipe_for(args.recipe_in.as_deref(), Flow::New)?.doc {
        RecipeDoc::New(r) => r,
        RecipeDoc::Current(_) => unreachable!("loaded for the new flow"),
    };
    if let Some(b) = args.film_base {
        recipe.calibration.film_base = Some(FilmBaseSource::Explicit(b));
    }
    if let Some(f) = args.measure.measure_inset {
        recipe.measure.inset = f;
    }
    // Before any decode: a bad inset would otherwise surface from the first frame's
    // effective area, blamed on that file.
    check_measure_inset(recipe.measure.inset)?;
    // What this command measures, and the look it never applies, so neither is read —
    // and neither may refuse the run. Keys only: this command takes none of the
    // conversion flags.
    recipe.scene_correction = scene_correction::SceneCorrectionParams::default();
    recipe.look = look::LookSection::default();
    recipe::validate(&recipe, KnobNames::KeyOnly)?;
    if args.strict && args.leader.is_none() {
        return Err(NcError::Usage(
            "--strict refuses an unguarded measurement: without --leader a fully exposed \
             frame among the inputs would become the roll's white. Pass the roll's leader \
             scan"
                .into(),
        ));
    }
    // A frame named twice would weigh twice in the pool — silently, since every
    // frame contributes the same sample count. The leader named as a frame too (the
    // natural glob when it sits beside them) would pool its unguarded edges.
    let mut seen: Vec<(PathBuf, &Path)> = Vec::new();
    if let Some(leader) = &args.leader {
        let key = collision_key(leader);
        if let Some(input) = args
            .inputs
            .iter()
            .find(|i| keys_collide(&collision_key(i), &key))
        {
            return Err(NcError::Usage(format!(
                "{} is both the --leader and an input frame; every input is pooled as \
                 picture, so leave the leader out of the frames",
                input.display()
            )));
        }
    }
    for input in &args.inputs {
        let key = collision_key(input);
        if let Some((_, first)) = seen.iter().find(|(k, _)| keys_collide(k, &key)) {
            let spelled = if *first == input.as_path() {
                String::new()
            } else {
                format!(" (also as {})", first.display())
            };
            return Err(NcError::Usage(format!(
                "{} is named twice{spelled}; each frame is pooled once, so a repeat would \
                 double its weight in the roll's white",
                input.display()
            )));
        }
        seen.push((key, input));
    }
    let base = match recipe.calibration.film_base {
        Some(FilmBaseSource::Explicit(b)) => {
            validate_explicit_film_base(&b)?;
            FilmBase::from(b)
        }
        _ => {
            return Err(NcError::Usage(
                "measure-roll needs the roll's film base stated explicitly — `--film-base \
                 R,G,B` or `calibration.film_base` as `{\"explicit\": [r, g, b]}` in the \
                 recipe: a base estimated per frame would measure each frame under a \
                 different decode. Measure it once with `hanten estimate --grid \
                 <base.tif>`"
                    .into(),
            ));
        }
    };
    if let Some(rf) = args.report.report_file.as_deref() {
        let inputs: Vec<&Path> = args
            .inputs
            .iter()
            .map(PathBuf::as_path)
            .chain(args.leader.as_deref())
            .collect();
        for input in inputs {
            ensure_write_targets_distinct(input, &[("--report-file", rf)])?;
        }
    }

    let budget = args.memory.budget();
    let mut warnings = Vec::new();
    let mut decode = None;

    let leader = match &args.leader {
        Some(path) => {
            let (aces, _, _, memory) =
                decode_for_roll_white(path, &recipe, &base, budget, &log, &mut warnings)?;
            let guard = roll_white::leader_guard(
                aces.rgb(),
                aces.width(),
                aces.height(),
                recipe.reconstruction.linearization,
            )
            .map_err(|e| NcError::Other(format!("{}: {}", path.display(), e.message())))?;
            Some(MeasuredLeader {
                input: path.clone(),
                guard,
                memory,
            })
        }
        None => {
            push_warning_buf(
                &mut warnings,
                &log,
                "no --leader: the measurement is unguarded, so a fully exposed frame among \
                 the inputs would become the roll's white (measured: it moves the gains \
                 0.4–1.3 stops). Pass the roll's leader scan"
                    .into(),
            );
            None
        }
    };

    let mut pool = Vec::new();
    let mut frames = Vec::with_capacity(args.inputs.len());
    for input in &args.inputs {
        let (aces, area, decoded, memory) =
            decode_for_roll_white(input, &recipe, &base, budget, &log, &mut warnings)?;
        let area = area.map_err(|e| {
            NcError::Usage(format!(
                "{}: {} (every frame is measured over its effective area)",
                input.display(),
                e.message()
            ))
        })?;
        // A capped or unsettled holder march leaves holder in the region, and the pool
        // would take it as picture — the same warning `convert` gives, so `--strict`
        // sees it.
        for w in film_base::effective_area_warnings(&area) {
            push_warning_buf(&mut warnings, &log, format!("{}: {w}", input.display()));
        }
        let counts = roll_white::pool_frame(
            aces.rgb(),
            aces.width(),
            area.region,
            leader.as_ref().map(|l| &l.guard),
            &mut pool,
        )?;
        if counts.kept == 0 {
            push_warning_buf(
                &mut warnings,
                &log,
                format!(
                    "{}: contributed no pixel ({} guarded, {} unusable) — is it a picture \
                     frame of this roll?",
                    input.display(),
                    counts.guarded,
                    counts.unusable
                ),
            );
        }
        frames.push(MeasuredFrame {
            input: input.clone(),
            region: area.region,
            counts,
            memory,
        });
        decode.get_or_insert(decoded);
    }
    let gains = roll_white::roll_gains(&pool)?;
    log.info(format_args!("roll white balance {gains:?}"));

    let report = MeasureRollReport {
        command: "measure-roll",
        identity: Identity::new(),
        film_base: base,
        decode: decode.expect("clap requires at least one input"),
        leader,
        frames,
        white_balance: RollWhiteBalance {
            gains,
            percentile: roll_white::PERCENTILE,
            pooled: pool.len() / 3,
        },
        reuse: WhiteBalanceReuse {
            flag: format!("--white-balance {},{},{}", gains[0], gains[1], gains[2]),
            recipe: WhiteBalanceFragment {
                scene_correction: WhiteBalanceSection {
                    white_balance: scene_correction::WhiteBalance::Explicit(gains),
                },
            },
        },
        warnings,
        elapsed_ms: elapsed_ms(started),
    };
    emit_json(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
        &log,
    )?;
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
    // The **resolved** output path, not `args.output`: `output_bytes` must stat the
    // file that was written, which a completed suffix makes a different path.
    output: &Path,
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
        output_bytes: file_len(output),
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
        preset: cfg.output.preset,
        // Via `depth()` — the single place a recipe value becomes a depth — so the
        // record cannot disagree with what `io::encode` actually wrote. Reading
        // `cfg.output.hdr` here would report `false` for a `film-master` run, whose
        // switch is pinned at its default while the branch resolves f32.
        output_depth: cfg.output.primary_depth_label(),
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
    /// `calibration.film_base` has no default (see `types::CalibrationParams::film_base`),
    /// so `validate` rejects an unstated one. Tests that are not *about* the film
    /// base use this, otherwise every unrelated assertion would trip that rule
    /// instead of the one under test. `Auto` is the value this used to default to,
    /// so these tests exercise exactly what they did before.
    fn base_cfg() -> ResolvedConfig {
        ResolvedConfig {
            calibration: CalibrationParams {
                film_base: Some(FilmBaseSource::Auto),
            },
            ..ResolvedConfig::default()
        }
    }

    use crate::types::{DensityParams, ExponentialParams};

    /// Parse a `convert` invocation (with the required input/output already set)
    /// and return its args, so merge can be tested against the real parser.
    fn parse_convert(extra: &[&str]) -> ConvertArgs {
        let mut argv = vec!["hanten", "convert", "in.tiff", "-o", "out.tiff"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Convert(a) => a,
            _ => unreachable!("expected convert"),
        }
    }

    /// A density-reconstruction config from its two blocks.
    fn density_cfg(density: DensityParams, curve: ExponentialParams) -> ResolvedConfig {
        ResolvedConfig {
            reconstruction: Reconstruction { density, curve },
            ..base_cfg()
        }
    }

    fn exponential_cfg(e: ExponentialParams) -> ResolvedConfig {
        density_cfg(DensityParams::default(), e)
    }

    /// The resolved curve of a config.
    fn curve_of(cfg: &ResolvedConfig) -> &ExponentialParams {
        &cfg.reconstruction.curve
    }

    /// The curve's gamma.
    fn gamma_of(cfg: &ResolvedConfig) -> f32 {
        curve_of(cfg).gamma
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

        // unspecified everywhere → the curve's default.
        let cfg = merge(base_cfg(), &parse_convert(&[])).unwrap();
        assert_eq!(gamma_of(&cfg), 2.0);
    }

    #[test]
    fn merge_measure_inset_flag_overrides_recipe_else_keeps_recipe_else_default() {
        // The forgotten-merge-arm regression: without the arm, `--measure-inset`
        // parses and is silently dropped.
        let recipe: ResolvedConfig = serde_json::from_str(r#"{"measure":{"inset":0.2}}"#).unwrap();
        assert_eq!(recipe.measure.inset, 0.2, "the recipe key deserializes");

        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(cfg.measure.inset, 0.2, "recipe kept with no flag");

        let cfg = merge(recipe, &parse_convert(&["--measure-inset", "0.12"])).unwrap();
        assert_eq!(cfg.measure.inset, 0.12, "flag wins over the recipe");

        let cfg = merge(base_cfg(), &parse_convert(&[])).unwrap();
        assert_eq!(
            cfg.measure.inset, DEFAULT_MEASURE_INSET,
            "unstated everywhere → the shared default"
        );
    }

    #[test]
    fn validate_bounds_the_measure_inset_from_either_provenance() {
        // A *value* rule, so it is in `validate` rather than `validate_convert` —
        // `roll` and every per-frame override reach only the former. Both
        // provenances hit the same check, which is why the bound has one home.
        for bad in [-0.1, 0.5, 1.0, f32::NAN, f32::INFINITY] {
            let mut cfg = base_cfg();
            cfg.measure.inset = bad;
            let err = validate(&cfg).unwrap_err();
            assert!(
                format!("{err}").contains("measure-inset"),
                "inset {bad} must be refused by name, got: {err}"
            );
            // Every remedy it names must be one that exists. `effective_area` never
            // sees a user-stated region — `--base-region` sets the film-base
            // source — so "state a region explicitly" was advice nothing could
            // follow. Asserting its *absence*, because both wordings name the knob.
            let err = format!("{err}");
            assert!(
                !err.contains("state a region"),
                "inset {bad}: there is no explicit measurement region to state: {err}"
            );
        }
        // Over the maximum, the remedies are lowering the fraction or not measuring
        // over the area at all.
        let mut over = base_cfg();
        over.measure.inset = 0.5;
        let err = format!("{}", validate(&over).unwrap_err());
        assert!(
            err.contains("lower the fraction") && !err.contains("--auto-d-max"),
            "{err}"
        );
        let mut ok = base_cfg();
        ok.measure.inset = 0.0;
        assert!(
            validate(&ok).is_ok(),
            "zero is legal — the stage floors it at one probe step on a measured \
             frame, which is not a usage question"
        );
        ok.measure.inset = crate::types::MAX_MEASURE_INSET;
        assert!(validate(&ok).is_ok(), "the bound itself is inclusive");
    }

    #[test]
    fn the_measure_inset_bound_has_exactly_one_definition() {
        // Two gates gave one knob different bounds once (the `headroom_stops`
        // lesson). `film_base::effective_area` and `cli::validate` must agree by
        // construction, so pin that they both refuse the same value.
        let img = crate::types::LinearImage::new(64, 64, vec![0.5; 64 * 64 * 3], None).unwrap();
        let over = crate::types::MAX_MEASURE_INSET + 0.01;
        assert!(film_base::effective_area(&img, over).is_err());
        let mut cfg = base_cfg();
        cfg.measure.inset = over;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn merge_handles_array_flags() {
        let cfg = merge(
            base_cfg(),
            &parse_convert(&["--white-balance", "1.1,1.0,0.9"]),
        )
        .unwrap();
        assert_eq!(cfg.print.white_balance, WbSource::Explicit([1.1, 1.0, 0.9]));
    }

    #[test]
    fn removed_algorithm_and_simple_flags_are_migration_errors() {
        // `--algorithm` is rejected with guidance naming the replacement.
        let err = reject_removed_flags(&parse_convert(&["--algorithm", "sigmoid"])).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("reconstruction.curve"), "{err}");
        // The remedy is to drop the flag, not to reach for the removed curve selector.
        assert!(err.to_string().contains("Drop the flag"), "{err}");
        assert!(!err.to_string().contains("--density-curve"), "{err}");

        // `--reconstruction`, whatever its value — density is the only one left.
        for value in ["simple", "density"] {
            let err =
                reject_removed_flags(&parse_convert(&["--reconstruction", value])).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{value}");
            assert!(
                err.to_string().contains(REMOVED_SIMPLE_RECONSTRUCTION),
                "{value}: {err}"
            );
        }

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

    /// Every flag the sigmoid owned is a migration error naming a remedy that exists.
    /// The refusal fires on both chains, so each message also says what works under
    /// `--new-flow`, which refuses the display tone.
    #[test]
    fn the_sigmoid_flags_are_migration_errors() {
        use clap::CommandFactory;
        for (flags, remedy, new_flow) in [
            (
                ["--sigmoid-contrast", "2"].as_slice(),
                "--density-gamma",
                "--contrast",
            ),
            (
                ["--sigmoid-toe", "0.2"].as_slice(),
                "--display-tone",
                "--display-tone-headroom",
            ),
            (
                ["--sigmoid-shoulder", "0"].as_slice(),
                "--display-tone",
                "--display-tone-headroom",
            ),
            (
                ["--sigmoid-mid-fraction", "0.5"].as_slice(),
                "--anchor-mid-offset",
                "--anchor-mid-offset",
            ),
            (
                ["--sigmoid-white-at-d-max"].as_slice(),
                "--anchor-mid-offset",
                "--anchor-mid-offset",
            ),
        ] {
            let err = reject_removed_flags(&parse_convert(flags)).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{flags:?}");
            let msg = err.to_string();
            assert!(msg.contains("removed with the sigmoid"), "{flags:?}: {msg}");
            assert!(msg.contains(remedy), "{flags:?}: {msg}");
            assert!(msg.contains(new_flow), "{flags:?}: {msg}");
            // What the message names for `--new-flow` is a flag that chain keeps.
            if new_flow.starts_with("--") {
                let mut argv = vec!["--new-flow"];
                argv.push(new_flow);
                argv.push("0.6");
                let args = parse_convert(&argv);
                assert!(
                    flow::reject_unavailable_flags(Flow::New, &args).is_ok(),
                    "{new_flow} is refused under --new-flow"
                );
            }
            // The remedy must itself parse.
            assert!(
                Cli::command()
                    .get_subcommands()
                    .find(|c| c.get_name() == "convert")
                    .unwrap()
                    .get_arguments()
                    .any(|a| a.get_long() == Some(remedy.trim_start_matches("--"))),
                "{remedy} is not a flag"
            );
        }
    }

    /// Every flag retired with the `characteristic` curve is a migration error at every
    /// value, on both chains, before anything coarser can refuse it — and no remedy names
    /// a flag that no longer exists.
    #[test]
    fn the_characteristic_flags_are_migration_errors() {
        let cases: &[(&[&str], &str)] = &[
            (
                &["--density-curve", "characteristic"],
                "--density-curve was removed",
            ),
            (
                &["--density-curve", "exponential"],
                "--density-curve was removed",
            ),
            (
                &["--density-curve", "sigmoid"],
                "--density-curve was removed",
            ),
            (&["--density-curve"], "--density-curve was removed"),
            (&["--film-stock", "portra-400"], "--film-stock was removed"),
            (&["--film-stock"], "--film-stock was removed"),
            (
                &["--preset", "characteristic-generic"],
                "--preset was removed",
            ),
            (&["--preset", "sigmoid-knees"], "--preset was removed"),
            (&["--preset"], "--preset was removed"),
        ];
        for (flags, want) in cases {
            for new_flow in [false, true] {
                let mut argv = flags.to_vec();
                if new_flow {
                    argv.push("--new-flow");
                }
                let err = reject_removed_flags(&parse_convert(&argv))
                    .unwrap_err()
                    .to_string();
                assert!(err.contains(want), "{argv:?}: {err}");
                assert!(err.contains("rop the flag"), "{argv:?}: {err}");
                for gone in ["--density-curve exponential", "Pass --", "--film-stock <"] {
                    assert!(!err.contains(gone), "{argv:?} advises `{gone}`: {err}");
                }
            }
        }
        // `characteristic` and `sigmoid` get their own history; the plain identity does not.
        let why = |v: &str| {
            reject_removed_flags(&parse_convert(&["--density-curve", v]))
                .unwrap_err()
                .to_string()
        };
        assert!(why("characteristic").contains("reference-snapshot"));
        assert!(why("sigmoid").contains("--display-tone-headroom"));
        // Every flag the `--preset` remedy names parses.
        use clap::CommandFactory;
        let convert = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "convert")
            .unwrap()
            .clone();
        for named in [
            "print-exposure",
            "contrast",
            "channel-grade",
            "highlight-desaturation",
            "params",
        ] {
            assert!(
                convert.get_arguments().any(|a| a.get_long() == Some(named)),
                "--{named} is not a flag"
            );
        }
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
            // The remedy must not name the retired selector, which is itself refused.
            assert!(
                !err.to_string().contains("reconstruction.type"),
                "{body}: advises a refused key: {err}"
            );
            // The remedy names the curve's keys, never a retired curve.
            assert!(err.to_string().contains("gamma, anchor"), "{body}: {err}");
            assert!(!err.to_string().contains("characteristic"), "{body}: {err}");
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
            "hanten",
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
    fn the_reference_density_and_retired_anchor_flags_are_migration_errors() {
        // Every one is refused before `merge`, on both chains, with a remedy naming the
        // one placement left — and the named remedy must itself be accepted, or the
        // message sends the user in a circle.
        for flags in [
            vec!["--d-max", "1.5"],
            vec!["--fixed-d-max"],
            vec!["--auto-d-max"],
            vec!["--no-d-max"],
            vec!["--anchor-white-at-reference"],
            vec!["--anchor-mid-fraction", "0.5"],
            vec!["--anchor-black-floor", "0.005"],
        ] {
            for new_flow in [false, true] {
                let mut argv = flags.clone();
                if new_flow {
                    argv.push("--new-flow");
                }
                let err = reject_removed_flags(&parse_convert(&argv)).unwrap_err();
                assert_eq!(err.exit_code(), 2, "{argv:?}");
                let msg = err.to_string();
                assert!(
                    msg.contains(flags[0]) && msg.contains("was removed"),
                    "{msg}"
                );
                assert!(msg.contains("--anchor-mid-offset"), "{msg}");
                assert!(msg.contains("Drop the flag"), "{msg}");
                assert!(!msg.contains("characteristic"), "{msg}");
            }
        }
        // Every old spelling reaches the migration message, not clap's generic error:
        // no value, and a negative value that would otherwise read as a flag.
        for argv in [
            vec!["--d-max"],
            vec!["--d-max", "-1.5"],
            vec!["--anchor-mid-fraction", "-0.5"],
            vec!["--anchor-black-floor"],
        ] {
            let err = reject_removed_flags(&parse_convert(&argv)).unwrap_err();
            assert!(err.to_string().contains("was removed"), "{argv:?}: {err}");
        }
        let remedy = parse_convert(&["--anchor-mid-offset", "0.5"]);
        reject_removed_flags(&remedy).unwrap();
        flow::reject_unavailable_flags(
            Flow::New,
            &parse_convert(&["--anchor-mid-offset", "0.5", "--new-flow"]),
        )
        .unwrap();
        merge(base_cfg(), &remedy).unwrap();
    }

    #[test]
    fn merge_anchor_placement_flags() {
        // Written as an equality on the resolved placement — a "does not error" check
        // would still pass if the flag were silently inert, which is the failure mode
        // the four-coupled-spots rule exists to catch.
        let cfg = merge(base_cfg(), &parse_convert(&["--anchor-mid-offset", "0.5"])).unwrap();
        let e = curve_of(&cfg);
        assert_eq!(e.anchor, AnchorPlacement::MidAtBaseOffset(0.5));
        // A flag must beat a **recipe-supplied** placement, not just a defaulted one.
        // The loops above start from this build's default, so a merge arm that never
        // ran would still look right there; only a recipe that states a *different*
        // rule can tell "the flag won" from "the default happened to match".
        let recipe = exponential_cfg(ExponentialParams {
            anchor: AnchorPlacement::MidAtBaseOffset(0.25),
            ..ExponentialParams::default()
        });
        let cfg = merge(
            recipe.clone(),
            &parse_convert(&["--anchor-mid-offset", "0.7"]),
        )
        .unwrap();
        let e = curve_of(&cfg);
        assert_eq!(e.anchor, AnchorPlacement::MidAtBaseOffset(0.7));
        // …and with no flag the recipe's own placement survives untouched.
        let cfg = merge(recipe, &parse_convert(&[])).unwrap();
        let e = curve_of(&cfg);
        assert_eq!(e.anchor, AnchorPlacement::MidAtBaseOffset(0.25));
    }

    #[test]
    fn validate_rejects_a_contrast_too_small_to_place_the_anchor() {
        // The placement adds MID_GREY_OUTPUT_DECADES / contrast, so a positive-but-tiny
        // slope overflows that quotient. Before this check it passed `validate` (positive
        // and under the cap), then panicked on a debug assertion — exit 101 for what is a
        // usage error.
        // The quotient overflows below ~2.2e-39, so those are the values under test.
        for bad in [1e-40, 1e-44, 2.1e-39] {
            let cfg = exponential_cfg(ExponentialParams {
                gamma: bad,
                anchor: AnchorPlacement::MidAtBaseOffset(0.5),
            });
            let Err(err) = validate(&cfg) else {
                panic!("gamma {bad} must be rejected")
            };
            assert!(matches!(err, NcError::Usage(_)), "{bad}: {err}");
            assert!(err.to_string().contains("--density-gamma"), "{err}");
        }
        // Just above the threshold the derivation stays finite and is therefore accepted —
        // the check guards the *overflow*, not "small contrast" in general. `f32::MIN_POSITIVE`
        // gives a finite anchor of ~6.3e37, and since `contrast * anchor` is then exactly
        // MID_GREY_OUTPUT_DECADES the render is a flat mid-grey rather than a broken one.
        validate(&exponential_cfg(ExponentialParams {
            gamma: f32::MIN_POSITIVE,
            anchor: AnchorPlacement::MidAtBaseOffset(0.5),
        }))
        .unwrap();
    }

    /// A **finite** anchor can still overflow the exponent's *product*, and that renders
    /// exactly as silently as a non-finite anchor did.
    ///
    /// `10^(gamma·(d − 2e38))` is `10^(−inf)` = `0.0` for every sample: exit 0, mean
    /// `[0,0,0]`, `clipped_high 0`, `non_finite 0`, no warning. It needs no exotic slope —
    /// the shipped default gamma of 2.0 reaches it — which is why finiteness of the anchor
    /// was the wrong line to draw. The line is "does any intermediate overflow".
    #[test]
    fn validate_rejects_an_anchor_whose_product_with_the_slope_overflows() {
        let cfg = exponential_cfg(ExponentialParams {
            anchor: AnchorPlacement::MidAtBaseOffset(2e38),
            ..ExponentialParams::default()
        });
        // The round-1 guard passes this: the anchor itself is perfectly finite.
        let anchor =
            AnchorPlacement::MidAtBaseOffset(2e38).anchor(ExponentialParams::default().gamma);
        assert!(anchor.is_finite(), "{anchor}");
        let Err(err) = validate(&cfg) else {
            panic!("an overflowing exponent must be rejected")
        };
        assert!(matches!(err, NcError::Usage(_)), "{err}");
        assert!(err.to_string().contains("silently black"), "{err}");
        // Falsifiable, and the scope boundary: the same absurd offset against a slope
        // small enough to keep the product finite (`3e38 × 1e-37` = 3e1) is honest
        // arithmetic, and bounding *that* is `algo/density-safety-bounds`' job, not this
        // guard's. It must still validate.
        validate(&exponential_cfg(ExponentialParams {
            gamma: 1e-37,
            anchor: AnchorPlacement::MidAtBaseOffset(3e38),
        }))
        .unwrap();
    }

    /// Slope positivity is diagnosed **before** the anchor's division by it. Ordering
    /// the division first called `0` and `nan` "too small to place the anchor" — neither
    /// is small.
    #[test]
    fn validate_diagnoses_a_non_positive_slope_before_the_anchor_division() {
        for bad in [0.0, f32::NAN, -1.0] {
            let cfg = exponential_cfg(ExponentialParams {
                gamma: bad,
                anchor: AnchorPlacement::MidAtBaseOffset(0.5),
            });
            let Err(err) = validate(&cfg) else {
                panic!("--density-gamma {bad} must be rejected")
            };
            let msg = err.to_string();
            assert!(msg.contains("--density-gamma"), "{bad}: {msg}");
            assert!(msg.contains("must be finite and > 0"), "{bad}: {msg}");
            // Not the anchor-division diagnosis, whose remedy does not apply here.
            assert!(!msg.contains("non-finite anchor"), "{bad}: {msg}");
        }
    }

    #[test]
    fn curve_default_warning_fires_for_every_recipe_that_leaves_the_curve_unpinned() {
        // The witness is a raw-JSON probe, so test it that way.
        let probe =
            |json: &str| unpinned_curve(&serde_json::from_str::<serde_json::Value>(json).unwrap());
        // A curve pinned by *type only* looks pinned and is not: the exponential's gamma
        // and anchor defaults have both moved, so this file renders differently than it
        // did with nothing in it to show that.
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{"type":"exponential"}}}"#),
            Some(UnpinnedCurve::MovedDefaults)
        );
        // Pinning every value this build would supply silences it — the falsifiable
        // half. The per-channel gain is stated too: since `pipeline_version` 4 it is one
        // of the defaults that moved, so a recipe silent on it is not fully pinned (below).
        assert_eq!(
            probe(
                r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.0,"anchor":{"mid-at-base-offset":0.62}},"density":{"scale":[1.0,1.0,1.0]}}}"#
            ),
            None
        );
        // Either of the two alone still floats the other.
        for json in [
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.0},"density":{"scale":[1.0,1.0,1.0]}}}"#,
            r#"{"reconstruction":{"curve":{"type":"exponential","anchor":{"mid-at-base-offset":0.62}},"density":{"scale":[1.0,1.0,1.0]}}}"#,
        ] {
            assert_eq!(probe(json), Some(UnpinnedCurve::MovedDefaults), "{json}");
        }
        // **An absent `calibration` section is NOT a floating value** — this is the
        // documented pipeline-profile shape (design-spec §8), and the reference comes
        // from another `--params` layer or a flag.
        for json in [
            // a profile: the whole look pinned, no calibration at all
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":2.0,"anchor":{"mid-at-base-offset":0.62}},"density":{"scale":[1.0,1.0,1.0]}}}"#,
            // a calibration that measured only a base — what `hanten estimate` emits
            r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}},"reconstruction":{"curve":{"type":"exponential","gamma":2.0,"anchor":{"mid-at-base-offset":0.62}},"density":{"scale":[1.0,1.0,1.0]}}}"#,
        ] {
            assert_eq!(probe(json), None, "{json}");
        }
        // **The per-channel gain is the other moved default under a stated
        // `reconstruction` block.** A recipe that pins the whole curve and says nothing
        // about it *looks* fully pinned and replays in a different **colour** — the
        // class `docs/tasks/core/recipe-replay-fidelity.md` tracks. Both spellings of
        // "unstated" count, an absent `density` block and a `density` block without the
        // key, because `--dump-params` writes it either way.
        for json in [
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":2.0,"anchor":{"mid-at-base-offset":0.62}}}}"#,
            r#"{"reconstruction":{"type":"density","curve":{"type":"exponential","gamma":2.0,"anchor":{"mid-at-base-offset":0.62}},"density":{"offset":[0.0,0.0,0.0]}}}"#,
        ] {
            assert_eq!(probe(json), Some(UnpinnedCurve::DensityScale), "{json}");
        }
        // Stating the gain silences it whatever the value — including one equal to this
        // build's default, the same "warn only on shapes this build cannot produce" rule.
        for scale in ["[1.0,1.0,1.0]", "[1.0,0.84,0.73]"] {
            let json = format!(
                r#"{{"reconstruction":{{"curve":{{"type":"exponential","gamma":2.0,"anchor":{{"mid-at-base-offset":0.62}}}},"density":{{"scale":{scale}}}}}}}"#
            );
            assert_eq!(probe(&json), None, "{json}");
        }
        // A curve finding is the more specific diagnosis, so it is reported first: this
        // recipe floats the curve's defaults *and* the gain.
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{"type":"exponential"}}}"#),
            Some(UnpinnedCurve::MovedDefaults)
        );
        // The retired tag decides nothing: a curve pinning both values without it is as
        // complete as one with it, and one pinning neither floats either way.
        assert_eq!(
            probe(
                r#"{"reconstruction":{"curve":{"gamma":2.0,"anchor":{"mid-at-base-offset":0.62}},"density":{"scale":[1.0,0.84,0.73]}}}"#
            ),
            None
        );
        assert_eq!(
            probe(r#"{"reconstruction":{"curve":{}}}"#),
            Some(UnpinnedCurve::MovedDefaults)
        );
        // A recipe with no `curve` section resolves to whichever curve is the default,
        // which has moved twice — silence would be exactly the "archived recipe
        // silently reinterpreted" case design-spec §7.2 forbids, and no
        // `meta.pipeline_version` rides a bare recipe to catch it instead.
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

        assert_eq!(curve_default_warning(None, None), None);
        // A recipe recording THIS build's version was produced by these defaults,
        // so nothing moved underneath it. Without this, a sidecar `--dump-params`
        // just wrote fails its own `--strict` replay.
        assert_eq!(
            curve_default_warning(
                Some(UnpinnedCurve::MovedDefaults),
                Some(version::PIPELINE_VERSION)
            ),
            None
        );
        // A different version, or none recorded at all, still warns.
        assert!(
            curve_default_warning(
                Some(UnpinnedCurve::MovedDefaults),
                Some(version::PIPELINE_VERSION - 1)
            )
            .is_some()
        );
        let msg =
            curve_default_warning(Some(UnpinnedCurve::MovedDefaults), None).expect("must warn");
        assert!(msg.contains("mid-at-base-offset"), "{msg}");
        let msg = curve_default_warning(Some(UnpinnedCurve::WholeCurve), None).expect("must warn");
        assert!(msg.contains("reconstruction.curve"), "{msg}");
        assert!(msg.contains("exponential"), "{msg}");
        let msg =
            curve_default_warning(Some(UnpinnedCurve::DensityScale), None).expect("must warn");
        assert!(msg.contains("reconstruction.density.scale"), "{msg}");
        // The losing rules' wording must be absent: two of these messages name a curve
        // knob, and asserting only on the shared word would not tell them apart.
        assert!(!msg.contains("gamma"), "{msg}");
    }

    #[test]
    fn sets_curve_anchor_probes_the_roll_fixed_placement() {
        let probe = |json: &str| {
            sets_curve_anchor(&serde_json::from_str::<serde_json::Value>(json).unwrap())
        };
        assert!(probe(
            r#"{"reconstruction":{"curve":{"anchor":{"mid-at-base-offset":0.62}}}}"#
        ));
        assert!(probe(
            r#"{"reconstruction":{"curve":{"anchor":{"mid-at-base-offset":0.4}}}}"#
        ));
        // A restating override still counts (same rule as `sets_calibration_film_base`), and a frame
        // that touches only non-placement keys does not.
        assert!(!probe(r#"{"reconstruction":{"curve":{"gamma":2.0}}}"#));
        assert!(!probe(r#"{"print":{"print_exposure":0.5}}"#));
    }

    /// Bounds for the placement.
    #[test]
    fn validate_bounds_the_placement() {
        // `MidAtBaseOffset` is a density above the base, so strictly positive.
        for bad in [0.0, -0.5, f32::NAN, f32::INFINITY] {
            {
                let cfg = exponential_cfg(ExponentialParams {
                    anchor: AnchorPlacement::MidAtBaseOffset(bad),
                    ..ExponentialParams::default()
                });
                assert!(
                    matches!(validate(&cfg), Err(NcError::Usage(_))),
                    "offset {bad} should fail"
                );
            }
        }
        // Representative good values pass.
        validate(&exponential_cfg(ExponentialParams {
            anchor: AnchorPlacement::MidAtBaseOffset(0.5),
            ..ExponentialParams::default()
        }))
        .unwrap();
    }

    #[test]
    fn recipe_parses_tagged_curve_keys() {
        // §9 places the curve knobs under the tagged `reconstruction.curve`; with
        // `deny_unknown_fields` a misplaced key would silently reject the recipe,
        // so pin the documented nesting.
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.4,
                "anchor":{"mid-at-base-offset":0.5}}}}"#,
        )
        .unwrap();
        assert_eq!(
            *curve_of(&cfg),
            ExponentialParams {
                gamma: 1.4,
                anchor: AnchorPlacement::MidAtBaseOffset(0.5),
            }
        );
        // A partial curve fills the defaults; the retired tag decides nothing.
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.2}}}"#,
        )
        .unwrap();
        assert_eq!(gamma_of(&cfg), 1.2);
        assert_eq!(curve_of(&cfg).anchor, ExponentialParams::default().anchor);
    }

    #[test]
    fn the_calibration_section_is_closed_and_names_its_members() {
        // The section is `deny_unknown_fields`, and the error names it. Falsifiable
        // control for the strictness the open-section design relies on: a member is
        // added by adding a field, never by the section quietly accepting one.
        let err = serde_json::from_str::<ResolvedConfig>(r#"{"calibration":{"flim_base":"auto"}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("flim_base"), "{err}");
        assert!(err.contains("film_base"), "{err}");
    }

    /// A recipe's retired `calibration.dmax`: its old default `"fixed"` is on every
    /// sidecar and `--dump-params` document an earlier build wrote, so it is dropped
    /// on load and the recipe replays; any other value asked for a reference this
    /// build no longer reads, and is refused with the key named.
    #[test]
    fn a_recipes_retired_dmax_is_dropped_at_its_old_default_and_refused_otherwise() {
        let mut old = serde_json::json!({"calibration": {"film_base": "auto", "dmax": "fixed"}});
        assert!(strip_retired_keys_at_old_defaults(&mut old));
        assert_eq!(
            old,
            serde_json::json!({"calibration": {"film_base": "auto"}})
        );
        reject_legacy_recipe_keys(&old, "recipe").unwrap();

        for stated in [
            serde_json::json!({"explicit": 1.5}),
            serde_json::json!("auto"),
            serde_json::json!("none"),
        ] {
            let mut v = serde_json::json!({"calibration": {"dmax": stated.clone()}});
            assert!(!strip_retired_keys_at_old_defaults(&mut v), "{stated}");
            let err = reject_legacy_recipe_keys(&v, "recipe")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("calibration.dmax") && err.contains("mid-at-base-offset"),
                "{err}"
            );
        }
    }

    #[test]
    fn dump_params_round_trips_through_params() {
        let cfg = merge(base_cfg(), &parse_convert(&["--density-gamma", "1.8"])).unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn resolved_recipe_emits_the_documented_reconstruction_schema() {
        // Schema fixtures (design-spec §8): every resolved recipe emits
        // `reconstruction.schema_version = 1` and the curve — an omitted input curve
        // never survives normalization, and neither retired `type` is written.
        let v = serde_json::to_value(base_cfg()).unwrap();
        assert_eq!(v["reconstruction"]["schema_version"], 1);
        assert!(v["reconstruction"].get("type").is_none(), "{v}");
        assert!(v["reconstruction"]["curve"].get("type").is_none(), "{v}");
        assert_eq!(v["reconstruction"]["curve"]["gamma"], 2.0);
        assert!(v["reconstruction"]["curve"].get("dmax").is_none(), "{v}");
        assert!(v["calibration"].get("dmax").is_none(), "{v}");
        // `f32` literals, not `[1.0, 1.0, 1.0]`: the default gain is `[1, 0.84, 0.73]`
        // and `0.84f32` widens to `0.8399999737739563` as an `f64`.
        assert_eq!(
            v["reconstruction"]["density"]["scale"],
            serde_json::json!([1.0f32, 0.84f32, 0.73f32])
        );

        // Partial input: omitted curve normalizes to the tagged default curve.
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"reconstruction":{"schema_version":1,"type":"density"}}"#)
                .unwrap();
        assert_eq!(*curve_of(&cfg), ExponentialParams::default());
        let v = serde_json::to_value(&cfg).unwrap();
        assert!(v["reconstruction"]["curve"].get("type").is_none(), "{v}");

        // An unsupported schema_version is rejected loudly through the recipe.
        assert!(
            serde_json::from_str::<ResolvedConfig>(r#"{"reconstruction":{"schema_version":2}}"#)
                .is_err()
        );
    }

    #[test]
    fn reconstruction_result_serializes_the_documented_shapes() {
        // The report's resolution diagnostics (design-spec §8): the placement rule and
        // the anchor it derived. No curve `type`, stock or out-of-table block: those went
        // with the `characteristic` curve.
        let v =
            serde_json::to_value(reconstruction_result(&Reconstruction::default(), 0.99)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "curve": {
                    "anchor": {"mid-at-base-offset": 0.62f32},
                    "anchor_value": 0.99f32
                }
            })
        );
    }

    #[test]
    fn report_recipe_echo_carries_the_tagged_reconstruction() {
        // The convert report's `recipe` is the effective config, so
        // `recipe.reconstruction` is the exact schema (design-spec §8).
        let report = Report {
            recipe: Some(base_cfg()),
            ..Report::default()
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["recipe"]["reconstruction"]["schema_version"], 1);
        assert_eq!(v["recipe"]["reconstruction"]["curve"]["gamma"], 2.0);
        // Absent for non-convert reports.
        let v = serde_json::to_value(Report::default()).unwrap();
        assert!(v.get("recipe").is_none());
        assert!(v.get("reconstruction_result").is_none());
    }

    #[test]
    fn the_retired_output_selectors_are_migration_errors_from_flag_and_recipe() {
        // `nf-retire/legacy-custom`: the three selectors retired with the only presets
        // that read them. Each spelling must name itself, its recipe key, and the
        // retirement — never parse as an unknown argument or an unknown field, which
        // would tell the user nothing about where the ability went.
        for (flag, value, key) in [
            ("--out-depth", "f32", "depth"),
            ("--out-depth", "u16", "depth"),
            ("--output-profile", "prophoto", "output_profile"),
            ("--bigtiff", "on", "bigtiff"),
            ("--bigtiff", "auto", "bigtiff"),
        ] {
            let err = reject_removed_flags(&parse_convert(&[flag, value])).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{flag} {value}");
            let msg = err.message();
            assert!(msg.contains(flag), "{flag}: {msg}");
            assert!(msg.contains(&format!("output.{key}")), "{flag}: {msg}");
            assert!(msg.contains("`legacy`"), "{flag}: {msg}");

            // The recipe path names the same replacement, and — being a file the user
            // edits, not a flag they typed — says to remove the key.
            let replacement = removed_output_selector(key).replacement;
            assert!(msg.contains(replacement), "{flag}: {msg}");
            let mut recipe = serde_json::json!({ "output": { key: value } });
            if strip_retired_keys_at_old_defaults(&mut recipe) {
                // The value every earlier build wrote by default: dropped, not refused.
                assert!(recipe["output"].get(key).is_none(), "{key}: {recipe}");
                reject_legacy_recipe_keys(&recipe, "recipe").unwrap();
                continue;
            }
            let err = reject_legacy_recipe_keys(&recipe, "recipe").unwrap_err();
            assert_eq!(err.exit_code(), 2, "{key}");
            let rmsg = err.message();
            assert!(rmsg.contains(&format!("`output.{key}`")), "{key}: {rmsg}");
            assert!(rmsg.contains(replacement), "{key}: {rmsg}");
            assert!(rmsg.contains("Remove the key"), "{key}: {rmsg}");
            // It must not blame a flag the user never typed.
            assert!(
                !rmsg.contains(&format!("{flag} (recipe key")),
                "{key}: {rmsg}"
            );
        }
        // Each points at something that exists: the depth message names presets
        // `parse` accepts.
        let msg = reject_removed_flags(&parse_convert(&["--out-depth", "f32"]))
            .unwrap_err()
            .message()
            .to_string();
        for replacement in ["display-p3", "film-master", "hdr-linear-tiff"] {
            assert!(OutputPreset::parse(replacement).is_ok());
            assert!(msg.contains(replacement), "{msg}");
        }
        assert!(reject_removed_flags(&parse_convert(&[])).is_ok());
    }

    #[test]
    fn a_recipe_written_before_the_retirement_still_loads() {
        // Every sidecar and `--dump-params` document the previous build wrote carried
        // all three retired selectors at their defaults — bare and enveloped alike.
        // Refusing them would refuse every recipe on disk, including ones already on a
        // surviving preset.
        let old_output = r#""output":{"preset":"display-p3","depth":"u16","output_profile":null,"bigtiff":"auto"}"#;
        for (name, body) in [
            ("bare", format!("{{{old_output}}}")),
            (
                "enveloped",
                format!(r#"{{"meta":{{"nc_version":"0.1.0"}},"params":{{{old_output}}}}}"#),
            ),
        ] {
            let loaded = load_recipe_body(name, &body)
                .unwrap_or_else(|e| panic!("{name}: an old default recipe must load: {e}"));
            let RecipeDoc::Current(cfg) = loaded.doc else {
                panic!("{name}: the current chain's recipe");
            };
            assert_eq!(cfg.output.preset, OutputPreset::DisplayP3, "{name}");
        }
        // A non-default value is still refused, since it asked for something.
        let err = load_recipe_body(
            "non-default",
            r#"{"output":{"preset":"display-p3","depth":"f32"}}"#,
        )
        .unwrap_err();
        assert!(err.message().contains("Remove the key"), "{err}");
    }

    #[test]
    fn removed_depth_flags_and_recipe_key_emit_migration_errors() {
        // nc is unreleased: the old spellings are rejections, never aliases.
        for flag in ["--output-hdr", "--output-sdr"] {
            let err = reject_removed_flags(&parse_convert(&[flag])).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{flag}");
            let msg = err.to_string();
            // `--out-depth`, which replaced them, has retired too, so the message
            // points past it at the presets rather than at a flag that is gone.
            assert!(msg.contains("--out-depth"), "{flag}: {msg}");
            assert!(msg.contains("display-p3"), "{flag}: {msg}");
            assert!(msg.contains("film-master"), "{flag}: {msg}");
        }
        let err =
            reject_legacy_recipe_keys(&serde_json::json!({"output": {"hdr": true}}), "recipe")
                .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        let msg = err.to_string();
        assert!(msg.contains("output.depth"), "{msg}");
        assert!(msg.contains("display-p3"), "{msg}");
        // A recipe naming only the preset is untouched by the migration check.
        reject_legacy_recipe_keys(
            &serde_json::json!({"output": {"preset": "display-p3"}}),
            "recipe",
        )
        .unwrap();
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
        // An atomic policy choice, so the flag also replaces a recipe's named preset
        // (flags win in both directions).
        assert_eq!(
            merge(recipe, &parse_convert(&["--output-preset", "display-p3"]))
                .unwrap()
                .output
                .preset,
            OutputPreset::DisplayP3
        );
        // No flag, no recipe key → the default, which is `gain-map-hdr` since the
        // `output/presets` migration (it was `legacy`).
        assert_eq!(
            merge(base_cfg(), &parse_convert(&[]))
                .unwrap()
                .output
                .preset,
            OutputPreset::GainMapHdr
        );
        // The flag shares `OutputPreset::parse`, so a renamed/unknown value is a
        // usage error at merge, not a silent fallback to the default.
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
        // `film-master` bypasses print controls entirely; every other preset reaches
        // the shared display stage and consumes the value.
        let cfg_with = |preset: OutputPreset| ResolvedConfig {
            print: PrintParams {
                linear_range: [0.02, 0.97],
                ..PrintParams::default()
            },
            output: OutputParams { preset },
            ..base_cfg()
        };
        // film-master → the print-control bypass sweep.
        let msg = validate_err(&cfg_with(OutputPreset::FilmMaster));
        assert!(msg.contains("linear_range"), "{msg}");
        assert!(
            msg.contains("bypasses all print and display controls"),
            "{msg}"
        );
        for preset in OutputPreset::ALL {
            if preset != OutputPreset::FilmMaster {
                validate(&cfg_with(preset)).unwrap();
            }
        }
        // The default pair is of course fine on both branches.
        validate(&base_cfg()).unwrap();
        validate(&film_master_cfg()).unwrap();
    }

    #[test]
    fn the_retired_display_tone_flags_are_refused_with_a_migration_error() {
        // On both chains: `reject_removed_flags` runs before the availability table, so
        // each remedy must hold under `--new-flow` too — and every one names only the
        // headroom flag, which both chains keep.
        for new_flow in [false, true] {
            let run = |extra: &[&str]| {
                let mut argv = extra.to_vec();
                if new_flow {
                    argv.push("--new-flow");
                }
                reject_removed_flags(&parse_convert(&argv))
                    .unwrap_err()
                    .to_string()
            };
            for (value, remedy) in [
                ("shoulder", "has no replacement"),
                ("none", "--display-tone-headroom 0"),
                ("reinhard", "always"),
                ("sigmoid", "was never a tone"),
            ] {
                let msg = run(&["--display-tone", value]);
                assert!(msg.contains("--display-tone was removed"), "{value}: {msg}");
                assert!(msg.contains(remedy), "{value}: {msg}");
                assert!(msg.contains("fit_range.headroom_stops"), "{value}: {msg}");
            }
            let msg = run(&["--highlight-compress", "0"]);
            assert!(msg.contains("--highlight-compress was removed"), "{msg}");
            assert!(msg.contains("--display-tone-headroom"), "{msg}");
        }
        // Falsifiable: the headroom flag itself is not a removed flag.
        reject_removed_flags(&parse_convert(&["--display-tone-headroom", "4"])).unwrap();
    }

    #[test]
    fn the_retired_display_tone_recipe_keys_are_refused_with_a_migration_error() {
        // Every `print.display_tone` value is refused, the old default included: replaying
        // `"shoulder"` would render differently, so stripping it would be a silent change.
        for (value, remedy) in [
            (serde_json::json!("shoulder"), "has no replacement"),
            (serde_json::json!("none"), "\"headroom_stops\": 0"),
            (
                serde_json::json!({"reinhard": {"headroom_stops": 4.0}}),
                "\"headroom_stops\": 4",
            ),
            (serde_json::json!("reinhard"), "remove the key"),
        ] {
            let mut v = serde_json::json!({"print": {"display_tone": value}});
            assert!(!strip_retired_keys_at_old_defaults(&mut v), "{value}");
            let msg = reject_legacy_recipe_keys(&v, "recipe r.json")
                .unwrap_err()
                .to_string();
            assert!(msg.contains("`print.display_tone`"), "{value}: {msg}");
            assert!(msg.contains(remedy), "{value}: {msg}");
            // Only the two retired tones are lost renders; a moved `reinhard` is not.
            let lost = matches!(value.as_str(), Some("shoulder" | "none"));
            assert_eq!(msg.contains("reference build"), lost, "{value}: {msg}");
            assert_eq!(msg.contains("render is unchanged"), !lost, "{value}: {msg}");
        }
        // `highlight_compress` at its old default is stripped, so a sidecar replays past
        // it; any other width is refused.
        let mut v = serde_json::json!({"print": {"highlight_compress": 0.0}});
        assert!(strip_retired_keys_at_old_defaults(&mut v));
        reject_legacy_recipe_keys(&v, "recipe r.json").unwrap();
        let mut v = serde_json::json!({"print": {"highlight_compress": 0.4}});
        assert!(!strip_retired_keys_at_old_defaults(&mut v));
        let msg = reject_legacy_recipe_keys(&v, "recipe r.json")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("`print.highlight_compress`"), "{msg}");
    }

    #[test]
    fn merge_display_tone_headroom_sets_fit_range() {
        // The merge arm — a forgotten one silently makes the flag a no-op.
        let cfg = merge(
            base_cfg(),
            &parse_convert(&["--display-tone-headroom", "3"]),
        )
        .unwrap();
        assert_eq!(cfg.fit_range.headroom_stops, 3.0);
        // Absent flag → the recipe's value survives.
        let recipe: ResolvedConfig =
            serde_json::from_str(r#"{"fit_range":{"headroom_stops":10.0}}"#).unwrap();
        assert_eq!(
            merge(recipe.clone(), &parse_convert(&[]))
                .unwrap()
                .fit_range
                .headroom_stops,
            10.0
        );
        // The flag wins over the recipe.
        assert_eq!(
            merge(recipe, &parse_convert(&["--display-tone-headroom", "3"]))
                .unwrap()
                .fit_range
                .headroom_stops,
            3.0
        );
        // No flag, no recipe key → the documented default.
        assert_eq!(
            base_cfg().fit_range.headroom_stops,
            crate::types::DEFAULT_HEADROOM_STOPS
        );
    }

    #[test]
    fn the_headroom_is_a_value_rule_so_roll_inherits_it() {
        // `roll` and every per-frame override reach `validate`, never `validate_convert`
        // — so a bound that lives only in `Headroom::new` is no gate at all for them: a
        // 36-frame roll decoded and reconstructed 36 times before failing. `validate`
        // must refuse the number itself, before anything is opened.
        let cfg = |stops: f32| ResolvedConfig {
            fit_range: crate::recipe::FitRange {
                headroom_stops: stops,
            },
            output: OutputParams {
                preset: OutputPreset::DisplayP3,
            },
            ..base_cfg()
        };
        for bad in [-1.0, f32::NAN, f32::INFINITY, 30.0] {
            let msg = validate_err(&cfg(bad));
            assert!(msg.contains("--display-tone-headroom"), "{bad}: {msg}");
            assert!(Headroom::new(bad).is_err(), "{bad}");
        }
        // Falsifiable: the endpoints of the accepted range validate clean, and the
        // renderer agrees with the gate on every one of them — the single-definition
        // property `types::check_headroom_stops` exists to keep.
        for good in [
            0.0,
            crate::types::DEFAULT_HEADROOM_STOPS,
            crate::types::MAX_HEADROOM_STOPS,
        ] {
            validate(&cfg(good)).unwrap_or_else(|e| panic!("{good}: {e}"));
            Headroom::new(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
    }

    #[test]
    fn film_master_refuses_a_non_default_headroom_and_accepts_the_reset() {
        // The master applies no display tone, so a stated headroom would be silently
        // ignored — refused by the resolved *value*, whichever provenance set it.
        let recipe: ResolvedConfig =
            serde_json::from_str(r#"{"fit_range":{"headroom_stops":3.0}}"#).unwrap();
        let master = |recipe: ResolvedConfig, extra: &[&str]| {
            let mut argv = vec!["--output-preset", "film-master", "--auto-base"];
            argv.extend_from_slice(extra);
            merge(recipe, &parse_convert(&argv)).unwrap()
        };
        for cfg in [
            master(recipe.clone(), &[]),
            master(base_cfg(), &["--display-tone-headroom", "3"]),
        ] {
            let msg = validate_err(&cfg);
            assert!(msg.contains("fit_range.headroom_stops"), "{msg}");
            assert!(
                msg.contains("bypasses all print and display controls"),
                "{msg}"
            );
        }
        // The flags-win reset: typing the default clears a recipe's value, which is what
        // lets one recipe serve every preset. A presence rule would kill it.
        validate(&master(recipe, &["--display-tone-headroom", "6"])).unwrap();
        // Every display preset takes a non-default headroom.
        for preset in OutputPreset::ALL {
            if preset != OutputPreset::FilmMaster {
                let cfg = ResolvedConfig {
                    fit_range: crate::recipe::FitRange {
                        headroom_stops: 3.0,
                    },
                    output: OutputParams { preset },
                    ..base_cfg()
                };
                validate(&cfg).unwrap_or_else(|e| panic!("{preset:?}: {e}"));
            }
        }
    }

    #[test]
    fn ultra_hdr_preset_is_convert_only_and_requires_a_jpeg_suffix() {
        let cfg = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::UltraHdrV1,
            },
            ..base_cfg()
        };
        let mut args = parse_convert(&["--output-preset", "ultra-hdr-v1"]);
        assert!(validate_convert(&cfg, &args, RecipePreset::Unstated).is_err());
        args.output = PathBuf::from("out.JPEG");
        validate_convert(&cfg, &args, RecipePreset::Unstated).unwrap();
        // Roll-capable now that names are container-aware: it derives
        // `<stem>_positive.jpg` for this preset.
        reject_roll_unsupported(&cfg).unwrap();
    }

    #[test]
    fn hdr_avif_presets_are_convert_only_and_require_an_avif_suffix() {
        for (preset, name) in [
            (OutputPreset::HdrPq, "hdr-pq"),
            (OutputPreset::HdrHlg, "hdr-hlg"),
        ] {
            let cfg = ResolvedConfig {
                output: OutputParams { preset },
                ..base_cfg()
            };
            // A `.tiff` (or the default) path is rejected; `.avif` in any case passes.
            let mut args = parse_convert(&["--output-preset", name]);
            let err = validate_convert(&cfg, &args, RecipePreset::Unstated)
                .unwrap_err()
                .to_string();
            assert!(err.contains(".avif"), "{name}: {err}");
            assert!(err.contains(name), "{name}: {err}");
            args.output = PathBuf::from("out.AVIF");
            validate_convert(&cfg, &args, RecipePreset::Unstated).unwrap();

            // Roll-capable now: `derived_extension` gives it `.avif`.
            reject_roll_unsupported(&cfg).unwrap();

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
                output: OutputParams { preset },
                ..base_cfg()
            };
            let mut args = parse_convert(&["--output-preset", name]);
            args.output = PathBuf::from("out.avif");
            let err = validate_convert(&cfg, &args, RecipePreset::Unstated)
                .unwrap_err()
                .to_string();
            assert!(err.contains(".tif"), "{name}: {err}");
            assert!(err.contains(name), "{name}: {err}");
            args.output = PathBuf::from("out.TIF");
            validate_convert(&cfg, &args, RecipePreset::Unstated).unwrap();

            // Roll-capable since roll derives container-aware names. This used to
            // assert the opposite; the refusal existed only because roll hardcoded
            // `.tiff`, and `derived_extension` closed that.
            reject_roll_unsupported(&cfg).unwrap();

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
        assert_eq!(required_extensions(OutputPreset::HdrPq), &["avif"]);
        assert_eq!(
            required_extensions(OutputPreset::HdrPqTiff),
            &["tif", "tiff"]
        );
    }

    #[test]
    fn hdr_linear_tiff_is_convert_only_and_requires_a_tiff_suffix() {
        let cfg = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::HdrLinearTiff,
            },
            ..base_cfg()
        };

        // `.jpg` is rejected and the message names both the preset and what it wants.
        let mut args = parse_convert(&["--output-preset", "hdr-linear-tiff"]);
        args.output = PathBuf::from("out.jpg");
        let err = validate_convert(&cfg, &args, RecipePreset::Unstated)
            .unwrap_err()
            .to_string();
        assert!(err.contains(".tif"), "{err}");
        assert!(err.contains("hdr-linear-tiff"), "{err}");

        // Both spellings pass, in any case.
        for name in ["out.tif", "out.TIFF", "out.tiff"] {
            args.output = PathBuf::from(name);
            validate_convert(&cfg, &args, RecipePreset::Unstated)
                .unwrap_or_else(|e| panic!("{name} should be accepted: {e}"));
        }

        // Roll accepts it now. The old refusal was roll's own naming gap rather than
        // anything about this container — which is why this preset, whose derived
        // `_positive.tiff` name already satisfied its suffix rule, was refused anyway.
        reject_roll_unsupported(&cfg).unwrap();

        // A non-default `--linear-range` **is** accepted: this preset genuinely
        // consumes it through the shared display stage, so rejecting it would refuse a
        // knob that works.
        let ranged = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::HdrLinearTiff,
            },
            print: PrintParams {
                linear_range: [0.05, 0.95],
                ..PrintParams::default()
            },
            ..base_cfg()
        };
        validate(&ranged).unwrap();
        // Control: the same range under `film-master` is still an error, so the
        // assertion above is proving preset-specific behaviour and not that the rule
        // stopped working altogether.
        let master_ranged = ResolvedConfig {
            print: ranged.print.clone(),
            ..film_master_cfg()
        };
        assert!(validate(&master_ranged).is_err());

        // The primary *and* the IR export are f32 here.
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
        // transfer at all, where the other two answer `None` for the opposite
        // reason — they are not HDR renditions. Both readings are "no transfer", so
        // the mapping must be pinned for it too or the interesting case is the one
        // nothing guards.
        for other in [
            OutputPreset::HdrLinearTiff,
            OutputPreset::FilmMaster,
            OutputPreset::UltraHdrV1,
        ] {
            assert_eq!(hdr::transfer_for(other), None, "{other:?}");
        }
    }

    #[test]
    fn sdr_presets_are_convert_only_and_require_a_tiff_suffix() {
        for (preset, name) in [
            (OutputPreset::DisplayP3, "display-p3"),
            (OutputPreset::Compatibility, "compatibility"),
        ] {
            let cfg = ResolvedConfig {
                output: OutputParams { preset },
                ..base_cfg()
            };

            // Suffix: `.jpg` refused naming both the preset and what it wants, every
            // TIFF spelling accepted in any case (the falsifiable half — a rule that
            // rejected *every* path would pass the first assertion alone).
            let mut args = parse_convert(&["--output-preset", name]);
            args.output = PathBuf::from("out.jpg");
            let err = validate_convert(&cfg, &args, RecipePreset::Unstated)
                .unwrap_err()
                .to_string();
            assert!(err.contains(".tif"), "{name}: {err}");
            assert!(err.contains(name), "{name}: {err}");
            for path in ["out.tif", "out.TIFF", "out.tiff"] {
                args.output = PathBuf::from(path);
                validate_convert(&cfg, &args, RecipePreset::Unstated)
                    .unwrap_or_else(|e| panic!("{name}: {path} should be accepted: {e}"));
            }

            // Roll-capable now; it derives `<stem>_positive.tiff`.
            reject_roll_unsupported(&cfg).unwrap();

            // A non-default `--linear-range` *is* accepted: these presets reach the
            // shared display stage.
            let ranged = ResolvedConfig {
                print: PrintParams {
                    linear_range: [0.05, 0.95],
                    ..PrintParams::default()
                },
                ..cfg.clone()
            };
            validate(&ranged).unwrap_or_else(|e| panic!("{name}: --linear-range: {e}"));

            // 16-bit integer for the primary *and* any IR plane — the point of
            // "losslessly stored SDR".
            assert_eq!(cfg.output.depth(), crate::types::OutDepth::U16);
            // And they are not Rec.2100 renditions, so they own no transfer.
            assert_eq!(hdr::transfer_for(preset), None, "{name}");
        }
    }

    #[test]
    fn a_missing_extension_is_completed_from_the_resolved_container() {
        // An extensionless path used to be a usage error; it is now completed from
        // whatever container the preset resolved. This is the behaviour swap, pinned
        // under each provenance so neither arm can quietly go back to refusing.
        let cfg = base_cfg();
        let mut args = parse_convert(&[]);
        args.output = PathBuf::from("positive");
        validate_convert(&cfg, &args, RecipePreset::Unstated).unwrap();
        assert_eq!(
            resolve_output_path(&args.output, cfg.output.preset, SuffixContext::Default).unwrap(),
            PathBuf::from("positive.jpg")
        );
        let named = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::FilmMaster,
            },
            ..base_cfg()
        };
        assert_eq!(
            resolve_output_path(&args.output, named.output.preset, SuffixContext::Chosen).unwrap(),
            PathBuf::from("positive.tiff"),
            "a TIFF preset completes the same stem to its own container"
        );
    }

    #[test]
    fn a_mismatched_suffix_blames_the_path_not_an_unpassed_preset() {
        // A suffix nc's containers know but this preset does not is still the usage
        // error it has always been — completion never rescues a *stated* one. With no
        // `--output-preset` typed the message must not blame a flag the user never
        // passed; it names the *default* preset instead. That distinction stopped
        // being derivable from the preset value when the default became a named one,
        // which is why `SuffixContext` exists.
        let cfg = base_cfg();
        let mut args = parse_convert(&[]);
        args.output = PathBuf::from("positive.tiff");
        let err = validate_convert(&cfg, &args, RecipePreset::Unstated)
            .unwrap_err()
            .to_string();
        assert!(err.contains(".jpg"), "{err}");
        assert!(err.contains("gain-map-hdr"), "{err}");
        assert!(
            !err.contains("--output-preset gain-map-hdr"),
            "the default path must not blame a flag the user never passed: {err}"
        );
        // The remedy the new rule adds has to be in the message someone actually
        // meets, or nobody learns that dropping the suffix works.
        assert!(err.contains("drop it"), "{err}");
        // The same mismatch *with* the flag blames the preset by name.
        let mut chosen = parse_convert(&["--output-preset", "gain-map-hdr"]);
        chosen.output = PathBuf::from("positive.tiff");
        let err = validate_convert(&cfg, &chosen, RecipePreset::Unstated)
            .unwrap_err()
            .to_string();
        assert!(err.contains("output preset `gain-map-hdr`"), "{err}");
        assert!(err.contains("no suffix at all"), "{err}");
        // Control: a named TIFF preset refuses the converse path and names itself,
        // and a `.jpg` path under the default is of course accepted.
        let named = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::FilmMaster,
            },
            ..base_cfg()
        };
        args.output = PathBuf::from("positive.jpg");
        let err = validate_convert(&named, &args, RecipePreset::Unstated)
            .unwrap_err()
            .to_string();
        assert!(err.contains("film-master"), "{err}");
        validate_convert(&cfg, &args, RecipePreset::Unstated).unwrap();
    }

    #[test]
    fn a_dotted_stem_is_completed_and_a_known_spelling_is_judged() {
        // The rule that tells a suffix from a stem: a dot-segment is a container
        // request only when *some* preset accepts that spelling. `-o out.v2` and
        // `-o roll-1.2` are stems and keep their dot; `.tif` is a spelling nc knows,
        // so under a JPEG preset it is the mismatch error rather than a stem.
        let jpeg = OutputPreset::GainMapHdr;
        for (given, want) in [
            ("out.v2", "out.v2.jpg"),
            ("roll-1.2", "roll-1.2.jpg"),
            ("scan.2026-09-22", "scan.2026-09-22.jpg"),
            // Degenerate but consistent: an empty dot-segment is not a spelling nc
            // knows, and nothing the user typed is ever dropped.
            ("out.", "out..jpg"),
            // A leading dot with nothing after it is a stem, not an extension.
            (".hidden", ".hidden.jpg"),
            ("dir/out", "dir/out.jpg"),
        ] {
            assert_eq!(
                resolve_output_path(Path::new(given), jpeg, SuffixContext::Default).unwrap(),
                PathBuf::from(want),
                "{given}"
            );
        }
        // The falsifiable half: a *known* spelling is judged, never appended to.
        let err = resolve_output_path(Path::new("out.tif"), jpeg, SuffixContext::Chosen)
            .unwrap_err()
            .to_string();
        assert!(err.contains(".jpg"), "{err}");
        // …and one this container accepts survives byte for byte, spelling and case
        // included.
        for keep in ["out.jpeg", "out.JPG", "out.v2.jpeg"] {
            assert_eq!(
                resolve_output_path(Path::new(keep), jpeg, SuffixContext::Chosen).unwrap(),
                PathBuf::from(keep),
                "{keep}"
            );
        }
        // A path that names no file has nothing to append to, and says so.
        for bad in [".", "..", "/"] {
            let err = resolve_output_path(Path::new(bad), jpeg, SuffixContext::Default)
                .unwrap_err()
                .to_string();
            assert!(err.contains("names no file"), "{bad}: {err}");
        }
        // A path whose last meaningful component is a *directory*. `file_name()`
        // normalises the trailing separator — and an interior `.` — away, so
        // appending would quietly write a sibling (`dir/` and `dir/.` both →
        // `dir.jpg`) instead of the file inside it the user meant. That is the
        // muscle-memory mistake (roll's sibling flag is spelled `--out-dir
        // positives/`), and `dir/.` is the spelling a manifest writes as `"."`.
        // Refused, not completed.
        for bad in ["dir/", "dir//", "dir/./", "dir/.", "a/b/.", "./out/"] {
            let err = resolve_output_path(Path::new(bad), jpeg, SuffixContext::Default)
                .unwrap_err()
                .to_string();
            assert!(err.contains("names a directory"), "{bad}: {err}");
            assert!(err.contains(bad), "{bad} must be named back: {err}");
        }
        // The two shapes stay distinct: a path `file_name()` itself declines keeps
        // the "names no file" wording, including `dir/..`, which the directory arm's
        // string test would never see.
        for bad in [".", "..", "/", "dir/.."] {
            let err = resolve_output_path(Path::new(bad), jpeg, SuffixContext::Default)
                .unwrap_err()
                .to_string();
            assert!(err.contains("names no file"), "{bad}: {err}");
            assert!(!err.contains("names a directory"), "{bad}: {err}");
        }
        // The falsifiable controls. Same paths *without* the trailing directory
        // component still complete — including a **leading** `./`, which is not a
        // trailing one — and `out.` is the documented degenerate stem, which pins
        // that the new rule is "separator then `.`" and not merely `ends_with('.')`.
        for (stem, want) in [
            ("dir", "dir.jpg"),
            ("a/b", "a/b.jpg"),
            ("./out", "./out.jpg"),
            ("out.", "out..jpg"),
            (".hidden", ".hidden.jpg"),
        ] {
            assert_eq!(
                resolve_output_path(Path::new(stem), jpeg, SuffixContext::Default).unwrap(),
                PathBuf::from(want),
                "{stem}"
            );
        }
    }

    #[test]
    fn a_roll_frames_unappendable_output_is_attributed_and_gets_a_remedy_it_can_reach() {
        // The rule `suffix_mismatch_error`'s arms already follow: a remedy must be
        // one the *reader's* command line can reach. A roll user meeting this is
        // running `hanten roll --out-dir` at that moment, so telling them to use it
        // is advice they are already taking — and with 40 manifest entries, a message
        // that does not say which frame is unactionable.
        let jpeg = OutputPreset::GainMapHdr;
        let frame = Path::new("/scans/f12.tif");
        for bad in ["/out/.", "/out/", "/out/sub/"] {
            let err = resolve_output_path(Path::new(bad), jpeg, SuffixContext::RollFrame(frame))
                .unwrap_err()
                .to_string();
            assert!(err.contains("frame /scans/f12.tif"), "{bad}: {err}");
            assert!(err.contains("`output`"), "{bad}: {err}");
            // The losing wording must be *absent*, not merely out-ranked — asserting
            // only that the right words appear cannot tell the two arms apart.
            assert!(!err.contains("hanten roll --out-dir"), "{bad}: {err}");
        }
        // …and `convert` keeps the wording that fits *its* reader, which is what
        // makes the assertion above falsifiable rather than vacuous.
        let err = resolve_output_path(Path::new("/out/."), jpeg, SuffixContext::Default)
            .unwrap_err()
            .to_string();
        assert!(err.contains("hanten roll --out-dir"), "{err}");
        assert!(!err.contains("frame "), "{err}");
    }

    #[test]
    fn the_known_suffix_set_is_the_union_of_every_presets_own_table() {
        // `is_container_suffix` is what separates a mismatch from a stem. The first
        // loop is true **by construction** today — `required_extensions` *is*
        // `container_for(preset).accepted()` and the union folds over the same
        // function — so it cannot fail as written; it is kept as a guard against
        // someone reimplementing either side independently, not as coverage.
        //
        // The falsifiable parts are below and in the second assertion: the foreign
        // spellings that must *not* read as containers, the `derived_extension ∈
        // accepted` check (two different tables could disagree), and the three
        // pinned canonical spellings.
        for preset in OutputPreset::ALL {
            for spelling in required_extensions(preset) {
                assert!(
                    is_container_suffix(OsStr::new(spelling)),
                    "{} accepts .{spelling} but the union does not know it",
                    preset.name()
                );
                // Case-insensitively too, which is how the accept check reads it.
                assert!(is_container_suffix(OsStr::new(&spelling.to_uppercase())));
            }
            // Every preset's *supplied* spelling is one its own table accepts — the
            // invariant that lets a completed or derived path skip re-checking.
            assert!(
                accepts(container_for(preset), OsStr::new(derived_extension(preset))),
                "{}",
                preset.name()
            );
        }
        // Falsifiable: a format nc does not write is a stem, not a suffix.
        for foreign in ["png", "webp", "exr", "v2"] {
            assert!(!is_container_suffix(OsStr::new(foreign)), "{foreign}");
        }
        // The spellings themselves, pinned so they cannot drift silently: `tiff` over
        // the `tif` that heads the accepted list, `jpg` over `jpeg`.
        assert_eq!(derived_extension(OutputPreset::DisplayP3), "tiff");
        assert_eq!(derived_extension(OutputPreset::GainMapHdr), "jpg");
        assert_eq!(derived_extension(OutputPreset::HdrPq), "avif");
    }

    #[test]
    fn a_recipe_chosen_preset_is_blamed_by_name_not_reported_as_the_default() {
        // A preset selected in `--params` is just as chosen as one typed, but the
        // merge erases the difference — so without the recipe witness the message
        // took the default arm and stated a falsehood: "with no --output-preset, nc
        // writes `display-p3`", when nc's default is `gain-map-hdr` and the *recipe*
        // picked `display-p3`. That sends the reader hunting for a default that does
        // not exist.
        let cfg = ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::DisplayP3,
            },
            ..base_cfg()
        };
        let mut args = parse_convert(&[]);
        args.output = PathBuf::from("out.jpg");
        let err = validate_convert(&cfg, &args, RecipePreset::Stated)
            .unwrap_err()
            .to_string();
        assert!(err.contains("output preset `display-p3`"), "{err}");
        assert!(
            !err.contains("--output-preset"),
            "the recipe path must not blame a flag either: {err}"
        );
        // Falsifiable: the *same* config with nothing stated is the genuine default
        // arm, and that message does name the no-preset default.
        let err = validate_convert(&cfg, &args, RecipePreset::Unstated)
            .unwrap_err()
            .to_string();
        assert!(err.contains("with no --output-preset"), "{err}");
    }

    #[test]
    fn load_recipe_records_whether_the_file_stated_an_output_preset() {
        // The witness behind the test above. `output.preset` resolves to a *named*
        // default, so only the raw JSON distinguishes "the recipe chose it" from
        // "nobody did" — a stated default is invisible once the config resolves.
        let stated =
            load_recipe_body("preset-stated", r#"{"output":{"preset":"display-p3"}}"#).unwrap();
        assert!(stated.output_preset_present);
        // A recipe that touches `output` without naming a preset has not stated one.
        let other = load_recipe_body("preset-absent", r#"{"output":{}}"#).unwrap();
        assert!(!other.output_preset_present);
        assert!(!load_recipe(None).unwrap().output_preset_present);
        // The envelope's `params` body is the witness, not the whole document.
        let wrapped = load_recipe_body(
            "preset-wrapped",
            r#"{"meta":{"nc_version":"0.1.0"},"params":{"output":{"preset":"film-master"}}}"#,
        )
        .unwrap();
        assert!(wrapped.output_preset_present);
    }

    #[test]
    fn roll_capability_is_not_derived_from_the_suffix_table() {
        // The break this pins actually happened: completing `required_extensions`
        // made every preset "pin a suffix", and roll refused all of them. The two
        // properties must be independently true for `film-master` — it states a
        // suffix *and* is roll-capable.
        assert!(!required_extensions(OutputPreset::FilmMaster).is_empty());
        reject_roll_unsupported(&film_master_cfg()).unwrap();
        // And every other preset: each states a suffix, and each is roll-capable.
        for preset in OutputPreset::ALL {
            assert!(!required_extensions(preset).is_empty(), "{}", preset.name());
            let cfg = ResolvedConfig {
                output: OutputParams { preset },
                ..base_cfg()
            };
            reject_roll_unsupported(&cfg).unwrap();
        }
    }

    #[test]
    fn film_master_rejects_every_non_default_print_control() {
        // The master bypasses stage 4, so a requested adjustment must be a loud
        // error, never silently dropped. Exhaustive over the print struct — the
        // destructuring in `validate_output_preset` makes a newly added control fail
        // to compile there, and this test pins the behaviour for each existing one.
        let cases: [(&str, PrintParams); 4] = [
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
        ];
        for (name, print) in cases {
            let cfg = ResolvedConfig {
                print,
                ..film_master_cfg()
            };
            let msg = validate_err(&cfg);
            assert!(msg.contains(name), "{name}: {msg}");
            assert!(msg.contains("film-master"), "{name}: {msg}");
            // The error must offer no ignore-conflicting-controls escape, and must point
            // at the float TIFF that does apply the controls.
            assert!(msg.contains("hdr-linear-tiff"), "{name}: {msg}");
            assert!(!msg.contains("`custom`"), "{name}: {msg}");
        }
        // The all-default print block is what the master requires.
        validate(&film_master_cfg()).unwrap();
    }

    #[test]
    fn roll_frame_override_of_output_preset_is_flagged_as_a_consistency_break() {
        // `output.preset` is a roll-fixed choice, like `film_base`, and the coarsest:
        // it changes which branch out of the ACEScg boundary a frame takes, so the frame
        // is a different image class. It once was the only one that warned about nothing.
        let probe = |json: &str| sets_output_preset(&serde_json::from_str(json).unwrap());
        assert!(probe(r#"{"output":{"preset":"display-p3"}}"#));
        assert!(probe(r#"{"output":{"preset":"film-master"}}"#));
        // A raw-JSON probe like `sets_calibration_film_base`: an override that merely *restates*
        // the shared preset is still a per-frame assertion, and the roll report has
        // nowhere else to surface it (`FrameStatus` carries no `output_render`).
        assert!(!probe(r#"{"output":{}}"#));
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
        recipe.calibration.film_base = Some(FilmBaseSource::Auto);
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

        // A branch with no display tone stage omits `display_tone` entirely — the key
        // set above is that assertion. A display preset names the operator and its
        // headroom, and its `content` states what the branch does *besides* tone rather
        // than naming a curve.
        for preset in [OutputPreset::DisplayP3, OutputPreset::HdrLinearTiff] {
            for stops in [0.0f32, 6.0, 10.0] {
                let block = value(&ResolvedConfig {
                    fit_range: crate::recipe::FitRange {
                        headroom_stops: stops,
                    },
                    output: OutputParams { preset },
                    ..base_cfg()
                });
                let operator = if stops == 0.0 {
                    crate::pipeline::fit_range::IDENTITY
                } else {
                    crate::pipeline::display_tone::EXTENDED_REINHARD
                };
                assert_eq!(
                    block["display_tone"],
                    serde_json::json!({ "operator": operator, "headroom_stops": stops }),
                    "{preset:?} {stops}"
                );
                let content = block["content"].as_str().unwrap();
                assert!(
                    !content.contains("reinhard") && !content.contains("shoulder"),
                    "{preset:?}: content names a tone curve: {content}"
                );
            }
        }

        // The default claims its base-derived placement.
        assert!(
            content.contains("film-base-derived anchor placement"),
            "the default's placement must be claimed: {content}"
        );

        // A display preset: the shared print controls and a display render both run.
        let sdr = value(&ResolvedConfig {
            output: OutputParams {
                preset: OutputPreset::DisplayP3,
            },
            ..base_cfg()
        });
        assert_eq!(sdr["preset"], "display-p3");
        assert_eq!(sdr["print_controls"], true);
        assert_eq!(sdr["display_render"], true);
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
        // The subject is the exact document `hanten params` prints — `run_params`
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
        assert!(msg.contains("calibration.film_base"), "{msg}");

        // Falsifiable control: the same document with a base stated does validate,
        // so the rejection above is about the film base and nothing else.
        let mut runnable = back.clone();
        runnable.calibration.film_base = Some(FilmBaseSource::Auto);
        validate(&runnable).unwrap();
    }

    #[test]
    fn validate_requires_a_stated_film_base() {
        // `calibration.film_base` has no default: `Dmin` is the divisor of the density
        // conversion, so falling into auto-detection by omission decided the most
        // consequential parameter for the user. All three stated forms are fine —
        // including `auto`, which is what this used to default to. The rule is
        // that the choice is *made*, not that it is explicit.
        let unstated = ResolvedConfig::default();
        assert_eq!(
            unstated.calibration.film_base, None,
            "there must be no default"
        );
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
            "calibration.film_base",
        ] {
            assert!(msg.contains(expected), "{expected} missing from: {msg}");
        }

        for stated in [
            FilmBaseSource::Auto,
            FilmBaseSource::Region([0, 0, 100, 40]),
            FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
        ] {
            let mut cfg = ResolvedConfig::default();
            cfg.calibration.film_base = Some(stated.clone());
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
        assert!(msg.contains("calibration.film_base"), "{msg}");
        for absent in ["--auto-base", "--film-base"] {
            assert!(!msg.contains(absent), "{absent} must not be offered: {msg}");
        }
        // `--base-region` *does* appear — but only inside the `hanten estimate`
        // invocation the message recommends, which is a different command and does
        // accept it. What must never appear is a `roll` flag.
        assert!(
            msg.matches("--base-region")
                .count()
                .eq(&msg.matches("hanten estimate --base-region").count()),
            "--base-region may only appear as an argument of `hanten estimate`: {msg}"
        );
        // The *requirement* is remedy-independent — only the wording moves.
        let mut stated = ResolvedConfig::default();
        stated.calibration.film_base = Some(FilmBaseSource::Auto);
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
        assert_eq!(contradictory.calibration.film_base, None);
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
        only_unstated.calibration.film_base = Some(FilmBaseSource::Auto);
        validate(&only_unstated).unwrap();
    }

    #[test]
    fn every_v1_film_base_spelling_gets_the_migration_error() {
        // The inverse of the back-compat test this replaces. Every recipe ever written
        // spells the base one of these three ways under a top-level `film_base`, and all
        // three now get the pinned migration guidance rather than an opaque
        // `deny_unknown_fields` serde message. Project policy is a migration error, never
        // an alias (the `algorithm` precedent).
        for json in [
            r#"{"film_base":{"source":"auto"}}"#,
            r#"{"film_base":{"source":{"region":[10,20,30,40]}}}"#,
            r#"{"film_base":{"source":{"explicit":[0.9,0.55,0.42]}}}"#,
            // …including the empty section, which is still the old *shape*.
            r#"{"film_base":{}}"#,
        ] {
            let err = reject_legacy_recipe_keys(
                &serde_json::from_str::<serde_json::Value>(json).unwrap(),
                "recipe",
            )
            .unwrap_err();
            let msg = err.message();
            assert!(msg.contains("calibration"), "{json}: {msg}");
            assert!(msg.contains("film_base"), "{json}: {msg}");
            // The remedy must name the *flattened* spelling, or it sends the user to a
            // `{"source": …}` wrapper that no longer exists.
            assert!(
                msg.contains(r#""calibration": {"film_base": {"explicit": [r, g, b]}}"#),
                "{json}: {msg}"
            );
        }

        // Each of the three spellings is what the new path accepts, unchanged.
        for (json, want) in [
            (
                r#"{"calibration":{"film_base":"auto"}}"#,
                FilmBaseSource::Auto,
            ),
            (
                r#"{"calibration":{"film_base":{"region":[10,20,30,40]}}}"#,
                FilmBaseSource::Region([10, 20, 30, 40]),
            ),
            (
                r#"{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}}}"#,
                FilmBaseSource::Explicit([0.9, 0.55, 0.42]),
            ),
        ] {
            let cfg: ResolvedConfig = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("the new spelling must parse: {json}: {e}"));
            assert_eq!(cfg.calibration.film_base, Some(want), "{json}");
            validate(&cfg).unwrap_or_else(|e| panic!("{json} must validate: {e}"));
        }

        // Falsifiable control: omitting the key still yields `None`, which `validate`
        // still refuses — the defaultless rule survived the move untouched.
        let omitted: ResolvedConfig = serde_json::from_str(r#"{"calibration":{}}"#).unwrap();
        assert_eq!(omitted.calibration.film_base, None);
        assert!(validate(&omitted).is_err());
    }

    #[test]
    fn auto_base_flag_states_the_source_rather_than_relying_on_a_default() {
        // The flag is the migration path for anyone who *wanted* detection: it
        // resolves to exactly the source that used to be implicit.
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--auto-base"])).unwrap();
        assert_eq!(cfg.calibration.film_base, Some(FilmBaseSource::Auto));
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_bad_params() {
        // Exponential gamma must be positive.
        let cfg = exponential_cfg(ExponentialParams {
            gamma: 0.0,
            ..ExponentialParams::default()
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

        // Non-positive density scale is rejected.
        let cfg = density_cfg(
            DensityParams {
                scale: [1.0, 0.0, 1.0],
                ..DensityParams::default()
            },
            ExponentialParams::default(),
        );
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // A clean default passes.
        validate(&base_cfg()).unwrap();
    }

    #[test]
    fn validate_rejects_recipe_smuggled_bad_values() {
        // A recipe can carry values the CLI value-parsers would have rejected,
        // so validate is the only guard for these once they're in the config.
        let mut cfg = base_cfg();
        cfg.calibration.film_base = Some(FilmBaseSource::Explicit([0.9, 0.0, 0.4])); // zero transmission
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = base_cfg();
        cfg.calibration.film_base = Some(FilmBaseSource::Explicit([0.9, 90.0, 0.4])); // "90" typo for "0.90"
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        let mut cfg = base_cfg();
        cfg.calibration.film_base = Some(FilmBaseSource::Explicit([1.0, 1.0, 1.0])); // 1.0 exactly is valid
        validate(&cfg).unwrap();

        let mut cfg = base_cfg();
        cfg.calibration.film_base = Some(FilmBaseSource::Region([0, 0, 0, 0])); // zero-area region
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
        recipe.calibration.film_base = Some(FilmBaseSource::Explicit([0.9, 0.5, 0.4]));
        let cfg = merge(recipe.clone(), &parse_convert(&[])).unwrap();
        assert_eq!(
            cfg.calibration.film_base,
            Some(FilmBaseSource::Explicit([0.9, 0.5, 0.4]))
        );

        // A flag replaces the whole source — no field is left behind to win on
        // precedence (the #5/#6 fix). `--base-region` beats a recipe explicit base.
        let cfg = merge(recipe, &parse_convert(&["--base-region", "0,0,100,40"])).unwrap();
        assert_eq!(
            cfg.calibration.film_base,
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
                "hanten",
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
                "hanten",
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
            let mut argv = vec!["hanten", "estimate", "in.tiff"];
            argv.extend_from_slice(bad);
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{bad:?} should conflict"
            );
        }
        // `--grid` with `--base-region` is the documented sub-rectangle mode.
        let cli = Cli::try_parse_from([
            "hanten",
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
    fn the_calibration_fragment_round_trips_as_a_recipe() {
        // The report's `calibration` object must parse back as a whole recipe with no
        // hand editing — that is the workflow it exists for
        // (`hanten estimate … | jq '{calibration}' > roll-cal.json`; the pipe straight
        // into `--params -` is the target shape, not today's).
        let fragment = CalibrationFragment {
            film_base: Some(FilmBaseSource::Explicit([0.553, 0.271, 0.159])),
        };
        let json = serde_json::to_string(&fragment).unwrap();
        assert_eq!(json, r#"{"film_base":{"explicit":[0.553,0.271,0.159]}}"#);
        let recipe: ResolvedConfig =
            serde_json::from_str(&format!(r#"{{"calibration":{json}}}"#)).unwrap();
        assert_eq!(
            recipe.calibration.film_base,
            Some(FilmBaseSource::Explicit([0.553, 0.271, 0.159]))
        );
        validate(&recipe).unwrap();
    }

    /// **Each member is emitted only when it was measured, and never defaulted**, so a
    /// fragment piped into `--params` pins nothing a later layer states.
    #[test]
    fn the_calibration_fragment_omits_what_was_not_measured() {
        let base_only = CalibrationFragment {
            film_base: Some(FilmBaseSource::Auto),
        };
        assert_eq!(
            serde_json::to_string(&base_only).unwrap(),
            r#"{"film_base":"auto"}"#
        );
        // Nothing measured is `None`, not `{}`: an empty object would pipe into
        // `--params` as a no-op a user could mistake for a calibration.
        assert!(CalibrationFragment::default().is_empty());
        assert!(calibration_fragment(&Report::default()).is_none());
    }

    #[test]
    fn film_base_flag_string_round_trips_exact_f32s() {
        // `Display` for f32 prints the shortest decimal that parses back to the
        // same bits, so the emitted `--film-base` string reproduces the exact
        // measured base — including awkward values with no short decimal form.
        let rgb = [0.553_712_3_f32, 1.0 / 3.0, f32::MIN_POSITIVE];
        let (flag, source) = reuse_ready(rgb).expect("a valid base is reuse-ready");
        let value = flag.strip_prefix("--film-base ").unwrap();
        assert_eq!(parse_rgb(value).unwrap(), rgb);
        // The two forms carry the same value — never allowed to drift.
        assert_eq!(source, FilmBaseSource::Explicit(rgb));
    }

    #[test]
    fn report_reuse_flattens_to_flat_keys_or_nothing() {
        // The wire contract: the flag half serializes as the flat top-level key
        // `film_base_flag`, the recipe half rides inside `calibration`, and the
        // `ReuseReady` wrapper / `reuse` field name never leaks. `None` emits
        // neither. Locks the `#[serde(flatten)]` + rename shape so a refactor
        // can't silently change the agent-facing JSON.
        // Values exactly representable in f32 (halves/quarters/eighths) so the
        // JSON literals match without precision noise — the shape is the point.
        let reuse = ReuseReady {
            flag: "--film-base 0.5,0.25,0.125".to_string(),
            source: FilmBaseSource::Explicit([0.5, 0.25, 0.125]),
        };
        let with = Report {
            calibration: calibration_fragment(&Report {
                reuse: Some(reuse.clone()),
                ..Report::default()
            }),
            reuse: Some(reuse),
            ..Report::default()
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["film_base_flag"], "--film-base 0.5,0.25,0.125");
        assert_eq!(
            v["calibration"],
            serde_json::json!({ "film_base": { "explicit": [0.5, 0.25, 0.125] } })
        );
        // The old key is gone, not renamed in place: a consumer still reading it must
        // fail loudly rather than silently see nothing.
        assert!(v.get("film_base_recipe").is_none());
        assert!(v.get("reuse").is_none(), "the wrapper name must not leak");

        let without = Report::default();
        let v = serde_json::to_value(&without).unwrap();
        assert!(v.get("film_base_flag").is_none());
        assert!(v.get("calibration").is_none());
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
        let (flag, source) = reuse_ready([0.553, 0.271, 0.159]).unwrap();
        assert_eq!(flag, "--film-base 0.553,0.271,0.159");
        assert_eq!(source, FilmBaseSource::Explicit([0.553, 0.271, 0.159]));
    }

    #[test]
    fn load_recipe_maps_failures_to_usage() {
        // No path → defaults, infallibly.
        let loaded = load_recipe(None).unwrap();
        assert_eq!(*loaded.cfg(), ResolvedConfig::default());

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

        // A valid partial recipe loads and fills defaults.
        let p = std::env::temp_dir().join(format!("nc-recipe-ok-{}.json", std::process::id()));
        std::fs::write(
            &p,
            r#"{"reconstruction":{"curve":{"type":"exponential","gamma":1.8}}}"#,
        )
        .unwrap();
        let got = load_recipe(Some(&p)).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(gamma_of(got.cfg()), 1.8);
        assert_eq!(got.cfg().print, PrintParams::default());
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
        assert_eq!(
            flat.cfg(),
            wrapped.cfg(),
            "both shapes resolve to one config"
        );
        assert_eq!(gamma_of(wrapped.cfg()), 1.8);
        // A bare recipe records no provenance; the envelope's is read but never
        // applied — only compared (see `pipeline_version_warning`).
        assert_eq!(flat.meta_pipeline_version, None);
        assert_eq!(wrapped.meta_pipeline_version, Some(7));
        // The retired `calibration.dmax` is handled on the recipe *body* either way:
        // dropped at its old default, refused otherwise.
        for body in [
            r#"{"calibration":{"film_base":"auto","dmax":"fixed"}}"#,
            r#"{"meta":{},"params":{"calibration":{"film_base":"auto","dmax":"fixed"}}}"#,
        ] {
            let got = load_recipe_body("env-dmax-fixed", body).unwrap();
            assert_eq!(got.cfg().calibration.film_base, Some(FilmBaseSource::Auto));
        }
        for body in [
            r#"{"calibration":{"dmax":{"explicit":1.6}}}"#,
            r#"{"meta":{},"params":{"calibration":{"dmax":{"explicit":1.6}}}}"#,
        ] {
            let err = load_recipe_body("env-dmax", body).unwrap_err();
            assert!(
                matches!(&err, NcError::Usage(m) if m.contains("calibration.dmax")),
                "{body}: {err}"
            );
        }
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
        // nothing leaves `calibration.film_base` unset (`None`), which `validate`
        // later rejects for `convert`. "All defaults" is not the same as "ready
        // to run" any more, and that is the point of the requirement.
        assert_eq!(
            *load_recipe_body("obj-bare", "{}").unwrap().cfg(),
            ResolvedConfig::default()
        );
        assert_eq!(
            *load_recipe_body("obj-envelope", r#"{"params": {}}"#)
                .unwrap()
                .cfg(),
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
        assert!(Cli::try_parse_from(["hanten", "roll", "-o", "out"]).is_err());
        // Both → mutually exclusive.
        assert!(
            Cli::try_parse_from(["hanten", "roll", "a.tif", "--frames", "m.json", "-o", "out"])
                .is_err()
        );
        // Either alone (with --out-dir) is fine.
        assert!(Cli::try_parse_from(["hanten", "roll", "a.tif", "b.tif", "-o", "out"]).is_ok());
        assert!(Cli::try_parse_from(["hanten", "roll", "--frames", "m.json", "-o", "out"]).is_ok());
        // --out-dir is required.
        assert!(Cli::try_parse_from(["hanten", "roll", "a.tif"]).is_err());
    }

    /// A bare tag over a newtype variant **replaces** it, so serde rejects the incomplete
    /// override. Every externally-tagged recipe variant is a newtype carrying a positional
    /// payload, where a bare tag states nothing *and there is nothing it could state*. A
    /// guard that once kept the base on a matching tag (for the since-retired
    /// `print.display_tone` struct variant) silently turned a malformed per-frame
    /// `{"film_base": {"source": "explicit"}}` into an inherit at exit 0.
    #[test]
    fn a_bare_tag_overlay_replaces_a_newtype_variant() {
        for base in [
            serde_json::json!({"explicit": [0.9, 0.55, 0.42]}), // FilmBaseSource / WbSource
            serde_json::json!({"region": [1, 2, 3, 4]}),        // FilmBaseSource::Region
        ] {
            let tag = base.as_object().unwrap().keys().next().unwrap().clone();
            let mut merged = base.clone();
            merge_json(&mut merged, &serde_json::json!(tag.clone()));
            assert_eq!(
                merged,
                serde_json::json!(tag),
                "a bare tag over the newtype variant {base} must replace it, so serde still \
                 rejects the incomplete override rather than silently inheriting"
            );
        }
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
        // for the per-frame `calibration.film_base` override path.
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
    fn per_frame_override_accepts_the_retired_tags_and_refuses_characteristic() {
        // Through `resolve_frames`: a per-frame override restating either retired `type`
        // at its surviving value applies and changes nothing — the overlay merges the tag
        // into the serialized shared config, and the deserializer drops it — while one
        // naming the retired `characteristic` curve is refused with its migration error.
        let dir = std::env::temp_dir().join(format!("nc-roll-typeswitch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("frames.json");
        let args = RollArgs {
            inputs: vec![],
            frames: Some(manifest.clone()),
            out_dir: dir.clone(),
            recipe_in: None,
            strict: false,
            new_flow: false,
            memory: MemoryArgs::default(),
            report: ReportArgs::default(),
        };
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.8,
            anchor: AnchorPlacement::MidAtBaseOffset(0.5),
        });
        shared.calibration.film_base = Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]));
        let log = Log::new(&args.report);
        let plan = |frames: &str| {
            std::fs::write(&manifest, frames).unwrap();
            resolve_frames(&args, &shared, None, &mut Vec::new(), &log)
        };

        let planned = plan(
            r#"{"frames":[
                 {"input":"a.tif","params":{"reconstruction":{"type":"density"}}},
                 {"input":"b.tif","params":{"reconstruction":{"curve":{"type":"exponential"}}}}
               ]}"#,
        )
        .expect("the retired tags at their surviving values must apply, not error");
        assert_eq!(planned.len(), 2);
        for frame in &planned {
            assert_eq!(frame.cfg.reconstruction, shared.reconstruction);
        }

        let refused = plan(
            r#"{"frames":[{"input":"b.tif",
                 "params":{"reconstruction":{"curve":{"type":"characteristic","stock":"ektar-100"}}}}]}"#,
        );
        std::fs::remove_dir_all(&dir).ok();
        let err = refused.expect_err("a characteristic override must be refused");
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .contains(crate::types::REMOVED_CHARACTERISTIC_CURVE),
            "{err}"
        );
    }

    #[test]
    fn per_frame_override_can_switch_film_base_variant_and_still_warns() {
        // A per-frame `params` override that flips the roll-fixed `calibration.film_base`
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
                          "params":{"calibration":{"film_base":{"explicit":[0.9,0.55,0.42]}}}}]}"#,
        )
        .unwrap();
        let args = RollArgs {
            inputs: vec![],
            frames: Some(manifest.clone()),
            out_dir: dir.clone(),
            recipe_in: None,
            strict: false,
            new_flow: false,
            memory: MemoryArgs::default(),
            report: ReportArgs::default(),
        };
        let shared = ResolvedConfig {
            calibration: CalibrationParams {
                film_base: Some(FilmBaseSource::Region([10, 10, 20, 20])),
            },
            ..base_cfg()
        };
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let planned = resolve_frames(&args, &shared, None, &mut warnings, &log);
        std::fs::remove_dir_all(&dir).ok();
        let planned = planned.expect("region→explicit override should apply, not error");
        assert_eq!(planned.len(), 1);
        assert_eq!(
            planned[0].cfg.calibration.film_base,
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
        // own knob and keeps the shared roll-fixed params (the film base) — the
        // "frame-local override applies to just that frame" guarantee at the
        // config level. Mirrors `resolve_frames`' merge.
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            anchor: AnchorPlacement::MidAtBaseOffset(0.5),
        });
        shared.calibration.film_base = Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]));
        let mut v = serde_json::to_value(&shared).unwrap();
        let ov: serde_json::Value =
            serde_json::from_str(r#"{"print":{"print_exposure":0.15}}"#).unwrap();
        merge_json(&mut v, &ov);
        let cfg: ResolvedConfig = serde_json::from_value(v).unwrap();
        assert_eq!(cfg.print.print_exposure, 0.15);
        assert_eq!(
            cfg.calibration.film_base,
            Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]))
        );
        assert_eq!(cfg.reconstruction, shared.reconstruction);
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
            new_flow: false,
            memory: MemoryArgs::default(),
            report: ReportArgs::default(),
        };
        let mut warnings = Vec::new();
        let log = Log::new(&args.report);
        let got = resolve_frames(&args, &base_cfg(), None, &mut warnings, &log);
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
            default_output_name(
                Path::new("/scans/frame01.tif"),
                Path::new("/out"),
                OutputPreset::DisplayP3,
            ),
            PathBuf::from("/out/frame01_positive.tiff")
        );
        // A manifest output: relative joins the out-dir, absolute is used verbatim,
        // and `None` falls back to the derived name.
        assert_eq!(
            resolve_frame_output(
                Some(Path::new("custom.tiff")),
                Path::new("/s/f.tif"),
                Path::new("/out"),
                OutputPreset::DisplayP3,
                Flow::Legacy,
            )
            .unwrap(),
            PathBuf::from("/out/custom.tiff")
        );
        assert_eq!(
            resolve_frame_output(
                Some(Path::new("/abs/c.tiff")),
                Path::new("/s/f.tif"),
                Path::new("/out"),
                OutputPreset::DisplayP3,
                Flow::Legacy,
            )
            .unwrap(),
            PathBuf::from("/abs/c.tiff")
        );
        assert_eq!(
            resolve_frame_output(
                None,
                Path::new("/s/f.tif"),
                Path::new("/out"),
                OutputPreset::DisplayP3,
                Flow::Legacy,
            )
            .unwrap(),
            PathBuf::from("/out/f_positive.tiff")
        );
    }

    #[test]
    fn roll_derives_every_container_suffix_and_checks_only_explicit_paths() {
        // The derived name follows the preset's container, which is the capability
        // the blanket convert-only roll refusal was waiting on. One case per
        // container rather than per preset — the mapping itself is
        // `derived_extension`'s job, and this asserts roll consumes it.
        for (preset, want) in [
            (OutputPreset::FilmMaster, "f_positive.tiff"),
            (OutputPreset::GainMapHdr, "f_positive.jpg"),
            (OutputPreset::UltraHdrV1, "f_positive.jpg"),
            (OutputPreset::HdrPq, "f_positive.avif"),
            (OutputPreset::DisplayP3, "f_positive.tiff"),
        ] {
            assert_eq!(
                default_output_name(Path::new("/s/f.tif"), Path::new("/out"), preset),
                PathBuf::from(format!("/out/{want}")),
                "{}",
                preset.name()
            );
        }
        // The invariant that lets roll skip re-checking a derived name: every
        // preset's derived spelling is one the suffix rule accepts, and passing it
        // back through the resolver returns it **unchanged** rather than completing
        // it a second time. Looped over `ALL`, so a new preset cannot pick an
        // extension its own table rejects.
        for preset in OutputPreset::ALL {
            let derived = default_output_name(Path::new("/s/f.tif"), Path::new("/out"), preset);
            assert_eq!(
                resolve_output_path(
                    &derived,
                    preset,
                    SuffixContext::RollFrame(Path::new("/s/f.tif"))
                )
                .unwrap_or_else(|e| panic!("{} derives a name it rejects: {e}", preset.name())),
                derived,
                "{} derives {} and the resolver then changed it",
                preset.name(),
                derived.display()
            );
        }
        // The pre-container roll name is unchanged for TIFF presets: `tiff`, not the
        // `tif` that heads the accepted list.
        assert_eq!(derived_extension(OutputPreset::DisplayP3), "tiff");
        // An **explicit** manifest path is checked, and the diagnosis names the frame
        // and the way out rather than just the rule.
        let err = resolve_frame_output(
            Some(Path::new("frame.tiff")),
            Path::new("/s/f.tif"),
            Path::new("/out"),
            OutputPreset::GainMapHdr,
            Flow::Legacy,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("frame /s/f.tif"), "{err}");
        assert!(err.contains(".jpg"), "{err}");
        assert!(err.contains("gain-map-hdr"), "{err}");
        // An explicit manifest path shares the whole rule, so one stating no suffix
        // is *completed* like a `convert` path rather than refused — and the
        // relative-to-out-dir join still happens first.
        for (preset, want) in [
            (OutputPreset::GainMapHdr, "/out/chosen.jpg"),
            (OutputPreset::DisplayP3, "/out/chosen.tiff"),
        ] {
            assert_eq!(
                resolve_frame_output(
                    Some(Path::new("chosen")),
                    Path::new("/s/f.tif"),
                    Path::new("/out"),
                    preset,
                    Flow::Legacy,
                )
                .unwrap(),
                PathBuf::from(want),
                "{}",
                preset.name()
            );
        }
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
        // additionally echoes the *resolved* base it used (a redundant echo
        // here since the recipe pins an explicit base). The per-frame entry is
        // the data-carrying `FrameStatus` — an "ok" frame serializes the flat
        // `"status":"ok"` with its payload as sibling keys.
        let mut shared = exponential_cfg(ExponentialParams {
            gamma: 1.0,
            anchor: AnchorPlacement::MidAtBaseOffset(0.5),
        });
        shared.calibration.film_base = Some(FilmBaseSource::Explicit([0.9, 0.55, 0.42]));
        let roll = RollReport {
            command: "roll",
            identity: Identity::with_params_hash(version::stable_hash("{}")),
            recipe: Some(shared),
            warnings: vec![],
            frames: vec![FrameReport {
                input: PathBuf::from("f1.tif"),
                output: Some(PathBuf::from("out/f1_positive.tiff")),
                status: FrameStatus::Ok {
                    film_base: Some(FilmBase::from([0.9, 0.55, 0.42])),
                    white_balance: None,
                    effective_area: None,
                    input_color: None,
                    loss: None,
                    output_stats: Some(OutputStats {
                        mean: [0.25, 0.5, 0.75],
                    }),
                    identity: Some(Box::new(Identity::with_params_hash(version::stable_hash(
                        "frame",
                    )))),
                    new_flow: None,
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
        let fb: Vec<f64> = v["recipe"]["calibration"]["film_base"]["explicit"]
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
        assert!(v["recipe"]["calibration"].get("dmax").is_none());
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
        assert!(v["frames"][0].get("dmax").is_none());
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
            recipe: None,
            overrides: None,
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
