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

## Unity object representation

`selection.unity_class_ids` selects which objects are exported. `read_kinds` independently
chooses their representation using exact positive Unity class IDs:

```yaml
read_kinds:
  default: auto
  classes:
    28: image
    114: typetree_json
    49: object_raw
```

A class override wins over `default`. Omitted policy preserves Sirius's existing native dispatch.
At most 256 class overrides are accepted; incompatible explicit class/mode pairs fail config
validation. An incompatible default for an encountered object fails that resource at runtime.

| Mode | Supported classes | Result |
| --- | --- | --- |
| `auto` | All positive IDs | Existing Sirius class-specific native export |
| `object_raw` | All positive IDs | Exact serialized object bytes as `.object.bin` |
| `typetree_json` | All positive IDs with readable type trees | Explicit JSON; opaque TypelessData also retains backing object bytes |
| `image` | 28 Texture2D, 213 Sprite, 187 Texture2DArray | Configured image rendition(s) |
| `image_archive` | 187 Texture2DArray | Each layer as configured image rendition(s) |
| `audio` | 83 AudioClip | Original encoded audio payload |
| `video` | 329 VideoClip, 152 legacy MovieTexture | Original encoded video payload |
| `text_bytes` | 49 TextAsset | TextAsset byte payload |
| `font` | 128 Font | Native font payload |
| `shader` | 48 Shader | Shader text; unsupported text extraction fails |
| `obj` | 43 Mesh | OBJ geometry; unsupported/empty geometry fails |

Explicit type-tree extraction never falls back to raw bytes. Explicit `shader`/`obj` do not
fall back to the JSON representations available under `auto`. `object_raw` and `typetree_json`
bypass class-specific decoding, including embedded SplitAcb/Cubism processing; these are
representation choices, not a claim to have produced audio/images/models. `object_raw` names
the entire serialized object deliberately: Haruki's `raw` handled some audio/video/font classes
as their extracted payload instead. Do not translate that old spelling without checking intent.

The policy is recorded in `summary.read_kinds` and contributes to cache identity. Any non-auto
policy conservatively sets `full_export=false`, even if all catalog resources were selected;
`full_catalog` continues to describe download selection. Offline verification rejects an invalid
policy or a non-auto policy claiming full native export. Legacy summaries without the field use
the default auto policy. Every output still carries object identity and participates in hashes,
resource limits, staging and publication verification.

This restores general representation control and the native modes listed above. Animator
and other original dispatch modes still require separate adapter/fixture audits. Unknown modes are rejected; no setting is
accepted as an unimplemented placeholder.

## Texture arrays

Texture2DArray (class 187) supports `auto`, `image` and `image_archive`. Each layer's mip0
becomes `{file_index}_{path_id}_layer_{layer:04}.{extension}` for every configured rendition.
This matches the original reader's layer scope: lower-resolution mips are not exported and
`image_archive` does not produce a ZIP. Names embedded in the asset never become output paths.
Inline data and catalog-scoped streamed dependencies are supported. Unknown formats, missing
or truncated streams and stripped mip0 fail explicitly.

Layers are decoded sequentially under CPU/image admission and cancellation controls. Native
reader allocation limits and the aggregate resource output budget apply; hashes and parent
object identity cover every image. A later layer/rendition failure prevents partial resource
publication. Synthetic fixtures verify mip stride, orientation and alpha; real game-corpus
acceptance remains a separate requirement.

## Unity media payloads

`auto` and the explicit `audio`/`video` modes extract AudioClip, VideoClip and legacy MovieTexture
payloads through the native Unity reader. Inline payloads and catalog-scoped streamed dependencies
are supported. Missing or truncated external resources fail the resource; there is no HTTP lookup
or arbitrary source-path fetch. `object_raw` still exports the serialized object and does not
resolve its media stream. Type-tree mode remains separate.

Payload bytes are preserved exactly. The Unity `audio_raw`, `video_raw` and `movie_ogv` journal
kinds describe extraction, not transcoding or a successful codec decode. The configured CRI audio
and USM video conversion formats do not apply to these Unity payloads. Audio extensions come from
Unity codec metadata; VideoClip extensions come from the original filename's suffix. Only a
single leading dot plus 1–16 ASCII alphanumeric characters is accepted. Output stems always use
file/object IDs, never the source name/path. File counts, hashes, object identity and aggregate
resource limits apply normally; failed extraction cannot publish a partial resource.

MovieTexture extraction is limited to pre-2019.3 layouts carrying `m_MovieData`; modern layouts
without that payload fail explicitly. Synthetic tests verify serialized layouts and exact opaque
payload bytes, dependency resolution, bounds and publication safety. They do not establish playable
codec coverage for every Unity AudioClip/VideoClip format; final game-corpus acceptance remains
required.

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

PNG supports `image: {format: png, compression: fast}` with `fast` (default), `default`
or `best`. These select encoder effort, not image quality: all preserve RGBA and row order.
Encoded file sizes and CPU time depend on image content; `best` is not a promise of a smaller
file for every input. The selected compression participates in export-cache identity and summaries.
Omitted compression and explicit `fast` serialize identically for legacy configuration compatibility.
The same encoded-output limit applies to all modes. Compression on a non-PNG format and unknown
options are rejected rather than silently ignored.

