# Background telemetry upload

## Goal

Ship privacy-projected local telemetry to the nc collection service under
explicit persistent consent, without adding network latency, output changes, or
new failure modes to foreground commands.

The authoritative protocol, consent, queue, privacy, and retention decisions are
in [`docs/telemetry-strategy.md`](../telemetry-strategy.md).

## Design

Add a `nc telemetry` maintenance surface:

- `enable` displays the field manifest, existing queue count/date range,
  retention, backend, and the resolved active JSONL/spool paths; it prompts on a
  TTY and requires `--yes` when non-interactive. `enable [--queue PATH]` selects
  exactly one permanent active JSONL: explicit `PATH`, otherwise the current
  `NC_TELEMETRY_LOG`/default resolution. Consent covers rotating/draining that
  file and future automatic `convert` success/failure/panic collection. Its spool
  is the same-parent `.<selected-basename>.nc-telemetry-spool`. Active same-path
  enable is an idempotent no-op with unchanged generation and no new helper;
  active A→B selection is rejected.
- `disable` revokes automatic collection and permission to start new requests
  but preserves data and the last generation/paths. It does not wait for
  already-started conversions, which may append one final local event; it does
  wait for bounded network work, so after return no request is active or may
  start.
- `purge` is inactive-only, waits for already-started consent snapshots, and
  durably removes recognized selected telemetry.
- `status` reports consent, queue/quarantine size, last successful upload, and
  last fail-soft error.
- `preview` prints the exact upload projection without sending it.
- `flush` performs a foreground drain and reports errors, but only while active
  valid consent permits every request.
- Hidden/internal `upload-once` performs one bounded background drain.

Store a fresh random immutable 128-bit generation plus the normalized absolute
selected active file and derived spool directory in versioned consent in the
platform config directory. Re-derive the sibling spool path during validation
and reject a mismatch. Inactive consent retains its last generation/paths for
precise purge targeting. A broader upload manifest
invalidates old consent; sending strictly less data does not. Later
collection/draining uses the stored paths independently of environment changes,
performs no filesystem discovery, and never touches custom telemetry paths that
were not selected.
`NC_TELEMETRY=0` disables automatic collection, helper launch, and network for
the process, while explicit local-only `--telemetry-file` remains available.

Publish consent using a same-directory create-new regular temp without following
symlinks, file flush + `fsync`, atomic rename, then parent-directory `fsync`.
Missing, torn, unreadable, unknown-version, wrong-owner/unsafe-permission,
non-regular, or symlinked consent/active/spool paths fail closed. Do not cache
permissive consent: automatic collection and helper launch re-read it.

Use a cross-process request lease beside the consent manifest, with acquisition
serialized by a short cross-process consent gate. A helper or `flush` holds the
gate while taking a shared lease and validating active consent plus
`NC_TELEMETRY=0` and equality with its captured generation/active/spool paths; it
then releases the gate and holds the lease through the response or fixed
10-second overall HTTPS timeout. `disable` holds the gate while taking the
exclusive lease, preventing new shared holders from overtaking it;
after any request finishes/times out it durably publishes inactive consent, then
returns. Concurrent enable/disable also uses gate + exclusive lease. This closes
the exact check→disable→send race.

Automatic collection uses a separate global collection lease. At invocation
start, supported `convert` acquires the shared collection lease, validates active
consent generation and exact active/spool paths under the gate, then holds its
immutable snapshot and shared lease through success/failure append or
panic/process end. Disable does not wait for this lease: an already-started
invocation may append one final local event after disable, but its pre-launch
gate check rejects inactive/retargeted consent, so it launches no helper. Explicit
per-run `--telemetry`/`--telemetry-file` is outside this snapshot and installs no
panic hook. If explicit `--telemetry` resolves to the retained selected active
path, its final append briefly uses shared collection lease → consent-gate path
check → that spool's queue lock for retarget/purge safety, without gaining a
persistent snapshot.

After output artifacts, reports, telemetry timing, and exit outcome are fixed,
spawn the current executable as a short-lived detached `upload-once` helper with
all standard streams disconnected. No network or endpoint latency is on the
critical path, but process creation has nonzero bounded overhead. Helper launch
and every background error are fail-soft. `enable` also launches a helper
immediately; no resident daemon or OS scheduler is required.

