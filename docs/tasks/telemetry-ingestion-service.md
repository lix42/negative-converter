# Telemetry ingestion service

## Goal

Provide the anonymous public HTTPS endpoint that validates, deduplicates, stores,
expires, and queries nc's privacy-filtered upload events within the approved
$10/month initial budget.

The service contract is fixed by
[`docs/telemetry-strategy.md`](../telemetry-strategy.md).

## Design

Deploy an nc-owned Cloudflare Worker with D1:

- `POST /v1/events` accepts upload-schema-v1 JSON batches of at most 100 events
  and 256 KiB.
- Validate the canonical envelope and the checked-in shared JSON Schema/corpus:
  exact keys, ID/frame/version encodings, numeric/string bounds, enum values,
  supported schema versions, and the forbidden-field policy. Unknown keys are
  errors.
- Insert each `event_id` under a D1 primary/unique key. A retry of a committed
  event is reported as `duplicate`, not inserted again.
- Return per-event `accepted`, `duplicate`, and permanently `rejected` IDs only
  after the transaction commits. Use 429/5xx for retryable service failures and
  400 for a malformed top-level request.
- Store the sanitized payload plus indexed cohort columns needed for
  version/platform/stage/algorithm/image-size/timing queries. Store client event
  and receipt time only at day granularity.
- Expire remote events after 180 days with a scheduled deletion.
- Keep the endpoint anonymous: there is no embedded client secret and no
  install/user/session identity. Source IP may be used transiently by Cloudflare
  abuse controls but is never copied into an event or D1. Disable Worker
  request/body observability logs and document Cloudflare as the processor.

Use Cloudflare edge/request limits, a coarse transient rate limit, strict body
caps, and schema validation as abuse controls. These do not authenticate a
public client or prove genuine nc provenance. Enforce a release/version
allowlist, quarantine anomalous or suspicious-volume cohorts outside analytical
tables, and provide an operational ingestion kill switch. Do not embed a client
secret. Telemetry is fail-soft, so service unavailability, kill switch, or a
free-tier limit never affects an nc command.

Deploy v1 only in a dedicated Cloudflare FREE-plan account/project with no
payment method or billing-enabled resource. Check in a cost model covering
bytes/event, requests and rows/day, D1 writes including indexes and retention
deletes, reads/query, and rolling 180-day storage at low/expected/worst accepted
volume. Enforce application daily event/byte/write/storage ceilings below
platform free limits. Quota exhaustion rejects fail closed with 429/503; clients
retain then locally expire data. Deployment must assert the expected account,
plan, bindings, quotas, logging state, kill switch, and absence of paid
resources. Any paid migration or possible spend above $10/month requires
explicit user approval first.

Provide checked-in deployment/configuration, D1 migrations, a local Worker/D1
test path, retention job, and documented SQL for the initial questions:
failure rate by release/error/stage and timing distributions by release,
algorithm, platform, CPU/image/input-size/output cohorts.

## How to Verify

- Worker tests accept every canonical upload fixture from
  `telemetry-schema-v2`'s shared schema/corpus, reject unsupported
  versions/unknown keys/wrong types/enums/encodings/ranges/forbidden
  fields/oversized batches, and return stable rejection codes.
- Replaying an `event_id` returns `duplicate` and leaves one D1 row.
- A simulated commit-before-response loss is safe to retry.
- The 180-day retention job deletes expired rows and preserves newer rows.
- No D1 migration, Worker log, or query stores IP/header/body data outside the
  approved event allowlist.
- Seeded success/failure/performance fixtures produce correct failure-rate and
  stage-timing query results.
- Release allowlist, kill switch, and suspicious-cohort tests prove disallowed
  or anomalous input never reaches analytical tables.
- Limit/load tests hit request, event, byte, D1-write, storage, and
  retention-delete ceilings and prove fail-closed rejection without paid
  overflow.
- Deployment documentation includes the hard free-plan boundary, cost model,
  explicit approval gate for paid migration, schema migration/rollback, and
  endpoint configuration for the client.

## Dependencies

- [Telemetry event schema v2](telemetry-schema-v2.md) — supplies the canonical
  upload-v1 fixtures, bounds, enums, and privacy allowlist.
