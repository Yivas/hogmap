# Understanding sizes

A size is useful only when its meaning is clear. HogMap keeps three different measurements separate. The Windows scanner supplies directory metadata to the current core, and the map uses known unique logical bytes. The synthetic sample remains a separate in-memory fixture.

## Logical bytes

Logical size is the length of a file's data, not a promise about space that deleting it would recover. It is the metric used by the first filter implementation.

A zero-byte file has a known size of zero. A file whose size could not be obtained has an unknown size. Those cases must not be merged: an inaccessible directory or a failed read cannot be reported as empty.

The index summary reports visible logical bytes (`known_logical_bytes`), bytes charged once per file identity (`unique_logical_bytes`), bytes of later hardlink names (`shared_logical_bytes`) and the number of files with unknown logical sizes, all separately. Its completeness flag also reflects pending, partial, inaccessible and stale entries. Overflow returns an error rather than a plausible-looking capped total.

## Physical attribution

Allocated storage can differ from logical length because of compression, sparse regions, additional streams and filesystem allocation rules. Multiple hardlink names can also refer to the same physical file.

The index preserves optional allocated size and file identity, and the core now aggregates **observed allocated bytes** as a metric of its own, independently of logical bytes. In each metric a file identity is charged once, at its first entry with a known size for that metric, and later names add shared bytes. A known logical size does not consume the allocated charge, and an unknown logical size does not skip a known allocated size. `Some(0)` is a known size; `None` is unknown and is never treated as zero.

Neither metric is a deduplicated physical total, and **physical attribution is not implemented**. Two separate indicators say whether a reconciliation could be trusted: entries with a known size but no identity, which are charged individually, and identities whose known samples disagree, which are counted once while the first charge is kept and no record is dropped. The primary Windows enumeration provides file identities and allocated sizes; its fallback lacks both, so fallback files are counted individually and allocated size stays unknown. The status tooltip and its accessible label report fallback directory counts after the engine returns.

The treemap can weigh either metric: `layout_with_metric` and `layout_members_with_metric` take the metric, and `layout`/`layout_members` keep the logical path. A hardlink adds no area if its identity has already been charged in the selected metric; the first known size for that metric can arrive on a later link. Every entry remains in the index, and an unknown size in the selected metric gets no area rather than being treated as zero. The window still paints logical bytes: there is no metric selector, and free, not-yet-scanned and gray composition remain unimplemented.

## Windows used and free space

Windows reports capacity and used/free space for a volume. A sum of file sizes is a different measurement. Filesystem metadata, shared allocation, access restrictions and a changing volume can explain differences.

The volume header shows global filesystem free/total capacity separately from map bytes. Windows GetDiskSpaceInformationW supplies Actual total/available allocation units, ignoring caller quotas; no caller-quota total or partition length is presented as filesystem capacity. Invalid units, arithmetic overflow, incoherence or a failed query leave both capacity values unknown with a reason, without erasing a known label or filesystem. A known zero-free value means a full volume, not unknown. The source and query-completion time stay in memory; chooser/header hover and accessible details show source, query age and any failure reason. This is an observation of a changing filesystem, not an atomic snapshot shared with the scan. Real quota-enabled and storage-pool environments remain unverified; see the [capacity contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md#windows-metadata-engine). The current map has no free-space or not-yet-scanned blocks and does not reconcile logical totals with Windows used space. Pending or unattributed composition remains future work; free space and unknown space are not real folders.

## Folders with no known bytes

The window gives area to known logical bytes; the library can instead weigh known allocated bytes. A folder with no charged bytes in the selected metric gets no rectangle, including when all its known sizes belong to identities already charged elsewhere. Its entries stay in the index, and any unknown sizes keep their unknown signal rather than becoming zero. Whether such a folder should keep a navigable empty cell is a product decision for the window and is not implemented.

## What is verified

The public API tests cover known versus unknown sizes, zero-byte files, logical and allocated hardlink charges, missing identities, conflicting samples, per-metric overflow and different logical/allocated geometry using synthetic metadata. Windows engine tests also scan small temporary fixtures with known lengths and hardlinks. They do not establish whole-volume reconciliation, compressed/sparse/ADS attribution or recoverable-space totals.

For the exact data model, read the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). To see how logical sizes affect a query, continue with [filters](./filters.md).
