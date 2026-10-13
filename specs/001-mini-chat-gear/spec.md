# Feature Specification: Mini Chat Gear

**Feature Branch**: `001-mini-chat-gear`

**Created**: 2026-10-03

**Status**: Draft

**Input**: User description: "Implement the mini-chat gear (gears/mini-chat/mini-chat, package cf-gears-mini-chat, lib mini_chat) and its SDK crate (gears/mini-chat/mini-chat-sdk, package cf-gears-mini-chat-sdk, lib mini_chat_sdk; added to the workspace; the gear depends on it). Multi-tenant AI chat gear: chat CRUD, message history, SSE streaming send-message with idempotent replay and one-running-turn-per-chat, turn status and turn mutations, attachments, models API, reactions, quota status, credit-based quota enforcement, CAS-guarded turn finalization with usage + audit outbox events, orphan watchdog, cleanup and thread-summary outbox workers, context assembly with token budget, canonical Problem error contract, OAGW provisioning and OpenAI-compatible provider adapters, bundled static model-policy and static audit plugins. Source of truth: gears/mini-chat/docs/PRD.md, DESIGN.md, ADR/*, acceptance-criteria.md (every item mandatory), docs/api/api.json. The specification MUST explicitly require automated tests: every acceptance-criteria item must be covered by automated tests that pass."

> **Normative references.** This specification states WHAT the feature must do. The exact codes,
> values, field names, defaults and wire shapes are those of `gears/mini-chat/docs/PRD.md`,
> `gears/mini-chat/docs/DESIGN.md` (incl. Appendix B configuration), the accepted ADRs in
> `gears/mini-chat/docs/ADR/` and the OpenAPI document `docs/api/api.json`. Where this spec
> summarizes them, the documents win. Items those documents mark "Not implemented", "Future" or
> "P2+" are out of scope.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Converse with an AI assistant in a private chat (Priority: P1)

An authenticated user creates a chat (optionally choosing a model and a title), sends a message and
watches the assistant's answer arrive incrementally as a stream. The conversation is persisted and
can be reloaded later page by page. Each chat belongs only to its creator.

**Why this priority**: This is the product. Without create + stream + history there is no value.

**Independent Test**: With a mock OpenAI-compatible provider, create a chat, send a message, assert
the stream events (`stream_started`, `delta`*, `done`), then list messages and see the user and
assistant messages with matching `request_id`, model and token counts.

**Acceptance Scenarios**:

1. **Given** an enabled catalog, **When** the user creates a chat without a model, **Then** 201 is
   returned with a `Location` header, the default model (first enabled `is_default`, else first
   enabled) and `message_count = 0`; no `title` field when untitled.
2. **Given** a chat, **When** the user sends `content`, **Then** the response is an event stream that
   begins with `stream_started` (resolved `request_id`, pre-allocated `message_id`,
   `is_new_turn: true`), relays every provider text delta as it arrives, and ends with exactly one
   `done` carrying `usage`, `effective_model`, `selected_model`, `quota_decision` and
   `quota_warnings`.
3. **Given** a completed turn, **When** the user lists messages, **Then** messages are chronological,
   each has `request_id`, `attachments` (array, possibly empty) and `my_reaction` (possibly null),
   assistant messages carry `model`, `input_tokens`, `output_tokens`; the chat `message_count` is 2.
4. **Given** several chats, **When** the user lists chats, **Then** the most recently active chat is
   first, and filtering/ordering/cursor pagination work; malformed queries are 400.
5. **Given** a chat, **When** the user renames it, **Then** only the title changes; invalid titles are
   400 `INVALID_TITLE`; the model can never be changed.
6. **Given** a chat, **When** the user deletes it, **Then** 204, later reads are 404, and provider-side
   cleanup is scheduled durably.

---

### User Story 2 - Reliable turns: idempotency, single running turn, status and recovery (Priority: P1)

The client may resend with the same `request_id` after a network hiccup, query the authoritative
turn status, and is protected against parallel generations in the same chat. Crashed or abandoned
turns are eventually finalized.

