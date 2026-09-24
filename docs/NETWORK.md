# Download request policy

The download configuration accepts an optional `network` section. Omitted fields
retain these defaults; profiles in the job service use the same configuration.

```yaml
network:
  connect_timeout_ms: 10000
  download_timeout_ms: 60000
  snapshot_timeout_ms: 10000
  refresh_timeout_ms: 30000
  revalidate_interval_ms: 120000
  snapshot_retry: {attempts: 3, delay_ms: 500, max_delay_ms: 5000}
  catalog_retry: {attempts: 3, delay_ms: 250, max_delay_ms: 5000}
  asset_retry: {attempts: 3, delay_ms: 250, max_delay_ms: 5000}
```

Request timeouts must be 100–300,000 milliseconds. `download_timeout_ms` covers
a complete catalog or asset request, including its body. Snapshot reads and
version refreshes override that request timeout with their respective values.
The connection timeout applies to every request. These are per-attempt bounds;
the job service's `timeout_seconds` separately bounds the complete operation.

Each retry policy allows 1–8 total attempts, including the first request.
Only transport errors, HTTP 429 and HTTP 5xx are retried. Backoff doubles from
`delay_ms`, capped by `max_delay_ms`. The initial delay must be 1–10,000 ms;
the cap must be at least that delay and at most 30,000 ms. Setting attempts to
1 disables retries. Policies do not interpret Retry-After or add jitter.

`snapshot_retry` covers the refresh/read/validate sequence: a transient read
failure can repeat the version refresh. Unavailable or stale snapshots, changed
identity, authentication failures, redirects, format errors and integrity
failures remain terminal. Catalog and asset policies restart the unpublished
file from byte zero. Cancelling during backoff stops the pending retry and
removes the operation's staging directory; verified cache entries survive.

`revalidate_interval_ms` permits 1,000–120,000 ms. The interval is checked
between download batches, not by a background timer, so an active batch can
extend the elapsed interval. Final snapshot revalidation before asset publication
is mandatory regardless of this setting. Version refresh still requires the
separate `refresh_token_env`; snapshot reads always use the internal token.
Credentials remain scoped to their original API or CDN origins, with redirects
disabled.
