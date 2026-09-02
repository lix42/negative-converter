# Negative Converter — core Progress Log

Execution log for the `core` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `core`:

- **`cli` is the only orchestrator; stages stay pure.** Decode (stage 1) and
  encode (stage 5) are I/O — they live in `src/io/` and are driven from `cli`;
  `pipeline::stages::render` is the pure algorithm→output-color core.
  Film-base estimation was deliberately pulled *out* of `render` into the
  orchestrator (so its warnings surface before a fallible render) — `render`
  takes an already-resolved `&FilmBase`.
- **`ResolvedConfig` is the recipe.** One nested per-stage struct doubles as the
  recipe, `--dump-params`, and `nc params` output, so the three can't drift.
  Merge model is `defaults ← recipe ← CLI` (flags win); an absent presence flag
  never clobbers a recipe value. Every recipe struct uses
  `deny_unknown_fields`, so a misplaced key is a loud error — keep new knobs in
  the section design-spec §9 assigns them.
- **A knob spans four coupled spots**: the CLI `*Overrides` field, the recipe
  `*Params` field, a `merge` arm, and usually a `validate` check. A forgotten
  `merge` arm makes the flag a silent no-op — add a merge test.
- **Validation happens at the CLI boundary; pure stages trust their inputs**
  for *config* values. Runtime-derived values (an estimated film base, a measured
  anchor) are the exception — guard those where they're consumed.
- **Exit codes (design-spec §11):** Usage=2, Decode=3, Unsupported=4, Write=5,
  Other=1. `NcError::exit_code()` is the single mapping.
- **stdout is report-only**; logs and warnings go to stderr. Reports emit
  *before* any `--strict` gate, so the machine-readable record always lands and
  the signal is the exit code. Known gap: those stdout writes still use
  `println!` and panic on a closed pipe (`core/stdout-broken-pipe-safety`).
- **lcms2 gotcha:** `transform_in_place` is infallible and Little CMS's default
  error handler silently swallows faults, so `cli` installs the *process-global*
  handler via `lcms2-sys` FFI at startup and `run_convert` checks the flag around
  the render. Don't move colour transforms somewhere that skips this.
- **`nc roll` is a separate subcommand**, not a mode of `convert`, and shares the
  per-frame core `convert_frame` — so a roll frame is **byte-identical** to the
  equivalent single `convert`. Config errors fail up front (exit 2/4); per-frame
  runtime errors are recorded and the roll continues, exiting 1. A roll whose
  recipe isn't an explicit film base warns loudly rather than failing.
- **Conversion identity is stamped into every report** (`src/version.rs`,
  `core/conversion-versioning` — built, not yet shipped): build identity (semver +
  git commit + dirty + target), the behavioral **`pipeline_version`**, and a
  `params_hash` of the canonical resolved recipe. `pipeline_version` is **1**, and
  it **collapses three default changes** since the `v0` baseline into one label:
  `film-base/dmax-reference` (roll-fixed nominal `Dmax = 2.0` **density**),
  `film-base/auto-base-redesign` (the inward-scan rebate detector), and
  `core/input-semantics`. `0` is the `docs/reports/v0-baseline.md` behavior. The
  label is **independent of semver** and bumps only when *default* conversion
  behavior changes, enforced by a golden drift gate
  (`version::PIPELINE_FINGERPRINTS`) over three small, deliberately
  target-independent fingerprints — the curated per-pixel vectors in
  `pipeline::stages::golden` (stages 3–4), `film_base::estimate` over the frozen
  scan in `pipeline::film_base::golden` (stage 2, which the render fingerprint
  cannot see because it is handed a hardcoded base), and the default recipe
  *values*. Deliberately **not** a whole-file or whole-frame checksum, which would
  be target-dependent. **Change a default in a fingerprinted stage and that test
  fails and tells you exactly what to update — but read
  `version::PipelineFingerprint` for the explicit list of what it does NOT cover
  (decode, stage-1b semantics, the lcms2 transform, encode, non-default film-base
  sources, real-scan geometry). It is the automatic half, not the whole answer.**
- **The sidecar is now `{ "meta": {…identity…}, "params": {…recipe…} }`.**
  `--params` accepts the envelope *and* a bare legacy recipe. Identity must never
  become a recipe key (`deny_unknown_fields` would reject every new sidecar), and
  identity / `output_stats` / `compare` are **operational** like `--report` and
  telemetry: no recipe keys, no `merge` arms, no effect on output bytes.


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
    2024, CI green proves it compiles); rejecting flags for the unselected
    algorithm is deliberate — inert params are retained so recipes round-trip
    across `--algorithm` switches.
