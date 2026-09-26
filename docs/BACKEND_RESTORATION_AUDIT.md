# Backend restoration audit

Audited against Haruki-Sekai-Asset-Updater commit
`3d33ed037f0ef5009e361e0535b3b19f8c239947` (local checkout HEAD verified).
This is the original capability inventory; the final field-by-field status is in
[HARUKI_CONFIG_AUDIT.md](HARUKI_CONFIG_AUDIT.md).

## Storage

The original root `Cargo.toml` enables OpenDAL `services-fs` and `services-s3` only.
The generic provider `scheme`/`options` configuration does not mean every OpenDAL service is
compiled into the original executable. Sirius already implements those two compiled service
families through its typed local/S3 adapters. No third service family was found in this baseline;
adding unrelated backends is not required merely because OpenDAL offers them.

Original `src/core/storage.rs` and `src/core/config/schema.rs` expose additional generic controls:

| Original capability | Sirius state | Remaining work |
| --- | --- | --- |
| Local filesystem and S3 providers | Implemented with verified publication | Production acceptance |
| Multiple destinations and provider selection | Per-profile storage configuration lists required providers | Document migration from global provider registry |
| Streaming upload, concurrency, retry, upload-before-cleanup | Implemented with read-back verification and per-object service progress | Production resource tests |
| Path-style / virtual-host-style S3 addressing | Explicit path_style, legacy default true; both signed request targets tested | Production endpoint acceptance |
| S3 public-read policy and per-file include/exclude rules | Explicit opt-in with exclusion precedence; write/multipart/marker tests | Production bucket-policy acceptance |
| Public base URL / planned storage target information | Offline target preview and per-provider publication URL receipts | Registry integration and production CDN verification |
| Region templates in bucket/root/prefix/options | Explicit profile providers plus mandatory region prefix | Audit equivalent multi-region configuration and migration |
| Generic scalar OpenDAL options | Typed storage class and SSE-S3/SSE-KMS write policy restored; real PUT/multipart/marker tests | Audit remaining options; do not silently accept ignored options |

The source builds an additional S3 operator with `default_acl=public-read` for matched files;
this is actual upload behavior, not merely a display setting. Public-read and address-style
controls are not Sekai-specific and cannot be dismissed as removed game adapters.

## Media

The original pipeline crate exposes `MediaBackend::{Ffi,Cli,Auto}`. Its `media-ffi` Cargo feature
activates optional `rsmpeg 0.18.0` with `ffmpeg7_1` and `link_system_ffmpeg`. The feature-disabled
module reports unsupported FFI explicitly. The root project forwards this feature, and the
original CI contains a dedicated media-ffi build/test job using FFmpeg 7.x development libraries.
Therefore the original FFI backend is a real production capability, not an inert configuration.

Relevant original sources:

- `crates/sekai-asset-pipeline/src/media.rs`: backend dispatch and retry/fallback policy.
- `crates/sekai-asset-pipeline/src/media/ffi.rs`: file/memory video and audio conversion.
- `crates/sekai-asset-pipeline/src/media/ffi/{audio,video,avio,raii,error}.rs`: generic codec,
  resampling/scaling, memory I/O and resource ownership primitives.
- `crates/sekai-asset-pipeline/src/media/ffi_disabled.rs`: explicit unsupported feature errors.

Sirius now integrates `media_backend: cli/ffi/auto` into FLAC/MP3/MP4 export. The optional
FFmpeg 7 bridge includes cancellation/deadline checks, runtime ABI validation and native ownership
handling; Auto falls back to CLI within the remaining deadline. Backend/library identity scopes
export cache entries. Synthetic audio/video, service/cache, fallback and feature-disabled tests
pass; see [MEDIA_FFI.md](MEDIA_FFI.md). Stream-copy muxing, ADX decoding and independent verification
still use the FFmpeg executable. Real Sirius fixture and three-platform package acceptance remain
pending. The codec portions are adapted independently of Sekai encryption, model/chart conventions
and game configuration; preserve Haruki attribution.

The adapter must account for Sirius IVF as well as M2V, independent color/alpha streams,
WAV/FLAC preservation, multiple output formats, frame/PCM verification, resource limits,
cancellation and worker drain. Feature-enabled and disabled builds must reject unsupported
choices before accepting jobs, and Linux/macOS/Windows packaging must declare actual runtime
requirements. A CLI fallback must be explicit and tested, not a successful no-op or a stub.

## Consequence for the restoration ledger

The previously broad “other OpenDAL backends” audit is resolved at the service-family level:
FS and S3 cover the original compiled baseline; storage configuration and publication parity is
recorded in [HARUKI_CONFIG_AUDIT.md](HARUKI_CONFIG_AUDIT.md). Media FFI/backend choice is
implemented and its real-fixture exports run in the FFmpeg 7 CI job; release packages use the CLI
backend by default.
