# CLI Framework

## Goal

Build the agent-facing command surface: clap argument parsing for all subcommands
and flags, JSON recipe load/merge (flags override a loaded recipe), JSON report
emission, the `params` subcommand, and stable exit codes. This is the scriptable
contract an agent drives.

## Design

`cli.rs` (+ `main.rs` calls into it):

```rust
pub enum Command { Convert(ConvertArgs), Inspect(IoArgs), Estimate(IoArgs), Params }

pub struct ConvertArgs {
    pub input: PathBuf, pub output: PathBuf,
    pub algorithm: Algorithm,
    pub film_base: FilmBaseParams,
    pub density: DensityParams, pub simple: SimpleParams, pub print: PrintParams,
    pub output: OutputParams,   // depth, profile, bigtiff, export-ir
    pub recipe_in: Option<PathBuf>, pub dump_params: Option<PathBuf>,
    pub report: ReportFormat, pub report_file: Option<PathBuf>,
    pub strict: bool, pub verbose: u8,
}
```

Responsibilities (this task; the actual conversion is `pipeline-orchestration`):
- Define the clap parser for every flag in design spec §9, grouped by stage.
- **Recipe merge:** load `--params recipe.json` into the param structs, then apply
  individual `--flag` overrides on top (flags win). Produce one resolved config.
- **`--dump-params`:** serialize the effective config to JSON.
- **`params` subcommand:** print the full default/effective parameter set as JSON.
- **Report emission:** a `Report` struct (estimated values, warnings, output path,
  timings) serialized to stdout or `--report-file`.
- **Exit codes:** translate `NcError` → process exit code (reuse
  `project-foundation`'s mapping). No interactive prompts anywhere.

## Implementation Suggestion

- Resolve `clap` derive vs builder API via Context7; derive is likely cleanest for
  this many grouped flags.
- Parse comma lists (`R,G,B`, `x,y,w,h`) with small value-parser helpers.
- Make recipe-merge a pure function `merge(recipe, cli_overrides) -> ResolvedConfig`
  so it's unit-testable without running the pipeline.
- Keep stdout for the JSON report clean (logs/warnings to stderr) so agents can
  pipe stdout straight into a parser.

## How to Verify

- `nc --help` and each subcommand's `--help` list the expected flags.
- `nc params` emits valid JSON of the full default param set.
- Recipe-merge unit test: a recipe value is overridden by the matching `--flag`;
  unspecified flags keep recipe values; unspecified-everywhere fall to defaults.
- `--dump-params` output re-loads via `--params` to the same config (round-trip).
- A forced error path exits with the documented non-zero code and a stderr message.

## Dependencies

- [Project foundation and core types](project-foundation.md)
