# Core contract

The library has five modules: `index`, `filter`, `scan`, `synthetic` and `treemap`. None reads a file, starts a process, opens a network connection or writes a cache. The input is metadata supplied by the caller. The separate desktop binary displays the synthetic producer in an egui/eframe 0.35.0 window; there is no filesystem scanner.

## Metadata index

`Index` stores a flat, append-only list. Each entry has an opaque ID, a parent, one native name, kind, optional logical/allocated byte counts, optional file identity and a pending/complete/partial/inaccessible/stale state. IDs belong to one index snapshot; they are not persistent identifiers and must not be carried across snapshots. A scanner/UI transport must validate the owning scan generation before passing IDs to an index.

An entry can only be appended below an existing directory. Its name is one component, not an absolute path: empty names, `.`/`..`, NUL, separators, colons and names longer than 255 UTF-16 units (the Win32 maximum component length) are rejected. Native Windows names that are not valid Unicode remain intact. The index does not duplicate full paths or expose mutable entries.

`summary()` returns:

| Field | Meaning |
|-|-|
| `files` | Visible file entries, hardlinks included |
| `known_logical_bytes` | Sum of every visible file entry with a known logical size |
| `unique_logical_bytes` | Logical bytes charged once per file identity |
| `shared_logical_bytes` | Logical bytes of later hardlink entries that were not charged again |
| `unknown_size_files` | Files whose logical size is unknown |
| `complete` | Every entry is `Complete`; unknown sizes also clear it |

The byte sum alone is never a claim of completeness. Zero-byte files and files with unknown sizes remain distinct. Overflow returns an error instead of wrapping or saturating.

Logical bytes count each visible hardlink entry in `known_logical_bytes`. `unique_logical_bytes` charges a file identity at the first entry that has that identity and a known logical size; later links only add to `shared_logical_bytes`. A link whose size is unknown does not consume the charge for its identity. Identity-less files, such as on a filesystem without stable file IDs, are counted individually. Consequently `known_logical_bytes == unique_logical_bytes + shared_logical_bytes` when no sum overflows. None of these values is physical, allocated or recoverable space, and directory-provided sizes are not added again to the file total.

The index maintains these totals as entries are appended, so `summary()` does not walk the entries. Keeping them current costs one set entry per charged file identity and a count of incomplete entries.

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

The desktop binary uses this result to highlight matches, dim non-matches and distinguish unknown decisions without changing geometry or totals. Directories keep the strongest descendant result as context (Yes, then Unknown, then No). Invalid input keeps the last valid highlight. Filtered geometry with separately labelled totals is not implemented.

## Scanner contract

A producer emits a `ScanEvent` tagged with a `ScanGeneration`. Node keys are opaque `NodeKey` values, local to that generation. The payloads are plain data: no `EntryId`, `io::Error`, closures or shared references, so a future elevated NTFS worker can send the same events over IPC while the UI process keeps the index.

Each `ScanRecord` carries a key, an optional parent key, one native name, kind, optional logical/allocated sizes and an optional file identity. The root record has no parent and must be a directory. The producer must emit a parent before its children. A record whose parent is unknown is counted as an unknown-parent diagnostic and is not buffered for later resolution; the same applies to a duplicate key. The consumer never panics on malformed records and never silently discards them.

The record contract has no attribute field yet. A producer must still avoid recursing into reparse points and hydrating cloud placeholders. Typed attributes will be added with the Win32 producer instead of guessing Win32 bit meanings here.

Events are:

| Event | Meaning |
|-|-|
| `Batch` | Records, capped by entry count and estimated payload bytes |
| `DirectoryDone(key)` | That directory's immediate children were enumerated; not its complete subtree |
| `DirectoryFailed { key, code }` | The directory could not be enumerated; `code` is producer-defined |
| `Progress` | Coalescable activity counters; no total or percentage |
| `Terminal(Finished/Cancelled/Failed(code))` | Exactly one, after all producer work |

Batches are capped by `MAX_BATCH_RECORDS` and `MAX_BATCH_BYTES`, and the channel is a bounded `sync_channel`. The only exception is a single record larger than `MAX_BATCH_BYTES` on its own: an empty batch accepts it instead of dropping it. Producers send essential events with a retry loop that checks an `AtomicBool` cancellation flag and stops on receiver disconnect. Progress may be dropped when the queue is full because it is coalescable. The terminal event is sent once even after cancellation and blocks while the queue is full. A consumer that requests cancellation must keep draining until it observes that terminal or a disconnect, or drop the `Receiver`; otherwise a producer blocked in that send never returns.

