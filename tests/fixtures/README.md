# Synthetic bundle prefix vector

`bundle-prefix.bin` contains synthetic bytes; it is not a game asset or a complete
UnityFS bundle. Plaintext byte i is i % 251, with bytes 0..8 replaced by `UnityFS\0`.
Length: 16421 bytes. Key: 00..0f, nonce seed: 10..17, original name: fixture.bundle.
The nonce is SHA256(seed || UTF8(name))[:8], followed by an eight-byte zero counter.
OpenSSL AES-128-CTR independently encrypted bytes 0..16384; the tail is unchanged.
These public test constants are not game keys. No real samples belong in this directory.
