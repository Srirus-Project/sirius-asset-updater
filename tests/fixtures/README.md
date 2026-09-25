# Synthetic bundle prefix vector

`bundle-prefix.bin` contains synthetic bytes; it is not a game asset or a complete
UnityFS bundle. Plaintext byte i is i % 251, with bytes 0..8 replaced by `UnityFS\0`.
Length: 16421 bytes. Key: 00..0f, nonce seed: 10..17, original name: fixture.bundle.
The nonce is SHA256(seed || UTF8(name))[:8], followed by an eight-byte zero counter.
OpenSSL AES-128-CTR independently encrypted bytes 0..16384; the tail is unchanged.
These public test constants are not game keys. No real samples belong in this directory.

The `synthetic_texture()` test builder in `src/export.rs` constructs a v22 Unity serialized
file with one Unity 2022.3 Texture2D, inline RGBA32 pixels, one mip and no external data. Its
16x32 pixels consist of two solid color/alpha halves, generated entirely in code. Field order
was cross-checked against unity-rs-core 0.5.1's parser and public oracle fixtures. It contains
no game assets. It tests actual multi-rendition export, orientation, object identity and staged
publication; optional independent decoding uses a separately installed FFmpeg.

`synthetic_typed_object()` builds an independent v22 serialized file containing an embedded
two-node type tree, synthetic class ID12345 and one integer value42. It tests explicit JSON
representation without depending on a game class or a built-in object layout. Raw-mode tests
compare exact serialized object bytes from the generated Texture2D fixture against export output.

`synthetic_unity_media()` builds v22 serialized AudioClip (Unity2022.3), VideoClip (Unity2022.3)
and legacy MovieTexture (Unity2018.4) objects. Payloads are deterministic opaque markers, not
playable or game-derived media. External fixtures use a generated `media.resS` with a two-byte
prefix and a known bounded region. Tests prove byte-preserving extraction and dependency/path/
size behavior, not codec support. A modern MovieTexture layout and hostile filename suffix are
negative inputs. Field ordering is cross-checked with unity-rs-core0.5.1's native readers.

`synthetic_texture_array()` builds a v22 class187 Texture2DArray with two 2x2 RGBA layers
and two mips per layer. Distinct generated pixels and dummy mip1 bytes verify layer stride,
orientation and alpha. Inline and streamed payloads (offset3 with prefix/trailer) test bounded
resolution. Unknown graphics format and stripped mip0 are negative fixtures. Layouts follow
unity-rs-core0.5.1 readers (Unity2022.3 and2023.2); no game data is included. Optional FFmpeg
checks independently decode both PNG/WebP renditions of both layers.
