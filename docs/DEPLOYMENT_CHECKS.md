# Deployment checks

1. Start Sirius API Proxy with matching environment/client/CDN settings. Use
   `sirius-asset-config.example.yaml` as the updater template and keep secrets in the
   environment. `SIRIUS_ASSET_CONFIG_PATH` selects the configuration, or `SIRIUS_ASSET_CONFIG_URI` reads it
   from an `fs`/`s3` source ([REMOTE_CONFIG.md](REMOTE_CONFIG.md)); never set both.
2. Set `SIRIUS_INTERNAL_TOKEN` for snapshot reads and a different `SIRIUS_API_TOKEN` for
   optional version refreshes. CDN username/password are separate and only go to their
   configured origin. Supply bundle key/nonce seed only when decryption is enabled.
3. Run `sirius-asset-updater check` offline. Run `probe` to inspect API readiness without
   contacting the CDN. Maintenance, stale snapshots or unknown credentials must stop the run.
4. Run the updater. Enable the example's `assets` section for full resources; otherwise
   it publishes only the catalog. Enable caching to reuse verified ciphertext after interruption.
   Reserve space for cache, publication and temporary outputs. Defaults limit each download
   to 512 MiB and the complete resource batch to 16 GiB.
5. Run `verify DIRECTORY` against the publication. Check the exact remote-file count and
   catalog identity. The 28 embedded locations in the validated JP catalog are not remote files.
6. For export, install FFmpeg, provide its configured executable path and the CRI key.
   Supply the SplitAcb XOR environment variable for scrambled SplitAcb resources.
   `retain_outputs: false` performs a full export/check with per-resource cleanup; true
   preserves outputs. Allow additional disk space for the latter.
7. Require exit code 0 and `summary.complete: true`. Inspect `input_files` against
   `catalog_files`: a successful selected subset is not complete catalog validation.

Downloaded assets and exported products are separate publications. The download receipt
hashes local bytes and is not a trusted server signature. Export journals bind outputs to
those inputs. Do not consume an incomplete export summary as a complete dataset.

SIGINT exits 130 and cleans temporary work. Completed ciphertext cache entries remain usable;
partial downloads restart rather than use HTTP Range. Final version rechecks prevent publishing
mixed resource versions. Three bounded retries handle transient observation/download failures;
authentication failures and validation errors require correcting configuration or upstream state.

The executable exits after each job. Configure an external scheduler and prevent overlapping
jobs against the same output path. Keep API/internal/CDN tokens and actual game keys out of
logs, source control, release archives and Docker build contexts.
