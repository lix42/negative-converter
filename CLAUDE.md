# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

`nc` — a command-line tool that reads a film **negative** scan (SilverFast
HDR/HDRi format first) and converts it to a **positive** image, written as TIFF.

The defining requirement is what "AI-friendly" means here: **every conversion
parameter is exposed as a CLI flag**, and the tool is deterministic and scriptable
with JSON recipes/reports. It does **not** mean using ML/AI to process images
(no auto-crop, generative restoration, etc.). Any future ML assistance is opt-in
and sits *around* a deterministic core. Keep this distinction — it has been
explicitly corrected once already.

## Source of truth (read these first)

- `docs/design-spec.md` — the authoritative Step-1 design (architecture, pipeline,
  CLI surface, parameter reference, exit codes, roadmap). It is the sole
  maintained design source; rendered HTML may be regenerated after the feature
  roadmap stabilizes.
- `docs/TASKS.md` — the plan: distilled design, the canonical dependency graph,
  and the task checklist grouped by epic. This is the control center for what to
  build next.
- `docs/tasks/<epic>/<name>.md` — per-task spec (goal / design / how-to-verify /
  deps). Task ids are `<epic>/<name>`.
- `docs/progress/<epic>.md` — execution log, one file per epic, each opening with
  an `## Epic summary`. Before starting a task, read your epic's file in full plus
  the `Epic summary` of every epic you depend on; append to your task's section as
  you work. `docs/progress/_unassigned.md` parks log sections that name no task.
- `docs/reports/<name>.md` — versioned conversion baselines / comparisons.
  `v0-baseline.md` records the current default-output behavior (the reference point
  future versions are measured against; see the `conversion-versioning` task).
- `docs/negative-convertor-research-report.md` — background research (image
  science, library survey). Context, not spec.

## Task-tracking workflow

Work is planned and tracked with the `task-tracking` skill (the `/tasks:*`
commands). The plan is in **epic mode**: `docs/TASKS.md` is the authoritative
status (the `[ ]`/`[~]`/`[x]` checkboxes) and the task-level dependency graph;
`docs/progress/<epic>.md` is the narrative. When picking up work: consult
`TASKS.md` for what's unblocked (a task is executable when all its deps are
`[x]`), read the task file plus its epic's progress file and the `Epic summary`
of each epic it depends on, then implement. Keep the epic rollup, the task-level
Mermaid diagram, the canonical dependency list, and per-task Dependencies
sections in sync — `TASKS.md` wins on conflicts. The **rollup may legitimately
contain cycles**; only the task graph must be acyclic.

**Moving a task between epics is a rename with a long tail.** The skill's
link-fixing step only covers `](…)` targets — backticked *prose* paths rot
silently, including task ids in `src/` doc comments and `docs/design-spec.md`.
Only a changed **stem** breaks hard (`algo-sigmoid` → `algo/sigmoid` now resolves
to nothing); a task that merely gained an epic prefix still substring-matches.
**Never bulk-rewrite ids** — `color-management` is also ordinary English for ICC
work, and `asset-manifest` / `perf-telemetry` also name skills. Progress logs are
**append-only**: add a cross-reference as a new dated entry, never as a mid-body
insertion (that silently breaks the verbatim history).

## Architecture

A pure-function pipeline orchestrated by a thin CLI layer.

**Current shipped architecture** — `stages::render` dispatches on the resolved
`output.preset`:

```text
decode → film-base → tagged reconstruction → FilmRgbImage
  ├ legacy (default, no preset) → finish_print → output color transform → encode
  └ film-master → NC film RGB v1 → linear ACEScg → encode (unclamped f32, no transform)
```

**Target replacement architecture (open roadmap tasks):**

```text
decode → film-base → tagged reconstruction + density curve → FilmRgbImage
       → NC film RGB v1 → linear ACEScg
         ├→ film-master
         └→ shared print controls → SDR/HDR render → encode
```

- All processing is **32-bit float in a linear working space**; bit-depth
  reduction happens only at the final encode. HDR is a first-class concern.
- **Density conversion and print rendering are separate sub-stages** — the core
  color-fidelity rule. Don't collapse them.
