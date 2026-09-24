# Sirius 1.2.0 restoration and acceptance ledger

Objective: retain Haruki's reusable service/platform capabilities, replace all Sekai-specific logic with Sirius implementations, test the complete production pipeline on yhm01, then publish both repositories as 1.2.0. A working CLI or one successful export alone is not completion.

Reference baseline: local Haruki-Sekai-API 07da6b80e6a59ece89251f4694afe94bea72e131 and Haruki-Sekai-Asset-Updater 3d33ed037f0ef5009e361e0535b3b19f8c239947. Preserve MIT attribution. Existing Sirius release baselines: API 5ab1e390ca514e550cb609ae4d299b19c105b79e, updater e7ffb7b905958de88aa8d5a06327dafcc8acd8ce.

## API requirements (all pending)

- Multi-region service configuration/routing with per-region protocol family, credentials and isolated state; retain v1.1 single-region config compatibility and reserved CN.
- Account pool, per-account locking, selection, health/cooldown and credential reload. No speculative retries of account mutations; no fabricated Global login.
- Configurable response cache, optional Redis backend, region/account/protocol-aware identity and invalidation; private data never shared through public cache.
- Upstream proxy, deadlines/limits, controlled retry policy, server TLS, logs/access logs and trusted-forwarding configuration.
- Remote node routing, priorities/failover, token-scoped internal calls and capability/version boundaries.
- Master registry/manifests, owner/consumer synchronization, notifications, optional generic persistence and Git publication. Do not restore Sekai table models or Ent code.
- Full documented config surface, examples, migration, meaningful local integration tests including failure/authorization cases.

## Updater requirements (all pending unless specifically evidenced)

- Authenticated long-running HTTP service, region configs, job submission/list/detail/cancel/retry, bounded queue/concurrency, progress, retention and safe shutdown/restart.
- Invoke Sirius catalog/download/verify/export pipeline from the service, not a stub or arbitrary shell executor; retain CLI support.
- Incremental download/export records by verified content identity; cache reuse, partial failure recovery and immutable publication semantics.
- Include/exclude filters, priority ordering, exact type selection and scoped replay; selected work is never claimed as full-catalog verification.
- OpenDAL/local/S3 providers, upload concurrency/retries and verified upload-before-cleanup policy; region-scoped object keys and secret references.
- Format configuration for image/audio/video and backend choice where implementable for Sirius; retain correct alpha/split audio semantics.
- Separate stage concurrency, bounded resource budgets, configurable retries/timeouts, logging/access logging and TLS/proxy configuration.
- Update trigger/scheduling, completion notification and generic publication integration where applicable. Sekai chart-hash/character-ID/3D model adapters are removed, not copied.
- End-to-end tests cover queue saturation, cancellation, restart, per-region serialization, incremental behavior, upload failure safety and secrets.

## Release gates (all pending)

- Audit each feature against original code/config; no placeholders or silent ignored config fields; record any game-specific non-applicability with evidence.
- Both repositories fmt/check/Clippy/tests and release packaging smoke pass.
- yhm01 official candidate full JP download/decrypt/export through restored service; retain and independently hash every exported file.
- yhm01 verify incremental second run, job API/auth/cancel, resource limits, restart and configured storage path; existing Haruki services unchanged.
- Global remains limited to verified contracts unless additional protocol research validates functionality; region isolation test for TW/EN/KR and CN rejection.
- Public source/artifact secret audit, three-platform artifacts, exact commits, version 1.2.0 tags/releases, published artifact audit.

## Work log

- Initial audit confirmed the missing components are real scope loss, not merely shorter examples/defaults.
- Implementation begins with the updater's durable job lifecycle and service orchestration; API restoration follows against the same ledger.

- Updater durable job ledger implemented and five lifecycle tests passed: restart interruption, exclusive state ownership, queue/retention bounds, per-region scheduling, cancellation acknowledgement, storage failure atomicity and reserved-region rejection. HTTP and pipeline integration remain pending; this does not satisfy the service gate yet.

- User requires category investigation before adapting filters. Verified Sirius catalog labels: InitialDownload, Everything, MV; use native key/dependency membership, not Sekai start_app/on_demand. Everything is a subset of full catalog (JP baseline: 13,364 versus 13,367 remote files). Exact tutorial/runtime required-address composition remains unverified; no invented preset.
