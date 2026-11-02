# Security policy

## Supported versions

| Version line | Supported |
|-|-|
| No release yet | Not applicable |

Nothing has been released, so no version line is supported. Reports about the current core can identify a commit and a synthetic reproduction. The first application release will define its own supported line here.

## Scope and planned model

HogMap is the Windows 11 x64 disk-space analyzer described in the [README](README.md). The current core only processes metadata supplied in memory; it has no scanner, GUI, network or process-launching code. The requirements below also govern the future application. Core tests do not establish that the future scanner or privileged worker meets them.

- **Read-only toward the analyzed volume.** The first version has no delete, move or clean operation. A defect that would let a product feature remove or modify user data is in scope precisely because no such feature is supposed to exist.
- **No network access, no telemetry.** The application is meant to work fully offline. An outbound connection, or anything that collects data about the machine or the scan, is a defect.
- **No service, no driver, no background process left behind.** Closing the application is meant to end everything it started.
- **Elevation limited to one worker.** The NTFS engine needs administrator rights, so a separate worker process is meant to be elevated while the window stays at normal user rights. A path that gives the window those rights, or that widens what the worker accepts over its channel, is in scope.
- **Filesystem metadata as untrusted input.** Malformed volume records, truncated reads, unusual name encodings and volumes changing during a scan have to end in a visible partial result, not in a crash or an out-of-bounds read.
- **Metadata only.** File contents are never read, reparse points are not followed recursively, and cloud placeholders are never downloaded.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for this repository:

<https://github.com/Yivas/hogmap/security/advisories/new>

Do not open a public issue for an undisclosed vulnerability. Include as much of the following as you can:

- the affected version or commit, and how you obtained the build;
- Windows version and build, filesystem and drive type, and whether the elevated worker was in use;
- steps to reproduce, expected behaviour and observed behaviour;
- the impact you believe it has, and any workaround you found.

## What not to send

Remove or redact, before attaching anything:

- credentials, tokens and account identifiers;
- personal paths, user names and machine names;
- disk or file inventories and folder listings;
- screenshots that show folder or file names;
- volume serial numbers and partition layout details you consider private.

The structure that triggers the problem is enough. A full listing of your disk is not, and it cannot be unpublished once it is attached.

## After the report

Reports are read by the maintainer, and what happens next depends on what the report shows. There is no response-time guarantee, no CVE assignment process, no embargo schedule and no bug bounty, and none should be inferred from this document. When a fix exists, the commit and the release notes will say which versions were affected.
