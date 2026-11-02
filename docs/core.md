# Core contract

The current library has two modules, `index` and `filter`. Neither reads a file, starts a process, opens a network connection or writes a cache. The input is metadata supplied by the caller. There is no scanner or GUI yet.

## Metadata index

`Index` stores a flat, append-only list. Each entry has an opaque ID, a parent, one native name, kind, optional logical/allocated byte counts, optional file identity and a pending/complete/inaccessible/stale state. IDs belong to one index snapshot; they are not persistent identifiers and must not be carried across snapshots. A scanner/UI transport must validate the owning scan generation before passing IDs to an index.

An entry can only be appended below an existing directory. Its name is one component, not an absolute path: empty names, `.`/`..`, NUL, separators and colons are rejected. Native Windows names that are not valid Unicode remain intact. The index does not duplicate full paths or expose mutable entries.

`summary()` returns the number of file entries, the sum of known logical bytes, the number of files with unknown logical sizes and whether all entries are complete. The byte sum alone is never a claim of completeness. Zero-byte files and files with unknown sizes remain distinct. Overflow returns an error instead of wrapping or saturating.

Logical bytes count each visible hardlink entry. File identity is stored separately, but physical attribution and deduplication are not implemented. In particular, the core never sums logical bytes and calls them allocated or recoverable space. Directory-provided sizes are not added again to the file total.

## Name and logical-size filter

`Filter::parse` either creates a complete valid filter or returns a typed error. A consumer must keep its previous valid filter when parsing fails. `evaluate` returns `Yes`, `No` or `Unknown` for one file; directories are context for matching descendants, not independent file matches. Filtering never changes the index or decides what gets scanned.

| Expression | Meaning |
|-|-|
| Empty | All files |
| `*.jpg` | Whole file name ending in `.jpg` |
| `|*.jpg` | Exclude those names |
| `>100mb` | Logical size strictly greater than 104857600 bytes |
| `*.jpg;>1mb` | A JPG with logical size strictly greater than 1048576 bytes |
| `*.jpg;*.png;>1mb;|draft*` | JPG or PNG, larger than 1 MiB, excluding names starting with `draft` |

Positive name masks are OR-ed. Every exclusion and every size condition must also pass. `*` matches zero or more characters, `?` exactly one Unicode scalar. ASCII letters are case-insensitive; other Unicode characters compare literally. This is not a claim of Windows-wide Unicode case folding. If a native name cannot be represented without loss, a name-dependent result is `Unknown`. A conclusive failing size or name condition still returns `No`.

Size conditions accept `<` or `>`, a non-negative decimal integer and `b`, `kb`, `mb`, `gb` or `tb` (case-insensitive). These legacy suffixes use powers of 1024; they do not mean decimal SI units. Comparisons are strict. The metric is always **logical bytes** in this first unit, not an implicit choice between logical and physical size. Unknown size returns `Unknown` when the other conditions do not rule the file out.

The parser accepts outer whitespace on each condition. It rejects empty conditions between separators, missing/unknown units, overflow, fractional/negative thresholds, paths, tag/attribute/date operators and unsupported syntax. An expression is limited to 4096 UTF-8 bytes and 64 conditions. It does not use regular expressions or expand filesystem paths. Literal name characters other than the reserved operators retain their literal meaning.

The future GUI will use this same result for two distinct operations: highlighting while preserving full geometry, or constructing filtered geometry with separately labelled totals. Neither operation is part of the current library.

## Verification boundary

Integration tests call the public parser, evaluator and index API with synthetic metadata and fixed expected answers. They cover the examples above, invalid/oversized input, unknown values, native Windows names, hierarchy rejection, logical hardlinks and overflow. Passing these tests does not prove scanner totals, no-cloud-recall behavior, UI responsiveness, physical attribution or portable distribution.
