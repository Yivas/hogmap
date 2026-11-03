# Development and checks

The code contains a std-only library and a desktop window over a labelled synthetic sample. Development checks exercise the public index, filter, scanner-contract, synthetic-producer and layout APIs with metadata in memory; they do not scan your files.

## Requirements

Use Rust 1.94.1 with rustfmt and clippy. Windows 11 x64 is the intended product platform, and the Windows CI job also exercises preservation of native file names that are not valid Unicode.

The library has no third-party dependencies. The default `desktop` feature adds optional egui/eframe 0.35.0 with glow, default fonts, AccessKit and window persistence for the binary. A reduced development profile keeps builds smaller by limiting debug information, disabling incremental caches and turning off dependency debug information; release builds are compiled separately.

## Run the checks

From the source repository root:

```text
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --no-default-features
cargo build --locked
```

Configure build/cache/temporary locations in your own shell session when needed. Do not change machine-wide settings or fill a disk to reproduce a test. Fixtures should be small and synthetic; a million-entry memory benchmark is not a reason to create a million real files.

[GitHub Actions](https://github.com/Yivas/hogmap/actions) runs the checks on Windows. A successful core job is not evidence of a working scanner, faithful interface or portable ZIP.

## Run the window

Run `cargo run --locked` from the repository root on Windows. The window starts a generated sample, not a scan of your disks. File → New scan of synthetic sample starts it again; Cancel scan or Escape, when no text field has focus, stops the current producer. The leading status is the producer lifecycle (Scanning, Finished, Cancelled or Failed), with metadata completeness reported separately. A finished sample deliberately includes unknown sizes and inaccessible directories.

Click to select, double-click a folder or small-item aggregate to open it, and use Back/Forward/Up/Home to navigate. Backspace goes up, Alt+Left/Right moves through history and Escape leaves the highlight field without cancelling. View → Dark and View → Light switch the theme straight away, and the choice is remembered for the next start. The status line shows the dataset root and the path to the current folder, plus the small-item count when the map is open inside an aggregate. The highlight field accepts [name and logical-size filters](./filters.md); it dims non-matches without changing map area or totals, and invalid input keeps the previous valid highlight.

### Map legend

The window opens in the dark theme, or in the theme chosen last time. A saved light theme is applied before the first frame, so it does not flash dark. Both themes draw the same rectangles, headers and labels; only colors differ. Some states also have a mark that does not depend on color; the others are told apart by fill only.

- Brown (dark theme) or tan (light theme): directories, with a name/size header when space permits. Header bands get slightly lighter (dark) or darker (light) with each nesting level, up to four levels. Directory borders are drawn darker than the thin seams between files.
- Blue: files with known logical bytes.
- Lilac: small-item aggregates, with a dotted inner border when the cell is at least 24 px on each side. Each has a count/size label when it fits and a hover tooltip such as “12 small items · 1.2 MiB”; double-click opens those members. Inside that member view the root header keeps the parent folder's name and adds the subset, such as “photos · 12 small items - 1.2 MiB”.
- Gray with a dashed inner border: partial entries, such as a folder whose scan was cancelled before it finished.
- A dotted line under a header: the folder's total is incomplete, because some sizes are unknown or some content below it is partial or could not be read. Its size reads “+ unknown”. Unknown sizes are not zero and have no area of their own.
- Dimmed, near-background gray: non-matching entries while highlighting; olive (dark) or muted beige (light): an unknown filter decision.
- A thin outline follows the pointer and a thicker, stronger outline marks the selection; on cells smaller than 8 px both are drawn just outside the cell. Keyboard focus is a dashed ring with a solid backing, outside the cell.

Marks are skipped on cells smaller than 8 px on either side.

Map labels are chosen to keep a contrast ratio of at least 4.5:1 against their fill in both themes. That checks the palette, not full accessibility.

The map uses logical bytes charged once per file identity, not physical or recoverable space. Free-space green and unattributed-volume gray are absent because volume data does not exist yet.

Only the window geometry and the chosen theme are saved in eframe's application-data directory. Names, paths, filter text and navigation are not saved, and egui's own widget and window memory is not saved either. A stored theme value that is missing, unknown or unreadable falls back to dark. AccessKit is enabled, but a full screen-reader and DPI acceptance pass is still pending. If eframe returns a startup error, HogMap prints it to stderr and exits with code 1; there is no native message box or automatic graphics-backend fallback.

## Know the boundaries

- `src/index.rs` owns the flat metadata snapshot and entry IDs.
- `src/filter.rs` parses and evaluates name/logical-size conditions.
- `src/scan.rs` defines the generation-tagged scanner contract, bounded batches and the reducer that builds an index.
- `src/synthetic.rs` streams a deterministic in-memory tree through that contract.
- `src/treemap.rs` computes squarified rectangles, headers and navigable aggregates without drawing.
- `src/main.rs` and `src/ui/` own the desktop consumer, cached rendering, navigation and UI strings.
- `tests/core.rs`, `tests/scan.rs`, `tests/treemap.rs`, `tests/aggregate_navigation.rs` and `tests/navigation.rs` check public behavior and pure UI logic with fixed expected answers. Unit tests in `src/ui/` check the location text, the contrast of both theme palettes and the stored theme preference.
- The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md) is the technical reference for implemented behavior.

Entry IDs belong to one index snapshot. The scanner contract checks the owning generation before applying records; the reducer never resolves an unknown parent later. IPC, privileged NTFS reading, a real filesystem scanner and physical attribution are not implemented. Layout rebuilds are limited to four per second while scanning and cached otherwise; hover does not rebuild the tree.

## Propose a change

Read [CONTRIBUTING](https://github.com/Yivas/hogmap/blob/main/CONTRIBUTING.md) and open an issue to agree the scope before writing a pull request. Small documentation corrections need no prior issue.

A useful core bug report identifies the commit, Rust version and a synthetic reproduction. Do not include personal paths, machine names, credentials, disk listings or screenshots containing file names. Undisclosed vulnerabilities belong in the [private security channel](https://github.com/Yivas/hogmap/security/advisories/new), not in a public issue.
