# Haruki configuration migration audit

Field-by-field comparison of the original updater configuration with Sirius. Reference:
Haruki-Sekai-Asset-Updater `3d33ed037f0ef5009e361e0535b3b19f8c239947` (MIT). Original citations
use `Haruki-Sekai-Asset-Updater@3d33ed03:path:line`; unqualified paths refer to this repository.
Private deployment configuration is not an audit input. Release gates are in
[RESTORATION_1_2.md](RESTORATION_1_2.md).

Classes: **reused** (same meaning), **adapted** (same capability, different shape or bound),
**not restored** (deliberate difference, rationale given), **not applicable** (game-specific or
internal to the original, evidence given). No original field is classified as missing: every
row below is either mapped or has a recorded reason. The audit covers configuration surfaces;
production acceptance of the mapped features remains a separate release gate.

Every Sirius configuration struct uses `deny_unknown_fields` (for example `src/lib.rs:111`,
`src/export.rs:22`, `src/service.rs:33`, `src/storage.rs:26`), so an original key copied
unchanged fails loading instead of being ignored. The original enforced this only for
`images`/`video`/`audio` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:863`,
`:883`, `:913`); its other sections silently dropped unknown keys.

Sirius splits the single original file into document kinds:

| Document | Type | Loaded by |
| --- | --- | --- |
| Download | `src/lib.rs::Config` | `check`, `probe`, plain run, service `download_config` |
| Export | `src/export.rs::ExportConfig` | `export CONFIG`, service `export_config` |
| Service | `src/service.rs::ServiceConfig` | `serve CONFIG` |
| Storage / publish | `src/storage.rs::Config` / `Command` | service `storage_config`, `publish`, `plan-storage` |

## Loading, environment and remote configuration

| Original | Class | Sirius |
| --- | --- | --- |
| `config_version` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:21`, checked at `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/load.rs:119`) | not applicable | No legacy Sirius schema to migrate; unknown shapes fail through `deny_unknown_fields` |
| `HARUKI_CONFIG_PATH` plus search of `./`, `../`, `../../` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/load.rs:241-249`) | adapted | `SIRIUS_ASSET_CONFIG_PATH`, default `sirius-asset-config.yaml`, no parent search (`src/config_source.rs:22-26`, `:100-118`). Other commands take an explicit path argument |
| `HARUKI_CONFIG_URI=opendal://…`, `HARUKI_CONFIG_OPENDAL_SCHEME/ROOT/OPTION_*` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/load.rs:24-30`, `:201-239`) | adapted | `SIRIUS_ASSET_CONFIG_URI=opendal://fs/KEY` or `opendal://s3/KEY` with typed `SIRIUS_ASSET_CONFIG_SOURCE__*`; download document only; URI plus path is an error instead of silent precedence. See [REMOTE_CONFIG.md](REMOTE_CONFIG.md) |
| `HARUKI__A__B=value` path overrides (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/env.rs:204-260`) | reused | One prefix per document kind, same path/value rules, applied before typed decoding (`src/config_env.rs:30-40`, `:77-103`). See [CONFIG_OVERRIDES.md](CONFIG_OVERRIDES.md) |
| `${env:VAR}` in any YAML string (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/env.rs:140-199`, `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/load.rs:105`) | not restored | Decision: secrets are typed `*_env` references (e.g. `src/lib.rs:127-141`, `src/assets.rs:37-38`, `src/storage.rs:97-101`) so values never enter parsed configuration, receipts or summaries; non-secret values use path overrides |
| 16 targeted `HARUKI_*` overrides (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/env.rs:17-83`) | adapted | Mapped to path overrides; see the table below |
| `RUST_LOG` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/service/logging.rs:97`) | not restored | Ignored by decision; `logging.level` accepts the original directive syntax instead ([APPLICATION_LOG.md](APPLICATION_LOG.md)) |
| `HARUKI_FLAT_PIPELINE` (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/payload.rs:239-248`) | not applicable | The source documents it as a benchmark switch, "Not a supported production mode" |
| `HARUKI_TEST_*` | not applicable | Test-only variables |

Original targeted overrides ran after the generic overrides and therefore won
(`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/load.rs:105-114`, `:154-160`). Sirius has one mechanism. Its values are YAML-typed:
booleans must be `true`/`false` (the original's `1`, `yes`, `on` spellings,
`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/env.rs:407-418`, fail typed decoding) and enum values must be the exact
lowercase names. An export override applies to every export document the process loads,
which matches the original's process-wide scope.

| Original variable (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/env.rs` line) | Sirius equivalent |
| --- | --- |
| `HARUKI_MEDIA_BACKEND` (18) | `SIRIUS_ASSET_EXPORT__MEDIA_BACKEND` = `cli`/`ffi`/`auto` |
| `HARUKI_ASSET_STUDIO_READ_BATCH_SIZE` (21) | None: no field; see [Unity read batching](#unity-read-batching) |
| `HARUKI_ASSET_STUDIO_IMAGE_FORMAT` (25) | None: the only legal original value is `raw_rgba` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/validate.rs:187-196`), an internal decode representation |
| `HARUKI_ASSET_HTTP_VERSION` (29) | `SIRIUS_ASSET__NETWORK__ASSET_HTTP_VERSION` = `auto`/`http1`. The env-only aliases `http1_only`, `http/1`, `http/1.1` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:82-96`) are not accepted |
| `HARUKI_MEDIA_ENCODE_CONCURRENCY` (36; set audio, video and aggregate) | Set both `SIRIUS_ASSET_EXPORT__STAGE_LIMITS__AUDIO_ENCODE` and `…__VIDEO_ENCODE` (1..64). `SIRIUS_ASSET_EXPORT__MEDIA_CONCURRENCY` (1..4) is the broader per-export gate over all FFmpeg/FFI operations |
| `HARUKI_AUDIO_ENCODE_CONCURRENCY` (42) | `SIRIUS_ASSET_EXPORT__STAGE_LIMITS__AUDIO_ENCODE` |
| `HARUKI_VIDEO_ENCODE_CONCURRENCY` (45) | `SIRIUS_ASSET_EXPORT__STAGE_LIMITS__VIDEO_ENCODE` |
| `HARUKI_DOWNLOAD_CONCURRENCY` (48) | `SIRIUS_ASSET__ASSETS__CONCURRENCY` (per job, 1..16); service-wide `SIRIUS_ASSET_SERVICE__MAX_DOWNLOADS` (1..64) |
| `HARUKI_POST_PROCESS_CONCURRENCY` (51) | `SIRIUS_ASSET_EXPORT__CONCURRENCY` (1..64) |
| `HARUKI_CONCURRENCY_AUTO_TUNE` (54) | The original switch sized workers and stages; set `SIRIUS_ASSET_EXPORT__CPU__AUTO_TUNE` and `SIRIUS_ASSET_EXPORT__STAGE_LIMITS__AUTO_TUNE` |
| `HARUKI_CPU_BUDGET_AUTO` (61) | `SIRIUS_ASSET_EXPORT__CPU__BUDGET_AUTO` |
| `HARUKI_CPU_BUDGET_RATIO` (64) | `SIRIUS_ASSET_EXPORT__CPU__BUDGET_RATIO` |
| `HARUKI_CPU_RESERVED` (68) | `SIRIUS_ASSET_EXPORT__CPU__RESERVED` |
| `HARUKI_CPU_THROTTLE_ENABLED` (71) | `SIRIUS_ASSET_EXPORT__CPU__THROTTLE__ENABLED` |
| `HARUKI_CPU_THROTTLE_SAMPLE_MS` (75) | `SIRIUS_ASSET_EXPORT__CPU__THROTTLE__SAMPLE_MS` (50..60000) |
| `HARUKI_MAX_IN_FLIGHT_BUNDLE_BYTES` (79) | `SIRIUS_ASSET_EXPORT__MAX_IN_FLIGHT_BUNDLE_BYTES` (per export) and/or `SIRIUS_ASSET_SERVICE__MAX_IN_FLIGHT_BUNDLE_BYTES` (shared by jobs) |

