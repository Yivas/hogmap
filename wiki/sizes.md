# Understanding sizes

A size is useful only when its meaning is clear. HogMap keeps three different measurements separate. The Windows scanner supplies directory metadata to the current core, and the map uses known unique logical bytes. The synthetic sample remains a separate in-memory fixture.

## Logical bytes

Logical size is the length of a file's data, not a promise about space that deleting it would recover. It is the metric used by the first filter implementation.

A zero-byte file has a known size of zero. A file whose size could not be obtained has an unknown size. Those cases must not be merged: an inaccessible directory or a failed read cannot be reported as empty.

The index summary reports visible logical bytes (`known_logical_bytes`), bytes charged once per file identity (`unique_logical_bytes`), bytes of later hardlink names (`shared_logical_bytes`) and the number of files with unknown logical sizes, all separately. Its completeness flag also reflects pending, partial, inaccessible and stale entries. Overflow returns an error rather than a plausible-looking capped total.

## Physical attribution

Allocated storage can differ from logical length because of compression, sparse regions, additional streams and filesystem allocation rules. Multiple hardlink names can also refer to the same physical file.

The index preserves optional allocated size and file identity, but **physical attribution is not implemented yet**. `known_logical_bytes` counts every visible file entry, hardlinks included. `unique_logical_bytes` charges a file identity once, at its first entry with a known size, and `shared_logical_bytes` holds the later names, so the visible and unique totals stay distinguishable. Neither is a deduplicated physical total, and neither may be labelled recoverable space. The primary Windows enumeration provides file identities and allocated sizes; its fallback lacks both, so fallback files are counted individually and allocated size stays unknown. The status tooltip and its accessible label report fallback directory counts after the engine returns. The treemap layout uses the unique charge: a later hardlink has no rectangle, although its entry remains in the index.

## Windows used and free space

Windows reports capacity and used/free space for a volume. A sum of file sizes is a different measurement. Filesystem metadata, shared allocation, access restrictions and a changing volume can explain differences.

The volume header shows global filesystem free/total capacity separately from map bytes. Windows GetDiskSpaceInformationW supplies Actual total/available allocation units, ignoring caller quotas; no caller-quota total or partition length is presented as filesystem capacity. Invalid units, arithmetic overflow, incoherence or a failed query leave both capacity values unknown with a reason, without erasing a known label or filesystem. A known zero-free value means a full volume, not unknown. The source and query-completion time stay in memory; chooser/header hover and accessible details show source, query age and any failure reason. This is an observation of a changing filesystem, not an atomic snapshot shared with the scan. Real quota-enabled and storage-pool environments remain unverified; see the [capacity contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md#windows-metadata-engine). The current map has no free-space or not-yet-scanned blocks and does not reconcile logical totals with Windows used space. Pending or unattributed composition remains future work; free space and unknown space are not real folders.

## Folders with no known bytes

The layout can only give area to known logical bytes. A folder whose content is entirely unknown sizes, inaccessible items or later hardlink names has no charged bytes, so the current layout gives it no rectangle. Its entries stay in the index and its subtree keeps reporting unknown content, so nothing is presented as zero. Whether such a folder should keep a navigable empty cell is a product decision for the window and is not implemented.

## What is verified

The public API tests cover known versus unknown sizes, zero-byte files, logical hardlink entries and overflow using synthetic metadata. Windows engine tests also scan small temporary fixtures with known lengths and hardlinks. They do not establish whole-volume reconciliation, compressed/sparse/ADS attribution or recoverable-space totals.

For the exact data model, read the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). To see how logical sizes affect a query, continue with [filters](./filters.md).
