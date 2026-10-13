<!--
Sync Impact Report
- Version change: (template, unversioned) → 1.0.0
- Modified principles: all template placeholders replaced
  - [PRINCIPLE_1_NAME] → I. Contract-First (Gear Docs Are Source of Truth)
  - [PRINCIPLE_2_NAME] → II. Gear Architecture Conformance
  - [PRINCIPLE_3_NAME] → III. Test-First (NON-NEGOTIABLE)
  - [PRINCIPLE_4_NAME] → IV. Black-Box Verifiability
  - [PRINCIPLE_5_NAME] → V. Security, Tenancy & Simplicity
- Added sections: Technical Constraints; Development Workflow & Quality Gates
- Removed sections: none
- Templates requiring updates: none (plan/spec/tasks templates read the constitution at runtime) ✅
- Follow-up TODOs: none
-->

# Constructor Fabric gears-rust Constitution

## Core Principles

### I. Contract-First (Gear Docs Are Source of Truth)

Each gear's behaviour is defined by its own documentation set (`PRD.md`, `DESIGN.md`, accepted
`ADR/`s and `acceptance-criteria.md` under `gears/<gear>/docs/`). Implementations MUST match the
routes, request/response bodies, status codes, error codes, SSE events, database schema,
configuration keys/defaults and outbound wire formats specified there. When any other workspace
file disagrees with a gear's docs, the gear's docs win. Every acceptance-criteria item is
mandatory.

Rationale: gears are consumed black-box by other gears, clients and acceptance suites; drift from
the documented contract is a defect even if the code "works".

### II. Gear Architecture Conformance

A gear MUST follow the platform's ToolKit conventions: a public `*-sdk` crate holding the
transport-agnostic contract (models, client trait, errors), and a gear crate with
`api`/`domain`/`infra` layering, registered via the `#[toolkit::gear]` machinery, wired through
`ClientHub`, configured via typed config with documented defaults, and exposing REST through the
`OperationBuilder`/OpenAPI path with canonical (RFC 9457 Problem) errors. Existing working gears
(oagw, credstore, types-registry, resolvers) are the reference for idioms.

Rationale: uniform structure lets the runtime compose gears into one binary and keeps them
maintainable.

### III. Test-First (NON-NEGOTIABLE)

Automated tests MUST be written for every acceptance-criteria item, and test tasks MUST precede
their implementation tasks. Tests MUST pass before a task is considered complete. Unit tests cover
pure domain logic (validation, quota math, state machines); integration tests exercise the HTTP
surface, the database and outbound provider requests using in-process servers and mock providers.

Rationale: the acceptance checklist explicitly requires each item to be implemented AND tested.

### IV. Black-Box Verifiability

All observable behaviour (HTTP/SSE responses, persisted rows, provider requests) MUST be
deterministic given configuration and inputs, and MUST be reachable through the server binary
built with the documented feature set. No behaviour may depend on hidden test-only switches.

Rationale: acceptance is checked black-box against the debug server build.

### V. Security, Tenancy & Simplicity

Every request MUST be authenticated via the platform security context and authorized via the
AuthZ resolver (PEP) with tenant/owner scoping applied to all data access. Secrets MUST never be
logged or returned. Prefer the simplest design that satisfies the contract (YAGNI); do not add
dependencies absent from `Cargo.lock`.

Rationale: multi-tenant isolation is a hard requirement; the build environment is offline.

## Technical Constraints

- Language/toolchain: Rust, pinned by `rust-toolchain.toml`; workspace lints apply.
- Offline builds: only crates already in `Cargo.lock` may be used.
- Persistence via SeaORM through `toolkit-db` (SQLite and Postgres supported).
- Outbound LLM traffic goes through OAGW to OpenAI-compatible providers per DESIGN.md wire format.
- The example server MUST build with
  `--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`.

## Development Workflow & Quality Gates

- Spec-Kit flow: constitution → specify → plan → tasks → implement → converge (repeat until
  nothing unbuilt remains).
- Gates before completion: `cargo build` of the server with the feature set above succeeds;
  `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` passes; every acceptance item maps
  to at least one passing test.
- Own tests MUST NOT be placed under `testing/e2e/suites/mini_chat/`.

## Governance

This constitution supersedes ad-hoc practice for work in this repository. Amendments require an
updated Sync Impact Report, a semantic version bump (MAJOR: principle removal/redefinition;
MINOR: new principle/section; PATCH: clarification) and review of dependent Spec-Kit artifacts.
Plans MUST include a Constitution Check gate; any violation MUST be justified in the plan's
Complexity Tracking table. Runtime development guidance lives in `AGENTS.md` and `guidelines/`.

**Version**: 1.0.0 | **Ratified**: 2026-10-03 | **Last Amended**: 2026-10-03
