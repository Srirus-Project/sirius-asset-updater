# Incremental decoded-resource export

Set these optional fields in the export configuration used by the CLI or a job profile:

```yaml
retain_outputs: true
cache_directory: ./cache/decoded
cache_revision: ""
cache_max_bytes: 53687091200 # optional 50 GiB of managed entries
cache_max_entries: 20000    # optional retained resource count
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
read as entries and are removed on the next owned cache open.

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
backend. Removing an idle cache only causes future decoding work.

## Capacity and eviction

Size the byte limit to hold at least one complete export when full re-exports should reuse the
cache. Eviction is least-recently-used, and a full re-export visits resources in the same order
as the run that filled the cache, so a smaller budget evicts every entry before it is needed:
on the JP catalog (~35 GiB decoded output) an 8 GiB limit produced no hits, while 48 GiB
produced 13,367 of 13,367 during 1.2.0 acceptance.

`cache_max_bytes` and `cache_max_entries` are optional. Omitted limits preserve unbounded
legacy capacity; configured byte capacity must be positive and entry capacity accepts
1..1000000. Limits require cache_directory and do not change resource identity. Bytes count
logical file sizes, including entry metadata, rather than filesystem allocation or compression.

After acquiring the exclusive process lock, opening a cache scans managed digest entries,
removes abandoned `.pending-export-*` paths, and evicts least-recently-used entries until the
limits hold. Last use is recorded through entry.json modification time; insertions and verified
hits update ordering, and equal timestamps use digest order. Do not externally modify this
private directory. Unknown non-cache names are left untouched and excluded from the budget;
this is not a general disk cleanup command.

Before insertion, old entries are evicted to make room. A single entry larger than the byte
limit is not cached: its verified export remains successful. Copies, insertion and eviction
share an in-process lock so an entry cannot disappear during a hit. Decoder workers still run
concurrently, but cache copies are serialized within one cache. Symlinks are never traversed
while counting or deleting managed entries. Publication/output copies are independent and
survive eviction. Cancellation is checked while scanning/copying and between evictions; OS
filesystem operations are not preemptible.

The capacity bound covers committed managed entries. Atomic replacement can temporarily use
one additional entry's staging space; exports, download caches, lock files, unknown paths,
and filesystem directory overhead are separate. Reserve disk headroom accordingly. An IO
failure while pruning/copying fails the resource/job rather than pretending limits were met.
The yhm01 full and incremental production acceptance gates remain separate from
the local HTTP service/restart and synthetic audio tests.
