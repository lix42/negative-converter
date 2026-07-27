# Negative Converter — telemetry Progress Log

Execution log for the `telemetry` epic: what was done and how, key decisions, what
works, what doesn't. TASKS.md holds the authoritative status (the checkboxes);
this file is the narrative beside it.

One `##` section per task in this epic, named by the bare task name (the part
after the `/`). Read this whole file before starting a task in this epic, and
read other epics' `Epic summary` sections when you depend on them. Append
entries — don't rewrite earlier ones.

## Epic summary

What other epics need to know about `telemetry`:

- **Telemetry is operational, never a conversion knob.** `--telemetry`,
  `--telemetry-file`, and `NC_TELEMETRY_LOG` live on the CLI arg struct only —
  they are **not** recipe keys (a `telemetry` key in a recipe is rejected exit 2)
  and must never reach `ResolvedConfig`, the sidecar, or `merge`/`validate`. This
  is the documented exception to the "every knob is a flag *and* a recipe key"
  rule, alongside `--report`.
- **It must not perturb output.** The record is emitted last, after the TIFF and
  sidecar are written, and only reads their facts. Per-stage timings ride a
  report-only channel. An e2e test asserts byte-identical TIFF **and** sidecar
  with telemetry on vs off — keep that true.
- **Fail-soft, deliberately.** A telemetry *write* failure warns on stderr and
  never fails the run, and is kept out of `report.warnings` so `--strict` can't
  promote it. The one exception is a `--telemetry-file` path colliding with a real
  artifact, which is a config error caught up front (exit 2).
- **The local record is `SCHEMA_VERSION` 2**, serialize-only, with a pinned
  wire-shape snapshot test — any field or ordering drift fails a test, which is
  your signal to bump the version. It carries **no pixels and no file paths**;
  keep that invariant.
- **Records only exist for successful converts today.** There is no `success`
  field — its existence implies success. Typed failure events arrive with
  `telemetry/schema-v2`.
- **The JSONL log is the queue to drain.** Appends are a single `write_all` to an
  `O_APPEND` handle so concurrent runs can't interleave lines.
- **`docs/telemetry-strategy.md` is the authoritative contract** for everything
  downstream: Cloudflare Worker + D1 ingestion, a separately versioned
  privacy-minimized upload projection with an exact field allowlist, persistent
  consent separate from local collection, the lease/spool/drain model, and
  sanitized function/module-only panic frames. Read it before implementing any of
  the four remaining tasks — it went through six review passes on race and
  ownership edge cases.
- **Explicitly rejected by the user:** persistent install identity, and uploading
  `params_hash`. Backend spend is capped at $10/month.
- **`telemetry/perf-instrumentation` is parked, not pending** — the criterion
  lab-benchmark approach was superseded by real-world telemetry and survives only
  on branch `prototype/perf-bench-instrumentation`.


## perf-instrumentation
**Status:** parked (superseded by `perf-telemetry`)
**Updated:** 2026-07-15

- Original goal: per-stage timings in the JSON report, tracing spans to stderr
  behind `-v`, and criterion benches for the hot kernels — local-only,
  report-side (byte-identical output untouched). Pre-release performance
  visibility.
- **Parked, not merged.** On review (2026-07-15) we decided the LAB
  micro-benchmark framing answers the wrong question: we don't primarily want to
  bench kernels on a synthetic image in a controlled setting — we want to know how
  `nc` behaves **in the real world** on the user's actual scans, emit that as
  machine-readable metadata, and eventually ship it to a server. That is now
  `perf-telemetry` (below).
- The prototype is preserved on branch `prototype/perf-bench-instrumentation`
  (see its `docs/prototypes/perf-bench-instrumentation.md`). Reusable parts (the
  per-stage `Instant`-pair timing in `stages::render` + orchestrator) were lifted
  into `perf-telemetry`; the criterion benches, the lib/bin split, and the
  `tracing` spans were **not** brought over.


## perf-telemetry
**Status:** done
**Updated:** 2026-07-15

- Goal: embedded, opt-in performance + context telemetry for `nc convert` — after
  a real conversion, collect a **full** metadata record (image + per-stage timing
  + run context + outcome) and emit it as JSON to a persistent local JSONL log
  and/or a one-off file. No new subcommand/entrypoint. Groundwork for a future
  background uploader (`telemetry-upload`).
