# Changelog

## 1.2.1
- Global assets are live-verified: full hk catalog export (13,429 resources, 953,052 files, incl. 27,741 WAV from CRI) and en/kr selections against the production CDNs, with a fully cache-hit incremental rerun.
- dry-run readiness checks the split-ACB secret whenever a job would read it; the CDN username is included in the notification-token overlap check.
- Shader exports raise only the total array element budget to 32,000,000 (unity-rs 0.5.2 `read_shader_text_with_limits`): a Global Unity 6000.3 URP shader has 4,010,378 elements, above the library default of 4,000,000.

- Global (HK/EN/KR) asset download, verification and export. The updater accepts schema-3
  snapshots with an explicit `catalog_layout`, `catalog_url`, `bundle_base_url` and
  `cdn_authorization`. It recomputes the URLs from the layout and rejects any difference. The
  Global layout is `{root}/asset/{platform}/catalog_{version}[_{locale}].bin` with a sibling
  `.hash`; bundles are in `{root}/asset/{platform}`. JP URLs and schema-1/2 snapshots are
  unchanged. Schema-2 snapshots are refused for Global.
- `catalog_locale` (`en`, `zh-Hant`, `zh-Hans`, `ko`; Global only) selects a localized catalog
  per profile. The default is the base catalog.
- Global downloads read the catalog `.hash` before and after the run. The base hash must match
  the snapshot's `platform_hash`, and any change fails the run without publishing.
- Remote bundle ids `https://dummy.net/asset/{platform}/…` map onto the bundle base. Every other
  absolute URL is still rejected.
- `cdn_roots.*.authorization: none` (HK/EN/KR only; no `username_env`/`credential_env`) sends
  no Authorization header and needs no CDN secret. `basic` stays the default and is required for
  JP.
- Receipts record `catalog_layout`, `bundle_base_url`, `catalog_locale` and `catalog_hash`.
  Verification and export use the stored bundle base instead of stripping the catalog URL.
  Older JP receipts still verify.
- Global examples use the verified server-list CDN roots with anonymous access. Dry-run plans
  show `catalog_locale`.
- Upgrade note: a proxy's `resource_snapshot` needs this updater. Older updaters reject
  schema-3 snapshots.
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
