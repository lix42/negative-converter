# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

It holds only what applies **across** the project. Subsystem rules live in the
`//!` docs of the module they govern (see "Where the detail lives"), and tool
rules in the nested `CLAUDE.md` of their directory. **Add a new lesson there, not
here**; this file gets at most a one-line pointer.

## What this project is

**Hanten** — a command-line tool that reads a film **negative** scan (SilverFast
HDR/HDRi format first) and converts it to a **positive** image. The binary is
`hanten`; `nc` is the internal name.

"AI-friendly" means **every conversion parameter is a CLI flag**, and the tool is
deterministic and scriptable with JSON recipes/reports. It does **not** mean using
ML to process images (no auto-crop, generative restoration, etc.); any future ML
assistance is opt-in and sits *around* a deterministic core.

### Hanten outside, `nc` inside

**Almost every `nc` in the tree is an identifier, not branding** — don't "tidy" it.

| Stays `nc` | Why |
|---|---|
| `nc-film-rgb-v1` (`working_mapping` in every report) | a **versioned** colour-space identifier; renaming it means a v2 |
| `nc_version`, telemetry `schema_version` | snapshot tests assert the exact JSON |
| `NC_*` environment variables | diagnostic and test surface |
| the telemetry log directory (`<data-dir>/nc/telemetry.jsonl`) | moving it orphans existing logs |
| the `(nc)` suffix in the coded-HDR ICC descriptions (`pipeline::color`) | it is **written into the profile bytes** of every `hdr-pq-tiff` / `hdr-hlg-tiff` |
| recipe keys, report fields, exit codes | the scripting contract |
| the crate and Cargo package | `NC_VERSION` is `CARGO_PKG_VERSION`; a `[[bin]]` section renames the binary |
| `nctool`, its `--nc`/`$NC`, `../nc-assets` | internal tooling and the machine-local asset symlink |
| command lines in `docs/progress/`, `docs/reports/`, `docs/spike/` and closed task files | they record what was **run**. Exception: a progress file's `## Epic summary` is current guidance, so it moves |

| Is Hanten | |
|---|---|
| the GitHub repository | `lix42/hanten` (renamed 2026-09-21; worktrees share `main/.git/config`) |
| `README.md`, `docs/design-spec.md`, `docs/using-nc.md`, `docs/TASKS.md`, progress-log titles | titles and opening prose |
| the binary, and every command line in a **live** doc, skill or script | anything written before 2026-09-21 spells it `nc` |
| clap's `about` line | do **not** also prefix `version_string()` — it would print `hanten Hanten 0.1.0` |
| the `(Hanten)` suffix in the Adobe RGB ICC description (`pipeline::color`) | a profile name is user-visible; like the `(nc)` one it is in every file's bytes once written, so it is an identifier too — don't "tidy" it to `(nc)` |
| the stderr prefix (`hanten: warning:`) | `scripts/real-scan-verify/harness.sh` greps the *message*, never the prefix |

`nctool` must keep accepting the pre-rename `nc` banner — see
`scripts/analysis/CLAUDE.md`.

## The migration rule (read before writing any `nf` code)

nc is migrating to the design in `docs/design-update.md` (the fixed decode plus a
staged rendering chain). While that runs, **structure for the long term beats
reusing what is there**:

- **Write the new stages fresh.** Do not shape a new stage around the old code's
  seams, types or fusions, and do not "extract" a stage out of a per-pixel body
  because that is where the arithmetic lives today.
- **Retiring an old path is not a loss of capability** — it is a list of abilities
  the new code may need to add. Decide each on its merits.
- **The old behaviour is preserved by git, not by code.** The reference is the
  `reserve` branch (from tag `pre-new-flow`, may take cherry-picks, so named by
  **commit**), built and cached by `scripts/reference-snapshot/build.sh` — its
  README is the procedure. In a review matrix it is a `builds` arm
  (`--build ref=…`, never `--nc`) carrying `expect_commit`.

