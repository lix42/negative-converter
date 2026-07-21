---
name: perf-telemetry
description: >-
  How nc's embedded performance + context telemetry works — collecting, reading,
  and extending it. Use when adding a telemetry field or event to a new feature,
  turning on / collecting perf logs from `nc convert` (`--telemetry`,
  `--telemetry-file`, `NC_TELEMETRY_LOG`), reading or analyzing the telemetry JSONL
  log (jq over timing / algorithm / megapixels), bumping the record
  `schema_version`, or reasoning about the determinism and fail-soft invariants the
  telemetry code must preserve.
---

# nc perf telemetry

`nc convert` can emit one JSON **telemetry record** per **successful** run — image
facts, per-stage timings, a compact conversion summary, and the outcome — to a
local append-only JSONL log and/or a one-off file. It is **opt-in**,
**best-effort**, and never perturbs the converted image. A record's existence is
the success signal (there is no `outcome.success` field), so a run that exits
non-zero — including a `--strict` warning promotion — writes **no** record. Full design: design-spec §9 (record shape) and §12
(roadmap). Code: `src/telemetry.rs` (record + builder + sinks), wired from
`cli::run_convert` / `cli::emit_telemetry`; per-stage timings come from
`pipeline::stages::render` via `StageTimings`.

## 1. Adding telemetry when you build a feature

The record is built in one place: `telemetry::build_record(RecordInputs) ->
TelemetryRecord` (`src/telemetry.rs`). To add a field:

1. Add it to the right nested struct — `ImageInfo`, `TimingInfo`, `ConversionInfo`,
   or `OutcomeInfo` (or `TelemetryRecord` for run-context). They derive
   `Serialize`; the JSON key is the field name verbatim (no `rename`). Use
   `#[serde(skip_serializing_if = "Option::is_none")]` for a field that is absent in
   some runs (as `timing_ms.ir_export` and `conversion.dmax` do).
2. Feed it in: add a field to `RecordInputs<'a>` and set it in `build_record`; then
   populate it at the call site in `cli::emit_telemetry` (which gathers everything
   after the conversion has succeeded). If it's a new timing, measure it with an
   `Instant` pair like the existing stages — per-stage timings for the pure core
   live on `stages::StageTimings`, orchestrator-side ones (decode/encode/ir_export)
   are measured in `run_convert`.
3. **Bump `SCHEMA_VERSION`** (`src/telemetry.rs`) whenever the wire shape changes —
   a new/removed/renamed field, or a changed type. Note this also applies to the
   embedded domain enums (`Algorithm`, `FilmBaseSource`, `SilverFastFormat`): if
   *their* serialization changes, bump too. The server keys ingestion off this.
4. Prefer fixed-width wire types (`u32`/`u64`, not `usize`) and reuse domain enums
   rather than restringifying them.

### Two invariants every addition MUST preserve

- **Determinism — never touch the deterministic path.** The record must not enter
  the recipe sidecar or change the output image bytes. Telemetry on vs off ⇒
  byte-identical TIFF *and* sidecar (guarded by the `telemetry_does_not_perturb_
  output_or_sidecar` test). Telemetry is gathered/written *last*, after the image +
  sidecar are on disk, and only *reads* their finished facts. Never route a
  telemetry value back into a stage, the recipe, or the sidecar.
- **Fail-soft — telemetry must never fail a conversion.** A telemetry write/serialize
  failure warns on stderr and is swallowed (exit stays 0). It must NOT enter
  `report.warnings` (that would let `--strict` promote it), and it is surfaced even
  under `--quiet` (via `Log::warn_always`, used by the `warn` closure in
  `emit_telemetry`, mirroring the `non_finite` precedent). The one loud exception
  is a `--telemetry-file`/log path
  that *collides* with a real artifact (input/output/sidecar/report-file) — that's a
  config error caught up front (exit 2), so telemetry can't clobber real data.

Do NOT add these flags to `ResolvedConfig`/`*Params`/`merge`/`validate`: telemetry
is **operational**, not a conversion knob (the four-coupled-spots rule does not
apply).

