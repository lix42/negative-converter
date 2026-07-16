# Prototype — perf lab benchmarks + stage instrumentation

**Status:** Parked prototype. **Not for merge** as-is (2026-07-15).
**Branch:** `prototype/perf-bench-instrumentation`, based on `origin/main` at the
time of writing (post #20/#21/#22).

## Why this branch exists

This was the original `perf-instrumentation` task's implementation: a *lab-setting*
performance harness. On review we decided it answers the wrong question. We don't
primarily want to micro-benchmark kernels on a synthetic image in a controlled
setting — we want to understand how `nc` behaves **in the real world** on the
user's actual scans (how big the images are, how long a real conversion takes),
emit that as machine-readable metadata, and eventually ship it to a telemetry
server in the background.

That real goal is now tracked separately as the **`perf-telemetry`** task
(embedded, opt-in, JSON telemetry record per conversion; no new binary/subcommand).
This branch is preserved because parts of it are directly reusable there, and
because the benchmarks remain genuinely useful later — just not the priority now.

## What this branch contains

1. **Crate split: binary-only → library + thin binary.** New `src/lib.rs`
   (`pub mod algo/cli/io/pipeline/types`); `src/main.rs` reduced to `use nc::cli;`.
   Needed so a separate bench crate can link the pipeline. Selected internals were
   widened `pub(crate)` → `pub` (`density::{DensityImage, to_density, render}`,
   `encode::encode_to_writer`) — documented in `lib.rs`.
2. **Criterion benches** (`benches/kernels.rs`, `harness = false`): `to_density`,
   `render` (auto-Dmax vs none), `simple` invert, u16/f32 encode, over a
   deterministic ~3.1 MP synthetic negative. Dev-dependency `criterion`; baselines
   recorded in `docs/progress.md`. Not a CI gate.
3. **`tracing` span instrumentation**: stage-boundary spans emitted to **stderr**
   (stdout stays report-only), filtered by `-v`/`--quiet`/`RUST_LOG`. New runtime
   deps `tracing` + `tracing-subscriber`.
4. **Per-stage `timings` block in the convert JSON report** (`decode_ms`,
   `film_base_ms`, `algorithm_ms`, `color_ms`, `encode_ms`, `ir_export_ms`),
   serialize-only, never fed back into the pixels.

## What is reusable for `perf-telemetry`

- The **per-stage timing measurement** (Instant pairs in `stages::render` +
  the orchestrator) and the **`timings` report block** are close to what the
  telemetry record needs — the telemetry task can lift the measurement and reshape
  the output channel.
- The **lib/bin split** is optional for telemetry but harmless, and keeps the door
  open for the benches.

## What is NOT wanted (for now)

- The criterion benches as a deliverable / anything resembling a separate perf
  entrypoint. Telemetry must be **embedded in the normal `nc` run**, opt-in via a
  flag, not a lab tool.

## How to resume

Check out `prototype/perf-bench-instrumentation`. It builds and passes the full
gate (`fmt`/`clippy -D warnings`/`build`/`test`) on its base. Cross-reference the
`perf-telemetry` task file for the chosen real-world direction before reviving any
of this.
