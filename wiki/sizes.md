# Understanding sizes

A size is useful only when its meaning is clear. HogMap keeps three different measurements separate. The current core stores optional metadata; it does not measure a disk yet.

## Logical bytes

Logical size is the length of a file's data, not a promise about space that deleting it would recover. It is the metric used by the first filter implementation.

A zero-byte file has a known size of zero. A file whose size could not be obtained has an unknown size. Those cases must not be merged: an inaccessible directory or a failed read cannot be reported as empty.

The index summary reports visible logical bytes (`known_logical_bytes`), bytes charged once per file identity (`unique_logical_bytes`), bytes of later hardlink names (`shared_logical_bytes`) and the number of files with unknown logical sizes, all separately. Its completeness flag also reflects pending, partial, inaccessible and stale entries. Overflow returns an error rather than a plausible-looking capped total.

## Physical attribution

Allocated storage can differ from logical length because of compression, sparse regions, additional streams and filesystem allocation rules. Multiple hardlink names can also refer to the same physical file.

The index preserves optional allocated size and file identity, but **physical attribution is not implemented yet**. `known_logical_bytes` counts every visible file entry, hardlinks included. `unique_logical_bytes` charges a file identity once, at its first entry with a known size, and `shared_logical_bytes` holds the later names, so the visible and unique totals stay distinguishable. Neither is a deduplicated physical total, and neither may be labelled recoverable space. The treemap layout uses the unique charge: a later hardlink has no rectangle, although its entry remains in the index.

## Windows used and free space

Windows reports capacity and used/free space for a volume. A sum of file sizes is a different measurement. Filesystem metadata, shared allocation, access restrictions and a changing volume can explain differences.

The future map must show pending or unattributed space with a reason and state, rather than forcing the file total to equal Windows used space. Free space and unknown space are not real folders.

## Folders with no known bytes

The layout can only give area to known logical bytes. A folder whose content is entirely unknown sizes, inaccessible items or later hardlink names has no charged bytes, so the current layout gives it no rectangle. Its entries stay in the index and its subtree keeps reporting unknown content, so nothing is presented as zero. Whether such a folder should keep a navigable empty cell is a product decision for the window and is not implemented.

## What is verified

The current public API tests cover known versus unknown sizes, zero-byte files, logical hardlink entries and overflow using synthetic metadata. No real scan or physical-size comparison has been performed by this core unit.

For the exact data model, read the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). To see how logical sizes affect a query, continue with [filters](./filters.md).
