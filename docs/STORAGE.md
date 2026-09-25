# Verified export storage (1.2.0 development)

Publish an existing retained export with `sirius-asset-updater publish publish-config.yaml`.
The command takes `input`, the expected `region`, and a `storage` configuration. See
[publish-config.example.yaml](../publish-config.example.yaml). For service jobs, set the profile's
`storage_config` to a file containing the storage configuration directly, as in
[storage-config.example.yaml](../storage-config.example.yaml). It requires `export_config` with
`retain_outputs: true`; validation-only exports cannot be published. Verify-only jobs do not upload.

OpenDAL 0.58.2 provides local filesystem and S3-compatible backends. All configured destinations
are required. Names must be unique and providers are processed in order; per-provider object
concurrency is bounded. Credentials are explicit environment-variable references. S3 requires
HTTPS, except literal loopback IPs for local testing. `backend.path_style` defaults to true for
existing configurations (`https://endpoint/bucket/key`). Set it to false for virtual-host-style
requests (`https://bucket.endpoint/key`), including signing against that request target. Virtual-host
mode requires a DNS endpoint and a bucket without dots, matching the backend/TLS constraints. It
disables implicit AWS configuration/metadata credentials and proxy discovery, and never follows
redirects. No credentials, endpoints or signed requests appear in publication receipts or errors.
Local destination roots must not overlap the export tree. Both trees must be owned by the updater
and immutable to other writers; these checks are not a filesystem security boundary.

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
Sirius profiles use explicit provider URLs rather than interpolating Sekai region templates;
the region is always added to the immutable publication key by the updater. Mutable publication
registries and notifications remain separate restoration work.

## S3 public-read policy

S3 providers accept `public_read` (default false), `public_read_include` and
`public_read_exclude` (default empty lists). A file receives `x-amz-acl: public-read` when
public_read is true or any include regex matches, unless an exclude regex matches.
Exclusions always win, including over the provider-wide flag. Match paths relative to the
export root, with forward slashes and without bucket/publication prefixes. The same rules
apply to `summary.json`, `resources.jsonl` and `complete.json`; include those explicitly if
anonymous consumers need the publication receipts. Local providers reject these S3-only fields.

Rules are compiled before publishing: each list has at most 128 nonempty patterns, each
at most 4096 bytes with a 1 MiB regex compilation limit. Invalid rules fail configuration.
ACLs are attached to ordinary writes and multipart initiation, not applied after publication.
Denied/unsupported ACLs fail the publication and preserve local output; there is no fallback
that silently removes the requested ACL. All object read-back checks remain authenticated.

With no matching rule, the uploader omits the ACL header. This preserves existing behavior;
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
credentials, addressing and ACLs retain their explicit typed fields. Other scalar options and
region-template migration remain under audit; unsupported fields must fail instead of being
silently ignored.


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