- Algorithms are pluggable behind the tagged `reconstruction` recipe object:
  `algo::reconstruct` resolves it into `simple` or `density` reconstruction
  (density selecting an `exponential` (default) or `sigmoid` curve);
  `algo::finish_print` is the stage-4 print bridge. The old `Converter` trait and
  `AlgoParams` are gone.
- The **IR channel** (HDRi 64-bit input) is decoded and, by default, **preserved
  but not acted on**; carry it through, don't consume it. The one exception is
  **IR-assisted film-holder detection** (`ir-holder-detection`): under an explicit
  `--film-type chromogenic` declaration on an IR scan, `film_base::estimate`
  consumes the IR plane to mask the opaque holder before the auto rebate search.
  IR-based dust removal remains a roadmap follow-up.
- Current module map (`src/`, all implemented): `types.rs` (shared types),
  `io/{decode,encode,ultra_hdr}.rs`,
  `pipeline/{film_base,color,stages,input_semantics,working_space,render_split,sdr,hdr,gain_map,memory}.rs`
  plus `pipeline/colorimetry/` — the **single source of truth for every
  standards-based matrix and luma vector**; see the colorimetry note below
  (`film_base::estimate` is stage 2, resolved by the orchestrator before the
  render; `stages::render` is the pure reconstruction→named-output core (stages
  3–5a): it dispatches on the resolved `output.preset` into the frozen `legacy`
  path (`reconstruct → finish_print → color::to_output`) or `film-master`
  (`reconstruct → map_nc_film_rgb_v1 → render_split::film_master`, no colour
  transform). The explicitly selected, `convert`-only `ultra-hdr-v1` arm is the
  first CLI-reachable display (5b) consumer: `stages::render_gain_map_source`
  resolves one shared source, `pipeline::gain_map` feeds the implemented
  `pipeline::sdr` and `pipeline::hdr` stages, and `io::ultra_hdr` writes legacy
  XMP/MPF metadata with no ISO claim. SDR returns opaque rendered-linear Display P3/sRGB
  pixels coupled to resolved 203-nit tone/gamut metadata;
  `color::encode_rendered_sdr` derives the matching transfer/profile without a
  second gamut transform. HDR returns either opaque display-linear BT.2020
  pixels (which gain-map work must convert to common linear Display P3 before
  ratio math) or opaque in-place Rec.2100 PQ/HLG pixels coupled to the fixed
  203-nit reference-white / 1000-nit peak, shoulder, gamut, HLG OOTF, and CICP
  contract. `output/presets` still owns the remaining presets, roll integration,
  and future default activation;
  `input_semantics::resolve` is the pure stage-1b transfer/meaning resolver,
  keyed on SilverFast XMP mode metadata — see the input-semantics note below;
  `working_space::map_nc_film_rgb_v1` is the typed NC film RGB v1 → linear
  ACEScg mapper; `render_split` is the named-output split out of that boundary —
  `film_master` (a pure unwrap: the bypass *is* the master) plus the shared print
  controls `WB → exposure → black point → linear_range`, resolved once and
  *borrowed* by both display branches. The `film-master` half and explicit
  `ultra-hdr-v1` consumer are wired; a non-default `print.linear_range` is
  accepted only by that display preset (legacy ignores it and film-master
  rejects it);
  `memory::preflight` is the stage-0 peak-memory gate — see the memory note below),
  `algo/{mod,simple,density,sigmoid}.rs`, `telemetry.rs`, `version.rs`
  (build/pipeline identity + `stable_hash`, the crate's only params-hash
  implementation — `telemetry::params_hash` delegates to it so the core report
  never depends on the opt-in telemetry module), `cli.rs`, `main.rs`.
  `main`/`cli` are the only orchestrators; stages stay pure. `build.rs` exposes
  the compile target triple as `NC_TARGET` plus `NC_GIT_COMMIT`/`NC_GIT_DIRTY`
  for the report's identity block.
- **Telemetry is operational, not a conversion knob.** `src/telemetry.rs` emits
  an opt-in, fail-soft, schema-versioned JSON record per `nc convert` run (image
  facts, per-stage timings, conversion summary) to a JSONL log / one-off file.
  Its flags (`--telemetry`, `--telemetry-file`, env `NC_TELEMETRY_LOG`) are the
  **exception** to the "every knob is a CLI flag *and* a recipe key" rule: like
  `--report`, they're operational, so they live only on the CLI arg struct, are
  **not** recipe keys, and must never perturb the deterministic image output.
  How-to lives in the `perf-telemetry` skill; record shape in design-spec §9.
