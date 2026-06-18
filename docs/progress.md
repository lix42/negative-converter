# Negative Converter — Progress Log

How each task is actually being carried out — what was done and how, key
decisions, what works, what doesn't, and notes for dependent tasks. TASKS.md holds
the authoritative status (the checkboxes); this file is the narrative beside it.

One `##` section per task, named by the kebab task name. Read this before starting
a task; update your own section as you work. Append entries — don't rewrite them.

## project-foundation
**Status:** done
**Updated:** 2026-06-13

- Goal: Cargo project, dependency declarations, module skeleton, and shared core
  types (`LinearImage`, `FilmBase`, `OutDepth`, `NcError`, param structs).
- **Done.** `cargo init` binary crate `nc` (edition 2024, Rust 1.94). Deps added
  via `cargo add` so versions track current crates.io: `clap` 4 (`derive`),
  `serde` 1 (`derive`), `serde_json`, `tiff`, `image`, `palette`, `rayon`,
  `kamadak-exif`, `lcms2` 6 (pulls `lcms2-sys`, builds the C lib via `pkg-config`/
  vendored — builds clean on macOS).
- Module tree per design-spec §10: `main.rs` (thin: dispatch + exit-code map),
  `cli.rs`, `io/{decode,encode}.rs`, `pipeline/{film_base,color,stages}.rs`,
  `algo/{mod,simple,density}.rs`, `types.rs`. All non-`types` modules are stubs:
  fixed function/trait signatures returning `todo!()` so the tree compiles and
  downstream tasks have a stable shape to fill.
- **Decisions / notes for dependent tasks:**
  - `types.rs` is the neutral contract — **no crate-specific image/TIFF types in
    it**. Conversions to/from `image`/`tiff` belong in `io/*`.
  - `NcError` → exit code lives in **one** place: `NcError::exit_code()` (§11
    mapping: Other=1, Usage=2, Decode=3, Unsupported=4, Write=5). `NcError` impls
    `Display + Error`; `type Result<T> = std::result::Result<T, NcError>` is the
    crate-wide alias. `main` prints the error to stderr and returns the code.
  - Added two enums beyond the task sketch: `OutDepth {U16,F32}` and
    `BigTiff {Auto,On,Off}`, both `#[serde(rename_all="lowercase")]` so recipe
    JSON reads `"u16"`/`"auto"` etc. `OutputParams` carries them.
  - Param structs (`FilmBaseParams`, `DensityParams`, `SimpleParams`,
    `PrintParams`, `OutputParams`) use `#[serde(default)]` + a `Default` impl, so
    a **partial** recipe fills the rest from defaults (tested). Fields mirror the
    §9 flag names exactly (`density_scale`, `print_exposure`, `invert_white_balance`,
    …). Defaults are neutral/identity placeholders — algo tasks refine the numbers.
  - Stub signatures already chosen (change if a task needs to):
    `io::decode::decode(&Path) -> Result<LinearImage>`,
    `io::encode::encode(&LinearImage, &OutputParams, Option<&[u8]> /*icc*/, &Path) -> Result<()>`,
    `pipeline::film_base::estimate(&LinearImage, &FilmBaseParams) -> Result<FilmBase>`,
    `pipeline::color::to_output(&LinearImage, &OutputParams) -> Result<(LinearImage, Vec<u8>)>`
    (returns the converted image **and** the ICC blob to embed),
    `algo::Converter::convert(&self, &LinearImage, &FilmBase) -> Result<LinearImage>`,
    `cli::run() -> Result<()>`.
  - `main.rs` has a temporary crate-level `#![allow(dead_code)]` (the stubs aren't
    wired until `pipeline-orchestration`). **Remove it** when that task lands so
    genuinely-dead code surfaces again.
- **Verify:** `cargo build` clean, `cargo test` 4/4 pass (incl. `DensityParams`
  JSON round-trip + partial-recipe-defaults), `cargo clippy --all-targets` clean.
  `Cargo.lock` committed (binary crate); `/target` gitignored.