## Source of truth (read these first)

- `docs/design-spec.md` — the authoritative design: architecture, CLI surface,
  §4 value terms, §8 determinism, §9 recipe schema, §11 exit codes.
- `docs/design-update.md` — the new-flow design the migration is heading to.
- `docs/TASKS.md` — the plan: canonical dependency graph and task checklist by epic.
- `docs/tasks/<epic>/<name>.md` — per-task file. Task ids are `<epic>/<name>`.
- `docs/progress/<epic>.md` — **append-only** execution log per epic, opening with
  an `## Epic summary`. `_unassigned.md` parks sections that name no task.
- `docs/using-nc.md` — the user guide, verified against the **binary**: it wins on
  what the CLI accepts today; the design spec wins on intent.
- `docs/reports/` — measurements of things that exist (`v0-baseline.md` is the
  reference point). `docs/spike/` — results of spikes (questions answered before
  the thing exists). `docs/design/` — new design documents. `spike/` and `design/`
  each have a README stating the distinction.
- `docs/colorimetry-maintenance.md` — the procedure for changing any colorimetry.

## Task-tracking workflow

Work is planned with the `task-tracking` skill (`/tasks:*`), in **epic mode**:
`docs/TASKS.md` holds the authoritative status (`[ ]`/`[~]`/`[x]`) and the
task-level graph; a task is executable when all its deps are `[x]`. Before
starting, read the task file, its epic's progress file in full, and the
`Epic summary` of every epic it depends on. Keep the rollup, the Mermaid diagram,
the dependency list and per-task Dependencies in sync — `TASKS.md` wins. The
rollup may contain cycles; the task graph must not.

- **Moving a task between epics is a rename with a long tail.** The skill fixes
  only `](…)` links; backticked prose paths (in `src/` docs and the design spec)
  rot silently. **Never bulk-rewrite ids** — `color-management`, `asset-manifest`
  and `perf-telemetry` are also ordinary words or skill names.
- **Progress logs are append-only**: add a cross-reference as a new dated entry,
  never mid-body. The exception is a **user-authorised consolidation pass**, which
  may summarise *done* tasks' sections while keeping every decision, gotcha,
  cited measurement and cited heading, and records itself in the file header.

## Architecture

A pure-function pipeline orchestrated by a thin CLI. Stages are pure
`(input, params) -> output`; `main`/`cli` are the only orchestrators, and a fact a
stage decides (e.g. `BaseEstimate::ir_mask_applied`) is **returned**, never
re-derived by the caller.

**Current chain** — `cli::convert_frame` dispatches on the resolved `output.preset`:

```text
decode → film-base → tagged reconstruction → FilmRgbImage
  ├ film-master → NC film RGB v1 → linear ACEScg → encode (unclamped f32, no transform)
  └ display presets → NC film RGB v1 → linear ACEScg → shared print controls
      ├ gain-map-hdr (default) / ultra-hdr-v1 → SDR + HDR + gain map → JPEG
      ├ display-p3 / compatibility → SDR → P3/sRGB → 16-bit TIFF
      ├ hdr-pq / hdr-hlg        → HDR → Rec.2100 PQ/HLG → 10-bit 4:4:4 AVIF
      ├ hdr-pq-tiff / hdr-hlg-tiff → the same signal → full-range 16-bit TIFF
      └ hdr-linear-tiff         → HDR, no transfer → 32-bit float BT.2020 TIFF
```

Every preset is atomic; `OutputPreset` (`types.rs`) is the list and says what
must stay in step with it. `gain-map-hdr` and `ultra-hdr-v1` are one render
packaged in two metadata dialects.

**New chain** (`--new-flow`, the migration target): `algo::fixed` decode →
`scene_correction` → `look` → `fit_range` → `fit_gamut` → encode, with the stage
order pinned by boundary types (`pipeline/chain.rs`). `--new-flow` is migration
scaffolding (`src/flow.rs`, deleted by `nf-core/default-flip`): it selects the
chain and its knobs, so unlike the operational flags it **does** change output.

