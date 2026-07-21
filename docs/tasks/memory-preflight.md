# Memory Preflight & In-Place Transform

## Goal

Make the pipeline's memory use **honest and bounded** without changing its
whole-image architecture. Two proportionate wins:

1. **Peak-memory preflight** — predict the run's peak allocation from the input
   dimensions/depth *before* allocating, compare against a budget, and fail (or
   warn) loudly when it would be exceeded — instead of advertising a 4 GiB input
   limit that silently implies a >20 GiB derived peak.
2. **Eliminate the color-transform clone** — transform the image in place rather
   than cloning the whole `LinearImage` (RGB *and* the never-transformed IR
   plane) in `pipeline::color::to_output`.

This is the cheap, high-value half of the memory-safety review (see
`docs/progress.md`). The expensive streaming/tiled half is a **separate,
evaluate-first task**: [streaming-tiled-io](streaming-tiled-io.md).

> **Context — the real gap.** `decode_limits()` advertises a **4 GiB input
> buffer**, but that guards only the `tiff` crate's u16 read buffer. Derived peak
> is a multiple of it, and **three full `LinearImage` buffers can overlap**: the
> orchestrator still owns the decoded `image` (used for `export_ir` after render,
> `cli.rs:1351`) while `render` produces the algorithm's `positive`
> (`stages.rs:107`) and then `to_output` clones it (`stages.rs:111` →
> `color.rs:132`). At the 4 GiB-u16 ceiling that's ~8 GiB × 3 ≈ **~24 GiB
> simultaneous peak, ~6× the 4 GiB input ceiling, unchecked** (the u16 read buffer
> drops before this, so it isn't additive). Making `to_output` in-place removes the
> clone but still leaves **two** overlapping images (decoded + positive ≈ 16 GiB),
> not one — the decoded `image` outlives the render. For real ~18 MP scans peak is
> ~600 MB (fine) — so this task is about the **ceiling being dishonest/unbounded**,
> not a problem current scans exhibit.

## Design

### 1. Peak-memory preflight

A single sizing model — the one source of truth for "how much will this run
allocate at peak" — reused by the preflight, the report, and (later)
`streaming-tiled-io`'s go/no-go. Compute from `width × height × channels ×
depth` and the known transient multipliers. The model **must account for the
concurrently-live full images**, not just one: the decoded `image` (kept for
`export_ir`), the algorithm output `positive`, and the `to_output` clone can
overlap — three images pre-fix, and **two even after** the in-place transform
below (decoded + positive), since the decoded image outlives the render. Also
fold in the u16+f32 decode coexistence and the u16 quantize buffer at encode.
Under-modelling this (e.g. counting only source + clone) would let the preflight
approve a conversion that still OOMs. Reconcile with `decode_limits()` so the
4 GiB read limit and the peak budget are not two unrelated numbers.

- **Where — must run before decode allocates.** The gate has to see the
  dimensions from a **metadata-only header/IFD probe** (like `decode`'s
  `dimensions()` / `colortype()` reads at `decode.rs:70-74`, which precede
  `read_image`) *before* any pixel buffer is allocated. Note the current flow
  can't just read `decode`'s return value: `decode` calls `read_image` and
  allocates the u16/f32 buffers **before** it returns dimensions — so for the
  oversized case this task targets, gating "after decode" would OOM before the
  check. Add a lightweight dimensions probe (shared with `nc inspect`) and gate on
  it up front.
- **Budget knob:** a memory budget is operational, not a conversion knob — it
  must not perturb deterministic image output (like `--strict`/`--report`, an arg
  only, **not** a recipe key). Provide an explicit `--max-memory <bytes>` override;
  for the default, decide between a **stable fixed budget** and a fraction of
  detected available RAM (see determinism below). Record the estimate and the
  decision in the JSON report.
- **Behavior:** over budget ⇒ fail loudly with the documented exit code and a
  message stating estimated peak vs budget (design-spec §11); optionally a
  warning tier below the hard limit. Never silently proceed into an OOM.
- **Determinism (scoped).** The *image output* stays fully deterministic (this
  gate never perturbs pixels), and the *estimate* is a pure function of
  dimensions/params. The *pass/fail decision*, however, is deliberately
  **environment-dependent** when the default budget tracks available RAM — the
  same input can pass on one machine and fail on another. That's acceptable for an
  operational limit (like an OOM), but state it explicitly and don't claim
  "same input + params ⇒ same decision." If a machine-independent decision is
  wanted, use a fixed default budget.

### 2. In-place color transform

`to_output` currently does `let mut out = image.clone();` then
`transform_in_place`. The orchestrator owns `rendered.image` and has no need of
the pre-transform copy, so:

- Change the signature to transform **in place** on a `&mut LinearImage` (or take
  the image by value and return it), returning only the ICC blob.
- Transform the **RGB channels only**; never touch/clone the IR plane.
- Preserve the existing loud guard that `rgb.len()` is a multiple of 3
  (`as_chunks_mut` tail check) and the lcms2 global-error-flag handling around
  the transform.

This removes one full RGB copy and one pointless full IR copy from peak (3 → 2
overlapping images). It does **not** get to a single image: the orchestrator's
decoded `image` and the algorithm's `positive` still coexist because the decoded
image is held for `export_ir` — the sizing model must keep counting both.

## Constraints (must hold)

- **Determinism unchanged.** Purely *how much* is allocated / *where* the
  transform writes — never *what* bytes result. Same input + recipe ⇒ identical
  output.
- **Fail loudly.** Preflight rejection and any allocation failure map to
  documented exit codes with a clear message; never a silent partial run.
- **Operational, not a recipe key.** The memory budget flag lives on the CLI arg
  struct only, like `--report`/telemetry — it must never be a recipe key or
  perturb output.
- **IR preserved.** The in-place change must keep the IR plane carried through
  intact (Step-1 rule).

## How to Verify

- Preflight estimate for known dimensions matches measured peak within a
  documented tolerance (cross-check against `real-scan-verification`'s measured
  peak and/or `perf-telemetry`).
- A synthetic oversized input (or a low `--max-memory`) is **rejected before**
  the large allocation, with the estimate-vs-budget message and the documented
  exit code; a within-budget run proceeds unchanged.
- The in-place transform produces **byte-identical** output to the current
  clone-based `to_output` (regression guard) and leaves the IR plane bit-identical.
- A memory-instrumented test (or measured RSS delta) shows the color stage no
  longer allocates a second full image.
- Report includes the estimated peak and the budget decision.

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md) — owns `run_convert` (where
  the preflight sits) and calls `to_output` (whose signature changes).
