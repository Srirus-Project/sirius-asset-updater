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

## Explicit API and CDN forward proxies

API version refresh/snapshot requests and CDN catalog/asset requests use separate clients:

```yaml
network:
  api_proxy:
    url_env: SIRIUS_API_PROXY_URL
    authorization_env: SIRIUS_API_PROXY_AUTHORIZATION # optional full header value
  cdn_proxy:
    url_env: SIRIUS_CDN_PROXY_URL
    authorization_env: SIRIUS_CDN_PROXY_AUTHORIZATION
```

Each URL must be an HTTP or HTTPS proxy origin, without userinfo, path, query or fragment
(for example `http://127.0.0.1:8080`). Authorization is the complete Proxy-Authorization value,
for example `Basic BASE64_VALUE` or a proxy-supported bearer scheme, stored in the referenced
environment variable. Proxy URLs and authentication values are never serialized in receipts
or offline check results. Missing environment variables and malformed values fail before requests.

Omitting a proxy uses direct connections. Ambient `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`,
`NO_PROXY` and OS proxy discovery are disabled. To migrate an environment-based deployment,
explicitly reference the desired URL variable in the appropriate configuration. A configured
proxy applies to every request in that client, including loopback destinations, with no bypass
or fallback to direct connections. Different region profiles may use different proxies.

HTTPS origins use CONNECT, followed by normal origin certificate validation inside the tunnel;
HTTPS proxy certificates are also validated. Proxy authentication belongs to the proxy transport,
never an origin default header. HTTP origins use standard forward-proxy requests, whose headers
(including origin authentication) are visible to that proxy. The existing API/CDN credential
scope checks and no-redirect policy still apply. SOCKS/PAC/custom trust roots are not configured
by these fields. Storage publication has its own transport and is unaffected.

Existing connection and request deadlines bound proxy connections and requests. An HTTP 407
response is terminal. A failed HTTPS CONNECT may be reported as a transport error by reqwest
and retried within the configured attempt bound; it never bypasses the proxy. Redirects remain
terminal and are never followed with either proxy or origin credentials.
