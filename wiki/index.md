# HogMap

HogMap is a Windows 11 x64 disk-space analyzer in development. Its intended interface is a nested treemap: folders contain their descendants, and rectangle sizes show the selected size metric.

## What exists today

The repository contains an in-memory metadata index, a name/logical-size filter, a scanner-to-UI contract with a deterministic synthetic producer, and a pure treemap layout, with tests on Windows. **There is no application window, filesystem scanner or downloadable release yet.** The Cargo version `0.0.0` is an unpublished development marker.

The current core does not inspect your disks: the synthetic producer creates metadata in memory. Passing its tests does not establish scan speed, graphical compatibility, physical attribution or portable distribution. The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md) describes the implemented behavior.

## Find what you need

- [Understand sizes](./sizes.md): why logical bytes, physical attribution and Windows used/free space differ.
- [Work with filters](./filters.md): examples from the implemented name and logical-size subset.
- [Develop the core](./development.md): requirements, checks and contribution links.

## What the application will do

The first runnable prototype will combine a dense, nested treemap with a compatible folder scanner. Navigation, filtering and cancellation must remain usable during a scan. A separate NTFS engine and a portable ZIP come later, after the prototype has been checked.

The application is intended to read metadata only: no delete, move or clean action, no file-content reads, no cloud downloads, no telemetry or network access. The window will not be elevated; any privileged NTFS work belongs in an isolated worker with explicit authorization.

These are requirements, not a claim that the future scanner already meets them. The current implementation and its limits are documented in the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). The treemap layout already computes rectangles, but no renderer draws them yet.

## Support and security

Use [GitHub Issues](https://github.com/Yivas/hogmap/issues) for proposals or a reproducible core bug. A small synthetic example is enough; do not attach personal paths, disk inventories or screenshots containing file names.

Report undisclosed vulnerabilities through the [private security channel](https://github.com/Yivas/hogmap/security/advisories/new), following the [security policy](https://github.com/Yivas/hogmap/blob/main/SECURITY.md).
