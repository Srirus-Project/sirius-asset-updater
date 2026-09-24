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
Video container/backend selection remains a separate pending restoration item.

## Receipts and completeness

Export summary schema 3 records selection, image/audio formats, selected/skipped Unity
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
