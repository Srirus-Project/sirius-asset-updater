# Offline export contract

`export CONFIG` reads a previously downloaded and decrypted publication without accessing
the game API or CDN. It first repeats complete `verify`: missing/changed files, symlinks,
provider discrepancies and catalog/dependency mismatches fail before decoding.

```yaml
input: ./downloads/catalog-example
output: ./exports/run-example
retain_outputs: true
concurrency: 4
cri_key_env: SIRIUS_CRI_KEY
split_acb_xor_env: SIRIUS_SPLIT_ACB_XOR
ffmpeg: /usr/bin/ffmpeg
media_timeout_seconds: 120
max_resource_output_bytes: 2147483648
paths: []
```

The CRI key is a decimal u64. The optional SplitAcb XOR secret is one hexadecimal byte,
with optional `0x` prefix; it is read only for scrambled SplitAcb resources. Real keys are
not distributed. FFmpeg handles video remux/decode and ADX; Rust libraries handle Unity,
MOC3, ACB/AWB and HCA. Python and .NET are not application runtime dependencies.

An empty `paths` selects all remote receipt files. Nonempty paths must exactly match receipt
entries; unknown paths fail. `catalog_files` is the full catalog count and `input_files` is
the selected count. Output directories must not already exist. Successful resources are
atomically retained; failures remove partial products and record errors while other resources
continue. Any failure gives exit 1 and complete=false. Cancellation stops new work, terminates
and reaps FFmpeg children, cleans active temporary directories and exits 130.

## Outputs

- `summary.json`: success/failure counts, object/output counts, byte totals, kinds and catalog hash.
- `resources.jsonl`: source path/hash, output directory, output paths, lengths, SHA-256 digests
  and available object class/path IDs, names and container addresses.
- Numbered directories: successful retained resources; names from game data are metadata
  and never determine filesystem paths.

Consumers must wait for `complete=true`. `retain_outputs: false` still writes, reads and
validates outputs before deleting each resource's temporary products; report bytes measure
cumulative processing, not final disk use. Concurrency is 1–4. Default resource output budget
is 2 GiB, configurable up to 16 GiB. Each external media invocation has a bounded timeout.

## Formats and validation

| Input | Output and validation |
| --- | --- |
| Texture2D / Sprite | PNG, decoded again by a PNG reader; catalog dependencies resolve atlases |
| TextAsset | Original bytes, with names/container addresses in the journal |
| Structured objects | Embedded TypeTree JSON, parsed again; invalid/missing trees fail |
| TypelessData in JSON | Companion `.object.bin`; JSON Offset/Size refers to object-relative bytes |
| Mesh | OBJ; structurally proven empty meshes preserve their JSON instead |
| Shader | Shader text, or explicitly labeled TypeTree JSON when source reconstruction is unsupported |
| Live2D | Header-validated MOC3 with separately exported textures and related JSON |
| ACB / inline ACB / SplitAcb | Ordered chunk reconstruction; all physical AWB waveforms to HCA PCM16 WAV with cue mappings |
| USM | Video elementary stream and MKV; independent color/alpha tracks; exact decoded frame count |
| ADX | Complete block checks, WAV decode and exact sample-count/rate/channel checks |

Unknown external AWB requirements fail. HCA uses the configured key and AWB subkey.
WAV validation checks RIFF length, PCM format, alignment and complete frames. A raw dump is
never used to disguise a failed TypeTree parse. Opaque binary companions are emitted only
after successful structural parsing.

USM remuxing preserves the video codec and uses declared frame rate plus generated timestamps.
Final MKVs are fully decoded and compared with the declared frame count. Color and alpha are
separate outputs, not a composited transparent movie. A plaintext retry is accepted only after
full decode/frame-count validation; `container-mask.json` records the chosen mode without keys.
Older headers that omit audio_codec are treated as ADX and unmasked per original audio packet.

ADX validation avoids discarding complete short final packets that FFmpeg's demuxer may flag
as corrupt. The decode must still exit successfully with no error diagnostics and the exact
expected PCM length. See the [FFmpeg ADX demuxer](https://github.com/FFmpeg/FFmpeg/blob/master/libavformat/adxdec.c).

Success covers the selected receipt and this format contract, not Unity runtime rendering,
scene/animation reconstruction, complete Live2D model3 applications, embedded package assets
or untested future game formats. Synthetic regression tests and optional local FFmpeg/catalog
tests are documented in the main README.
