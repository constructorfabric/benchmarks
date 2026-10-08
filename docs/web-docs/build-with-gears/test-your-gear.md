---
title: Test your gear
description: Unit, integration, and end-to-end testing for gears, plus the local quality gates that back them.
sidebar:
  label: Test your gear
  order: 11
---

Gears aims for high test coverage with a layered strategy. This page shows where each test type fits and how to run them locally. For the conceptual model, see [Testing model](../../concepts/testing-model/).

## The three test layers

- **Unit tests** — test individual functions and domain logic in isolation. The domain layer
  has no HTTP or DB types, so it is straightforward to unit test.
- **Integration tests** — test a gear's interactions, including secure ORM behavior against a
  real (often SQLite/in-memory) database and cross-layer wiring.
- **End-to-end (E2E) tests** — boot a server with real gears and exercise complete request
  flows through the API gateway.

## Run tests locally

```sh
make test              # run the test suite
make coverage          # unit + e2e with coverage
make coverage-unit     # unit tests with coverage
make coverage-e2e-local  # e2e tests with coverage
```

The framework targets **90%+ coverage** across unit, integration, E2E, performance, and security testing.

## Local quality gates

Tests are one part of a shift-left quality loop that runs before CI:

```sh
make check   # formatting, linting (Clippy + custom Dylints), tests, security
make all     # full pipeline: build + checks + e2e-local
```

Custom architectural lints enforce layer boundaries, versioned REST paths, `OperationBuilder` metadata, and restrictions on raw SQL — so structural and security mistakes are caught at compile time, not review time.

## Fuzzing (for parsers and validation)

```sh
make fuzz                        # quick smoke test (30s per target)
make fuzz-run FUZZ_TARGET=fuzz_odata_filter FUZZ_SECONDS=600
```

## See also

- [Testing model](../../concepts/testing-model/) — the strategy behind these layers.
- [Architecture and quality gates](../../contribute/architecture-and-quality-gates/) — the
  full CI and lint policy.
