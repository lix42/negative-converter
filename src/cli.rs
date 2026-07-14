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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::io::decode::{DecodeInfo, decode};
use crate::io::encode;
use crate::pipeline::{film_base, stages};
use crate::types::{
    Algorithm, BigTiff, DensityParams, EncodeReport, FilmBase, FilmBaseParams, FilmBaseSource,
    InputColor, InputParams, NcError, OutDepth, OutputParams, PrintParams, Result, SimpleParams,
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
    /// Estimate the base from the detected border (the default behavior).
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
    /// Output bit depth (default `u16`).
    #[arg(long, value_enum)]
    pub out_depth: Option<OutDepth>,
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
    /// How the film base was chosen, as the structured [`FilmBaseSource`]
    /// (`"auto"` / `{"region":[…]}` / `{"explicit":[…]}`) so an agent gets the
    /// sampled rectangle / explicit values without string-parsing a label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub film_base_source: Option<FilmBaseSource>,
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

    // film base: the three source flags are mutually exclusive (clap-enforced);
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

    // output
    if let Some(v) = args.output_opts.out_depth {
        cfg.output.out_depth = v;
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
/// adversarial links.
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

/// Reject write targets that would clobber the input scan or one another —
/// e.g. `-o` equal to the input (destroys the negative), or `--report-file`
/// equal to the output/sidecar (truncates a just-written artifact) — all of
/// which would otherwise "succeed" with exit 0. Fail loudly up front instead.
fn ensure_write_targets_distinct(input: &Path, targets: &[(&str, &Path)]) -> Result<()> {
    let input_key = collision_key(input);
    let mut seen: Vec<(&str, PathBuf)> = Vec::with_capacity(targets.len());
    for (label, path) in targets {
        let key = collision_key(path);
        if key == input_key {
            return Err(NcError::Usage(format!(
                "{label} ({}) would overwrite the input scan",
                path.display()
            )));
        }
        if let Some((other, _)) = seen.iter().find(|(_, k)| *k == key) {
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

    // Stage 1 — decode.
    let (image, info) = decode(&args.input)?;
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

    // Clear any stale lcms2 flag so only errors from *this* render are counted.
    let _ = cms_error_occurred();
    // Stages 2–4 — film-base estimate → algorithm → output color transform.
    let rendered = stages::render(
        &image,
        &cfg.film_base,
        stages::algo_params(cfg.algorithm, &cfg.simple, &cfg.density, &cfg.print),
        &cfg.output,
    )?;
    // lcms2 transform/profile failures reach us only through the global handler
    // (`transform_in_place` is infallible), so check the flag it sets.
    if cms_error_occurred() {
        return Err(NcError::Other(
            "color management (lcms2) reported a runtime error; see stderr".into(),
        ));
    }
    report.film_base = Some(rendered.film_base);

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
    if let Some(path) = &export_ir {
        encode::export_ir(&image, cfg.output.out_depth, path)?;
        log.info(format_args!("wrote IR plane {}", path.display()));
        report.ir_exported = Some(path.clone());
    }

    // Stage 5 — encode + effective-recipe sidecar.
    let loss = encode::encode(
        &rendered.image,
        &cfg.output,
        Some(&rendered.icc),
        &args.output,
    )?;
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

    report.elapsed_ms = Some(elapsed_ms(started));

    // Emit the report before the `--strict` gate so the machine-readable record
    // lands even when a warning then fails the run. (A hard I/O error above
    // returns earlier — its exit code and stderr message are the signal there.)
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

    // Suggested Dmin via best-effort auto detection. For inspect this is
    // informational — real scans (holder → rebate → picture) usually need an
    // explicit `--base-region`/`--film-base`, so a failure is a note, not fatal.
    match film_base::estimate(&image, &FilmBaseParams::default()) {
        Ok(base) => {
            report.film_base = Some(base);
            report.film_base_source = Some(FilmBaseSource::Auto);
        }
        // The estimate error already carries actionable advice (pass
        // --base-region/--film-base); wrap it with a short lead-in, no duplicate.
        Err(e) => push_warning(
            &mut report,
            &log,
            format!("suggested Dmin unavailable — {e}"),
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

    let base = film_base::estimate(
        &image,
        &FilmBaseParams {
            source: source.clone(),
        },
    )?;

    let mut report = Report {
        command: Some("estimate"),
        input: Some(args.input.clone()),
        film_base: Some(base),
        film_base_source: Some(source),
        ..Report::default()
    };
    for w in &info.warnings {
        push_warning(&mut report, &log, w.clone());
    }
    report.elapsed_ms = Some(elapsed_ms(started));
    emit_report(
        &report,
        args.report.report,
        args.report.report_file.as_deref(),
    )
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
    fn dump_params_round_trips_through_params() {
        let cfg = merge(
            ResolvedConfig::default(),
            &parse_convert(&["--density-gamma", "1.8", "--out-depth", "f32"]),
        );
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ResolvedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
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
}