`ScanGeneration` only separates sessions. It does not authenticate a producer and is not a security control. Nothing here defines an IPC transport: before one exists, the consumer must enforce a total node and byte budget for the whole scan, a maximum component length of 255 UTF-16 units and an authenticated, restricted channel. Per-batch caps bound one message, not the index.

`ScanReducer` owns one fresh index for its generation and maps node keys to entry IDs. Events from another generation are counted as stale and ignored before their keys are resolved. A drain call applies a bounded amount of work, normally one whole batch plus control events, so a UI can call it once per frame. The reducer tracks pending directories. On `Cancelled`, `Failed` or a disconnect without a terminal, every still-pending directory becomes `Partial`; on `Finished` the same transition exposes a producer that omitted a directory outcome. A channel that closes before any terminal is reported as `missing_terminal`.

`ScanDiagnostics` counts stale generations, unknown parents, duplicate keys, invalid records, unknown directory outcomes, duplicate directory outcomes, events after the terminal, partial directories, directory failures and a missing terminal. The last directory failure code is retained for a status line. These counters are protocol evidence, not filesystem errors.

## Synthetic producer

`synthetic` implements a deterministic producer over the same channel and reducer. A `SyntheticSpec` fixes the root name, directory/file counts, inaccessible directories, hardlink pairs, unknown-size files and progress interval. SplitMix64 derives every choice from the seed, so the same seed produces the same records, names, sizes and identities.

The producer emits the root first, then parents before children, reports inaccessible directories with a synthetic code that is not a Win32 error, and shares file identities for hardlink pairs. It sends a final progress summary before its terminal, even when the periodic interval is zero. It builds its plan in memory and does not touch a disk. It is a contract and layout fixture, not the filesystem scanner.

## Treemap layout

`treemap::layout` is pure. It takes an index, a subtree root, a rectangle and `LayoutParams { min_pixel_area, header_height }`, and returns placed cells. It does not measure text, draw or depend on egui.

Weights are integer logical bytes. The first entry with a file identity and a known size charges those bytes; later links have zero weight, so a hardlink is never charged twice. Unknown logical sizes get no area and set `has_unknown`. Known zero-byte entries also get no cell. Every placed node reports `known_sum` and `has_unknown`, and the root reports the subtree totals.

Siblings whose expected area is below `min_pixel_area` are grouped before squarification into one `NodeRef::Aggregate { parent }` cell. The aggregate keeps the member entry IDs, so it remains navigable, and its weight is the sum of its members. A directory cell shows a header when its height is at least twice `header_height`; otherwise the header collapses and the whole rectangle is content, and a `header_height` of zero disables headers. Child rectangles tile the parent's content rectangle, with area proportional to known bytes.

A directory whose subtree holds no charged bytes gets no cell. A folder that only holds unknown sizes, inaccessible content or later hardlink names stays in the index and keeps its unknown flags, but the map offers no area to open. This is the current behavior; whether such folders deserve a navigable zero-area cell is a product decision pending window review.

Both the children map and the subtree totals are rebuilt on every call, and hardlink charging uses one entry per identity. That keeps `layout` pure and O(n) per call: a caller that redraws should cache the layout per index version instead of recomputing it every frame.

Squarification accumulates in `f64` and converts to `f32` at the boundary. The order is deterministic: size descending, then native name, then entry ID. Layout descends iteratively, only into directories whose content area reaches `min_pixel_area`; a deep tree does not use call-stack recursion. Invalid geometry, invalid parameters, an unknown root and size overflow return typed errors instead of producing bogus rectangles.

`layout_members` opens an aggregate's selected siblings using their original IDs and descendant trees, without unrelated siblings. Members must be nonempty, distinct and immediate children of the supplied parent; invalid selections return `UnknownRoot`. The root cell is the parent context, but its totals cover only the selected members. Further aggregates keep their members and remain navigable. The desktop lowers the grouping threshold in detail views so at least one member can expand on each visit.

Volume composition is not implemented: free and unattributed space need volume capacity and accounting data that this unit does not receive. The layout only tiles the known scanned bytes.

## Desktop consumer

The default `desktop` Cargo feature enables optional eframe for `src/main.rs` and `src/ui/`; the library modules never import it. `cargo test --locked --no-default-features` tests the core without GUI dependencies. There is no separate crate or runtime dependency in the library.

