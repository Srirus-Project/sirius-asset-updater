# Incremental decoded-resource export

Set these optional fields in the export configuration used by the CLI or a job profile:

```yaml
retain_outputs: true
cache_directory: ./cache/decoded
cache_revision: ""
```

The default is no export cache. This directory must be separate from the input and
output trees (neither containing nor contained by them). Use a private directory
separate from the ciphertext download cache. A process lock excludes overlapping
runs using the same decoded cache; a busy cache fails the job instead of silently
running without ownership. Configure separate directories for concurrent profiles.

Each resource is keyed by its verified stored content, provider, source path,
external dependency paths/hashes, region, environment, platform, CDN origin,
client/protocol version, exact type selection, image/audio settings and decoder
secrets. The running updater executable and resolved FFmpeg executable are also
hashed. Only an aggregate digest is persisted; raw secrets are never recorded.
Configured SplitAcb secret references must resolve even if the selected resources
do not eventually use that decoder. Without caching they retain lazy resolution.

Catalog version changes alone do not invalidate unchanged resources. A dependency
change does invalidate its users. Concurrency, output directory and source order
do not affect identity. Replacing either executable invalidates entries. Set
`cache_revision` (up to 256 bytes) to a new deployment-specific value after changing
external shared libraries or decoder runtime settings without changing those
executables. The revision requires a configured cache.

On a hit, the exporter verifies the entry report checksum and every output's size
and SHA-256 while copying into a private staging directory. Symlinks, unsafe paths,
duplicates, malformed metadata and mismatched bytes are misses. A valid copy is
renamed into the current run's resource directory. Outputs are independent copies,
so repairing a cache cannot change prior publications. The current resource output
budget also applies to hits. Cache metadata is limited to 16 MiB per resource.

On a miss, normal decoding and format validation run. Only error-free resource
reports are eligible for cache insertion. Payload copies and metadata are synced
before atomic entry rename. Corrupt entries are replaced under the process lock.
Storage errors fail the resource rather than pretending the cache was written.
Successful entries survive other resource failures, job cancellation and restart;
a retry creates a fresh output tree and can reuse those verified entries. Failed
resources are decoded again. Interrupted `.pending-export-*` directories are never
read as entries and can be removed while the cache is idle.

Cache copies check cancellation between 64 KiB reads. Active staging directories
are removed on normal cancellation; previously completed outputs and cache entries
remain. As with other local file operations, an OS-level blocked write/fsync is not
preemptible. An abrupt process kill can leave staging directories for idle cleanup.

Export summary schema 4 adds `cache_hits`; each resource report adds `cache_hit`.
Hits contribute to the usual output hashes, object counts and completeness checks.
`full_catalog` and `full_export` keep their existing selection semantics. A hit
validates previously decoded output integrity; it does not repeat codec roundtrips.
Consequently caching requires `retain_outputs: true`; validation-only exports must
leave the cache disabled to exercise decoders each time.

The cache is private local state, not a signed manifest or an upload/publication
backend. It consumes additional disk space and currently has no automatic pruning
or total-size eviction. Removing an idle cache only causes future decoding work.
The yhm01 full and incremental production acceptance gates remain separate from
the local HTTP service/restart and synthetic audio tests.