Use the selected JSONL as the permanent active append path. Derive its private
same-parent `.<basename>.nc-telemetry-spool`; all raw/batch/panic/temp/quarantine/
status files and `queue.lock`/`drain.lock` live only there. Different selected
basenames in one parent therefore cannot collide. Rotation orders active file
flush/`fsync`, atomic same-filesystem rename into the spool, common-parent and
spool `fsync`, safe recreation at the selected path + file `fsync`, and parent
`fsync`.
Projection writes/syncs a complete batch temp, renames and parent-syncs it before
deleting and parent-syncing the raw input. Persist all legacy event IDs in the
immutable batch before networking.

At every startup reconcile every recognized raw/batch/panic/temp state before
rotation or networking. Safely complete only a provably complete temp;
otherwise quarantine/remove it with a local counter. Validate all immutable
states, recreate a missing active file durably, ignore unexpected names, and
fail closed on symlinks/non-regular managed states.

Lock ordering is:

- collection: shared collection lease → brief consent-gate validation, release
  gate, then queue lock at append;
- helper: drain lease → queue lock for rotation, release queue, then consent
  gate → shared request lease per network request;
- disable: consent gate → exclusive request lease;
- purge: exclusive collection lease → drain lease → consent gate validation →
  queue lock;
- inactive same-path re-enable: exclusive collection lease → drain lease →
  consent gate → exclusive request lease.

No path reverses these orders.

Inactive same-path enable performs a helper handoff in this order: acquire the
exclusive collection lease and wait for every old-generation invocation's final
append or panic/process end; acquire the spool's drain lease and wait for its old
holder; then acquire consent gate → exclusive request lease, revalidate the
retained inactive generation/paths, and durably publish a fresh same-path
generation. Release every lock before launching exactly one replacement helper
for the immediate drain. Because it never holds the gate while waiting for
collection/drain, old invocations and a helper can finish append/gate/request work
and exit without a cycle.

Changing inactive selection A→B takes exclusive collection lease → A drain
lease → consent gate → A queue lock. Reconcile and require A's active file empty
and its spool free of raw events, batches, panic-ready files, data-bearing temps,
quarantine, or other managed telemetry records. A preexisting batch or a late
snapshot append/panic therefore blocks retarget. On nonempty A, publish no
manifest change and tell the user to re-enable the same A and drain, or purge
while inactive. Only after an empty proof initialize B's stable active/spool
skeleton, reset empty-A counters, and atomically publish a fresh B
generation/paths. Active A→B is always rejected; active same-A enable remains a
no-op.

POST the canonical shared-schema envelope at no more than 100 events/256 KiB to
`/v1/events`. Send immutable batches whole or as deterministic immutable
sub-batches. A complete response must account for every ID. Durably quarantine
permanent rejections, then delete and parent-sync the batch only when every ID is
accepted, duplicate, or quarantined. There is no fragile per-ID checkpoint:
network failure, incomplete/mismatched response, 429/5xx, or a crash retains the
unchanged batch for full deduplicated retry. Malformed/truncated local records go
to bounded quarantine. Only one helper drains at a time.

Each helper captures consent generation and exact active/spool paths. It must
match all three under gate + shared request lease before every request. Retarget
cannot occur while active or while an old A helper holds drain. Inactive same-A
enable waits that helper's drain lease before changing generation and launches
one replacement only after releasing every handoff lock.

Cap the selected active file plus owned spool/quarantine storage at 25 MiB and
expire local records after 30 days. Surface dropped/expired counts only through
local `status`; never emit recursive telemetry about telemetry. Preserve the
existing one-off-file behavior outside this cap.

The endpoint is configured at build/release time and accepts only the separate
upload-v1 shape from `telemetry-schema-v2`. No client secret or stable identity is
embedded in the public binary.

