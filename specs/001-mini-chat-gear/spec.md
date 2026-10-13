# Feature Specification: Mini-Chat Gear (multi-tenant AI chat) and its SDK

**Feature Branch**: `001-mini-chat-gear`

**Created**: 2026-10-03

**Status**: Draft

**Input**: User description: "Implement the `mini-chat` gear and its new SDK crate for the gears-rust
workspace. Multi-tenant AI chat: chat CRUD, message history, SSE streaming of LLM responses through
the outbound gateway to an OpenAI-compatible provider, idempotent replay by request_id, one running
turn per chat, tail-only turn mutations, attachments, model catalog API, reactions, per-user credit
quotas with premium→standard downgrade, settlement and usage outbox, context assembly with thread
summary, canonical error contract, web search, background cleanup and owner/tenant authorization.
The authoritative contract is `gears/mini-chat/docs`; every acceptance-criteria item must be
implemented AND covered by automated tests (tests are explicitly REQUIRED)."

**Authoritative references**: `gears/mini-chat/docs/PRD.md`, `DESIGN.md`, `ADR/0001..0010`,
`acceptance-criteria.md`. The exact wire schemas (routes, DTOs, SSE payloads) are captured in
`contracts/openapi-mini-chat.json` (extracted from the repository's generated OpenAPI document).
Where this spec summarizes, the gear docs are the source of truth for exact codes and values.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Manage chats (Priority: P1)

A chat user creates a chat (optionally choosing a model and title), lists their chats ordered by
most recent activity with filtering/ordering/cursor pagination, opens one (metadata plus message
count), renames it and deletes it.

**Why this priority**: Every other capability lives inside a chat.

**Independent Test**: Exercise create/get/list/patch/delete through the REST API and verify bodies,
status codes, validation errors and ordering.

**Acceptance Scenarios**:

1. **Given** an enabled default model, **When** the user creates a chat without a model, **Then**
   201 with `Location: /mini-chat/v1/chats/{id}` and the chat uses the first enabled `is_default`
   model (else the first enabled model).
2. **Given** an unknown or disabled model, **When** creating a chat, **Then** 400
   `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`.
3. **Given** a whitespace-only or >255-character title, **When** creating or renaming, **Then** 400
   `INVALID_TITLE`; a valid title is trimmed.
4. **Given** several chats, **When** listing, **Then** they are ordered by `updated_at` desc, `id`
   tiebreak; sending a message, retry or edit moves the chat to the top; `$filter`/`$orderby` work
   on `updated_at`, `id`, `title`; unknown fields or malformed cursors return 400 with the OData
   resource type; `limit` above 100 is clamped.
5. **Given** a deleted chat, **When** it or any sub-resource is requested, **Then** 404; a second
   delete is 404.
6. **Given** a chat owned by another user or tenant, **When** accessed, **Then** 404.

---

### User Story 2 - Send a message and receive a streamed answer (Priority: P1)

The user sends a message; the gear checks quota, assembles context, calls the provider and relays
the answer as an SSE stream (`stream_started`, `ping*`, `delta`/`tool`, `citations?`, `done` |
`error`), persisting the user message, assistant message, turn state and usage.

**Why this priority**: Core product value.

**Independent Test**: Against a mock OpenAI-compatible provider, send a message and verify the SSE
event sequence, persisted messages, turn status, usage, and the exact provider request.

**Acceptance Scenarios**:

1. **Given** a chat, **When** the user posts `messages:stream`, **Then** the first event is
   `stream_started{request_id, message_id, is_new_turn:true}`, deltas are relayed as received (not
   buffered), and exactly one terminal `done{usage, effective_model, selected_model,
   quota_decision, ...}` ends the stream.
2. **Given** empty/whitespace content, invalid/duplicate/foreign/not-ready attachment ids, too many
   images, input above `max_input_tokens`, or a kill switch, **When** sending, **Then** a canonical
   JSON Problem is returned and no stream is opened and no provider call is made.
3. **Given** a completed turn with request id R, **When** R is posted again, **Then** the stored
   answer is replayed (`is_new_turn:false`) with no provider call and no quota change.
4. **Given** a running turn, **When** another message is posted, **Then** 409 `aborted`
   `turn_already_running`; once the turn is terminal, a new message is accepted.
5. **Given** a provider failure after the stream started, **Then** a terminal `error{code,message}`
   event with a sanitized message (no provider ids) ends the stream and the turn becomes failed.

---

### User Story 3 - Read history, turn status and react (Priority: P1)

The user pages through messages with OData filtering/ordering, queries a turn's status after a
disconnect, and likes/dislikes assistant messages.

**Independent Test**: List messages after several turns; GET turn status; PUT/DELETE reactions.

**Acceptance Scenarios**:

1. Every message carries `id`, `request_id`, `role`, `content`, `attachments` (always an array),
   `my_reaction` (always present, may be null), `created_at`, and optional `model`,
   `input_tokens`, `output_tokens` (omitted when absent/zero).
2. Messages are chronologically ordered; `message_count` reflects non-deleted messages.
3. Reactions are allowed only on assistant messages (400 `failed_precondition`, `reaction_target`
   otherwise), PUT is idempotent/replaceable, DELETE is idempotent (204); an invalid value is 400
   `INVALID_REACTION`.
4. Turn status maps `running|completed|failed|cancelled` to `running|done|error|cancelled`;
   soft-deleted turns return 404.

---

### User Story 4 - Retry, edit or delete the last turn (Priority: P2)

**Acceptance Scenarios**:

1. Only the latest non-deleted turn (greatest `(started_at, id)`) in a terminal state can be
   mutated; non-terminal → 400 `failed_precondition` `turn_state`; not latest → 409
   `NOT_LATEST_TURN`; concurrent loser → 409 `GENERATION_IN_PROGRESS`; foreign requester → 403.
2. Retry/edit run the full send pipeline (quota, context budget, attachment checks) with a new
   server-generated request id, stream SSE, soft-delete the previous turn only after preflight
   passes, carry forward attachments (deleted ones excluded) and the web-search flag, and bump the
   chat's `updated_at`.
3. Delete soft-deletes the turn (204) and its messages leave the history.
4. Mutations invalidate an existing thread summary that covers the mutated turn.

---

### User Story 5 - Attachments (Priority: P2)

Upload documents and images (multipart), get their status, delete them; documents are indexed in a
per-chat provider vector store, images are stored as provider files with an optional thumbnail.

**Acceptance Scenarios**:

1. Size, MIME type, per-chat document count and total storage limits are validated with the
   documented codes (400 `FILE_TOO_LARGE`, 400 `UNSUPPORTED_CONTENT_TYPE`, 429
   `document_limit`/`storage_limit`).
2. A successful upload returns 201 `ready` (or `uploaded` when indexing continues in background);
   provider failures mark the row `failed` with `error_code` and return 503.
3. Deleting an unreferenced attachment returns 204 and is idempotent; deleting a referenced one is
   409 `attachment_locked`; provider cleanup happens asynchronously with retries.
4. Ready documents enable the `file_search` tool with the chat's vector store; images are sent as
   multimodal input; abandoned uploads are failed by a background reaper.

---

### User Story 6 - Models and quota (Priority: P2)

**Acceptance Scenarios**:

1. `GET /v1/models` lists only enabled catalog models with public fields only; `GET
   /v1/models/{id}` returns 404 for unknown/disabled.
2. `GET /v1/quota/status` reports `premium` and `total` tiers per `daily`/`monthly` period with
   limits, usage, remaining, percentage, next reset, warning/exhausted flags consistent with actual
   usage.
3. Quota is reserved before every provider call; an exhausted premium tier downgrades to
   standard (`quota_decision: downgrade`, `downgrade_from`), all tiers exhausted → 429
   `resource_exhausted` with `quota_scope` `tokens`.
4. Every terminal outcome settles the reserve exactly once (actual usage when known, bounded
   estimate otherwise) and enqueues exactly one usage event.

---

### User Story 7 - Web search, context assembly, cleanup (Priority: P3)

**Acceptance Scenarios**:

1. `web_search.enabled` adds the provider web search tool (when the effective model supports it)
   plus the system prompt guard; kill switch → 400 `FEATURE_DISABLED`; daily quota → 429
   `web_search`; per-turn limit exceeded mid-stream → SSE `error{web_search_calls_exceeded}`;
   web citations are relayed in the `citations` event.
2. Context = system prompt + thread summary + recent history within budget; oldest whole turns are
   dropped first; the summary is generated by a background worker when the trigger fires.
3. Chat deletion enqueues provider cleanup (vector store and files) which is retried until done.

### Edge Cases

- Client disconnect mid-stream: turn becomes `cancelled`, partial text persisted if any, a bounded
  estimate is settled, no `error` event is sent.
- Process crash with a running turn: the orphan watchdog fails it (`orphan_timeout`).
- Malformed JSON (400), schema mismatch (422), missing JSON content type (415), non-UUID path (400).
- PDP failure: 503 with `Retry-After: 5`; PDP deny: 403.
- Reuse of a request id of a failed/cancelled/deleted turn: 409 `request_id_conflict`; replay is
  checked before the running-turn guard.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide the REST routes listed in PRD §7.1 under `/mini-chat/v1` with
  the request/response schemas of `contracts/openapi-mini-chat.json`.
- **FR-002**: The system MUST enforce tenant and owner isolation on every chat, message, turn,
  attachment and reaction; hidden resources return 404 with the matching `resource_type`.
- **FR-003**: The system MUST validate chat model (`INVALID_MODEL`) and title (`INVALID_TITLE`) and
  keep a chat's model immutable.
- **FR-004**: The system MUST support OData `$filter`/`$orderby` and cursor pagination on chat and
  message lists with documented 400 errors.
- **FR-005**: The system MUST stream answers over SSE following the PRD/DESIGN event contract and
  ordering, without buffering, with `ping` before content and a terminal `done` or `error`.
- **FR-006**: The system MUST run preflight validation (content, attachments, image limits, kill
  switches, context budget, quota) before any provider call and return a Problem without opening
  the stream on failure.
- **FR-007**: The system MUST persist the user message at turn start and the assistant message,
  usage, effective model and turn state at finalization.
- **FR-008**: The system MUST implement replay, `request_id_conflict` and the single-running-turn
  guard with replay checked first.
- **FR-009**: The system MUST implement retry/edit/delete of the latest terminal turn with the
  documented errors and full send pipeline.
- **FR-010**: The system MUST implement attachments upload/get/delete with limits, async indexing,
  cleanup outbox and an abandoned-upload reaper.
- **FR-011**: The system MUST expose read-only model list/get and quota status.
- **FR-012**: The system MUST implement reserve→execute→settle credit quotas with tier downgrade,
  per-model credit multipliers, warnings and exactly-once settlement and usage outbox.
- **FR-013**: The system MUST assemble context deterministically (system prompt, tool guidance,
  thread summary, recent history) within the input budget and run the thread summary worker.
- **FR-014**: The system MUST map all errors to the canonical error contract (REST Problem, SSE
  `{code,message}`) and sanitize provider identifiers.
- **FR-015**: The system MUST support web search tool enablement, citations, accounting and daily /
  per-turn limits.
- **FR-016**: The system MUST clean up provider resources after chat deletion via a retried
  background outbox.
- **FR-017**: Automated tests MUST cover every item of `acceptance-criteria.md` and pass under
  `cargo test` (tests are REQUIRED; test tasks precede implementation tasks).
- **FR-018**: A new SDK crate `cf-gears-mini-chat-sdk` (lib `mini_chat_sdk`) MUST hold the public
  contracts (plugin traits for model policy and audit, policy snapshot/model catalog types, usage
  event types) and be a workspace member that the gear depends on.

### Key Entities

- **Chat**: owner (tenant, user), model (immutable), title, temporary flag, timestamps, soft delete.
- **Turn**: (chat, request_id) correlation, state machine running→completed/cancelled/failed,
  requester, effective model, reserve and settlement data, soft delete.
- **Message**: role, content, request_id, model, token counts, linked attachments, reactions.
- **Attachment**: kind document/image, status pending/uploaded/ready/failed, purpose flags, size,
  provider file (internal), thumbnail.
- **Chat vector store**: per-chat provider index (internal identifier).
- **Thread summary**: compressed history frontier per chat.
- **Quota usage**: per user, bucket (`total`, `tier:premium`), period (daily/monthly) counters of
  credits, reserved credits, tokens and tool calls.
- **Reaction**: user × assistant message → like/dislike.
- **Outbox events**: usage snapshot, audit, chat cleanup, attachment cleanup, thread summary.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of the acceptance-criteria checklist items map to at least one passing automated
  test.
- **SC-002**: Zero cross-tenant or cross-user data access in tests (all foreign access → 404).
- **SC-003**: The first streamed text reaches the client before the provider finishes its answer
  (no buffering), verified with a slow mock provider.
- **SC-004**: Every turn that reserved quota produces exactly one settlement and exactly one usage
  event across completed, failed and cancelled outcomes.
- **SC-005**: Replaying a completed request makes zero provider calls and changes no quota row.
- **SC-006**: The example server builds with the required feature set and serves the gear's routes.

## Assumptions

- The hidden acceptance environment uses an OpenAI Responses-compatible mock provider through OAGW
  and the static policy/audit plugins bundled in the gear; the provider configuration keys follow
  DESIGN.md.
- MCP, document summaries, hard purge, image daily quotas and other items marked "not implemented"
  in the docs/ADRs remain out of scope.
- SQLite (and PostgreSQL) are the supported databases; migrations are created by the gear.
- License gate in P1 is the api-gateway base license middleware.
