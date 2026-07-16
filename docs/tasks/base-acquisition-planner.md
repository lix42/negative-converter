# Base-acquisition planner (the roll / single cascade)

## Goal

Implement the automatic **acquisition cascade** that resolves a roll's `Dmin` and
`Dmax` from whatever the user provides, emits a **frozen recipe with provenance +
confidence**, and decides when to fall back from roll to single conversion. This
is the "plan" phase of `roll-conversion` and the brains of its auto mode.

## Background

Design discussion (2026-07). The cascade, in decreasing reliability:

**`Dmin` (film base):**
1. User provides a **dedicated unexposed frame** → `estimate` reads it (Tier 1).
2. Else a scan **with a rebate**: user gives the region (happy path) → else
   `--auto-base` (with IR holder masking). Content estimation is **never
   auto-invoked** here — it is scene-dependent and can silently wash out (see
   `film-base-content-fallback`), so it is used **only on explicit opt-in**;
   otherwise the planner drops loudly.
3. Else **scan all frames** with auto-base and take a **high-confidence,
   cross-frame-agreeing** result; if none, **drop loudly to single conversion**
   (not silently to content).

**`Dmax`:** measured from a **fully-exposed reference frame** (Tier 1,
`dmax-reference`) → else per-stock constant / a nominal corrected-density anchor
(in density units — see `dmax-reference`).

**Single mode:** try auto-base once; else **drop loudly**. Content `Dmin` is used
only when **explicitly opted in** (never a silent fallback); `Dmax` falls back to a
fixed anchor.

Principles established:

- **Plan → frozen recipe → deterministic apply.** All heuristics live here and
  resolve to a recipe; conversion is replay. The recipe records *which rung won,
  which frame / region, and the confidence* (auditable, agent-readable) — this is
  what keeps the deterministic core clean.
- **Cross-frame agreement** is a strong corroborator (the roll-mode analog of
  cross-edge agreement): the same base from ≥ 2 frames beats one frame's edge.
- **Auto-detect reference frames**: an unexposed frame is whole-frame uniform
  bright base (extreme rebate); a fully-exposed frame is whole-frame dark-RGB /
  bright-IR. Both must be **confirmable / overridable**, never silently assumed (a
  blank-sky or lightbox shot can fool them). Two references (roll head + tail)
  cross-check for confidence.
- **Roll → content-`Dmin` is the weak rung:** "one content `Dmin` for the roll" is
  ill-defined (scene-dependent). Prefer a **user-designated** frame's content
  base, or **drop to single**, loudly — never synthesize a roll base from mixed
  frames.

**Superseding note:** this task **absorbs the source-sequencing, selection, and
"which source won" provenance** that `auto-base-redesign` and
`film-base-content-fallback` only sketch — those stay *detectors / estimators*;
ordering, selection, cross-frame agreement, and provenance live here.

## Design

- A planner that runs the ladder over a batch and emits the frozen roll recipe
  (`film_base.source` resolved to explicit; `density.dmax` resolved), plus a
  provenance block (rung, source frame / region, confidence, agreement spread).
- Reference-frame auto-detection (unexposed → `Dmin`; fully-exposed → `Dmax`),
  confidence-gated with a `--reference` / `--dmax-reference` override.
- Cross-frame agreement check with a reported spread.
- Drop-to-single decision — loud, recorded in the report.
- Pure / deterministic given the batch; no hidden state.

## How to Verify

- Phoenix batch with `933` (unexposed) + `1010` (fully-exposed) present → planner
  auto-detects both, freezes `Dmin` / `Dmax`, records provenance.
- Batch with **two** frames whose rebates agree (e.g. Ektar `991` + another
  detectable-rebate frame) → cross-frame agreement corroborates; confidence +
  spread recorded. With a rebate on **only one** frame → a single *uncorroborated*
  candidate: the planner records low confidence and either uses it with a warning
  or drops to single per the gate (cross-frame agreement needs ≥ 2 frames).
- Batch with neither → **loud drop-to-single** (content fallback only if
  explicitly opted in, never automatic).

## Dependencies

- [Roll conversion](roll-conversion.md)
- [Robust auto film-base detection](auto-base-redesign.md)
- [IR-assisted film-holder detection](ir-holder-detection.md)
- [Content-based film-base fallback](film-base-content-fallback.md)
- [Roll-fixed Dmax from a fully-exposed reference frame](dmax-reference.md)
