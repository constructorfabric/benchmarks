---
title: Testing model
description: The layered testing strategy — unit, integration, and E2E — backed by compile-time architectural lints and local quality gates.
sidebar:
  label: Testing model
  order: 15
---

Gears treats testing as a layered strategy backed by compile-time guarantees, not an afterthought. The goal is fast, local, high-confidence feedback before code reaches CI.

## Layers

- **Unit tests** — exercise individual functions and domain logic in isolation. Because the domain layer contains no HTTP or DB types, it is cheap to unit-test.
- **Integration tests** — exercise a gear's cross-layer wiring and secure-ORM behavior against a real (often in-memory) database, verifying that scoping and transactions behave correctly.
- **End-to-end (E2E) tests** — boot a server with real gears and drive complete request flows through the API gateway, verifying authentication, authorization, and response shapes together.
- **Fuzz tests** — exercise parsers and validation logic (e.g. the OData `$filter` parser) with randomized inputs to surface panics, overflows, and edge cases. Targets live under `tools/fuzz/` and are run with `make fuzz` (30s smoke per target) or `make fuzz-run FUZZ_TARGET=<target> FUZZ_SECONDS=<n>` for longer campaigns.

## Compile-time guarantees complement tests

A large class of mistakes never needs a test because it cannot compile. Custom architectural lints (Dylints) enforce layer boundaries, versioned REST paths, mandatory `OperationBuilder` metadata, and restrictions on raw SQL; strict Clippy rules deny whole categories of async and numeric bugs. This is why the [secure data path](../secure-data-path/) is a structural property.

## Local-first quality loop

The same building blocks compose locally, so cross-gear behavior is testable before a distributed deployment. The framework targets **90%+ coverage** across unit, integration, E2E, performance, and security testing, and the local gates (`make check`, `make all`) run the same checks as CI.

## See also

- [Test your gear](../../build-with-gears/test-your-gear/) — the commands and how-to.
- [Architecture and quality gates](../../contribute/architecture-and-quality-gates/) — the full CI/lint policy.
- [Spec-driven, AI-native development](../../introduction/spec-driven-ai-native/) — why fast local feedback matters more with AI.
