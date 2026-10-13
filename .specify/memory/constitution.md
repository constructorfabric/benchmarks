<!--
Sync Impact Report
- Version change: (template) → 1.0.0
- Modified principles: all placeholders replaced (initial ratification)
  - I. Component Docs Are the Contract
  - II. Test-First, Acceptance-Driven (NON-NEGOTIABLE)
  - III. Platform Conventions
  - IV. Offline, Lockfile-Bounded Dependencies
  - V. Security & Isolation by Default
  - VI. Streaming & Accounting Integrity
- Added sections: Technical Constraints, Development Workflow & Quality Gates, Governance
- Removed sections: none
- Templates requiring updates: none (templates read the constitution at runtime) ✅
- Deferred TODOs: none
-->
# gears-rust Constitution

## Core Principles

### I. Component Docs Are the Contract

`gears/<gear>/docs/` (PRD.md, DESIGN.md, accepted ADRs, acceptance-criteria.md) is the single
source of truth for a gear's behavior. Routes, request/response bodies, SSE events, status codes,
error codes, database schema, configuration keys and provider wire formats MUST match those
documents exactly. When any other workspace file (sample config, platform docs) disagrees, the
gear's own docs win. Behavior specified only in an ADR is still mandatory.

### II. Test-First, Acceptance-Driven (NON-NEGOTIABLE)

Every acceptance-criteria item MUST be both implemented and covered by an automated test that
passes. Tests are written before or alongside the code they verify; a task is not done until
its test is green under `cargo test`. Tests MUST NOT live in directories reserved for external
acceptance suites (`testing/e2e/suites/<gear>/`).

### III. Platform Conventions

New gears follow the patterns of existing gears: toolkit gear registration (`#[toolkit::gear]`),
a separate `*-sdk` crate for public contracts, ClientHub wiring, secure tenant-scoped database
access via toolkit-db, the canonical error contract via `toolkit-canonical-errors`, OData-style
list filtering/ordering/cursor pagination, and REST routes registered through the API gateway's
operation builder. Plugins are discovered through the types registry by GTS identifiers.

### IV. Offline, Lockfile-Bounded Dependencies

Builds run offline. No dependency may be added that is not already resolved in `Cargo.lock`.
The server MUST build with
`cargo build --bin cf-gears-example-server --no-default-features --features
mini-chat,static-authn,static-authz,single-tenant,static-credstore`.

### V. Security & Isolation by Default

Every resource access is scoped to the caller's tenant and owner; foreign or cross-tenant
access is indistinguishable from "not found". Provider-originated error details are sanitized
before reaching clients. Internal identifiers (provider ids, provider file ids, internal
reservation ids) are never exposed in public responses or events.

### VI. Streaming & Accounting Integrity

Streaming responses are relayed incrementally, never buffered in full. Requests carrying a
known `request_id` replay stored results with no side effects. Quota is reserved before any
outbound provider call and every terminal outcome settles exactly once.

## Technical Constraints

- Language: Rust (toolchain pinned by `rust-toolchain.toml`), edition 2024, `unsafe_code`
  forbidden.
- Persistence: SeaORM with migrations, SQLite and PostgreSQL supported.
- LLM providers are reached only through the outbound API gateway (OAGW).

## Development Workflow & Quality Gates

- Spec-Kit flow: constitution → specify → plan → tasks → implement → converge.
- Quality gates: the server binary builds with the required features; `cargo test -p
  cf-gears-mini-chat -p cf-gears-mini-chat-sdk` passes; every acceptance item maps to at
  least one test.

## Governance

This constitution supersedes ad-hoc practices for this workspace. Amendments are made through
`/speckit-constitution`, documented in the Sync Impact Report, and versioned with semantic
versioning (MAJOR: principle removal/redefinition; MINOR: new principle/section; PATCH:
clarification). Plans MUST include a Constitution Check against these principles; violations
require written justification in the plan's Complexity Tracking table.

**Version**: 1.0.0 | **Ratified**: 2026-10-03 | **Last Amended**: 2026-10-03
