# Contributing

HogMap is pre-implementation: there is no code, no build, no test suite and no release. What exists is the plan described in the [README](README.md), and that plan is what there is to work on.

## What is useful now

- **Ideas and requirements.** What the analyzer has to do to be worth using, and what it must never do.
- **Objections to the plan.** A goal that cannot be met, a size rule that would report the wrong number, an interaction that would not survive a slow disk.
- **Filesystem cases.** Volumes, filesystems and folder shapes that scanners usually get wrong: junctions, hardlinks, compressed or deduplicated folders, cloud placeholders, restricted directories.
- **Corrections to these documents.** A broken link, a claim that is no longer true, a sentence that confuses.

## Open an issue first

Issues are open and are the place to start. Open one before writing a pull request: with no code, there is no interface to change yet, and disagreeing in an issue is cheaper than disagreeing in a diff. Small documentation fixes need no prior issue.

Bug reports only make sense once there is something to run. The bug template asks for the build, the Windows version, the filesystem and the reproduction steps, and it will stay unanswered until a prototype exists.

## Build and tests

No build, test or formatting commands exist, so none are documented here. The first code will bring them, and this guide will list the exact commands before the first pull request that depends on them is reviewed.

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
3. Keep every claim about status accurate. Pre-implementation stays pre-implementation until a prototype exists, and no version, speed or compatibility claim may run ahead of the code.
4. Push to a branch in your fork and open the pull request against `main`.

Reviews are done by the maintainer. No review time is promised, and a pull request can be declined because the design is still open.

## Code of conduct

Participation follows [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
