# Job service

Run `sirius-asset-updater serve sirius-service-config.yaml`. The existing one-shot CLI remains
available. Set the environment variable named by `token_env` to a distinct service bearer token.
All job routes require that token; `/health` is unauthenticated process liveness.

| Method | Route | Operation |
| --- | --- | --- |
| POST | `/api/v1/jobs` | Submit configured work; 202 with persisted job (or a dry-run plan) |
| GET | `/api/v1/jobs` | List retained jobs |
| GET | `/api/v1/jobs/{id}` | Get status/progress |
| POST | `/api/v1/jobs/{id}/cancel` | Request cancellation |
| POST | `/api/v1/jobs/{id}/retry` | Create a new job from a failed/cancelled job |

Request example:

```json
{"region":"jp","profile":"jp-full","operation":"update"}
```

Operations are `update`, `export` and `verify`. Profiles are configured by the operator, never
filesystem paths or credentials supplied by an HTTP caller. `update` downloads and verifies a
publication and, when `export_config` is set, exports it. Standalone `verify` and `export` require
a configured `input`; standalone export also requires `export_config`. Region must match the
profile and source receipt. CN remains reserved. Global capabilities remain those in REGIONS.md.

Submission accepts an optional `Idempotency-Key` header (1–128 ASCII letters, digits, `-`,
`_`, `.`, or `:`). Reuse the key with the same region/profile/operation after a lost response:
the service returns 202 with the original job's **current** state, without queuing another job.
This also works when the queue is full and after a process restart. A retained key reused with a
different valid request returns 409; malformed or duplicate headers return 400. Authentication,
profile validation and shutdown admission checks still apply. Requests without a key keep their
existing behavior and always submit new work.

The key is scoped to the entire service ledger, so trigger clients should include the region,
profile, operation and catalog identity in their key derivation. Only its SHA-256 digest is stored
and returned as `idempotency_sha256`; keys must not contain secrets. Terminal jobs continue to
reserve their key until ledger retention removes the record. After pruning, the same key can
create a new job: this is not an indefinite exactly-once guarantee. Trigger owners must persist
the acknowledged job ID and completion state. Failed/cancelled submissions return the original
job on replay; use the explicit retry route to request new execution. The retry route creates
new work each time and does not inherit or accept this submission idempotency contract.
A submission key does not pin a catalog version: `update` still obtains and validates the current
snapshot when it executes. An owner integration must reconcile the resulting receipt identity.

## Dry-run planning

Add `"dry_run": true` to a submission to preview it without creating work (restored from the
original Haruki `dry_run` request field; `false` or omitted keeps normal submission):

```json
{"region":"jp","profile":"jp-full","operation":"update","dry_run":true}
```

Authentication, the optional User-Agent filter, shutdown admission (503), strict body parsing
and profile/region/operation validation (400) are identical to a real submission. The service
then re-reads the profile's documents on the blocking pool, applying `SIRIUS_ASSET__*`,
`SIRIUS_ASSET_EXPORT__*` and `SIRIUS_ASSET_STORAGE__*` overrides, exactly as a worker would at
execution time, so edits made after startup are reflected. `update` loads the download document
(region/logging checks, offline `check` validation and secret presence); `update`/`export` with
`export_config` validate the export document and CRI key presence; `storage_config` resolves the
`plan-storage` provider preview. `verify` reads no documents.

A dry run never creates or persists a job, never consumes a queue slot or completion reservation,
never creates output/cache/storage directories and never contacts the Game API, CDN, STS or
storage. Like the original, it stops after planning: catalog selection is **not** resolved (that
requires the live snapshot/catalog), so `selection` describes configured keys and pattern counts,
not selected files. FFmpeg availability, standalone `input` contents and storage permissions are
not probed; a ready plan is not a guarantee that execution succeeds.

The response is 200 when `ready` is true and 422 when a prerequisite would fail the job; both use
the same shape. It has no job `id` and cannot be polled:

```json
{"dry_run":true,"ready":true,"issues":[],
 "request":{"region":"jp","profile":"jp-full","operation":"update"},
 "steps":["download","verify","export","verify_export","publish"],
 "download":{"environment":"release","platform":"iOS","client_version":"1.0.3",
   "protocol_version":"...","refresh_enabled":true,"catalog_only":false,"decryption_enabled":true,
   "selection":{"entire_catalog":false,"keys":["InitialDownload"],"include_patterns":0,
     "exclude_patterns":0,"priority_patterns":1},
   "missing_secrets":0,"invalid_secret_fields":[]},
 "export":{"retain_outputs":true,"incremental_cache":true,"secrets_ready":true},
 "storage":{"providers":["primary"]}}
```

