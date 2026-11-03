# Contributing

HogMap has an in-memory core, a read-only Windows metadata scanner and a desktop window that can scan folders or drive roots. No release is available yet. Read the [README](README.md) and [core contract](docs/core.md) before proposing a change.

## What is useful now

- **Ideas and requirements.** What the analyzer has to do to be worth using, and what it must never do.
- **Objections to the plan.** A goal that cannot be met, a size rule that would report the wrong number, an interaction that would not survive a slow disk.
- **Filesystem cases.** Volumes, filesystems and folder shapes that scanners usually get wrong: junctions, hardlinks, compressed or deduplicated folders, cloud placeholders, restricted directories.
- **Corrections to these documents.** A broken link, a claim that is no longer true, a sentence that confuses.

## Open an issue first

Issues are open and are the place to start. Open one before writing a pull request so the scope can be agreed before the implementation. Small documentation fixes need no prior issue.

For a core bug, include the commit, Rust version and a small synthetic test that demonstrates the problem. For a GUI bug, reproduce it in the synthetic sample when possible. For a scanner bug, describe a small, sanitized fixture, filesystem, Windows build and visible status/error code. Never attach a real disk inventory.

## Build and tests

The development toolchain is Rust 1.94.1 with rustfmt and clippy. Run from the repository root:

```text
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --no-default-features
cargo build --locked
```

CI runs those checks on Windows with the exact toolchain. The in-memory modules use std only; the Windows engine uses windows-sys 0.61.2, and optional eframe 0.35.0 belongs to the default `desktop` feature and the binary. Most tests use metadata in memory. Windows scanner tests create and remove small temporary fixtures, including known-length files and hardlinks, and query drive metadata; they do not scan whole volumes. Set build/cache/temporary locations in your own shell session if needed; no machine-specific paths are committed.

