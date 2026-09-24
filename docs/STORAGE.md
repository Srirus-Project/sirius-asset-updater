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
by provider count. Service final progress uses these counts. Upload progress currently reports
the phase followed by final totals, rather than per-object counters.

Each invocation uses a new UUID; previous publications are never replaced. There is no mutable
`latest` pointer. Different stores cannot commit atomically: if a later completion-marker write
fails, earlier stores may already expose a complete publication, but local outputs remain.
A job retry creates a fresh publication; it does not reuse a failed remote prefix. Failed or
cancelled attempts can leave incomplete prefixes, and failed/broken multipart aborts can leave
remote uploads. Consumers ignore these; operators can configure bucket lifecycle cleanup.
Do not delete prefixes merely because an active transfer has not written a marker yet.

## Limits and failure behavior

- `concurrency`: 1–32, default 4; upload buffering is approximately 8 MiB per in-flight writer,
  plus one 1 MiB read-back chunk and transport overhead. Multiple jobs multiply this budget.
- `attempts`: 1–8, default 3, for each object/marker. Only temporary backend errors or timeouts
  retry. Permission failures, integrity failures and redirects fail without automatic retry.
- `retry_delay_ms`: 1–10000, default 500; exponential backoff capped at 30 seconds.
- `object_timeout_seconds`: 1–3600, default 300. Writer creation and upload-plus-read-back each
  have this bound. S3 HTTP requests additionally have a 10-second connect and 60-second request
  timeout. Retries receive a new attempt deadline; the service's overall job deadline still applies.
- Multipart uploads use 8 MiB chunks and one part request at a time per object. Cancellation
  drains active uploads and attempts a bounded three-second abort for each active writer.
- Source verification, cancellation or any provider failure before cleanup preserves the local
  export. Once verified cleanup begins it runs to completion without cancellation; a filesystem
  deletion error can leave a partially removed local tree, while all remote copies are complete.

This restores the local/S3 publication path. Publication URLs, mutable registries,
notifications, scheduling and production storage acceptance remain in the restoration audit.
