# Application logging

The executable initializes one process-wide logger from the top-level `logging` configuration.
HTTP access logging remains independently configured by `access_log`.

```yaml
logging:
  level: info
  format: json
  output:
    type: file
    path: ./logs/application.log
    rotation: daily
    max_files: 7
  queue_capacity: 4096
```

Without this section, application events use info-level text on stderr. `level` accepts
`off`, `error`, `warn`, `info`, `debug` and `trace`. `format` is `text` or `json`.

`level` also accepts the original Haruki per-target directive list,
`default[,target=level...]`, for example
`warn,sirius_asset_updater::export=debug,sirius_asset_updater::export::cache=off`. The
first item must be a bare default level; each further item is `target=level`. An event uses
the level of the longest configured target equal to its target or a `::`-separated module
prefix of it (`sirius_asset_updater::exp` does not cover `sirius_asset_updater::export`),
otherwise the default. Levels are the six lowercase names above; case variants, `warning`,
bare targets, empty items and whitespace are rejected rather than normalized. Directives are
bounded to 1024 bytes (and at most 64 targets); targets are ASCII `[A-Za-z0-9_:]`, must not repeat and
must be `sirius_asset_updater` or one of its `::` modules. Dependency targets such as the
original's `hyper=warn` are rejected: dependency events are never emitted, so such a directive
could not take effect. Invalid values fail configuration loading. `RUST_LOG` is still ignored.
Output is `{type: stderr}`, `{type: stdout}`, or a file as shown above. Stdout output is
explicitly opt-in because it interleaves logs with CLI JSON/command output; prefer stderr
or files for scripting. Existing CLI usage/error diagnostics and failures before logger
initialization can still appear directly on stderr, outside this event format.

File output appends, rotates at UTC hourly/daily boundaries or uses `rotation: never`, and
retains at most `max_files` (1–3650) matching its filename prefix. Use separate filenames
for application and access logs. An invalid or unwritable configured sink fails startup
before the server binds or workers start. Relative paths use the process working directory.

The nonblocking queue allows 1–65536 records, default 4096. A full queue drops records instead
of blocking requests; the next event reports cumulative `queue_dropped_records`. The guard
flushes on normal process exit after worker shutdown. Abrupt process termination can lose
queued events. Rotation/retention and bounded asynchronous writes reuse the access-log sink.

JSON events contain UTC `timestamp`, `level`, `target`, `queue_dropped_records`, and `fields`.
Text events use the same fields encoded as escaped JSON after a timestamp/level prefix, so
newlines in field values cannot forge extra log lines. Field strings are capped at 1024 bytes
on a UTF-8 boundary. Only application targets are enabled, even at debug/trace; dependency
HTTP/TLS/debug payload logging is disabled. `RUST_LOG` is not an alternate configuration path.

Allowed fields are message/event/stage/region/job_id/status/error_code, progress counters
(completed/failed/total/bytes/cache_hits), listen/operation/attempt. Headers, bodies, URLs,
credentials, raw decoder errors and resource names are not recorded by application call sites.
This is a field/target policy, not a sanitizer for arbitrary secrets embedded in allowed fields;
keep future event messages static and use sanitized error codes. CLI error diagnostics and
per-resource export reports retain their existing, separate behavior.

Library embedders own their tracing subscriber; parsing a library configuration alone does
not install a process-global logger. The shipped command-line entry points initialize it.

For the updater, put logging in the download configuration for one-shot download/check/probe,
in the export or publish command configuration for those commands, or at the job-service root
for `serve`. Service profiles reject logging in their referenced download/export configurations;
the service owns a single process logger. Offline verify/inspect commands use default logging.
Job end events are emitted only after the terminal status has been persisted. Per-resource
cache hits are debug events; normal progress/stage changes are info and retries are warnings.
