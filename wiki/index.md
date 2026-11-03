# HogMap

HogMap is a Windows 11 x64 disk-space analyzer in development. Its intended interface is a nested treemap: folders contain their descendants, and rectangle sizes show the selected size metric.

## What exists today

**The desktop window can scan real Windows folders and drive roots read-only. There is no downloadable release or installer yet.** The drive/path chooser lists real drives and also offers a separate synthetic sample generated in memory. The window supports progressive updates, selection, directory and aggregate zoom, navigation history, cancellation and filter highlighting. The Cargo version `0.0.0` is an unpublished development marker.

The engine reads directory metadata, never file contents. The header shows queried volume free/total capacity, or unknown, separately from map bytes. The status tooltip and its accessible label show completeness, directory failures and the engine's fallback/skip counts once scanning returns. Passing tests does not establish scan speed, graphical compatibility, physical attribution or portable distribution. The [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md) describes the implemented behavior.

## Find what you need

- [Understand sizes](./sizes.md): why logical bytes, physical attribution and Windows used/free space differ.
- [Work with filters](./filters.md): examples from the implemented name and logical-size subset.
- [Run a scan or sample and develop the core](./development.md): requirements, checks and contribution links.

## What the application will do

The current prototype combines a nested logical-byte treemap with the Windows directory scanner. Free/not-yet-scanned map blocks, snapshots, export and alternate view modes are not implemented. A separate privileged NTFS engine and a portable ZIP remain later work, after the prototype has been checked.

There is no delete, move or clean action, telemetry or account. The scanner skips observed reparse/cloud/recall entries rather than following them. Real cloud-provider behavior and concurrent path replacement remain unverified; metadata-only intent is not a guarantee against provider fetch. Selected network paths or mapped drives can cause Windows filesystem I/O to a server. The window is not elevated; privileged NTFS work and its isolated worker are not implemented.

The current implementation and its limits are documented in the [core contract](https://github.com/Yivas/hogmap/blob/main/docs/core.md). The renderer draws only known unique logical bytes, not free or unattributed volume space. [Development and checks](./development.md#run-the-window) explains how to start a scan or sample and read its colors.

## Support and security

Use [GitHub Issues](https://github.com/Yivas/hogmap/issues) for proposals or a reproducible core bug. A small synthetic example is enough; do not attach personal paths, disk inventories or screenshots containing file names.

Report undisclosed vulnerabilities through the [private security channel](https://github.com/Yivas/hogmap/security/advisories/new), following the [security policy](https://github.com/Yivas/hogmap/blob/main/SECURITY.md).
