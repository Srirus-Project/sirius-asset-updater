# Native catalog selection

Sirius uses Addressables keys. The observed clients declare `InitialDownload`, `Everything`
and `MV`; these are not Sekai's start_app/on_demand partitions. Keys are resolved from the
catalog's serialized key index, never guessed from bundle filenames.

```yaml
assets:
  selection:
    keys: [InitialDownload]
    include: []
    exclude: []
    priority: ['^cri_assets_cri/']
```

- Omit selection (or leave all lists empty) for the entire catalog.
- `keys` accepts actual labels or explicit address keys. Multiple keys form a union. Unknown
  or empty named keys fail explicitly; they do not become successful no-op jobs.
- `include`/`exclude` are regular expressions over each root location's primary key or internal
  ID. Exclusions win when selecting roots. The full dependency closure is then added, including
  a dependency which also matches an exclusion: exclusion must not break a selected root.
- `priority` is an ordered list of regexes over final remote relative paths. First matching
  expression wins; unmatched files come last, with lexicographic order within each priority.
- Empty results are errors. Configure a different selection instead of treating an empty
  publication as a completed update.
- `inspect-keys CATALOG_FILE` reports the catalog's string-key/location index for inspection.

On the investigated JP catalog, InitialDownload includes 3,289 remote files, Everything 13,364,
and MV 110 after dependencies/deduplication. InitialDownload and MV are disjoint subsets of
Everything. All catalog entries include 13,367 remote files: three are outside Everything.
These are sample counts, not hardcoded application rules or guarantees for other versions/regions.

Receipts persist the selection; verification reconstructs it from the original catalog and
checks exactly the expected assets and dependency graph. Missing selection in a legacy receipt
means the entire catalog. The v1.1 serialized locations graph remains unchanged.

`verify.full_catalog` and export `summary.full_catalog` distinguish full scope from a successfully
completed subset. `catalog_remote_files` / export `catalog_files` retain the whole-catalog remote
count. For full acceptance require full_catalog=true, all remote files processed and zero errors.
Catalog-only publication and native Everything selection must not be claimed as full acceptance.
Embedded RuntimePath entries are recorded, not downloaded from the CDN.

The tutorial has a separate address-list asset and runtime required-address logic. No tutorial
preset is provided until that composition is verified. MasterDownload's presentation/tips
priority is unrelated to bundle download priority.
