# FFmpeg encoding backends

The optional `media-ffi` Cargo feature currently compiles a real codec bridge adapted from
Haruki-Sekai-Asset-Updater commit `3d33ed037f0ef5009e361e0535b3b19f8c239947`, under its retained MIT
notice. No Sekai account, encryption, model or chart logic is included. Export configuration accepts `media_backend: cli` (default), `ffi`, or `auto`.
FLAC, MP3 and MP4 encoding uses the selected backend. Stream-copy muxing, ADX decoding and
independent output verification still require the configured FFmpeg executable.

`ffi` requires a feature-enabled build and fails explicitly on unavailable codecs or unsupported
input. `auto` tries FFI when compiled in, removes a failed attempt's partial output and retries
with CLI using the original remaining deadline. Feature-disabled Auto uses CLI. Cancellation
and timeout never trigger a retry. FFI attempts are not repeated; `media_retry` applies only to
FFmpeg child processes ([EXPORT_OPTIONS.md](EXPORT_OPTIONS.md#media-process-retry)). Both backends share the configured media admission gate.
The export summary records the requested backend, completed FFI encodings, fallback attempts and media retries;
these counters do not replace verification results. Cache hits do not count as new encodings.
Backend policy and the linked library version/build digest participate in decoded-cache identity.

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

- Preserve and accept real Sirius stream-copy MKV, M2V/IVF timing and separate alpha alongside
  H.264/AAC output. Synthetic bridge and actual export/service tests are necessary but insufficient.
- Audit library licensing/runtime dependencies for all release targets and test enabled/disabled
  packages. Default archives still use CLI without dynamically linked FFmpeg libraries.
- Complete real Sirius fixture comparisons and the full yhm01 service acceptance.

This feature does not remove the separately installed FFmpeg executable requirement and does not
change existing release archives or deployment defaults. Do not enable it in production merely
because the bridge's unit tests pass.

## Controlled calls and local IO

`controlled(cancel, absolute_deadline, work)` installs per-thread operation context, checks before
and after work and between checked codec operations, and returns distinct Cancelled/Timeout
errors. Nested control scopes are rejected and scope state is cleared on errors or unwinding.
Input contexts retain an Arc to the same control until FFmpeg closes them; interrupt callbacks
can read cancellation/deadlines even when invoked from a different thread. Tests cancel a real
120-second synthetic input after its output is created, expire a running conversion, and verify
subsequent conversions recover. This is cooperative: a single codec call or OS-blocked file IO
cannot be forcibly preempted, so workers must be joined rather than abandoned.

File paths must be absolute; Unix path bytes are preserved without lossy conversion. Inputs set
FFmpeg's protocol whitelist to file only, including for nested playlist resources. A local HTTP
listener test verifies that a playlist cannot open a network segment. Raw FFmpeg logging is set
to quiet once for the process; callers receive typed operation errors instead. The embedding
application must account for this process-wide logging setting.

The caller must still use private output staging and remove partial files on errors/cancellation.
These codec helpers do not publish, delete or atomically replace export outputs themselves. The
export adapter removes failed FFI outputs before a permitted fallback and drains admitted workers.

## Video bridge verification

The H.264 encoder is explicitly libx264 with medium/CRF 18, yuv420p and two codec threads;
AAC remains 192 kbit/s. MP3 explicitly selects libmp3lame. Missing encoders are errors. MP4 uses
faststart. Positive even dimensions are required; changing decoded dimensions is rejected,
not resized. Explicit frame rates must be positive, and absent stream frame rates are errors
rather than guessed 30 fps. The bridge uses constant-rate frame indexing and validates decoded timestamps against that
clock (allowing one input timestamp tick for rounding). Nonzero muxed stream starts, variable
video timing and discontinuous audio require CLI; explicit FFI fails instead of flattening the
timeline. Raw M2V entry points allow a demuxer origin. Auto fallback is tested with real nonzero
and variable-rate movies, including partial-output removal and frame timestamps compared to CLI.

An ignored integration test generates 12-frame M2V and IVF inputs, converts direct native
streams and zero-start IVF/PCM MKV, and independently checks H.264/yuv420p, dimensions, frame rate,
12 decoded frames, AAC sample rate/channels/padding, preserved inputs and moov-before-mdat
placement with CLI FFmpeg and adjacent ffprobe. MPEG-2/PCM muxing produces a 40 ms video offset;
the bridge explicitly rejects this case, and the export Auto test verifies its CLI fallback
against independently probed audio/video frame timestamps. Invalid explicit frame rates and odd dimensions
are rejected before output creation. Run `ffi_video_preserves_m2v_ivf_frames_and_muxed_audio`
with the same environment/feature flags as the audio integration test. CI runs both tests.

This found and fixed a real tail-frame loss: FFmpeg 7 libx264 can leave packet duration zero
although frame duration is set. MP4 then ends at the last PTS and its edit list hides the final
frame. Video packets with unspecified duration now receive one encoder-timebase tick before
muxer timebase rescaling. Real Sirius/alpha fixtures and production verification remain separate acceptance gates;
synthetic success does not complete them.
