<!--
Sync Impact Report
- Version change: (template, unversioned) → 1.0.0
- Modified principles: all placeholders replaced (initial ratification)
  - [PRINCIPLE_1_NAME] → I. Spec Documents Are the Source of Truth
  - [PRINCIPLE_2_NAME] → II. Platform Conventions First
  - [PRINCIPLE_3_NAME] → III. Test-First, Acceptance-Driven (NON-NEGOTIABLE)
  - [PRINCIPLE_4_NAME] → IV. Tenant Isolation and Fail-Closed Authorization
  - [PRINCIPLE_5_NAME] → V. Streaming-First Without Buffering
  - Added: VI. Deterministic Quota and Billing
  - Added: VII. P1 Scope Discipline and Simplicity
- Added sections: Build and Runtime Constraints; Development Workflow and Quality Gates
- Removed sections: none
- Templates requiring updates: none (plan/spec/tasks templates read the constitution at runtime) ✅
- Follow-up TODOs: none
-->
# Constructor Fabric gears-rust — mini-chat Constitution

## Core Principles

### I. Spec Documents Are the Source of Truth

The documents in `gears/mini-chat/docs/` (`PRD.md`, `DESIGN.md`, the accepted `ADR/*.md` and
`acceptance-criteria.md`) define the behaviour of the `mini-chat` gear and its SDK crate. The
generated OpenAPI document `docs/api/api.json` pins the wire schemas. When any other workspace
file (for example `config/mini-chat.yaml` or platform documents under `docs/`) disagrees, the
gear documents win. Accepted ADRs override the PRD/DESIGN text they supersede. Every route,
body, status code, SSE event, error reason, database column, configuration key and provider
request MUST match these documents.

Rationale: a hidden black-box acceptance suite checks the gear against these documents.

### II. Platform Conventions First

The gear MUST be built the way the other gears of the workspace are built: ToolKit gear
registration and lifecycle (`init`, `start`, `stop`, REST registration), Secure ORM scoped
entities and migrations, `toolkit-canonical-errors` `Problem` responses, `toolkit-odata`
cursor pagination, types-registry plugin discovery for the model-policy and audit plugins,
the toolkit-db transactional outbox, and OAGW (`ServiceGatewayClientV1`) for every outbound
provider call. No direct HTTP egress to LLM or RAG providers is allowed.

Rationale: platform consistency is what makes the gear composable and testable.

### III. Test-First, Acceptance-Driven (NON-NEGOTIABLE)

Every item of `acceptance-criteria.md` MUST be implemented and covered by automated tests that
pass. Tests are written before or together with the implementation they verify (TDD ordering in
`tasks.md`: test tasks precede the implementation tasks they cover). Own tests MUST NOT be placed
in `testing/e2e/suites/mini_chat/`. A feature is not done until its tests pass.

Rationale: the acceptance checklist explicitly requires each item to be both implemented and
tested.

### IV. Tenant Isolation and Fail-Closed Authorization

Every data access MUST be scoped by the PDP constraints compiled to an `AccessScope`
(tenant + owner). Resources hidden by scope return 404; a PDP deny returns 403; a PDP failure
returns 503 with `Retry-After: 5`. Provider identifiers (file, vector store, response ids)
MUST never appear in API responses, SSE payloads or error messages; provider error text is
sanitized before it reaches a client.

Rationale: chat content is owner-only and multi-tenant.

### V. Streaming-First Without Buffering

Assistant output MUST be relayed as SSE events as soon as each provider event is parsed, through
bounded channels (16–64). Every stream starts with `stream_started` and ends with exactly one
terminal `done` or `error`; terminal events are emitted only after the finalization transaction
commits. Pre-stream failures are JSON `Problem` responses and never open a stream.

Rationale: low latency and a stable, deterministic client contract.

### VI. Deterministic Quota and Billing

Quota is reserved before any provider call, re-checked in the reserve transaction, and settled
exactly once per reserved turn through a CAS on `chat_turns.state = 'running'`. Settlement,
usage-outbox enqueue and audit-outbox enqueue happen in the same transaction. Credit arithmetic
uses integer `credits_micro` with per-component ceiling division. Replay is side-effect free.

Rationale: no credit drift under disconnects, crashes or races.

### VII. P1 Scope Discipline and Simplicity

Only P1 behaviour is implemented. Items the documents mark "Not implemented", "Future" or P2+
(MCP, document summaries, hard purge, chunk caps, image quotas, policy snapshot caching) stay
unimplemented. Prefer the simplest design that satisfies the documents; no speculative
abstractions.

Rationale: scope creep costs time and risks contradicting the documented contract.

## Build and Runtime Constraints

- Offline build: only crates already present in `Cargo.lock` may be used; no new dependencies
  outside the lockfile.
- The SDK crate lives in `gears/mini-chat/mini-chat-sdk/` (package `cf-gears-mini-chat-sdk`,
  library `mini_chat_sdk`), is a workspace member, and the gear depends on it.
- The server MUST build with
  `cargo build --bin cf-gears-example-server --no-default-features --features
  mini-chat,static-authn,static-authz,single-tenant,static-credstore`.
- The example server imports `mini_chat::infra::plugins::{static_audit, static_model_policy}`;
  these modules MUST remain and register the bundled plugins.
- Servers started manually are stopped by recorded pid only (never by command-line matching).

## Development Workflow and Quality Gates

- Work follows Spec-Kit: constitution → specify → plan → tasks → implement → converge.
- Gate 1: `cargo build` of the server with the features above succeeds.
- Gate 2: `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` passes.
- Gate 3: a smoke run of the server with a mini-chat configuration and a mock
  OpenAI-compatible provider exercises the HTTP/SSE contract end to end.
- Every acceptance-criteria item maps to at least one test (traceability kept in `tasks.md`).

## Governance

This constitution supersedes ad-hoc practice for this feature. Amendments are made by editing
this file with a Sync Impact Report and a semantic version bump (MAJOR: principle removal or
redefinition; MINOR: new principle or section; PATCH: wording). Plans and task lists MUST include
a Constitution Check against the principles above; any deviation MUST be justified in the plan's
Complexity Tracking table.

**Version**: 1.0.0 | **Ratified**: 2026-10-03 | **Last Amended**: 2026-10-03
