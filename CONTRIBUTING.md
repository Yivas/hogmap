# Contributing

HogMap has an in-memory core and a desktop window over a synthetic sample, but no filesystem scanner or release yet. Read the [README](README.md) and [core contract](docs/core.md) before proposing a change.

## What is useful now

- **Ideas and requirements.** What the analyzer has to do to be worth using, and what it must never do.
- **Objections to the plan.** A goal that cannot be met, a size rule that would report the wrong number, an interaction that would not survive a slow disk.
- **Filesystem cases.** Volumes, filesystems and folder shapes that scanners usually get wrong: junctions, hardlinks, compressed or deduplicated folders, cloud placeholders, restricted directories.
- **Corrections to these documents.** A broken link, a claim that is no longer true, a sentence that confuses.

## Open an issue first

Issues are open and are the place to start. Open one before writing a pull request so the scope can be agreed before the implementation. Small documentation fixes need no prior issue.

For a core bug, include the commit, Rust version and a small synthetic test that demonstrates the problem. For a GUI bug, identify the commit and reproduce it in the synthetic sample. A filesystem scanner is not available yet. Never attach a real disk inventory.

## Build and tests

The development toolchain is Rust 1.94.1 with rustfmt and clippy. Run from the repository root:

```text
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --no-default-features
cargo build --locked
```

CI runs those checks on Windows with the exact toolchain. The library stays std-only; optional eframe 0.35.0 belongs to the default `desktop` feature and the binary. Tests use synthetic metadata in memory, not files on your disks. Set build/cache/temporary locations in your own shell session if needed; no machine-specific paths are committed.

To launch the window on Windows, run `cargo run --locked`. It starts a labelled synthetic sample, not a scan of your files. Select with a click, enter a directory or small-item aggregate with a double-click, and navigate with Back/Forward/Up/Home. Backspace goes up, Alt+Left/Right moves through history and Escape cancels a scan; in the highlight field, Escape only leaves the field. The highlight field accepts the [documented filter subset](docs/core.md#name-and-logical-size-filter); invalid input leaves the previous highlight active. It never changes totals or geometry.

Only the window geometry and the chosen theme are saved in eframe's application-data directory; scanned names, paths, filter text and navigation are not persisted. Startup errors returned by eframe print to stderr and exit with code 1; no native message box is implemented. Real-filesystem scanning, free-space composition, driver fallback and full DPI/accessibility acceptance are still pending.

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
