# Understanding sizes

A size is useful only when its meaning is clear. HogMap keeps three different measurements separate. The current core stores optional metadata; it does not measure a disk yet.

## Logical bytes

Logical size is the length of a file's data, not a promise about space that deleting it would recover. It is the metric used by the first filter implementation.

A zero-byte file has a known size of zero. A file whose size could not be obtained has an unknown size. Those cases must not be merged: an inaccessible directory or a failed read cannot be reported as empty.

The index summary reports known logical bytes and the number of files with unknown logical sizes separately. Its completeness flag also reflects pending, inaccessible and stale entries. Overflow returns an error rather than a plausible-looking capped total.

## Physical attribution

Allocated storage can differ from logical length because of compression, sparse regions, additional streams and filesystem allocation rules. Multiple hardlink names can also refer to the same physical file.

The index preserves optional allocated size and file identity, but **physical attribution is not implemented yet**. Logical totals count visible file entries, including hardlinks. They are not deduplicated physical totals and must not be labelled recoverable space.

## Windows used and free space

Windows reports capacity and used/free space for a volume. A sum of file sizes is a different measurement. Filesystem metadata, shared allocation, access restrictions and a changing volume can explain differences.

The future map must show pending or unattributed space with a reason and state, rather than forcing the file total to equal Windows used space. Free space and unknown space are not real folders.

## What is verified

The current public API tests cover known versus unknown sizes, zero-byte files, logical hardlink entries and overflow using synthetic metadata. No real scan or physical-size comparison has been performed by this core unit.

For the exact data model, read the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). To see how logical sizes affect a query, continue with [filters](./filters.md).
