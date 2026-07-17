# Embedded performance + context telemetry

## Goal

Embedded, opt-in performance + context telemetry for `nc convert`: after a real
conversion, collect a **full** metadata record about the image and the run and
emit it as JSON to a persistent local JSONL log and/or a one-off file — **without
a new entrypoint/subcommand** and **without perturbing** the deterministic image
and sidecar output. This lays the groundwork for a future background uploader
(tracked separately as `telemetry-upload`); this task produces the record and the
local queue an uploader will later drain.

This is the *real-world* successor to the original `perf-instrumentation` task,
which built LAB criterion micro-benchmarks — the wrong question. That work is
preserved (unmerged) on `prototype/perf-bench-instrumentation`; the per-stage
timing measurement is lifted from it, the criterion/lib-split/tracing parts are
not.

## Design

### Flag surface (operational, NOT recipe keys)

Telemetry is an operational concern like `--report`/`--verbose`, so its flags are
**not** conversion knobs: they are **not** added to `ResolvedConfig` / any
`*Params` / `merge` / `validate`, and never appear in the recipe sidecar.

- `--telemetry` (bool): append one JSON record (one line) to the default local
  JSONL log.
- `--telemetry-file <path>`: also write the record to this path (`-` = stdout).
  May be given with or without `--telemetry`.
- Telemetry is **collected iff at least one** of these is present (opt-in). Both
  may be combined ("Both" — the record then lands in both sinks).
- **Default log path** (dependency-free, per minimal-deps preference; no
  `directories` crate), highest priority first: `NC_TELEMETRY_LOG` overrides
  everything, else `$XDG_DATA_HOME/nc/telemetry.jsonl` when set, else
  `%APPDATA%\nc\telemetry.jsonl` on Windows, else
  `$HOME/.local/share/nc/telemetry.jsonl` (the XDG default base — the universal
  last-resort fallback on any platform with `HOME`). Matches `resolve_log_path`.

### Record schema (Full), serialize-only JSON

- `schema_version` (integer, `1`) — for server ingestion / forward-compat.
- `timestamp_ms` — UNIX epoch milliseconds (`std::time::SystemTime`; the name
  carries the unit so the server never guesses; no date crate).
- run context: `nc_version` (`CARGO_PKG_VERSION`), `target` (compile triple, via a
  dependency-free `build.rs` → `NC_TARGET`), `cpu_count`
  (`std::thread::available_parallelism`, `None` on `Err`).
- `image`: `format` (hdr/hdri), `width`, `height`, `megapixels`, `bit_depth`,
  `channels`, `ir_present`, `input_bytes`, `output_bytes` (sizes best-effort
  `Option`).
- `timing_ms`: `total` + per-stage `decode`, `film_base`, `algorithm`, `color`,
  `encode`, and `ir_export` (only when it ran; omitted otherwise).
- `conversion`: `algorithm`, a stable `params_hash` (FNV-1a over the effective
  recipe JSON — the same bytes as the sidecar, so identical conversions share a
  hash), `film_base_source`, `dmax` (resolved anchor, when applied), `output_hdr`.
- `outcome`: `warnings` (count), `clipped` / `non_finite` sample counts (from
  `EncodeReport`). No `success` flag today — a record is emitted only after a
  conversion succeeds, so a constant `true` would carry no information (and could
  contradict `non_finite > 0`); a `success`/`status` field returns with the
  failure-path record in `telemetry-strategy`/`telemetry-upload`.

### Determinism boundary (critical)

The record (timing, timestamp, system info) MUST NOT enter the recipe sidecar or
change the image bytes. Telemetry on or off, the output TIFF and sidecar JSON are
**byte-identical**. The existing stdout `--report` stays as-is; telemetry is its
own separate sink. Mechanically: per-stage timings ride a report-only channel
(`stages::StageTimings` on `Rendered`, plus orchestrator `Instant` pairs) that is
never serialized into the sidecar; telemetry is emitted last, after the output +
sidecar are written, and only *reads* their facts.

### Best-effort / fail-soft (a documented deviation from fail-loudly)

A telemetry *write* failure must NOT fail a successful conversion — the image
already succeeded, and telemetry is non-critical observability. So the
orchestrator **warns on stderr and continues** (exit stays 0), and `--strict`
does **not** promote it (the telemetry failure is kept out of `report.warnings`).
This is the one place the house fail-loudly rule is deliberately relaxed.

The exception is a `--telemetry-file` path — **or** a `--telemetry` log path
(`NC_TELEMETRY_LOG` / the default) — that would **collide** with a real write
target (input / output / sidecar / report-file): that is a *config* error, caught
up front by the same collision guard as `--report-file`, and stays a loud usage
error (exit 2) — clobbering the just-written output, or silently appending a JSONL
line into the irreplaceable input scan, is not something to swallow. (`-`/stdout
is not a filesystem target and is excluded from the check.)

### Out of scope (follow-up)

The actual background/server upload. This task produces the record + the local
JSONL queue an uploader will later drain. Tracked as `telemetry-upload` (depends
on this task); see design-spec §12.

## Implementation Suggestion

- A pure record-builder (`telemetry::build_record(RecordInputs) ->
  TelemetryRecord`) that serde-serializes; the orchestrator (`cli::run_convert`)
  gathers the inputs and writes the sink(s). Stages stay pure — only the
  orchestrator does telemetry I/O, after the conversion succeeds.
- Reuse the prototype's per-stage timing idea: return a small `StageTimings` from
  `stages::render` (a report-only channel, like the existing `ConvertReport`) and
  measure decode/encode/ir_export with `Instant` pairs in the orchestrator — but
  keep all of it OUT of the sidecar.
- Append to the JSONL log with create-append semantics; create parent dirs; one
  compact JSON object per line. The one-off `--telemetry-file` overwrites (a
  single record); `-` prints the compact line to stdout (note: pair it with
  `--report none`/`--report-file` if a parser consumes stdout, since the report
  is on stdout by default).

## How to Verify

- **Unit:** record builder yields the expected fields from a known image +
  timings; `megapixels`/`channels` derived correctly; missing IR ⇒
  `ir_present=false`, no `ir_export` timing (and it's omitted from the JSON);
  `params_hash` stable + input-sensitive; JSONL append vs one-off overwrite.
- **E2E:** `--telemetry-file <path>` on a committed fixture ⇒ valid JSON with all
  schema fields, `schema_version=1`, finite timings, correct dims/bytes;
  `--telemetry` (with `NC_TELEMETRY_LOG` at a temp file) appends exactly one line
  per run (two runs ⇒ two lines); both flags ⇒ record in both sinks.
- **INVARIANT:** convert with telemetry ON vs OFF ⇒ byte-identical output TIFF
  AND byte-identical sidecar.
- **Fail-soft:** an unwritable telemetry path ⇒ stderr warning, conversion still
  exits 0; `--strict` does not change that. A colliding `--telemetry-file` ⇒ loud
  usage error (exit 2), no artifact written.
- **Real-scan spot check** (per CLAUDE.md; never read the scans into context):
  run the CLI on a real scan and report only derived numbers (dims, MP, per-stage
  ms, bytes).

## Dependencies

- [Pipeline orchestration](pipeline-orchestration.md)