**Why this priority**: Without it users get duplicate charges, stuck chats, or lost answers.

**Independent Test**: Replay a completed `request_id` and assert no provider call, no quota change,
no new outbox rows; send while a turn is running and get 409 `turn_already_running`; query the
turn status endpoint for each state; let the orphan watchdog finalize a stale running turn.

**Acceptance Scenarios**:

1. **Given** a completed turn, **When** the same `request_id` is sent again, **Then** a replay stream
   (`stream_started` with `is_new_turn: false`, one `delta` with the full stored text, `done`) is
   returned with no provider request, no quota reserve/settlement and no outbox event.
2. **Given** a running/failed/cancelled/deleted turn, **When** its `request_id` is reused, **Then** 409
   `request_id_conflict`; replay is checked before the parallel-turn guard.
3. **Given** a running turn, **When** another send arrives with a different `request_id`, **Then** 409
   `turn_already_running`; once the turn is terminal a new send is accepted.
4. **Given** a stream that the client disconnects mid-way, **Then** the turn becomes `cancelled`,
   partial text (if any) is persisted, and quota is settled with the estimated formula.
5. **Given** a running turn with stale progress beyond the watchdog timeout, **Then** the watchdog
   marks it `failed`/`orphan_timeout`, settles estimated quota and enqueues the usage event exactly
   once.

---

### User Story 3 - Fix the last turn: retry, edit, delete (Priority: P2)

The user can regenerate, edit, or remove only the latest, already-finished turn. Mutations go through
the same streaming, quota, context-budget and attachment checks as a new message and get a new
server-generated `request_id`.

**Independent Test**: Complete two turns; retry the first → 409 `NOT_LATEST_TURN`; retry the second →
new stream with new `request_id`, old turn soft-deleted with `replaced_by_request_id`; edit with empty
content → 400 `EMPTY_CONTENT`; delete → 204; mutate a running turn → 400 `turn_state`/`STATE`.

**Acceptance Scenarios**:

1. Only the latest non-deleted terminal turn may be mutated; others give 409 `NOT_LATEST_TURN`; a
   running turn gives 400 failed_precondition; a turn by another requester gives 403.
2. A quota or kill-switch rejection of retry/edit leaves the previous turn intact.
3. Concurrent mutations resolve deterministically: one wins, the loser gets 409
   `GENERATION_IN_PROGRESS` (or `NOT_LATEST_TURN`).
4. Retry/edit copy the original message's non-deleted attachment links and reuse its web-search flag;
   a thread summary covering the mutated turn is invalidated.
5. Mutations emit `turn_retry` / `turn_edit` / `turn_delete` audit events.

---

### User Story 4 - Work with documents and images (Priority: P2)

