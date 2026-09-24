# Update trigger architecture audit

Reference: Haruki-Sekai-API `07da6b80e6a59ece89251f4694afe94bea72e131` and
Haruki-Sekai-Asset-Updater `3d33ed037f0ef5009e361e0535b3b19f8c239947`.

The original asset service exposes HTTP submit/list/detail/cancel routes in
`src/service/http.rs`. It does not run an independent recurring catalog update timer.
The API owns cron scheduling (`src/updater/scheduler.rs`), detects asset changes in its
master/version update path, and dispatches HTTP requests to configured asset updater servers
(`src/updater/master.rs`, `call_all_asset_updaters` / `call_asset_updater`). Original polling
progress uses the job HTTP endpoints; there is no asset-service WebSocket notification route.
Do not confuse the export resource scheduler with recurring version checks.

Sirius restoration should preserve this responsibility split: the API/version owner detects
catalog changes; authenticated updater profiles execute the requested region's pipeline.
Do not restore Sekai asset-hash payloads or introduce a second timer that blindly exports the
same version. Native Sirius label selection remains documented in SELECTION.md.

## Implemented prerequisite

The updater's durable job submission supports optional idempotency keys. Concurrent retries,
queue saturation and restart retain the same acknowledged job identity while its record is
retained. See JOB_SERVICE.md for the exact boundary, terminal-job behavior and retention limit.
Successful jobs also persist a credential-free catalog/export/publication outcome atomically with
completion. API consumers can reconcile actual processed identity and scope without access to the
updater filesystem. This is infrastructure for reliable dispatch, not a claim that owner-to-updater
integration is already complete.

## Remaining owner integration

- Persist per-region, per-target pending/acknowledged/completed dispatch state and catalog identity.
- Send only configured profile/region/operation to the updater with a deterministic, non-secret
  idempotency key. Use explicit destination credentials and bounded transport policy.
- Poll acknowledged job IDs; reconcile completed receipts with the requested catalog identity.
  HTTP acceptance is not successful publication. Handle failed/cancelled/pruned jobs explicitly.
- Prevent dispatch feedback loops when an updater refreshes its API snapshot; prevent a delayed
  update from being recorded as publication of a catalog it did not process.
- Reconcile after restart and transient failures without unbounded retry storms; test region
  isolation, credential scope, lost acknowledgements, retention and publication failures.
- Connect completion notification/registry publication only after verified exports/storage.

These remain 1.2.0 release requirements, alongside full production acceptance.
