# Backend restoration audit

Audited against Haruki-Sekai-Asset-Updater commit
`3d33ed037f0ef5009e361e0535b3b19f8c239947` (local checkout HEAD verified).
This is a capability inventory, not completion of the 1.2.0 release gate.

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
| Streaming upload, concurrency, retry, upload-before-cleanup | Implemented with read-back verification | Fine upload progress and production resource tests |
| Path-style / virtual-host-style S3 addressing | Explicit path_style, legacy default true; both signed request targets tested | Production endpoint acceptance |
| S3 public-read policy and per-file include/exclude rules | Explicit opt-in with exclusion precedence; write/multipart/marker tests | Production bucket-policy acceptance |
| Public base URL / planned storage target information | Offline target preview and per-provider publication URL receipts | Registry integration and production CDN verification |
| Region templates in bucket/root/prefix/options | Explicit profile providers plus mandatory region prefix | Audit equivalent multi-region configuration and migration |
| Generic scalar OpenDAL options | Typed subset only | Map supported non-game options; do not silently accept ignored options |

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

Sirius currently uses native Rust Unity/CRI parsing and the configured FFmpeg executable for
media conversion/validation. This restores media results but does not restore FFI/Auto backend
selection. That remains required work. The reusable codec portions can be adapted independently
of Sekai's encryption, model/chart conventions and game configuration; preserve Haruki attribution.
Do not import the entire Sekai pipeline solely to obtain its codec layer.

The adapter must account for Sirius IVF as well as M2V, independent color/alpha streams,
WAV/FLAC preservation, multiple output formats, frame/PCM verification, resource limits,
cancellation and worker drain. Feature-enabled and disabled builds must reject unsupported
choices before accepting jobs, and Linux/macOS/Windows packaging must declare actual runtime
requirements. A CLI fallback must be explicit and tested, not a successful no-op or a stub.

## Consequence for the restoration ledger

The previously broad “other OpenDAL backends” audit is resolved at the service-family level:
FS and S3 cover the original compiled baseline. Storage **configuration and publication** parity
is still incomplete. Media FFI/backend choice remains a release-blocking generic capability.
Neither finding relaxes the full yhm01 acceptance, public artifact audit or 1.2.0 release gates.
