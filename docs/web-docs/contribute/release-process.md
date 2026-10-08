---
title: Release process
description: SemVer rules for crates and public contracts, the deprecation policy, and release-plz automation.
sidebar:
  label: Release process
  order: 9
---

Gears uses per-crate Semantic Versioning and automated releases. This applies to all Rust crates and to public contracts (Rust API, REST, gRPC/proto, CLI).

## SemVer rules

- **PATCH (x.y.Z)** — bugfixes, performance, internal refactors, docs/tests. No public API or behavior change that can break downstream compilation.
- **MINOR (x.Y.z)** — backward-compatible new features and APIs. No breaking changes.
- **MAJOR (X.y.z)** — any breaking change.

**Pre-1.0 policy:** for `0.x.y`, `0.(x+1).0` is breaking and `0.x.(y+1)` is non-breaking — before 1.0, MINOR behaves like MAJOR.

## What counts as breaking (Rust)

Existing downstream code may fail to compile or a stable contract is violated — e.g. removing/renaming a `pub` item, changing signatures or generic bounds, changing public struct/enum layout, removing a `pub` field or relied-upon trait impl, adding a trait method without a default, or tightening bounds/visibility. **If in doubt, treat it as breaking.**

## Public contracts

- **Rust crate API** — governed by the SemVer rules above.
- **REST API** — versioned in the URL (`/v1/…`); breaking changes require a new version and a deprecation period.
- **gRPC / Protobuf** — treat `.proto` as public: never reuse field numbers, don't change field types incompatibly, prefer adding optional fields.
- **CLI** — flags, commands, and output formats are public; breaking changes require MAJOR.

## Deprecation

Prefer deprecation over removal: mark APIs deprecated for at least one MINOR release with migration notes. Removal is breaking and requires MAJOR.

## Automation (release-plz)

- Release PRs are labeled `release-plz`.
- The repository-level `CHANGELOG.md` is the single changelog source.
- ToolKit is released as a unified framework: only `cf-gears-toolkit` produces changelog entries and releases; other `cf-gears-toolkit-*` crates publish without separate entries.

## Required release notes

Every release documents **Added**, **Changed**, **Fixed**, and **Breaking** (with migration steps). If you touched a public surface, justify the version-bump category in the PR description.

## See also

- [Code contribution guide](../code-contribution-guide/) — the PR workflow that precedes a release.
- [CONTRIBUTING.md](https://github.com/constructorfabric/gears-rust/blob/main/CONTRIBUTING.md) — the canonical versioning section.
