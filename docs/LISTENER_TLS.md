# HTTPS listener

The service accepts an optional top-level `tls` block alongside `listen`:

```yaml
listen: 127.0.0.1:8443
tls:
  certificate_file: /etc/sirius/tls/fullchain.pem
  private_key_file: /etc/sirius/tls/key.pem
  handshake_timeout_ms: 10000
```

Omitting `tls` retains the plain HTTP listener, suitable behind a separately managed
reverse proxy. Enabling it makes that listener HTTPS only; it does not open a second
HTTP port or redirect plaintext traffic. HTTP/1.1 and HTTP/2 are negotiated through
ALPN. Existing API/job bearer authentication still applies; TLS does not grant access.

The API accepts this block in its single-region configuration or at the root of a
multi-region deployment. Nested regional `tls` settings are rejected because regions
share one listener. The updater accepts it in the job-service configuration passed
to `serve`, not in download or offline-export configurations.

Certificate chains and an unencrypted private key must be PEM. Files must be regular
files, each no larger than 128 KiB. Symlinks used by certificate renewal tools are
resolved when reading; the opened target is checked. On Unix, a private key must not
be writable by its group or accessible by others; 0600 and service-group 0640 are
accepted. Keep deployment keys outside the repository and release archives. On Windows,
use service-account ACLs; Unix mode-bit checks do not apply.

The loader verifies parsing and the certificate/key match before the listener is
bound or background jobs start. Errors do not include PEM contents or file paths.
The operator must supply a chain valid for clients' hostnames and trust stores;
loading is not a CA issuance or expiration audit. TLS uses Rustls's supported TLS 1.2
and 1.3 defaults. `handshake_timeout_ms` permits 100–60,000 ms, default 10,000.
A stalled handshake is closed without creating an HTTP request.

Certificates are loaded once. Restart after renewal; replacing files does not silently
change existing service state. SIGINT/SIGTERM stop accepting connections and drain active
HTTP requests through the existing shutdown path. The API also stops its Master workers;
the updater retains its job cancellation/drain and durable queue behavior. There is no
mutual TLS or custom cipher configuration in this implementation.

Local tests exercise verified HTTPS with a test-only trust store, HTTP/2, bearer checks,
peer-address extraction, plain-HTTP rejection on a TLS port, handshake timeout and active
request draining. Negative tests reject malformed/mismatched/oversized keys, missing
files and unsafe Unix permissions. Synthetic fixture keys are public test data only.
