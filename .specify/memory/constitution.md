<!--
Sync Impact Report
- Version change: (template) → 1.0.0
- Modified principles: all template placeholders replaced (initial ratification)
  - [PRINCIPLE_1] → I. Contract Fidelity
  - [PRINCIPLE_2] → II. Platform Conventions
  - [PRINCIPLE_3] → III. Test-First, Every Acceptance Criterion Covered (NON-NEGOTIABLE)
  - [PRINCIPLE_4] → IV. Tenant and Owner Isolation, Fail-Closed Authorization
  - [PRINCIPLE_5] → V. Streaming-First and Billing Correctness
  - added VI. P1 Scope Discipline and Simplicity
- Added sections: Technical Constraints, Development Workflow & Quality Gates
- Removed sections: none
- Templates requiring updates: none (plan/spec/tasks templates read the constitution at runtime) ✅
- Follow-up TODOs: none
-->

# Constructor Fabric gears-rust Constitution

## Core Principles

### I. Contract Fidelity

The component's own documents are the source of truth. For the `mini-chat` gear these are
`gears/mini-chat/docs/PRD.md`, `DESIGN.md`, every accepted `ADR/*.md` and
`acceptance-criteria.md`; `docs/api/api.json` is the OpenAPI reference for request and response
schemas.

- Wire formats MUST match the documents exactly: routes and URL prefix, request/response JSON
  (field names, optional-field omission rules), SSE event names, payloads and ordering, canonical
  RFC 9457 `Problem` errors (category, HTTP status, machine-readable reason fields), database
  tables/columns, configuration keys and defaults, and the requests sent to LLM providers.
- When another workspace file disagrees with the gear's documents, the gear's documents win.
- Behavior marked "not implemented", "Future" or "P2+" in an ADR MUST NOT be invented.

Rationale: a black-box acceptance suite checks the HTTP/SSE API, the database and provider
requests; any drift from the contract is a defect.

### II. Platform Conventions

Gears MUST be written the way the existing workspace gears are written: ToolKit gear lifecycle
(`init`/`start`/`stop`, REST registration), ClientHub wiring, types-registry plugin discovery,
Secure ORM scoped entities (`AccessScope`), the shared toolkit-db outbox, the OAGW in-process
proxy client, `toolkit-canonical-errors` and the platform OData pagination.

- No dependency that is absent from `Cargo.lock` may be added; builds run offline.
- SDK crates hold the public contract types (plugin APIs, models, errors); the gear crate holds
  the implementation.

Rationale: consistency with the platform keeps the gear composable in the single server binary.

### III. Test-First, Every Acceptance Criterion Covered (NON-NEGOTIABLE)

- Every item of the feature's acceptance checklist MUST be both implemented and covered by at
  least one automated test that passes.
- Test tasks are written before (or together with) the implementation they verify; a test that
  fails first and then passes is the expected cycle.
- Unit tests live in separate `*_tests.rs` files next to the code under test; HTTP-level
  integration tests exercise the gear through its routes with a mock LLM provider.
- Gear tests MUST NOT be placed in `testing/e2e/suites/mini_chat/` (reserved for the acceptance
  suite).

Rationale: the acceptance checklist is mandatory; untested behavior is assumed broken.

### IV. Tenant and Owner Isolation, Fail-Closed Authorization

- Every data access MUST be authorized through the PDP (PolicyEnforcer) and scoped by the
  compiled `AccessScope` (tenant column + owner column); child resources are reached only through
  an owner-scoped parent chat.
- Foreign, missing and soft-deleted resources are indistinguishable (404). PDP denial is 403;
  PDP evaluation failure is 503 with `Retry-After`.
- Provider identifiers (file ids, vector store ids, response ids, credentials) MUST NEVER appear
  in API responses, SSE payloads or error messages.

Rationale: chat content is owner-only customer data.

### V. Streaming-First and Billing Correctness

- No layer may buffer the full LLM response; events are relayed one by one through bounded
  channels.
- Quota is decided before any outbound provider call (reserve → execute → settle).
- Every terminal path is CAS-guarded on `state = 'running'`; the CAS winner alone settles quota
  and enqueues usage/audit outbox events, all in one database transaction. Replays are
  side-effect-free.

Rationale: users must never be double-charged or able to evade quotas.

### VI. P1 Scope Discipline and Simplicity

Implement the P1 scope the documents describe — no more. Prefer the simplest design that satisfies
the contract; additional abstraction requires a concrete need in the contract.

## Technical Constraints

- Rust toolchain pinned by `rust-toolchain.toml`; Rust 2024 edition, workspace lints.
- Database: SQLite and PostgreSQL through SeaORM / toolkit-db; migrations run on both engines.
- Build gate: `cargo build --bin cf-gears-example-server --no-default-features --features
  mini-chat,static-authn,static-authz,single-tenant,static-credstore` MUST succeed.
- Server configuration for the gear: `config/mini-chat.yaml` keys and defaults follow DESIGN.md
  Appendix B.

## Development Workflow & Quality Gates

- Work follows Spec-Kit: constitution → specify → plan → tasks → implement → converge.
- A change is done only when the build gate passes and `cargo test` for the gear and SDK crates
  passes.
- Converge runs after implementation; any unbuilt spec item is appended as new tasks and
  implemented before the work is declared complete.

## Governance

This constitution supersedes ad-hoc practices for work in this repository. Amendments are made by
editing this file with a Sync Impact Report, bumping the version per semantic versioning (MAJOR:
principle removal/redefinition; MINOR: new principle or section; PATCH: wording). Plans MUST
include a Constitution Check against the principles above; any violation MUST be justified in the
plan's Complexity Tracking table.

**Version**: 1.0.0 | **Ratified**: 2026-10-03 | **Last Amended**: 2026-10-03