The original generic override example `HARUKI__SERVER__PORT` becomes
`SIRIUS_ASSET_SERVICE__LISTEN=0.0.0.0:19091`: `listen` is one socket address.

## Server, authentication and TLS (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:52-112`)

| Original | Class | Sirius |
| --- | --- | --- |
| `server.host`, `server.port` | adapted | Service `listen` (`src/service.rs:41`), required, no default (original `0.0.0.0:8080`) |
| `server.proxy` | adapted | Independent download `network.api_proxy`/`cdn_proxy`, environment-referenced URL/auth (`src/network.rs:53-54`, `src/proxy.rs`); see [NETWORK.md](NETWORK.md). The original also used it for chart-hash Git pushes (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/git_sync.rs:37`), which are not applicable |
| `server.asset_http_version` | reused | Download `network.asset_http_version: auto/http1` (`src/network.rs:41-47`), applied only to the catalog/resource client (`src/proxy.rs:78-87`) |
| `server.auth.enabled` (default false) | not restored | Bearer authentication is always required; disabling it or accepting User-Agent alone is not implemented |
| `server.auth.bearer_token` | adapted | Required service `token_env` (`src/service.rs:46`); the value is never in YAML |
| `server.auth.user_agent_prefix` | adapted | Optional `user_agent_prefix` (`src/service.rs:48`); in Sirius an extra filter after Bearer, in the original an alternative credential (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/validate.rs:59-79`) |
| `server.tls.enabled/cert_file/key_file` | reused | Optional `tls.certificate_file/private_key_file/handshake_timeout_ms` (`src/server.rs:18-23`); presence enables it. See [LISTENER_TLS.md](LISTENER_TLS.md) |

