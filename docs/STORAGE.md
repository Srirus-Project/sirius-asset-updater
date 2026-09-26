# Verified export storage (1.2.0 development)

Publish an existing retained export with `sirius-asset-updater publish publish-config.yaml`.
The command takes `input`, the expected `region`, and a `storage` configuration. See
[publish-config.example.yaml](../publish-config.example.yaml). For service jobs, set the profile's
`storage_config` to a file containing the storage configuration directly, as in
[storage-config.example.yaml](../storage-config.example.yaml). It requires `export_config` with
`retain_outputs: true`; validation-only exports cannot be published. Verify-only jobs do not upload.

OpenDAL 0.58.2 provides local filesystem and S3-compatible backends. All configured destinations
are required. Names must be unique and providers are processed in order; per-provider object
concurrency is bounded. Credentials use explicit environment references or the shared-file source below. S3 requires
HTTPS, except literal loopback IPs for local testing. `backend.path_style` defaults to true for
existing configurations (`https://endpoint/bucket/key`). Set it to false for virtual-host-style
requests (`https://bucket.endpoint/key`), including signing against that request target. Virtual-host
mode requires a DNS endpoint and a bucket without dots, matching the backend/TLS constraints. It
disables implicit AWS configuration/metadata credentials and proxy discovery, and never follows
redirects. No credentials, endpoints or signed requests appear in publication receipts or errors.
Local destination roots must not overlap the export tree. Both trees must be owned by the updater
and immutable to other writers; these checks are not a filesystem security boundary. A local
destination stages each object under `<directory>/.staging` before moving it into place; object
keys and publication prefixes are `/`-separated on every platform, including Windows.

## Target preview and public URLs

Run `sirius-asset-updater plan-storage publish-config.yaml` with the same command configuration
as `publish`. It validates storage configuration and required credential environment variables,
but does not read the input export, create provider directories or make storage/network requests.
Configured application logging still applies. JSON output is explicitly marked `preview: true`;
its `example_publication_id` is illustrative, not a reserved ID or a verified publication.
It reports provider names, region-scoped key prefixes and optional public URLs, without backend
credentials, origin endpoints or local directory paths. Reserved CN fails before planning.

Each provider may set `public_base_url`, an HTTPS URL rooted at its bucket/filesystem root as
served by your CDN. It supports a path prefix and rejects userinfo, query strings and fragments.
The publication's provider receipt then includes `public_url`, formed by appending the complete
`PREFIX/REGION/publications/UUID/` key prefix to that base. The trailing slash allows consumers
to append URL-encoded relative object paths (including `complete.json`). Omitted configuration
omits the field. For example, base `https://cdn.example/storage/` with prefix `assets` produces
`https://cdn.example/storage/assets/jp/publications/UUID/`.