- **Every standards-based coefficient lives in `pipeline/colorimetry/`, and the
  runtime never derives one.** `definitions.rs` holds the *source data*
  (primaries, white points, cone-response matrices, PQ/HLG constants,
  normatively tabulated vectors) with provenance; `pinned.rs` holds the reviewed
  literals the renderer multiplies by; `derive.rs` (the binary64 math) and
  `audit.rs` (the check/regen harness) are **`#[cfg(test)]`**, so rendering
  cannot start deriving per run. Never add a matrix or luma literal to a stage —
  import it. Product policy (reference white, peak nits, shoulder, gain-map
  offsets) stays with its stage and refers to a *named* space instead of
  restating colorimetry. Workflow for changing any of it:
  `docs/colorimetry-maintenance.md`; `NC_COLORIMETRY_REGEN=1 cargo test
  colorimetry::audit` regenerates `derived-artifacts.txt` (and **only** that —
  it never rewrites `pinned.rs`, so a generator run can never silently move
  pixels). When migrating a coefficient *into* it, inventory by reading the
  consuming functions, not by grepping for `const` — two hid as inline literals
  (an HLG OETF coefficient spelled `0.178_832_77`, which digit separators keep
  out of a naive grep, and two luma vectors inline in a `match` arm). Four
  gotchas that have already cost time:
  - **`pinned.rs` is not the only runtime consumer of a definition.**
    `pipeline::color` feeds `definitions::{REC709, DISPLAY_P3, ACESCG, PROPHOTO}`
    straight into Little CMS, so editing one of those four changes ICC bytes and
    every lcms2-transformed pixel *even with `pinned.rs` untouched and every
    audit `ulps` at 0*. Nothing automated catches it: `PIPELINE_FINGERPRINTS`
    stops before lcms2 and the audit only compares pinned artifacts. Treat those
    four as a pixel change regardless of the ulp column.
  - **Two Bradford conventions coexist deliberately.** `BRADFORD` (exact `f64`
    inverse) is canonical; `BRADFORD_PUBLISHED_INVERSE` (Lindbloom's printed
    7-decimal inverse) exists *only* because `NC_FILM_RGB_V1_TO_ACESCG` was
    pinned with it — re-deriving v1 with the canonical one shifts it 9.1e-8, a
    pixel change to a frozen identifier. A test fails if you collapse them.
  - **The three luma vectors have three different provenances, and each has its
    own verification rule.** `BT2020_LUMA` is transcribed from a normative table
    and deliberately does *not* match a derivation from its own primaries (~2e-6,
    ~17 ulps) — the standard rounds and encoders use the rounded form.
    `DISPLAY_P3_LUMA` *is* an exact derivation. `SRGB_LUMA` is the derivation
    **rounded to six decimals** (0/−6/43 ulps), so it carries its own
    `SRGB_LUMA_MAX_ULPS = 43` instead of relaxing the shared ±1 bound. Tests pin
    each relationship; don't "correct" one to match another.
  - **The check tolerance is ±1 `f32` ulp and that is measured, not lazy.** Three
    shipped entries sit exactly one ulp off the canonical derivation for reasons
    unrecoverable from the repo. For scale, the chromaticities are specified to
    three decimals, so their own ±5e-4 rounding moves entries ~3,500 ulps.
