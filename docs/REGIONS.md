# Region support in v1.1.0

Region is distinct from deployment environment and UI language. Run one proxy instance per
region, with separate configuration, tokens, game credentials, session locks and storage.
Changing region requires a restart; hot reload changes compatible protobuf definitions within
one protocol family and cannot switch regions or account identity.

| Region | Game selection | Area ID | Default platform | Protocol family | Current capability |
| --- | --- | --- | --- | --- | --- |
| `jp` | Japan | Not inferred | `iOS` | JP 1.0.3 | Existing JP proxy and verified download/export pipeline |
| `tw` | TW/HK/MO | 2 | `Android` | Global 1.0.1 | Server discovery and anonymous version query |
| `en` | EN Region | 3 | `Android` | Global 1.0.1 | Server discovery and anonymous version query |
| `kr` | Korea | 4 | `Android` | Global 1.0.1 | Server discovery and anonymous version query |
| `cn` | Reserved | Unknown | Not operational | Not supplied | Configuration is recognized but startup/check rejects it |

`global` is not a region. TW, EN and KR have distinct API roots and Master versions. EN and KR
may share CDN hosts but use distinct base paths. Known production endpoints and CDN paths
that belong to another region are rejected; custom deployment origins remain configurable.
The `|`-separated entries returned by discovery are alternate URLs: select exactly one URL,
never paste the whole list into an endpoint. No automatic endpoint switching is performed.

## API proxy

Omitting `region` preserves JP behavior. `platform` accepts exactly `iOS` or `Android`; when
omitted it follows the table. A Global instance selects `protocol/global/1.0.1` by default;
an explicit protocol path must have the matching family. The existing JP default path remains
`protocol/sirius/1.0.3`. Both bundles generate native prost/pbjson codecs at build time. Exact
fingerprints select native codecs; compatible changed definitions use dynamic startup/reload.

`GET /api/v1/regions` reports the selected region and capability/reservation metadata.
`GET /api/v1/servers` calls the anonymous Global server-list RPC; it is not supported by JP.
`GET /api/v1/system` includes region, platform, protocol family, supported RPCs and observed
Master/resource versions. All three endpoints require the API token.

Global player/profile/ranking/announcement/account operations return HTTP 501 without sending
a request. The Global bundle deliberately includes only the two verified RPCs and their
necessary types. JP authentication, account registration and Master decryption are not assumed
to work on Global; no SDK registration/login implementation is included in this release.
Global automatic Master storage is rejected until that pipeline is verified.

## Asset updater

Configure the same region, platform, client version and protocol version as the proxy.
`protocol_version` defaults to 1.0.3 for JP and 1.0.1 for TW/EN/KR; it can be pinned explicitly
when deploying a new verified bundle. `cdn_roots` matches the entire HTTPS base URL, including
its path. Username/password environment references belong only to that configured base URL.
Redirects remain disabled and unknown roots/references are rejected before CDN requests.

New snapshots use schema version 2 and require explicit region identity. A regionless schema-1
snapshot is accepted only for legacy JP/iOS/protocol-1.0.3. A mismatched or missing schema-2
region, platform, environment, client or protocol version fails before any CDN request.
Publication names contain region and platform; receipts and export summaries retain identity.
Cache keys additionally include region, environment and platform, even for shared CDN URLs.
Old ciphertext caches are not deleted but use a different namespace and may be downloaded again.

Global transport supports HTTPS CDN prefixes and Android paths, but end-to-end Global asset
acquisition/decryption has not been verified. A successful version query does not imply a
ready resource snapshot: `x-asset-version: unknown` or a missing Android hash produces no ready
snapshot. The updater refuses to manufacture hashes or substitute an iOS/JP snapshot. Keep
separate output, cache and export directories for each region. Real credentials and keys are
never shipped; do not reuse JP credentials for Global.

## Upgrade from v1.0.0

Upgrade the paired proxy and updater together: the old updater cannot consume schema-2
snapshots. Existing JP configuration remains valid with omitted region/platform. Existing
schema-1 JP receipts remain readable for offline verification/export. Export summary schema 2
adds region and platform. Keep published outputs immutable; directory names are opaque and
must not be parsed as the previous `catalog-UUID` naming convention.

`cn` has no default API/CDN, invented area ID, copied Global protocol or fallback to JP.
Enabling it later requires verified endpoints, login/protobuf contracts and resource behavior.