The original maps `server.asset_http_version` into its pipeline client in
`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/runner.rs`. Tests verify protocol versions observed by both TLS
server and client, Basic origin authentication, and rejection of redirects and untrusted
certificates. Developer-feature-unified tests alone do not prove production HTTP/2 support.

## Logging (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:124-157`)

| Original | Class | Sirius |
| --- | --- | --- |
| `logging.level` (default `INFO`, case-insensitive, `warning` alias, EnvFilter directives: `Haruki-Sekai-Asset-Updater@3d33ed03:src/service/logging.rs:138-156`) | adapted | `logging.level` accepts the six lowercase levels and `default[,target=level...]` (`src/application_log.rs:56-104`). Targets outside `sirius_asset_updater` (e.g. `hyper=warn`) are rejected because dependency events are never emitted. See [APPLICATION_LOG.md](APPLICATION_LOG.md) |
| `logging.format: pretty/json` | adapted | `format: text/json` |
| `logging.file` (in addition to stdout) | adapted | One sink: `output: stderr/stdout/file` with rotation and retention; default stderr |
| `logging.access.enabled` (default true) | adapted | Presence of service `access_log` (`src/service.rs:45`); default off |
| `logging.access.format` (`${time}` template) | reused | `format: template` plus `template` (`src/access_log.rs:66-73`, `:199-228`). Differences: `${path}` is the route template, `${time}` is UTC RFC 3339, unknown placeholders fail. See [ACCESS_LOG.md](ACCESS_LOG.md) |
| `logging.access.file` | reused | `access_log.output: {type: file, ...}` |

## Execution and retry (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:341-397`)