The original Haruki `webp_lossless` flag is not reproduced: at the audited baseline its dynamic
and native RGBA export paths always use `WebPEncoder::new_lossless`, regardless of that flag.
Sirius likewise exports lossless WebP. Multiple image renditions use the list form below.

### Multiple image renditions

`image` accepts the existing single object or a nonempty list of format objects:

```yaml
image:
  - {format: png, compression: best}
  - {format: webp}
  - {format: jpeg, quality: 90, background: [255, 255, 255]}
```

Select any combination of PNG, WebP, BMP, TGA and JPEG (one of each, up to five).
Duplicate formats are rejected even if their compression, quality or background differs,
preventing output-path collisions. Invalid options and empty lists fail configuration loading.
Lists are canonicalized by format extension; order does not change cache identity. A singleton
list serializes as the legacy object, including omitted/explicit-fast PNG equivalence.

Texture2D and Sprite objects are decoded once. Each rendition is then encoded, written and
verified sequentially under the existing image-stage admission and cancellation controls.
CPU permits are released between encoding and file/decoder I/O. Each file keeps the object's
stable stem with its own extension, carries the same source object identity in the journal,
and contributes its bytes/hash to verification and storage publication. The resource byte
budget applies to the sum of all renditions and other outputs, not separately to each format.
A later encoding/verification/size failure prevents publication of the entire staged resource;
partial results cannot populate a successful decoded-cache entry.

`summary.image` is an object for one rendition and a canonical array for multiple renditions.
Readers that only understand the old object shape must be upgraded before using this setting.
Format membership and per-format options participate in cache identity. The full-export flag
still describes object/provider selection; additional renditions do not broaden that scope.

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

Audio accepts either a legacy scalar or a nonempty list, for example `audio: [wav, flac, mp3]`.
All seven nonempty combinations are supported. Empty, duplicate and unknown formats are errors.
Selections are canonicalized, so list order and scalar-versus-singleton syntax do not change
cache identity or summary representation. Codec quality tuning remains fixed as described above.

When MP3 is selected without FLAC, WAV remains as the lossless preservation output even if not
explicitly listed. With `[flac, mp3]`, every conversion/verification completes before the temporary
WAV is removed; FLAC is the preservation and video-muxing source. Explicit WAV always remains.
Each retained output is recorded exactly once and counted toward the same resource byte budget.
Old scalar configuration and schema-4 summaries remain readable; multi-format summaries serialize
`audio` as a canonical array, which consumers must allow.

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

`concurrency` bounds concurrently processed resources (1..64, default 4). `media_concurrency` separately
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

