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
- **Known gap:** there is still **no `pipeline_version` constant** in the code —
  `core/conversion-versioning` owns creating it, and `film-base/dmax-reference`
  already changed the default render, so that commit is the v0→v1 boundary.


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

**Status:** not started
**Updated:** —

- Goal: Stamp every conversion with a machine-readable identity and a **behavioral pipeline version**, so outputs are attributable and conversion quality / performance can be compared across versions of `nc`.


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
