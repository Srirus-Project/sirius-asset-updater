# Job service (1.2.0 development)

Run `sirius-asset-updater serve sirius-service-config.yaml`. The existing one-shot CLI remains
available. Set the environment variable named by `token_env` to a distinct service bearer token.
All job routes require that token; `/health` is unauthenticated process liveness.

| Method | Route | Operation |
| --- | --- | --- |
| POST | `/api/v1/jobs` | Submit configured work; 202 with persisted job |
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
Use `assets` in the download configuration to request all remote resources. Omitting it remains
catalog-only; a catalog-only result cannot pass a requested full export.

The ledger is persisted before submission is acknowledged. Queue overflow returns 429; only one
job per region executes at once, with a configurable total execution limit. A running cancellation
stays `cancelling` until its worker actually exits. Export cancellation waits for active resources
and bounded media subprocesses; it is not an immediate process kill. Never remove a job's files
while cancellation is pending. Export progress is read from its summary once per second; final counts are persisted even for
short jobs. Verify progress counts the catalog plus the downloaded assets actually verified;
full-catalog scope remains in verification.json. A job deadline reports `failed` with
`job_timeout`, distinct from an acknowledged user cancellation. Deadlines send a cancellation
signal and wait for workers to exit before releasing the region slot. A progress-storage failure
also signals cancellation and drains the exporter.

SIGINT/SIGTERM stop admission and cancel/drain running workers. Queued jobs survive restart;
interrupted running jobs become failed and can be retried explicitly. Completed publications
are retained. The ledger directory has exclusive process ownership, so two services cannot
schedule from one ledger concurrently. Terminal retention prunes records only, not outputs.

Profiles can opt into [incremental export](EXPORT_CACHE.md); successful resource caches
survive job retries and service restarts while each job retains independent outputs.

Profiles can set `storage_config` to publish retained exports to configured [local/S3 storage](STORAGE.md).
All destinations must pass upload/read-back before optional local export cleanup.

This is an implementation milestone, not the complete 1.2.0 platform restoration. Additional publication,
stage tuning, version-trigger integration and remaining acceptance requirements
are tracked separately in RESTORATION_1_2.md. Do not publish 1.2.0 from this milestone alone.

## Shared media budget

`max_media_processes` bounds active media work across every job of this service (default 4,
range 1..16). Each export also retains its own `media_concurrency` limit. Both limits must be
acquired before starting FFmpeg or an FFI conversion. This includes media decode, mux, encoding
and independent media verification calls routed through the exporter. Waiting for either slot
consumes the same operation deadline and checks cancellation; failed admission releases any
already-held local slot. Auto backend fallback releases the FFI slot before reacquiring for CLI.
Standalone CLI exports retain their existing per-export limit without a service-level cap.

This controls simultaneous operations, not FFmpeg's internal thread count or total process RSS.
Apply deployment CPU/memory limits separately. Shared download/memory admission and the
remaining original resource tuning controls are separate restoration work. The service budget
does not change cache identity or output formats.

## Shared upload budget

`max_uploads` limits object upload attempts across every job's storage providers (default 4,
range 1..32), in addition to per-publication `storage.concurrency`. It includes local and S3
objects, read-back verification and completion markers. Jobs waiting for admission do not open
source streams or create writers; their wait consumes the same per-attempt timeout. Permits are
released before retry backoff and after any bounded multipart abort. Cancellation drains work
and releases queued waiters, preserving the existing publication/cleanup contract. Standalone
storage commands keep the configured per-publication limit.