| Original | Class | Sirius |
| --- | --- | --- |
| `execution.timeout_seconds` (300) | reused | Service `timeout_seconds` (`src/service.rs:68`, default 3600) |
| `execution.allow_cancel` | reused | Service `allow_cancel` (`src/service.rs:36`, default true). Disabled user cancellation returns 409 without mutation; shutdown/deadline cancellation stays active |
| `execution.asset_bundle_cache_dir` | adapted | Download `assets.cache_directory` (`src/assets.rs:17`), a verified per-resource cache; export has a separate decoded cache ([EXPORT_CACHE.md](EXPORT_CACHE.md)) |
| `execution.max_in_flight_bundle_bytes` | not applicable | Parsed but never read by the original: only `resources.memory.max_in_flight_bundle_bytes` is used (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/runner.rs:77`, `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/pipeline.rs:43`). Not copied |
| `execution.batch_save_size` | adapted | Per-entry durable cache commits; see [Download checkpoint adaptation](#download-checkpoint-adaptation) |
| `execution.max_concurrent_jobs` (4; `0` = unlimited, `Haruki-Sekai-Asset-Updater@3d33ed03:src/service/jobs/manager.rs:36-39`) | adapted | Service `max_concurrent_jobs` 1..64 (`src/service.rs:52`); at most one job per region; bounded queue `max_queued_jobs` |
| `execution.retain_terminal_jobs` | reused | Service `retain_terminal_jobs` (`src/service.rs:66`), `0` keeps all (`src/jobs.rs:553`) |
| `execution.retry.*` for catalog/bundle downloads (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/runner.rs:157-160`) | adapted | `network.snapshot_retry`/`catalog_retry`/`asset_retry {attempts, delay_ms, max_delay_ms}` (`src/network.rs:7-37`), attempts 1..8 |
| `execution.retry.*` for uploads (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/storage.rs:267-294`) | adapted | Storage `attempts`/`retry_delay_ms` (`src/storage.rs:34-38`) |
| `execution.retry.*` for FFmpeg commands (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/media.rs:59`, `:171`, `:247`, `:335`, `:504-555`) | adapted | Export `media_retry {attempts, delay_ms, max_delay_ms}` (`src/export_options.rs` `MediaRetry`, `src/export.rs` `ffmpeg_deadline`): same spawn-kind/stderr-marker transient classification, attempts 1..8 default 1 (no retry; original 4), delays 0..60000 ms without jitter (`initial_backoff_ms`→`delay_ms`, `max_backoff_ms`→`max_delay_ms`). Applies to every FFmpeg child including remux and verification decodes; never after cancellation or past `media_timeout_seconds`; partial outputs removed first; output verification and FFI are not retried (original FFI `Media` errors were non-retryable). See [EXPORT_OPTIONS.md](EXPORT_OPTIONS.md#media-process-retry) |
| `execution.retry.*` for chart-hash Git (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/git_sync.rs:286`) | not applicable | See Git sync |

## Backends (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:162-190`)

| Original | Class | Sirius |
| --- | --- | --- |
| `asset_studio.read_batch_size` | not restored | See [Unity read batching](#unity-read-batching) |
| `asset_studio.image_format` | not applicable | Single legal value `raw_rgba` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/validate.rs:187-196`) |
| `asset_studio.read_kinds` (type name → kind) | adapted | Export `read_kinds: {default, classes}` keyed by Unity class ID (`src/read_policy.rs:44-60`); see [Unity object representation](#unity-object-representation-and-animator) |
| `media.backend` (default `ffi`) | reused | Export `media_backend` (`src/export.rs:44`, `src/media_backend.rs:6-11`), default `cli` |
| `media.ffmpeg_path` | reused | Export `ffmpeg` (`src/export.rs:99`), required unless raw-only |
| `image.backend` | not applicable | Single-valued enum `rust` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:194-197`) |
| `image.png_compression` | adapted | Per-rendition `{format: png, compression}` (`src/export_options.rs:86-90`) |
| `image.webp_lossless` | not applicable | Ignored by the original encoder; see [WebP](#webp-lossless) |
| `image.jpeg_quality` (95) | adapted | Per-rendition `{format: jpeg, quality, background}` (`src/export_options.rs:94-97`); both required |

## Concurrency and resources (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:444-526`)

| Original | Class | Sirius |
| --- | --- | --- |
| `concurrency.auto_tune` | adapted | Export `cpu.auto_tune` (workers, `src/cpu_policy.rs:8`) and `stage_limits.auto_tune` (`src/stage_limits.rs:14`). Sirius stage values stay upper bounds; the original treated them as floors and replaced `video_encode` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/tuning.rs:56-100`) |
| `concurrency.download` (32) | adapted | Download `assets.concurrency` (default 4, 1..16) and service `max_downloads` (default 4, 1..64) |
| `concurrency.upload` (4) | adapted | Storage `concurrency` (default 4, 1..32) and service `max_uploads` (default 4, 1..32) |
| `concurrency.post_process` (16; `0` = auto, `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/tuning.rs:69`) | adapted | Export `concurrency` (default 4, 1..64); use `cpu.auto_tune` instead of `0` |
| `concurrency.acb/usm/hca/images` (12/6/16/12) | adapted | `stage_limits.acb/usm/hca/image` (`src/stage_limits.rs:15-18`), default no cap |
| `concurrency.media_encode` (12, legacy aggregate) | adapted | `stage_limits.audio_encode` + `video_encode`; per-export `media_concurrency` (default 2, 1..4) and service `max_media_processes` (default 4, 1..16) gate all media operations |
| `concurrency.audio_encode/video_encode` (12/4) | adapted | `stage_limits.audio_encode/video_encode` (`src/stage_limits.rs:19-20`), default no cap |
| `resources.cpu.budget_auto/budget_ratio/reserved` | reused | Export `cpu.*` (`src/cpu_policy.rs:7-14`), same formula and ratio bound |
| `resources.cpu.throttle.enabled/sample_ms` | reused | Export `cpu.throttle` (`src/cpu_throttle.rs:12-30`); enabling is rejected on non-Unix; sample 50..60000 ms |
| `resources.memory.max_in_flight_bundle_bytes` | reused | Export and service `max_in_flight_bundle_bytes` (`src/export.rs:75`, `src/service.rs:62`); `0` disables |

Sirius additions without an original field: `cpu.limit_stages`, service `max_cpu_stages`,
`stage_limits.wait_timeout_seconds`, `media_timeout_seconds`. See [EXPORT.md](EXPORT.md) and
[EXPORT_OPTIONS.md](EXPORT_OPTIONS.md).

## Storage (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:529-552`)

The original compiled OpenDAL `fs` and `s3`; Sirius has `local` and `s3` backends
(`src/storage.rs:71-103`). The option-by-option OpenDAL map is in
[STORAGE.md](STORAGE.md#remaining-opendal-option-audit).

| Original | Class | Sirius |
| --- | --- | --- |
| `name` | reused | `providers[].name`, required |
| `scheme` / `kind` | adapted | `backend.type: local/s3` |
| `root`, `prefix` (legacy) | adapted | `prefix` (region/publication suffix is always added) or local `directory` |
| `public_base_url` | reused | Same, with region template ([STORAGE.md](STORAGE.md)) |
| `options` (scalar map) | adapted | Typed fields and `write_options`; unknown keys fail |
| `endpoint` + `tls` | adapted | Full `endpoint` URL |
| `bucket`, `region`, `path_style` | reused | Same names |
| `public_read` | reused | S3 `public_read`, plus per-provider include/exclude |
| `access_key` / `secret_key` | adapted | `access_key_id_env` / `secret_access_key_env` / `session_token_env`, `credentials_file`, `assume_role` |

## Regions (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:634-946`)

| Original | Class | Sirius |
| --- | --- | --- |
| `regions.<name>` map, `enabled` | adapted | Fixed `Region` enum (`src/region.rs:5`), `cn` rejected (`src/service.rs:167`); a service profile per enabled region |
| `provider.kind: colorful_palette/nuverse` and URL templates, `profile`, `profile_hashes`, `asset_version_url`, `app_version` | not applicable | Sekai CDN URL schemes (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/regions.rs:25-120`). Sirius discovers versions from its Game API snapshot (`game_api_root`, `environment`, `client_version`, `protocol_version`, `platform`: `src/lib.rs:116-131`) and authenticated `cdn_roots` (`src/lib.rs:133`) |
| `provider.required_cookies` / `cookie_bootstrap_url` | not applicable | Sekai cookie bootstrap (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/provider.rs:22-57`). Sirius CDN uses Basic credentials from `cdn_roots.*.username_env/credential_env`, or none for Global roots with `authorization: none` (`CdnAuth`, `src/lib.rs`) |
| `crypto.aes_key_hex/aes_iv_hex` | not applicable | Decrypt Sekai's AES-CBC asset manifest (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/provider.rs:94-111`). Sirius bundle-prefix decryption is `assets.decrypt.{key_hex_env, nonce_seed_hex_env}` (`src/assets.rs:35-39`); CRI keys are export `cri_key_env`/`split_acb_xor_env` |
| `runtime.unity_version` | not applicable | See [X-Unity-Version](#x-unity-version) |
| `paths.asset_save_dir` | adapted | Download `output` (`src/lib.rs:132`); replaced by the service per job |
| `paths.downloaded_asset_record_file` | adapted | Receipts and the verified per-resource cache |
| `filters.start_app` / `on_demand` | not applicable / adapted | Category split follows Sekai `AssetCategory::StartApp/OnDemand/LivePv` (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/planning.rs:25-41`). Include patterns map to `assets.selection.include` and native `keys` (`src/catalog.rs:30-39`; [SELECTION.md](SELECTION.md)); no fabricated category mapping |
| `filters.skip` | adapted | `assets.selection.exclude` (roots only; dependencies of selected roots are still downloaded) |
| `filters.priority` | reused | `assets.selection.priority` |
| `export.by_category` | not applicable | Splits output by Sekai `resources/{startapp,ondemand}` paths (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export.rs:115-143`) |
| `export.asset_studio_types` (default `all`) | adapted | `selection.providers/unity_class_ids/embedded_audio` (`src/export_options.rs:12-16`); empty means all |
| `export.raw_bundles.{output_dir, include, exclude}` | adapted | Export `raw_bundles` (`src/raw_bundles.rs:13-18`); see [Raw bundles](#raw-bundles) |
| `export.haruki_3d.*` (18 fields, `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:772-791`) | not applicable | Sekai 3D model/FBX exporter, `role_character3d_ids`, master data; removed by policy |
| `export.usm.export/decode`, `export.acb.export/decode` | adapted | `selection.providers`, `selection.embedded_audio`, `cri.acb/usm: decode/preserve` (`src/export_options.rs:413-416`); see [CRI stage evidence](#cri-stage-evidence) |
| `export.hca.decode` | not restored | Destructive in the original; see [CRI stage evidence](#cri-stage-evidence) |
| `export.images.formats` (default `[png]`) | adapted | `image:` single rendition or list of 1..5 (`src/export_options.rs:155-204`); `jpg` becomes `{format: jpeg, quality, background}`; BMP/TGA added |
| `export.video.formats` (default `[mp4]`) | adapted | `video: source/mkv/mp4/mkv_and_mp4` (`src/export_options.rs:228-234`), default `mkv`; native M2V/IVF always retained |
| `export.video.direct_mp4` | not restored | See [direct_mp4](#videodirect_mp4) |
| `export.audio.formats` (default `[mp3]`) | adapted | `audio:` scalar or list (`src/export_options.rs:218-222`), default `wav`; MP3 without FLAC keeps WAV |
| `upload.enabled` / `providers` | adapted | Service profile `storage_config` (`src/service.rs:92`), or the `publish` command |
| `upload.public_read.include/exclude` | adapted | Per-S3-provider `public_read_include/exclude` (`src/storage.rs:91-93`) |
| `upload.remove_local_after_upload` | reused | Storage `remove_local_after_upload` (`src/storage.rs:40`) |

## Git sync (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/schema.rs:605-630`)

`git_sync.chart_hashes.*` (enabled, repository_dir, username, email, password, sign_commits,
signing_format, signing_key, signing_program) is **not applicable**: it publishes Sekai chart
hashes (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/git_sync.rs`). Sirius has no chart-hash equivalent.

## Internal original settings

- `sekai-asset-pipeline` `PipelineOptions` is built only from `AppConfig`
  (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/pipeline.rs:16`); it has no independent input. Not applicable.
- `sekai-asset-client` hard-coded connect/request timeouts and manifest/bundle size caps
  (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-client/src/options.rs:28-73`) were not user configuration. Sirius exposes
  `network.connect_timeout_ms`/`download_timeout_ms` and `assets.max_file_bytes`/`max_total_bytes`.

## HTTP job submission (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/models.rs:6-22`)

| Original | Class | Sirius |
| --- | --- | --- |
| `region` | reused | Submission `region` (`src/service.rs:824-830`) |
| (region config implied) | adapted | `profile` selects a configured profile; callers never supply paths |
| `mode: update` | reused | `operation: update`; `export`, `verify` and `POST /api/v1/jobs/{id}/retry` added |
| `mode: prefetch_raw_bundles` | adapted | A profile with `download_config` and no `export_config` downloads and verifies only; `raw_bundles.mode: only` publishes bundles |
| `asset_version` / `asset_hash` | not applicable | Fill Colorful Palette URL templates; Nuverse ignores them (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/regions.rs:47-70`, `:98-101`). Sirius takes versions from the verified snapshot; caller-pinned versions are not supported |
| `dry_run` | adapted | Optional `dry_run` returns a synchronous plan and creates no job (`src/service.rs:1021-1029`); the original queued a job that stopped after planning (`Haruki-Sekai-Asset-Updater@3d33ed03:src/service/jobs/runner.rs:128`). See [JOB_SERVICE.md](JOB_SERVICE.md) |

## Recorded decisions

### video.direct_mp4

In the original, `direct_mp4` takes effect only when MP4 is the sole video format; FFmpeg then
reads the USM container itself and writes an MP4, and the source USM is removed
(`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/media_postprocess/usm.rs:170-212`, guard at `:182-187`).
Without it, MP4-only output still extracts M2V in memory and writes only MP4 (`:226-269`). Sirius
deliberately keeps native M2V/IVF: it demuxes USM natively, retains the elementary stream as the
preservation output and checks every container's frame count against USM metadata
([EXPORT_OPTIONS.md](EXPORT_OPTIONS.md#video-output)). There is no MP4-only output shape to
select, so the switch is not exposed; copying it fails as an unknown field.

### X-Unity-Version

The original sends `runtime.unity_version` only as an `X-Unity-Version` request header
(`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-client/src/client.rs:70-73`, set from `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/config/pipeline.rs:69`), which
is part of its Sekai CDN client. The Sirius catalog/resource client sends its own User-Agent,
Basic origin credentials and no Unity header (`src/proxy.rs:56-87`). Production download runs
recorded in [RESTORATION_1_2.md](RESTORATION_1_2.md) used this client. Whether the Sirius CDN
behaves differently when the header is present was not investigated; no field is exposed.

### Paths replaced by the service

Service jobs replace the download document's `output` and the export document's
`input`/`output` with per-job directories under `output_directory/<region>/<job-id>/`
(`src/service.rs:523`, `:590-591`). The fields are still required and validated when the
service starts; the replacement is documented in [JOB_SERVICE.md](JOB_SERVICE.md) and emits
no warning. Profile `input` is used only by `export`/`verify`; `update` ignores it and uses its
own download (`src/service.rs:93`, `:514-546`).

### Raw-only exports

With `raw_bundles.mode: only`, the exporter copies matching Unity bundles and returns before any
decoder runs (`src/export.rs:652-659`). The following settings are still parsed and validated
but have no effect in that mode: `read_kinds`, `cri`, `image`, `audio`, `video`, `media_backend`
(an `ffi` value still requires an FFI-capable build), `stage_limits` caps, `media_concurrency`,
`media_timeout_seconds`, `media_retry`, `split_acb_xor_env`, `selection.unity_class_ids` and
`selection.embedded_audio`. `cri_key_env` and `ffmpeg` may be omitted (`src/export.rs:260`).
`selection.providers`, `paths`, `concurrency`, `cpu` and byte limits still apply. The inert
output-affecting values are recorded in the summary (`src/export.rs:456-470`) and in the
decoded-cache scope (`src/export_cache.rs:77-105`), so changing them invalidates cached entries;
the scheduling-only `stage_limits`, `media_concurrency`, `media_timeout_seconds` and `media_retry`
are in neither. The summary always
reports `full_export: false`. No warning is emitted.

### Media command retry

See the `execution.retry` rows: FFmpeg command retry is restored as opt-in `media_retry` (default one attempt).

## Changed defaults and limits

| Setting | Original | Sirius |
| --- | --- | --- |
| Job authentication | off by default | Bearer always required |
| Listen address | `0.0.0.0:8080` | `listen` required |
| Job timeout | 300 s | 3600 s |
| Concurrent jobs | 4, `0` = unlimited | 4, 1..64; one per region; queue 64 |
| Download concurrency | 32 | 4 per job (1..16), 4 service-wide (1..64) |
| Post-process workers | 16, `0` = auto | 4 (1..64); `0` rejected, use `cpu.auto_tune` |
| Stage caps acb/usm/hca/images | 12/6/16/12 | none unless configured (1..64) |
| Audio/video encode caps | 12/4 | none unless configured; `media_concurrency` 2; service `max_media_processes` 4 |
| Download retry | 4 attempts, 1000-4000 ms | 3 attempts, 250-5000 ms (snapshot 500 ms), 1..8 |
| Upload retry | 4 attempts, 1000-4000 ms | 3 attempts (1..8), 500 ms doubling, capped at 30 s |
| FFmpeg command retry | 4 attempts, 1000-4000 ms, jittered | `media_retry` 1 attempt (1..8), 1000-4000 ms doubling, no jitter |
| Media backend | `ffi` | `cli` |
| Image / video / audio formats | `png` / `mp4` / `mp3` | `png` / `mkv` / `wav` |
| JPEG | `jpg`, global quality 95 | `jpeg` with required `quality` and `background` |
| Log level | `INFO`, case-insensitive, `warning` | lowercase only |
| Log output | stdout, plus optional file | one sink, default stderr |
| Access log | on, stdout | off unless `access_log` is present |
| `asset_http_version` env aliases | accepted | only `auto`/`http1` |
| Environment booleans | `1/0/yes/no/on/off` | YAML `true`/`false` |
| Unknown keys | ignored in most sections | rejected everywhere |

## Evidence details

### Download checkpoint adaptation

Original `execution.batch_save_size` defaults to 50; 0 disables intermediate checkpoints.
`record_completed_bundle` in `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/asset_execution/runner.rs` accumulates successful
records and calls `save_download_record` in `Haruki-Sekai-Asset-Updater@3d33ed03:src/core/download_records.rs`. Each save
serializes the entire bundle-path/hash map and replaces its JSON file. Batching amortizes
that whole-map serialization and write.

Sirius uses independent verified cache entries in `src/cache.rs::Pending`, scoped by catalog
identity and resource identity. Each entry writes data plus a small size/SHA-256 record,
syncs them, then renames staging into place; Unix also syncs directories. Restore verifies
length and content hash before reuse. There is no accumulated whole-catalog cache-index
rewrite for this setting to batch. Metadata write size per completed resource is independent
of the total resource count; copying/hashing the resource itself is still proportional to
its bytes. Export cache persistence is separate in `src/export_cache.rs`.

Keep immediate per-entry checkpointing as this implementation's durability contract. Do not
add an ignored `batch_save_size` alias or claim that buffering all records until the end is
implemented. Full-run filesystem cost and recovery must still be measured on the final
candidate; this architectural mapping is not a performance result.

### Unity read batching

The original native reader in `Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/unity.rs` tunes a chunk
length (image-heavy groups at least 64, MonoBehaviour-heavy groups at most 32), then reads and
writes each object sequentially inside each chunk. The boundary updates batch-count/timing
statistics, not concurrent read admission or a retained payload batch. Sirius likewise processes
objects sequentially within each resource, with explicit image/CPU gates. The field is not
mapped to resource concurrency, and no missing parallel decoder is claimed from it.

### WebP lossless

`backends.image.webp_lossless` had no effect at this baseline: it is forwarded into pipeline
options, but `Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/images.rs` always calls
`WebPEncoder::new_lossless` in `encode_dynamic_image` and `encode_native_rgba_ir`. Sirius always
writes lossless WebP, matching the actual behavior. Original lossy WebP support is not claimed.

### Unity object representation and Animator

`read_kinds.default` plus exact `classes` overrides are a representation policy separate from
object selection. `auto`/`object_raw`/`typetree_json` and the image, TextAsset, font, shader,
OBJ, Texture2DArray, AudioClip, VideoClip and MovieTexture adapters are connected and tested;
summaries, cache identities and full-export claims account for representation changes. The
original `raw` kind's class-specific media/font payload semantics are not relabeled as full object
bytes ([EXPORT_OPTIONS.md](EXPORT_OPTIONS.md)).

Animator: the original native reader (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/unity.rs:277`) has
no Animator dispatch arm. `default_native_read_kind` (`:780`) returns `typetree_json` for
Animator and AnimatorController, and the generic arm (`:464`) falls back to raw object bytes
when type-tree reading fails. The `animator` selector (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/selectors.rs:60`)
only selects those classes. `animator_bundle_fbx` appears in payload naming and manifest
handling (`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/payload/manifest.rs:85`) and tests, with no
producer at this baseline. Conclusion: no FBX animation requirement follows from the selector or
payload label. Sirius `auto` writes type-tree JSON for classes without an adapter
(`src/export.rs:1087-1093`); an operator wanting the original's raw fallback selects
`object_raw` for class 95/91. This does not prove every animation payload decodes, nor parity
with external AssetStudio tools.

### Raw bundles

`regions.*.export.raw_bundles` is restored as export `raw_bundles` with receipt-path
include/exclude regexes, `alongside`/`only` modes and a safe `output_prefix` inside staged output.
The original writes deobfuscated payloads; Sirius copies verified stored/decrypted Unity bundles.
They reach hash journals, cache, offline verification and storage publication. An arbitrary
`output_dir` maps to storage placement, not untracked writes outside receipts. Raw-only cannot
claim `full_export`; see [Raw-only exports](#raw-only-exports) and
[EXPORT_OPTIONS.md](EXPORT_OPTIONS.md#raw-unity-bundles).

### CRI stage evidence

Original `region.export.acb.export/decode` gates `handle_acb_files` in
`Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/media_postprocess/acb.rs`; USM flags similarly gate
`handle_usm_files` in `Haruki-Sekai-Asset-Updater@3d33ed03:crates/sekai-asset-pipeline/src/export/media_postprocess.rs`. Sirius `cri.acb/usm: decode|preserve` provides
explicit container retention versus decoding, including its embedded ACB adapter, rather than
silently dropping selected output. Summary, cache and verification retain this distinction.

Original `hca.decode=false` does not preserve raw HCA: in `extract_acb_tracks_from_reader` it
returns the default result after in-memory extraction, without putting tracks in
`generated_files` or `hca_tracks`, and the batched caller then removes `source_files`. A false
flag can therefore produce no waveforms and delete the source ACB. Sirius does not reproduce this
as a destructive mode; `cri.acb: preserve` keeps full ACBs. It is not raw-waveform parity.

## Original-side silent ignores not reproduced

- `execution.max_in_flight_bundle_bytes`: parsed, never read.
- `backends.image.webp_lossless: false`: ignored by the encoder.
- Unknown keys outside `images`/`video`/`audio`: dropped silently.