- **2026-06-18 (#5/#6 enum rework, user-directed):** the two deferred merge gaps
  were fixed by modeling mutually-exclusive choices as enums (illegal states
  unrepresentable), not patching the booleans. **Recipe shape changed** — spec
  §9 (md+html) updated to match:
  - **`FilmBaseSource { Auto, Region([u32;4]), Explicit([f32;3]) }`** replaces the
    `film_base`/`base_region`/`auto_base` trio. `FilmBaseParams` is now
    `{ source }`. Recipe: `"film_base": { "source": "auto" | {"region":[…]} |
    {"explicit":[…]} }`. Higher specificity always wins with no fallback, so it
    was always one choice, not three knobs.
  - **`InputColor { Auto, Linear, Profile(String) }`** replaces
    `assume_linear`/`input_profile`. `InputParams` is now `{ color, export_ir }`.
    Recipe: `"input": { "color": "auto" | "linear" | {"profile":"<icc>"} }`.
    `"auto"` (the no-flag default) = the file's embedded/default profile, which is
    **not** linear — that's why `assume_linear` can't be inferred from "no
    profile". **For color-management/decode: define what `Auto` resolves to.**
  - **CLI:** the source flags within each group are now a clap mutual-exclusion
    group (`conflicts_with`/`conflicts_with_all`) — passing two is a usage error.
    `merge` maps whichever single flag is present to the enum, replacing the
    recipe's choice; so `--input-profile` over a recipe `linear` now wins cleanly
    (the #6 bug) and `--base-region` over a recipe explicit base wins (the #5 bug).
  - **Verified:** fmt/clippy/build clean, **27 tests**; manual `nc params` shows
    the new shapes; recipe load→`--dump-params` round-trips the nested variants;
    `--assume-linear` over a `{"profile":…}` recipe resolves to `"linear"`.


## pipeline-orchestration
**Status:** done
**Updated:** 2026-07-14

- Goal: wire `convert`/`inspect`/`estimate` (+ `params`) end to end, producing a
  positive TIFF and JSON reports from a real scan. Final Step-1 MVP task.
- **Done.** The four subcommands run end to end. Full CI gate clean
  (`fmt --check`, `clippy --all-targets -D warnings`, `build`, `test`); suite is
  **124 tests** (110 unit + 14 new end-to-end in `tests/pipeline.rs`).

### What was built
- **`pipeline/stages.rs` (the pure in-memory core).** `render(image,
  &FilmBaseParams, AlgoParams, &OutputParams) -> Result<Rendered>` threads stages
  2–4 (film-base estimate → algorithm → output color transform) and returns
  `Rendered { image, icc, film_base }`. `algo_params(algorithm, &simple,
  &density, &print) -> AlgoParams` assembles the selected algorithm's param set.
  Both are **pure** (no I/O); decode (stage 1) and encode (stage 5) are I/O and
  stay in the `cli` orchestrator, honoring "stages stay pure; main/cli
  orchestrate."
- **`cli.rs` (orchestration).** `run_convert` does decode → IR-export fast-fail
  guard → render → lcms error check → BigTIFF-auto notice → optional IR export →
  encode → effective-recipe sidecar → report emit → `--strict` gate. `run_inspect`
  decodes and reports `DecodeInfo` + a best-effort auto `Dmin` (a failed auto is a
  report *warning*, not fatal — real scans need `--base-region`/`--film-base`).
  `run_estimate` runs only film-base estimation from the selected source and emits
  the `FilmBase`. `run_params` unchanged.
- **Report shape.** One `Report` struct serves all commands; per-command
  irrelevant fields are `None`/empty and omitted via `skip_serializing_if`, so
  stdout is a clean per-command JSON object. Serialize-only (embeds the
  serialize-only `DecodeInfo`/`EncodeReport`). `film_base_source` is the
  **structured** `FilmBaseSource` (`"auto"` / `{"region":[…]}` / `{"explicit":[…]}`),
  not a display string, so an agent gets the sampled rect without string-parsing.
- **`io/encode.rs`.** Added `plans_bigtiff(&OutputParams, &LinearImage, icc_len)`
  reusing the internal `resolve_bigtiff`, so orchestration can report an `auto`
  BigTIFF promotion without duplicating the threshold logic.
- **Removed `#![allow(dead_code)]` from `main.rs`.** Revealed three deliberately
  unused-by-Step-1 items now behind narrow, documented `#[allow(dead_code)]`:
  `algo::Algorithm` + `AlgoParams::algorithm()` (the CLI/recipe standardized on
  the identical `types::Algorithm`) and `color::icc_profile` (orchestration gets
  the ICC from `to_output`). See follow-up below.

### Key decisions / notes for dependents
- **lcms2 gotcha handled (CLAUDE.md).** `color.rs` builds profiles/transforms on
  lcms2's **global** context, and `transform_in_place` is infallible; Little CMS's
  default handler is a **no-op that silently swallows errors** (verified in the
  vendored source). The safe `lcms2` wrapper only exposes the handler per
  `ThreadContext`, which would *not* cover the global-context transforms — so
  `cli` installs the process-global handler directly via the **`lcms2-sys` FFI**
  (`cmsSetLogErrorHandler`), added as a direct dep (`cargo add lcms2-sys`, 4.0.7,
  already transitively present). The handler sets an `AtomicBool` + logs to
  stderr; `run_convert` clears the flag right before `render` and checks it right
  after, turning a runtime CMS fault into a loud `NcError::Other`.
- **Exit codes (§11):** Usage=2 (bad params/recipe, bad `--film-base`), Decode=3
  (unreadable/non-TIFF input), Unsupported=4 (`--export-ir` on an HDR scan with no
  IR plane — fails *before* any output is written), Write=5 (unwritable output
  path), Other=1 (degenerate estimated base; `--strict` warning promotion; lcms
  fault). All exercised end-to-end.
- **`--strict`** promotes any accumulated warning to exit 1 *after* emitting the
  report (the machine-readable record still lands). Output/sidecar are written
  before the strict gate because clip counts are only known post-encode — a
  `--strict` failure therefore leaves the (honestly-reported-as-clipped) files on
  disk; the loud exit code is the signal. Documented in the flag's behavior.
- **IR handling:** the "IR present but not consumed in Step 1" notice is a report
  warning **only when `--export-ir` is absent** — exporting *is* the user handling
  it, so `--strict --export-ir` is a usable workflow on the primary HDRi format
  (otherwise every HDRi `--strict` run would fail on that notice).
- **`estimate` gained the film-base flags** (`EstimateArgs` flattens
  `FilmBaseOverrides`) so the design-spec §8 calibrate-once-from-a-reference
  workflow (`nc estimate ref.tif --base-region …`) works; `inspect` keeps the
  bare `IoArgs`. Explicit-base range validation is shared with `convert` via
  `validate_explicit_film_base`.
- **Verbosity:** `Log { verbose, quiet }` — `-v` enables stderr progress lines,
  `--quiet` silences them; warnings always land in the report regardless, and a
  **non-finite (NaN/inf) fault is echoed to stderr even under `--quiet`** (it's a
  numerical fault, not routine clipping, so `--quiet --report none` can't fully
  hide it). stdout stays report-only.

### Real-scan verification (committed fixtures — the large `../nc-assets/*`
  and `~/Pictures/scan/*` dirs are absent in this environment, so verification
  used the committed real SilverFast scans `tests/fixtures/{hdr-48bit,hdri-64bit}.tif`,
  502×462, from the small asset set):
- `inspect hdri-64bit.tif` → `format=hdri 502x462 ir=true`,
  make/model/software `Plustek / OpticFilm 8300i / SilverFast 9.2.8`; auto Dmin
  reported unavailable (non-uniform border, relative spread 0.83) as a warning —
  the documented real-scan behavior.
- `estimate hdr-48bit.tif --base-region 10,10,50,50` → structured
  `film_base_source = {"region":[10,10,50,50]}`, a finite per-channel base.
- `convert … --algorithm density` (default **u16**) → exit 0, report warns
  **100% clipped_high (695772/695772)** — the known no-white-anchor issue below.
- `convert … --algorithm density --out-depth f32 --film-base 0.9,0.55,0.42` →
  exit 0, `loss = {clipped_low:0, clipped_high:0, non_finite:0}` (clean HDR).
- `convert … --export-ir ir.tiff` on HDRi → writes a 1-channel IR TIFF; on HDR →
  exit 4, no output written. `--strict --export-ir` on HDRi → exit 0.
- Determinism confirmed: two identical `convert` runs produce **byte-identical**
  TIFF + sidecar; a sidecar reloaded via `--params` reproduces the output.

### Known issue explicitly NOT fixed here (parallel task `dmax-white-anchor`)
- With the **current** default density params the render maps scene black (the
  base) to `1.0` with all detail above it, so the default **u16** encode clips
  heavily (≈100% on these fixtures) — surfaced loudly via the clip report and
  `--strict`-promotable. This is **temporary**: the parallel `dmax-white-anchor`
  branch adds a Dmax white anchor that drops the default clip fraction to ~0.5%
  (per that agent), so nothing here treats the heavy clipping as permanent. Test
  wording/assertions were kept anchor-independent — `u16_clipping_is_reported_and_
  strict_promotes_it` forces clipping with a large `--print-exposure` rather than
  relying on the default, so it stays valid after the anchor lands. Verify HDR
  output end-to-end with `--out-depth f32` (clean) meanwhile.
- **Report is extensible for the incoming `dmax`.** `dmax-white-anchor` adds a
  defaulted `Converter::convert_reported -> ConvertReport { dmax }`; at merge the
  resolved anchor rides into the JSON report by adding one optional field to
  `Report` and carrying a `ConvertReport` on `stages::Rendered` (which already
  bundles the algorithm's outputs) — the flat `skip_serializing_if` report shape
  takes a new optional field without reshaping the JSON. Not integrated here per
  the coordination note (its API does not exist in this branch); cli.rs edits were
  kept tight so its `DmaxOverrides` merge/validate/flatten conflict stays small.

### Follow-ups / deferred (with reasons)
- **Unify the two `Algorithm` enums.** `types::Algorithm` (CLI/recipe) and
  `algo::Algorithm` are identical; two reviewers suggested collapsing them via
  `pub use crate::types::Algorithm;` in `algo`. Deferred to keep this task's diff
  off the completed `algo-interface` module's type identity during parallel work;
  left behind a documented `#[allow(dead_code)]`. Cheap, low-risk cleanup.
- **Per-command `Report` enum.** A tagged `enum ReportBody { Convert/Inspect/
  Estimate }` would make "field set for the wrong command" unrepresentable
  (type-design review). Deferred as beyond Step-1 MVP; the flat all-`Option`
  shape with `skip_serializing_if` is tested and produces correct per-command JSON.
- **lcms handler** latches on *any* lcms log (can't see severity), so a benign
  recoverable ICC-parse warning during a custom `--output-profile` could fail an
  otherwise-good run — a *loud* false-positive (not a silent-wrong image), kept as
  the fail-safe posture; refine by inspecting error codes if it ever bites.

### Review
- Ran `pr-review-toolkit:review-pr` (code / silent-failure / tests / type-design /
  comments) — 1 full round + 1 confirmation round.
  - **Fixed:** stale `cli.rs` module doc; `--quiet --report none` could hide a
    non-finite fault (now always stderr-echoed); IR-present warning made
    `--strict --export-ir` unusable (now gated); lossy `film_base_source` string
    → structured enum; duplicated explicit-base validation → shared
    `validate_explicit_film_base`; IR export reordered before main encode; lcms
    flag cleared before render; dangling `Reporter::warn` doc link. Added tests:
    determinism, sidecar recipe round-trip via `--params`, exact exit codes
    (1/3/5), `-v` stdout-cleanliness, `--report-file`.
  - **Deferred (above):** Algorithm-enum unification, per-command Report enum,
    lcms severity discrimination. Confirmation re-review came back clean of
    important issues.

- 2026-07-14 — **PR #16 review fixes (data-loss guards).** (1) Every write
  target (`--output`, the sidecar, `--dump-params`, `--report-file`,
  `--export-ir`) is now checked against the input scan and against each other
  before anything is decoded or written — previously `-o <input>` destroyed the
  negative and `--report-file <output>` truncated the just-written TIFF, both
  with exit 0. `encode::sidecar_path` extracted so the CLI can include the
  sidecar in the check. (2) `--input-profile` / recipe `input.color.profile`
  was a silent no-op (parsed, never applied); `convert` now rejects it with
  exit 4 until input-side color management lands (§9 note added). Four new E2E
  tests pin all of it.
- 2026-07-14 — **rebased onto merged #16 and wired the report.** The rebase had
  one trivial conflict (cli.rs import list). The merge-time follow-up landed in
  this branch since orchestration is now underneath it: `stages::render` calls
  `Converter::convert_reported`, `Rendered` carries the `ConvertReport`, and the
  convert JSON report gains an optional `dmax` field (auto/explicit value;
  absent for `--no-d-max` and `simple`). E2E test pins both presence and
  absence.
- 2026-07-14 — **closed out.** Manual review approved; shipped via `/ship`
  (gates re-run green: 110 unit + 13 integration tests; CLAUDE.md refreshed —
  module map, dead-code note, and the lcms2 global-handler mechanism now match
  the implementation; branch rebased onto post-docs main). **Step-1 MVP is
  complete** — Phase 4 closes. Merge-time follow-up recorded for
  `dmax-white-anchor` integration: switch `stages::render` to
  `Converter::convert_reported`, carry `ConvertReport` on `Rendered`, add one
  Option field to `Report`.


## roll-conversion
**Status:** done (implementation; user ships the checkbox)
**Updated:** 2026-07-19

- Goal: add a **roll workflow** — convert a batch of frames from ONE shared,
  frozen recipe so the whole roll is color-consistent and reproducible. This is
  the **batch-apply** half of plan→recipe→apply: it replays a *provided* frozen
  recipe over N frames, independent of how the recipe was produced. The
  auto-cascade that *generates* the recipe is the separate, dependent
  `base-acquisition-planner` task (NOT built here). Extends design-spec §12 item 6.

### What was built
- **New `nc roll` subcommand** (not a mode of `convert`) — see the decision below.
  `RollArgs` in `cli.rs`: positional `inputs` (files / directories / shell globs)
  **or** `--frames <manifest.json>`; required `-o/--out-dir <DIR>`; `--params`
  (the shared frozen recipe); `--strict`; the shared `ReportArgs`. Adds **no new
  recipe keys** — it reuses the existing `ResolvedConfig`/recipe shape; its flags
  are operational (like `--report`/telemetry).
- **Shared per-frame core `convert_frame`.** Extracted the decode → film-base →
  render → optional IR export → encode + sidecar block out of `run_convert` into
  a pure-of-orchestration `convert_frame(command, input, output, &cfg, &log) ->
  ConvertedFrame`. Both `run_convert` (single frame) and `run_roll` (per frame)
  call it, so **a roll frame's output is byte-identical to a single `convert`**
  with the same effective recipe (verified by a test that diffs the bytes).
  `run_convert`'s report emission, `--strict` gate, and telemetry stay in the
  orchestrator (telemetry is `convert`-only). Extracted
  `reject_unsupported_input_color` (the `input.color` profile guard) so both
  commands fail identically on a profile-bearing recipe.
- **Roll report** (`RollReport` on stdout / `--report-file`): `command:"roll"`, the
  shared frozen `recipe` **once** (its roll-fixed `film_base` / `density.dmax` live
  there, not repeated per frame), a `frames[]` list (`FrameReport`: input/output,
  `status` ok|failed, per-frame `film_base`/`dmax`/`white_balance`/`balance_range`/
  `loss`/`warnings`, the applied `overrides`, and `error` on failure), and a
  `summary { total, succeeded, failed }`. Emitted via a new generic `emit_json`
  helper (`emit_report` now wraps it).
- **Per-frame overrides** via the `--frames` manifest: each entry is
  `{ input, output?, params? }` where `params` is a *partial* recipe deep-merged
  (`merge_json`) onto the shared recipe's JSON, then deserialized back with
  `deny_unknown_fields` (so a typo'd override key is a loud error). This is the
  frame-local knob (e.g. per-frame `print.print_exposure`) and the shape
  `base-acquisition-planner` will emit.
- **Naming scheme:** default per-frame output is `<out-dir>/<input-stem>_positive.tiff`
  (sidecar `<...>.tiff.json` as usual); a manifest may set an explicit `output`
  (relative → joined onto out-dir, absolute → verbatim).
- **Determinism & safety:** positional inputs are sorted+deduped and directories
  expand to their sorted `.tif`/`.tiff` files, so frame order is deterministic. All
  per-frame outputs + sidecars (and `--report-file`) are collision-checked against
  every input and against one another up front (`ensure_roll_targets_distinct`,
  the multi-input analogue of `ensure_write_targets_distinct`) — a same-stem
  collision fails loudly (exit 2) before anything is written. `input.export_ir` is
  rejected in roll mode (one path, N frames would overwrite it).

### Key decisions / notes for dependents
- **Subcommand vs mode → new `nc roll` subcommand.** `convert` takes a single
  `input` positional + a single `-o <file>`; batch needs multiple inputs and an
  output *directory* with a naming scheme plus a differently-shaped roll report.
  Overloading `convert` would muddy its contract and risk its byte-identical
  guarantee. A separate subcommand keeps `convert` untouched and lets `roll` own
  its small operational surface while sharing the recipe machinery and the
  `convert_frame` core. **Single-frame `convert` output is unchanged** (all
  pre-existing convert/telemetry integration tests pass verbatim).
- **Config errors vs runtime errors.** A bad *shared* recipe or a bad per-frame
  *override* (bad merge, unsupported knob) fails loudly **up front** (exit 2/4)
  before any frame is converted. A per-frame **runtime** error (unreadable input,
  degenerate base) is **recorded** (`status:"failed"` + `error`) and the roll
  **continues**; the process then exits **1** with a summary on stderr — the report
  (emitted first) carries the per-frame detail. `--strict` promotes any frame's
  warnings to a failing exit after the report is emitted (convert's contract,
  aggregated).
- **Sequential, not parallel.** Frames are converted sequentially for simple,
  deterministic report ordering and logging. Per-frame output is independent, so
  `rayon`-parallelizing the loop is a safe future optimization (output bytes are
  unaffected); left out to keep the scaffold lean.

### Coordination notes
- **`dmax-reference` reconcile (trivial).** That parallel task changes
  `density.dmax` semantics + the default render. Roll treats `density.dmax` exactly
  as it exists on `main` today (it only carries the shared recipe's value through
  to `convert_frame`). When `dmax-reference` merges, the roll frozen-recipe handling
  of `density.dmax` needs no code change — the shared recipe simply carries whatever
  the new semantics define; only the `ROLL_RECIPE` test fixture's explicit
  `{"dmax":{"explicit":1.6}}` may want a value refresh if defaults shift.
- **`base-acquisition-planner` (the dependent).** It owns the **plan** step:
  detect the roll-fixed film base / `Dmax` once and *emit* the frozen recipe (and,
  for per-frame differences, a `--frames` manifest) that this `nc roll` then
  applies. The manifest shape (`{ frames: [{ input, output?, params? }] }`, partial
  `params` deep-merged per frame) is the intended hand-off contract.

### Verification
- CI gate clean in the worktree: `cargo fmt --all --check`, `cargo clippy
  --all-targets -- -D warnings`, `cargo build`, `cargo test` all pass. Suite is
  **305 tests** (252 unit + 53 end-to-end), +13 for this task.
- New end-to-end tests (`tests/pipeline.rs`, driving the compiled binary against
  the committed `tests/fixtures/{hdr-48bit,hdri-64bit}.tif`): batch from a
  hand-authored frozen recipe → per-frame outputs + sidecars + a roll report with
  the shared Dmin/Dmax once; **re-run is byte-identical**; a `--frames` per-frame
  `print_exposure` override applies to **just that frame** (each roll output diffed
  byte-for-byte against the equivalent single `convert`); a missing-input frame is
  recorded `failed` while the good frame still converts and the roll exits 1; a
  same-stem output collision is rejected (exit 2). Unit tests cover `merge_json`,
  the per-frame override merge keeping roll-fixed params, manifest
  `deny_unknown_fields`, output naming, the export-ir rejection, the target
  collision guard, and the roll-report shape.

### Review-fix loop (2026-07-19)
Two-engine review (Codex + 5 pr-review lenses) over the uncommitted changes; all
findings verified and applied (still uncommitted):
- **Roll-fixed film-base invariant now enforced.** (a) When the shared recipe's
  resolved `film_base.source` is not `explicit`, `run_roll` emits a loud
  **roll-level warning** (new `RollReport.warnings`, echoed to stderr, promoted to
  a non-zero exit by `--strict`) explaining the roll is not frozen/color-consistent
  and how to calibrate once — a warning, not a hard error, so best-effort batches
  stay usable. (b) A per-frame manifest override whose `params` sets `film_base`
  is **applied** (the frame converts with its overridden base) with the same loud,
  `--strict`-promotable roll-level warning (`resolve_frames` pushes it into
  `roll_warnings`) — a warn-and-continue, not a reject, per the user's course
  correction that roll-fixed-invariant violations warn rather than fail.
  `density.dmax` overrides stay allowed pending `dmax-reference`.
- **`FrameReport` de-stringified.** The `status:&str` + all-`Option` payload +
  `error` layout became a data-carrying `FrameStatus` enum (`Ok { … }` / `Failed
  { error }`), internally tagged (`#[serde(tag="status")]`) and `#[serde(flatten)]`ed
  so the JSON wire shape is unchanged (`warnings`/`overrides` are common fields).
- **Failed frames keep their warnings.** `convert_frame` now accumulates warnings
  into a caller-owned buffer (`push_warning_buf`); on the `Err` path `run_roll`
  attaches them to the failed frame's report (previously hardcoded empty), so a
  frame that warns then fails still surfaces the warning even under `--quiet`.
- **Manifest subdirectory outputs are created** (`create_dir_all(output.parent())`
  per frame) before encode. Per-frame command label passed as `"roll"`.
- **Docs corrected** (design-spec.md + .html together): the shared recipe *config*
  appears once; each frame additionally echoes its *resolved* base/Dmax (the old
  "not repeated per frame" wording was wrong). Clarified positional inputs
  (directories expand; globs are shell-expanded, not by nc). Added a `merge_json`
  doc note on multi-variant enum overrides being rejected loudly by `from_value`.
- **Tests added** (now driving 21 roll-related tests): directory expansion (sorted,
  non-TIFFs ignored), empty-batch errors on both paths, the not-frozen warning +
  its `--strict` promotion (report still emits, `failed==0`), a two-frame
  determinism diff, per-frame sidecar records the merged recipe, manifest subdir
  output, per-frame `film_base` override warns and `--strict`-promotes (frame still
  converts), a warn-then-fail frame keeping its warning, and a `frame_report_err`
  unit test.

### Review-fix loop — P2 pass (2026-07-21)
Rebased onto `origin/main` (now carries the merged #36 "7 tracked tasks" and #37);
three verified Codex P2 findings applied (all `src/cli.rs`):
- **`merge_json` replaces an externally-tagged enum variant switch instead of
  unioning tags.** A per-frame override that flips a one-key enum map to a
  different variant (real case: shared `film_base.source={"region":…}` + override
  `{"film_base":{"source":{"explicit":…}}}`) was deep-merged key-by-key into a
  two-tag `{"region":…,"explicit":…}` object that no enum deserializes, so a valid
  override became a confusing `from_value` rejection. New `is_variant_switch` guard
  (both sides single-key objects with *different* keys) replaces wholesale; same-tag
  objects still deep-merge (a partial sub-field override keeps its siblings). Unit
  test for the merge both ways + a `resolve_frames` test proving the region→explicit
  override applies **and** still raises the roll-fixed-base warning.
- **The `--frames` manifest is protected from roll write targets.** `run_roll` now
  adds `args.frames` (the manifest path) to the read-input set passed to
  `ensure_roll_targets_distinct`, so `--report-file` (or any output) equal to the
  manifest is rejected (exit 2) before any write. Guard test added.
- **Directory expansion fails loudly on an unreadable entry.** `expand_input`'s
  `read_dir` iteration dropped per-entry `Err`s via `filter_map(Result::ok)` →
  a silently short batch; it now propagates each entry error as a usage error
  (exit 2, same class as failing to open the directory). Happy-path expansion test
  added (a per-entry `read_dir` error is not portably reproducible in a unit test).


## base-acquisition-planner

**Status:** not started
**Updated:** —

- Goal: Implement the automatic **acquisition cascade** that resolves a roll's `Dmin` and `Dmax` from whatever the user provides, emits a **frozen recipe with provenance + confidence**, and decides when to fall back from roll to single conversion.


## conversion-versioning

**Status:** done
**Updated:** 2026-07-28

- Goal: Stamp every conversion with a machine-readable identity and a **behavioral pipeline version**, so outputs are attributable and conversion quality / performance can be compared across versions of `nc`.

### 2026-07-27 — implemented (uncommitted on `feat/conversion-versioning`)

Full CI gate clean: `cargo fmt --all --check`, `cargo clippy --all-targets -D
warnings`, `cargo build`, `cargo test` — **459 tests** (363 unit + 96 end-to-end),
plus **58** stdlib Python tests for `nctool` (40 pre-existing manifest + 18 new
`compare`). This entry continues work a previous agent started and left
mid-flight; what was inherited vs. changed is recorded at the end.

#### The three identity layers (`src/version.rs`, new)

1. **Build identity** — `NC_VERSION` (crate semver), `git_commit`, `git_dirty`,
   `TARGET`. `build.rs` was **extended** (not duplicated) beside the existing
   `NC_TARGET`: it now also emits `NC_GIT_COMMIT` / `NC_GIT_DIRTY`.
2. **`PIPELINE_VERSION: u32 = 1`** — the behavioral label, independent of semver.
3. **`params_hash`** — `stable_hash` (FNV-1a, hand-rolled because
   `DefaultHasher` is explicitly not stable across toolchains) over the canonical
   resolved-recipe JSON.

All three ride an `Identity` struct into `Report.identity` and `RollReport.identity`.

- **`pipeline_version` starts at 1, not 0** — deliberately, and against the task
  file's literal "starts at 0". This epic's own summary already recorded the
  reason: `film-base/dmax-reference` **already changed the default render** (the
  per-frame 99.5th-percentile anchor became the roll-fixed nominal `Dmax = 2.0`),
  so that commit is the v0→v1 boundary. `0` is documented in the
  `PIPELINE_VERSION` history table as the `docs/reports/v0-baseline.md` behavior;
  it predates the constant, so no fingerprint of it can be computed from this tree.
  design-spec §12 item 14's "deferred `pipeline_version` bump" obligation is now
  discharged and the spec says so.
- **`git_commit`/`git_dirty` are omitted, not `"unknown"`**, when the build tree
  had no usable git — a consumer must never mistake a placeholder for a hash, and
  a commit-less build must not claim a clean tree (`build.rs` reports `dirty`
  unknown whenever the commit is unknown).
- **`nc --version`** prints semver + `pipeline_version` (with `PIPELINE_BEHAVIOR`,
  a one-line description of its default render) + commit (`-dirty` marked) +
  target, so an output is attributable without running a conversion.
- **The params hash is ONE implementation, reused — not a second hasher.** Layer 3
  already existed as `telemetry::params_hash` (stable across toolchains, exactly
  the property this task needs). It was **moved**, not duplicated:
  `version::stable_hash` is now the sole implementation and
  `telemetry::params_hash` is a one-line delegation to it. Verified: the FNV
  constants appear at exactly one site in `src/`.
  - **Why the implementation moved to `version.rs` rather than the report calling
    into `telemetry`:** the report is core output and must not depend on the
    telemetry module — telemetry is opt-in, operational, and a candidate for
    future extraction, so pointing the report at it would invert the dependency.
    `version` is the neutral home both can use. The public name
    `telemetry::params_hash` is kept so the record's field and its call site read
    unchanged.
  - Two independent hashes of the same recipe that could disagree is exactly what
    `core/dependency-hygiene` exists to remove, so this keeps one source of truth:
    a record's `params_hash` and a report's `identity.params_hash` are now the same
    function over the same bytes and cannot drift apart.
  - **The pre-existing tests still pass, unmodified:**
    `telemetry::tests::params_hash_is_stable_and_input_sensitive`,
    `telemetry_records_sigmoid_curve_and_params_hash`, and
    `telemetry_params_hash_matches_identical_conversions` — the last two were the
    model for this task's own `params_hash_is_the_hash_of_the_dump_params_bytes`.
  - The **one** other FNV site is `fnv1a_hex` in `tests/pipeline.rs`: a deliberate
    independent reimplementation in the *integration* suite (which cannot link the
    binary crate's internals). It is not a second production hasher and cannot
    disagree silently — a disagreement is the test failing, which is its purpose:
    it pins the wire value from outside the code under test.

#### The sidecar envelope (the round-trip trap)

The sidecar is now `{ "meta": {…identity…}, "params": {…recipe…} }`. Identity as
**bare recipe keys** was never an option: every recipe struct is
`deny_unknown_fields`, so each new sidecar would have failed to reload through
`--params`. `load_recipe` accepts **both** shapes, told apart by a top-level
`params` key (which is not, and must never become, a recipe key):

- `meta` is deserialized as a raw `Value` on purpose — it is provenance, so an
  **older** build must tolerate a **newer** build's extra `meta` fields, and
  nothing in it may influence a conversion. The `params` body keeps its full
  `deny_unknown_fields` strictness.
- A document with `meta` and no `params` is a **malformed envelope**, not a bare
  recipe: it gets a pointed error instead of serde's opaque
  `unknown field 'meta'`. A third sibling key is rejected too.
- Bare recipes still parse straight from the file **text**, so their
  line/column-bearing serde diagnostics are unchanged.
- `canonical_params_json` is the single producer of the sidecar body, of
  `--dump-params`, and of the `params_hash` input — so the advertised hash is
  always the hash of a document an agent can reproduce
  (`stable_hash(--dump-params bytes) == identity.params_hash`, pinned by a test).
- **Replaying a recipe from another `pipeline_version`** (`meta.pipeline_version`
  ≠ this build's) is a loud, `--strict`-promotable warning on `convert` and a
  roll-level warning on `roll`: the parameters still apply, but the default render
  changed underneath them. Surfacing that mismatch is the whole point of the label.

#### The golden drift gate — and why it is safe on both targets

**This was the biggest risk in the task and it is worth reading before touching
it.** The requirement is "changing a default output without bumping
`pipeline_version` fails a golden test". The naive implementation — checksum the
encoded TIFF, or hash reconstruct output over a full frame — **passes locally and
fails CI**: transcendental FP (`powf`/`10^`/`log10`) differs ~1 ULP across libm
implementations, and the lcms2 transform plus embedded ICC bytes differ by target
(CLAUDE.md; design-spec §8 scopes byte-identity to one build/architecture).

The gate (`version::PIPELINE_FINGERPRINTS` + `mod drift_gate`) instead hashes
**two small, provably platform-independent things**, keyed by `PIPELINE_VERSION`:

- `render` — the default-path result of the **curated per-pixel vectors** in
  `pipeline::stages::golden` (`Reconstruction::default()` + `PrintParams::default()`
  over `golden::pixels()` / `golden::base()`): every output pixel's `f32` bit
  pattern plus the resolved `Dmax`/WB/balance-range diagnostics, rendered as hex
  text. Safe because these are **exactly** the values
  `golden_density_exponential_default_is_bit_identical` already pins as literals
  and CI already proves agree on both targets — hashing them adds a version label
  without widening the numeric surface by one value. It stops at
  `reconstruct_and_print`, i.e. **before** the lcms2 transform, so no post-lcms2
  pixel and no ICC byte enters it. It is not a whole-frame or whole-file checksum.
- `recipe` — the canonical JSON of `ResolvedConfig::default()`. Serde-generated
  **text**, hashed with pure integer arithmetic; no FP, no platform dependency.
  This covers default changes the curated vectors cannot see because they live
  outside stages 3–4 (`output.depth`, `output.profile`, `film_base.source`, input
  defaults).
- To keep one definition of the vectors, `stages::golden` (and its `pixels()` /
  `base()`) became test-only `pub(crate)` rather than being copied.
- **Failure messages are the interface.** They print the computed fingerprint so
  the fix is a copy-paste, and they distinguish the cases: a changed `render` is a
  version bump; a changed `recipe` is a bump *if a default value moved*, but if you
  only **added an opt-in knob** with a neutral default, no default pixel moved —
  update the `recipe` fingerprint **without** bumping and note why here. That false
  positive is a deliberate trade: an allowlist of "behavior-bearing" keys would
  silently stop covering whatever key nobody remembered to add.
- Bumping the version is **not free**: a version with no recorded row fails with
  "add this row", because a bump with no fingerprint leaves the new version
  undefended against the next silent change.
- A sibling test perturbs one default by `f32::EPSILON` and asserts the
  fingerprint moves, so the gate is proven to actually detect what it claims to.

#### Comparison basis in the report, not a second pixel path

`io::encode` now returns `EncodeOutcome { loss, stats }` (`types.rs`), adding
`OutputStats { mean: [f64; 3] }` — the per-channel mean of the samples **as
written**, from the encoder's existing single pass. Report-only, like `loss`.

Only the mean is recorded, deliberately: for a fixed scan + recipe, per-channel
mean ΔRGB between two builds is exactly the difference of the two means
(`mean(a) − mean(b) = mean(a − b)`), so `compare` derives the metric from two run
records **without ever re-reading, registering, or shipping pixels** — the
`analysis` epic's "only derived numbers leave the tools" rule holds by
construction. The u16 path accumulates in **integer** `u64` over the written
values (exactly reproducible given identical pixels); the f32 path sums finite
samples only, so one `NaN` cannot poison a channel mean (the fault itself is still
reported by `EncodeReport::non_finite`).

#### Comparison harness (`scripts/analysis/`)

- **`benchmark.json`** — the fixed benchmark manifest, two sets. `fixtures` (the
  committed 502×462 decoder fixtures; needs no Drive assets, so `compare` is
  runnable and unit-testable on any checkout) and `rolls` (six real exposed Ektar
  + Phoenix frames — the rolls `v0-baseline.md` was measured on — each with its
  roll's frozen recipe from `scripts/real-scan-verify/recipes/`).
- **`nctool/compare.py`** — `python -m nctool compare run|diff`. `run` converts a
  set with one `nc` binary and writes a run record (build identity + per frame:
  `params_hash`, channel means, clip fraction, per-stage timings). `diff` emits a
  report keyed on **both** identities with per-channel mean ΔRGB, clip-fraction
  delta, and timing delta per frame.
- **Asset identity comes from the existing `manifest.json`, not a second
  inventory.** A `rolls` case names `roll` + frame stem; path and `sha256` are
  resolved through `<asset-root>/manifest.json`, and a checksum mismatch fails
  loudly (comparing two builds over silently different input bytes would blame the
  code for an asset change). Timings come from the **telemetry** record via
  `--telemetry-file` — reused, not reimplemented.
- **"Zero diff" covers the deterministic fields only** (`params_hash`, means, clip
  counts). Wall-clock timings are informational and reported separately: measured
  on the real-scan set they varied **±1.4 s** between two runs of one build, so
  folding them into the verdict would make a zero diff impossible. A **same
  build** producing a non-zero diff exits 1 — that breaks determinism and is the
  loudest thing this harness can find.
- A build that reports no `identity`/`output_stats` (anything predating this task)
  is **refused loudly** rather than recorded as nulls that would diff to "no
  change" — verified against a real binary built from `main`.

#### Verified

- **No output pixel changed.** Built the unmodified baseline (a throwaway worktree
  at `HEAD` = `0d05c80`) and diffed **10** outputs against the new binary,
  byte-for-byte identical across: the default u16 render, a region-estimated base,
  f32 HDR, sigmoid, `simple`, auto-WB + `--auto-d-max` + full print knobs, the
  exported IR plane, and both frames of a two-frame roll. An e2e test pins the
  invariant going forward by driving one conversion through every identity path
  (report on/off, bare recipe, enveloped sidecar, version-skew warning) and
  asserting one set of TIFF bytes.
- **`build.rs` survives a missing git.** `rsync`'d the working tree (uncommitted
  changes included) with no `.git` into `/private/tmp` and built it: build
  succeeds, `nc --version` prints `commit: unknown`, the report **omits**
  `git_commit`/`git_dirty` entirely, and the output TIFF is byte-identical to the
  git-ful build's. A missing `git` binary takes the same `Command::output().ok()?`
  branch.
- **Real-scan comparison runs.** `compare run --set rolls` over the six full-size
  frames (~37 s, checksums verified against the asset manifest) then a second run
  diffed to `identical: true`, all mean ΔRGB `[0,0,0]`. Its clip fractions
  (6.9–10.3%) independently reproduce the `analysis` epic's documented
  "u16 clips 4.8–10.3% high by default".

#### Notes for dependents

- **`output/presets` depends on this task.** Activating named presets **changes
  default pixels**, so it must cross a `pipeline_version` boundary: bump
  `PIPELINE_VERSION` to 2, add a history-table row, refresh `PIPELINE_BEHAVIOR`,
  and record a new `PIPELINE_FINGERPRINTS` row. The drift gate will fail first and
  print the exact values to paste — treat that failure as the checklist.
- **Nothing here is a conversion knob**, so **no new recipe key and no `merge`
  arm exists or should exist**. Identity, `output_stats`, and `compare` are
  operational in the documented `--report`/telemetry sense: CLI-only, never in
  `ResolvedConfig`, never affecting output bytes. An e2e test asserts each identity
  field placed bare in a recipe is a loud exit-2 unknown-key error, so the
  `deny_unknown_fields` guarantee is pinned in the direction that matters.
- **If you add an opt-in knob**, the `recipe` fingerprint will trip. That is
  expected; update it without bumping `PIPELINE_VERSION` and say so here.

#### Left undone

- The task is **not** marked `[x]` and nothing is committed — the user ships it.
- `docs/reports/v0-baseline.md` is left as the historical `v0` record and was
  **not** rewritten; it already names `pipeline_version 0`. A `v1` baseline report
  measured with `nctool compare` is the natural follow-up but was not in scope.
- Quality metrics beyond mean ΔRGB (ΔE2000, SSIM) are deliberately out of scope —
  design-spec §12 item 7's QA harness.
- The Python tests are not run by CI (`.github/workflows/ci.yml` is Rust-only, as
  it already was for `manifest`). Run them with
  `cd scripts/analysis && python3 -m unittest discover -s nctool -t .`.

#### Inherited vs. changed

A previous agent was killed mid-flight by an unrelated billing limit, leaving
uncommitted work. It **did not compile** (`run_roll` did not destructure
`LoadedRecipe`'s new field) and had **no docs and no tests** for any of it.

- **Kept, essentially as written** (it was good, and matched the task file):
  `src/version.rs`'s identity types and `stable_hash`; the `build.rs` extension
  including its fail-soft contract and its documented `rerun-if-changed`
  trade-off; the `telemetry::params_hash` delegation; `OutputStats` /
  `EncodeOutcome` and the encoder's channel means; the report/sidecar envelope
  design in `cli.rs` (`SidecarEnvelope`, `split_envelope`,
  `canonical_params_json`). Its choice of `PIPELINE_VERSION = 1` over the task
  file's "0" was verified against this epic's summary and kept.
- **Fixed/completed:** the compile error; `pipeline_version_warning` was written
  but **never called** (dead code that would have failed clippy) — now wired into
  both `run_convert` and `run_roll`; `RollReport` gained the missing `identity`.
- **Written from scratch (none of it existed):** the whole drift gate — its
  `version.rs` doc comment referenced a test
  (`default_render_fingerprint_matches_the_pipeline_version` in
  `stages::golden`) that **did not exist**, so the task's central requirement was
  unimplemented; the `pub(crate)` exposure of the golden vectors; every test
  (11 e2e + 4 `cli` unit + 3 drift-gate + 18 Python); the entire comparison
  harness and benchmark manifest; and all documentation.

### 2026-07-27 (later) — review-round fixes (still uncommitted)

Six independent review engines went over the uncommitted diff. Everything below is
a verified finding fixed in place. Gates after: `cargo fmt --all --check`,
`cargo clippy --all-targets -D warnings`, `cargo build`, `cargo test` — **470
tests** (368 unit + 102 end-to-end) — plus **80** stdlib Python tests
(`cd scripts/analysis && python3 -m unittest discover -s nctool -t .`; pytest is not
installed here). `cargo doc --no-deps` warning count is **unchanged at 8**, all
pre-existing on `main`; the two this diff had introduced in `version.rs` are gone.

#### The drift gate did not cover the default film-base stage

The largest correctness hole. `render_fingerprint_text` calls
`reconstruct_and_print` with a **hardcoded** base, while `FilmBaseParams.source`
defaults to `auto` — so `film_base::estimate` ran over real pixels on every default
conversion with **no fingerprint touching it**, and the `recipe` hash saw only the
string `"auto"`. Not hypothetical: `5b22b6f` (auto film-base redesign, 2026-07-16)
landed one day after the `v0` baseline report, in exactly that stage. Worse, a
`compare diff` across two such builds would show non-zero ΔRGB under the *same*
identity, and the harness would then blame **nondeterminism**.

- Added a third fingerprint, `base`: `stable_hash` over `film_base::estimate`'s
  result for `FilmBaseParams::default()` over a new **frozen** synthetic scan,
  `pipeline::film_base::golden::scan()`.
- **It is cross-platform safe for a stronger reason than `render` is:** `film_base`
  contains **no transcendental at all** — no `powf`/`10^`/`log10`/`exp`/`sqrt` —
  only integer indexing, IEEE `+ - * /`, comparisons, and a nearest-rank order
  statistic whose result is one of the input values bit-for-bit. The ~1-ULP libm
  divergence that rules out a whole-frame reconstruct hash has nothing to act on.
- The frozen band is deliberately **not flat**: a uniform band returns the same
  value for *any* percentile, so retuning `SAMPLE_PERCENTILE` — one of the changes
  this fingerprint exists to catch — would have left the hash unmoved. A 7%
  along-edge ripple (ten levels, inside `MAX_RELATIVE_SPREAD` and
  `STRIP_CONTINUITY_TOL`) puts p90 and p97 on different levels;
  `the_frozen_drift_gate_scan_resolves_cleanly_and_is_percentile_sensitive` pins
  both that and the clean (warning-free) resolution, so the hash can never
  degenerate into fingerprinting a failure path.
- `golden::scan()` is self-contained rather than reusing
  `tests::scan_with_rebate`, which is a *parameterized* helper free to evolve.

**What the gate now covers, exactly:** stage 2 `film_base::estimate` on the `auto`
default (`base`), stages 3–4 `reconstruct_and_print` on the curated vectors
(`render`), and the default recipe **values** (`recipe`). **What it still does
not:** stage 1 `io::decode`; stage 1b `input_semantics::resolve`; the lcms2 output
transform and embedded ICC bytes (excluded *deliberately* — target-dependent, so no
cross-platform hash exists); `io::encode` quantization/clip accounting; the
`Region`/`Explicit`/grid film-base sources; and the auto detector's behavior on
**real** rebate geometry (one frozen synthetic layout catches a retuned constant,
not every regression). A change confined to those can still move default output with
every test green — `scripts/real-scan-verify/` and `nctool compare` are the other
half. The three places that overstated this (`version.rs`, design-spec §9, this
epic's summary) now say so.

#### The test that proved the gate works could not fail

`the_fingerprint_gate_actually_detects_a_changed_default` compared
`stable_hash(bits.join(","))` against `stable_hash(render_fingerprint_text())` — two
differently-*shaped* strings, so `assert_ne!` held for **any** input, perturbation or
not. It was the only executing evidence for the "changing a default output without
bumping fails" verify bullet.

`render_fingerprint_text` is now parameterized on `(&Reconstruction, &PrintParams)`
and `base_fingerprint_text` on `&FilmBaseParams`, so the test hashes **like-shaped**
text and compares it against the **recorded row**. Three perturbations, one per
fingerprint: print (`print_exposure + f32::EPSILON`), reconstruction
(`ExponentialParams { gamma: 1.05 }` — a print-only nudge left the density curve
unproven), and film base (an explicit source). It then re-asserts that the
*unperturbed* defaults **do** match the row, so a shape mismatch can never be
mistaken for detection.

#### `PIPELINE_BEHAVIOR` is now paired to the version by a gate

Bump the version, record fingerprints, forget the `PIPELINE_BEHAVIOR` line, and
`nc --version` described the *previous* render with every test green.
`PipelineFingerprint` gained `behavior`, and two assertions close it: the current
version's row must carry *this* `PIPELINE_BEHAVIOR`, and no two rows may share a
behavior string (so copy-pasting the old row's description fails instead).

**Deviation from the review's suggestion, deliberately.** It asked for
`PIPELINE_BEHAVIOR` to become a *lookup* into the table. That would force the table
out of `cfg(test)` and into the shipped binary, where `render`/`base`/`recipe` are
never read — a `dead_code` allow with no named consumer, which CLAUDE.md forbids. A
`const fn` lookup would be worse still: a missing row would be a **compile error**,
so you could not build the binary to run the test that prints the fingerprints to
paste. The two assertions get the same guarantee with neither cost. Also fixed here:
`version.rs`'s two non-test doc comments linked `[PIPELINE_FINGERPRINTS]` /
`[PipelineFingerprint]`, which are `cfg(test)`-only, so `cargo doc` warned.

`PIPELINE_BEHAVIOR` also now names the **film-base source** — without it two builds
differing only in the base algorithm read as the same behavior string.

#### The v1 history row was factually wrong

"everything else unchanged" was false: `5b22b6f` (auto film-base redesign) and `#43`
(input semantics) also changed default behavior after the `v0` baseline. The row now
states that **v1 collapses three default changes into one label**, with the fair
framing: `v0-baseline.md`'s own numbers used an explicit `--film-base`, so *those
numbers* stay comparable, but the *default* render crossed three boundaries with
only one label available to record them. Corrected in `version.rs`, design-spec §9
and §12 item 16, and this epic's summary. `Dmax = 2.0` now carries its **density**
units everywhere in this diff.

#### `compare` reported verdicts on records it never compared

Measured before the fix: `compare diff` on two `{}` documents printed
`"identical": true` at **exit 0**, as did a `schema_version` of 99 against 1, and as
did re-diffing a *diff report*. `cmd_diff` validated only `benchmark_set` equality,
then `diff_frames` compared `None != None` across all of `DETERMINISTIC` and
concluded identity. The realistic path: CI writes `compare diff … > report.json` and
a later step or path typo re-diffs that file — green, having compared nothing.

`validate_record` now refuses a wrong/absent `schema_version`, a missing
`benchmark_set`, an empty `frames` list, and any frame missing any field the verdict
reads (a `mean` that is not three numbers included). Measured after: exit **2** with
`schema_version is None, not 1 — … (a diff report is not a run record)`.

#### `compare` called ordinary iteration a broken determinism contract

`same_build = a["identity"] == b["identity"]` — but two builds from **different
uncommitted trees at the same commit** produce identical identity dicts
(`git_dirty: true`, same commit/version/target). The harness printed *"the SAME build
produced a non-zero diff — the pipeline is not deterministic"* and exited 1, on the
most likely first use of the tool. `pins_source()` now gates that assertion on an
identity that actually pins the source (`git_commit` present **and**
`git_dirty == false`); otherwise it prints a note explaining why the two records are
indistinguishable and exits 0. Measured: two `dirty=true` records at one commit →
exit 0 with the note; the same pair with `dirty=false` → exit 1 "not deterministic".

#### `{"params": []}` converted with all-default parameters

serde's derived visitor accepts a *sequence* for a struct and every `ResolvedConfig`
field has a default, so `{"params": []}` exited 0 with
`params_hash = 3575c9feb5d42b2b` — **byte-identical to the default recipe's hash**,
with no warning. A truncated or mis-generated sidecar silently converted with
defaults instead of the recipe the operator believed was applied, defeating the
round-trip contract the envelope exists to keep. `split_envelope` now requires
`params` to be an **object**, and `load_recipe` requires the whole document to be one
(the pre-existing bare `[]` hole, fixed in the same pass — it was two lines). Both
measured at exit **2** with a message naming what was found.

#### `build.rs` could stamp an unrelated repository

`git rev-parse` walks **up** from `CARGO_MANIFEST_DIR`, and nothing checked the
repository it found was nc's. For a feature whose entire purpose is attribution, this
was the worst failure mode in the diff. `is_nc_repository` now requires
`rev-parse --show-toplevel` (canonicalized) to be the package directory itself.

Verified end to end: `rsync`'d this tree (minus `.git`) into a subdirectory of an
unrelated `git init` repo and built it. Before, `git rev-parse --short=12 HEAD` there
returns `6dbef1ccabdc` — the *outer* repo's commit, which the old code would have
stamped as this build's provenance. After: `nc --version` prints `commit: unknown`
and the report **omits** `git_commit`/`git_dirty` entirely. (A future workspace layout
placing this crate in a subdirectory would trip the same check and degrade to
`unknown`; noted in `build.rs`.)

#### Identity went stale exactly when you commit

The script printed no `rerun-if-changed`, and the doc blamed only "a commit that
touches no package file (e.g. `--amend`)". But **no** commit touches package files:
the ordinary edit → build → test → commit → use-the-binary path left the binary
reporting the **parent** commit *and* `dirty: true` on a now-clean tree — and
`dirty: true` is the same value the pre-commit build emitted, so nothing
distinguished them.

**The review's suggested fix was factually wrong for our normal environment, and I
implemented a different one.** It said to name `.git/HEAD` + `.git/index`, asserting
"both are real files in a worktree; only `.git` itself is a file". Measured in this
very worktree: `.git` **is** a file (a `gitdir:` pointer), so `test -e .git/HEAD` is
**NO** — the path does not exist, and every feature worktree we develop in is a
linked worktree. Instead `build.rs` asks git: `rev-parse --git-path <name>` resolves
to the real files under `…/main/.git/worktrees/<name>/` and works in both layouts.
Since naming any path disables Cargo's default any-package-file rule, the source
directories are named explicitly (`src`, `tests`, `docs`, `scripts`, `.github`,
`build.rs`, `Cargo.toml`, `Cargo.lock`) — deliberately *not* the package root, whose
recursive scan would sweep in `target/` and re-run on every build.

**A second trap, found only by testing the real scenario rather than assuming:**
watching `HEAD` does **not** notice a commit. On a branch, `HEAD` is a
`ref: refs/heads/<branch>` pointer whose *contents never change* when you commit —
the branch ref does. My first attempt watched `HEAD` + `index` and a throwaway repo
proved it *still* reported the parent commit after `git commit` + a plain
`cargo build`. So `refs` (scanned recursively by Cargo, catching loose-ref writes)
and `packed-refs` (catching a gc that packs them) are watched too; `HEAD` still
matters for a detached checkout, where it holds the hash directly.

Verified in a throwaway repo whose root is this crate: commit → plain `cargo build`
(no `cargo clean`) → `nc --version` reports the **new** commit, and the full
edit → build (`<hash>-dirty`) → commit → build (clean `<new hash>`) path works —
exactly the case the old doc dismissed as an `--amend` corner. A no-op build stays
`Finished` in ~0.1 s. Residual and documented: a **new untracked file** outside the
named directories can leave `git_dirty` stale until the next watched-path change.

#### Identity was missing from `inspect`, `estimate`, and roll frames

`nc inspect` emitted no `identity` key at all, though the task file says "Every
**report** carries…" and design-spec §9 says "`identity`, every report" (scoping only
`params_hash` to convert/roll). Both now stamp `Identity::new()` — which also
resolves the coupled type question the review flagged: `Identity::new()` had **no
production caller**, so `params_hash: None` was a construction artifact while the
schema promised the state existed. Option (a) taken: stamp it, making `None` real.
`params_hash` stays `Option<String>` and §9 is unchanged.
`inspect_and_estimate_carry_build_identity_without_a_params_hash` pins that
`params_hash` is **omitted**, not null.

`FrameStatus::Ok` gained `output_stats` and `identity` — placed inside the `Ok`
variant beside `loss`, since neither is meaningful for a failed frame.
`roll_frames_carry_their_own_identity_and_comparison_basis` pins that an
un-overridden frame's hash equals the roll's shared hash while an overridden frame's
**differs**, and that each frame's sidecar `meta` agrees with its report entry.

#### `meta.pipeline_version` could silently disable the skew warning

`.and_then(Value::as_u64).map(|v| v as u32)` had two holes: `4294967297` truncated
to `1`, *matching* this build and suppressing the warning by pretending to agree;
and `1.0` / `"1"` / a negative all yielded `None`, indistinguishable from an absent
field, so **no skew check ran at all**. A sidecar round-tripped through a tool that
emits `1.0` would replay on a later build and silently produce different pixels.
`meta_pipeline_version` now uses `u32::try_from` and makes present-but-unreadable a
**loud usage error**. Measured after: `1.0`, `"1"`, `-1`, `null`, `4294967297` all
exit **2** naming `meta.pipeline_version`; `1` and `9999` exit 0, the latter with the
skew warning. Pinned by a `cli` unit test over all five bad forms plus an e2e test.

#### The input fingerprint the checksum guarantee needed

`--skip-checksums` never entered the record and frames carried `input` only as a
basename, so the guarantee the checksum exists for — *"comparing two builds over
silently different input bytes would attribute an asset change to the code"* — was
unverifiable from the artifacts. Frames now carry `input_sha256` plus
`checksums: verified | computed | skipped`:

- `verified` — hashed and matched the asset manifest's digest;
- `computed` — hashed, but the set has no recorded expectation. This is the
  `fixtures` (repo-rooted) case the review asked about: it passed
  `expect_sha256 = None` and so was **never checksummed either way**. Now it is
  always hashed, so `diff` can still prove both builds read the same bytes;
- `skipped` — `--skip-checksums`, which also prints a warning at `run` time.

`diff` compares the digests and **refuses** the comparison (exit 2) when they
differ — different input bytes are not a build comparison at all — and surfaces
`checksums_skipped` in both the report and a stderr note.

#### Everything else, with its pinning test

1. **`version.rs` asserted a false invariant.** `git_commit.is_some() == git_dirty.is_some()`
   would have **failed CI** on a machine with a readable `HEAD` and an unreadable
   index — a state `build.rs` deliberately allows and documents. Now one-directional
   (`git_dirty.is_some() ⇒ git_commit.is_some()`), in
   `a_known_commit_may_have_unknown_cleanliness_but_not_the_reverse`. In that state
   `nc --version` printed a bare hash indistinguishable from clean; it now prints
   `commit: <hash> (dirty unknown)`, and `version_string_names_every_identity_axis`
   asserts a dirty tree is marked.
2. **"byte-identical to `--dump-params`" was false for the sidecar body** — nesting
   indents it two extra spaces. Corrected at `version.rs` (module doc and
   `recipe_fingerprint_text`), `cli::canonical_params_json`, design-spec §9, and the
   `perf-telemetry` skill: the hash is reproducible from a `--dump-params` **file**,
   and the sidecar body is the same *document* compared as parsed JSON. Also noted
   that `nc params` adds a trailing newline (`println!`) while `--dump-params`
   (`write_json`) does not, so hash the file, not the stdout.
3. **`output_stats` had no design-spec §9 entry** despite being the whole comparison
   basis (`compare` hard-fails without it) and had **no test at all**. §9 now
   documents it, including the depth-dependent units. Pinned by
   `output_stats_report_the_written_samples_for_both_depths`: three finite numbers in
   `[0,1]` for u16, present for `--output-hdr`, and a saturated render
   (`--print-exposure 40`) tying `loss.clipped_high > 0` to a mean near display
   white.
4. **A recorded row is history.** The `render` and `base` failure messages now say
   **never edit an existing row in place** (one version labelling two behaviors
   makes every output already stamped with it unattributable), and name `recipe` as
   the one field the table sanctions editing.
   `the_table_records_exactly_the_shipped_versions` asserts a row exists for every
   version in `1..=PIPELINE_VERSION`, none above, none duplicated, and no two
   sharing a behavior string.
5. **The pre-versioning refusal checked blocks, not fields.**
   `output_stats: {"mean": null}`, a **missing `loss` block**, and
   `{"identity": {"nc_version": "0.1.0"}}` all passed, after which
   `loss.get("clipped_low", 0)` defaulted three of five `DETERMINISTIC` fields to
   `0` — making the clip half of the verdict trivially equal instead of loudly
   missing. `_report_gaps` now checks every field the verdict reads (seven hollow
   shapes covered by `test_a_present_but_hollow_report_is_refused_field_by_field`).
6. **`compare` now refuses/annotates incomparable pairs.** `mean` is written by
   `channel_means_u16` (quantized `[0,1]`) or `channel_means_f32` (verbatim,
   unclamped) depending on `output.hdr`, so a u16-vs-f32 compare reported a *unit*
   change as a rendering regression. The record now carries `output_hdr` per frame;
   `diff` sets `output_depth_changed` and **withholds** `mean_delta_rgb` rather than
   computing nonsense, and adds top-level `target_changed` /
   `pipeline_version_changed` / `checksums_skipped` plus a `notes[]` array echoed to
   stderr.
7. **A stale `--out` survived a failed `compare run`** — now written to a temp file in
   the same directory and `os.replace`d on success, with a loud message on `OSError`
   instead of a traceback. `test_a_failed_run_leaves_no_partial_out_file` asserts no
   record and no `.tmp.` leftover.
8. **`compare.py` error hygiene.** Bare `KeyError` tracebacks for
   `case["roll"]`/`["frame"]`/`["input"]` are now messages (`_need`);
   `except (OSError, json.JSONDecodeError)` gained `UnicodeDecodeError`, so diffing a
   TIFF is a message rather than a traceback; an explicit `null` count no longer
   raises `TypeError` and **exits 1** — the same code as "not deterministic", which
   made a malformed record read as a determinism failure — but exits 2 through
   `validate_record`. `load_json` also refuses a non-object document. The exit-code
   convention is now documented in the module docstring, including *why* it inverts
   the sibling `manifest` convention recorded at `progress/analysis.md`: a
   *discrepancy* between two different builds is this tool's normal answer, so `0`
   covers both verdicts, `1` is reserved for a comparison that failed or proved a
   broken invariant, and `2` for operational/usage.
9. **`params` is pinned as a reserved key.** `params_and_meta_are_not_recipe_keys`
   asserts `to_value(ResolvedConfig::default())`'s top-level keys contain neither
   `params` nor `meta`, and an e2e case pins that a `params` key **inside** the
   recipe body is a loud exit 2. *Deviation:* the review asked for `"params": {}` to
   be added to `identity_fields_are_not_recipe_keys` as an exit-2 case, but a bare
   `{"params": {}}` is by design a *valid empty envelope* — asserting exit 2 on it
   would contradict the envelope contract. The nested form is the meaningful pin.
10. **Tautological tests repaired.** `identity_carries_the_build_and_behavior_labels`
    re-stated constants `Identity::new()` had just copied; it is now
    `identity_serializes_the_wire_shape_and_omits_unknown_git_facts`, asserting on the
    **serialized JSON** (the actual contract) and that a git-less build's object is
    exactly `nc_version`/`pipeline_version`/`target` with no `null`s.
    `params_hash_rides_in_the_identity_block` likewise asserts through serialization.
    `stable_hash("{}") == stable_hash("{}")` deleted.
    `canonical_params_json_is_the_dump_params_document` (which asserted the
    function's own body) became a **round-trip** test: the canonical document must
    reload to an identical `ResolvedConfig`. `tests/pipeline.rs`'s "pipeline_version
    is an integer" now cross-checks the report against `nc --version`. The
    exit-code-only strict-body test gained `contains("print_exposur")` and
    `--film-base`, so the only difference from the accepted case is the typo.
11. **New tests** for the roll skew path (`roll_warns_about_a_version_skewed_shared_recipe`
    — distinct wiring from `convert`'s, previously untested) and for `cmd_run`, which
    the Python suite did not touch at all: checksum-drift abort, non-executable `nc`,
    cross-case identity disagreement, the `fixtures` repo-root branch, and
    `--skip-checksums` recording.
12. **Comment/doc corrections.** `types.rs` claimed the encoder's "single pass over
    the output data yields both" — the means are a **second** pass, paid
    unconditionally including under `--report none`; the doc now says so and explains
    why it is not made conditional (it would push report mode down into
    `io::encode`). `cli.rs` claimed the skew warning "lands in the report even if the
    frame then fails" — it does not: `convert_frame(…)?` propagates and
    `emit_report` is never reached, so on `convert` stderr is the only place it
    appears (`roll` genuinely does keep it).

#### Disclosed deviations from the task file

- **`pipeline_version` starts at 1, not 0** — already disclosed in the previous
  entry, and the reason (the v0→v1 boundary already happened) stands.
- **"bumping it passes" (task file, How to Verify) is not what happens.** Bumping
  `PIPELINE_VERSION` alone **panics** until a `PIPELINE_FINGERPRINTS` row is added.
  This is deliberate and is the stronger contract: a bump with no recorded
  fingerprint would leave the new version undefended against the next silent
  change. The failure message prints the exact row to paste, so the bump is one
  copy-paste, not a puzzle.

#### Deferred — recorded, not implemented

- `DETERMINISTIC` is a hardcoded allowlist and the record schema is otherwise
  unpinned — the same failure mode `version.rs` argues against for the `recipe`
  fingerprint, in the opposite direction.
- `identical: true` cannot see a pixel **permutation** or an exactly-compensating
  change: it is a signed per-channel mean. The field name overstates the
  measurement; the module docstring now says so explicitly, but the name is
  unchanged.
- `params_hash` identifies the **requested** config, not the resolved values (`auto`
  base, `percentile` WB), so `params_hash_changed: false` ≠ same effective params.
  `--output-profile <path>` also puts a machine-local path into the hash.
- `git status --porcelain` counts **untracked** files, so a stray scratch file pins
  `dirty: true` permanently and desensitizes the flag `pins_source` leans on.
- `tests/pipeline.rs`'s `report_carries_every_identity_layer` panics unless the tree
  is a git checkout, so `cargo test` fails from a source tarball (the *build*
  degrades gracefully; the test does not).

#### Recommendation for the user (a gate change, so not made here)

**The Python suite runs in no CI gate.** `.github/workflows/ci.yml` is Rust-only, so
`compare.py` — the tool that decides whether two builds differ — can rot silently,
as `manifest` already could. Adding a `python3 -m unittest discover` step would fix
it, but it changes the gate for everyone, so it is left as an explicit
recommendation rather than done unilaterally.

### 2026-07-27 (round 2) — `compare.py` validation hardening (still uncommitted)

A delta re-review found four gaps, all in the record-validation layer added earlier
the same day, all reproduced before fixing. **Rust side untouched this round** — no
output pixel, no `params_hash`, no report field changed. Python suite: **86 tests**
(was 80); all four Rust gates still green.

The common shape: `validate_record` checked that fields were *present* and
right-*shaped*, but `diff_frames` then depended on properties it never asserted —
name uniqueness, numeric types, and a depth marker whose *absence* compares equal to
absence.

1. **Duplicate frame names silently dropped a measurement.** `diff_frames` indexes
   `{f["name"]: f for f in frames}`, which keeps only the **last** entry. Measured:
   two records holding two frames each, where the *last* duplicate agreed and the
   first did not, produced `identical: true`, **rc 0**, `frames_compared: 1` — the
   disagreement was never looked at. `validate_record` now rejects duplicates, and
   `resolve_cases` rejects duplicate **case names** at `run` time too, where the fix
   is a one-line manifest edit rather than a wasted conversion. Pinned by
   `test_duplicate_frame_names_are_refused` and
   `test_duplicate_case_names_are_refused_at_resolve_time`.
2. **`output_hdr` was not required, defeating its own purpose.** The marker exists so
   a u16 mean (quantized `[0,1]`) is never subtracted from an f32 mean (verbatim,
   unclamped) — but a *missing* marker equals a missing marker, so two records that
   both omitted it got `output_depth_changed: False` and had their means compared.
   Measured: u16 `0.5` vs f32 `2.0` diffed as a rendering delta; and with coinciding
   numbers, `identical: true` at rc 0. It is now in `FRAME_FIELDS` and must be a
   **bool** — refusing an older record is the only safe reading of "the units are
   unknown". `test_a_record_without_the_depth_marker_is_refused`.
3. **Numeric values were never validated, only counted.** The three-element `mean`
   check accepted any members, so `mean: ["x", 0, 0]` reached `round(y - x, 12)` and
   raised an uncaught **`TypeError` traceback exiting 1** — this tool's "comparison
   failed / invariant broken" code, which reads to a caller as a determinism failure.
   The quieter half: `_number` coerced a malformed `clipped` to `0`, making the clip
   side of the verdict trivially equal. `NUMERIC_FRAME_FIELDS` + `_is_number` now
   require real, **finite** numbers (`bool` excluded — it is an `int` subclass, so
   `True` would otherwise pass as a count), and every bad form exits **2** writing no
   report. `_number` is now documented as defense-in-depth for the unvalidated
   `timing_ms`, not as the validation.
   `test_non_numeric_measurements_are_refused_not_tracebacked` covers five forms.
4. **A record with no usable `identity` was diffed anyway.** `{}` equals `{}`, so two
   unattributable records compared as "one build" and returned success for a diff
   attributable to neither — contradicting the format's own "keyed on
   pipeline_version + commit + target" premise. Measured: `identity: {}` **and** an
   absent `identity` both gave `identical: true` at rc 0.
   `test_a_record_without_a_usable_identity_is_refused`.

**Pushed back on one detail of (4), with evidence.** The review asked for the record's
`identity` to require the same fields as `REQUIRED_REPORT["identity"]`, i.e. including
`params_hash`. That would reject **every record `compare run` writes**:
`_build_identity` deliberately drops `params_hash` because it is *per-frame* (a
benchmark set spans several recipes), and `test_reads_stats_loss_and_timings` has
always asserted `assertNotIn("params_hash", identity)`. A real record's identity block
is exactly `{nc_version, git_commit, git_dirty, pipeline_version, target}`. So the new
`REQUIRED_RECORD_IDENTITY` is `("nc_version", "pipeline_version", "target")` — a
separate constant from `REQUIRED_REPORT`, documented as to why — with
`git_commit`/`git_dirty` optional (a no-git build legitimately omits them, and
`pins_source` already refuses determinism conclusions without them).
`test_a_no_git_build_is_still_a_valid_record` pins that direction so a future
tightening cannot quietly break it.

**Not over-tightened, verified:** `compare run --set fixtures` ×2 → `diff` still gives
`identical: True`, 5 frames compared, all mean deltas `[0,0,0]`, `notes: []`, rc 0 —
and the records written before this round still validate.

### 2026-07-27 (round 3) — container validation, and a pattern sweep (still uncommitted)

Two more findings, both the **same shape as each other**: the guard landed on the
field but not on its container. Fixed, plus a deliberate sweep for other instances of
the two bug shapes these three rounds have produced — the results of that sweep are
the more useful half of this entry. Python suite **86 → 88**; one new `cli` unit test
and one new e2e test. No output pixel, no `params_hash`, no report field changed.

#### 1. A malformed `meta` *container* silently lost provenance

`Value::get` on a non-object returns `None`, and `meta_pipeline_version` read that as
"this file records no `pipeline_version`" — indistinguishable from a bare legacy
recipe. So a corrupt **field** inside `meta` was a loud exit 2 (round 1's fix) while a
corrupt **whole `meta` block** replayed with *no skew check at all*. Measured before:
`meta` = `null` / `"x"` / `[]` / `123` / `true` → all rc 0, zero skew warnings.

`split_envelope` now requires `meta`, when the key is present, to be an object.
Checked against the **raw JSON object** rather than `envelope.meta`, because serde
folds `"meta": null` into the same `None` an omitted key produces — and an omitted
`meta` must stay legal (a hand-wrapped `--dump-params` recipe has no provenance to
record). Unknown *fields* inside a well-formed `meta` stay lenient: that leniency is
the forward-compatibility contract, not an oversight. Measured after: all five
malformed forms rc 2 naming ``sidecar `meta` must be an object``; omitted `meta`,
`{}`, and `{"invented":[1],"pipeline_version":7}` all still accepted.
`cli::tests::a_malformed_meta_container_is_as_loud_as_a_malformed_field` plus an e2e
test cover both directions.

#### 2. A record with no checksum evidence *attested* that checksums weren't skipped

`checksums_skipped` is an **affirmative claim** derived from an optional field. With
`checksums`/`input_sha256` stripped and different commits on each side, measured:
`rc=0  "identical": true  "checksums_skipped": false  notes=[]` — two builds over
unverified (or genuinely different) input bytes reported identical *and* attested that
verification happened. In the artifact whose whole purpose is attribution, a false
attestation is worse than an omission.

`checksums` is now a required frame field constrained to `verified|computed|skipped`,
and the two modes that *claim* a digest must carry one (an unsubstantiated `verified`
is the same false attestation by another route). `skipped` with a null digest stays
legal — that is the honest state, and the diff already surfaces it as a caveat. A
refused comparison now writes **no report at all**, so no attestation escapes.

#### The sweep — what recurs and what does not

Asked to check whether either shape appears elsewhere rather than wait for a fourth
pass. Both answers are worth recording.

**Pattern A — guard on the leaf, not the container: EIGHT more instances, all fixed.**
Every JSON document `compare.py` reads is external input (an `nc` report, a telemetry
record, a hand-editable benchmark or asset manifest), so a *parent* can be the wrong
type as easily as a leaf, and chained `x.get("a").get("b")` then raises
`AttributeError` — a traceback instead of the exit-coded message this module promises
in its own `load_json` docstring. Measured, all `AttributeError` before the fix:

- `report["recipe"]` as a truthy list / a string, and `recipe.output` as a string —
  three sites on the path that reads an `nc` report, i.e. the actual untrusted
  boundary. (`(report.get("recipe") or {})` looked safe but only rescues *falsy*
  non-dicts: `[]` survives it, `[1]` does not.)
- `timing_ms` as a non-dict, via `_timing_delta`.
- `bench["sets"]` as a list, a set spec as a string, a non-list `cases`, and a case
  that is not an object.
- `assets["rolls"]` as a list, a roll as a string, a non-list `frames`, and a frame
  entry that is not an object.

All now funnel parents through a small `_dict()` helper (documented with *why*), so a
malformed container produces the same "missing"/"not in the manifest" message a
malformed leaf does. Timings, being informational, degrade to "no timings" rather than
sinking the comparison. `test_a_malformed_container_is_a_message_not_an_attributeerror`
covers all twelve probes.

**Pattern B — absent evidence rendering as an affirmative negative: no further
instances.** I enumerated every boolean and status the record or the diff emits and
checked each one's polarity against what guarantees its evidence:

- `checksums_skipped` — the finding above; now substantiated.
- `target_changed`, `pipeline_version_changed` — sound, because round 2 made `target`
  and `pipeline_version` required non-`None` in `REQUIRED_RECORD_IDENTITY`.
- `output_depth_changed` — sound, because round 2 made `output_hdr` required *and*
  a bool, so `false` means genuinely equal rather than both-absent.
- `params_hash_changed` and `identical` — sound, because every `DETERMINISTIC` field
  is required and (round 2) type-checked.
- `input_sha256_changed` — already the **honest** polarity: it renders `null`, not
  `false`, when either digest is missing, and there is a test asserting that. After
  fix 2 a missing digest is only possible under `skipped`, which is surfaced.
- `notes: []` — sound once all of the above are.
- Rust side: `git_dirty` is **omitted** when unknown rather than `false`,
  `git_commit` omitted rather than the string `"unknown"`, and `params_hash` omitted
  for `inspect`/`estimate` — all three deliberately model absence as omission, and
  each has a test. `pins_source()` returns `false` when evidence is missing, which is
  the *conservative* direction (it refuses to claim determinism, never asserts it).
  `pipeline_version_warning(None)` staying silent is correct for a genuinely
  version-less legacy recipe, and after fix 1 that is the only way to reach it.

So: Pattern A was systemic in the Python layer and is now closed at every site I could
find; Pattern B had exactly the one instance already reported.

**Not over-tightened, verified again:** `compare run --set fixtures` ×2 → `diff` →
rc 0, `identical: True`, 5 frames, `checksums_skipped: false` (now substantiated by a
real digest per frame), `notes: []`; and the records written in rounds 1–2 still
validate at rc 0.

### 2026-07-28 — task closed, shipped

Landed after three review rounds (Codex + five pr-review lenses, then two Codex
delta passes). ~30 findings fixed, several rejected as false positives. Final
gates: **369 unit + 103 integration Rust, 88 Python, 0 failed**. Output pixels
byte-identical to the pre-work binary across 10 conversions; default
`params_hash` unchanged at `3575c9feb5d42b2b`.

- **Notes for `output/presets` (the dependent):** activating presets changes
  default pixels, so it must cross a version boundary — bump `PIPELINE_VERSION`
  to 2, add a `PIPELINE_FINGERPRINTS` row, refresh `PIPELINE_BEHAVIOR`. The drift
  gate fails first and prints the exact values to paste; treat that failure as the
  checklist. Do **not** edit row 1's `render` hash in place.
- **`pipeline_version` is still absent from the `film-master` report** — that
  carve-out is `color/film-master-render-pipeline`'s, with its absence pinned by a
  test there. Adding it is this epic's follow-up.
- **Two recurring bug shapes were swept, not just patched.** *Pattern A* (a
  validated leaf inside an unvalidated container, so a malformed parent degrades
  to "absent") recurred at **8 further sites** in `compare.py`, three of them on
  the path that reads an `nc` report — the real untrusted boundary. Note
  `(x.get("k") or {})` rescues only *falsy* non-dicts: `[]` survives it, `[1]`
  does not. *Pattern B* (absent evidence rendering as an affirmative negative) had
  exactly one instance (`checksums_skipped: false` with no digest anywhere); every
  other boolean's polarity was audited and is sound. Re-check both patterns when
  extending the record schema.
- **Open gap, deliberately not closed here:** `.github/workflows/ci.yml` is
  Rust-only, so `nctool compare` — the tool that decides whether two builds differ
  — is guarded by 88 tests **no gate runs**. Changing CI affects every
  contributor, so it is left as a recommendation rather than slipped into this
  task. Pre-existing for the manifest suite; this task widened it.
- **Scope kept:** metric set is mean ΔRGB + clip-fraction delta + timings only;
  ΔE2000/SSIM remain design-spec §12 item 7. Timings are excluded from the
  `identical` verdict (they varied ±1.4 s between runs of one build) and reported
  separately.

### 2026-07-28 (round 4) — the nondeterminism claim is now precondition-guarded

Lands **after** the closing entry above, which it amends: the Python suite is now
**91** tests (was 88). Rust unchanged (369 unit + 103 integration); no output pixel,
no `params_hash`, no report field moved.

A fourth review pass found two more routes to the *same* false accusation, which made
it a **class** rather than a series of bugs: `compare` was blaming the pipeline for
differences it had not ruled out other causes for. Round 1 closed the dirty-identity
route with `pins_source()`; that was one precondition treated as the whole guard.

**Measuring first found five routes, not the two reported** — all with the *same
clean* identity on both sides, all previously rc 1 "the pipeline is not deterministic":

| route | why the accusation is wrong |
|---|---|
| differing per-frame `params_hash` | the two runs used **different recipes**; of course the output differs |
| `checksums: skipped`, null digest | `diff` printed a note conceding the bytes were never verified, then contradicted itself on the next line |
| differing `output_hdr` | the two means are in different units |
| differing frame sets | the two runs did not convert the same work |
| dirty identity | round 1's route, now one precondition among several |

The fix is a **precondition set**, not another special case. `determinism_blockers()`
returns every alternative explanation for a non-zero diff; the claim fires only when
that list is empty. A blocked check is **not** a failure — it is a non-zero diff that
simply is not a determinism claim, so it keeps rc **0** (the "verdict delivered" code)
and each blocked reason rides in `notes[]` as `determinism_check_blocked: …`, naming
the frame and what to fix. Measured after: all five routes rc 0 + `identical: false` +
a naming note.

**The accusation is now defensible when it does fire.** It states what it ruled out:
`same commit (abc123, clean tree), same target (…), same pipeline_version (1), and
across all N frame(s): identical input bytes by sha256, identical params_hash,
identical output depth, and the same frame set in the same order`.

**The tightening did not make the check unreachable** — that was the risk worth
guarding, since it would have silently removed the guarantee.
`test_the_determinism_claim_still_fires_when_nothing_else_explains_it` asserts the
genuine case at rc 1 *and* that `determinism_blockers()` is empty for it;
`test_the_determinism_claim_requires_every_other_cause_ruled_out` covers all five
blocked routes; `test_every_offending_frame_is_named_not_just_the_first` pins that a
multi-frame set names every offender rather than the first.

**For a future author adding a record field:** the guard is now the precondition set,
so ask whether your field can independently change the recorded numbers. If it can, it
belongs in `determinism_blockers()` — omitting it turns a real, explainable difference
back into a false accusation against the pipeline. The docstring there says so too.

One note on the reported test spec: a **digest mismatch** was asked for as a rc 0 +
note case, but round 3 already refuses it earlier at rc **2** ("these cases converted
DIFFERENT input bytes … this is not a build comparison"), which is the stronger and
already-agreed outcome. The precondition is kept in the set as defense-in-depth should
that ordering ever change, and is tested at the `determinism_blockers()` level rather
than end-to-end, where rc 2 correctly wins.

**Invariants re-verified:** `compare run --set fixtures` ×2 → `diff` → rc 0,
`identical: True`, 5 frames, `notes: []`; records from rounds 1–3 still validate; the
dirty pair still rc 0 with its note; output byte-identical to the pre-round-1 binary;
`params_hash` still `3575c9feb5d42b2b`.


### 2026-07-27 (rebased after `color/film-master-render-pipeline`)

PR #59 added the neutral-default recipe keys `print.linear_range: [0, 1]` and
`output.preset: legacy`. Rebasing conversion versioning onto that merge changed
only the canonical default-recipe fingerprint
(`e1bd4fb5cb789ded` → `8a5b874faa30d391`). The recorded render and auto
film-base fingerprints stayed unchanged and the full legacy/film-master test suite
remained green, so this is the drift gate's sanctioned opt-in-schema refresh:
update the current row's `recipe` field without bumping `PIPELINE_VERSION`.


## dependency-hygiene

**Status:** not started
**Updated:** —

- Goal: Remove dead weight surfaced by the dependency/module-hygiene review (see [`_unassigned.md`](_unassigned.md)): three declared-but-unused crates and a duplicate algorithm selector enum.


## release-readiness

**Status:** not started
**Updated:** —

- Goal: Get `nc` ready for a public release: correct the public documentation that currently misstates the product, choose a license, add crate/release metadata, define supported platforms, and package binaries.


## stdout-broken-pipe-safety

**Status:** not started
**Updated:** —

- Goal: Make every stdout write in `nc` tolerate a **closed pipe** without a panic or backtrace, exiting cleanly instead.


## value-domain-terminology

**Status:** not started
**Updated:** —

- Goal: Make nc's value-domain terminology — especially `Dmin`/`Dmax` — easy to understand, use, and maintain for **both people and agents**.

### 2026-08-04 — cross-reference: `conversion-versioning`'s contract has a known hole

Appended rather than edited into the `conversion-versioning` section above, which records
the task as shipped and stays verbatim.

- **`pipeline_version` covers the *default* path only, and that is now a demonstrated gap
  rather than a theoretical one.** `algo/reference-anchored-sigmoid` (2026-08-03) moved three
  sigmoid defaults — `contrast` 1.0 → ≈2.0687, `shoulder` 0.2 → 0.6, and a new
  `curve.anchor` defaulting to mid-grey placement where the previous behavior pinned display
  white. The default curve is `exponential`, so `PIPELINE_VERSION` correctly did not move and
  `PIPELINE_FINGERPRINTS` correctly did not fail — yet every recipe selecting `sigmoid`
  renders differently, even with `contrast`/`toe`/`shoulder`/`dmax` all pinned, because
  omitted keys take the new defaults.
- **Filed as a sibling, not by reopening this task**, following the precedent
  `film-base/dmax-anchor-reliability` set for `film-base/dmax-reference`: the new work changes
  what a **completed** task's contract *promises*. Reopening would also have made
  `output/presets` non-executable, since it depends on this task — a real cost for a
  bookkeeping choice. New task: `core/recipe-replay-fidelity`, dependent on this task and on
  the algo task that exposed the gap.
- **The stopgap that exists today:** `cli::sigmoid_anchor_default_warning` — a hand-written,
  `--strict`-promotable warning when a loaded recipe selects sigmoid with no `anchor`,
  modelled on this task's own `pipeline_version_warning`. It closes one instance and does not
  generalize (it names one knob and one date in prose, and says nothing about the `contrast`
  and `shoulder` moves with the identical property). The new task owns retiring it.
- **Two remedies already considered and rejected, recorded so they are not re-derived:**
  bumping `reconstruction.schema_version` (it versions schema *shape* and is checked for
  exact equality, so a bump rejects every archived recipe — including the majority selecting
  `exponential`, which the change does not touch), and per-schema-version historical default
  tables (defensible, but it cannot stop at one knob, and committing to maintaining
  historical defaults is a policy decision that belongs in the new task, not an algo one).

## recipe-composition

**Status:** not started
**Updated:** 2026-08-11

- Goal: `--params` repeatable (file or `-`), `roll` gains convert's override flags,
  one precedence chain. Enables the pipeline/calibration split.
- **No schema change needed** — verified 2026-08-11 that both halves already parse:
  a recipe with only `reconstruction`/`print`/`output` works when the base comes
  from a flag, and one with only `film_base` + `dmax` works with everything else
  defaulted. The single missing mechanic is that `--params` rejects repetition.
- Precedence extends the existing rule rather than replacing it: flags already beat
  the recipe **by source, not value**, and layering adds ordering among recipes.

## profile-authoring

**Status:** not started
**Updated:** 2026-08-11

- Goal: `nc params` → `nc profile`; takes overrides, validates config-only, writes
  annotated JSONC with `--out`, no image. Deletes `--dump-params`.
- Why `--dump-params` goes, measured 2026-08-11: its output is byte-identical to
  the sidecar every conversion already writes, and it carries nothing the image
  produced — the same flags over two *different* scans emit identical files. It
  records modes (`"auto"`, `"percentile"`), never the measurements the report holds
  beside it. So "freeze a recipe" ran a full decode/render/encode to echo the
  flags just typed, and the result still re-measured per frame.
- JSONC because it is a **superset**: existing recipes, sidecars and `--params`
  files stay valid, the schema's tagged enums keep working, and the machine
  contracts (stdout report, sidecar) stay plain JSON. Comments are generated from
  the schema and **not preserved** across a round trip, so nc must never rewrite a
  user's file in place.

## unfrozen-auto-mode-warning

**Status:** not started
**Updated:** 2026-08-11

- Goal: warn when a recipe applied to a roll still re-measures per frame.
- The gap, one run with `--base-region … --auto-wb percentile --auto-d-max`: the
  report held `film_base {0.163, 0.080, 0.038}`, `dmax 0.581`, `wb [1.228, 1.0,
  0.721]` while the recipe held `{"region": …}`, `"auto"`, `"percentile"`. Applying
  it to a roll re-derived all three per frame, with no warning beyond an incidental
  region-uniformity note.
- Precedent exists: roll already warns when the film base is not `explicit`.

### 2026-09-01 — cross-reference: the v3 `recipe` fingerprint was refreshed in place

Appended rather than edited into the `conversion-versioning` section above, which
records the task as shipped and stays verbatim.

- `output/linear-render` added `print.display_tone` (default `shoulder`), so the
  **default recipe document gained a key** and its hash moved
  `5b22d0505ed4fb79` → `a26e8ec6434e8ebc`. `render` and `base` are byte-identical,
  because the default selector resolves to exactly the shoulder v3 already applied
  — no default pixel moved, and `PIPELINE_VERSION` stays 3.
- This is the case the table sanctions editing `recipe` in place for. Bumping
  instead would have been actively harmful: `pipeline_version_warning` fires on any
  mismatch, so a user re-running an archived v3 sidecar would be told "the output
  will not match the original" when it matches exactly.
