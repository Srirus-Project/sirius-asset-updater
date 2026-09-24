# Development rules

This is a standalone Sirius project for BanG Dream! Our Notes. Do not reintroduce Sekai game protocols,
CP/Nuverse providers, Sekai account databases or Ent schemas. Generic service persistence and Master publication/ingestion
may be restored for Sirius; do not restore game-specific table models.
Reuse Haruki concepts only where they fit the actual Sirius protocol.
Keep Haruki MIT attribution and Sirius attribution in LICENSE; sources are in docs/SOURCES.md.

- Rust implementation lives in `src/`; restore reusable service capabilities without importing Sekai adapters.
- Never commit tokens, credentials, local config, game binaries or downloaded resources.
- Test locally; no live game availability is required for tests.
- Run `cargo fmt --all -- --check`, `cargo check --locked --all-targets`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`, and `cargo test --locked`.
- The user authorized restoration, yhm01 full acceptance and release 1.2.0. Publish only after the restoration ledger and full acceptance gates pass. Preserve existing Actions settings; CI/Release/Docker target this project's binaries.
- Commit subjects use `[Feat]`, `[Fix]`, `[Chore]` or `[Docs]` and an imperative description.
- Include `Co-authored-by: Codex <noreply@openai.com>` in Codex commit bodies.

JSON uses sonic-rs; YAML uses yaml_serde. No serde_json/serde_yaml dependency.
Validate snapshot identity, freshness and credential scope before contacting a CDN.
Do not follow redirects with credentials; publish catalog plus receipt atomically.
The sample test requires SIRIUS_CATALOG_SAMPLE and is ignored by default.
Unity/CRI export uses native Rust libraries (unity-rs-core/cridecoder) and FFmpeg.
Do not add a Python or .NET runtime dependency to the application. Python release
packaging and smoke-test scripts are development tooling only.

Keep private research links, full binary extracts and actual decryption keys outside this repository.
Synthetic vectors must document their origin and must not contain game asset data.

Write README.md, AGENTS.md, release notes and user-facing documentation in English.
Use Sirius names and SIRIUS_* environment variables. The game is BanG Dream! Our Notes.
Keep explicit derived-from links to the appropriate Haruki repository in README.md.
Release archives must include runtime files, example configuration, documentation and licenses.
Keep repository visibility, release publication and workflow activation explicit operations.

Region is independent of environment. Preserve legacy JP defaults, explicit region identity in new snapshots,
region-scoped caches and the reserved (non-operational) CN boundary. Never silently use JP schemas or
credentials for Global. Keep the capability matrix in docs/REGIONS.md accurate.