To launch the window on Windows, run `cargo run --locked`. It opens the drive/path chooser; it does not automatically scan a disk. Select a listed drive or enter an existing directory such as `C:\` or `C:\scan-fixture`, then choose Start. Drive listing runs in the background with a “Listing drives…” state; reopening uses the session cache, and Refresh drives explicitly updates it. Path and volume checks also run off the UI thread before scanning starts. The path must exist and be a directory; observed symlinks, junctions and reparse ancestors are rejected, and the engine also checks before traversal. It reads directory metadata only and never opens file contents or modifies the selected tree. Use a small local fixture first. Mapped drives and network paths can cause filesystem I/O to a server.

File → Explore synthetic sample is separate and touches no analyzed files. New scan (Ctrl+N) reopens the chooser; Rescan (F5) repeats the selected scan root or sample with fresh navigation, not the zoomed folder. Stop scan requests cancellation immediately while queued events continue to drain; synchronous Windows I/O has no cancellation deadline. Replacement and normal close show “Stopping…” and poll for completion without joining an unfinished thread on the UI thread. Repeated replacements coalesce; a new producer waits for the old one to return rather than accumulating threads. The volume header shows actual queried free/total bytes, or unknown, separately from the logical-byte map. The full status detail is available on hover and in the status area's AccessKit label: completeness, the latest directory failure and the counts-only engine report once it returns. Ordinary inaccessible directories do not leave a red fatal-error message. A known zero-free capacity is displayed as zero, not unknown.

Select with a click, enter a directory or small-item aggregate with a double-click, and navigate with Back/Forward/Up/Home. Backspace goes up, Alt+Left/Right moves through history and Escape cancels a scan; in the highlight field, Escape only leaves the field. The highlight field accepts the [documented filter subset](docs/core.md#name-and-logical-size-filter); invalid input leaves the previous highlight active. It never changes totals or geometry.

Only the window geometry and the chosen theme are saved in eframe's application-data directory; scanned names, paths, filter text and navigation are not persisted. Startup errors returned by eframe print to stderr and exit with code 1; no native message box is implemented. Free/not-yet-scanned map blocks, snapshots, export and alternate view modes are not implemented. Real-provider cloud behavior, graphics-driver fallback and full DPI/accessibility acceptance are still pending; see the engine's [unverified limits](docs/core.md#windows-metadata-engine).

## Measuring the scanner

On Windows, run the headless example against an authorized local directory:

```text
cargo run --release --locked --example scan_bench -- "C:\scan-fixture" --workers 2
```

It reads metadata only and prints counts and timings, never scanned paths or names. Workers may be 1–8; zero or omitting the option uses the provisional storage policy (one for seek-penalty/unknown storage, up to four otherwise). The reported `workers` and `seek_penalty` show the effective choice. The headless consumer blocks for events and drains continuously without layout or painting. `--unthrottled-drain` (also `--unthrottled`) explicitly selects this default. `--ui-cadence` instead uses a 33 ms cadence and approximately 4 ms drain budget for a diagnostic comparison, not a GUI responsiveness test.

Use `--time-limit-seconds 90` for a fixed observation window (1–590 seconds). It requests cancellation at the limit, keeps draining to the single terminal and labels the result `run_partial=1`; it cannot interrupt synchronous Windows I/O. Keep an external hard timeout and memory limit as a safety net. Every five seconds, progress includes cumulative counts and window entries/s and directories/s, so a killed run still has a labelled lower-bound observation. Partial rates count received directory records, including discovered but unenumerated directories; they are not completed-directory rates. Producer wall time includes backpressure. Per-worker accumulated API/wait times overlap; do not add them to infer elapsed scan time. The first received batch includes the root-only publication, not necessarily a populated subtree.

The inline index-memory estimate excludes names, spare capacity, hash tables and other allocations; measure process memory externally. Pending-path charges and peak node counts are separate estimates, not peak working set. Report resource-limit hits, inaccessible/partial directories, protocol diagnostics and cache state with all timing results. `--hold-for-sample` adds a 1500 ms pause after `scan_done=1` for external memory sampling; that pause is excluded from scan timings.

Use one warm-up and one measured run, recording hardware, permissions and cache state; a warm-up does not prove every metadata page stayed cached. Start with a small fixture, use an external timeout and memory limit for larger roots, and never publish a real disk inventory. These measurements do not establish GUI responsiveness, physical attribution or equality with Windows volume-used bytes.

## Documentation site

The wiki lives in `wiki/`, uses the default VitePress theme and is published at <https://yivas.github.io/hogmap/>. It describes implemented behavior separately from planned application features. Keep the [core contract](docs/core.md) as the primary technical reference instead of maintaining competing copies.

The documentation tool versions are pinned in `wiki/package.json` and its lockfile. With the declared Node version and documentation dependencies available, run from the repository root:

```text
npm --prefix wiki run build
npm --prefix wiki run preview
```

Preview binds only to `127.0.0.1:4175`; open <http://127.0.0.1:4175/hogmap/>. Building the site is separate from building the Rust core. The Pages runner restores the locked dependencies with lifecycle scripts disabled, audits them and builds the site; no application release or tag is created by publishing documentation.

## Keep private data out

Issues and pull requests are public and permanent. Do not include:

- credentials, tokens or account identifiers;
- personal paths, user names or machine names;
- disk or file inventories, or folder listings;
- screenshots that show folder or file names.

Describe the shape of the problem instead. The folder structure, the file type, the filesystem and the size range are usually enough.

## Security reports

A vulnerability goes through the private channel described in [SECURITY.md](SECURITY.md), not a public issue.

## Pull request flow

1. Find or open the issue that describes the change.
2. Keep the pull request to one subject, and explain what it changes for a reader of these documents or for a future user of the tool.
3. Keep every claim about status accurate. A tested core is not a tested application, and no version, speed or compatibility claim may run ahead of its evidence.
4. Push to a branch in your fork and open the pull request against `main`.

Reviews are done by the maintainer. No review time is promised, and a pull request can be declined because the design is still open.

## Code of conduct

Participation follows [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
