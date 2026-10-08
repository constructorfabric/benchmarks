---
title: Add or change a gear
description: Conventions for contributing a new gear or changing an existing one — naming, layering, specs, tests, and the guidelines to follow.
sidebar:
  label: Add or change a gear
  order: 5
---

Adding functionality to the platform usually means adding or changing a gear. Beyond the general [code contribution guide](../code-contribution-guide/), gears have specific conventions.

## Naming and structure

- Gear directories under `gears/` must use **kebab-case** (validated by `tools/scripts/validate_gear_names.py` and enforced in CI).
- Follow the two-crate-plus-server structure and the DDD-light layering described in [Gear anatomy](../../build-with-gears/gear-anatomy/): a public SDK crate, an implementation crate with separated `domain/`, `api/`, and `infra/` layers, and a server crate.
- Prefer **soft-deletion** for entities; provide hard-deletion with retention routines.

## Start from specs for larger features

For anything beyond a small change, begin with the specification templates (PRD, DESIGN, ADR, FEATURE, UPSTREAM_REQS) — see [Spec-driven workflow](../spec-driven-workflow/).

## Follow the guidelines

When implementing, follow the project guidelines:

- **Rust** — [RUST.md](https://github.com/constructorfabric/gears-rust/blob/main/guidelines/DNA/languages/RUST.md).
- **REST APIs** — [API.md](https://github.com/constructorfabric/gears-rust/blob/main/guidelines/DNA/REST/API.md) and [STATUS_CODES.md](https://github.com/constructorfabric/gears-rust/blob/main/guidelines/DNA/REST/STATUS_CODES.md).
- **ToolKit architecture & invariants** — [toolkit_unified_system/README.md](https://github.com/constructorfabric/gears-rust/blob/main/docs/toolkit_unified_system/README.md).
- **Security** — [SECURITY.md](https://github.com/constructorfabric/gears-rust/blob/main/SECURITY.md) and [secure coding guidelines](https://github.com/constructorfabric/gears-rust/blob/main/guidelines/SECURITY.md).

## Tests

Always include unit tests for new code, integration tests for gear interactions, and E2E tests for complete request flows. See [Test your gear](../../build-with-gears/test-your-gear/) and [Testing model](../../concepts/testing-model/).

## See also

- [Build your first gear](../../build-with-gears/your-first-gear/) — the end-to-end walkthrough.
- [Architecture and quality gates](../architecture-and-quality-gates/) — what CI enforces.