Purge is allowed only from inactive consent. It takes exclusive collection
lease, waits all existing invocation snapshots, then takes the drain lease. It
next validates the retained inactive generation/paths under the consent gate and
holds that gate while taking the queue lock and clearing state, so re-enable
cannot retarget the operation. It durably clears only recognized managed data,
quarantine, status, temp records, and counters, then atomically recreates a safe
empty selected active JSONL. It preserves the private spool directory and the
existing `queue.lock`/`drain.lock` files and inodes; live locks are never
unlinked or recreated. Disable has already waited for requests, so purge cannot
race networking.

## How to Verify

- A fake endpoint proves prior local-v1 and current local-v2 records are projected
  and drained after `enable`, with event IDs assigned and durably persisted only
  in immutable uploader batches.
- Default/custom/unset enable resolution and a later environment change prove
  only the consent-stored active/spool pair is collected/rotated/drained;
  unselected explicit custom paths remain local and untouched. Existing default
  and custom local-v1 JSONL files import from their selected paths, and two
  different same-parent selected files derive non-colliding spools/locks.
- Concurrent append/rotate, corrupt/truncated lines, helper contention, network
  loss, 429/5xx, partial/mismatched responses, permanent rejection,
  commit-before-response loss, and a helper crash before/after every file/parent
  `fsync`, rename, projection, quarantine, response, and deletion transition
  produce no silent loss; startup reconciles every raw/batch/panic/temp state and
  retries deduplicate.
- Default/disabled consent and `NC_TELEMETRY=0` perform no automatic collection,
  helper launch, or network. Missing/torn/unreadable/unknown/symlinked consent
  fails closed. Exact check→disable→send scheduling proves either a shared lease
  protects the bounded request while disable waits, or disable wins and the send
  never starts. Timeout tests prove disable returns only after no request is in
  flight.
- Invocation snapshot tests show disable does not wait for a running conversion;
  an already-started invocation may append exactly one final queued event, but
  launches no helper. Explicit per-run telemetry installs no panic hook.
- Active A→B is rejected; active same-A enable is an idempotent no-op with
  unchanged generation and no helper launch. Inactive A→B waits old
  snapshots/helper and rejects any A active record, batch, panic, data-bearing
  temp, or quarantine without publishing B, directing same-A drain or purge.
  Empty A atomically transitions to fresh B generation/paths.
- An exact inactive same-path schedule holds the old helper's drain lease while
  enable begins and lets an old consented conversion finish after disable.
  Enable waits collection-exclusive for its final append, then waits drain
  without holding the gate, publishes only after both old actors exit, releases
  all locks, then leaves exactly one replacement helper to immediate-drain.
- Unix/Windows subprocess tests race inactive-only purge with managed collection
  append, explicit `--telemetry` append to the retained active path, panic,
  rotation, drain, disable, and re-enable. Purge waits
  collection-exclusive, cannot race a request, removes only recognized selected
  data, recreates a safe empty active file, preserves the spool and original lock
  inodes, and exhibits no deadlock. A final-transition purge-versus-explicit-
  append race on Unix and Windows proves no split-brain lock recreation.
- `preview` exactly matches transmitted JSON and contains none of the forbidden
  fields in the strategy manifest.
- Queue age/size caps and quarantine/status counters are deterministic and do not
  emit recursive events.
- Platform tests cover detached invocation and locking on supported Unix and
  Windows targets without imposing a platform requirement on the protocol.
- A deliberately hanging endpoint proves no endpoint latency reaches the
  foreground. Failed launch leaves recorded timing, stdout/stderr, exit code,
  output TIFF, sidecar, and report unchanged; a separate platform-tolerant test
  checks bounded launch overhead rather than impossible wall-clock equality.
- The canonical local-v2 ready panic fixture owned by
  `telemetry-schema-v2`'s shared corpus is consumed
  through projection, acknowledgement/deletion, retry/deduplication, and
  permanent rejection/quarantine.

## Dependencies

- [Telemetry event schema v2](telemetry-schema-v2.md) — supplies local event
  parsing, pure legacy projection, shared fixtures, and the upload privacy
  projection.
- [Telemetry ingestion service](telemetry-ingestion-service.md) — supplies the
  deployed `/v1/events` acknowledgement and retry contract.