Rules every stage keeps:

- **32-bit float in a linear working space; no stage clamps.** Range clamping
  happens only at the u16 encode, which counts clamped and non-finite samples
  into `EncodeReport` for a report warning. Never clamp silently.
- **Density conversion and print rendering are separate stages** — the core
  colour-fidelity rule. Don't merge them.
- **The IR plane is carried through, not consumed.** The one exception is holder
  detection in `film_base`, gated on the *measured* `ir_separability`, never on
  `--film-type`.
- **Every standards-based matrix, luma vector and transfer constant lives in
  `pipeline/colorimetry/`** — never a literal in a stage. Editing `REC709`,
  `DISPLAY_P3`, `ACESCG`, `ADOBE_RGB` or `BT2020` changes ICC bytes and pixels even with
  `pinned.rs` untouched, and no gate catches it.
- **Per-pixel maps go through `pipeline/pixels`**; floating-point reductions run
  in a fixed order, or output stops being byte-identical.
- **Adding a full-frame buffer to any stage means updating `pipeline/memory.rs`'s
  model.** Nothing tests the model against the code, and a new preset calibrates
  its own `RunProfile`.

### Where the detail lives

Read the module docs before changing these; they hold the traps.

| Area | Read |
|---|---|
| output paths, suffixes, preset → container | `cli::resolve_output_path`, `container_for`, `Unappendable` |
| knob merge, validation order, removed keys | `cli::merge`, `validate`, `validate_convert`, `validate_output_preset`, `strip_retired_keys_at_old_defaults` |
| new-flow flags and recipe | `src/flow.rs`, `src/recipe.rs` |
| new-flow destination set (axes, table, derivation) | `src/destination.rs` |
| reconstruction, density scale, anchors | `types.rs` (`DensityParams::default_scale_for`, `AnchorPlacement`), `algo/fixed.rs` |
| film base, IR holder mask, measurement region | `pipeline/film_base.rs` |
| film-stock data and the retiring characteristic curve | `film_stock/` (evidence for the decode's constants; `docs/datasheets/`), `algo/characteristic.rs` |
| display tone, SDR/HDR bounds | `pipeline/display_tone.rs`, `sdr.rs`, `hdr.rs`, `render_split.rs`; the new chain's SDR/HDR branch contract in `pipeline/chain.rs` |
| gain map, Ultra HDR / ISO 21496-1 container | `pipeline/gain_map.rs` (legacy), `pipeline/gain_ratio.rs` (the new chain's per-channel gain), `gain_map/iso.rs`, `io/ultra_hdr.rs`, `scripts/iso-decoder-oracle/`, `Cargo.toml` (`ultrahdr-sys`'s `jpeg-max-dimension`) |
| AVIF / libaom | `io/avif.rs`, `Cargo.toml` comments |
| colorimetry | `pipeline/colorimetry/`, `docs/colorimetry-maintenance.md` |
| memory preflight | `pipeline/memory.rs` |
| lcms2 transforms and fault handler | `pipeline/color.rs`; `cli.rs`'s `CMS_ERROR` handler, cleared before and checked after each render |
| goldens, cross-platform bounds, drift gate | `stages::golden`, `pipeline/chain_golden.rs`, `version.rs` (`PipelineFingerprint`) |
| diagnostic probes | `pipeline/shadow_metrics.rs`, `algo/curve_probe.rs` |
| telemetry | `telemetry.rs`, the `perf-telemetry` skill |
| build identity (`NC_GIT_*`) | `build.rs` |

Gain-map container changes need the manual `scripts/iso-decoder-oracle/` check
(macOS only): exiftool and libultrahdr both accept files no decoder parses.

## Commands and gates

Rust (edition 2024), one binary crate `nc` with binary `hanten`; `Cargo.lock` is
committed.

- **Before pushing, match CI** (`.github/workflows/ci.yml`):
  `python3 scripts/check-vendored-native.py` → `cargo fmt --all --check` →
  `cargo clippy --all-targets --all-features -- -D warnings` →
  `cargo build --all-targets --all-features` →
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features` → the `nctool` suite
  (`scripts/analysis/CLAUDE.md`) → `cargo test --all-features`.
- **Match CI's toolchain too.** CI takes the latest stable; `rustup check` first,
  since a newer clippy adds lints a local green run never saw.
- **`cargo doc` warnings fail CI** (a renamed item leaves dangling intra-doc
  links). Links to `#[cfg(test)]` items must be plain backticks, since rustdoc
  builds without `cfg(test)`.
- **`cargo test --lib` fails** (no `[lib]` target): use `cargo test --bin hanten
  <filter>` for in-`src` tests. A bare `cargo test <filter>` also runs
  `tests/pipeline.rs` and prints two `test result` lines — read both.
- **Read the test count, not `ok`** — a filter matching nothing passes. Don't pipe a
  gate into `tail`: the exit status becomes the pipe's. Redirect to a file and
  check `$?`.
- **Only `aarch64-apple-darwin` is installed**, so `#[cfg(target_os = "linux")]`
  code first compiles in CI. Gate only the I/O and keep logic in un-gated, tested
  helpers (`pipeline::memory` is the pattern).
- **Item-level `allow(dead_code)` needs a comment naming its consumer.**
- **The shell is zsh**: pass flag lists as arrays (`"${args[@]}"`), and quote a
  word starting with `=`.
- `tools/review-app` has its own gates — see its `CLAUDE.md`.

## Conventions

### Writing

- **Write for the future reader.** Docs and comments earn their place by long-term
  value: the constraint, the reason, the trap — not how the decision was reached,
  and not a count of how often something broke.
- **No gate reads prose.** After changing behaviour, grep for the *negation* of the
  claim you just falsified, worded several ways and across every path including
  `docs/TASKS.md` — the stale sentence is never in your diff.
- **Scripted edits:** inserting before `fn X` lands between `X`'s doc and its
  signature; a batch with fail-fast asserts skips later edits once rustfmt moves
  an anchor. Apply edits independently and grep the result.
- **A task file tracks work; it does not specify it.** Record the goal, the open
  questions and what is known vs unknown; leave formulas, tables, signatures and
  test lists to implementation.
- **A plan doc is not code.** In review, edit it only for findings that would
  mislead the design or waste work, not for gaps implementation will surface.
- **A user-visible change updates `docs/using-nc.md` in the same PR** (a flag,
  subcommand, default, recipe key, preset, exit code, or a report field or message
  a user acts on). Verify by running the binary — the `update-usingnc-doc` skill.
- **Value terms (high/low/bright/dark):** read design-spec §4 first. As scene
  luminance rises, transmission falls while density, positive and output rise;
  "bright"/"dark" mean the *scene*. `Dmin` is a transmission (the film base);
  the anchor `A` is a scalar density (`Dmax`, a leader density, retired).

### Knobs and recipes

- **Every conversion knob is a CLI flag and a recipe key** — nothing reachable
  only from code. A knob spans the `*Overrides` field (`cli.rs`), the `*Params`
  field (`types.rs`), a `merge` arm with a merge test (a missing arm is a silent
  no-op), and usually a `validate` rule. Flags win over the recipe. Exceptions:
  operational flags (`--report`, `--telemetry*`, `--max-memory`) never change the
  image and are not recipe keys; `--preset` is a CLI-only expansion; `--new-flow`
  selects the chain.
- **Recipe shape follows design-spec §9** (every struct is `deny_unknown_fields`).
  Mutually exclusive knobs are one enum field, never parallel `Option`s or bools.
- **Retiring a recipe key:** strip its old default (every sidecar serializes it),
  refuse any other value with a migration message, and never alias. Refuse even the
  old default if replaying it would now render differently.
- **Report prose that names an operation is a claim about the run** — derive it
  from the resolved config, or state the fact in a field.

### Fail loudly

- Map errors to the documented exit codes (design-spec §11); surface clipping and
  unsupported input as errors or report warnings, never a quietly wrong image.
- **Diagnose the most specific fault first, and make every remedy work** — it must
  name an action the user's branch and command accept (walk reachable *values*,
  not knobs). A rule must run before anything coarser can refuse. Tests assert the
  losing rule's wording is **absent** and go through the real path (`merge` or the
  binary), not the rule directly.
