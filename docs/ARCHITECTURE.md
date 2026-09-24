# Asset updater architecture

The CLI reads a validated Game API snapshot, downloads the version-pinned catalog,
and optionally downloads its remote assets. All outputs belong to one private
staging directory, renamed into place only after the complete operation succeeds.

- `src/lib.rs`: configuration, snapshot validation, HTTP clients, catalog and receipt publication.
- `src/catalog.rs`: bounded Addressables binary v2 strings, locations and dependency graph.
- `src/assets.rs`: provider-aware download planning and AES-128-CTR bundle prefix decoding.
- `src/update.rs`: bounded resource downloads, retries, digests and final version revalidation.
- `src/readiness.rs`: offline configuration checks, API token scoped refreshes and snapshot revalidation.
- `src/cache.rs`: version/catalog-scoped raw-file cache, integrity verification and writer lock.
- `src/verify.rs`: offline publication verification, including provider set and dependency graph.
- `src/export.rs`: bounded offline Unity/CRI export, per-resource staging, format validation and journals.
- `src/main.rs`: one-shot updates, preflight/probe/verification and Ctrl-C cancellation.
- `src/tests.rs`: synthetic catalogs, independent crypto vector and local HTTP fixtures.

Runtime property `Fwk.Resource.RemoteAssetDir` resolves exclusively to the pinned
version directory. Absolute URLs must have the same exact directory prefix.
Unknown providers and unsafe paths fail the update before resource requests.
Embedded RuntimePath locations are recorded but not downloaded; a CDN updater
cannot recover files that ship only inside an installation package.

Encrypted bundles, plain Unity bundles and CRI resources are distinct provider
classes. Only the encrypted provider uses the optional bundle key. The nonce uses
the original basename, including any hash: SHA256(seed || UTF8(basename))[:8].
AES-128-CTR uses this nonce followed by a big-endian 64-bit counter starting at 0.
Only the first min(16384, size) bytes change. Plain Unity bundles and decrypted
bundles must start with UnityFS; this is a header check, not full Unity parsing.
CRI files remain byte-for-byte as downloaded.

Transport failures, HTTP 429 and 5xx get at most three attempts with exponential
backoff. Each retry restarts its unpublished file; there is no HTTP Range append.
401/403, redirects, malformed catalogs, size siriustions and decrypt failures are
terminal. Credentials are origin-scoped and never included in receipts.

After all files are ready, the updater re-reads the snapshot. It must be fresh and
match the original resource version, platform hash, CDN and credential reference.
An optional refresh_token_env uses a separate API-scope bearer token to invoke
Game API /system at startup, between files after 120 seconds, and before publishing.
Without it another caller must keep Game API observations fresh. Maintenance,
refresh failure or identity changes stop the run. Account registration is never performed.

SHA-256 receipts describe received/stored bytes. They are not comparisons against
a trusted remote manifest. Failed/cancelled runs remove their staging directory;
previous runs remain untouched. The optional cache keeps original downloaded bytes,
including ciphertext, across failed/cancelled runs. Its identity includes CDN/version
URL, catalog SHA-256, relative path and provider. Every hit is size/hash checked and
copied into the new staging directory; decryption never modifies cached bytes.
Cache publication waits for provider validation; a writer lock serializes users of
the same cache. Background scheduling is not implemented.
The separate offline `export` command verifies the complete download before parsing it.
It exports Unity objects, reconstructs inline/SplitAcb audio, decodes physical AWB
waveforms, and demuxes USM color/alpha tracks. Video is remuxed and fully decoded
with frame-count verification. Only successful resources are atomically retained;
the overall summary remains incomplete if any selected resource fails. Validation
mode performs the same writes/checks but removes each resource after recording its
output digests. See [the export contract](EXPORT.md) for formats and limits.
