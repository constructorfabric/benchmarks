<!--
Sync Impact Report
- Version change: template (unversioned) → 1.0.0
- Modified principles: all template placeholders replaced
  - [PRINCIPLE_1_NAME] → I. Contract-First (Gear Docs Are the Source of Truth)
  - [PRINCIPLE_2_NAME] → II. Gear Architecture Conformance
  - [PRINCIPLE_3_NAME] → III. Test-First (NON-NEGOTIABLE)
  - [PRINCIPLE_4_NAME] → IV. Security & Multi-Tenancy
  - [PRINCIPLE_5_NAME] → V. Offline, Lockfile-Bound Dependencies
  - added → VI. Simplicity & Determinism
- Added sections: Technical Constraints; Development Workflow & Quality Gates
- Removed sections: none
- Templates requiring updates: plan-template.md (Constitution Check reads this file at runtime) ✅ no edit needed;
  spec-template.md ✅; tasks-template.md ✅ (tests are mandatory per Principle III — specs MUST request them)
- Follow-up TODOs: none
-->
# Constructor Fabric gears-rust Constitution

## Core Principles

### I. Contract-First (Gear Docs Are the Source of Truth)

- A gear's contract lives in `gears/<gear>/docs/`: `PRD.md`, `DESIGN.md`, the accepted ADRs
  under `ADR/`, and `acceptance-criteria.md`.
- When any other workspace file (sample configs, platform docs, generated OpenAPI) disagrees
  with the gear docs, the gear docs MUST win.
- Routes, request/response bodies, SSE events, status codes, error codes, configuration keys and
  defaults, database schema and outbound provider wire format MUST match the documents exactly.

Rationale: the gear is verified black-box against its documented contract; any drift is a defect.

### II. Gear Architecture Conformance

- Gears MUST follow ToolKit conventions: registration with `#[toolkit::gear(...)]`, DDD-light
  layering (`api/`, `domain/`, `infra/`), a separate SDK crate holding the public contract
  (models, errors, plugin traits), ClientHub registration, REST via `OperationBuilder` with
  OpenAPI, and canonical errors rendered as RFC 9457 Problem responses.
- Platform services (authn/authz resolvers, OAGW, types-registry, credstore) MUST be consumed
  through their SDKs, never re-implemented.

Rationale: uniform gears are composable by the runtime and reviewable by the team.

### III. Test-First (NON-NEGOTIABLE)

- Every item of a gear's `acceptance-criteria.md` MUST be covered by automated tests that pass.
- Tests are written before (or together with, failing first) the implementation they cover.
- Coverage MUST include unit tests for domain logic and integration tests that drive the
  HTTP/SSE surface against an in-process database and a mock OpenAI-compatible provider.
- Feature specifications MUST explicitly request tests so that task generation includes them.

Rationale: acceptance is judged by behavior; untested behavior is unverified behavior.

### IV. Security & Multi-Tenancy

- Tenant and owner isolation MUST be enforced on every resource read and write; foreign
  resources MUST be indistinguishable from missing ones where the contract says so.
- Authorization MUST go through the PEP flow (authz-resolver); scope filters MUST be applied in
  queries, not post-filtered.
- Errors reaching clients MUST be sanitized (no provider bodies, internal ids, or stack traces);
  secrets MUST never be logged.

Rationale: a multi-tenant AI service handles user data and paid provider credentials.

### V. Offline, Lockfile-Bound Dependencies

- Only crates already present in `Cargo.lock` may be used; builds run offline with the toolchain
  pinned in `rust-toolchain.toml`.
- New workspace members MUST be registered in the root `Cargo.toml` and use workspace
  dependency declarations.

Rationale: reproducible builds in air-gapped CI.

### VI. Simplicity & Determinism

- Streaming responses MUST be relayed incrementally, never buffered.
- Idempotent replays, context assembly and truncation MUST be deterministic.
- Build what the spec requires (YAGNI); deferred features stay deferred as their ADRs say.

Rationale: predictable behavior is testable behavior.

## Technical Constraints

- Language/runtime: Rust (edition 2024), tokio, axum, SeaORM (SQLite and PostgreSQL), utoipa.
- The example server `apps/cf-gears-example-server` MUST build with
  `--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`.
- Workspace lints (`[lints] workspace = true`) apply to every crate.

## Development Workflow & Quality Gates

- Spec-Kit flow: constitution → specify → plan → tasks → implement → converge.
- A change is done when it builds, its tests pass, and converge finds no unbuilt work.
- Plans MUST include a Constitution Check that verifies Principles I–VI.

## Governance

- This constitution supersedes ad-hoc practice in this repository; `guidelines/` and
  `docs/toolkit_unified_system/` provide runtime development guidance.
- Amendments require updating this file with a Sync Impact Report and a semantic version bump
  (MAJOR: principle removal/redefinition; MINOR: new principle/section; PATCH: wording).
- Every plan and review MUST verify compliance; any deviation MUST be justified in the plan's
  Complexity Tracking table.

**Version**: 1.0.0 | **Ratified**: 2026-10-03 | **Last Amended**: 2026-10-03
