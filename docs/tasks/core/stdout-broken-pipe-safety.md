# Stdout broken-pipe safety

## Goal

Make every stdout write in `nc` tolerate a **closed pipe** without a panic or
backtrace, exiting cleanly instead. Today the JSON report and the other stdout
emitters use `println!`, which panics (`failed printing to stdout: Broken pipe`)
when the reader goes away — the classic `nc … | head` / `nc … | jq 'first'` case,
where the downstream consumer exits after reading enough. A panic there prints an
ugly backtrace to stderr and returns a failure code even though the conversion
itself fully succeeded.

> **Pre-existing on `main`, not introduced by the telemetry work.** `emit_report`
> (convert/inspect/estimate reports) and `nc params` already used `println!` before
> `perf-telemetry`. (`--dump-params` is *not* a stdout writer — it serializes to a
> file via `fs::write`, so it's out of scope here.) The telemetry `--telemetry-file -`
> path *already* fixed this
> locally with a fail-soft `writeln!(std::io::stdout(), …)` whose error is warned
> and swallowed — so the fix pattern exists in-tree and can be reused. Note that
> because `emit_report` runs *before* telemetry, on an already-closed stdout it is
> the report write that panics first, which is why the telemetry-local fix is not
> sufficient on its own.

## Design

- **Inventory the stdout writers.** All go through `cli.rs`: `emit_report` (the
  `println!("{json}")` branch, shared by convert/inspect/estimate) and `run_params`
  (`nc params`). (`--dump-params` writes to a *file*, not stdout, so it's not in
  scope.) Route every one through a single helper that writes to
  `std::io::stdout()` and maps a `BrokenPipe` error to a clean, silent exit
  (`ErrorKind::BrokenPipe` ⇒ no backtrace, no stderr spew), while other I/O errors
  still surface as the documented write error.
- **Clean exit, not a swallow.** A broken pipe on stdout is not a conversion
  failure (the image/sidecar were already written to disk); it means the consumer
  stopped reading. The process should terminate promptly and quietly. Options to
  evaluate: catching `BrokenPipe` and returning success/`ExitCode`, or resetting
  the `SIGPIPE` disposition to `SIG_DFL` at startup (Rust ignores `SIGPIPE` by
  default, which is what turns the write into an `Err` that `println!` unwraps).
  Pick one and document the choice; the SIG_DFL approach is the smaller, more
  uniform fix but changes signal behavior process-wide, so weigh it against an
  explicit per-write handler.
- **Keep the stdout-is-clean-JSON contract.** Whatever the mechanism, stdout still
  carries only the report/params JSON, and stderr stays for logs/warnings.

## Implementation Suggestion

- Reuse the telemetry `-`/stdout pattern (`writeln!(std::io::stdout(), …)` +
  matching on the `io::Result`) as the shared helper, so report and params writes
  become fail-soft the same way the telemetry stdout sink already is.
- If going the `SIG_DFL` route, set it once in `cli::run` (next to the lcms2
  handler install) and document why (restores the conventional CLI pipe behavior).
- Map `ErrorKind::BrokenPipe` to a distinct clean path; leave non-pipe write
  errors mapping to the existing `NcError::Write` (exit 5).

## How to Verify

- E2E: `nc convert … --report json | head -c 1` (or pipe into a reader that closes
  early) exits without a panic/backtrace and with a clean status; the output TIFF +
  sidecar are still written. Same for `nc params | head -1` and
  `nc inspect … | head -1`.
- Regression: normal (unpiped, or fully-consumed) runs still print the complete
  JSON to stdout unchanged.
- No stray backtrace text on stderr for the broken-pipe case.

## Dependencies

- [CLI framework](cli-framework.md) — owns the stdout emitters (`emit_report`,
  `run_params`, dispatch) this hardens.