`issues` uses stable codes: `download_config_invalid`, `download_secrets_not_ready`,
`export_config_invalid`, `export_secrets_not_ready` and `storage_config_invalid`; a section
whose document is invalid is omitted. Like job status, the plan contains no filesystem paths,
URLs, storage prefixes, environment variable names or secret values (only counts and static
field names). Use the operator-side `check` and `plan-storage` commands for full detail.

`Idempotency-Key` is rejected with 400 on a dry run (including malformed or duplicate headers):
a preview neither looks up, reserves nor replays a key, so the same key remains available for
the real submission. Planning is bounded by `timeout_seconds` and abandoned when shutdown
begins; both return 503. An abandoned configuration read has no side effects.

Successful new jobs include an `outcome` saved in the same ledger transaction as `completed`:

- `verification`: verified catalog SHA-256, receipt environment/resource version/platform hash,
  region/platform, full-catalog scope and verified input counters.
- `export`: null when no export ran; otherwise full-export scope, final local-retention state,
  output file count and output bytes. Validation-only export is not retained publication.
- `publication_id`: null when no storage publication ran; otherwise the UUID of the verified
  storage publication described by the job's `publication.json`.

Queued/running/failed/cancelled jobs have no outcome. An interrupted worker cannot leave a
successful outcome; ledger write failure cannot acknowledge completion. Older completed records
without this field remain readable but cannot prove catalog identity for automated reconciliation.
Export catalog digest, region, platform and full-catalog scope must match the earlier input
verification before the pipeline can complete. A successful subset or catalog-only job remains a
subset: consumers must check `verification.full_catalog`, `export.full_export` and the requested
retention/publication requirements, rather than trusting `completed` alone.

The result excludes filesystem paths, CDN URLs and credential references. Version labels come
from the verified local receipt; offline verification is not an independent assertion that the
server currently advertises that version. Trigger owners must compare the outcome with their
requested region/environment/platform/resource version/platform hash and reconcile newer work.
The publication UUID identifies a receipt; it does not provide a public download URL.

Outputs are separated under `output_directory/<region>/<job-id>/`. Download output and export
input/output paths are set by the service; other pipeline options come from the configured files.
The `output` of a profile's download document and the `input`/`output` of its export document are
still required (the download `output` must be non-empty; the export paths are not otherwise
checked), then replaced for each job without a warning; the profile's own `input` is used only by
`export`/`verify` and ignored, unvalidated, by `update`.
Use `assets` in the download configuration to request all remote resources. Omitting it remains
catalog-only; a catalog-only result cannot pass a requested full export.

The ledger is persisted before submission is acknowledged. Queue overflow returns 429; only one
job per region executes at once, with a configurable total execution limit. A running cancellation
stays `cancelling` until its worker actually exits. Export cancellation waits for active resources
and bounded media subprocesses; it is not an immediate process kill. Never remove a job's files
while cancellation is pending. Download progress is sampled once per second after catalog selection:
`total` counts selected remote resources, `completed` counts fully downloaded/cache-restored and
validated batches, and `bytes` counts their input bytes (including cache hits). These are not
wire-transfer bytes or partial-file percentages; the catalog itself is excluded. Before a valid
plan exists, total remains unknown. Each attempt resets counters, and progress does not imply
publication: final version validation and atomic publication must still succeed.
Export progress is read from its summary once per second; final counts are persisted even for
short jobs. Verify progress counts the catalog plus the downloaded assets actually verified;
full-catalog scope remains in verification.json. A job deadline reports `failed` with
`job_timeout`, distinct from an acknowledged user cancellation. Deadlines send a cancellation
signal and wait for workers to exit before releasing the region slot. A progress-storage failure
also signals cancellation and drains the exporter.

`allow_cancel` defaults to `true`, preserving existing behavior and the original Haruki policy.
Set it to `false` to reject authenticated `POST /api/v1/jobs/{id}/cancel` calls with HTTP 409
before any ledger mutation. Authentication still runs first (unauthenticated calls return 401).
The option applies to both queued and running user cancellations and is loaded on service startup.
Restart with `true` to permit cancellation again, including previously queued work. It does not
turn off job deadlines, service shutdown, pipeline/storage-failure cleanup or interrupted-worker
recovery. Failed/cancelled jobs remain eligible for the existing explicit retry operation.

SIGINT/SIGTERM stop admission and cancel/drain running workers. Queued jobs survive restart;
interrupted running jobs become failed and can be retried explicitly. Completed publications
are retained. The ledger directory has exclusive process ownership, so two services cannot
schedule from one ledger concurrently. Terminal retention prunes records only, not outputs.