- **CI:** `.github/workflows/ci.yml` runs on every PR + push to `main`:
  `cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` → build →
  test (ubuntu-latest, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache`).
  **The gate is strict** — keep `fmt` clean and zero clippy warnings, or CI fails.

## silverfast-decode
**Status:** not started
**Updated:** —

- Goal: read SilverFast HDR (48-bit RGB) and HDRi (64-bit RGB+IR) TIFFs into a
  linear `f32` `LinearImage`, preserving the IR plane.
- Note (from project-foundation review): build the result via
  `LinearImage::new(w, h, rgb, ir)` — it validates the buffer-length invariants
  (`rgb.len()==w*h*3`, `ir.len()==w*h`, non-zero dims, no size overflow) at the
  boundary. Don't construct the struct literally and skip the check.
- Note (from PR #2 review): `nc inspect` / the JSON report need original format,
  channel count, bits-per-sample, and decoder warnings — data lost once the image
  is normalized. Return a `DecodeInfo` alongside the `LinearImage` (decide the
  exact shape here) so inspection doesn't have to re-parse the file.

## tiff-encode
**Status:** not started
**Updated:** —

- Goal: write u16/f32 TIFF with embedded ICC, BigTIFF auto-promote, IR export, and
  sidecar JSON.

## color-management
**Status:** not started
**Updated:** —

- Goal: working→output ICC transforms with depth-aware default profile (sRGB for
  u16, wide-gamut for f32); provide the ICC blob to embed.

## film-base-estimation
**Status:** not started
**Updated:** —

- Goal: estimate `Dmin` `FilmBase` from border/region with full CLI override.
- Note (from project-foundation review): `FilmBaseParams` keeps three flat fields
  (`film_base`, `base_region`, `auto_base`) which can express contradictory combos.
  **Enforce and unit-test the precedence** here: explicit `film_base` overrides
  `base_region` overrides `auto_base`. Use `FilmBase::from([f32;3])` for the
  `film_base` override (conversion lives in `types.rs`). If the flat shape proves
  awkward, consider collapsing to an enum `FilmBaseSource { Auto, Region(..),
  Explicit(..) }` — deferred from foundation, decide here.

## algo-interface
**Status:** done
**Updated:** 2026-06-16

- Goal: `Converter` trait + algorithm selection so converters are pluggable.
- **Done.** Everything lives in `src/algo/mod.rs`:
  - `Converter` trait kept **object-safe** — params live in the implementor, no
    associated `Params` type, `convert(&self, image, base) -> Result<LinearImage>`.
    The design-spec §7.2 sketch shows an associated-type variant; that can't form
    `Box<dyn Converter>`, which `build()` and the verification both need, so this
    task supersedes the sketch (noted in a doc comment on the trait).
  - `Algorithm { Simple, Density }` — `Copy`, `serde(rename_all="lowercase")` so it
    round-trips as `"simple"`/`"density"`, `#[default] Density` (the documented
    default algorithm).
  - `FromStr for Algorithm` with `type Err = NcError`; unknown names →
    `NcError::Usage` (exit 2), failing loudly instead of defaulting. CLI parses
    `--algorithm` through this.
  - `AlgoParams` enum: `Simple(SimpleParams)` and
    `Density { density: DensityParams, print: PrintParams }`. **Decision:** the
    `Density` variant (and the `Density` converter struct) carries **both**
    sub-stages' params now — density correction + the separate print render —
    rather than deferring `PrintParams` to `algo-density`. They stay distinct
    fields, preserving the density/print separation (core fidelity rule).
    `AlgoParams::algorithm()` reports which algorithm a param set selects.
  - `build(params: AlgoParams) -> Box<dyn Converter>` — **infallible**, takes the
    param set by value and moves it into the converter (no clone). The task sketch
    had `build(algo, params)` taking the algorithm separately, but the
    `AlgoParams` variant already *is* the algorithm selector (`AlgoParams::algorithm()`
    derives it totally), so a separate `Algorithm` argument carried zero info and
    only created a mismatch error that one argument makes unrepresentable
    ("make illegal states unrepresentable"). Any `--algorithm` vs flag
    contradiction is resolved/rejected in `cli-framework` where the flag context
    lives, and the CLI hands `build` one already-valid `AlgoParams`. (Decision from
    the ship code review — type-design agent.) The match is exhaustive over
    `AlgoParams`, so a future algorithm variant fails at compile time.
  - `AlgoParams::algorithm() -> Algorithm` kept (CLI uses it to derive the
    algorithm for the JSON report from the param set alone).
- **Touched `algo/density.rs`:** `Density` struct now has `density: DensityParams`
  + `print: PrintParams` (was `params: DensityParams`). `algo-density` fills the
    `convert` body and consumes both fields.
