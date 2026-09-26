# Remote download configuration

The main download configuration (the document used by `check`, `probe` and a plain run, otherwise
read from `SIRIUS_ASSET_CONFIG_PATH`) can be read from the same OpenDAL backends Sirius already
uses for publication. This restores the original `HARUKI_CONFIG_URI` loader, adapted to Sirius.

```sh
SIRIUS_ASSET_CONFIG_URI=opendal://s3/sirius/jp/sirius-asset-config.yaml
SIRIUS_ASSET_CONFIG_SOURCE__S3__ENDPOINT=https://s3.example.com
SIRIUS_ASSET_CONFIG_SOURCE__S3__BUCKET=ops-config
SIRIUS_ASSET_CONFIG_SOURCE__S3__REGION=us-east-1
SIRIUS_ASSET_CONFIG_SOURCE__S3__ACCESS_KEY_ID_ENV=CONFIG_READER_ACCESS_KEY_ID
SIRIUS_ASSET_CONFIG_SOURCE__S3__SECRET_ACCESS_KEY_ENV=CONFIG_READER_SECRET_ACCESS_KEY
CONFIG_READER_ACCESS_KEY_ID=...        # the referenced secret values
CONFIG_READER_SECRET_ACCESS_KEY=...
sirius-asset-updater check
```

```sh
SIRIUS_ASSET_CONFIG_URI=opendal://fs/jp/sirius-asset-config.yaml
SIRIUS_ASSET_CONFIG_SOURCE__FS__ROOT=/srv/sirius-config
```

## Location and precedence

| Variables set | Result |
| --- | --- |
| neither | local `sirius-asset-config.yaml` (unchanged) |
| `SIRIUS_ASSET_CONFIG_PATH` | that local file (unchanged) |
| `SIRIUS_ASSET_CONFIG_URI` | remote object; `SIRIUS_ASSET_CONFIG_SOURCE__*` describes the backend |
| both | configuration error, nothing is read |
| `SIRIUS_ASSET_CONFIG_SOURCE__*` without a URI | configuration error, nothing is read |

A blank or whitespace-only `SIRIUS_ASSET_CONFIG_URI` counts as unset, as in the original. The
original gave the URI silent precedence over `HARUKI_CONFIG_PATH`; Sirius rejects the combination
so a stale variable cannot select a different configuration than the operator expects.

## URI

`opendal://<backend>/<key>` where `<backend>` is `fs` or `s3` and `<key>` is the object path
relative to the backend root (bucket for S3). The URI carries no settings and no credentials.
Rejected: any other scheme or backend, userinfo (`user:pass@`), query or fragment (for example a
presigned `?X-Amz-Signature=`), percent-encoding, backslashes, whitespace, non-ASCII, empty, `.`
or `..` segments, keys over 1024 bytes and URIs over 2048 bytes.

## Bootstrap variables

`SIRIUS_ASSET_CONFIG_SOURCE__` variables are decoded with the same path rules as
[configuration overrides](CONFIG_OVERRIDES.md) into a typed structure with
`deny_unknown_fields`; a misspelled name is an error. Exactly one section, matching the URI
backend, must be present.

| Field | Meaning |
| --- | --- |
| `TIMEOUT_SECONDS` | Whole fetch deadline including credential acquisition, 1..300, default 30 |
| `FS__ROOT` | Absolute directory without `..` components |
| `S3__ENDPOINT`, `S3__BUCKET`, `S3__REGION` | As in the storage S3 backend |
| `S3__PATH_STYLE` | Default `true`; `false` selects virtual-host addressing |
| `S3__REQUEST_PAYER` | Default `false` |
| `S3__ACCESS_KEY_ID_ENV`, `S3__SECRET_ACCESS_KEY_ENV`, `S3__SESSION_TOKEN_ENV` | Names of environment variables holding credentials |
| `S3__CREDENTIALS_FILE__PATH` / `__PROFILE` / `__REFRESH_SECONDS` | Explicit shared credential file (mutually exclusive with the `*_ENV` keys) |
| `S3__ASSUME_ROLE__ROLE_ARN` / `__REGION` / `__SESSION_NAME` / `__EXTERNAL_ID_ENV` / `__DURATION_SECONDS` | Explicit STS AssumeRole from the base credentials |

S3 sources reuse the publication backend's validation, credential references, shared credential
file and STS code (see [STORAGE.md](STORAGE.md)): HTTPS endpoints only (plain HTTP only for
loopback test fixtures), no userinfo/query/path on the endpoint, verified TLS, no ambient or OS
proxy, no redirects, no ambient AWS profile/metadata discovery. Write, ACL and encryption options
do not apply to a read and are not accepted. A key in the example above could be read with a
read-only (`s3:GetObject`) credential.

## Fetch, limits and overrides

The loader stats the object, rejects non-files, empty objects and objects over **1 MiB**, then
reads exactly the stated length and fails if a different length arrives. The text must be UTF-8.
The bytes are read once per process: application logging and the command decode the same
snapshot. `SIRIUS_ASSET__A__B=value` overrides are applied after the fetch exactly as for a local
file, followed by normal typed decoding and validation.

Errors are static and never include the URI, key, endpoint, bucket, credential variable names or
values: `invalid configuration` (URI, bootstrap, conflict, empty/non-UTF-8/invalid document),
`required secret is unavailable`, `response exceeds configured size limit`, `request failed`
(timeout or temporary backend error) and `remote configuration source is unavailable` (missing
object, denied access, not a file, changed during read). There is no retry; the command fails
and the operator or scheduler reruns it.

## Scope and what is not restored

- Only the main download configuration is remote. Documents named on the command line
  (`serve`, `export`, `publish`, `plan-storage`) and service profile documents
  (`download_config`, `export_config`, `storage_config`) remain local files. The service validates
  every profile document synchronously at startup and re-reads it per job; making those remote
  would add a network dependency and a validation/job-time consistency window to every job, so it
  is intentionally not done. Mount remote documents locally if needed; each job read is then a
  snapshot of the local file at job start, as documented in [CONFIG_OVERRIDES.md](CONFIG_OVERRIDES.md).
- The original accepted any compiled OpenDAL scheme through `HARUKI_CONFIG_OPENDAL_SCHEME` and
  free-form `HARUKI_CONFIG_OPENDAL_OPTION_*` options, including inline secret values. Sirius builds
  only the `fs` and `s3` services, and free-form options would bypass its credential-reference,
  endpoint and transport policy, so other schemes and untyped options are not supported.
  `HARUKI_CONFIG_OPENDAL_ROOT` maps to `FS__ROOT`; for S3 the key is the full object path.
- The original's provider name in `opendal://<provider>/<path>` was only a label; in Sirius that
  position selects the backend.
- The original's `${env:VAR}` interpolation is not restored (see
  [CONFIG_OVERRIDES.md](CONFIG_OVERRIDES.md)).
