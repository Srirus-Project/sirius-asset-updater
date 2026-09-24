# Changelog

## 1.1.0

- Add explicit JP/TW/EN/KR identities and reserve CN without enabling unverified networking.
- Separate region, platform and environment; reject mismatched known service endpoints.
- Document capability boundaries and paired-service upgrade requirements in `docs/REGIONS.md`.
- Validate snapshot region/platform/protocol identity before contacting a CDN; retain legacy JP receipt support.
- Support safe CDN base paths and Android catalog paths without dropping region-specific prefixes.
- Include region/platform in publication names and export summaries; scope cache identities by region/environment/platform.
- Keep Global end-to-end asset acquisition gated on valid observed snapshots and separately configured secrets.


## 1.0.0

- Initial public-release candidate for BanG Dream! Our Notes.
- Replace the pre-release Viola codename with Sirius configuration filenames,
  SIRIUS_* environment variables and container user names. Old names are not aliases.
- Retain the appropriate Haruki derived-from attribution and MIT notices.
- Include runtime files, configuration examples, documentation and licenses in release archives.
- Keep real credentials, downloaded content and private research out of public artifacts.

See README.md for supported features, validated scope and current limitations.
