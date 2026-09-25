# Haruki configuration migration audit

This is an evidence-based working audit, not a claim of complete configuration parity.
The reference updater commit is `3d33ed037f0ef5009e361e0535b3b19f8c239947`.
Original field definitions are in `src/core/config/schema.rs`; references below use that
repository's paths. Current paths refer to this repository. Private deployment configuration
is not an audit input. Release gates remain in [RESTORATION_1_2.md](RESTORATION_1_2.md).

## Verified transport mapping

| Original setting | Sirius mapping and behavior |
| --- | --- |
| `server.host`, `server.port` | Service `listen`; configured bind address and port |
| `server.proxy` | Independent download `network.api_proxy` and `network.cdn_proxy`, environment-referenced URL/auth; see [NETWORK.md](NETWORK.md) |
| `server.asset_http_version` | Download `network.asset_http_version: auto / http1`; production HTTP/2 support and actual TLS ALPN fixture verification |
| `server.tls.enabled/cert_file/key_file` | Optional service `tls.certificate_file/private_key_file`, validated before bind; see [LISTENER_TLS.md](LISTENER_TLS.md) |
| `server.auth.bearer_token` | Required service `token_env`; secret values are not embedded in YAML |
| `server.auth.enabled`, `server.auth.user_agent_prefix` | No switch to disable job authentication and no User-Agent prefix filter currently. User-Agent is not authentication; configurable prefix filtering remains a genuine behavior difference, not a restored setting. |

The original runner maps `server.asset_http_version` into its asset pipeline client in
`src/core/asset_execution/runner.rs`. Sirius's `src/proxy.rs::cdn_builder` applies the choice
only to its catalog/resource client. Tests verify protocol versions observed by both TLS
server and client, Basic origin authentication, rejection of redirects and untrusted certificates.
Do not use developer-feature-unified tests alone to prove production HTTP/2 dependency support.

## Download checkpoint adaptation

Original `execution.batch_save_size` defaults to 50; 0 disables intermediate checkpoints.
`record_completed_bundle` in `src/core/asset_execution/runner.rs` accumulates successful
records and calls `save_download_record` in `src/core/download_records.rs`. Each save
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
implemented. Full-run filesystem cost and recovery must still be measured on the final yhm01
candidate; this architectural mapping is not a performance result.

## Newly identified outstanding comparisons

The following remain open after reading the original schema against current types. They are
not dismissed as Sekai-specific merely because the current implementation is smaller:

- `execution.allow_cancel`: Sirius exposes cancellation but currently has no operator disable
  switch. Decide and document the service policy or restore the toggle.
- Region `export.images.formats` is restored as `image: [{format: ...}, ...]`, preserving the
  legacy object form. Actual synthetic Texture2D resource exports cover all 31 nonempty sets of
  five supported formats, independent FFmpeg decoding, journal hashes/object identity, aggregate
  output limits and failed-stage nonpublication. PNG effort maps to per-format `compression`,
  JPEG quality/background are explicit. List order and singleton syntax share cache identities.
  Final real-game corpus and deployed storage acceptance still apply.
- `backends.image.webp_lossless` was a nonfunctional option at this baseline: source forwards
  it into pipeline options, but `crates/sekai-asset-pipeline/src/export/images.rs` always calls
  `WebPEncoder::new_lossless` in `encode_dynamic_image` and `encode_native_rgba_ir`; the file
  writer delegates to the same encoder. Sirius lossless WebP preserves that actual behavior.
  Do not copy the ignored boolean or claim original lossy WebP support.
- `backends.asset_studio.read_batch_size/read_kinds`: compare the actual native reader execution
  behavior before mapping to concurrency or class selection. Those are different controls.
- `regions.*.export.raw_bundles`: independently audit filtered raw-bundle publication and paths;
  retaining downloaded inputs alone does not prove equivalent export/publication behavior.
- Complete field-by-field logging, environment override, region-path, export-stage and upload
  migration coverage, including aliases and defaults. Existing specialized docs are evidence,
  not a substitute for this remaining inventory.

Sekai `start_app/on_demand`, Colorful Palette/Nuverse providers, Sekai chart hashes and Haruki
3D role-character IDs require game-specific applicability decisions. Sirius native labels and
transitive dependencies are documented in [SELECTION.md](SELECTION.md); no fabricated category
mapping is accepted. Existing audio/video, storage and CPU restoration are documented separately
and must remain part of final acceptance.
