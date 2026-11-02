# Working with filters

The core can evaluate a filter against one file's name and optional logical size. There is no filter bar or scanner yet, so these examples describe library behavior rather than instructions for an application you can launch.

## Start with a name

`*.jpg` matches names such as `photo.jpg` and `PHOTO.JPG`. Prefixing the mask with `|` excludes those names instead: `|*.jpg` matches `notes.txt`, but not `photo.jpg`.

`*` matches zero or more characters and `?` matches one Unicode scalar. ASCII letters are case-insensitive; other Unicode characters compare literally. A native Windows name that cannot be represented without loss produces an unknown name-dependent result rather than disappearing silently.

## Add a size condition

`*.jpg;>1mb` requires both a JPG name and a logical size strictly larger than 1048576 bytes. A file exactly that size does not pass.

The legacy suffixes `kb`, `mb`, `gb` and `tb` use powers of 1024. They do not mean decimal SI units. This first implementation always compares **logical bytes**, not allocated or recoverable storage.

## Combine names and exclusions

`*.jpg;*.png;>1mb;|draft*` means:

- JPG or PNG;
- logical size greater than 1 MiB;
- name not starting with `draft`.

Positive name masks are OR-ed. Every exclusion and every size condition must also pass. Filtering leaves the full index unchanged, so changing the expression will not require a future scanner to read the same tree again.

## Handle unknowns and errors

A file with an unknown logical size cannot be declared larger or smaller than a threshold. The evaluator returns `Unknown` unless another condition rules it out conclusively. For example, a non-JPG name fails `*.jpg;>1mb` even when its size is unknown.

Parsing is all-or-nothing. A consumer must keep the previous valid filter when a new expression is invalid. Empty conditions, paths, unsupported size units, fractional thresholds and overflow are rejected. Date, tag and attribute filters are outside this subset.

The full grammar, bounds and API semantics live in the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). [Development and checks](./development.md) explains how to run the tests that exercise it.
