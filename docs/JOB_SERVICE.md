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
stage tuning, scheduling, application logging and remaining acceptance requirements
are tracked separately in RESTORATION_1_2.md. Do not publish 1.2.0 from this milestone alone.