- **Why real-world, not lab:** decided with the user (see the parked
  `perf-instrumentation` note). Telemetry is embedded in the normal run, opt-in
  via a flag, and describes actual conversions — not a separate benchmark tool.
- **User-approved decisions honored:** sink = **BOTH** (a persistent append-only
  JSONL log AND an optional per-run file); record scope = **FULL**.
- **Flag surface (operational, NOT recipe keys):** `--telemetry` (append to the
  JSONL log) and `--telemetry-file <path>` (`-` = stdout; overwrites a one-off
  file); may be combined. Collected iff at least one is present. These are on
  `ConvertArgs` directly — **not** in `ResolvedConfig`/`*Params`/`merge`/`validate`
  (they're operational like `--report`, and must never touch the recipe/sidecar).
- **Default log path (dependency-free):** `NC_TELEMETRY_LOG` overrides, else
  `$XDG_DATA_HOME/nc/telemetry.jsonl`, else `$HOME/.local/share/nc/telemetry.jsonl`
  (Unix) / `%APPDATA%\nc\telemetry.jsonl` (Windows). Chose a hand-rolled XDG
  resolver over the `directories` crate to honor the house minimal-deps rule (the
  task explicitly called this acceptable). No new crate was added.
- **Record schema (`schema_version` 1, serialize-only):** `timestamp_ms`
  (epoch ms via `SystemTime`; unit in the name, no date crate), run context
  (`nc_version`, `target`, `cpu_count`), `image` (format/dims/megapixels/bit_depth/
  channels/ir_present/input_bytes/output_bytes), `timing_ms` (total + decode/
  film_base/algorithm/color/encode, and ir_export only when it ran), `conversion`
  (algorithm, `params_hash` = FNV-1a over the effective recipe JSON, film_base_source,
  dmax when applied, output_hdr), `outcome` (warnings/clipped/non_finite — no
  `success` flag; see the round-2 note below). See design-spec §9 for the shape;
  both `.md` and `.html` updated.
- **`target` triple:** added a dependency-free `build.rs` that re-exports Cargo's
  build-script `TARGET` as `NC_TARGET`, read at runtime via `env!("NC_TARGET")`.
- **Determinism boundary (verified):** telemetry is emitted last (after the output
  TIFF + sidecar are written) and only reads their facts. Per-stage timings ride a
  report-only channel (`stages::StageTimings` on `Rendered` + orchestrator
  `Instant` pairs), never serialized into the sidecar. The e2e
  `telemetry_does_not_perturb_output_or_sidecar` test asserts byte-identical TIFF
  **and** sidecar with telemetry on vs off.
- **Fail-soft (documented deviation from fail-loudly):** a telemetry *write*
  failure is warned on stderr and never fails the run; kept out of
  `report.warnings` so `--strict` can't promote it (the image already succeeded).
  A `--telemetry-file` path *collision* with a real artifact is the exception — a
  config error, caught up front by the existing collision guard (exit 2).
- **`-`/stdout caveat:** `--telemetry-file -` writes the compact line to stdout;
  since the report is on stdout by default, pair it with `--report none`/
  `--report-file` when a parser consumes stdout (documented on the flag + in §9).
- **Tests:** unit (record-builder fields, missing-IR omits `ir_export`, stable
  `params_hash`, JSONL append vs one-off overwrite) in `src/telemetry.rs`; e2e
  (full record on a fixture, ir_export timing, one-line-per-run, both sinks, the
  determinism invariant, fail-soft under `--strict`, collision usage error) in
  `tests/pipeline.rs`. Real-scan spot check ran the release binary on the
  committed real-scan fixture (full-size assets aren't in this environment):
  502×462, 0.2319 MP, per-stage ms populated, dmax 1.6195, clipped count matched
  the report warning.
- **Notes for `telemetry-upload`:** the JSONL log is the queue to drain (one
  object per line, crash-safe append). Upload must stay off the conversion
  critical path, honor an `NC_TELEMETRY=0`-style off switch (design-spec §12), and
  key ingestion off `schema_version`. Records already carry no pixels and no file
  *paths* — keep that invariant.

### Round-2 review fixes (2026-07-17, uncommitted — Codex + 5 pr-review agents)
- **[Codex P2] Atomic JSONL append.** `append_jsonl` now builds the record line +
  its `\n` into one buffer and emits it with a single `write_all` to an `O_APPEND`
  handle. On POSIX an append write below `PIPE_BUF` (4 KiB; a record is far
  smaller) is atomic, so two concurrent `--telemetry` runs sharing a log can't
  interleave a body with another's newline. `writeln!` (two writes) forfeited that.
  Added `append_jsonl_is_atomic_under_concurrency` (8 threads × 200 appends, every
  line must parse; count exact).
- **[tests] `outcome` wiring pinned end-to-end.** New e2e tests:
  `telemetry_outcome_reports_clipping_and_warnings` (+12-stop exposure ⇒
  `outcome.clipped > 0` and `warnings >= 1`) and
  `telemetry_outcome_counts_ir_ignored_warning` (HDRi w/o `--export-ir`, f32 out ⇒
  `clipped == 0`, `warnings >= 1`) — proving `report.warnings.len()` /
  `clipped_total()` actually flow into the record, not just type-check.
- **[tests] Flags are operational, not recipe keys.** New
  `telemetry_key_in_recipe_is_rejected`: a `--params` recipe with a `telemetry` key
  is rejected exit 2 by `deny_unknown_fields`, no artifact written.
- **[type-design] `build_record` is now fully pure.** `timestamp_ms` and
  `cpu_count` are injected via `RecordInputs`; the orchestrator does the ambient
  reads (`telemetry::now_unix_millis` / `telemetry::cpu_count`), mirroring
  `default_log_path`→`resolve_log_path`. Only compile-time constants
  (`CARGO_PKG_VERSION`, `NC_TARGET`) remain in the builder.
- **[type-design] Dropped `OutcomeInfo.success`.** It was a hardcoded `true` that
  carried no information and could contradict `non_finite > 0`. A `success`/`status`
  field returns with the failure-path record in `telemetry-strategy`/
  `telemetry-upload`, where it actually varies. **`schema_version` stays 1** — the
  feature is unreleased, so no record with the old shape exists in the wild and
  there's no ingestion compat to preserve. SKILL + design-spec (md+html) + record
  example + task schema all updated to match.
- **[silent-failure] `Log::warn_always`.** Added a helper that prints
  `nc: warning:` regardless of `--quiet`; `emit_telemetry`'s `warn` closure now
  delegates to it, removing the duplicated format string and the fragile coupling
  to `Log::warn`'s internal quiet-gating.
- **[comments/docs] `--dump-params` is a file writer, not stdout.** Corrected the
  stdout-writer list (accurate set: `emit_report` + `nc params`) in design-spec
  (md+html), `TASKS.md`, and `stdout-broken-pipe-safety.md`.
- **[docs LOW] Log-path precedence + Option contract.** Fixed the task file's
  default-log-path order (APPDATA before the HOME fallback, matching
  `resolve_log_path`); fixed the SKILL's jq fallback to honor `XDG_DATA_HOME`
  first; documented the omitted-vs-null Option contract on `TelemetryRecord`.
- **DEFER (not done here):** the default stdout **report** (`emit_report`'s
  `println!`) panicking on a broken pipe is the pre-existing
  `stdout-broken-pipe-safety` task, out of scope for perf-telemetry.

### Rebase onto origin/main + algo-sigmoid interaction (2026-07-17, uncommitted)
- **Rebased** the branch onto `origin/main` (now carrying `algo-sigmoid` #27 and
  `auto-base-redesign` #23). Conflicts in `src/pipeline/stages.rs` and `src/cli.rs`.
- **Reconciliation:** `auto-base-redesign` moved film-base estimation OUT of
  `stages::render` (the orchestrator now resolves the base and passes `&FilmBase`,
  so estimate warnings surface before the fallible render). So the film-base
  **timing** moved with it: `StageTimings` now carries only `algorithm_ms` +
  `color_ms` (the two stages `render` still runs); the orchestrator measures
  `film_base_ms` around its own `film_base::estimate` call (like `decode_ms` /
  `encode_ms`) and folds it into the telemetry `TimingInfo`. Kept `algo-sigmoid`'s
  `--algorithm sigmoid` warning (density-gamma no-op) alongside the telemetry
  decode-timing line in `run_convert`.
- **Sigmoid telemetry check (verified):** a `--algorithm sigmoid` conversion
  produces a sane record — `conversion.algorithm` = "sigmoid", a resolved `dmax`
  (sigmoid shares the density anchor), all per-stage timings populated, and
  `params_hash` covers the `sigmoid.*` recipe keys (changing `--sigmoid-contrast`
  changes the hash, since the hash is over the effective recipe JSON). Added the
  e2e test `telemetry_records_sigmoid_algorithm_and_params_hash`.

### Round-3 review fixes (2026-07-17, uncommitted — Codex + 5 pr-review lenses)
- **[Codex P2] Case-only telemetry-file/output collision.** `-o out.tiff
  --telemetry-file OUT.TIFF` on a case-insensitive FS (macOS/Windows default) was
  NOT rejected: with neither file pre-existing, `collision_key` can't canonicalize
  to a shared casing and `ensure_write_targets_distinct` compared the keys with a
  case-sensitive `==`, so the guard passed and the telemetry write clobbered the
  just-written TIFF (exit 0). Fix: new `keys_collide(a, b)` helper comparing exactly
  OR ignoring ASCII case (`to_string_lossy().eq_ignore_ascii_case`), used for both
  the input-key and seen-set checks. Conservative over-reject (can't cheaply detect
  per-volume case sensitivity; false-rejecting a case-only pair in one invocation is
  a harmless annoyance vs. silently clobbering the output). Doc comments on
  `collision_key`/`ensure_write_targets_distinct` updated. Unit tests:
  `keys_collide_is_case_insensitivity_aware` and
  `write_targets_reject_case_only_collision_before_creation`.
- **[tests] Strengthened `append_jsonl_is_atomic_under_concurrency`:** payloads now
  padded past a 4 KiB page (was ~30 bytes) so an interleaved pre-fix two-write would
  actually corrupt a line, per-thread pad char distinct so a splice shows up as a
  JSON parse failure.
- **[type-design] Wire-shape snapshot test `record_wire_shape_is_pinned`:** pins the
  exact serialized JSON for a fully-populated and a minimal record (fixed
  `nc_version`/`target` literals), so any field/order/foreign-enum drift that should
  bump `SCHEMA_VERSION` fails a test.
- **[comment] `append_jsonl` atomicity rationale corrected:** the guarantee is
  `O_APPEND` offset-then-write atomicity on a local FS, not the `PIPE_BUF` bound
  (which governs *pipe* writes) — reworded, with the distinction called out.
- **[docs] Stale/inaccurate references fixed:** `telemetry-strategy.md` no longer
  cites the dropped `outcome.success` field (a record's existence implies success);
  design-spec §12 item 17 emit_report consumer list now reads "convert/inspect/
  estimate" (md+html); design-spec §9 collision parenthetical now reads
  "(`NC_TELEMETRY_LOG` or the default path)" (md+html).
- 2026-07-27: Epic-migration redirect — the stdout-broken-pipe-safety task path
  cited above is now
  [core/stdout-broken-pipe-safety](../tasks/core/stdout-broken-pipe-safety.md).
  The entry above is preserved verbatim.


## strategy
**Status:** done
**Updated:** 2026-07-23

- 2026-07-23: Started the strategy spike by inventorying the shipped schema,
  queue behavior, determinism/fail-soft invariants, roadmap constraints, and the
  existing `telemetry-upload` child. Research and user interview are in progress;
  no infrastructure, schema, consent, or child-task decision has been made yet.
- 2026-07-23: Initial research found that OTLP's logs transport is stable and
  vendor-neutral, but durable delivery still requires a Collector with persistent
  storage or nc-owned queue/ack/retry logic; adopting OTLP does not replace the
  uploader design. Flagged an expanded privacy threat model for the design:
  stable `params_hash` plus precise timestamps, dimensions, and file sizes are
  path-free but can still correlate or fingerprint workflows. A Rust panic hook
  can observe panics even with abort behavior, but it is not general native crash
  capture, and raw panic payloads/backtraces can disclose user paths or data.
- 2026-07-23: User interview prioritized real-world performance and failure
  rates, capped initial backend spend at $10/month, rejected persistent install
  identity and remote `params_hash`, selected sanitized function/module-only panic
  frames, and accepted a detached `nc telemetry upload-once` helper. Upload
  consent must be persistent and separate from local collection; enabling it may
  transmit the previously accumulated explicitly opted-in queue.
- 2026-07-23: Backend pricing research favors a schema-validating Cloudflare
  Worker plus D1 for the initial owned ingestion service: expected volume fits
  the free tier and the paid Workers floor is $5/month. D1 preserves exact rows,
  unique-event deduplication, and ordinary SQL; Analytics Engine was rejected for
  v1 because it can sample at high volume and fixes retention at three months,
  both undesirable for exact failure-rate analysis.
- 2026-07-23: User approved the final strategy and automatic future success/
  failure collection under persistent upload consent. Wrote
  `docs/telemetry-strategy.md` with the infrastructure/protocol, separate local-v2
  and upload-v1 schemas, exact upload field allowlist, forbidden-data rules,
  consent UX, locked rotate/spool/ack drain model, panic boundary, retention,
  query goals, and verification gates. Split implementation into
  `telemetry-schema-v2`, `telemetry-ingestion-service`, the refined
  `telemetry-upload`, and `telemetry-panic-hook`; no standalone usage-event task
  because feature adoption is not a selected goal.
- 2026-07-23: Review-fix pass hardened the approved docs against twelve verified
  edge cases without changing status or dependencies: explicit durable
  raw/batch/temp recovery and fsync ordering; per-panic atomic ready files;
  request-by-request fail-closed consent and revoke races; one consent-stored
  managed queue path; a shared machine-readable cross-language contract with
  complete bounds/enums; convert-only v1 scope; panic fixture ownership; a hard
  FREE-plan quota/cost boundary; anonymous-provenance caveats and abuse
  quarantine/kill switch; and realistic no-network-critical-path/bounded-spawn
  timing requirements.
- 2026-07-23: Second review-fix pass removed remaining ownership and race
  ambiguity: consent now stores a permanent selected active JSONL plus a uniquely
  derived private sibling spool; a shared/exclusive request lease makes disable
  wait for bounded in-flight HTTPS work; bit depth is correctly 16 bits/sample;
  the shared schema owns the panic-ready fixture and relational validation; and
  pure legacy projection is separated from uploader-owned durable ID assignment.
- 2026-07-23: Third review-fix pass separated invocation-lifetime collection
  consent from request-lifetime network consent. Supported converts now hold an
  immutable generation/path collection snapshot through append or panic; disable
  stops new work and waits only for bounded requests, while inactive-only purge
  waits collection-exclusive before clearing selected state. Helper A→B retarget
  checks and a cycle-free collection/drain/gate/request/queue lock order are
  explicit; per-run telemetry no longer implies panic capture.
- 2026-07-23: Fourth review-fix pass made queue retarget non-stranding and purge
  lock-stable. Active A→B is rejected; inactive retarget waits old snapshots and
  helper work and publishes B only after proving A has no managed event, batch,
  panic, temp, or quarantine data. Purge now clears data while preserving the
  active path, spool skeleton, and original queue/drain lock inodes, with
  Unix/Windows final-transition races required.
- 2026-07-23: Fifth review-fix pass made active same-path enable a true
  idempotent no-op and specified an inactive same-path helper handoff: wait drain
  before gate/request locks, publish a fresh generation only after the old helper
  exits, then launch exactly one replacement after releasing locks. Panic-hook
  sequencing now depends on `telemetry-upload`, which owns the consent,
  collection-lease, spool, reconciliation, and purge runtime it requires;
  schema-v2 remains transitive.
- 2026-07-23: Sixth review-fix pass closed the inactive same-path handoff race by
  acquiring collection-exclusive before drain. A replacement generation/helper
  now waits both an old conversion's post-disable final append/panic lifetime and
  the old helper, then publishes only under gate/request exclusion after both
  finish.


## schema-v2
**Status:** not started
**Updated:** 2026-07-23

- Goal: add typed local success/failure events and a separately versioned,
  allowlisted upload projection with random per-event deduplication IDs.


## ingestion-service
**Status:** not started
**Updated:** 2026-07-23

- Goal: implement the validating Cloudflare Worker + D1 ingestion, exact
  deduplication, retention, and initial performance/failure analysis queries.


## upload
**Status:** not started
**Updated:** 2026-07-23

- Goal: implement generation-bound invocation collection and request leases for
  a selected active JSONL/private spool, durable rotation/recovery, detached
  draining, non-stranding retarget, lock-stable inactive purge, and
  retry/quarantine maintenance.


## panic-hook
**Status:** not started
**Updated:** 2026-07-23

- Goal: capture consented Rust panics through an isolated spool with only
  sanitized `nc` function/module frames and unchanged normal panic behavior.
