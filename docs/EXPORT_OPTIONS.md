# Export selection and formats

Download selection operates on catalog labels and dependency closure. Export selection
operates on verified downloaded resources and their decoded Unity objects. Keep the
required downloaded dependencies even when exporting only selected classes.

```yaml
paths: []
selection:
  providers: [unity]
  unity_class_ids: [28, 213]
  embedded_audio: false
image: {format: webp}
audio: wav
```

`providers` accepts `unity` (both encrypted/decrypted Unity bundle providers) and `cri`.
An empty list means both. `paths` selects exact receipt resource paths within these
providers; unknown/excluded paths fail. An empty resulting resource set is an error.
`unity_class_ids` selects exact positive serialized Unity class IDs; empty means all.
For example, 28 is Texture2D, 213 Sprite, 49 TextAsset and 114 MonoBehaviour. Class
filters apply only to Unity objects. Duplicate IDs/providers and class filters with
CRI-only selection are rejected. There is no Sekai category or model-ID translation.

Selected Sprites still resolve atlas dependencies. Referenced helper objects remain
available to readers without being independently exported. Selecting MonoBehaviour
can export JSON, Cubism or embedded SplitAcb data according to the decoded object.
`embedded_audio: false` skips embedded ACB waveform extraction while retaining selected
object metadata/backing bytes. It does not suppress standalone CRI resources; use the
provider selection for those. Selecting only TextAsset does not imply reconstructing
its owning SplitAcb MonoBehaviour. Unsupported or malformed selected data still fails.

## Image output

PNG remains the default. PNG, lossless WebP, BMP and TGA use the native Rust encoder
and preserve RGBA. Texture rows retain the existing Unity-to-display transformation;
Sprites are already in display order. Non-PNG results are additionally decoded by
FFmpeg before being recorded. The same per-resource size limit applies to every format.

JPEG requires explicit compositing, for example:

```yaml
image: {format: jpeg, quality: 90, background: [255, 255, 255]}
```

JPEG is lossy and cannot store alpha. Both quality (1..100) and RGB background are
mandatory so transparency handling is intentional. Unknown format options fail parsing.

## Audio output

`audio: wav` preserves existing PCM WAV output. `audio: flac` uses FFmpeg after native
HCA or verified ADX decoding, then decodes the FLAC back to PCM and compares channel
count, sample rate and every sample with the source WAV. Only after this check succeeds
is FLAC recorded and the intermediate WAV removed. Cue metadata remains unchanged.
This applies to embedded ACB, standalone ACB and USM audio. Matroska video muxing keeps
the selected WAV/FLAC audio and original video stream; alpha video handling is unchanged.

`audio: mp3` adds a lossy compatibility file using FFmpeg/libmp3lame and also records the
original decoded PCM WAV. MP3 never replaces the preservation source. Subsequent USM video
muxing uses that WAV, avoiding an extra lossy intermediate before MP4 encoding.

The encoder preserves mono/stereo and native MP3 sample rates: 32/44.1/48 kHz at 192 kbit/s,
16/22.05/24 kHz at 128 kbit/s, and 8/11.025/12 kHz at 64 kbit/s. Unsupported rates, empty PCM
or more than two channels fail explicitly; no implicit resampling or downmixing is performed.
The complete MP3 is decoded with errors treated as failures. Channels and sample rate must
match; sample-frame count may differ by at most 1,152 for codec framing/padding. This verifies
structure/duration, not lossless PCM equivalence. Both WAV and MP3 count toward the resource
output budget, journal, storage publication and independent hash verification.

Audio mode participates in cache identity. Arbitrary simultaneous format lists and codec
quality tuning remain outside the current scalar audio configuration.

## Video output

`video` accepts `source`, `mkv` (default), `mp4`, or `mkv_and_mp4`.
All modes retain native demuxed video streams with their actual M2V/IVF extensions and the
selected decoded WAV/FLAC audio. Sirius can carry either video codec; `source` does not rename
IVF as M2V. It uses a temporary MKV for full decode/frame-count verification, then removes that
container. MKV remuxes the original video and selected audio without re-encoding.

MP4 is an additional compatibility rendition: H.264/libx264, medium preset, CRF 18,
YUV420P, AAC 192 kbit/s and fast-start metadata. This rendition is lossy; original video and
WAV/FLAC audio remain the preservation outputs. `mp4` removes the intermediate MKV after
validation, while `mkv_and_mp4` retains both. Encoder options are fixed in this version;
FFmpeg must provide libx264/AAC. Unsupported dimensions or encoder failures fail the resource
rather than silently resizing/padding or changing the requested format.

Every output container is fully decoded and its video frame count checked against the USM
metadata. The existing media deadline, cancellation and output-byte budget apply. USM color
and alpha streams remain in separate directories: MP4 is not a combined transparency export,
and its alpha rendition is also lossy. Native alpha streams remain intact.

Video mode is recorded in schema-4 summaries and participates in decoded-cache identity.
Existing configurations keep MKV behavior; existing summaries without `video` read as MKV.
The active backend remains the configured FFmpeg executable for media and native Rust for
Unity/CRI parsing. FFI/backend-choice restoration remains a separate audit item; no ignored
backend setting is exposed.

## Receipts and completeness

Export summary schema 4 records selection, image/audio/video formats, selected/skipped Unity
object counts and `full_export`. Resource journals report the same object counts and
identities for outputs. `unity_objects` counts all source objects; selected and skipped
counts explain a successful subset without concealing omitted objects.

- `full_catalog`: every remote catalog resource was included as an input.
- `full_export`: full catalog scope with all providers/classes and embedded audio enabled.
- `complete`: all selected resources succeeded and at least one output was generated.

These scope flags do not imply completion on their own. Production full acceptance
requires `complete=true`, `full_catalog=true`, `full_export=true`, zero failures and
independent retained-file verification. A subset may complete successfully while
`full_export=false`; selecting classes absent from all inputs cannot report success.
Legacy summaries missing new fields deserialize conservatively with `full_export=false`.

## Resource and media concurrency

`concurrency` bounds concurrently processed resources (1..4). `media_concurrency` separately
bounds FFmpeg processing children shared by those resource workers (1..4, default 2).
Native parsing/image conversion can progress while other resources wait for a media slot.
Audio conversion, video remux/encoding and independent decode validation share this limit;
the one-off executable version probe precedes resource execution.

`media_timeout_seconds` covers both admission wait and subprocess execution. Waiting observes
cancellation at most every 20 ms and never launches a child after a cancelled/expired admission.
A slot is released on normal completion, cancellation, deadline or spawn failure. Existing child
cancellation/deadline handling kills and reaps the process before returning.

Limits apply per export job, not host-wide: multiply the media limit by active service jobs when
planning capacity. A lower media limit may require a longer media deadline for long videos.
The default permits two processing children rather than tying child count to four resource workers.
Set 4 explicitly for the previous potential parallelism. These options control scheduling, not
output identity, and do not invalidate content caches by themselves. They are not hard RSS limits;
output byte budgets, download concurrency and storage upload concurrency are separate controls.