The user uploads documents (indexed into the chat's own vector store for `file_search`), spreadsheets
(code interpreter) and images (multimodal input, thumbnail preview), references them in messages,
gets citations, and can delete unreferenced attachments.

**Independent Test**: Upload a PDF (mock Files + vector-store API) → 201 `ready`; send a message → the
provider request contains `file_search` with the chat vector store id; upload a PNG → thumbnail
present; attach it → provider request has an `input_image`; delete a referenced attachment → 409
`attachment_locked`; delete an unreferenced one → 204 and a cleanup outbox row.

**Acceptance Scenarios**:

1. Size, MIME type, per-chat document count and per-chat total size limits are validated with the
   documented categories and reasons; failed uploads stay visible with `status: failed`.
2. Indexing is polled within the request deadline; still-indexing documents return `uploaded` and
   finish in the background; provider failure or timeout produces `failed` / `indexing_failed`.
3. Abandoned uploads (`pending`/`uploaded` beyond `stale_after_secs`) are failed by the upload reaper
   with `upload_abandoned`, and their provider file is scheduled for deletion.
4. Images are rejected for models without `VISION_INPUT` (after downgrade) and when `disable_images`
   is on; more than `max_images_per_message` images give `TOO_MANY_IMAGES`.

---

### User Story 5 - Fair, predictable usage limits (Priority: P2)

Every provider call is preceded by a credit reserve against daily and monthly buckets; premium usage
falls back to standard when the premium budget is exhausted; when nothing is left the request is
rejected before any provider call. Settlement charges actual (or estimated) usage exactly once. The
user can query remaining quota.

**Independent Test**: Seed `quota_usage` near the premium limit, send on a premium chat → `done` shows
`quota_decision: downgrade`, `downgrade_reason: premium_quota_exhausted`; seed all buckets → 429
`resource_exhausted` with `violations[0].subject = tokens` and no provider call; check
`GET /quota/status` numbers against the buckets.

**Acceptance Scenarios**:

1. Reserve-before-execute on every provider call (send, retry, edit); the reserve is re-checked in the
   reserve transaction.
2. Kill switches (`disable_premium_tier`, `force_standard_tier`, `disable_web_search`,
   `disable_file_search`, `disable_images`, `disable_code_interpreter`) behave as documented.
3. Daily web-search and code-interpreter quotas reject with 429 and the right quota scope only when the
   tool is sent; per-message tool call limits fail the turn mid-stream.
4. Settlement uses the canonical credit formula with per-component ceiling, overshoot tolerance, and
   the estimated formula for aborted/failed-without-usage turns; exactly one usage outbox event per
   reserved turn, in the same transaction.

---

### User Story 6 - Model catalog, reactions and quota status (Priority: P3)

The user can browse enabled models (without internal fields), like/dislike assistant answers, and see
their quota status.

**Independent Test**: `GET /models` lists only enabled entries; disabled model → 404; `PUT` reaction
on an assistant message → 200, idempotent; on a user message → 400 `reaction_target`.

---

### User Story 7 - Long conversations stay functional (Priority: P3)

Context assembly fits the system prompt, thread summary and recent messages into the effective
model's budget deterministically; when the context is large a background thread-summary task
compresses older history; mutations invalidate stale summaries.

**Independent Test**: Use a tiny-context model; after enough turns a thread-summary outbox message is
enqueued, the handler calls the provider (non-streaming), stores the summary, marks messages
compressed, and the next request contains the summary preamble instead of old messages.

### Edge Cases

- Non-UUID path parameters → 400 `invalid_path_params`; malformed JSON → 400; schema mismatch → 422;
  missing JSON content type → 415.
- Chat whose model was removed from the catalog → 400 `INVALID_MODEL` on stream/retry/edit/upload;
  model merely disabled → downgrade with `model_disabled`.
- Provider 429 → SSE `error` `rate_limited`; provider failure → `provider_error` with a sanitized
  message (no provider ids, URLs or credentials); provider timeout → `provider_timeout`.
- Client disconnect after the terminal event is a no-op.
- Finalization failure → SSE `error` `finalization_failed`; turn left for the watchdog.
- Repeated attachment `DELETE` → 204 without a duplicate outbox event.
- PDP denial → 403; PDP failure → 503 with `Retry-After: 5`; foreign/soft-deleted resources → 404.
- Empty provider answer still completes the turn with an empty assistant message.

## Requirements *(mandatory)*

### Functional Requirements

**Packaging & platform**

- **FR-001**: The gear MUST be a ToolKit gear named `mini-chat` in package `cf-gears-mini-chat`
  (lib `mini_chat`) with REST, database and background-task capabilities; it MUST be registered so
  that the example server built with feature `mini-chat` serves it under `/mini-chat`.
- **FR-002**: An SDK package `cf-gears-mini-chat-sdk` (lib `mini_chat_sdk`) MUST exist at
  `gears/mini-chat/mini-chat-sdk`, be a workspace member, and hold the public plugin contracts
  (model-policy plugin client/spec, audit plugin client/spec), the policy snapshot / catalog /
  kill-switch / user-limit types and the usage / audit event types; the gear MUST depend on it.
- **FR-003**: Two bundled plugin gears MUST be provided: `static-mini-chat-model-policy-plugin`
  (`mini_chat::infra::plugins::static_model_policy`) serving a fixed catalog, kill switches and
  default limits from its config, and `static-mini-chat-audit-plugin`
  (`mini_chat::infra::plugins::static_audit`) logging audit events. Both are discovered through the
  types registry by `vendor`.
- **FR-004**: The gear configuration MUST accept the keys, defaults and validation of DESIGN
  Appendix B (unknown keys rejected where documented) and MUST provision OAGW upstreams/routes for
  every configured provider entry at start.

**Principles & constraints** (acceptance: Principles & Constraints)

- **FR-010**: Every resource MUST be tenant- and owner-scoped; foreign access is 404 (or 403 for a
  turn whose requester is another user, per DESIGN §3.9).
- **FR-011**: The context window budget MUST be enforced both for the input message
  (`INPUT_TOO_LONG`) and for the full assembled request (`CONTEXT_BUDGET_EXCEEDED`).
- **FR-012**: Streaming responses MUST be relayed event by event without buffering, through bounded
  channels.
- **FR-013**: A chat's model MUST be immutable once set.
- **FR-014**: Quota MUST be checked (and reserved) before any outbound provider call.

**Chat CRUD & messages API**

- **FR-020**: Create/get/list/update/delete chats with model and title validation as in DESIGN §3.3.
- **FR-021**: The chat list MUST support `$filter`, `$orderby`, `limit` and opaque cursors with the
  documented validation errors (`gts.cf.core.odata.query.v1~`), default order most-recent-activity
  first.
- **FR-022**: Message list MUST support filtering, ordering and pagination; each message MUST always
  include `id`, `request_id`, `role`, `content`, `attachments`, `my_reaction`, `created_at`, plus the
  optional `model`, `input_tokens`, `output_tokens` per DESIGN §3.3; `message_count` and chronological
  order MUST stay correct across turns and mutations.

**Streaming & SSE**

- **FR-030**: `POST /v1/chats/{id}/messages:stream` MUST stream a response correlated by a request id
  (client-provided or server-generated v4).
- **FR-031**: Preflight validation (content, attachment ids, image limits, kill switches, quota,
  context budget) MUST run before any provider call and return JSON errors without opening a stream.
- **FR-032**: The assistant message and usage MUST be persisted once the stream completes; the
  terminal SSE event is sent only after the finalization transaction commits.
- **FR-033**: The full SSE event contract (`stream_started`, `ping`, `delta`, `tool`, `citations`,
  `done`, `error`) and its ordering MUST be honored; `done` exposes usage and quota/downgrade outcome
  without internal identifiers; `error` is terminal with a sanitized message.

**Idempotency, parallel turns, lifecycle, mutations**

- **FR-040**: Replay of a completed `request_id` MUST be side-effect-free; conflicting reuse MUST be
  409 `request_id_conflict`; replay MUST be checked before the parallel-turn guard.
- **FR-041**: At most one running turn per chat; a new turn is accepted once the previous is
  terminal.
- **FR-042**: Turn status API and the turn state machine (`running` → `completed`/`failed`/
  `cancelled`, exposed as `running`/`done`/`error`/`cancelled`) MUST be consistent end to end,
  including partial content on cancellation and null content on failure.
- **FR-043**: Retry/edit/delete MUST act only on the latest terminal turn, go through the full send
  pipeline with a new server request id, resolve concurrent mutations deterministically, and carry
  forward attachments and the web-search flag.

**Attachments**

- **FR-050**: Upload/get/delete documents and images with size, type and per-chat limit validation.
- **FR-051**: Asynchronous indexing lifecycle (25 s request deadline, background polling, failure and
  timeout handling).
- **FR-052**: Attachments MUST be made available to the right provider tools (`file_search` with the
  chat vector store; `code_interpreter` container file ids; `input_image` file ids).
- **FR-053**: Cleanup (outbox-driven) and abandoned-upload recovery (upload reaper) MUST behave
  correctly under failure.

**Models, reactions, quota status**

- **FR-060**: Models API lists/gets only enabled catalog entries without internal fields.
- **FR-061**: Reactions set/remove on assistant messages only, idempotently.
- **FR-062**: `GET /v1/quota/status` MUST report per-tier, per-period quota consistent with usage.

**Quota, settlement, outbox**

- **FR-070**: Reserve-before-execute on every provider call; tier downgrade premium → standard;
  credits and tokens accounted per model and tier with the canonical formula.
- **FR-071**: Every terminal outcome settles quota exactly once with actual / estimated / released
  usage per DESIGN §5.7–5.9; usage is published through the outbox exactly once per turn.

**Context assembly, errors, web search, cleanup, authorization**

- **FR-080**: System prompt, tool guards, thread summary and recent history MUST be assembled and
  truncated deterministically within budget; tool availability and guidance MUST be reflected in the
  provider request.
- **FR-081**: All REST errors MUST map to the canonical `Problem` contract (ADR-0004) consistently
  across REST and streaming; provider-originated error details MUST be sanitized.
- **FR-082**: Web search MUST be reported (tool events), cited, accounted and quota-limited.
- **FR-083**: Chat deletion MUST trigger reliable background cleanup of provider resources; thread
  summary generation, failure/retry and mutation-driven invalidation MUST behave as documented.
- **FR-084**: Every operation MUST be authorized through the PDP and scoped to its owner; cross-tenant
  or foreign access is rejected consistently.

**Testing (explicitly requested)**

- **FR-090**: Every item of `gears/mini-chat/docs/acceptance-criteria.md` MUST be covered by at least
  one automated test that passes. Tests MUST include unit tests (in separate `*_tests.rs` files) for
  pure logic (credit arithmetic, cascade, estimation, truncation, sanitization, SSE parsing, billing
  outcome derivation, error mapping) and HTTP-level integration tests that drive the gear's routes
  against a mock OpenAI-compatible provider and inspect responses, SSE events, database rows and the
  requests the provider received.
- **FR-091**: Tests MUST NOT live in `testing/e2e/suites/mini_chat/`.
- **FR-092**: A traceability table MUST map each acceptance-criteria item to the tests that cover it.

### Key Entities

- **Chat**: owner tenant + user, locked model, optional title, timestamps, soft delete.
- **Message**: role, content, request id, effective model, token counts, compression flag.
- **Turn (chat_turns)**: idempotency key, state machine, preflight reserve snapshot, error code,
  tool counters, progress timestamp, soft delete / replacement link.
- **Attachment**: kind, MIME, size, status, provider file id (internal), thumbnail, cleanup state.
- **Message–attachment link**, **Thread summary** (frontier), **Chat vector store**, **Quota usage
  bucket** (tenant, user, period, bucket), **Message reaction**.
- **Outbox events**: usage event, audit event, attachment cleanup, chat cleanup, thread summary task.
- **Policy snapshot**: model catalog, kill switches; **User limits** per tier.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of the acceptance-criteria checklist items have at least one passing automated
  test, listed in the traceability table.
- **SC-002**: The example server builds with the documented feature set and starts with
  `config/mini-chat.yaml`.
- **SC-003**: A user sees the first streamed answer fragment as soon as the provider emits it (no
  accumulation before relay), verified by an ordering test.
- **SC-004**: Replaying a completed request causes zero provider calls and zero changes to quota or
  outbox (verified by counters and row counts).
- **SC-005**: For every turn that reserved quota, exactly one usage event is enqueued (verified for
  completed, failed, cancelled and orphaned turns).
- **SC-006**: No response body, stream event or error message contains a provider identifier,
  URL or credential (verified with provider error payloads containing such values).

## Assumptions

- The hidden acceptance environment uses the configuration keys and defaults of DESIGN Appendix B
  and an OpenAI-compatible mock provider reachable through OAGW with `use_http`.
- The bundled static plugins are the only policy/audit plugins in tests; policy version is 1.
- MCP, document summaries, hard purge, policy caching, image daily quota and other ADR-deferred items
  are out of scope.
- Metrics are emitted where convenient but are not part of the black-box contract.
- Leader election uses the no-op elector unless built with the `k8s` feature.
