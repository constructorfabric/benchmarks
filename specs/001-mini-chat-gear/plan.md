# Implementation Plan: Mini Chat Gear

**Branch**: `001-mini-chat-gear` | **Date**: 2026-10-03 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/001-mini-chat-gear/spec.md`

## Summary

Build the `mini-chat` ToolKit gear (crate `cf-gears-mini-chat`, lib `mini_chat`) and its SDK (`cf-gears-mini-chat-sdk`, lib `mini_chat_sdk`) so that the 19 REST/SSE operations, quota/billing engine, provider adapters (through OAGW), attachments, outbox workers and background jobs behave exactly as `gears/mini-chat/docs` specifies. Approach and decisions: [research.md](./research.md). Data model: [data-model.md](./data-model.md). Contracts: [contracts/](./contracts/).

## Technical Context

**Language/Version**: Rust (edition 2024, toolchain pinned by `rust-toolchain.toml`)

**Primary Dependencies**: toolkit (gear runtime, OperationBuilder, OData extractor), toolkit-db (Secure ORM, migrations, outbox), toolkit-canonical-errors, toolkit-security, authz-resolver-sdk (PolicyEnforcer), authn-resolver-sdk (S2S), oagw-sdk (ServiceGatewayClientV1, SSE parser), types-registry-sdk (plugin discovery), sea-orm 2.0, axum 0.8, utoipa, image 0.25, multer 3

**Storage**: SQLite (primary for tests/e2e) and PostgreSQL via dual-dialect migrations; shared toolkit outbox tables

**Testing**: `cargo test -p cf-gears-mini-chat -p cf-gears-mini-chat-sdk` — unit tests (`*_tests.rs` companions) and integration tests (`tests/*.rs`) against SQLite + fake OAGW gateway; manual server smoke run with a Python mock provider

**Target Platform**: Linux server (in-process gear of `cf-gears-example-server`)

**Project Type**: web-service gear (library crate) + SDK library

**Performance Goals**: streaming relay without buffering (bounded channel 16–64), in-gear TTFT overhead p99 < 50 ms

**Constraints**: offline, lockfile-bound crates; build with `--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`; DB/table names fixed by DESIGN; config keys/defaults per DESIGN Appendix B

**Scale/Scope**: 19 operations, 10 tables, 5 outbox queues, 2 background workers, 2 bundled plugin gears

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Check | Status |
|---|---|---|
| I. Contract-first | Routes/DTOs from `docs/api/api.json` subset; errors per ADR-0004; schema per DESIGN §3.7; config per Appendix B | PASS |
| II. Gear conformance | `#[toolkit::gear]`, api/domain/infra layering, SDK crate with plugin traits + GTS specs, ClientHub, OperationBuilder, canonical errors | PASS |
| III. Test-first | Test tasks precede implementation in every story phase; every acceptance-criteria item mapped to tests (see tasks.md) | PASS |
| IV. Security & multi-tenancy | PEP via PolicyEnforcer + `ensure_owner`; 404 masking; 503 on PDP failure; provider-id sanitization; no secrets logged | PASS |
| V. Offline deps | Only crates already in `Cargo.lock` (image, multer, httpmock-free tests); SDK added to workspace members | PASS |
| VI. Simplicity & determinism | Bounded channel relay; CAS-guarded finalization; deterministic truncation; MCP and other ADR "Future" items excluded | PASS |

Post-design re-check: PASS (no violations; Complexity Tracking empty).

## Project Structure

### Documentation (this feature)

```text
specs/001-mini-chat-gear/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/ (openapi.json, sse-and-outbox.md)
└── tasks.md
```

### Source Code (repository root)

```text
gears/mini-chat/
├── mini-chat-sdk/                 # cf-gears-mini-chat-sdk (lib mini_chat_sdk)
│   └── src/{lib.rs, models.rs, plugin_api.rs, gts.rs, error.rs}
└── mini-chat/                     # cf-gears-mini-chat (lib mini_chat)
    ├── src/
    │   ├── lib.rs, gear.rs, config.rs
    │   ├── api/rest/{routes.rs, dto.rs, error.rs, sse.rs, handlers/*.rs}
    │   ├── domain/{error.rs, authz.rs, models.rs, credits.rs, estimation.rs, quota.rs,
    │   │           context.rs, sanitize.rs, services/*.rs}
    │   └── infra/
    │       ├── db/{entities/*.rs, migrations/*.rs, repo/*.rs}
    │       ├── llm/{mod.rs, openai_responses.rs, chat_completions.rs, anthropic.rs, resolver.rs,
    │       │        provisioning.rs, storage.rs}
    │       ├── outbox/{mod.rs, payloads.rs, handlers/*.rs}
    │       ├── workers/{orphan_watchdog.rs, upload_reaper.rs, leader.rs}
    │       ├── policy.rs, audit.rs, thumbnail.rs, metrics.rs
    │       └── plugins/{static_model_policy/*, static_audit/*}
    └── tests/                     # integration tests (HTTP/SSE + SQLite + fake OAGW)
```

**Structure Decision**: single gear crate plus SDK crate inside `gears/mini-chat/`, following the credstore/oagw layout; bundled plugins live in the gear crate at the module paths the example server already imports.

## Complexity Tracking

No constitution violations.
