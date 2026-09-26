# Changelog

## 1.2.1 (unreleased)

- **Breaking:** rename the Global Traditional Chinese region from `tw` to `hk`, matching the
  game's own identifiers (`/prod/hk_…`, `l12-prod-hk-…`). All output uses `hk`: job records,
  summaries, receipts, logs, cache identities, regional API routes, publication object keys
  and `{region}`/`{server}` storage templates. Published paths move from `…/tw/…` to
  `…/hk/…`; existing publications are not relocated.
- Accept `tw` only as a deprecated input alias for `hk` in configurations, job requests and
  the `verify-export` CLI argument, logging one warning per process.
- Read API snapshots and on-disk receipts/journals whose region is `tw` as `hk`, so older API
  proxies and existing state keep working. Completion-target identities are unchanged; download
  and export caches keyed by `tw` are missed and rebuilt rather than reused.
- See `docs/REGIONS.md` ("Upgrade to 1.2.1") for migration notes.

## 1.2.0

Restores the reusable service capabilities of the original Haruki asset updater for Sirius.
Sekai chart-hash, character-ID and 3D model adapters are not copied; see
`docs/HARUKI_CONFIG_AUDIT.md` for the field-by-field audit and `docs/RESTORATION_1_2.md` for the ledger.

- Authenticated long-running job service: durable queue, idempotent submission, status and
  progress, cancel/retry, retention, safe restart and shutdown, and offline dry-run planning.
- Service jobs run the real Sirius catalog, download, verification, decryption and export
  pipeline; downloads and decoded exports are reused by verified content identity.
- Native catalog selection with include/exclude filters, priorities and exact Unity types;
  partial selections are never reported as full exports.
- Image, audio and video formats with CLI or FFI media backends, CRI ACB/USM decode or
  preserve policies, raw Unity bundle export, and bounded retry of transient FFmpeg failures.
- Local and S3 publication with verified read-back before cleanup, credential references,
  STS/credential files and public-read policies.
- Separate stage concurrency, resource budgets and CPU policy; TLS, proxies, application logs
  with per-target levels, and access logs with optional templates.
- Durable completion notifications.
- Environment path overrides for every configuration document and optional remote loading of
  the download configuration; all configuration structures reject unknown fields.

## 1.1.0

- Add explicit JP/TW/EN/KR identities and reserve CN without enabling unverified networking.
- Separate region, platform and environment; reject mismatched known service endpoints.
- Document capability boundaries and paired-service upgrade requirements in `docs/REGIONS.md`.
- Validate snapshot region/platform/protocol identity before contacting a CDN; retain legacy JP receipt support.
- Support safe CDN base paths and Android catalog paths without dropping region-specific prefixes.
- Include region/platform in publication names and export summaries; scope cache identities by region/environment/platform.
- Keep Global end-to-end asset acquisition gated on valid observed snapshots and separately configured secrets.


## 1.0.0

- Initial public-release candidate for BanG Dream! Our Notes.
- Replace the pre-release Viola codename with Sirius configuration filenames,
  SIRIUS_* environment variables and container user names. Old names are not aliases.
- Retain the appropriate Haruki derived-from attribution and MIT notices.
- Include runtime files, configuration examples, documentation and licenses in release archives.
- Keep real credentials, downloaded content and private research out of public artifacts.

See README.md for supported features, validated scope and current limitations.
