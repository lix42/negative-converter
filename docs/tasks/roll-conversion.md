# Roll conversion (batch with a shared frozen recipe)

## Goal

Add a **roll workflow**: convert a batch of frames from one roll with a single,
shared, frozen recipe, so the whole roll is color-consistent and reproducible —
distinct from converting one frame in isolation. Roll-fixed parameters (`Dmin`,
`Dmax`) are resolved once and reused; frame-local parameters stay per frame.

## Background

The tool is single-input today (`nc convert <one file>`). The design discussion
(2026-07) established two workflows:

- **Roll converting (strongly preferred):** detect the film base (and `Dmax`)
  once, apply the same config to every frame. This is where correctness lives —
  one reliable `Dmin` / `Dmax` per roll keeps frames consistent.
- **Single converting:** a lone frame with no shared base; best-effort per frame.

The real model is *which parameters are roll-fixed vs frame-local*: `Dmin` (base)
and — after `dmax-reference` — `Dmax` are roll-fixed; print / exposure and any
per-frame `--auto-*` modes are frame-local. This command is the **batch-apply
scaffold**: it converts N frames from a single **provided** frozen recipe
(hand-authored or via `--params`), independent of *how* that recipe was produced —
so it stands alone and is verifiable without any auto-detection. The automatic
cascade that *generates* the recipe (and later wires in as the default "auto mode"
on a batch) is the dependent `base-acquisition-planner` task — a separate, later
integration, **not** a prerequisite here.

Extends design-spec §12 roadmap item 6 ("Roll-level presets & batch mode").

## Design

- Batch input: multiple files / a directory / a glob → per-frame outputs (a naming
  scheme), plus a roll-level JSON report (per-frame status + the shared recipe).
- One **frozen shared recipe** carries the roll-fixed params (`film_base`,
  `density.dmax`); frame-local params may be overridden per frame.
- Architecture: **plan → recipe → apply.** This task owns **apply**: deterministic
  replay of a *given* frozen recipe over N frames. The **plan** step (resolving the
  roll-fixed params — the messy, heuristic part) is the separate
  `base-acquisition-planner`; roll-conversion does not require it — a recipe from
  `--params` or hand-authored suffices. Keeps the pure pipeline pure (CLI
  orchestrates; stages stay pure).
- Determinism unchanged: same batch + same recipe ⇒ identical bytes per frame.
- Single-frame `nc convert` remains; roll mode is additive.

## How to Verify

- Convert a Phoenix roll (`936` / `956` / `958`) from one **hand-authored** frozen
  recipe (`--params`, no planner needed) → per-frame positives + a roll report; the
  shared `Dmin` / `Dmax` appear once.
- Re-running the batch is byte-identical (determinism).
- A frame-local override (e.g. print exposure) applies to just that one frame.

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
- [Display-range white anchor (Dmax)](dmax-white-anchor.md)
