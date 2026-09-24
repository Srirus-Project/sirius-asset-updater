# FFmpeg FFI development backend

The optional `media-ffi` Cargo feature currently compiles a real codec bridge adapted from
Haruki-Sekai-Asset-Updater commit `3d33ed037f0ef5009e361e0535b3b19f8c239947`, under its retained MIT
notice. No Sekai account, encryption, model or chart logic is included. This is implementation
work toward 1.2.0, not an available export backend setting: the export pipeline still uses CLI.

## Build and verified behavior

Use FFmpeg 7.x development libraries (avformat/avcodec major 61, avutil major 59), Clang and
pkg-config. rsmpeg 0.18.0 is built without its default FFmpeg 8 feature, with ffmpeg7_1 and
link_system_ffmpeg instead. Startup of each bridge conversion checks runtime library majors.
The dependency's package version suffix mentions FFmpeg 8; the selected feature and linked
libraries determine this build's ABI. Default builds do not link FFmpeg libraries.

On macOS with a separately installed FFmpeg 7:

```sh
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg@7/lib/pkgconfig cargo test --locked --features media-ffi
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg@7/lib/pkgconfig cargo clippy --locked --all-targets --all-features -- -D warnings
PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg@7/lib/pkgconfig SIRIUS_TEST_FFMPEG=/opt/homebrew/bin/ffmpeg cargo test --locked --features media-ffi ffi_audio_preserves_pcm_and_native_mp3_shapes -- --ignored
```

CI adds a separate Debian trixie job with FFmpeg 7 development libraries; the default job checks
the feature-disabled build. This workflow has not yet been run remotely for the release candidate.
Tests exercise allocation/ownership wrappers, memory IO, rejected malformed input, WAV-to-FLAC
and WAV-to-MP3. Independent CLI decoding verifies exact FLAC PCM for nine sample rates and mono/
stereo, and MP3 output duration within codec frame padding for the same combinations. MP3 rejects
unsupported rates and channel counts, rather than silently resampling/downmixing. File and memory
input entry points share the transcode loop. The bridge's custom AVIO owns and explicitly frees
the current buffer, including a buffer replaced by libavformat.

## Required integration before release

- Connect explicit CLI/FFI/Auto policy to actual export operations, with visible fallback semantics.
- Preserve Sirius stream-copy MKV, M2V/IVF timing, separate alpha, H.264/AAC parameters, source
  preservation and independent frame-count verification. Generic video entry points are ported
  but not yet accepted as equivalent to the Sirius CLI pipeline.
- Integrate cancellation, deadline checks, media admission and worker drain. Direct bridge calls
  currently have no cancellation contract and must not be exposed as cancellable service work.
- Restrict inputs to the exporter's local paths, preserve path encoding, and keep raw FFmpeg logs
  out of application logs. Review all remaining unsafe/error paths before pipeline activation.
- Include library versions/backend policy in decoded-cache identity; validate feature-disabled
  selections explicitly before job admission. Audit library licensing/runtime dependencies for
  all release targets, and test both enabled and disabled packages.
- Complete real Sirius fixture comparisons and the full yhm01 service acceptance.

This feature does not remove the separately installed FFmpeg executable requirement and does not
change existing release archives or deployment defaults. Do not enable it in production merely
because the bridge's unit tests pass.