- **Validate the resolved value, never a proxy for it.** Value rules go in
  `validate` (shared with `roll` and per-frame overrides). Flag-presence rules must
  run before anything coarser can refuse: `--new-flow` availability in
  `flow::reject_unavailable_flags` (before `merge`), the rest in `validate_convert`.
  A presence rule refuses a flag only when it forces something the branch cannot
  produce; an identity value is spared only where a recipe could have set the
  knob.

### Determinism and tests

- **Same inputs + params ⇒ identical output, per build and architecture** — libm
  and lcms2 differ by target (design-spec §8). Pin bit-identity with curated
  per-pixel goldens (`stages::golden`, `chain_golden`); never checksum a full
  frame, an encoded file or post-lcms2 pixels in a cross-platform gate. When a
  value cannot be pinned exactly, bound it by enumeration (`reachable_window`),
  never by a rounding-margin argument. One documented exception, to the *exit
  status* only: the memory preflight's warn tier compares against detected RAM, so
  under `--strict` the same run can pass on one machine and fail on another
  (`pipeline/memory.rs`) — keep `--strict` tests to small fixtures.
- **Changing a default render trips the drift gate** (`version::PIPELINE_FINGERPRINTS`):
  add a row, never edit a historical one.
- **`cargo test` never runs the `#[ignore]`d asset probes.** After moving a
  default, re-run them by hand (`cargo test --release -- --ignored`).
