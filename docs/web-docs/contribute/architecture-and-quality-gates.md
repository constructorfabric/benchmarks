---
title: Architecture and quality gates
description: The formatting, linting, testing, security, and fuzzing checks — enforced locally and in CI — that every contribution must pass.
sidebar:
  label: Architecture and quality gates
  order: 6
---

Gears enforces architecture and quality with automated gates that run locally and in CI. Run them before pushing.

## The check suite

```bash
# Formatting, linting, tests, and security
make check                       # Linux/Mac
python tools/scripts/ci.py check # Windows

# Full pipeline (build + checks + e2e-local)
make all                         # Linux/Mac
python tools/scripts/ci.py all   # Windows
```

CI workflows may be skipped for PRs that only touch `*.md` files or `docs/**` due to path filters.

## Architectural lints

Structural rules are enforced at compile time by custom **Dylints** and strict Clippy policy, so violations fail the build rather than relying on review:

- layer boundaries (domain isolation, DTO placement, no `api`/`infra` leakage into `domain`);
- versioned REST paths and mandatory `OperationBuilder` metadata;
- no raw SQL outside migrations;
- denied async/numeric footguns (e.g. `await_holding_lock`, `async_yields_async`).

## Coverage

Aim for high coverage across unit, integration, and E2E tests (the framework targets 90%+):

```bash
make coverage            # unit + e2e with coverage
make coverage-unit
make coverage-e2e-local
```

## Fuzzing

Before submitting changes to parsers or validation logic, run fuzzing:

```bash
make fuzz                                         # 30s smoke per target
python tools/scripts/ci.py fuzz --seconds 300     # 5 min per target
make fuzz-run FUZZ_TARGET=fuzz_odata_filter FUZZ_SECONDS=600
```

See [tools/fuzz/README.md](https://github.com/constructorfabric/gears-rust/blob/main/tools/fuzz/README.md).

## Supply-chain policy

Dependency risk is checked with `cargo-deny` (licenses, bans, advisories), pinned toolchain, and committed lockfiles — see [Compliance and FIPS](../../concepts/compliance-and-fips/).

## See also

- [Testing model](../../concepts/testing-model/) — the strategy behind these gates.
- [Test your gear](../../build-with-gears/test-your-gear/) — the same commands from a builder's view.