- **Notes for dependent tasks:**
  - `algo-simple` / `algo-density`: just implement `Converter::convert` on the
    existing `Simple` / `Density` structs; the field shapes are fixed (`Simple.params`,
    `Density.density` + `Density.print`). Don't widen the trait — push new tone
    controls into the param structs.
  - `cli-framework`: parse `--algorithm` via `Algorithm::from_str` (maps unknown →
    `Usage` for you); assemble an `AlgoParams` for the chosen algorithm and pass it
    to `algo::build`. `Algorithm` serializes lowercase for the JSON report/recipe.
- **Verify:** `cargo build`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` all clean; `cargo test` 13/13 (6 new: `from_str` ok + unknown
  → exit 2, default = density, lowercase serialize, object-safe boxed call, `build`
  for both algorithms, `build` mismatch → exit 2). Object-safety proven by a test
  `Identity` converter exercised through `Box<dyn Converter>`.

## cli-framework
**Status:** done
**Updated:** 2026-06-18

- Goal: clap subcommands, recipe load/merge (flags override), JSON report,
  `params` subcommand, exit-code mapping.
- Note (from project-foundation review): param-struct ranges are doc-only so far.
  Add validation **at the parse/merge boundary** (not in the pure stages): reject
  NaN, `clip_low > clip_high`, non-positive gamma/gains, etc., mapping failures to
  `NcError::Usage` (exit 2) so bad recipes fail loudly. The pure stages then trust
  their inputs.
- Note (from PR #2 review): **reject unknown recipe keys.** With `#[serde(default)]`
  alone a typo like `density_gama` silently deserializes to the default → a quietly
  wrong image, which the "fail loudly" rule forbids. Add `#[serde(deny_unknown_fields)]`
  (or equivalent) on the recipe-facing structs — placement depends on the recipe
  layout you choose here (per-struct sub-objects vs one flat object); deny only
  works cleanly with the former.
- Note (from PR #2 review): `--export-ir <path>` (design §9) has no typed home yet —
  `OutputParams` only carries depth/profile/bigtiff. Add the path here (or in a
  dedicated output config) as you assemble the full param surface, so orchestration
  can drive the IR exporter. Likewise the encoder needs the resolved recipe JSON to
  write the `out.tiff.json` sidecar — pass it the Report/Recipe value once defined.

- **Done.** `cli.rs` holds the full agent-facing surface; `cli::run()` parses and
  dispatches. `main`'s exit-code mapping is unchanged.
- **Decisions for dependent tasks (esp. `pipeline-orchestration`):**
  - **Recipe = nested per-stage objects** (user decision), not a flat bag:
    `{ "algorithm": "...", "input": {…}, "film_base": {…}, "density": {…},
    "print": {…}, "simple": {…}, "output": {…} }`. This is the *only* layout that
    lets `#[serde(deny_unknown_fields)]` reject typos at every level
    (`serde(flatten)` would silently defeat it). The single struct that *is* this
    shape is `cli::ResolvedConfig` — it doubles as the recipe (partial, serde
    defaults fill gaps), the `--dump-params` output, and `nc params` output, so the
    three can't drift. **Updated design-spec §8/§9 + HTML** to document the nesting.
  - **Merge model:** clap arg structs use `Option<T>` per knob (+ presence-flag
    `bool`s for `--auto-base`/`--assume-linear`). `merge(cfg, &ConvertArgs)` is a
    pure fn applying `defaults ← recipe ← CLI` (flags win); a `false` presence flag
    never clobbers a recipe `true`. Orchestration consumes the returned
    `ResolvedConfig` — it should not re-read CLI args.
  - **Validation at the boundary:** `validate(&ResolvedConfig)` rejects NaN/inf,
    `clip_low > clip_high`, non-positive gamma/gains/film-base, zero `base-region`
    w/h → all `NcError::Usage` (exit 2). **Pure stages can trust their inputs** —
    don't re-validate ranges downstream.
  - **New types added to `types.rs`:** `Algorithm {Simple,Density}` (default
    `Density`, serde-lowercase + `clap::ValueEnum`); `InputParams`
    (`assume_linear`, `input_profile`) for the §9 input flags; `export_ir:
    Option<String>` added to `OutputParams`. `OutDepth`/`BigTiff` gained
    `clap::ValueEnum` (their lowercase ValueEnum names already match serde).
    `deny_unknown_fields` added to all recipe-facing param structs.
  - **stdout is report-only;** logs/warnings/errors go to stderr (agents pipe
    stdout). `--report json|none`, `--report-file`, `-v`, `--quiet`, `--strict`
    are parsed and carried; the `Report` struct + `emit_report()` exist but are
    populated by orchestration (kept minimal here).
  - **clap error handling:** `Cli::parse()` lets clap exit directly — `--help`/
    `--version` exit 0, usage/value-parser errors exit 2 — so those don't route
    through `NcError`. Everything else flows through the `NcError` exit-code map.
  - **Stubs:** `convert`/`inspect`/`estimate` resolve+validate config (and write
    `--dump-params`) then return `NcError::Other("… not yet wired
    (pipeline-orchestration)")` (exit 1). The pipeline replaces those returns.
    `main.rs`'s `#![allow(dead_code)]` still needed (Report/emit_report unused
    until wired) — remove it in `pipeline-orchestration`.