Resource workers are explicitly configurable beyond four for larger hosts. Automatic widening
is opt-in through [CPU worker sizing](EXPORT.md#cpu-worker-sizing). Each worker can hold a decoded resource and its dependencies; configure
`max_in_flight_bundle_bytes` and OS memory limits before increasing the worker count for large
assets. Media admission remains separately bounded by `media_concurrency` and the service's
`max_media_processes`; increasing resource workers does not bypass those limits. Job concurrency
can multiply resource workers across regions. Sampled CPU throttling is described in [CPU policy](EXPORT.md#sampled-cpu-throttling); automatic sizing of individual stages remains restoration work.

## Independent decoder stages

Each export profile may constrain specific processing stages independently:

```yaml
stage_limits:
  acb: 2
  usm: 1
  hca: 4
  image: 4
  audio_encode: 2
  video_encode: 1
  wait_timeout_seconds: 3600
```

Each stage defaults to omitted/null (no additional limit), or accepts 1..64. ACB slots cover
container parsing, waveforms and their publication, including embedded and SplitAcb containers.
USM slots cover one color/alpha container's extraction, decoding, validation and output, including
its plaintext fallback. HCA slots cover native PCM decoding in both ACB and USM and are released
before media validation or optional audio encoding. Image slots cover Texture2D/Sprite decoding
and image encoding; the encoded bytes may remain resident while output is written.

Admission checks cancellation at least every 20 ms. `wait_timeout_seconds` (1..3600) bounds
waiting for each stage, not the synchronous decoder's execution. RAII releases slots on success,
error or unwind. Workers acquire container stages before HCA/media; no stage recursively acquires
itself. Waiting still occupies a resource worker, so these controls do not guarantee fair task
ordering. They do not impose a memory ceiling. Decode-cache
hits bypass these stages; scheduling controls do not change cache identity. Limits are per export,
with existing shared service media and byte budgets still applying across jobs.

`audio_encode` limits FLAC/MP3 conversions and `video_encode` limits MP4 conversions.
Both apply to CLI, FFI and Auto; Auto holds its encoding slot across FFI failure and CLI
fallback. Encoding slots are acquired before the existing local/shared media slots, and
all waits and attempts share the original `media_timeout_seconds` deadline. The stage wait
limit may shorten admission but never extends the media deadline. Stream-copy remuxing,
ADX decoding and independent output verification remain under the general media gate; they
do not acquire an encoding slot. These are per-export caps, with no change to default output
formats or decoded-cache identity.

### Automatic stage widths

`stage_limits.auto_tune` defaults to false, preserving the optional explicit caps above.
When enabled, omitted stages receive a CPU-budget-derived cap. Existing explicit stage
values remain upper bounds; automatic sizing never widens them. The budget uses the
profile's `cpu.budget_auto`, `budget_ratio` and `reserved` with the CPU count captured when
loading the export profile. ACB, USM, HCA, image and audio encoding use the budget clamped
to 1–64. MP4 encoding uses half the budget, rounded down and clamped to 1–64, because both
current CLI and FFI video encoders use two encoder threads. A budget below two still
allows one video conversion. These are admission estimates, not benchmark-optimal widths
or a guarantee about total decoder/codec threads or measured CPU utilization.

```yaml
cpu:
  auto_tune: true
  budget_ratio: 0.75
  reserved: 1
stage_limits:
  auto_tune: true
  usm: 2
  video_encode: 1
```

Worker sizing (`cpu.auto_tune`), aggregate CPU admission (`cpu.limit_stages`), sampled
throttling and stage sizing are independent switches. All configured worker/media/service
and byte-budget gates still apply; automatic stage sizing does not increase
`media_concurrency` or a service's shared media/CPU limits. Effective stage values are
logged alongside the actual worker width. Scheduling configuration remains outside the
content-cache identity, so changing it alone does not invalidate verified export results.
This policy intentionally does not import corpus-specific Sekai performance ratios;
Sirius production throughput still requires measurement on the final candidate.

## Raw Unity bundles

`raw_bundles` publishes verified stored Unity bundle bytes, after download decryption when
applicable. It is independent of object representation (`object_raw` is one serialized object).
Omit it to keep decoded-only behavior:

```yaml
raw_bundles:
  mode: alongside
  include: ['\.bundle$']
  exclude: ['debug']
  output_prefix: raw
```

`alongside` adds matching Unity bundles to the usual decoded outputs. `only` selects matching
Unity bundles without invoking Unity/CRI decoders or FFmpeg; `cri_key_env` and `ffmpeg` may be
omitted in that mode. Standalone CRI resources are excluded from raw-bundle selection. Regexes
match Sirius receipt relative paths, with empty include meaning all Unity bundles and exclusion
winning. Patterns are bounded and validated. These filters operate within the downloaded catalog
selection and export `paths`/provider selection; they do not download unselected resources or
invent Sekai categories. Raw-only empty selections fail.

Each bundle is stored at `{resource_index}/{output_prefix}/{source_relative_path}`. Prefixes
must be safe relative paths inside the staged export; choose the storage provider directory/prefix
for external placement instead of bypassing publication with an arbitrary output directory.
Original Sirius filenames/extensions are preserved. Records use kind `raw_bundle`, the complete
stored-source SHA-256 and no object identity. Copying is streamed and cancellable, shares the
resource output budget with decoded images/audio, and verifies the source hash before publication.
Any copy/decode failure prevents publication of that resource, including previously staged files.

Raw policy contributes to decoded-cache identity and is recorded in the export summary. Offline
verification checks raw paths, source hashes and policy; normal storage publication uploads and
reads back raw files alongside receipts. Raw-only always sets `full_export=false`, even when all
catalog resources are represented. `full_catalog` remains an independent scope declaration;
retained raw bundles alone never satisfy full decoded acceptance.

## CRI container decoding policy

ACB/USM inputs can be retained as containers instead of decoded:

```yaml
cri:
  acb: preserve # decode (default) | preserve
  usm: decode   # decode (default) | preserve
```

`acb: preserve` records `container.acb` containing the exact ACB bytes presented to the
reader. For embedded SplitAcb this means after chunk reassembly/XOR restoration, inside the
parent object's export directory; the original serialized backing bytes still follow normal
Unity export rules. It does not extract waveforms, decode HCA or apply the audio format list.
`usm: preserve` records the original `container.usm` before alpha-channel splitting, demux or
transcode. Its contents retain the container's original encoding/encryption; this is not a
claim of playable decoded media. Unknown CRI signatures still fail.

Outputs use `cri_acb_container` / `cri_usm_container` journal kinds and participate in ordinary
hashes, parent object identity, aggregate limits, staging, cache and storage verification.
A preservation policy always makes `full_export=false`, including for a full-catalog download.
Offline verification rejects container records under a contradictory decoded policy or a false
full-export claim. Default/explicit decode keeps the previous decoding behavior. The existing
run-level CRI key and FFmpeg configuration remain required for normal export jobs because other
selected Unity/CRI stages may need them; the preservation stage itself invokes neither decoder
nor FFmpeg. Use raw_bundles.only for the separate decoder-free Unity-bundle publication workflow.

This restores explicit control over whether selected CRI containers are decoded. It does not
copy the old HCA flag: the audited Haruki native ACB path with `hca.decode=false` extracts tracks
in memory, returns no track outputs and subsequently removes on-disk source ACBs. It is not a
reliable raw-HCA export operation. Standalone encoded-HCA extraction and further CRI-stage skip
policies require separate applicability/fixture work.
