# Development and checks

The code contains in-memory metadata modules, a Windows directory scanner and a desktop consumer. Most tests use synthetic metadata; Windows engine tests also create and remove small temporary fixtures and query drive metadata. They do not scan whole volumes.

## Requirements

Use Rust 1.94.1 with rustfmt and clippy. Windows 11 x64 is the intended product platform, and the Windows CI job also exercises preservation of native file names that are not valid Unicode.

The in-memory modules use only std; the Windows engine uses windows-sys 0.61.2. The default `desktop` feature adds optional egui/eframe 0.35.0 with glow, default fonts, AccessKit and window persistence for the binary. A reduced development profile keeps builds smaller by limiting debug information, disabling incremental caches and turning off dependency debug information; release builds are compiled separately.

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

[GitHub Actions](https://github.com/Yivas/hogmap/actions) runs the checks on Windows. CI also fails if `allow(unsafe_code)` appears outside the audited `src/scan_win.rs`, `src/volume.rs` and `src/name_arena.rs` boundaries. A successful test job is not evidence of faithful native-window behavior, whole-volume accuracy or a portable ZIP.

## Run the window

Run `cargo run --locked` from the repository root on Windows. The window opens a chooser and does not automatically scan a disk. Select a real listed drive or enter an existing directory such as `C:\` or `C:\scan-fixture`, then choose Start. Drive listing runs in a background task with “Listing drives…” shown meanwhile. Reopening uses the session cache; Refresh drives explicitly updates it. Path and volume checks run off the UI thread before the scanner starts; the header's label and free/total bytes come from a fresh drive query, not the chooser's cached list, so the first Start and a later rescan show current capacity. Invalid or non-directory paths are rejected, as are observed reparse points/junctions and reparse ancestors. The engine reads directory metadata, never file contents, and does not modify the selected tree. Begin with a small local fixture. Selecting a mapped drive or network path can cause Windows filesystem I/O to a server.

File → Explore synthetic sample instead streams generated metadata in memory; its header explicitly says sample and has no volume capacity. Ctrl+N opens the chooser again. Rescan or F5 repeats the selected scan root or sample, not the zoomed folder, and clears navigation and selection. Stop scan or Escape outside the highlight field requests cancellation immediately while the consumer keeps draining queued events. Escape or the chooser's Close while a path is being checked cancels that preparation. An in-flight synchronous Windows call has no cancellation deadline. Replacement and close show “Stopping…” while polling for completion, without joining unfinished threads on the UI thread. A close with work still running waits up to two seconds for it to finish; if a thread is still blocked after that, the window closes and detaches it instead of blocking shutdown. Repeated requests coalesce; a replacement waits for the old producer to return instead of accumulating threads.

The leading status reports producer activity separately from metadata completeness. A finished scan can still have incomplete metadata; the sample deliberately includes unknown sizes and inaccessible directories. Failures show readable safety/resource messages; other Windows error codes appear as `error N`, or `code N` once for a drive-listing failure. The full status detail is available on hover and in the status area's accessible label: protocol diagnostics, the latest directory failure and, once the engine returns, fallback directories, reparse/cloud skips, invalid names, pending-memory limit hits, maximum queue/local-frontier depth and cancellation wakeups. Ordinary directory failures stay in that detail, not as a permanent red fatal-error message.

Click to select, double-click a folder or small-item aggregate to open it, and use Back/Forward/Up/Home to navigate. Backspace goes up, Alt+Left/Right moves through history and Escape leaves the highlight field without cancelling. View → Dark and View → Light switch the theme straight away, and the choice is remembered for the next start. The status line shows the dataset root and the path to the current folder, plus the small-item count when the map is open inside an aggregate. The highlight field accepts [name and logical-size filters](./filters.md); it dims non-matches without changing map area or totals, and invalid input keeps the previous valid highlight. The match counter appears only while the field has non-whitespace text.

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

The map uses logical bytes charged once per file identity, not physical or recoverable space. The header displays queried global filesystem free/total capacity separately, or unknown if unavailable. Capacity uses the Actual fields of GetDiskSpaceInformationW (available from Windows 10 build 17763), never its caller-quota fields. The existing chooser/header hover and accessible details show the provider, query age and a reason when capacity is unknown; exact capture time and provenance are retained in memory, not saved. A local non-elevated Windows 11 metadata test exercises the real provider; quota-enabled/pool environments and atomic reconciliation with a scan are not verified. See the [capacity semantics](https://github.com/Yivas/hogmap/blob/main/docs/core.md#windows-metadata-engine). Free-space and not-yet-scanned blocks are absent from the map; snapshots, export and alternate view modes are not implemented.

Only the window geometry and the chosen theme are saved in eframe's application-data directory. Names, paths, filter text and navigation are not saved, and egui's own widget and window memory is not saved either. A stored theme value that is missing, unknown or unreadable falls back to dark. AccessKit is enabled, but a full screen-reader and DPI acceptance pass is still pending. If eframe returns a startup error, HogMap prints it to stderr and exits with code 1; there is no native message box or automatic graphics-backend fallback.

## Know the boundaries

- `src/index.rs` owns the flat metadata snapshot and entry IDs.
- `src/filter.rs` parses and evaluates name/logical-size conditions.
- `src/scan.rs` defines the generation-tagged scanner contract, bounded batches and the reducer that builds an index.
- `src/scan_win.rs` enumerates Windows directory metadata with bounded workers; `src/volume.rs` queries drive roots, labels, capacities, filesystem and whether the device reports a seek penalty. The scanner currently uses four workers on both SSD and HDD (the HDD value is provisional), and on NTFS hard disks it visits pending folders in MFT record order to reduce head movement. `examples/scan_bench.rs` measures a scan from the command line without printing paths or names; see CONTRIBUTING.
- `src/synthetic.rs` streams a separate deterministic in-memory tree through that contract.
- `src/treemap.rs` computes squarified rectangles, headers and navigable aggregates without drawing.
- `src/main.rs` and `src/ui/` own the desktop consumer, cached rendering, navigation and UI strings.
- `tests/core.rs`, `tests/scan.rs`, `tests/treemap.rs`, `tests/aggregate_navigation.rs` and `tests/navigation.rs` check public behavior and pure UI logic with fixed expected answers. Unit tests in `src/ui/` check location text, volume-choice mapping, path validation, scan-code messages, theme contrast and the stored theme preference. `tests/scan_win.rs` exercises small real Windows fixtures, not whole drives.
- The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md) is the technical reference for implemented behavior.

Checked u32 entry-ID and name-arena ceilings return `CapacityExceeded`; these [representation limits](https://github.com/Yivas/hogmap/blob/main/docs/core.md#metadata-index) are not a safe process-memory budget, and allocation exhaustion can occur sooner.

Entry IDs belong to one index snapshot. The scanner contract checks the owning generation before applying records; the reducer never resolves an unknown parent later. IPC, privileged NTFS reading and physical attribution are not implemented. Real cloud providers, long-path I/O and path-replacement races still need authorized environment-specific checks; see the [engine limits](https://github.com/Yivas/hogmap/blob/main/docs/core.md#windows-metadata-engine). Layout rebuilds are limited to four per second while scanning and cached otherwise; hover does not rebuild the tree.

## Propose a change

Read [CONTRIBUTING](https://github.com/Yivas/hogmap/blob/main/CONTRIBUTING.md) and open an issue to agree the scope before writing a pull request. Small documentation corrections need no prior issue.

A useful core bug report identifies the commit, Rust version and a synthetic reproduction. Do not include personal paths, machine names, credentials, disk listings or screenshots containing file names. Undisclosed vulnerabilities belong in the [private security channel](https://github.com/Yivas/hogmap/security/advisories/new), not in a public issue.
