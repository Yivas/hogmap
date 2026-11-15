# Filters

Use the expression field on a scanned tree or the labelled synthetic sample. It works on cached metadata and never rescans. Choose **Highlight** to dim non-matches without changing geometry or totals, or **Filter** to remove non-matching files from the map. The mode selector and Ctrl+Shift+F switch between them; Ctrl+F focuses the expression. Neither the mode nor expression is saved.

Filter keeps undecided files visible in their distinct tone. Its total line separates **Filtered matches**, **Undecided** known bytes and unknown-size count, and **Full snapshot** bytes. Undecided files are not matches. Totals cover the snapshot, even when zoomed. Filter hides volume/free/residual cells: excluding files does not turn their space into free space. A file whose size is unknown has no invented map area.

## File and folder masks

`*.jpg` matches `photo.jpg` and `PHOTO.JPG`. `?` matches exactly one Unicode scalar and `*` matches zero or more. Masks match the whole name, not a substring. ASCII letters are case-insensitive; other characters compare literally, without claiming Windows-wide Unicode folding.

A leading backslash selects folder components: `\temp` matches files anywhere beneath a folder named temp, including nested descendants. `\build*` matches a folder component starting with build. The snapshot root also counts as a folder component. This is not a path: `\temp\nested` is invalid. A file named temp does not itself match `\temp`.

Prefix a mask with `|` to exclude it: `|*.tmp` excludes temporary names, and `|\temp` excludes files beneath temp folders.

## Logical and allocated sizes

`*.jpg;s>1mb` selects JPGs with listed logical size strictly greater than 1048576 bytes. A file exactly that size does not pass. Bare `>1mb` means the same as `s>1mb`.

`d<16k` compares observed allocated size, independently of the map's Logical/Allocated choice. Unknown allocation stays undecided. Allocation is not physical attribution or recoverable space.

Use a non-negative integer and `b`, `k`/`kb`, `m`/`mb`, `g`/`gb` or `t`/`tb`. Suffixes are case-insensitive and use powers of 1024, not decimal SI. Fractions, missing units and `<=`/`>=` are invalid.

## Creation, modification and access age

- `c>2d`: created more than two days ago.
- `m<10s`: modified less than ten seconds ago.
- `a<1h`: accessed less than one hour ago.

Duration units are `s` seconds, `m` minutes, `h` hours, `d` days and `w` weeks, with an integer threshold. Comparisons are strict. The reference instant is fixed when the expression is applied; reapply it to advance the clock. Missing, zero or future timestamps stay undecided. Filesystem access times can be delayed or disabled; an access age is an observation, not proof that someone opened the file.

## Attributes

`:attr:hidden` and `+:attr:hidden` require the hidden flag; `-:attr:hidden` forbids it. Every attribute condition must pass.

| Attribute | Example |
|-|-|
| File kind | `:attr:file` |
| Directory kind | `:attr:dir` |
| System | `:attr:system` |
| Hidden | `-:attr:hidden` |
| Compressed | `:attr:compressed` |
| Encrypted | `:attr:encrypted` |
| Offline | `-:attr:offline` |
| Sparse | `:attr:sparse` |
| Alternate data streams | `:attr:ads` |

The map counts files, while directories provide context. Consequently `:attr:dir` alone has no file matches; use a folder mask to select files inside a folder. Unknown flags remain undecided even when forbidden. Ordinary enumeration does not inspect streams, so ADS presence is unknown unless supplied in memory; the filter does not trigger extra queries or content reads. Skipped offline/cloud entries retain their existing safety behavior.

## File classes

`:class:images` expands into extension masks. `-:class:images` excludes those masks. Built-in classes are:

| Class | Extensions |
|-|-|
| images | jpg, jpeg, png, gif, webp |
| audio | mp3, wav, flac |
| video | mp4, mkv, avi |
| documents | txt, pdf, docx, md |
| archives | zip, 7z, tar, gz |

The core also accepts an in-memory user class table, including replacement definitions. There is no class editor or class-based coloring in the window yet. An unknown class name is an error, not an empty class.

## Temporary tags

The in-memory tag model supports four bits: `:1`/`:red`, `:2`/`:yellow`, `:3`/`:green`, and `:4`/`:blue`. `:tag:` means any tag; `-:tag:` means untagged. `+:red` requires red and `-:red` forbids it. Descendants inherit their nearest folder assignment unless explicitly overridden; zero clears inherited tags.

The tagging UI is not implemented. Ordinary scans start untagged; these conditions are usable with tags supplied through the core API, not through a hidden keyboard action. Nothing is persisted.

## Combining conditions

Separate conditions with `;`. Positive file masks, folder masks, class masks and positive tags form **one OR group**. There is no folder-over-file precedence. All exclusions, negative tags, sizes, ages and attributes are AND conditions. Without positive alternatives, the OR group starts true.

- `*.jpg;*.png;s>1mb;|draft*`: JPG or PNG, larger than 1 MiB, excluding draft names.
- `\temp;*.jpg`: files beneath temp **or** JPGs anywhere in the snapshot.
- `*.jpg;:red;s>1mb;-:blue`: JPG or red-tagged, larger than 1 MiB, with blue forbidden.
- `:class:archives;m>2w;-:attr:system`: archives modified more than two weeks ago, excluding system files.

## Unknowns and errors

An unavailable input is undecided, not false or zero. Another conclusive failing AND condition can still reject the file. A known matching positive alternative can establish the OR group even when another alternative is unknown. Native names that cannot be represented losslessly stay undecided when a name-dependent decision cannot otherwise be established.

Parsing is all-or-nothing. An error keeps the previous valid filter active and shows a correction message. Expressions are limited to **4096 UTF-8 bytes and 64 conditions**. Empty conditions (`*.jpg;;>1mb`), missing units (`m<10`), malformed durations, unknown attributes/classes/tags, overflow and unsupported paths are rejected. Fuzzy keywords and regular expressions are not supported.

The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md#name-and-logical-size-filter) specifies the APIs, grammar, cached decisions and apparent-weight accounting. [Development and checks](./development.md) separates headless coverage from native acceptance.