- **Peak memory is gated before decode, and `pipeline/memory.rs` owns the model.**
  Every command that decodes runs `memory::preflight` on a metadata-only
  `io::decode::probe` (never `read_image` — `decode` only returns dimensions after
  it has allocated) and fails with **exit 6** (`NcError::Resource`) over budget:
  fixed 6 GiB default, `--max-memory` to override, a `--strict`-promotable warning
  above ~70% of detected RAM. On `roll` the gate runs per frame and a rejected frame
  follows roll's ordinary per-frame handling — recorded in its report entry, siblings
  still written, roll exits **1**, not 6. `--max-memory` is another **operational**
  flag (arg struct only, never a recipe key, never perturbs output) — with two
  caveats: the budget also caps the `tiff` read buffers (`min(4 GiB, budget)`), so a
  small-but-passing budget can turn a decodable file into an exit-3 decode failure;
  and the warn tier is the **first documented exception** to the "same inputs +
  params ⇒ identical output" rule below — it compares against detected RAM, so with
  `--strict` the same run can exit 0 on a big machine and non-zero on a small one
  (the *image* is still bit-identical; only the exit differs).
  The model counts the *simultaneously live* full-frame buffers — **decode 18 ·
  film-base 16+12·s · render 32+12·s · encode 38+12·s bytes/px** for HDRi u16
  (`s` = sampled rectangle ÷ frame; ~0.69 for the auto interior, 1.0 for a
  full-frame `--base-region`). For `convert` the **encode** phase is the peak: the
  decoded image is held for `--export-ir` while the rendered one exists, and the
  u16 quantize buffer sits on both. For `inspect`/`estimate` the **film-base**
  phase is — `film_base::region_channels` materializes the sampled rectangle
  unstrided into three `Vec<f32>`, so sampling is *not* free (that omission was a
  real bug in the first version of the model). The `12·s` term appears in *every
  later* phase because freed pages stay resident — the same retention rule that
  sums the encode buffers; treating it as a competing phase instead
  under-estimated a real run by 10%. Two images overlapping
  is by design; three was the bug (`color::to_output` cloned; it now **consumes and
  returns** the image, transforming those very buffers). If you add a full-frame
  buffer to any stage, update that model — **nothing tests it against the code**, so
  the gate silently under-approves until someone does. `decode` is
  `decode_within(&Path, budget_bytes)`. `RunProfile::Convert` models the TIFF
  paths; `RunProfile::UltraHdrV1` separately counts shared-source, dual-render,
  gain-map, JPEG, native-copy, and package staging. Future presets must add and
  calibrate their own profile before activation.

### Stack / commands

