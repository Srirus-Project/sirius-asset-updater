# Preparing and publishing v1.0.0

Release preparation does not change repository visibility, enable Actions, create a tag or
publish an image. Keep these operations explicit. The initial public history should contain
only reviewed source, minimal protocol dependencies, synthetic fixtures and documentation.
Never include real configuration, credentials, game assets or private research.

## Local gates

1. Run fmt, check, Clippy with all targets/features, and the complete test suite.
2. Build with `cargo build --release --locked` using Rust 1.96 or later. Ensure Cargo.toml
   and the root package in Cargo.lock both identify 1.0.0.
3. Run `python3 scripts/package-release.py --target macos-arm64` on the matching host;
   use linux-x64, linux-arm64 or windows-x64 on those matching build hosts. The script
   packages an explicit allowlist, not the checkout. Python 3.12+ is release tooling only.
4. Run `python3 scripts/smoke-release.py dist/ARCHIVE`. It extracts into a fresh directory,
   verifies the exact manifest/file hashes and starts the packaged application offline.
   API checks include native protocol loading, version reporting and token isolation.
   Updater checks validate the example configuration with synthetic environment secrets.
5. Build the Dockerfile with `--build-arg VERSION=v1.0.0`. Validate non-root startup,
   mounted configuration and runtime dependencies with `python3 scripts/smoke-container.py IMAGE`.
   The updater export command needs FFmpeg.
6. Audit the tracked tree and every public branch/tag for credentials, private paths and
   unnecessary history. Keep any development-history backup outside the repository.

## Publication gate

The release workflow tests and smoke-tests every advertised host archive before creating a
GitHub Release. A failed target prevents publication. The API archive must include protocol/;
the updater archive includes export configuration and documents the external FFmpeg dependency.
Both include examples, documentation, licenses, a per-file manifest and archive checksums.

After review, explicitly configure Actions and registry permissions, confirm all desired
build targets pass, and create `v1.0.0` at the reviewed commit. The tag must match Cargo.toml.
Repository visibility remains a separate owner decision. No previous pre-release version
or internal development history is required in the public release branch.

The pre-release codename has changed: migrate old configuration names and environment
references to the Sirius examples. No aliases for the previous names are provided.
Application, game client, protocol and resource versions are independent.

Docker pushes to main build and smoke-test only. Registry publication requires a version
tag or an explicit manual publish input, and runs only after the offline container smoke test.