- **Verify:** `cargo fmt --check`, `clippy --all-targets -D warnings`, build all
  clean; `cargo test` 14/14 (6 new cli tests: parser `debug_assert`, comma-list
  parsers, merge precedence, dump→reload round-trip, unknown-key rejection,
  validation). Manual: `nc --help`/`convert --help` list every §9 flag; `nc params`
  emits the full default JSON; dump→`--params` reload round-trips byte-identical;
  forced usage/validation/bad-recipe/bad-value paths all exit 2.
- **2026-06-18 (ship review):** multi-agent review before merge. Fixes:
  - **Bug:** `PrintParams::print_exposure` default was `1.0`; spec §9 neutral is
    `0.0` (exposure is in **stops/EV**, not a linear multiplier — every other print
    default is identity). Corrected to `0.0` and documented the unit in `types.rs`.
  - **`--strict` made an explicit deferral:** it's parsed but only acted on by
    `pipeline-orchestration` (promote warnings→errors); marked so in `run_convert`
    rather than looking silently dropped. **For pipeline-orchestration: wire
    `args.strict` into the warnings path.**
  - **Tests +3 → 25 total:** boolean presence-flag merge (`assume_linear`/
    `auto_base` — a `false` flag never clobbers a recipe `true`), `load_recipe`
    error mapping (missing/malformed/unknown-key file → `NcError::Usage`), and
    recipe-smuggled bad values caught by `validate` (zero film-base transmission,
    zero-area `base_region`) — recipes bypass clap value-parsers, so `validate` is
    their only guard.
  - **Deferred (noted, not done):** profile/`export_ir` as `PathBuf`/enum vs
    `String`; range bounds on print knobs and `film_base ≤ 1.0`; a `ValidatedConfig`
    newtype to make "unvalidated config reaches a stage" unrepresentable; a
    `--no-assume-linear` counterpart. These belong to the algorithm / film-base /
    pipeline-orchestration tasks that own those semantics.
- **2026-06-18 (PR #5 bot review):** addressed automated review (claude-review /
  Codex / Gemini). Fixes (26 tests):
  - **`export_ir` moved `OutputParams` → `InputParams`** (recipe key
    `output.export_ir` → `input.export_ir`). Spec §9 lists `--export-ir` under
    Input/decode; with `deny_unknown_fields` the old home rejected the
    documented recipe shape. Code now matches the spec.
  - **`--seed <n>` now parsed** (reserved `Option<u64>` on `ConvertArgs`, carried
    like `--strict`). Spec §documents it; clap previously rejected it as unknown,
    so the documented interface wasn't actually accepted.
  - **Equal clip endpoints rejected:** `validate` now requires `clip_low <
    clip_high` (was `<=`) — equal bounds are a zero-width interval the simple
    remap can't normalize without dividing by zero.
  - **Declined (with reasons):** let-chain "unstable" claim is false here (edition
    2024, CI green proves it compiles); `assume_linear`/`input_profile` override +
    film-base source precedence (explicit>region>auto) belong to
    film-base-estimation/pipeline-orchestration; rejecting flags for the
    unselected algorithm is deliberate — inert params are retained so recipes
    round-trip across `--algorithm` switches.

## algo-simple
**Status:** not started
**Updated:** —

- Goal: channel-inversion baseline converter (debug / B&W) with white balance and
  black/white points.

## algo-density
**Status:** not started
**Updated:** —

- Goal: density-domain converter (Cineon/negadoctor style) with separate density
  and print-render sub-stages; the default algorithm.

## pipeline-orchestration
**Status:** not started
**Updated:** —

- Goal: wire `convert`/`inspect`/`estimate` end to end, producing a positive TIFF
  and JSON reports from a real scan.
