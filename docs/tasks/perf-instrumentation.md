# Performance Instrumentation

## Goal

Make nc's performance measurable — locally and agent-readably — before release:
per-stage timings in the JSON report, structured tracing for humans, and
benchmarks that pin the hot kernels against regression. No remote telemetry
(that is a separate, opt-in roadmap item, §12); everything here is local and
deterministic-output-safe.

## Design

Three layers, all instrumenting at the **orchestrator** level (`cli`/`stages`)
so pipeline stages stay pure:

1. **Per-stage timings in the report.** Extend the report's `elapsed_ms` into a
   `timings` block: wall-clock per stage (`decode`, `film_base`, `algorithm`,
   `color`, `encode`, plus `ir_export` when run) alongside the existing total.
   Timings live in the *report only* — never the image or the recipe sidecar —
   so byte-identical determinism (§8) is untouched; the determinism E2E tests
   compare TIFF + sidecar, not the report.
2. **`tracing` spans for humans.** Add the `tracing` crate with spans around
   the same stage boundaries; `tracing-subscriber` renders them to **stderr**
   behind the existing `-v`/`-vv` verbosity (stdout stays pure JSON — the agent
   contract). This replaces ad-hoc `log.info` timing lines with structure and
   gives future work (e.g. `tracing-chrome` flame traces) a hook without new
   plumbing.
3. **Benchmarks for the hot kernels.** `criterion` benches for the per-pixel
   loops (`to_density` + anchored render, `simple` inversion, the auto-Dmax
   percentile, u16/f32 encode) on synthetic images sized to be meaningful
   (≥ a few MP), so a perf regression fails visibly. Wire `cargo bench` into
   the docs (not the CI gate — bench runners are too noisy for a hard gate;
   record baseline numbers in `progress.md` instead).

One-off analysis (not code in this task, but its verification uses them):
`cargo flamegraph` for CPU hotspots and `hyperfine` + `/usr/bin/time -l` for
end-to-end numbers — the same measurements `real-scan-verification`'s resource
row needs, so run them on the real assets when those exist and record findings.

## Implementation Suggestion

- Keep the `timings` report block a plain serialize-only struct of
  `Option<f64>` ms fields (same `skip_serializing_if` pattern as the rest of
  `Report`); populate from `Instant` pairs in `run_convert` — no need to thread
  clocks into pure stages.
- `stages::render` currently spans stages 2–4 in one call; either time inside
  it (returning a small timings struct alongside `Rendered`, like
  `ConvertReport`) or split the calls in the orchestrator — prefer whichever
  keeps `render` pure and total.
- `tracing` + `tracing-subscriber` with the `fmt` writer to stderr and
  `EnvFilter` default derived from `-v` count; no `RUST_LOG` surprises unless
  explicitly opted into (document precedence).
- Dependencies via `cargo add`; benches under `benches/` with deterministic
  synthetic inputs (fixed pattern, no RNG) so numbers are comparable across
  runs.

## How to Verify

- `convert --report json` on a fixture shows a `timings` block whose stage sum
  ≈ `elapsed_ms` (within scheduling noise); `inspect`/`estimate` reports stay
  unchanged except their existing totals.
- Determinism regression: byte-identical E2E tests still pass (timings never
  touch TIFF/sidecar).
- `-v` shows per-stage lines on stderr; stdout remains clean JSON
  (`verbose_keeps_stdout_clean` E2E still green).
- `cargo bench` runs the kernel benches; baseline numbers recorded in
  `progress.md`.
- Instrumentation overhead is negligible: default-run wall-clock within noise
  of the uninstrumented build on a fixture (spot-check with `hyperfine`).

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