The window starts a fixed, explicitly labelled sample on one thread and drains one complete batch at most plus bounded control events per frame. It keeps the channel bounded and drops the receiver before joining a replaced or closing producer, so a terminal send cannot deadlock shutdown. Cancel sets its flag and visible lifecycle immediately while draining continues. Lifecycle (Scanning, Finished, Cancelled, Failed) is separate from metadata completeness; unknown sizes and inaccessible directories do not turn a finished producer into a partial lifecycle.

Layout is cached by consumed-event version, navigation and viewport rectangle, at most four rebuilds per second while scanning. Hover and selection reuse geometry and fitted labels. Navigation stores native name components and aggregate member names in memory, not snapshot IDs, so it can resolve them against a new sample. If a path has not arrived yet during rescan, the root is shown temporarily without rewriting history.

The window opens in the dark theme unless a saved choice says otherwise; both are fixed palettes, independent of the system preference. View → Dark/Light switches between them in place. The stored choice is applied before the first frame, and eframe keeps the native window hidden until that frame is painted, so a saved light theme does not flash dark. Both menu items are selectable, so AccessKit reports the active one as toggled. Theme colors are resolved only when painting and in egui's visuals; layout parameters, header height and label fitting do not read them, so both themes produce the same rectangles and labels, and switching themes never rebuilds the cached layout. Cached cells store what an entry is (directory depth, file, aggregate, incomplete state), not colors. Some states also carry a mark that does not depend on hue: a dashed inner border for partial entries, a dotted inner border for aggregates at least 24 px on each side, and a dotted header rule when a folder's total is incomplete (`has_unknown`: unknown sizes, or partial or unreadable content below it). Marks are skipped on cells under 8 px. Directories, files, non-matching entries and unknown filter decisions are told apart by fill only. The painter also hatches inaccessible entries and gives stale ones the dashed border, but the current product never shows either: an inaccessible directory has no known bytes and so no cell, and nothing produces the stale state yet. Leaf cells get a 1 px seam in a darker shade of their own fill, omitted on non-matching cells. Hover (1 px), selection (2 px, stronger contrast than hover on every fill) and keyboard focus are outlines, so they never replace the state fill; on cells under 8 px hover and selection are drawn outside the cell. Focus is a two-tone dashed ring outside the control.

Only window geometry and the theme are persisted by eframe in its application-data directory. The theme is one plain string under the `theme` key, written as `v1:dark` or `v1:light`; a missing, unknown, future-version or corrupt value falls back to dark rather than guessing. Egui memory persistence is disabled, so egui's widget and in-app window state is not stored, and names, filters and navigation are never serialized. AccessKit is enabled; map cells expose accessible labels, selection and focus. Backspace goes up, Alt+Left/Right moves through history and Escape cancels a scan; while the highlight field has focus, Escape leaves the field and the scan keeps running. Text editing retains its navigation keys. The status line shows the dataset root and the native path components, and appends the small-item count when the map shows an aggregate's members. In that member view the root cell keeps the parent directory's name and adds the same subset count, so its totals are not mistaken for the whole folder.

An error returned by `eframe::run_native` prints a clear message to stderr and returns exit code 1. There is no native message box, graphics fallback or claim that upstream graphics panics are caught. Real-driver fallback, DPI coverage and full accessibility testing remain separate checks.

## Verification boundary

Integration tests call the public index, filter, scan, synthetic and treemap API with synthetic metadata and fixed expected answers. They cover the filter grammar, unknown values, native Windows names, hierarchy, component length and overflow; scan generations, parent order, batching caps, cancellation under backpressure, a dropped receiver after cancellation, duplicate keys, invalid records, unknown directory outcomes, disconnect without terminal, inaccessible directories, hardlinks, unknown sizes, terminal handling and determinism; and layout containment, sibling disjointness, area conservation, proportionality, headers, aggregation, unknown sizes, determinism and a deep tree.

`tests/navigation.rs` also exercises history across new snapshots, aggregate locations, hit testing, measured-width label fitting, highlight decisions, invalid-filter retention and lifecycle states through the binary's pure logic module. `tests/aggregate_navigation.rs` checks aggregate member layouts through the public treemap API. Unit tests in `src/ui/theme.rs` check both palettes' contrast, each theme overriding a different system preference, and the stored preference read and written through `eframe::Storage`.

Passing these tests does not prove scanner totals, no-cloud-recall behavior, UI responsiveness, graphical compatibility, physical attribution, volume composition or portable distribution.