Profiles can opt into [incremental export](EXPORT_CACHE.md); successful resource caches
survive job retries and service restarts while each job retains independent outputs.

Profiles can set `storage_config` to publish retained exports to configured [local/S3 storage](STORAGE.md).
All destinations must pass upload/read-back before optional local export cleanup.

Production acceptance of the service (full and incremental JP and Global HK runs, restart,
cancellation, independent per-file audit) is recorded in [RESTORATION_1_2.md](RESTORATION_1_2.md).

## Shared media budget

`max_media_processes` bounds active media work across every job of this service (default 4,
range 1..16). Each export also retains its own `media_concurrency` limit. Both limits must be
acquired before starting FFmpeg or an FFI conversion. This includes media decode, mux, encoding
and independent media verification calls routed through the exporter. Waiting for either slot
consumes the same operation deadline and checks cancellation; failed admission releases any
already-held local slot. Auto backend fallback releases the FFI slot before reacquiring for CLI.
Standalone CLI exports retain their existing per-export limit without a service-level cap.

This controls simultaneous operations, not FFmpeg's internal thread count or total process RSS.
Apply deployment CPU/memory limits separately. Per-export CPU sizing, stage limits and throttling are
configured in export profiles (`cpu`, `stage_limits`; see [EXPORT.md](EXPORT.md) and
[EXPORT_OPTIONS.md](EXPORT_OPTIONS.md)); `max_cpu_stages` is the service-wide pool. The service budget
does not change cache identity or output formats.

## Shared upload budget

`max_uploads` limits object upload attempts across every job's storage providers (default 4,
range 1..32), in addition to per-publication `storage.concurrency`. It includes local and S3
objects, read-back verification and completion markers. Jobs waiting for admission do not open
source streams or create writers; their wait consumes the same per-attempt timeout. Permits are
released before retry backoff and after any bounded multipart abort. Cancellation drains work
and releases queued waiters, preserving the existing publication/cleanup contract. Standalone
storage commands keep the configured per-publication limit.

## Shared CDN download budget

`max_downloads` limits simultaneous CDN download attempts across all jobs (default 4,
range 1..64). Catalog and resource downloads share these slots; per-profile asset concurrency
still limits each resource batch. Admission, headers, streamed body, file writes and final sync
share the existing `network.download_timeout_ms` attempt budget. A queued timeout sends no
request. Cancellation releases the permit when the download future is dropped, and retries
release slots during backoff. Failed or partial files remain in unpublished staging.

Snapshot/version control requests do not consume CDN slots. Verified download-cache hits skip
network admission. Decryption and export run after download admission is released; this setting
is not an in-flight decoded-memory limit. Standalone downloads use the same attempt deadline
without a service-wide semaphore. Existing retry counts and whole-run/job deadlines still apply.

## Soft resource byte budget

`max_in_flight_bundle_bytes` is optional at both service and export configuration levels (default
0, disabled). Enabled limits are acquired together before decoding a resource; the service
budget is shared across all jobs, and the export budget applies to its own workers. Weight is
the verified main-resource file size plus on-disk sizes of the distinct Unity dependencies.
Permits remain held through decoding/output creation and release on success, failure, panic
unwind or cancellation. Cache hits bypass decoding and do not consume this budget. A resource
larger than a configured budget consumes that entire budget exclusively rather than deadlocking.
Waiters check cancellation every 20 ms, allowing job shutdown/deadline cancellation to drain them.

This adapts Haruki's estimated bundle-byte admission to Sirius's staged download/decode pipeline.
Download streaming has its separate shared concurrency bound; source bytes are already on disk
before export. Compressed input size does not predict decompressed textures/PCM, retained indexes,
FFmpeg allocations or filesystem cache, so this is **not a hard RSS ceiling**. Continue using
container/systemd limits for total memory. Zero preserves prior behavior; no arbitrary default
memory budget is inferred from the host. Budget settings do not change exported content or cache
identity. Remaining CPU/stage tuning and final production resource acceptance are still required.

## Retained-export verification progress

During `verify_export`, job progress reports the number and bytes of payload files whose hashes
have been checked, with the expected output-file total from the completed export summary.
The verifier updates an in-memory snapshot every 256 files and at resource boundaries; the
service persists the latest counters at most once per second. No filenames or credentials are
included. The two receipt/index files are not counted as payload files. Reaching the expected
file count is not completion: journal totals, exact-tree checks and inventory persistence must
still succeed before the job can become completed. Corruption remains a failed job, and failed
progress persistence or cancellation stops verification without acknowledging success.

