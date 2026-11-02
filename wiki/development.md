# Development and checks

The current code is a library, not an application. It has no `cargo run` entry point. Development checks exercise the public index and filter API with synthetic metadata in memory; they do not scan your files.

## Requirements

Use Rust 1.94.1 with rustfmt and clippy. Windows 11 x64 is the intended product platform, and the Windows CI job also exercises preservation of native file names that are not valid Unicode.

The core currently has no third-party dependencies. The GUI stack has been selected as egui/eframe 0.35.0, but it has not been added or tested as an application yet.

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
- `tests/core.rs` checks their public behavior with fixed expected answers.
- The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md) is the technical reference for implemented behavior.

Entry IDs belong to one index snapshot. A future scanner/UI transport must check the owning scan generation before using them. IPC, privileged NTFS reading and physical attribution are not implemented by this unit.

## Propose a change

Read [CONTRIBUTING](https://github.com/Yivas/hogmap/blob/main/CONTRIBUTING.md) and open an issue to agree the scope before writing a pull request. Small documentation corrections need no prior issue.

A useful core bug report identifies the commit, Rust version and a synthetic reproduction. Do not include personal paths, machine names, credentials, disk listings or screenshots containing file names. Undisclosed vulnerabilities belong in the [private security channel](https://github.com/Yivas/hogmap/security/advisories/new), not in a public issue.