Rust (edition 2024), single binary crate `nc`. Dependencies: `clap` (`derive`),
`tiff`, `image`, `palette`, `lcms2`, `serde`/`serde_json`, `rayon`,
`kamadak-exif`, `roxmltree` (read-only XML — parses the SilverFast XMP packet
for input provenance), `libc` (one `sysctlbyname("hw.memsize")` call on Darwin for
the memory preflight's warn tier; Linux reads `/proc/meminfo` with no dep)
(see `Cargo.toml` for versions; bump with `cargo add`).

- `cargo build` — build · `cargo test` — all tests · `cargo test <name>` — one test
- `cargo clippy --all-targets` — lint (keep clean)
- **Before pushing, match CI** (`.github/workflows/ci.yml`, runs on every PR):
  `cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
  `cargo build` → `cargo test`. The gate is strict — warnings fail the build.
- **Determinism is per build/architecture, not cross-platform — write golden
  tests accordingly.** Tests run locally (macOS/aarch64) *and* in CI (x86_64
  Linux), so a green local `cargo test` is not proof CI is green. Reconstruction's
  transcendental FP (`powf` / `10^` / `log10`) differs by ~1 ULP across libm
  implementations, and the lcms2 color transform + embedded ICC bytes differ by
  target — so a checked-in **bit-exact hash of a whole encoded TIFF, or of
  reconstruct output over a full frame, is green on the capture host and red on the
  other target** (design-spec §8 scopes byte-identity to a single
  build/architecture). Pin bit-identity with the small **curated per-pixel**
  vectors in `pipeline::stages::golden` (captured from the reference code) — those
  specific values happen to agree across libm; never checksum a full frame, an
  encoded file, or post-lcms2 (color-transformed) pixels in a cross-platform gate.
  Note what `golden` therefore does **not** cover: `assert_golden` pins
  `reconstruct_and_print`, i.e. **pre**-color-transform pixels. Nothing committed
  guards `color::to_output`'s output across targets, so a change there is verified
  by same-machine before/after comparison, not by a checked-in vector.
- **Only `aarch64-apple-darwin` is installed here, so `#[cfg(target_os = "linux")]`
  code never compiles locally** — all four gates pass green with a type error
  sitting in a Linux-only branch, and CI is the first place it builds
  (`cargo check --target x86_64-unknown-linux-gnu` can't run: no target std, and
  lcms2-sys would need a cross C toolchain). When adding platform-gated code, gate
  only the *I/O* and keep the parsing/logic in un-gated helpers with unit tests, so
  every target compiles and exercises it — `pipeline::memory`'s
  `parse_meminfo_total` / `parse_cgroup_limit` / `parse_self_cgroup` are the
  pattern. Same reasoning applies to a `cfg` trichotomy used as a function's tail
  expression: bind each branch to a `let` instead, or adding a statement later
  turns the surviving block into an `unused_must_use` error on one target only.
- `Cargo.lock` is committed (binary crate). The crate-level `#![allow(dead_code)]`
  is gone; the remaining allows are narrow, documented item-level ones
  (`algo/mod.rs`, `pipeline/working_space.rs`, `pipeline/render_split.rs` — the
  last two for the `AcesCgImage` accessors and the shared display stage awaiting
  their SDR/HDR consumers) for API surface the single Step-1 path doesn't
  exercise — don't add new ones without a comment saying who will use it.
- **Codex review on a worktree.** `/codex:review` is a codex-plugin *command*
  (not a skill) that reviews the **current directory's** git state — so run it
  *from inside the worktree you want reviewed*. Pick the scope to match where the
  work lives: use **`--scope working-tree`** for **uncommitted** changes (diff vs
  `HEAD`) — the state our feature worktrees are usually left in for review; if the
  work is **committed** on the branch, `working-tree` would review nothing, so use
  a base/branch comparison (`--scope branch --base <ref>`) against the branch's
  fork point. Don't lean on the *default* base compare when the worktree's base
  lags `origin/main`: it shows confusing reverse-diffs of already-merged work —
  pass the intended base explicitly. **Neither scope covers both halves, and the
  default silently picks one** (verified in the companion's `scripts/lib/git.mjs`):
  `working-tree` sends staged + unstaged diffs plus untracked file contents —
  but **inlines an untracked file only if it is text under 24 KiB**
  (`MAX_UNTRACKED_BYTES`), else it sends just a `(skipped: …)` marker, so a big
  new module is silently reviewed as a filename; `git add -N <path>` first
  routes it through the size-unlimited unstaged diff. It sends
  no commits; `branch` sends a two-dot `merge-base..HEAD` range, so it sees
  commits but neither the working tree nor untracked files; and `--scope auto`
  (the default) takes working-tree whenever the tree is dirty **at all**,
  otherwise branch. With committed *and* uncommitted work, one `auto` run
  reviews the dirty tree and never looks at the commits — run both scopes. The
  command wraps
  `node "<codex-plugin>/scripts/codex-companion.mjs" review --wait --scope
  working-tree` (`--wait` = foreground/verbatim, `--background` = detach; the
  plugin path is under `~/.claude/plugins/cache/openai-codex/...`). It is
  review-only — no fixes, no model override, no custom focus text; use
  `/codex:adversarial-review` for custom framing. **Gotcha:** `/codex:setup`
  verifies install + auth but **not** reviewer-model support — if a review 400s
  with "model ... requires a newer version of Codex," upgrade the Codex CLI or
  switch its default model (the reviewer picks the model, and a review routed
  through `/codex:rescue` is *not* tracked by `/codex:status`).

## Conventions

- **Skill layout.** Agent skills live in `.agents/skills/` (the directory Codex
  CLI scans; Codex invokes them as `$<name>`); `.claude/skills/` holds relative
  symlinks into it for Claude Code. Exception: `review-fix-loop` ships as two
  intentionally divergent variants — `.claude/skills/` (Claude Code two-engine
  loop) and `.agents/skills/` (tool-agnostic) — never symlink one to the other.
- **Agent layout is deliberately *not* mirrored.** Subagent definitions live in
  `.claude/agents/` only — `nc-reviewer` (the review engine) and `nc-fixer` (the
  single actor allowed to edit during a review loop), both driven by the
  `review-fix-loop` skill. There is no `.agents/` counterpart and no symlink:
  Codex CLI doesn't read `.claude/agents/`, and that is the point — Codex is the
  loop's *other* engine, so it must not inherit `nc-reviewer`'s project primer.
  Each agent carries a summary of the rules below and is told **this file wins on
  conflict**, so update CLAUDE.md first and treat an agent's primer as a lossy
  cache, not a second source of truth.
- **Value terms (high/low/bright/dark).** Before using these in code or docs, read
  design-spec §4 "Terminology & value domains". A pixel lives in several
  **per-channel** spaces; as scene luminance rises, `transmission ↓ · density ↑ ·
  positive ↑ · output ↑` (transmission runs backwards). "bright"/"dark" always mean
  the *scene*, never a raw pixel value — the film base is the highest transmission
  yet renders to black. `Dmin` is a transmission (the film base); `Dmax` is a
  **scalar** display-white anchor in **density** units — never conflate the two.
- **Prefer pure functions over classes/structs-with-behavior.** Each pipeline
  stage is a pure `(input, params) -> output` function; the CLI is the only
  orchestrator. (Matches the global guidance in `~/.claude/CLAUDE.md`.)
- **Every conversion knob is a CLI flag and a recipe-JSON key** — nothing
  reachable only from code. Determinism is required: same inputs + params ⇒
  identical output. The JSON report goes to stdout cleanly (logs/warnings to
  stderr) so agents can pipe it. Mechanically, a knob spans four coupled spots:
  a field in the CLI `*Overrides` struct (`cli.rs`), the recipe `*Params` struct
  (`types.rs`), a `merge` arm, and usually a `validate` check — a forgotten
  `merge` arm silently makes the flag a no-op, so add a merge test for new knobs.
- **Recipe shape mirrors design-spec §9.** A flag's recipe key lives under the
  stage section §9 assigns it (`--export-ir` ⇒ `input.export_ir`); because every
  recipe struct uses `deny_unknown_fields`, a misplaced key silently rejects
  docs-shaped recipes — keep structs and §9 in sync. Model a set of
  mutually-exclusive knobs as **one enum field** (e.g. `FilmBaseSource`,
  `InputColor`), not parallel `Option`/bool fields: independent fields can encode
  illegal combinations and silently break the flags-win merge.
  The tagged reconstruction schema (§9's `reconstruction.*` paths) is the
  **shipped** schema: one tagged `reconstruction` object (`schema_version` 1)
  selects `simple`/`density` and the density curve. The legacy `algorithm` +
  top-level `density`/`sigmoid`/`simple` forms are **rejected with migration
  errors** — never re-add them as recipe keys or aliases.
- **Fail loudly.** Map errors to the documented exit codes (design spec §11);
  surface clipping / unsupported-input as explicit errors or report warnings,
  never a quietly wrong image.
  - *lcms2 gotcha:* `Transform::transform_in_place` (`cmsDoTransform`) is
    infallible — Little CMS reports runtime transform failures only via the
    process-global `cmsSetLogErrorHandler`. `color.rs` uses the **global**
    context, and the safe `lcms2` wrapper only exposes per-`ThreadContext`
    handlers, so `cli` installs the global handler via `lcms2-sys` FFI at
    startup (sets an `AtomicBool` + logs to stderr); `run_convert` clears the
    flag before the render and checks it after, turning a CMS fault into a loud
    error instead of a silently unconverted image.
  - *Film-base gotcha:* an explicit `--film-base` is CLI-validated; a
    `Region`/`Auto` base is estimated from pixels at runtime. Since
    the `auto-base-redesign` task, `film_base::estimate` **guards the resolved
    base finite-and-positive on every channel at birth** (a region on the dark
    holder → zero channel now errors loudly there, not silently downstream). The
    per-algo guards (`algo/simple.rs`, `algo/density.rs`) remain as
    defense-in-depth for any base reaching a converter directly.
  - *Clamping boundary:* range-clamp to the output gamut **only** at the u16
    encode step; color/algo stages pass values through unclamped (float output
    preserves the current rendered working values). `io::encode` counts every
    clamped and non-finite (`NaN`)
    sample into `EncodeReport` (`types.rs`) so the loss rides back to the
    orchestrator as a report warning (`--strict` promotes it) — never clamp
    silently anywhere.
  - *Output-preset atomicity is deliberately asymmetric — don't unify it.* A named
    preset rejects a **non-default resolved value** for `output.hdr` /
    `output_profile` / `bigtiff` (either provenance), but rejects `--output-sdr` by
    **flag presence**. `--output-sdr` has no recipe spelling (`output.hdr = false`
    *is* the serde default, indistinguishable from omission), so no value rule can
    see it, and unlike `--bigtiff auto` its documented meaning ("force 16-bit
    integer") is one the master contradicts. Collapsing the two rules silently
    writes an f32 master when the user asked for 16-bit.
  - *`validate` is not the whole `convert` gate.* Every rule inside it reads only
    the resolved config — which is why `roll` and each per-frame override share it
    verbatim. `convert` must call **`validate_convert`**, which composes it with the
    flag-presence check above; `output/presets` is the next orchestrator that has to.
  - *`--strict` assertions need an IR-free fixture.* `tests/fixtures/hdri-64bit.tif`
    carries an IR plane, so every frame emits the "IR preserved but not used"
    warning and **any** `--strict` run on it exits non-zero regardless of the
    behavior under test. Use `tests/fixtures/hdr-48bit.tif` (IR-free) whenever a test
    must prove a *specific* warning is strict-promotable, and add a no-override
    control run so the assertion is falsifiable.
  - *Changing a default render trips the drift gate — read its failure message.*
    `version::PIPELINE_FINGERPRINTS` pairs each `pipeline_version` with hashes over
    `film_base::estimate`, `reconstruct_and_print` on the curated `stages::golden`
    vectors, and the default recipe JSON. Bumping is deliberately **not** free: a
    version with no recorded row panics. **Never edit a historical row's `render`
    in place** — that silently makes one version label two behaviors. A new
    *opt-in* knob with a neutral default legitimately moves only the `recipe`
    hash; refresh that row without bumping. The gate stops before lcms2, so it
    covers neither the output transform nor `io::{decode,encode}`.
  - *`params` is a reserved top-level recipe key.* `split_envelope` tells a
    `{meta, params}` sidecar from a bare legacy recipe by that key alone, so adding
    a `params` stage section to `ResolvedConfig` would silently reinterpret every
    recipe as an envelope. A test asserts it stays absent.
  - *`build.rs` git gotcha (cost me two wrong attempts):* in a **linked worktree
    `.git` is a file**, so `cargo:rerun-if-changed=.git/HEAD` names a path that
    doesn't exist — resolve it with `git rev-parse --git-path`. And watching `HEAD`
    alone never notices a commit: on a branch it's a `ref:` pointer whose contents
    don't change, so `refs`/`packed-refs`/`index` must be watched too, or the
    binary reports the **parent** commit and `dirty: true` after every commit.
    `git rev-parse` also walks *up*, so verify `--show-toplevel` matches
    `CARGO_MANIFEST_DIR` or a nested checkout stamps an unrelated repo's commit.
- **Verify against real sample files.** There is no public spec for the SilverFast
  HDRi on-disk layout; the decoder must be validated against the user's actual
  scans and degrade gracefully on unrecognized layouts. Sample scans live in the
  [nc-assets Google Drive folder](https://drive.google.com/drive/folders/1qXE2jF3MuVnQ2sW0pGTp3URwBJuf_LV6) — the
  canonical source (50–160 MB each) — reached locally via a **machine-local
  symlink** `../nc-assets → <GoogleDrive>/temp/nc-assets` (each machine points its
  own; not committed). The folder is organized `rolls/<roll>/`, `samples/`,
  `converted/{nc,nlp}/`, with a tracked `manifest.json` inventory at its root
  (roles, dims, `ir_present`, checksums, NLP↔source links) — regenerate/validate
  it with `python -m nctool manifest generate` / `validate` (or the
  `asset-manifest` skill; `scripts/analysis/generate_manifest.py` is now a thin
  shim into `nctool`). Decoder
  unit-test fixtures are committed separately under `tests/fixtures/`.
  **Never read them into context**; inspect IFD
  structure with `exiftool` (`tiffinfo` is not installed here) or `nc inspect`, and
  exercise the pipeline on them either with a throwaway `#[ignore]` test that calls
  `io::decode` and prints only derived numbers, or via the committed
  `scripts/real-scan-verify/` harness (staged verification driving the `nc` binary,
  derived numbers only — see its `README.md`). Note: real scans are laid out
  `dark holder → thin inset rebate → picture` (the rebate is not the outer margin),
  so `--auto-base` is best-effort; measure `Dmin` once from an unexposed reference
  and reuse it via `--base-region`/`--film-base` (design-spec §8).
- For any library API, fetch current docs via Context7 rather than relying on
  memory.
