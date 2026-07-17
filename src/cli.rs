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

use crate::io::decode::{DecodeInfo, decode};
use crate::io::encode;
use crate::pipeline::{film_base, stages};
use crate::telemetry;
use crate::types::{
    Algorithm, BigTiff, DensityParams, DmaxSource, EncodeReport, FilmBase, FilmBaseParams,
    FilmBaseSource, InputColor, InputParams, NcError, OutputParams, PrintParams, Result,
    SigmoidParams, SimpleParams,
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
/// calibrate-once-from-a-reference workflow works, design-spec §8), and
/// reporting controls.
#[derive(Args, Debug)]
pub struct EstimateArgs {
    /// Input negative scan (SilverFast HDR/HDRi TIFF).
    pub input: PathBuf,
    #[command(flatten)]
    pub film_base: FilmBaseOverrides,
    /// Treat estimation warnings (e.g. a non-uniform `--base-region`) as a hard
    /// error. `estimate` produces the `Dmin` a roll is calibrated on, so a
    /// script baking the result into a recipe wants a plausible-looking-but-bad
    /// base to fail loudly rather than be echoed back.
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
    /// Conversion algorithm (default `density`).
    #[arg(long, value_enum)]
    pub algorithm: Option<Algorithm>,

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

// --- per-stage override groups (all-Option; presence flags for booleans) ----

/// Input / decode overrides (design-spec §9, stage 1).
///
/// `--assume-linear` and `--input-profile` are the two non-default `input.color`
/// choices and are mutually exclusive (clap rejects passing both); whichever is
/// given replaces the recipe's color choice.
#[derive(Args, Debug, Default)]
pub struct InputOverrides {
    /// Treat scanner data as already linear (skip input-profile handling).
    #[arg(long, conflicts_with = "input_profile")]
    pub assume_linear: bool,
    /// Input ICC profile selector / path.
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

/// Density-stage overrides (design-spec §9, `algorithm = density`).
#[derive(Args, Debug, Default)]
pub struct DensityOverrides {
    /// Per-channel density gain.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub density_scale: Option<[f32; 3]>,
    /// Per-channel density offset (orange-mask compensation).
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub density_offset: Option<[f32; 3]>,
    /// Film / print curve gamma.
    #[arg(long)]
    pub density_gamma: Option<f32>,
}

/// Display-white anchor (`Dmax`) overrides (design-spec §9, `density.dmax`).
///
/// One mutually-exclusive choice, like [`FilmBaseOverrides`]: the three flags
/// conflict (clap rejects passing more than one) and whichever is given replaces
/// the recipe's `density.dmax` entirely.
#[derive(Args, Debug, Default)]
pub struct DmaxOverrides {
    /// Explicit display-white anchor density (`Dmax`); a scalar, applied to all
    /// channels. Reuses one frame's value across a roll for a fixed-print look.
    #[arg(long = "d-max", value_name = "D",
          conflicts_with_all = ["auto_d_max", "no_d_max"])]
    pub d_max: Option<f32>,
    /// Measure the anchor per frame (the default behavior).
    #[arg(long = "auto-d-max", conflicts_with = "no_d_max")]
    pub auto_d_max: bool,
    /// Disable the anchor — scene-referred output (base → 1.0, detail above).
    #[arg(long = "no-d-max")]
    pub no_d_max: bool,
}

/// Sigmoid-algorithm overrides (design-spec §7.3/§9, `algorithm = sigmoid`).
/// The flags are `--sigmoid-*`-prefixed for namespacing; the recipe keys drop
/// the prefix (`sigmoid.contrast` etc.), like `--d-max` ⇒ `density.dmax`.
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

/// Print / tone-render overrides (design-spec §9).
#[derive(Args, Debug, Default)]
pub struct PrintOverrides {
    /// Overall positive exposure.
    #[arg(long)]
    pub print_exposure: Option<f32>,
    /// Paper black / shadow floor.
    #[arg(long)]
    pub black_point: Option<f32>,
    /// Highlight / neutral white-balance gains.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub white_balance: Option<[f32; 3]>,
    /// Highlight roll-off amount.
    #[arg(long)]
    pub highlight_compress: Option<f32>,
}

/// Simple-algorithm overrides (design-spec §9, `algorithm = simple`).
#[derive(Args, Debug, Default)]
pub struct SimpleOverrides {
    /// White-balance gains applied to the inverted result.
    #[arg(long, value_name = "R,G,B", value_parser = parse_rgb)]
    pub invert_white_balance: Option<[f32; 3]>,
    /// Low clip point.
    #[arg(long)]
    pub clip_low: Option<f32>,
    /// High clip point.
    #[arg(long)]
    pub clip_high: Option<f32>,
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
    /// Output ICC profile (`sRGB` / `prophoto` / `acescg` / path).
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
/// typo'd keys at every level — `serde(flatten)` would defeat that.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ResolvedConfig {
    pub algorithm: Algorithm,
    pub input: InputParams,
    pub film_base: FilmBaseParams,
    pub density: DensityParams,
    pub sigmoid: SigmoidParams,
    pub print: PrintParams,
    pub simple: SimpleParams,
    pub output: OutputParams,
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

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
    /// Algorithm that ran (`convert`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<Algorithm>,
    /// What the decoder found (`inspect`): format, dimensions, channels, bit
    /// depth, IR presence, scanner metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode: Option<DecodeInfo>,
    /// Estimated / resolved film base (the `Dmin` anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_base: Option<FilmBase>,
    /// Resolved display-white anchor density (`Dmax`) the density render used
    /// (`convert`): the auto-measured or explicit value, absent for
    /// `dmax = none` or the `simple` algorithm. Reported so a roll can reuse a
    /// frame's anchor deliberately (`--d-max`) — a batch-consistency choice,
    /// not calibrate-once (design-spec §9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dmax: Option<f32>,
    /// How the film base was chosen, as the structured [`FilmBaseSource`]
    /// (`"auto"` / `{"region":[…]}` / `{"explicit":[…]}`) so an agent gets the
    /// sampled rectangle / explicit values without string-parsing a label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_base_source: Option<FilmBaseSource>,
    /// Candidate unexposed-rebate bands from the inward-scan detector
    /// (`inspect` only): edge, a rectangle usable verbatim as `--base-region`,
    /// the proposed base, and the measured spread (lower = more uniform). Lets
    /// a user confirm a region instead of measuring one in an image viewer —
    /// and a future UI draws its highlight rectangles from the same data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_candidates: Option<Vec<film_base::RebateCandidate>>,
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

/// Load a recipe file into a [`ResolvedConfig`], or the defaults when no recipe
/// is given. A read failure or invalid/unknown-key JSON is a usage error.
fn load_recipe(path: Option<&Path>) -> Result<ResolvedConfig> {
    match path {
        None => Ok(ResolvedConfig::default()),
        Some(p) => {
            let txt = std::fs::read_to_string(p)
                .map_err(|e| NcError::Usage(format!("cannot read recipe {}: {e}", p.display())))?;
            serde_json::from_str(&txt)
                .map_err(|e| NcError::Usage(format!("invalid recipe {}: {e}", p.display())))
        }
    }
}

/// Apply CLI overrides on top of a (recipe or default) config; flags win.
///
/// Pure and total: `Option` overrides replace when `Some`, presence-flag
/// booleans (`--auto-base`, `--assume-linear`) replace only when set — a `false`
/// flag never clobbers a recipe `true`, since you disable auto-base by supplying
/// an explicit base, not by passing `false`.
pub fn merge(mut cfg: ResolvedConfig, args: &ConvertArgs) -> ResolvedConfig {
    if let Some(a) = args.algorithm {
        cfg.algorithm = a;
    }

    // input color: `--assume-linear` / `--input-profile` are mutually exclusive
    // (clap-enforced); whichever is given replaces the recipe's choice, and
    // neither leaves it untouched. So `--input-profile` over a recipe `linear`
    // wins cleanly — there's one field, not two booleans to disagree.
    if args.input_opts.assume_linear {
        cfg.input.color = InputColor::Linear;
    } else if let Some(p) = &args.input_opts.input_profile {
        cfg.input.color = InputColor::Profile(p.clone());
    }
    if let Some(p) = &args.input_opts.export_ir {
        cfg.input.export_ir = Some(p.clone());
    }

    // film base: the four source flags are mutually exclusive (clap-enforced);
    // whichever is given replaces the recipe's source entirely.
    if let Some(src) = film_base_source_override(&args.film_base) {
        cfg.film_base.source = src;
    }

    // density
    if let Some(v) = args.density.density_scale {
        cfg.density.density_scale = v;
    }
    if let Some(v) = args.density.density_offset {
        cfg.density.density_offset = v;
    }
    if let Some(v) = args.density.density_gamma {
        cfg.density.density_gamma = v;
    }

    // dmax anchor: the three flags are mutually exclusive (clap-enforced);
    // whichever is given replaces the recipe's `density.dmax` entirely.
    if let Some(v) = args.dmax.d_max {
        cfg.density.dmax = DmaxSource::Explicit(v);
    } else if args.dmax.auto_d_max {
        cfg.density.dmax = DmaxSource::Auto;
    } else if args.dmax.no_d_max {
        cfg.density.dmax = DmaxSource::None;
    }

    // sigmoid
    if let Some(v) = args.sigmoid.sigmoid_contrast {
        cfg.sigmoid.contrast = v;
    }
    if let Some(v) = args.sigmoid.sigmoid_toe {
        cfg.sigmoid.toe = v;
    }
    if let Some(v) = args.sigmoid.sigmoid_shoulder {
        cfg.sigmoid.shoulder = v;
    }

    // print
    if let Some(v) = args.print.print_exposure {
        cfg.print.print_exposure = v;
    }
    if let Some(v) = args.print.black_point {
        cfg.print.black_point = v;
    }
    if let Some(v) = args.print.white_balance {
        cfg.print.white_balance = v;
    }
    if let Some(v) = args.print.highlight_compress {
        cfg.print.highlight_compress = v;
    }

    // simple
    if let Some(v) = args.simple.invert_white_balance {
        cfg.simple.invert_white_balance = v;
    }
    if let Some(v) = args.simple.clip_low {
        cfg.simple.clip_low = v;
    }
    if let Some(v) = args.simple.clip_high {
        cfg.simple.clip_high = v;
    }

    // output: `--output-hdr` is a presence flag — passing it switches the output
    // to 32-bit float; when absent it must not clobber a recipe's `hdr: true`
    // (same convention as `--assume-linear`), so only a set flag merges.
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

    cfg
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

    // Density: gamma and per-channel gain must be positive; offset just finite.
    positive("--density-gamma", &[cfg.density.density_gamma])?;
    positive("--density-scale", &cfg.density.density_scale)?;
    finite("--density-offset", &cfg.density.density_offset)?;

    // Dmax anchor: an explicit anchor is a corrected density — scene white sits at
    // a positive density above the base's `D = 0`, so a non-positive / non-finite
    // value (e.g. a sign typo) would brighten past white or blow out. Reject it
    // loudly; `Auto`/`None` need no value check.
    if let DmaxSource::Explicit(d) = cfg.density.dmax {
        positive("--d-max", &[d])?;
    }

    // Sigmoid: the S-curve is anchored on `[0, Dmax]` — both its white knee and
    // its black floor derive from the anchor — so `dmax = none` (scene-referred,
    // no anchor) cannot drive it (design-spec §7.3).
    if cfg.algorithm == Algorithm::Sigmoid && cfg.density.dmax == DmaxSource::None {
        return Err(usage(
            "--algorithm sigmoid needs a display-white anchor (--auto-d-max or \
             --d-max <d>); --no-d-max / `density.dmax = none` is only supported \
             by --algorithm density"
                .into(),
        ));
    }
    // Contrast (mid-density slope) must be positive AND bounded above: an extreme
    // slope collapses the S-curve into a hard black/white threshold whose knees
    // silently launder the blow-out into a finite two-level image (highlights →
    // exactly 1.0, shadows → the floor) that trips *neither* the clip nor the
    // non-finite counter — a silent destruction the `density` algorithm avoids
    // (it overflows to +inf, which is counted). Cap it; use `--algorithm density`
    // for genuinely extreme contrast.
    positive("--sigmoid-contrast", &[cfg.sigmoid.contrast])?;
    if cfg.sigmoid.contrast > crate::algo::sigmoid::SIGMOID_CONTRAST_MAX {
        return Err(usage(format!(
            "--sigmoid-contrast ({}) must be <= {} (beyond this the S-curve is a hard \
             threshold that silently destroys tonal detail; use --algorithm density)",
            cfg.sigmoid.contrast,
            crate::algo::sigmoid::SIGMOID_CONTRAST_MAX
        )));
    }
    // Knee widths non-negative AND bounded above: 0 disables a knee, a negative
    // width would silently be treated as "off" by the curve, and a huge *finite*
    // width flattens the image into near-uniform tone (giant shoulder → all-black,
    // giant toe → all-white) with samples that stay finite and in range — the same
    // silent-destruction class the contrast cap closes. Reject both loudly.
    finite(
        "--sigmoid-toe/--sigmoid-shoulder",
        &[cfg.sigmoid.toe, cfg.sigmoid.shoulder],
    )?;
    let knee_max = crate::algo::sigmoid::SIGMOID_KNEE_MAX;
    if cfg.sigmoid.toe < 0.0
        || cfg.sigmoid.shoulder < 0.0
        || cfg.sigmoid.toe > knee_max
        || cfg.sigmoid.shoulder > knee_max
    {
        return Err(usage(format!(
            "--sigmoid-toe ({}) and --sigmoid-shoulder ({}) must be in [0, {knee_max}] \
             (0 disables the knee; a larger width flattens the image into near-uniform \
             tone without tripping the clip/non-finite counters)",
            cfg.sigmoid.toe, cfg.sigmoid.shoulder
        )));
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
    positive("--white-balance", &cfg.print.white_balance)?;

    // Simple: gains positive; clip range finite and ordered.
    positive("--invert-white-balance", &cfg.simple.invert_white_balance)?;
    finite(
        "--clip-low/--clip-high",
        &[cfg.simple.clip_low, cfg.simple.clip_high],
    )?;
    // Equal endpoints leave a zero-width interval the simple remap can't
    // normalize without dividing by zero, so require strictly low < high.
    if cfg.simple.clip_low >= cfg.simple.clip_high {
        return Err(usage(format!(
            "--clip-low ({}) must be < --clip-high ({})",
            cfg.simple.clip_low, cfg.simple.clip_high
        )));
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
    if format == ReportFormat::None {
        return Ok(());
    }
    match file {
        Some(p) => write_json(p, report),
        None => {
            let json = serde_json::to_string_pretty(report)
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

/// `nc convert` — the full pipeline: decode → film-base → algorithm → output
/// color transform → encode (+ sidecar, + optional IR export). Warnings are
/// collected into the report and echoed to stderr; `--strict` promotes any of
/// them to a non-zero exit.
fn run_convert(args: ConvertArgs) -> Result<()> {
    let started = Instant::now();
    let log = Log::new(&args.report);

    let cfg = merge(load_recipe(args.recipe_in.as_deref())?, &args);
    validate(&cfg)?;

    // `input.color` profiles are parsed into the recipe shape, but input-side
    // color management is not implemented yet — reject loudly rather than
    // silently ignoring a documented knob (scans are decoded as linear).
    if let InputColor::Profile(p) = &cfg.input.color {
        return Err(NcError::Unsupported(format!(
            "--input-profile {p}: input-side color management is not implemented              yet; SilverFast scans are decoded as linear (omit the flag or use              --assume-linear)"
        )));
    }

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

    let mut report = Report {
        command: Some("convert"),
        input: Some(args.input.clone()),
        output: Some(args.output.clone()),
        algorithm: Some(cfg.algorithm),
        film_base_source: Some(cfg.film_base.source.clone()),
        ..Report::default()
    };

    // `sigmoid` consumes the density section's scale/offset/dmax but replaces the
    // stage-3 straight line that `density_gamma` parameterizes, so a customized
    // gamma would be a silent no-op — the four-spot trap in disguise. Unlike a
    // fully inert section (e.g. `simple.*` under `--algorithm density`), this is a
    // *partially* consumed section, so warn loudly instead of staying silent.
    if cfg.algorithm == Algorithm::Sigmoid
        && cfg.density.density_gamma != DensityParams::default().density_gamma
    {
        push_warning(
            &mut report,
            &log,
            format!(
                "--algorithm sigmoid ignores --density-gamma (got {}); the S-curve's \
                 mid-density slope is --sigmoid-contrast",
                cfg.density.density_gamma
            ),
        );
    }

    // Stage 1 — decode. Per-stage wall clocks feed the telemetry record only
    // (they never touch the image/sidecar); measure them regardless of whether
    // telemetry is enabled so the render path is uniform.
    let stage_started = Instant::now();
    let (image, info) = decode(&args.input)?;
    let decode_ms = elapsed_ms(stage_started);
    log.info(format_args!(
        "decoded {:?} {}x{} (ir={})",
        info.format, info.width, info.height, info.ir_present
    ));
    for w in &info.warnings {
        push_warning(&mut report, &log, w.clone());
    }

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
        push_warning(
            &mut report,
            &log,
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
        push_warning(&mut report, &log, w);
    }

    // Clear any stale lcms2 flag so only errors from *this* render are counted.
    let _ = cms_error_occurred();
    // Stages 3–4 — algorithm → output color transform.
    let rendered = stages::render(
        &image,
        &base.base,
        stages::algo_params(
            cfg.algorithm,
            &cfg.simple,
            &cfg.density,
            &cfg.sigmoid,
            &cfg.print,
        ),
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

    // Report an `auto` BigTIFF promotion (an automatic decision the user didn't
    // explicitly request).
    if cfg.output.bigtiff == BigTiff::Auto
        && encode::plans_bigtiff(&cfg.output, &rendered.image, rendered.icc.len())
    {
        push_warning(
            &mut report,
            &log,
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
    let loss = encode::encode(
        &rendered.image,
        &cfg.output,
        Some(&rendered.icc),
        &args.output,
    )?;
    let encode_ms = elapsed_ms(stage_started);
    report.loss = Some(loss);
    if loss.any_loss() {
        push_warning(
            &mut report,
            &log,
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

    let recipe_json = serde_json::to_string_pretty(&cfg)
        .map_err(|e| NcError::Other(format!("serializing recipe for sidecar: {e}")))?;
    encode::write_sidecar(&args.output, &recipe_json)?;
    log.info(format_args!("wrote {}", args.output.display()));

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
        let timings = telemetry::TimingInfo {
            total: total_ms,
            decode: decode_ms,
            film_base: film_base_ms,
            algorithm: rendered.timings.algorithm_ms,
            color: rendered.timings.color_ms,
            encode: encode_ms,
            ir_export: ir_export_ms,
        };
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

/// `nc estimate` — run only film-base / `Dmin` estimation from the selected
/// source (default `auto`, or `--base-region`/`--film-base`) and emit the
/// resolved [`FilmBase`] as JSON. Auto detection may fail loudly on real scans;
/// that propagates as an error (the user asked for an estimate we can't give).
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

    let est = film_base::estimate(
        &image,
        &FilmBaseParams {
            source: source.clone(),
        },
    )?;

    let mut report = Report {
        command: Some("estimate"),
        input: Some(args.input.clone()),
        film_base: Some(est.base),
        film_base_source: Some(source),
        ..Report::default()
    };
    for w in &info.warnings {
        push_warning(&mut report, &log, w.clone());
    }
    for w in est.warnings {
        push_warning(&mut report, &log, w);
    }
    report.elapsed_ms = Some(elapsed_ms(started));
    // Emit the report before the `--strict` gate so the machine-readable record
    // (the measured base) lands even when a warning then fails the run.
    emit_report(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
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
        algorithm: cfg.algorithm,
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
        let recipe: ResolvedConfig =
            serde_json::from_str(r#"{"density":{"density_gamma":2.0}}"#).unwrap();

        // recipe value, no flag → recipe kept
        let cfg = merge(recipe.clone(), &parse_convert(&[]));
        assert_eq!(cfg.density.density_gamma, 2.0);

        // matching flag → flag wins
        let cfg = merge(recipe, &parse_convert(&["--density-gamma", "1.5"]));
        assert_eq!(cfg.density.density_gamma, 1.5);

        // unspecified everywhere → default
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&[]));
        assert_eq!(cfg.density.density_gamma, 1.0);
    }

    #[test]
    fn merge_handles_algorithm_and_array_flags() {
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--algorithm", "simple", "--white-balance", "1.1,1.0,0.9"]),
        );
        assert_eq!(cfg.algorithm, Algorithm::Simple);
        assert_eq!(cfg.print.white_balance, [1.1, 1.0, 0.9]);
    }

    #[test]
    fn merge_dmax_flags_map_to_the_source_enum() {
        // Each flag maps to its variant; a forgotten merge arm would leave the
        // default and silently make the flag a no-op (the four-spot-wiring trap).
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--d-max", "1.75"]),
        );
        assert_eq!(cfg.density.dmax, DmaxSource::Explicit(1.75));
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--no-d-max"]));
        assert_eq!(cfg.density.dmax, DmaxSource::None);
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--auto-d-max"]));
        assert_eq!(cfg.density.dmax, DmaxSource::Auto);

        // No flag keeps the recipe's choice; a flag replaces it (flags win).
        let mut recipe = ResolvedConfig::default();
        recipe.density.dmax = DmaxSource::Explicit(2.0);
        assert_eq!(
            merge(recipe.clone(), &parse_convert(&[])).density.dmax,
            DmaxSource::Explicit(2.0)
        );
        assert_eq!(
            merge(recipe, &parse_convert(&["--no-d-max"])).density.dmax,
            DmaxSource::None
        );
    }

    #[test]
    fn merge_sigmoid_flags_override_recipe_else_keep_recipe() {
        // Each flag maps to its field; a forgotten merge arm would leave the
        // default and silently make the flag a no-op (the four-spot-wiring trap).
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&[
                "--algorithm",
                "sigmoid",
                "--sigmoid-contrast",
                "1.6",
                "--sigmoid-toe",
                "0.1",
                "--sigmoid-shoulder",
                "0.35",
            ]),
        );
        assert_eq!(cfg.algorithm, Algorithm::Sigmoid);
        assert_eq!(cfg.sigmoid.contrast, 1.6);
        assert_eq!(cfg.sigmoid.toe, 0.1);
        assert_eq!(cfg.sigmoid.shoulder, 0.35);

        // No flag keeps the recipe's values; a flag replaces only its own knob.
        let recipe: ResolvedConfig =
            serde_json::from_str(r#"{"sigmoid":{"contrast":2.0,"toe":0.05}}"#).unwrap();
        let cfg = merge(recipe, &parse_convert(&["--sigmoid-shoulder", "0.4"]));
        assert_eq!(cfg.sigmoid.contrast, 2.0);
        assert_eq!(cfg.sigmoid.toe, 0.05);
        assert_eq!(cfg.sigmoid.shoulder, 0.4);
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
            let mut cfg = ResolvedConfig::default();
            cfg.sigmoid.contrast = bad;
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "contrast {bad} should fail"
            );
        }
        // The cap itself is accepted (boundary is inclusive).
        let mut cfg = ResolvedConfig::default();
        cfg.sigmoid.contrast = crate::algo::sigmoid::SIGMOID_CONTRAST_MAX;
        validate(&cfg).unwrap();
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
            let mut cfg = ResolvedConfig::default();
            cfg.sigmoid.toe = toe;
            cfg.sigmoid.shoulder = shoulder;
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "toe={toe} shoulder={shoulder} should fail"
            );
        }
        // Zero widths (both knees off = the straight line) and the cap itself are
        // valid (boundary inclusive).
        let mut cfg = ResolvedConfig::default();
        cfg.sigmoid.toe = 0.0;
        cfg.sigmoid.shoulder = 0.0;
        validate(&cfg).unwrap();
        let mut cfg = ResolvedConfig::default();
        cfg.sigmoid.toe = knee_max;
        cfg.sigmoid.shoulder = knee_max;
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_sigmoid_without_a_dmax_anchor() {
        // The S-curve is anchored on [0, Dmax]; `dmax = none` only works for
        // the density algorithm's scene-referred output.
        let mut cfg = ResolvedConfig {
            algorithm: Algorithm::Sigmoid,
            ..ResolvedConfig::default()
        };
        cfg.density.dmax = DmaxSource::None;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        // Auto and Explicit anchors are fine under sigmoid...
        cfg.density.dmax = DmaxSource::Auto;
        validate(&cfg).unwrap();
        cfg.density.dmax = DmaxSource::Explicit(1.4);
        validate(&cfg).unwrap();
        // ...and `none` stays valid for the density algorithm.
        cfg.algorithm = Algorithm::Density;
        cfg.density.dmax = DmaxSource::None;
        validate(&cfg).unwrap();
    }

    #[test]
    fn recipe_parses_nested_sigmoid_keys() {
        // §9 places the sigmoid knobs under `sigmoid.*` (no flag prefix); with
        // `deny_unknown_fields` a misplaced key would silently reject the recipe,
        // so pin the documented nesting.
        let cfg: ResolvedConfig = serde_json::from_str(
            r#"{"algorithm":"sigmoid","sigmoid":{"contrast":1.4,"toe":0.15,"shoulder":0.3}}"#,
        )
        .unwrap();
        assert_eq!(cfg.algorithm, Algorithm::Sigmoid);
        assert_eq!(cfg.sigmoid.contrast, 1.4);
        assert_eq!(cfg.sigmoid.toe, 0.15);
        assert_eq!(cfg.sigmoid.shoulder, 0.3);
        // Partial section fills the remaining defaults.
        let cfg: ResolvedConfig = serde_json::from_str(r#"{"sigmoid":{"toe":0.0}}"#).unwrap();
        assert_eq!(cfg.sigmoid.toe, 0.0);
        assert_eq!(cfg.sigmoid.contrast, SigmoidParams::default().contrast);
    }

    #[test]
    fn recipe_parses_nested_density_dmax_key() {
        // The recipe key lives under `density.dmax`; with `deny_unknown_fields` at
        // every level a misplaced key would silently reject, so pin the documented
        // (§9) nesting and all three variant wire-forms through `ResolvedConfig`.
        let cfg: ResolvedConfig =
            serde_json::from_str(r#"{"density":{"dmax":{"explicit":1.5}}}"#).unwrap();
        assert_eq!(cfg.density.dmax, DmaxSource::Explicit(1.5));
        let cfg: ResolvedConfig = serde_json::from_str(r#"{"density":{"dmax":"none"}}"#).unwrap();
        assert_eq!(cfg.density.dmax, DmaxSource::None);
        let cfg: ResolvedConfig = serde_json::from_str(r#"{"density":{"dmax":"auto"}}"#).unwrap();
        assert_eq!(cfg.density.dmax, DmaxSource::Auto);
    }

    #[test]
    fn mutually_exclusive_dmax_flags_are_rejected() {
        for pair in [
            ["--d-max", "1.5", "--no-d-max"].as_slice(),
            ["--d-max", "1.5", "--auto-d-max"].as_slice(),
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
            let mut cfg = ResolvedConfig::default();
            cfg.density.dmax = DmaxSource::Explicit(bad);
            assert!(
                matches!(validate(&cfg), Err(NcError::Usage(_))),
                "explicit d-max {bad} should fail"
            );
        }
        // A positive explicit anchor, and Auto / None, all validate.
        let mut cfg = ResolvedConfig::default();
        cfg.density.dmax = DmaxSource::Explicit(1.8);
        validate(&cfg).unwrap();
        cfg.density.dmax = DmaxSource::None;
        validate(&cfg).unwrap();
        cfg.density.dmax = DmaxSource::Auto;
        validate(&cfg).unwrap();
    }

    #[test]
    fn dump_params_round_trips_through_params() {
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--density-gamma", "1.8", "--output-hdr"]),
        );
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn merge_output_hdr_flag_sets_but_never_clears() {
        // Flag present → hdr on (a forgotten merge arm would silently make the
        // flag a no-op — the four-spot-wiring trap).
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--output-hdr"]));
        assert!(cfg.output.hdr);
        // No flag → the default stays off.
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&[]));
        assert!(!cfg.output.hdr);
        // An absent (false) presence flag must not clobber a recipe `true`.
        let recipe: ResolvedConfig = serde_json::from_str(r#"{"output":{"hdr":true}}"#).unwrap();
        let cfg = merge(recipe.clone(), &parse_convert(&[]));
        assert!(cfg.output.hdr);
        // `--output-sdr` is the explicit escape hatch: it forces a recipe
        // `hdr: true` back to 16-bit (flags win by presence, not value).
        let cfg = merge(recipe, &parse_convert(&["--output-sdr"]));
        assert!(!cfg.output.hdr);
        // ...and is a no-op on an already-SDR config.
        let cfg = merge(ResolvedConfig::default(), &parse_convert(&["--output-sdr"]));
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
        assert!(serde_json::from_str::<ResolvedConfig>(r#"{"densty":{}}"#).is_err());
        // Typo'd key inside a known section (the silent-default trap).
        assert!(
            serde_json::from_str::<ResolvedConfig>(r#"{"density":{"density_gama":1.0}}"#).is_err()
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
        let mut cfg = ResolvedConfig::default();
        cfg.simple.clip_low = 0.9;
        cfg.simple.clip_high = 0.1;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = ResolvedConfig::default();
        cfg.density.density_gamma = 0.0;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // Equal clip endpoints are a zero-width interval → rejected.
        let mut cfg = ResolvedConfig::default();
        cfg.simple.clip_low = 0.5;
        cfg.simple.clip_high = 0.5;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        let mut cfg = ResolvedConfig::default();
        cfg.print.white_balance = [1.0, f32::NAN, 1.0];
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));

        // Negative highlight compression is rejected (the density render silently
        // treats it as "off", so a wrong-sign value must fail loudly, not no-op).
        let mut cfg = ResolvedConfig::default();
        cfg.print.highlight_compress = -0.3;
        assert!(matches!(validate(&cfg), Err(NcError::Usage(_))));
        // Zero is valid (disables the roll-off).
        let mut cfg = ResolvedConfig::default();
        cfg.print.highlight_compress = 0.0;
        validate(&cfg).unwrap();

        // A clean default passes.
        validate(&ResolvedConfig::default()).unwrap();
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
        );
        assert_eq!(cfg.input.export_ir.as_deref(), Some("ir.tiff"));

        // The reserved `--seed` flag parses rather than being rejected by clap.
        let args = parse_convert(&["--seed", "42"]);
        assert_eq!(args.seed, Some(42));
    }

    #[test]
    fn merge_keeps_recipe_source_until_a_flag_replaces_it() {
        // No flag → the recipe's mutually-exclusive choice survives.
        let mut recipe = ResolvedConfig::default();
        recipe.input.color = InputColor::Linear;
        recipe.film_base.source = FilmBaseSource::Explicit([0.9, 0.5, 0.4]);
        let cfg = merge(recipe.clone(), &parse_convert(&[]));
        assert_eq!(cfg.input.color, InputColor::Linear);
        assert_eq!(
            cfg.film_base.source,
            FilmBaseSource::Explicit([0.9, 0.5, 0.4])
        );

        // A flag replaces the whole source — no field is left behind to win on
        // precedence (the #5/#6 fix). `--input-profile` beats a recipe `linear`,
        // and `--base-region` beats a recipe explicit base.
        let cfg = merge(
            recipe,
            &parse_convert(&["--input-profile", "prophoto", "--base-region", "0,0,100,40"]),
        );
        assert_eq!(cfg.input.color, InputColor::Profile("prophoto".into()));
        assert_eq!(
            cfg.film_base.source,
            FilmBaseSource::Region([0, 0, 100, 40])
        );
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
                "--assume-linear",
                "--input-profile",
                "srgb"
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
    fn load_recipe_maps_failures_to_usage() {
        // No path → defaults, infallibly.
        assert_eq!(load_recipe(None).unwrap(), ResolvedConfig::default());

        // Missing file → Usage (exit 2), not Other.
        let missing = std::env::temp_dir().join("nc-no-such-recipe-xyz.json");
        assert!(matches!(
            load_recipe(Some(&missing)),
            Err(NcError::Usage(_))
        ));

        // Malformed JSON and unknown keys both map to Usage.
        for (tag, body) in [
            ("malformed", "{ not json"),
            ("unknown-key", r#"{"density":{"density_gama":1.0}}"#),
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
        std::fs::write(&p, r#"{"density":{"density_gamma":1.8}}"#).unwrap();
        let got = load_recipe(Some(&p)).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(got.density.density_gamma, 1.8);
        assert_eq!(got.print, PrintParams::default());
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
}
