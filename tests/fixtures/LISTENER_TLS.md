# Synthetic HTTPS listener fixtures

The listener certificate and PKCS#8 private key were generated locally with OpenSSL
for localhost/127.0.0.1; they do not authenticate any deployed service or game.
The certificate is self-signed with SAN DNS:localhost,IP:127.0.0.1 and basicConstraints
CA:FALSE. Tests add this certificate only to their isolated client trust store.
Never deploy this public test key. `listener-other-cert.pem` belongs to a different
locally generated key and checks certificate/key mismatch rejection.
