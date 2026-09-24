# Access logs and trusted forwarding

Omit `access_log` to keep access logging disabled. Configure it at the service root
(alongside `listen`/`tls`), never inside API regional configurations. For the updater,
this belongs in the job-service YAML passed to `serve`.

```yaml
access_log:
  format: json # json or text
  output:
    type: file # alternatively: {type: stdout}
    path: ./logs/access.log
    rotation: daily # never, hourly or daily; rotation uses UTC
    max_files: 7
  queue_capacity: 4096
  trusted_proxies: [127.0.0.1/32, '::1/128']
  proxy_header: x-forwarded-for
```

The block enables logging. Format defaults to JSON, output to stdout, queue capacity
to 4,096, and trusted proxies to an empty list. File output defaults to daily rotation
and seven retained files. Parent directories are created; startup fails if the initial
log file cannot be opened. Files append rather than truncate. Rotated names derive from
the configured basename plus the UTC period. Retention applies to the appender's matching
files; use a dedicated path per service process. With `never`, no periodic new file is
created. Allowed queue sizes are 1–65,536; retention is 1–3,650 files.

Each record contains a timestamp, server-generated UUID request ID, HTTP method, matched
route template, peer/client IP, status, outcome, duration and queue-drop counter. The
response includes `x-request-id`; a caller-supplied value is not reused. Route parameters
(player IDs, job IDs), raw URLs, query strings, request/response bodies, authorization,
cookies and arbitrary headers are excluded. Unmatched routes are `<unmatched>`. Unknown
HTTP extension methods are `OTHER`. Text format quotes route templates; JSON is one object
per line. A stdout sink shares that stream with any other application output; use a file
for a dedicated JSON stream.

The timestamp is request arrival and duration runs until response headers are available.
`outcome: response` includes the HTTP status, including 401/403/404/5xx. A cancelled
handler future records `outcome: cancelled` and a null status. This does not measure
streamed-body completion or client disconnects after headers. Rejected TLS handshakes
never become HTTP requests and therefore have no access record.

Forwarding affects the logged client IP and the `ClientIp` request extension only; it
does not bypass authentication or change account/region selection. Without trusted CIDRs,
only the socket peer is used. When that peer is trusted, the configured header is parsed
as a comma-separated IP chain and walked from right to left across trusted hops. The
first untrusted address is the client; values farther left are ignored. If every hop is
trusted, the leftmost address is used. IPv4-mapped IPv6 peers are normalized to IPv4.

At most 128 trusted CIDRs, one header value, 4 KiB and 32 addresses are accepted. Multiple
header values, malformed addresses, `unknown`, empty elements, address/port pairs or an
oversized chain fall back to the socket peer. Missing socket identity stays null and
never trusts forwarding. RFC Forwarded syntax is not parsed. A custom header must carry
the same IP-list syntax; authorization, proxy-authorization, cookie and host cannot be
configured as address headers.

Writes use a bounded background queue. A full queue drops records to avoid blocking
request handling; `queue_dropped_records` in subsequent records exposes the cumulative
queue drop count, also available through `AccessLog::dropped_records()`. The worker guard
flushes queued records on normal shutdown. These are best-effort operational logs, not
a durable audit ledger: abrupt termination or storage failure can lose records.

This restores access logging and trusted-hop handling. Application log level/format/file
configuration is a separate restoration item; this block does not redirect existing
pipeline progress output or change tracing filters.
