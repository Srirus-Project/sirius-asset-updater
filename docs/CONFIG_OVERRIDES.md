# Environment configuration overrides

Every YAML document can be adjusted by environment variables without editing the file. This
restores the original `HARUKI__A__B=value` mechanism, with one prefix per Sirius document kind:

| Prefix | Document |
| --- | --- |
| `SIRIUS_ASSET__` | Download/update configuration (`SIRIUS_ASSET_CONFIG_PATH` or `SIRIUS_ASSET_CONFIG_URI`, `check`, `probe`, service `download_config`) |
| `SIRIUS_ASSET_EXPORT__` | `export CONFIG` and service `export_config` |
| `SIRIUS_ASSET_SERVICE__` | `serve CONFIG` |
| `SIRIUS_ASSET_STORAGE__` | Service profile `storage_config` |
| `SIRIUS_ASSET_PUBLISH__` | `publish CONFIG` and `plan-storage CONFIG` |

The name after the prefix is a path split on `__`; segments are lowercased. A numeric segment
indexes a sequence, growing it with nulls when needed. Missing mappings are created. Values are
parsed as YAML when possible (`42`, `true`, `[a, b]`, `{format: png}`) and otherwise used as the
literal string; an empty value is an empty string.

```sh
SIRIUS_ASSET_SERVICE__MAX_CONCURRENT_JOBS=2
SIRIUS_ASSET_EXPORT__IMAGE__0__FORMAT=webp
SIRIUS_ASSET__LOGGING__LEVEL=debug
```

The original's targeted variables (`HARUKI_MEDIA_BACKEND`, `HARUKI_CPU_BUDGET_RATIO`, ...) have no
separate names; their path equivalents are listed in
[HARUKI_CONFIG_AUDIT.md](HARUKI_CONFIG_AUDIT.md#loading-environment-and-remote-configuration).

Overrides are applied to the parsed YAML before typed decoding, so they are subject to the same
`deny_unknown_fields` rules and validation as file contents: a misspelled path or wrong type is a
configuration error, not a silently ignored setting. Descending through a scalar or indexing a
mapping is also an error. Application logging reads the overridden `logging` section. Overrides
apply wherever that document kind is loaded, including every service profile that references a
document of that kind; the service re-reads profile documents per job, so the job process's
environment at load time is what counts.

`SIRIUS_ASSET_CONFIG_SOURCE__` uses the same path rules but is not a document override: it is the
typed bootstrap for a remote download configuration and is built from the environment alone
(see [REMOTE_CONFIG.md](REMOTE_CONFIG.md)). Download overrides apply after a remote fetch.

Limits: at most 256 overrides per document kind, 16 path segments, 64 KiB per value and
sequence index 1024. Only ASCII letters, digits and `_` are accepted in segments, so mapping keys
containing `-`, `.` or uppercase letters cannot be addressed; edit the file for those.

There is intentionally no `${env:VAR}` string interpolation. Secrets remain typed `*_env`
references, so their values are never copied into parsed configuration, receipts or summaries.
Avoid placing secret values in overrides for the same reason.
