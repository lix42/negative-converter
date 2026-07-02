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
  CLI surface, parameter reference, exit codes, roadmap). `docs/design-spec.html`
  is the same content for humans; **edit both together** when the design changes.
- `docs/TASKS.md` — the plan: distilled design, the canonical dependency graph,
  and the phased task checklist. This is the control center for what to build next.
- `docs/tasks/<name>.md` — per-task spec (goal / design / how-to-verify / deps).
- `docs/progress.md` — execution log; read the relevant section before starting a
  task, append to your task's section as you work.
- `docs/negative-convertor-research-report.md` — background research (image
  science, library survey). Context, not spec.

## Task-tracking workflow

Work is planned and tracked with the `task-tracking` skill (the `/tasks:*`
commands). `docs/TASKS.md` is the authoritative status (the `[ ]`/`[~]`/`[x]`
checkboxes) and the dependency graph; `docs/progress.md` is the narrative. When
picking up work: consult `TASKS.md` for what's unblocked (a task is executable
when all its deps are `[x]`), read the task file and the relevant `progress.md`
sections, then implement. Keep the Mermaid diagram, the canonical dependency list,
and per-task Dependencies sections in sync — `TASKS.md` wins on conflicts.

## Architecture (Step 1)

A pure-function pipeline orchestrated by a thin CLI layer. Stages:

```
decode → film-base estimate → algorithm (simple|density) → output color transform → encode
```

- All processing is **32-bit float in a linear working space**; bit-depth
  reduction happens only at the final encode. HDR is a first-class concern.
- **Density conversion and print rendering are separate sub-stages** — the core
  color-fidelity rule. Don't collapse them.
- Algorithms are **pluggable** behind a `Converter` trait. Step 1 ships two:
  `simple` (channel-inversion baseline / debug / B&W) and `density` (Cineon /
  darktable-negadoctor density-domain, the default).
- The **IR channel** (HDRi 64-bit input) is decoded and **preserved but not acted
  on** in Step 1; IR-based dust removal is a roadmap follow-up. Carry it through,
  don't consume it.
- Module map (`src/`, scaffolded — `types.rs` filled, other modules are stubs):
  `types.rs` (shared types), `io/{decode,encode}.rs`,
  `pipeline/{film_base,color,stages}.rs`, `algo/{mod,simple,density}.rs`,
  `cli.rs`, `main.rs`. `main`/`cli` are the only orchestrators; stages stay pure.

### Stack / commands

Rust (edition 2024), single binary crate `nc`. Dependencies: `clap` (`derive`),
`tiff`, `image`, `palette`, `lcms2`, `serde`/`serde_json`, `rayon`,
`kamadak-exif` (see `Cargo.toml` for versions; bump with `cargo add`).

- `cargo build` — build · `cargo test` — all tests · `cargo test <name>` — one test
- `cargo clippy --all-targets` — lint (keep clean)
- **Before pushing, match CI** (`.github/workflows/ci.yml`, runs on every PR):
  `cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
  `cargo build` → `cargo test`. The gate is strict — warnings fail the build.
- `Cargo.lock` is committed (binary crate). `main.rs` carries a temporary
  `#![allow(dead_code)]` while stages are stubs — remove it once
  `pipeline-orchestration` wires them together.

## Conventions

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
- **Fail loudly.** Map errors to the documented exit codes (design spec §11);
  surface clipping / unsupported-input as explicit errors or report warnings,
  never a quietly wrong image.
  - *lcms2 gotcha:* `Transform::transform_in_place` (`cmsDoTransform`) is
    infallible — Little CMS reports runtime transform failures only via the
    process-global `cmsSetLogErrorHandler`, so `main`/`cli` must install one
    (lcms2 `ThreadContext::set_error_logging_function`) at startup.
  - *Clamping boundary:* range-clamp to the output gamut **only** at the u16
    encode step; color/algo stages pass values through unclamped (f32 output is
    HDR/scene-referred). `io::encode` counts every clamped and non-finite (`NaN`)
    sample into `EncodeReport` (`types.rs`) so the loss rides back to the
    orchestrator as a report warning (`--strict` promotes it) — never clamp
    silently anywhere.
- **Verify against real sample files.** There is no public spec for the SilverFast
  HDRi on-disk layout; the decoder must be validated against the user's actual
  scans and degrade gracefully on unrecognized layouts. Sample scans live at
  `../nc-assets/{48,64}bit-full/` and `~/Pictures/scan/` (50–160 MB each) — **never
  read them into context**; inspect IFD structure with `tiffinfo`, and exercise the
  pipeline on them with a throwaway `#[ignore]` test that calls `io::decode` and
  prints only derived numbers (remove it after). Note: real scans are laid out
  `dark holder → thin inset rebate → picture` (the rebate is not the outer margin),
  so `--auto-base` is best-effort; measure `Dmin` once from an unexposed reference
  and reuse it via `--base-region`/`--film-base` (design-spec §8).
- For any library API, fetch current docs via Context7 rather than relying on
  memory.
