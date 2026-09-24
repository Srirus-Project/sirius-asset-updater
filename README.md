# Sirius Asset Updater

A Rust resource downloader and exporter for **BanG Dream! Our Notes**. Derived from
[Haruki-Sekai-Asset-Updater](https://github.com/Team-Haruki/Haruki-Sekai-Asset-Updater),
with game-specific providers and a standalone implementation. Haruki's MIT attribution
is retained in [LICENSE](LICENSE); see [sources](docs/SOURCES.md). This is an unofficial project.

## Features

- Consume version-pinned snapshots from Sirius API Proxy; validate freshness, identity and CDN scope.
- Download Addressables catalogs and optional remote assets with bounded concurrency and retries.
- Decode encrypted Unity bundle prefixes; preserve plain bundles and CRI containers.
- Reuse verified ciphertext caches and publish complete downloads atomically with SHA-256 receipts.
- Export Unity objects, sprites, Live2D MOC3, inline/SplitAcb audio and USM color/alpha video offline.
- Validate decoded outputs, exact ADX sample counts and complete video frame counts.

Application version **1.0.0** is independent of the JP iOS 1.0.3 game/protocol baseline.
This is a one-shot CLI. Use an external scheduler for recurring downloads; it does not manage game accounts.

## Quick start

Extract a release archive, or build with Rust 1.96 or later:

```sh
cargo build --release --locked
cp sirius-asset-config.example.yaml sirius-asset-config.yaml
# Supply the secrets referenced by your configuration.
./target/release/sirius-asset-updater check
./target/release/sirius-asset-updater probe
./target/release/sirius-asset-updater
# Release archive: ./sirius-asset-updater (sirius-asset-updater.exe on Windows)
```

`SIRIUS_ASSET_CONFIG_PATH` overrides the configuration path. `check` validates configuration
and secret presence offline. `probe` refreshes/reads the API snapshot without contacting the CDN.
API, internal and CDN credentials have separate scopes; API and internal tokens must differ.
`refresh_token_env` lets the updater renew version observations before and during a download.

The default example downloads only the catalog. Enable `assets` to download all remote resources.
Set `assets.decrypt` to decrypt bundle prefixes using a 16-byte key and 8-byte nonce seed,
provided as hexadecimal environment variables. No real game keys are distributed.
Concurrency defaults to 4 (range 1–16); per-file and total byte limits bound downloads.
Cache and published resources occupy separate space, and old cache versions are not removed automatically.

Each published directory contains `catalog_main.bin`, `receipt.json`, a resource plan and,
when enabled, an `assets/` tree. `verify` checks files, stored hashes, provider identities and
the catalog dependency graph. Hashes attest to local receipt integrity, not a server signature.
Embedded RuntimePath resources are recorded but are not downloaded from the CDN.

```sh
./sirius-asset-updater verify ./downloads/catalogs/PUBLISHED_DIRECTORY
./sirius-asset-updater inspect-catalog /path/to/catalog_main.bin
```

Unknown providers, unsafe paths, mismatched versions, stale observations and unknown credentials
fail closed. CDN requests do not follow redirects. Transient failures have bounded retries;
401/403 and validation failures are terminal. Failed/cancelled updates preserve previous publications.
Verified complete cache entries survive interruption; partial files restart from byte zero.

## Offline export

Install **FFmpeg** and configure its executable path in `export-config.yaml`.
FFmpeg is required for export, including video validation and ADX decoding; it is not required
for catalog/download/verify commands. The Docker image includes it. On macOS use `brew install ffmpeg`;
on Debian/Ubuntu use `apt install ffmpeg`; on Windows install a trusted FFmpeg build and set its full path.
Release archives do not redistribute FFmpeg. Unity/CRI parsing uses native Rust libraries;
the application does not require Python or .NET.

```sh
cp export-config.example.yaml export-config.yaml
# Set input to a verified, decrypted download and supply SIRIUS_CRI_KEY.
# Supply SIRIUS_SPLIT_ACB_XOR for scrambled SplitAcb resources.
./sirius-asset-updater export export-config.yaml
```

`retain_outputs: true` keeps all successful outputs. `false` still generates and validates
every output but deletes each resource's temporary products after recording its hashes.
The summary is complete only when every selected resource succeeds. Unknown paths fail;
a selected subset does not constitute a full-catalog acceptance test.
See [export formats and limits](docs/EXPORT.md).

The current JP catalog passed full download and export validation: **13,367 remote resources,
zero failures**, 896,263 Unity objects and 952,604 outputs (34.59 GB cumulatively).
This includes 9,259 PNGs, 239 MOC3 models, 27,700 HCA WAVs, 39 ADX WAVs and 222 video tracks
from 201 USM files. Validation used per-resource cleanup with representative outputs retained.
This does not cover the 28 embedded locations, Unity runtime rendering, complete scene reconstruction
or future game formats. Alpha and color tracks are exported separately, not composited.

## Development and release

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
SIRIUS_TEST_FFMPEG=/path/to/ffmpeg cargo test --locked real_ffmpeg_decodes_short_final_adx_packet_without_losing_samples -- --ignored
SIRIUS_CATALOG_SAMPLE=/path/to/catalog.bin cargo test --locked real_catalog_fixture -- --ignored
```

Tests use synthetic fixtures; optional tests require local tools or a privately held catalog.
Exit code 0 means success, 1 means failure, and 130 means cancellation.
See [deployment checks](docs/DEPLOYMENT_CHECKS.md), [architecture](docs/ARCHITECTURE.md)
and [release preparation](docs/RELEASING.md). Repository visibility and workflow activation
are separate operations from release preparation.