## 2. Collecting perf logs

Both sinks are opt-in; telemetry is collected iff at least one flag is present, and
both may be combined.

```bash
# Append one record (one line) to the persistent JSONL log.
nc convert in.tiff -o out.tiff --film-base 0.9,0.55,0.42 --telemetry

# Also write this run's record to a one-off file (overwrites). `-` = stdout.
nc convert in.tiff -o out.tiff --film-base 0.9,0.55,0.42 --telemetry-file run.json

# Both sinks at once; send the one-off to stdout (pair with --report none so
# stdout carries only the telemetry line, since the report is on stdout by default).
nc convert in.tiff -o out.tiff --film-base 0.9,0.55,0.42 \
  --telemetry --telemetry-file - --report none
```

Default log path (first match wins): `$NC_TELEMETRY_LOG` → `$XDG_DATA_HOME/nc/telemetry.jsonl`
→ `%APPDATA%\nc\telemetry.jsonl` (Windows) → `$HOME/.local/share/nc/telemetry.jsonl`.
Point a whole batch at a scratch log with `NC_TELEMETRY_LOG=/tmp/nc.jsonl`. The log
is create-append with parent dirs created; **one compact JSON object per line**, so
N runs append N lines. `--telemetry-file <path>` overwrites (a single record).

## 3. Reading the logs

Each line is a standalone JSON object with this shape (see `src/telemetry.rs`):

```json
{ "schema_version":1, "timestamp_ms":1752566400000,
  "nc_version":"0.1.0", "target":"aarch64-apple-darwin", "cpu_count":14,
  "image":{"format":"hdri","width":502,"height":462,"megapixels":0.231924,
           "bit_depth":16,"channels":3,"ir_present":true,
           "input_bytes":2017230,"output_bytes":1392370},
  "timing_ms":{"total":30.0,"decode":5.0,"film_base":0.0,"algorithm":4.4,
               "color":18.4,"encode":1.0,"ir_export":0.6},
  "conversion":{"algorithm":"density","params_hash":"92a827ffd2d0aebd",
                "film_base_source":{"explicit":[0.9,0.55,0.42]},
                "dmax":1.6195,"output_hdr":false},
  "outcome":{"warnings":1,"clipped":3419,"non_finite":0} }
```

`timing_ms.ir_export` appears only when `--export-ir` ran; `conversion.dmax` only
when the density render applied an anchor. `params_hash` is a stable FNV-1a of the
effective recipe JSON (the sidecar bytes), so identical conversions share a hash.

`jq` recipes over the JSONL log (`jq -c` reads it line by line):

```bash
LOG="${NC_TELEMETRY_LOG:-${XDG_DATA_HOME:-$HOME/.local/share}/nc/telemetry.jsonl}"

# Per-stage timing for every run.
jq -c '{ts:.timestamp_ms, total:.timing_ms.total, decode:.timing_ms.decode, \
         algo:.timing_ms.algorithm, color:.timing_ms.color, encode:.timing_ms.encode}' "$LOG"

# Only density runs.
jq -c 'select(.conversion.algorithm == "density")' "$LOG"

# Megapixels vs total ms (TSV — feed a scatter / spot the slow ones).
jq -r '[.image.megapixels, .timing_ms.total] | @tsv' "$LOG"

# Throughput: megapixels per second per run.
jq -r '[.image.megapixels, (.image.megapixels / (.timing_ms.total/1000))] | @tsv' "$LOG"

# Runs that clipped or hit a non-finite sample.
jq -c 'select(.outcome.clipped > 0 or .outcome.non_finite > 0) \
       | {ts:.timestamp_ms, algo:.conversion.algorithm, clipped:.outcome.clipped}' "$LOG"

# Group timing stats by nc_version (across runs).
jq -s 'group_by(.nc_version)[] | {version: .[0].nc_version, runs: length, \
        avg_total_ms: (map(.timing_ms.total) | add/length)}' "$LOG"
```
