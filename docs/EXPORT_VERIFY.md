# Retained export verification

Run `sirius-asset-updater verify-export DIRECTORY REGION` with an explicit expected region
(`jp`, `hk`, `en`, or `kr`). CN is reserved. This requires no game server or FFmpeg.
Service jobs automatically verify retained exports after decoding, before reporting completion;
the job directory receives `export-verification.json`. Validation-only exports have no retained
payloads and cannot pass this check.

The verifier accepts schema-4 export receipts, requires a complete successful export with retained
outputs, then streams the resource journal. Every output must be a regular file with the recorded
size and SHA-256. It rejects duplicate resources/paths, unsafe relative paths, symbolic links,
unlisted files/directories, missing files, malformed or truncated records, and inconsistent
resource/object/type/cache-hit/file/byte totals. Receipt hashes are rechecked before success.
A resource may have zero selected outputs; its empty resource directory is still required.

The result includes receipt hashes and verified resource/file/byte totals. `full_catalog` and
`full_export` are scope declarations from the receipt; this command does not reopen the original
catalog or prove source authenticity. A deliberately altered receipt with corresponding altered
files is not an authentic game-data proof. Existing download verification remains separate.

Verification uses 64 KiB payload reads, a maximum 1 MiB summary and 16 MiB journal line,
and at most one million resource records. It holds one resource's output-path set at a time,
and writes a temporary JSON-lines allowlist containing each output plus both receipt files.
The temporary allowlist is removed on failure, cancellation or after the caller releases it.
No payload is uploaded or removed by this command. The allowlist is the input boundary for
[storage publication](STORAGE.md), which separately rechecks upload and read-back bytes before cleanup.

Keep the source directory immutable and owned by the updater throughout verification and
publication. The verifier rejects observed links and detects receipt changes, but is not a
filesystem snapshot or protection against a concurrent malicious filesystem writer.