- **`tests/pipeline.rs`'s `run()` passes arguments verbatim**: a test that writes
  a TIFF states its preset.

### Real scans and renders

- **Real scans live in `../nc-assets`** (a machine-local symlink to the
  [nc-assets Drive folder](https://drive.google.com/drive/folders/1qXE2jF3MuVnQ2sW0pGTp3URwBJuf_LV6);
  inventory and roles via the `asset-manifest` skill). **Never read a scan or an
  output image into context**: use `exiftool`, `hanten inspect`,
  `scripts/real-scan-verify/`, an `#[ignore]` probe printing derived numbers, or
  `nctool metrics` for output pixels. Decoder fixtures are in `tests/fixtures/`.
- There is no public spec for the SilverFast HDRi layout: validate the decoder
  against real scans and degrade gracefully on unknown layouts.
- Real scans are laid out `holder → thin rebate → picture`, so `--auto-base` is
  best-effort; measure `Dmin` once from an unexposed frame and reuse it.
- **Comparing renders by eye:** use `tools/review-app` with sets from the
  `render-review-set` skill. **Never commit or publish a review set** — the images
  are the user's photographs.

### Process

- **Rebuilding onto a concurrently merged PR:** when the base shipped the same
  concept, take its design and re-apply yours. Then diff what you *dropped*
  (`git diff <old-branch> -- <path>`) — lost tests and serde attributes fail no gate.
- **Codex reviews** follow the `review-fix-loop` skill; a failed Codex review still
  exits 0, so judge it by its output.
- **Skills** live in `.agents/skills/` (Codex's directory), with relative
  symlinks in `.claude/skills/`. Exception: `review-fix-loop` has two deliberately
  different variants — never symlink one to the other.
- **Agents** (`nc-reviewer`, `nc-fixer`) live in `.claude/agents/` only, never
  mirrored: Codex is the loop's other engine and must not inherit their primer.
  Each summarises this file and the module docs, and defers to them.
