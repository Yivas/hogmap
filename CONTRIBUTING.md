# Contributing

HogMap has an early in-memory core and tests, but no application window, scanner or release yet. Read the [README](README.md) and [core contract](docs/core.md) before proposing a change.

## What is useful now

- **Ideas and requirements.** What the analyzer has to do to be worth using, and what it must never do.
- **Objections to the plan.** A goal that cannot be met, a size rule that would report the wrong number, an interaction that would not survive a slow disk.
- **Filesystem cases.** Volumes, filesystems and folder shapes that scanners usually get wrong: junctions, hardlinks, compressed or deduplicated folders, cloud placeholders, restricted directories.
- **Corrections to these documents.** A broken link, a claim that is no longer true, a sentence that confuses.

## Open an issue first

Issues are open and are the place to start. Open one before writing a pull request so the scope can be agreed before the implementation. Small documentation fixes need no prior issue.

For a core bug, include the commit, Rust version and a small synthetic test that demonstrates the problem. Scanner and GUI reports need an application build, which does not exist yet. Never attach a real disk inventory.

## Build and tests

The development toolchain is Rust 1.94.1 with rustfmt and clippy. Run from the repository root:

```text
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

CI runs those checks on Windows with the exact toolchain. The current core has no third-party dependencies. Tests use synthetic metadata in memory, not files on your disks. Set build/cache/temporary locations in your own shell session if needed; no machine-specific paths are committed. There is no `cargo run` entry point yet.

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
