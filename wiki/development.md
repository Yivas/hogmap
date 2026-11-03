# Development and checks

The current code is a library, not an application. It has no `cargo run` entry point and no window. Development checks exercise the public index, filter, scanner-contract, synthetic-producer and layout APIs with metadata in memory; they do not scan your files.

## Requirements

Use Rust 1.94.1 with rustfmt and clippy. Windows 11 x64 is the intended product platform, and the Windows CI job also exercises preservation of native file names that are not valid Unicode.

The core currently has no third-party dependencies. The GUI stack has been selected as egui/eframe 0.35.0, but it has not been added or tested as an application yet. A reduced development profile keeps builds smaller by limiting debug information, disabling incremental caches and turning off dependency debug information; release builds are compiled separately.

## Run the checks

From the source repository root:

```text
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Configure build/cache/temporary locations in your own shell session when needed. Do not change machine-wide settings or fill a disk to reproduce a test. Fixtures should be small and synthetic; a million-entry memory benchmark is not a reason to create a million real files.

[GitHub Actions](https://github.com/Yivas/hogmap/actions) runs the checks on Windows. A successful core job is not evidence of a working scanner, faithful interface or portable ZIP.

## Know the boundaries

- `src/index.rs` owns the flat metadata snapshot and entry IDs.
- `src/filter.rs` parses and evaluates name/logical-size conditions.
- `src/scan.rs` defines the generation-tagged scanner contract, bounded batches and the reducer that builds an index.
- `src/synthetic.rs` streams a deterministic in-memory tree through that contract.
- `src/treemap.rs` computes squarified rectangles, headers and navigable aggregates without drawing.
- `tests/core.rs`, `tests/scan.rs` and `tests/treemap.rs` check their public behavior with fixed expected answers.
- The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md) is the technical reference for implemented behavior.

Entry IDs belong to one index snapshot. The scanner contract checks the owning generation before applying records; the reducer never resolves an unknown parent later. IPC, privileged NTFS reading, a real filesystem scanner, a renderer and physical attribution are not implemented by this unit.

## Propose a change

Read [CONTRIBUTING](https://github.com/Yivas/hogmap/blob/main/CONTRIBUTING.md) and open an issue to agree the scope before writing a pull request. Small documentation corrections need no prior issue.

A useful core bug report identifies the commit, Rust version and a synthetic reproduction. Do not include personal paths, machine names, credentials, disk listings or screenshots containing file names. Undisclosed vulnerabilities belong in the [private security channel](https://github.com/Yivas/hogmap/security/advisories/new), not in a public issue.