`max_cpu_stages` optionally caps instrumented native decode and CLI/FFI media stages across
all concurrent exports (1..256, default omitted/null). It composes with profile-local
`cpu.limit_stages` and existing stage/media/byte budgets. See [CPU admission](EXPORT.md#aggregate-cpu-stage-admission)
for acquisition order, timeouts and the distinction from an OS CPU quota.

## Completion notifications

The job ledger now writes schema version 2 and reads versions 1 and 2. Opening a legacy
ledger migrates it; older binaries that only understand version 1 cannot reopen the migrated
state. Back up stopped-service state before upgrading; do not downgrade a live ledger.

When `completion_notifications` targets are configured, a successful job with an outcome records
its completion event and pending recipient identities in the same atomic ledger write. Events
survive terminal-job pruning and restart; acknowledging one recipient leaves other recipients
pending. Changing configured recipients does not reroute existing events. Failed/cancelled jobs
do not generate successful completion notices.

The outbox holds at most 4,096 events, each bounded to 64 KiB, with at most 16 recipients.
Admission reserves room for accepted queued/running work in configured regions. Full queues
reject new affected work rather than losing completion notices. Persistence failure leaves
the previous in-memory state intact. Recipient identities are opaque hashes, not credentials.

Configure up to 16 named regional recipients (empty/omitted disables new notifications):

```yaml
completion_notifications:
  - name: asset-index
    region: jp
    endpoint: https://index.example.com/internal/asset-completions
    token_env: SIRIUS_ASSET_COMPLETION_TOKEN
    timeout_seconds: 10 # 1..60; includes response body
    retry_seconds: 30   # 1..3600; fixed delay after every unsuccessful attempt
```

The region must have a service profile. Endpoints require HTTPS, or HTTP with a literal loopback
IP for local services/tests. URL credentials, query strings and fragments are rejected. Requests
use a dedicated Bearer token, verified TLS, no redirects, no ambient proxy and no transparent HTTP
retries. Tokens must differ from the service token, configured download API/CDN credentials and
other completion-target tokens. Do not reuse storage or other service credentials. Tokens are
loaded on startup; rotate them with a restart. No endpoint or token is included in delivery status.

Each POST body is `{schema_version:1,job_id,request,outcome,completed_at}`; `request` and `outcome`
use the job contracts above. `Idempotency-Key` is the job UUID. Accept by returning HTTP 202 and
exact JSON `{"schema_version":1,"job_id":"THE_RECEIVED_UUID"}` (at most 1 KiB; no extra fields).
Other statuses, mismatched/malformed acknowledgements, timeouts and failed acknowledgement
persistence leave the event pending. The receiver must durably deduplicate by job UUID before
acknowledging: delivery is at least once and a crash after acceptance can cause a duplicate.
Check outcome scope/version before updating an index; completion does not imply a full export.

Each target has an independent worker, at most one request in flight and FIFO pending-event
selection. One failed recipient does not block other recipients or jobs. A recipient's rejected
head event remains pending and blocks later events to that recipient until corrected. Workers
reconcile on startup and task completion, and poll at one-second intervals. Retries resend only
the persisted notice, never rerun the pipeline. All failures retry with the configured delay;
there is no automatic discard. Shutdown cancels pending HTTP requests and retains unacknowledged
events. Events for removed or changed recipients remain pending: restore the original name,
region and canonical endpoint to resume them. Credential rotation keeps recipient identity.
There is currently no administrative discard/retarget operation.

`GET /api/v1/completion-notifications` requires the service token. It reports total pending events,
per-target pending counts, attempts, last attempt/acknowledgement times and sanitized error codes,
plus the number of deliveries whose recipient is no longer configured. Pending events are durable;
attempt counters/timestamps and retry timers reset on restart. Acknowledged events disappear from
the outbox. Backlog can eventually reject new affected jobs under the capacity rule above.

### Optional client identification filter

Service `user_agent_prefix` optionally requires a case-sensitive prefix on one valid User-Agent
header for every protected job/status/notification endpoint. Omit it for the previous behavior.
The configured prefix must be nonblank printable ASCII and at most256bytes. Missing, malformed,
duplicate or nonmatching User-Agent headers return401 before job mutation; duplicate Authorization
headers also fail. A matching User-Agent never replaces the required Bearer token. Restart to apply.

For API-owned jobs set `asset_dispatch.targets[].user_agent` in sirius-api-proxy to a value starting
with this prefix, for example updater `SiriusClient/` and API target `SiriusClient/api-proxy`.
Other clients must supply the same header on submissions and polling. The filter is client
identification only; it is not a secret or a second authentication factor. Health routing is unchanged.