This URL is configured routing information, not proof of anonymous availability. No CDN probe
or public-access change is performed; configure matching CDN paths and ACL/bucket policies.
Sirius profiles may use the region templates described below. The region is always added to
the immutable publication key by the updater. The original updater has no publication
registry, `latest` pointer or publication notification: it overwrites exported files at stable
`prefix/relative-path` keys (`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/storage.rs:96-181`,
`:234-245`), and its only other mutable state object is the incremental download record
(`Haruki-Sekai-Asset-Updater@3d33ed03:src/core/download_records.rs:96-130`; see
[HARUKI_CONFIG_AUDIT.md](HARUKI_CONFIG_AUDIT.md)). Its chart-hash Git sync is Sekai-specific and
not applicable. There is therefore nothing to restore here. Sirius adds its own
[completion notifications](JOB_SERVICE.md#completion-notifications) for verified jobs; a
consumer-facing master registry belongs to the API requirements in
[RESTORATION_1_2.md](RESTORATION_1_2.md), not to the updater.

## S3 public-read policy

S3 providers accept `public_read` (default false), `public_read_include` and
`public_read_exclude` (default empty lists). A file receives `x-amz-acl: public-read` when
public_read is true or any include regex matches, unless an exclude regex matches.
A public default ACL also selects all paths, subject to the same exclusions (see below).
Exclusions always win, including over the provider-wide flag. Match paths relative to the
export root, with forward slashes and without bucket/publication prefixes. The same rules
apply to `summary.json`, `resources.jsonl` and `complete.json`; include those explicitly if
anonymous consumers need the publication receipts. Local providers reject these S3-only fields.

Rules are compiled before publishing: each list has at most 128 nonempty patterns, each
at most 4096 bytes with a 1 MiB regex compilation limit. Invalid rules fail configuration.
ACLs are attached to ordinary writes and multipart initiation, not applied after publication.
Denied/unsupported ACLs fail the publication and preserve local output; there is no fallback
that silently removes the requested ACL. All object read-back checks remain authenticated.

With no matching rule and no default_acl, the uploader omits the ACL header. This preserves existing behavior;
it does not override bucket policies or prove anonymous access is denied. Stores with ACLs
disabled should leave public-read off and manage public access through their bucket policy.
Unlike the original Haruki provider-wide flag, Sirius exclusions can override public_read=true;
move original global file patterns into each selected S3 provider's include/exclude lists.

## Publication contract

1. Independently verify the retained export and create a bounded, disk-backed object allowlist.
2. For each provider, upload every declared output plus `summary.json` and `resources.jsonl` to
   `PREFIX/REGION/publications/UUID/`. Recheck source bytes while uploading. Each object is then
   read back from storage and checked by size and SHA-256; an ETag alone is never accepted.
3. After all providers' objects pass, write and read back each `complete.json` marker. It contains
   the verification report, including hashes of both export receipts. Consumers must ignore
   prefixes without a valid completion marker and validate the receipts against it.
4. Only after every marker passes may explicitly enabled `remove_local_after_upload` remove
   the source export directory. It defaults to false. Reverify the exact local tree before
   removal; changed receipts or extra files prevent cleanup. Download inputs and decoded caches
   are never removed. Keep publication output/receipts outside the source directory.

The service writes `publication.json` beside its exports directory. CLI success prints the same
receipt; save stdout outside the export directory if cleanup is enabled. File/byte counts include
both export receipt files, exclude the completion marker, and are per destination, not multiplied
by provider count. Service upload progress instead counts all destination copies: completed/total
objects and verified bytes accumulate across providers. It includes both receipts, excludes markers,
and advances only after authenticated read-back succeeds (not merely after sending bytes).
The bounded in-memory watch snapshot is persisted by the service at most once per second and at
successful completion. Phases identify `publish_upload_N_of_M`, `publish_markers`, optional
`publish_cleanup`, and final `publish`. Object totals can reach 100% before marker/cleanup completion;
only the completed job/outcome indicates a successful publication. Short phases may not appear in
persisted snapshots. Failed/cancelled jobs retain their last persisted progress, not invented totals.
A progress persistence failure cancels and drains active uploads before failing the job.

Each invocation uses a new UUID; previous publications are never replaced. There is no mutable
`latest` pointer. Different stores cannot commit atomically: if a later completion-marker write
fails, earlier stores may already expose a complete publication, but local outputs remain.
A job retry creates a fresh publication; it does not reuse a failed remote prefix. Failed or
cancelled attempts can leave incomplete prefixes, and failed/broken multipart aborts can leave
remote uploads. Consumers ignore these; operators can configure bucket lifecycle cleanup.
Do not delete prefixes merely because an active transfer has not written a marker yet.

## Limits and failure behavior

- `concurrency`: 1–32, default 4; upload buffering is approximately 8 MiB per in-flight writer,
  plus one 1 MiB read-back chunk and transport overhead. Service jobs additionally share
  `max_uploads` (default 4, range 1–32), preventing per-job concurrency from multiplying the total.
- `attempts`: 1–8, default 3, for each object/marker. Only temporary backend errors or timeouts
  retry. Permission failures, integrity failures and redirects fail without automatic retry.
- `retry_delay_ms`: 1–10000, default 500; exponential backoff capped at 30 seconds.
- `object_timeout_seconds`: 1–3600, default 300. One absolute attempt deadline covers shared
  admission, source metadata, writer creation, upload and read-back. Completion-marker admission,
  write and read-back also share one deadline. S3 HTTP requests additionally have a 10-second
  connect and 60-second request timeout. Retries receive a new attempt deadline; the service's
  overall job deadline still applies. A bounded multipart abort may extend cancellation cleanup
  by up to three seconds; the upload permit stays held through that cleanup.
- Multipart uploads use 8 MiB chunks and one part request at a time per object. Cancellation
  drains active uploads and attempts a bounded three-second abort for each active writer.
- Source verification, cancellation or any provider failure before cleanup preserves the local
  export. Once verified cleanup begins it runs to completion without cancellation; a filesystem
  deletion error can leave a partially removed local tree, while all remote copies are complete.

This restores the local/S3 publication path. Mutable registries,
notifications, scheduling and production storage acceptance remain in the restoration audit.

## S3 write policy migration

Haruki's scalar `options.default_storage_class`, `options.server_side_encryption`, and
`options.server_side_encryption_aws_kms_key_id` map to the typed Sirius
`backend.write_options.storage_class`, `server_side_encryption`, and `kms_key_id_env` fields.
The KMS field names an environment variable containing the key identifier or alias; do not
place a key value in the configuration. The map rejects unknown fields. Storage classes must
be uppercase ASCII identifiers (letters, digits, underscore, at most 64 bytes); the destination
validates whether it supports that class. Encryption accepts `AES256` or `aws:kms`; a KMS key
reference requires `aws:kms`. Omitted fields preserve destination bucket defaults.

These policies apply to ordinary writes, multipart initiation, and publication markers,
including public-read uploads. The same verified-readback publication contract still applies:
classes that make new objects unavailable for immediate reads cannot complete publication.
KMS access must permit both writes and verification reads. A failed verification retains local
exports and does not report a completed publication. Tests observe actual signed S3 requests
and complete a multipart publication against a local fixture; cloud bucket/KMS acceptance
remains a deployment test.

This restores specific generic options, not arbitrary OpenDAL option passthrough. Endpoint,
credentials, addressing and ACLs retain their explicit typed fields. Other original scalar options are mapped or ruled out in the option table below; unsupported fields fail instead of being silently
ignored. Supported region-template migration is described below.


### Requester Pays and upload checksums

Haruki `options.enable_request_payer` maps to `backend.request_payer` (default false).
Enable it only for destinations whose request charges should be accepted by the configured
requesting account. It attaches the requester-pays header to uploads, multipart operations
and verification reads, including publication markers. It does not grant access; a denied
request fails publication without removing the policy or deleting local exports.

Haruki `options.checksum_algorithm` maps to `backend.write_options.checksum_algorithm`.
The supported value is `crc32c`; omission preserves the old no-extra-checksum behavior.
The backend calculates and sends checksums for ordinary PUTs and multipart parts, including
receipts and completion markers. This is additional transport validation: the independent
SHA-256 read-back checks remain mandatory before completion or local cleanup.

The locked OpenDAL S3 backend rejects MD5 for multipart initiation. Since retained exports
can require multipart uploads, Sirius rejects `md5` at configuration validation rather than
failing halfway through a large publication. Other algorithms also fail explicitly. Destinations
must support CRC32C multipart/checksum semantics; unsupported/denied requests fail rather than
silently retrying without checksums. Local fixture tests check actual request bodies with an
independent CRC32C implementation; acceptance against the deployed object store remains required.


## Sirius region templates

`{region}` expands to the selected Sirius region (`jp`, `hk`, `en`, `kr`). `{server}` is a
migration alias with exactly the same meaning. It does not refer to environment, platform,
UI language, Sekai region names or S3 signing region. CN remains reserved and cannot be planned
or published. Unknown placeholders and unmatched braces fail configuration.

Templates are supported in provider `prefix`, `public_base_url`, local `backend.directory`,
and S3 `backend.bucket` / `backend.endpoint`. They are expanded before the existing path, URL,
bucket and directory-overlap checks. The same resolution function supplies preview and actual
publication. Configuration validation checks all four operational expansions without accessing
storage. Preview still creates no directories, performs no network requests and reserves no ID.

For example:

```yaml
providers:
  - name: regional-files
    prefix: assets
    public_base_url: https://cdn-{region}.example.invalid/
    backend:
      type: local
      directory: ./published/{region}
```

An EN publication is placed under `./published/en/assets/en/publications/UUID/` and its public
URL starts with `https://cdn-en.example.invalid/assets/en/publications/UUID/`. The mandatory
`REGION/publications/UUID` suffix is always appended, even when `prefix` itself contains a region
template; templates do not disable immutable region isolation or change existing literal paths.

S3 deployments can similarly use `bucket: sirius-{region}`. Credential variable names/values,
S3 `backend.region`, provider names, ACL regexes and encryption options are not interpolated.
Keep separate profiles for independent credentials, signing regions or other policies. Resolution
never silently substitutes credentials from a different region. Public URLs remain declarations
of CDN routing, not a claim that anonymous access was tested.

### Customer-provided S3 encryption keys (SSE-C)

Set `backend.write_options.customer_key_base64_env` to an environment variable containing
exactly 32 bytes encoded as standard Base64. The uploader selects AES256 and derives the
Base64 MD5 key checksum, so separate algorithm/key-MD5 settings are unnecessary. This maps
Haruki's `server_side_encryption_customer_algorithm`, `server_side_encryption_customer_key`
and `server_side_encryption_customer_key_md5` options to one validated reference.

SSE-C cannot be combined with `server_side_encryption` or `kms_key_id_env`. Keys are never
region-templated or included in publication receipts; store them outside the repository and
retain the key needed to read each publication. S3 endpoints require HTTPS except explicit
literal-loopback fixtures. The configured storage service receives the customer key as required
by SSE-C, on both uploads and verification reads. Missing/wrong keys or unsupported encryption
fail publication without a fallback to unencrypted writes. Local test fixtures verify headers,
not server-side cryptography; deployed storage acceptance is still required.

## Remaining OpenDAL option audit

The original scalar option map could configure the backend beyond the operations used by the
updater. Sirius exposes explicit fields and rejects unknown options. This audit distinguishes
implemented mappings from operations the immutable publication pipeline does not perform:

| Original S3 option | Sirius mapping or applicability |
| --- | --- |
| root | Provider `prefix`, with mandatory region/publication suffix; local root uses `backend.directory` |
| bucket / endpoint / region | Explicit backend fields; signing region stays independent of game region |
| access_key_id / secret_access_key / session_token | Environment-referenced credentials, including externally refreshed temporary credentials |
| enable_virtual_host_style | Inverse of `path_style` |
| default_acl | `write_options.default_acl` plus the per-path public-read policy documented below |
| default_storage_class / server_side_encryption / server_side_encryption_aws_kms_key_id | Typed write policy documented above |
| server_side_encryption_customer_* | Validated SSE-C environment reference documented above |
| checksum_algorithm / aws_checksum_algorithm | CRC32C; multipart-incompatible MD5 fails validation |
| enable_request_payer | `request_payer` |
| disable_config_load | Always enabled; storage identities come from explicit references |
| role_arn / external_id / role_session_name / assume_role_duration_seconds | `backend.assume_role`, with environment-referenced external ID and explicit STS region, documented below |
| profile | Explicit `credentials_file.path/profile` selects a shared static-key section; no ambient discovery, process or SSO provider chain |
| assume_role_session_tags | The original scalar-only option parser rejected nested maps; no tag-map migration is claimed |
| enable_versioning | No version-list/get/delete API is used; server bucket versioning continues to operate independently |
| batch_max_operations / delete_max_size | No remote deletion or batch-delete operation is performed; failed prefixes require bucket lifecycle/operator cleanup |
| disable_stat_with_override | Deprecated backend compatibility flag; publication uses authenticated full-byte GET verification |
| disable_write_with_if_match | Deprecated flag; immutable UUID publications do not perform conditional overwrite |
| enable_write_with_append | No append operation: complete objects and multipart parts are written under fresh publication prefixes |
| skip_signature / allow_anonymous | Not exposed: publication and read-back require configured authenticated storage; public-read ACLs do not disable signing |

General AWS SDK provider-chain parity is not claimed: process/SSO/metadata discovery is not
exposed. Production acceptance is not completed by this mapping. Do not pass original options unchanged or assume unsupported keys are ignored.


### Default S3 object ACL

Haruki `options.default_acl` maps to `backend.write_options.default_acl`. Supported object
canned ACLs are `private`, `public-read`, `public-read-write`, `authenticated-read`,
`aws-exec-read`, `bucket-owner-read`, and `bucket-owner-full-control`. Unknown values and
bucket-only `log-delivery-write` fail validation; the uploader does not modify bucket ACLs.
Omission preserves the previous per-path public-read behavior.

ACL precedence for each path (including export receipts and completion markers):

- A public default (`public-read` or `public-read-write`) selects all paths. Matching exclusions
  use explicit `private`; other paths retain the configured public default.
- For non-public defaults, `public_read` or matching includes select `public-read`, unless an
  exclusion matches. Unselected/excluded paths retain the non-public default.
- Without a default, unselected/excluded paths omit the ACL header as before.

Thus public exclusions cannot be bypassed by a public default. Exclusions from public-read do
not remove other grants of a non-public default such as `authenticated-read` or bucket-owner
access, nor do ACLs override external bucket/CDN policy. A configured `public-read-write` is
used explicitly as requested; the uploader never promotes a read-only setting to that ACL.

The policy applies at PUT/multipart initiation, including markers; no separate mutation follows
successful upload. Destinations with ACLs disabled can reject any ACL header, including private.
Backend denials preserve local exports and never trigger a retry with the ACL removed. Tests
verify actual request headers and failure behavior; effective cloud permissions require deployed
bucket acceptance.


## Explicit STS AssumeRole

An S3 backend may configure `assume_role` in addition to its required base credential references:

```yaml
assume_role:
  role_arn: arn:aws:iam::123456789012:role/SiriusPublisher
  region: ap-northeast-1
  session_name: sirius-assets
  duration_seconds: 3600
  # external_id_env: SIRIUS_STORAGE_EXTERNAL_ID
```

The base access/secret key and optional session token authorize only the STS role assumption.
S3 operations then use the returned temporary key and session token. There is no fallback to
base credentials, anonymous requests, metadata credentials or ambient AWS profiles if STS fails.
Role/session/external-ID/partition validation occurs before I/O; duration is 900–43200 seconds
(default 3600), still subject to the role's server-side maximum and role-chaining limits.

`assume_role.region` selects and signs the regional STS request independently of S3 signing
region and Sirius game region. The supported AWS partitions are aws, aws-cn and aws-us-gov;
role ARN and region must agree. Only the matching official regional HTTPS STS authority is
allowed. No production endpoint override is provided. A test-only in-process fixture routes
synthetic requests locally; that override is absent from production builds/configuration.

The custom credential provider uses a dedicated verified-TLS, non-redirecting client with no
ambient proxy. Connect timeout is ten seconds, total HTTP request/body timeout sixty seconds,
and response body is capped at 64 KiB. The existing object attempt/job deadline and cancellation
also cover credential acquisition, so shorter deadlines win. Raw STS responses, role/external
IDs and credentials are not returned in errors or publication receipts. Plan/config validation
does not fetch STS credentials or contact storage.

Credentials are cached in each operator's signer, with serialized concurrent refresh before
expiry (the locked credential implementation refreshes within two minutes). Public/private
operators and separate jobs may have separate caches. Refresh failure fails signing; an expired
credential or base identity is never silently substituted. Environment source values are captured
when constructing the operator; a configured shared-file source instead reloads as described below.
Role credentials can refresh throughout the operator lifetime. Restart/retry constructs new
operators and reads source credentials again.

Local tests cover expiration-aware refresh, concurrent single acquisition, malformed/expired/
oversized/redirect/denied responses, actual S3 temporary-identity publication and a stalled STS
request bounded by a one-second object attempt. They do not prove deployed IAM trust policies
or cloud permissions; those remain part of production storage acceptance.


## Explicit shared credential files

As an alternative to access/secret/session environment references, configure:

```yaml
credentials_file:
  path: /run/secrets/sirius-storage-credentials
  profile: publisher
  refresh_seconds: 60
```

Omit `access_key_id_env`, `secret_access_key_env` and `session_token_env` when using this source;
combining sources fails validation. `profile` defaults to `default`; refresh is 1–3600 seconds,
default 60. The path/profile are literal and are not region-templated. Configure independent
files or profiles for independently authorized storage accounts. No AWS_PROFILE/HOME discovery,
external credential commands, SSO or metadata fallback runs.

The bounded UTF-8 file contains standard static key entries in `[publisher]` or `[profile publisher]`:
`aws_access_key_id`, `aws_secret_access_key`, optional `aws_session_token`, and optional RFC3339
`expiration`. Selected sections must be unique, with no duplicate or unknown fields. Inactive
profiles are not fallback identities. Files are at most 64 KiB, regular and not symlinks; Unix
permissions must have no group/other access (for example 0600 or 0400). Keep the parent directory
trusted and replace files atomically rather than modifying them during reads. These checks do
not isolate the updater from another process with the same operating-system identity.

The source works for direct S3 signing and as the base identity for `assume_role`. Configuration
and preview read/validate the selected credentials locally without contacting STS or storage.
At runtime, signer cache freshness causes rereading after `refresh_seconds`; an explicit
expiration can require earlier rereading and is never extended. Invalid/missing/expired files
fail subsequent acquisition without retaining stale source keys or switching profiles. Fixing
or atomically replacing the file permits the next acquisition to recover. Blocking file reads
run off the async executor; cancellation drops their result but cannot interrupt an individual
operating-system filesystem call.

For AssumeRole, already-issued valid role credentials retain their own lifetime. Source-file
rotation is consumed on the next STS acquisition; it is not immediate revocation of an existing
role session. Environment-backed sources retain their previous operator-lifetime behavior.
Tests verify live source rotation and malformed-file recovery in one cached signer, direct S3
publication and file-backed STS acquisition; deployed credential rotation is still an acceptance gate.
