# HogMap

A disk-space analyzer in development for Windows 11 that draws a scanned folder or drive as a nested treemap of known logical bytes, or of observed allocated bytes chosen in View.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Documentation](https://yivas.github.io/hogmap/) · [Status](#status) · [What it will do](#what-it-will-do) · [Roadmap](#roadmap) · [Contributing](#contributing-and-support) · [License](#license)

## Status

HogMap is in early development. The desktop window can scan real Windows folders and drive roots read-only, with progressive updates, selection, directory/aggregate zoom, history, cancellation and filter highlighting. The chooser lists real drives in aligned free/total rows and also offers a separate, labelled synthetic sample. New scan, Rescan and Stop have visible names. More/Less detail adjusts cell density through the toolbar, View menu or Ctrl++/Ctrl+-. A single status line shows progress, elapsed time and a brief selection; full paths and accounting remain in hover and accessible text. There is no installer or release yet. The `0.0.0` Cargo package is an unpublished development marker, not a downloadable version.

The Windows engine reads directory metadata, not file contents. Window scans retain origin-bound observations in memory and check conflicting hardlink identities asynchronously after Finished. At an unzoomed whole-volume root, green **Free space** is independently available in either metric, provisionally during scanning and from eligible authoritative observations after Finished. Gray **Not yet scanned · estimate** appears only during scanning; it is a changing residual, not measured inaccessible space. **Reserved by Windows** retains its allocated/Finished eligibility. The map is not a complete disk partition: estimated difference is status text only, with no inaccessible-space block or blank area standing for it. The volume header keeps its separately queried free/total capacity; unavailable capacity stays unknown. The status tooltip and its accessible label include fallback and skip counts after scanning. Drive listing and path/volume checks run in background tasks; reopening the chooser uses the session drive cache, with an explicit Refresh drives action. The [core contract](docs/core.md) describes the implemented subset and its limits; [CONTRIBUTING.md](CONTRIBUTING.md#build-and-tests) lists development checks and usage. Build and launch with `cargo run --locked` on Windows using the toolchain described there. Local synthetic headless frame measurements are documented in the wiki; they are not native scan-speed, GPU-use or GUI-compatibility guarantees.

## The problem

Windows reports free space per volume, and File Explorer lists one folder level at a time. Neither answers the question that sends people looking for a disk analyzer: which folder, and which files inside it, are using the space. Answering it by hand means opening folder after folder and reading properties on the way down.

HogMap is aimed at that question. One scan of a folder or a whole volume, and a map where each rectangle shows a clearly labelled size metric, so the answer is visible instead of assembled.

## What it will do

- **Nested treemap.** Each folder is a rectangle that contains its files and subfolders, sized by the selected metric, with labels and borders that stay readable as depth increases. The layout targets the dense, classic look of SpaceSniffer rather than a dashboard of cards and charts; HogMap is not affiliated with that project, and none of its icons, text or assets are reused.
- **Read-only analysis.** The first version is meant to only look: select, navigate, filter, copy a path, open the containing folder, read properties. No delete, move or clean action is planned for it.
- **Portable ZIP.** One archive that will run without an installer, a service, a driver or an account.
- **No network, no telemetry.** The application is meant to work fully offline and to collect nothing about the machine or the scan.
- **Two scan engines.** A compatible engine that walks any chosen folder on any supported filesystem, and a fast NTFS engine that reads volume metadata for a whole drive. The NTFS engine needs administrator rights, so only an isolated worker process is meant to be elevated; the window keeps normal user rights.
- **Sizes that mean what they say.** Logical size, on-disk size attributable to each entry, and the used and free space Windows reports will be kept apart. Apparent size counts each hardlink name and can exceed disk capacity; unique allocated bytes per identity separately determine the estimated difference. Reparse points will not be followed recursively, and cloud placeholders will never be downloaded in order to measure them.
- **Navigation while scanning.** Folders appear as they are examined, cancel stops the visible work at once, and the map does not reshuffle itself on every update.

## Scope and limits

The target is Windows 11 x64 on the desktop and nothing else: no other operating system, no server, no plugin system, no database, no recovery of deleted files. It is meant to read metadata only and never open file contents, and it will not delete, move or clean anything, which also means it will not free space for you. The total is labelled **Apparent size** for the selected metric: `known_bytes + resolution_delta_bytes`. The charged record uses its authoritative size after conflict resolution; aliases retain their listed sizes. This is not full per-name authoritative correction and carries no error bound. Apparent size can exceed disk capacity. The separate estimated difference is `total - free - storage_reserve - allocated_unique`, using identity-deduplicated allocated bytes after authoritative corrections. Status, tooltips and About show the repeated hard-link name total (`allocated_apparent - allocated_unique`) separately: the map counts each listed name, so map + free + reserve + estimated difference can exceed disk capacity by those repeated-name bytes. Cell sizes and areas use that same apparent total: every known name contributes, including hardlink aliases, and volume cells are normalized beside the displayed tree. Logical tree bytes and volume-capacity bytes are comparable only in order of magnitude, not physical attribution. Logical is the default metric; View → Allocated size chooses observed allocation. Neither is physical or recoverable space, and highlight size conditions stay logical. Folder and zoomed views have no volume cells. Running capacity comes from the fresh drive query at scan start, without claiming identity evidence; final Free requires a proven unchanged whole-volume NTFS origin. Known Free remains independently drawable when total capacity is unknown, but the gray estimate requires known total/free/tree bytes. Unknown reserve is omitted from that estimate explicitly; known zero remains known. Unreadable directories and skips remain coverage diagnostics, not measured causes of the estimated difference. No inaccessible-space block, snapshots, export or alternate view mode is implemented. Their toolbar tools and the Snapshots tab are not present; disabled states are reserved for real actions temporarily unavailable. The one-line status keeps lifecycle, counts, size, completeness and elapsed time; detailed accounting and location stay in the tooltip and accessible text. Ordinary status updates are coalesced to about 4 Hz, while terminal states, errors and explicit interactions stay immediate. Scan ingestion uses separate roughly 67 ms ticks; map geometry and meshes rebuild no faster than 4 Hz during ordinary growth, with adaptive backoff. Explicit navigation, metric/detail changes and resizing redraw immediately. Headless geometry and accessibility tests do not establish native visual acceptance.

## Roadmap

Three steps, in order, each one usable before the next begins.

1. **Prototype and compatible engine.** A window that already looks and behaves like the intended layout, driven by a synthetic dataset, plus the compatible scanner for a chosen folder and the tests that go with both.
2. **NTFS engine.** Volume metadata reading through an evaluated library or a narrow implementation, with the size rules, hardlink handling, fallback to the compatible engine and the isolated elevated worker.
3. **Polish and portable ZIP.** The blocking, navigation and DPI problems that real use exposes, then the archive.

Incremental rescans and advanced options come after those steps, and only if real use justifies the complexity they add.

## Platform and stack

Windows 11 x64, as a desktop application. The selected stack is Rust 1.94.1 with egui/eframe 0.35.0 and custom drawing of rectangles and text. The in-memory modules use only the standard library; the Windows metadata engine uses windows-sys 0.61.2. The default `desktop` feature adds egui/eframe 0.35.0 with glow, default fonts, AccessKit and window persistence. `cargo test --locked --no-default-features` keeps core checks free of GUI dependencies.

## Privacy and security

The application has no telemetry, account or network-discovery feature. Selecting a mapped drive or network path can still cause Windows filesystem I/O to its server. The scanner reads directory metadata only, never file contents, and skips reparse points and cloud/recall entries rather than traversing them. Real provider behavior and concurrent path-replacement races remain unverified; this is not a guarantee of no cloud fetch. Only window geometry and theme are saved locally. The window runs without elevation; a separate privileged NTFS engine and restricted IPC remain planned, not implemented.

Security reports go through the private channel described in [SECURITY.md](SECURITY.md).

## Contributing and support

The project is open source and collaborative, and issues are welcome now: ideas, requirements, objections to the plan and reproducible core bugs. Reports should identify the source commit and use the synthetic sample or a small, sanitized filesystem fixture, never a real disk inventory.

Pull requests are welcome too, with one condition. Open an issue first and settle the change before writing it. Build, test and formatting commands are listed in [CONTRIBUTING.md](CONTRIBUTING.md).

[CONTRIBUTING.md](CONTRIBUTING.md) has the details, [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) covers how people are expected to behave, and [SECURITY.md](SECURITY.md) explains where a vulnerability report goes instead of a public issue.

## License

MIT, copyright Yivas. The full text is in [LICENSE](LICENSE).

HogMap is an independent project and reuses no icons, text or assets from other disk analyzers.
