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

See [incremental export](EXPORT_CACHE.md) for optional verified decoded-resource reuse
and recovery across retries/restarts.

## Outputs

- `summary.json`: success/failure counts, object/output counts, byte totals, kinds and catalog hash.
- `resources.jsonl`: source path/hash, output directory, output paths, lengths, SHA-256 digests
  and available object class/path IDs, names and container addresses.
- Numbered directories: successful retained resources; names from game data are metadata
  and never determine filesystem paths.

Consumers must wait for `complete=true`. `retain_outputs: false` still writes, reads and
validates outputs before deleting each resource's temporary products; report bytes measure
cumulative processing, not final disk use. Concurrency is 1–64 (default 4). Default resource output budget
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

## CPU worker sizing

Manual `concurrency` remains unchanged by default. Opt in per export profile:

```yaml
concurrency: 4
cpu:
  auto_tune: true
  budget_auto: true
  budget_ratio: 0.75
  reserved: 1
```

With automatic sizing, the CPU budget is `max(1, floor(available CPUs * budget_ratio)
- reserved)`, with saturating subtraction. `budget_auto: false` uses all available CPUs
instead and ignores ratio/reservation for sizing. The worker count is the larger of the
configured concurrency and budget, capped at twice available CPUs and 64, then at the
number of selected resources. For example, 16 available CPUs with the settings above
produce 11 workers. Ratios must be finite, greater than zero and at most one, even when
automatic sizing is disabled. CPU discovery uses Rust's `available_parallelism`, falling
back to one if unavailable; discovery is performed once per export, not continuously.
The effective worker count is logged when the resource pool starts.

This restores the original generic post-process sizing policy. Configured concurrency
is a floor before caps, so a reservation is not a hard CPU-use limit. FFmpeg and native
libraries may use their own threads; use OS CPU quotas for strict isolation. Each job
sizes independently: shared service media/download/upload/byte admission still applies.
Network and media concurrency are not automatically widened, and this option does not
implement CPU load sampling or independent codec/image stage controls. Validate workload
memory and performance before increasing concurrency on a production host.

### Aggregate CPU-stage admission

`cpu.limit_stages: true` enables a shared per-export CPU-stage pool sized from
`budget_auto`, `budget_ratio` and `reserved`, independently of `auto_tune`. It defaults
to false to preserve existing scheduling. CPU availability is captured when loading the
export configuration. With ratio 0.5 and reservation 1 on 16 available CPUs, at most seven
instrumented CPU stages run simultaneously even if the resource worker count is larger.

The pool covers Unity parsing/object conversion, CRI container extraction, HCA decode,
FFmpeg processing children and FFI conversion. Container wrappers release CPU slots before
nested waveform processing; HCA releases before media validation/encoding. An image stage
slot is taken before its CPU slot, and media admission precedes CPU admission. Auto fallback
releases the FFI CPU slot and reacquires for CLI within the original media deadline.
Native admission uses `stage_limits.wait_timeout_seconds`; media keeps its original deadline.
Cancellation and failure release local slots even while waiting for the service pool.

The job service can additionally set `max_cpu_stages: 8` (1..256, omitted/null disables)
to share one pool across all exports, including profiles without a local CPU pool. Both
limits apply when enabled. These bound admitted stages, not OS thread counts or total CPU
percent: codecs may use internal threads, and filesystem/cache hashing and runtime overhead
are outside this pool. CPU-use sampling/throttling remains separate work. OS quotas remain
necessary for strict process-tree CPU isolation.
