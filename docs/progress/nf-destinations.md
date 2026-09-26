# Hanten — nf-destinations Progress Log

Execution log for the `nf-destinations` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status; this file is the
narrative beside it.

One `##` section per task in this epic, named by the bare task name. Read this
whole file before starting a task in this epic, and read other epics' `Epic
summary` sections when you depend on them. Append entries — don't rewrite earlier
ones.

## Epic summary

Where a render can go: the destination set, the direct Adobe RGB combination, memory profiles, and which destination the default resolves.

**The destination set has landed** (`preset-set`, 2026-09-26). Under `--new-flow` a
destination is four separate knobs — `--range sdr|hdr`, `--transfer
native|linear|pq|hlg`, `--gamut display-p3|adobe-rgb|bt2020`, `--container
tiff|jpeg|avif` (recipe `output.display`) — or `--film-master` (recipe `output`:
`"film-master"`). What other epics need:

- **One table, `destination::ROWS`**, drives resolution, refusals, remedies and the
  container. Adding a destination is adding a row (a `NotYet` row is refused naming its
  task). An unset axis is derived — its default when a consistent row has it, else the
  one value left, else a refusal — independent of which rows are ready. The report's
  `new_flow.destination` states every resolved axis and replays exactly.
- **HDR** is peak `1000/203` in BT.2020; `hdr::from_new_chain` clamps to the peak and
  counts (`new_flow.peak_clamp`, folded into `loss`), then the legacy HDR encoders run.
- **The film master refuses every stage it does not run**, keyed on "asks for" (neither
  default nor identity) per stage.
- Memory arms per buffer shape: `NewFlowU16Tiff` (measured for SDR), `NewFlowF32Tiff`
  and `NewFlowAvif` provisional.
- nctool keys metrics on (gamut, transfer); a review matrix may state a `destination`.

## preset-set

**Status:** done
**Updated:** 2026-09-26

- 2026-09-19: created with the new-flow plan. Goal: the destination set.
- 2026-09-25: started. The user settled the three open questions: a destination is
  **separate knobs** (range × gamut × container, perhaps the gain-map dialect), not a
  name per combination, with unsupported combinations refused at the CLI; the selectors
  are **new-flow only**, with their own `output` recipe section, and `--output-preset`
  is refused under `--new-flow`; **HDR destinations stay available** before the flip
  where possible (gain map via `render_pair` + `gain_ratio`, PQ, HLG, linear float).
  Recorded in the task file with the design risks to settle before coding.
- 2026-09-25: shape agreed with the user — four axes (`--range`, `--transfer`,
  `--gamut`, `--container`) driven by one table, unset axes derived from it,
  `film-master` its own selection. The per-channel gain-map JPEG split out to
  `gain-map-destination`, because the tree's gain-map writer packages only a
  single-channel luminance map (the legacy XMP dialect cannot signal more).
- 2026-09-25: implemented, awaiting review. What landed:
  - **`src/destination.rs`** is the set: four axes, one table (`ROWS`), and a pure
    `resolve`. Unset axes are derived in order range → transfer → gamut → container
    (default if a consistent row has it, else the one value left, else refuse), **ignoring
    readiness**, so `--range hdr` names the gain-map row and is refused as not yet rather
    than silently becoming a TIFF. Every remedy a refusal offers is proven to resolve by
    an exhaustive test over all stated combinations; offers are the fewest flags to add,
    computed over what the run stated (a flag cannot remove a recipe axis).
  - Written today: SDR `native` TIFF in Display P3 (default, byte-identical to before) or
    Adobe RGB; HDR BT.2020 `linear` f32 TIFF, `pq`/`hlg` u16 TIFF or AVIF.
    `DestinationGamut::Bt2020` reuses the pinned `ACESCG_TO_BT2020` / `BT2020_LUMA`.
  - **HDR hand-off** `hdr::from_new_chain` clamps to `[0, 1000/203]` and counts what it
    clamped (`new_flow.peak_clamp`, folded into `loss`, so `--strict` sees it), then the
    legacy HDR encoders and report blocks are reused unchanged.
  - **`film-master` refuses every stage it does not run**, one rule per stage, keyed on
    "asks for" (not the default and not the identity): scene correction, the look, fit
    range. The legacy chain already refused the same; the look-only rule the task named
    would have silently ignored a roll's measured white balance.
  - The HDR "SDR-range signal" warning is flow-aware (new flow names `--exposure` /
    `--range sdr`); legacy text byte-identical.
  - Memory: `NewFlowSdrTiff` → `NewFlowU16Tiff` (SDR and coded HDR share it);
    `NewFlowF32Tiff` and `NewFlowAvif` are **provisional, counted not measured** —
    `memory-profiles` owes the measurement.
  - nctool: `metrics.space_for_destination` keys on (gamut, transfer) and refuses an axis
    left to derivation; a review matrix may state `destination` instead of
    `output_preset`. One matrix still cannot hold a reference cell and a new-flow cell.
  - Review: nc-reviewer plus a cold stand-in (Codex was out of credits); 16 findings and
    two follow-ups fixed.
- 2026-09-26: done. Rebased onto `nf-retire/regional-balance`; the ship review (Codex,
  back online, and `ship:diff-reviewer`) found three more, all fixed:
  - **A roll's per-frame `output.display` now merges axis by axis.** `DisplayAxes`
    writes only stated axes, so two one-key axis objects looked like an
    externally-tagged enum switch to `cli::merge_json` and replaced the shared recipe's
    axes; `is_variant_switch` now exempts `destination::AXIS_KEYS`. Any future sparse
    (`skip_serializing_if`) struct in a recipe hits the same heuristic.
  - The suffix refusal names the film master the way the user chose it (flag or recipe).
  - `pipeline::hdr::sdr_range_warning` takes an `SdrRangeLevers` enum, not `Flow`:
    `Flow` never reaches a stage module.
  - For dependents: `direct-preset` is now a rendering question (the gamut is a knob);
    `memory-profiles` owes measurements for `NewFlowF32Tiff` and `NewFlowAvif` (and the
    coded-HDR use of `NewFlowU16Tiff`); `default-destination` moves axis defaults, not a
    name; `gain-map-destination` adds a ready row, after which `--range hdr` alone
    resolves to it.

## direct-preset

**Status:** not started
**Updated:** 2026-09-19

- 2026-09-19: created with the new-flow plan. Goal: the direct destination for external editing.

## memory-profiles

**Status:** not started
**Updated:** 2026-09-19

- 2026-09-19: created with the new-flow plan. Goal: a memory profile per destination.

## default-destination

**Status:** not started
**Updated:** 2026-09-19

- 2026-09-19: created with the new-flow plan. Goal: which destination the default resolves.

## gain-map-destination

**Status:** not started
**Updated:** 2026-09-25

- 2026-09-25: split out of `preset-set` (user decision). Goal: the new chain's HDR JPEG
  with a per-channel ISO 21496-1 gain map.
