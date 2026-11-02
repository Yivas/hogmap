# HogMap

A planned disk-space analyzer for Windows 11 that draws a scanned folder or volume as a nested treemap, so the folders and files holding the space are visible at a glance.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Status](#status) · [What it will do](#what-it-will-do) · [Roadmap](#roadmap) · [Contributing](#contributing-and-support) · [License](#license)

## Status

HogMap is in early development. The repository contains an in-memory metadata index and a name/logical-size filter, with tests on Windows. There is no application window, filesystem scanner, installer or release yet. The `0.0.0` Cargo package is an unpublished development marker, not a downloadable version.

The core performs no filesystem or network operations. Its [contract](docs/core.md) describes the implemented subset and its limits; [CONTRIBUTING.md](CONTRIBUTING.md#build-and-tests) lists the development checks. The analyzer capabilities below are still goals, not features you can use today. No scan-speed, memory-use or GUI-compatibility claim has been established.

## The problem

Windows reports free space per volume, and File Explorer lists one folder level at a time. Neither answers the question that sends people looking for a disk analyzer: which folder, and which files inside it, are using the space. Answering it by hand means opening folder after folder and reading properties on the way down.

HogMap is aimed at that question. One scan of a folder or a whole volume, and a map where the size of every rectangle corresponds to what that folder or file occupies, so the answer is visible instead of assembled.

## What it will do

- **Nested treemap.** Each folder is a rectangle that contains its files and subfolders, sized by the selected metric, with labels and borders that stay readable as depth increases. The layout targets the dense, classic look of SpaceSniffer rather than a dashboard of cards and charts; HogMap is not affiliated with that project, and none of its icons, text or assets are reused.
- **Read-only analysis.** The first version is meant to only look: select, navigate, filter, copy a path, open the containing folder, read properties. No delete, move or clean action is planned for it.
- **Portable ZIP.** One archive that will run without an installer, a service, a driver or an account.
- **No network, no telemetry.** The application is meant to work fully offline and to collect nothing about the machine or the scan.
- **Two scan engines.** A compatible engine that walks any chosen folder on any supported filesystem, and a fast NTFS engine that reads volume metadata for a whole drive. The NTFS engine needs administrator rights, so only an isolated worker process is meant to be elevated; the window keeps normal user rights.
- **Sizes that mean what they say.** Logical size, on-disk size attributable to each entry, and the used and free space Windows reports will be kept apart. Hardlinks will not be counted twice, reparse points will not be followed recursively, and cloud placeholders will never be downloaded in order to measure them.
- **Navigation while scanning.** Folders appear as they are examined, cancel stops the visible work at once, and the map does not reshuffle itself on every update.

## Scope and limits

The target is Windows 11 x64 on the desktop and nothing else: no other operating system, no server, no plugin system, no database, no recovery of deleted files. It is meant to read metadata only and never open file contents, and it will not delete, move or clean anything, which also means it will not free space for you. Until a runnable prototype exists, the analyzer capabilities above stay goals.

## Roadmap

Three steps, in order, each one usable before the next begins.

1. **Prototype and compatible engine.** A window that already looks and behaves like the intended layout, driven by a synthetic dataset, plus the compatible scanner for a chosen folder and the tests that go with both.
2. **NTFS engine.** Volume metadata reading through an evaluated library or a narrow implementation, with the size rules, hardlink handling, fallback to the compatible engine and the isolated elevated worker.
3. **Polish and portable ZIP.** The blocking, navigation and DPI problems that real use exposes, then the archive.

Incremental rescans and advanced options come after those steps, and only if real use justifies the complexity they add.

## Platform and stack

Windows 11 x64, as a desktop application. The selected stack is Rust 1.94.1 with egui/eframe 0.35.0 and custom drawing of rectangles and text. The first core unit uses only the standard library; GUI dependencies will be added with the window, not before they have a consumer.

## Privacy and security

The application is planned to run with no network access, no telemetry and no account. It reads file metadata only, never file contents, does not follow reparse points recursively and does not download cloud placeholders. Elevation is confined to a separate worker process with a restricted channel, while the window keeps normal user rights.

Security reports go through the private channel described in [SECURITY.md](SECURITY.md).

## Contributing and support

The project is open source and collaborative, and issues are welcome now: ideas, requirements, objections to the plan and reproducible core bugs. Scanner or GUI reports need an application build, which does not exist yet.

Pull requests are welcome too, with one condition. Open an issue first and settle the change before writing it. Build, test and formatting commands are listed in [CONTRIBUTING.md](CONTRIBUTING.md).

[CONTRIBUTING.md](CONTRIBUTING.md) has the details, [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) covers how people are expected to behave, and [SECURITY.md](SECURITY.md) explains where a vulnerability report goes instead of a public issue.

## License

MIT, copyright Yivas. The full text is in [LICENSE](LICENSE).

HogMap is an independent project and reuses no icons, text or assets from other disk analyzers.
