# Conversion versioning & baseline comparison

## Goal

Stamp every conversion with a machine-readable identity and a **behavioral
pipeline version**, so outputs are attributable and conversion quality /
performance can be compared across versions of `nc`. Establish **`v0` = the
current default behavior** as the baseline (recorded in
`docs/reports/v0-baseline.md`).

## Background

To tell whether a change (auto white-balance, a tone curve, the Dmax rework)
*improved* or *regressed* the conversion, every output must record **what produced
it**, and there must be a stable label that bumps when default behavior changes.
Today the JSON report carries the resolved params but **no version identity**, so
two outputs from different builds are indistinguishable and there's nothing to
diff a baseline against. The v0 report is the first data point; this task makes
version comparison a first-class, repeatable capability.

## Design

Three layers of identity, written into the JSON **report**; mirrored into the
sidecar **only** via a backward-compatible envelope (see the round-trip note
below), never as bare recipe keys:

1. **Build identity** — crate semver (`nc_version`, 0.1.0 today) + git commit
   (short hash + dirty flag) via `build.rs`. `nc --version` prints them too.
2. **Pipeline version** (the comparison axis) — an explicit integer
   `pipeline_version`, **independent of semver**, that bumps *only* when default
   conversion behavior changes (default algorithm/params, tone curve, auto-WB,
   Dmax handling). Starts at `0` (= v0). **Enforced by a golden-output test:** if a
   committed fixture's default output changes and the version isn't bumped, CI
   fails — so the behavioral version can't silently drift.
3. **Params hash** — a hash of the resolved effective params (canonical
   `--dump-params` JSON), so identical configs are detectable across frames and
   versions.

**Preserve the sidecar `--params` round-trip.** The sidecar is the serialized
resolved recipe and is intentionally reloadable via `--params`, whose schema uses
`deny_unknown_fields`. Do **not** add `nc_version` / `pipeline_version` /
`params_hash` as bare keys into the recipe object — that makes every new sidecar
(and this metadata) fail to reload. Either keep identity in the **report only**,
or wrap the sidecar as `{ "meta": { …identity… }, "params": { …recipe… } }` with a
loader that still accepts a bare (legacy) recipe. The established recipe round-trip
must keep working.

**Comparison harness.** A benchmark manifest (a fixed set of scans + recipes —
e.g. the Ektar / Phoenix rolls) and a `compare` step that converts the set under
two builds and emits a diff keyed by `pipeline_version` + commit: per-channel mean
ΔRGB, clip-fraction delta, and timing per frame. Deterministic: re-running the same
build yields a zero diff.

**Boundaries / connections.** Quality metrics beyond mean ΔRGB (ΔE2000, SSIM) are
the QA-harness roadmap item (design-spec §12 item 7) — keep this task's metric set
small and extend there. Timings connect to `perf-instrumentation`. This task owns
the *identity stamping* + *version label* + *comparison scaffold*, not the full
metric suite.

## How to Verify

- Every **report** carries `nc_version`, git commit, `pipeline_version`,
  `params_hash`; the sidecar stays reloadable via `--params` (bare-recipe
  round-trip preserved — any identity in the sidecar sits in a `meta` envelope,
  not the recipe body).
- Changing a default output without bumping `pipeline_version` fails the golden
  test; bumping it passes.
- `compare` on the benchmark set produces a version-keyed diff report; re-running
  the same build yields zero diff.

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
