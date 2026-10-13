# Feature Specification: Mini Chat Gear (multi-tenant AI chat) and its SDK

**Feature Branch**: `001-mini-chat-gear`

**Created**: 2026-10-03

**Status**: Draft

**Input**: User description: "Implement the mini-chat gear (multi-tenant AI chat) and its SDK crate in the
gears-rust workspace ... Every item of acceptance-criteria.md MUST be implemented AND covered by automated
tests ... tests are explicitly required by this specification."

**Normative sources**: `gears/mini-chat/docs/PRD.md`, `gears/mini-chat/docs/DESIGN.md`,
`gears/mini-chat/docs/ADR/*.md` (accepted), `gears/mini-chat/docs/acceptance-criteria.md`. Where this
spec summarises a rule, the referenced document holds the exact codes, values and field names; where any
other workspace file disagrees, those documents win. Each user story below names the acceptance-checklist
items (AC area) it covers.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Manage chats and browse history (Priority: P1)

An authenticated user creates a chat (optionally choosing a model from the catalog and a title), lists
their chats ordered by most recent activity with filtering/ordering/pagination, opens one (metadata plus
message count), renames it, and deletes it. They browse the chat's messages page by page. Nobody else —
no other user, no other tenant — can see or touch these chats.

**Why this priority**: every other capability lives inside a chat; without chats there is no product.

**Independent Test**: create/list/get/rename/delete chats and list messages through the HTTP API as two
different users and two tenants; verify bodies, status codes, ordering, pagination and isolation.

**AC areas**: Chat CRUD; Messages API (listing, response contract); Principles (isolation, model
immutability); Authorization.

**Acceptance Scenarios**:

1. **Given** an enabled catalog with a default model, **When** the user creates a chat without a model,
   **Then** 201 is returned with a `Location` header, the default model and `message_count = 0`.
2. **Given** an unknown or disabled model, **When** the user creates a chat with it, **Then** 400
   `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`.
3. **Given** a blank or >255-character title, **When** creating or renaming, **Then** 400 `INVALID_TITLE`.
4. **Given** chats with different activity, **When** listing, **Then** they are ordered by `updated_at`
   desc then `id`, support `$filter`/`$orderby` on `updated_at`, `id`, `title`, cursor pagination, clamp
   `limit` to 100 and reject `limit=0`, unknown fields or malformed cursors with 400.
5. **Given** a rename body that also contains `model`, **When** PATCHed, **Then** only the title changes.
6. **Given** another user's or tenant's chat, **When** any operation targets it, **Then** 404 `not_found`
   with the chat resource type; a PDP deny is 403 and a PDP failure is 503 with `Retry-After: 5`.
7. **Given** a deleted chat, **When** it is fetched or deleted again, **Then** 404.
8. **Given** a chat with turns, **When** listing messages, **Then** every message carries `request_id`,
   `attachments` (array, possibly empty) and `my_reaction` (possibly null), messages are chronological,
   and `$filter`/`$orderby` on `created_at`, `id`, `role` work with pagination.

---

### User Story 2 - Send a message and receive a streamed answer (Priority: P1)

The user sends a message to a chat and receives a real-time SSE stream: `stream_started` (request id,
assistant message id, `is_new_turn`), optional `ping`s before content, `delta`/`tool` events,
optional `citations`, then exactly one terminal `done` (usage, effective/selected model, quota decision,
optional downgrade fields and quota warnings) or `error` (`{code, message}`, sanitized). The user and
assistant messages and the usage are persisted; quota is reserved before the provider call and settled
exactly once afterwards.

**Why this priority**: this is the core value of the product.

**Independent Test**: point the gear at a mock OpenAI-compatible provider, send messages, assert the SSE
event sequence, the outbound provider request body, the persisted rows (`messages`, `chat_turns`,
`quota_usage`) and outbox usage/audit events.

**AC areas**: Streaming: Send Message; SSE Event Contract; Settlement & Finalization; Context Assembly;
Principles (no buffering, quota before provider, context budget); Error Mapping & Sanitization.

**Acceptance Scenarios**:

1. **Given** a valid chat, **When** the user posts content with or without a `request_id`, **Then** the
   stream starts with `stream_started` carrying that (or a generated) `request_id` and ends with `done`
   whose `usage` matches the provider usage; the turn is `completed`, messages are persisted and the
   chat's `updated_at` is bumped.
2. **Given** empty/whitespace content, too many images, duplicate/foreign/not-ready attachment ids, a
   message over `max_input_tokens`, a mandatory context over budget, or a kill-switched feature, **When**
   sent, **Then** a JSON `Problem` is returned with the documented category/reason and no provider call.
3. **Given** the provider fails, times out, throttles or returns an error containing provider ids,
   **When** streaming, **Then** the stream ends with `error{code}` (`provider_error`, `provider_timeout`,
   `rate_limited`) and a message with provider ids, URLs and keys replaced.
4. **Given** the client disconnects mid-stream, **Then** the turn becomes `cancelled`, partial text is
   persisted when non-empty, and an estimated settlement is applied.
5. **Given** a summary exists and history is long, **Then** the provider request contains the system
   prompt (plus tool guards), the summary as a user message with the preamble, the recent messages that
   fit the budget (whole turns, oldest dropped first) and the current message.

---

### User Story 3 - Recover safely: idempotent replay, one running turn, turn status (Priority: P1)

A client that lost its connection asks for the turn status by request id, and replaying a completed
request id returns the stored answer without any new provider call, quota change or event. A second
message while a turn is running is rejected; once the turn is terminal a new one is accepted.

**Independent Test**: send, replay with the same request id, assert identical content and unchanged
quota/outbox rows; race two sends; query turn status in every state.

**AC areas**: Idempotency & Replay; Parallel Turn Enforcement; Turn Lifecycle.

**Acceptance Scenarios**:

1. **Given** a completed turn, **When** the same `request_id` is posted, **Then** `stream_started`
   (`is_new_turn=false`), one `delta` with the full text and `done` are sent; no provider request, no
   quota or outbox change.
2. **Given** a running/failed/cancelled/soft-deleted turn, **When** its `request_id` is reused, **Then**
   409 `aborted` with `context.reason = request_id_conflict`.
3. **Given** any running turn in the chat and a new `request_id`, **Then** 409 `turn_already_running`;
   replay of a completed request id is still served while another turn runs.
4. **Given** each turn state, **When** the status endpoint is called, **Then** it returns
   `running|done|error|cancelled`, `error_code` only on error, `assistant_message_id` when persisted, and
   404 for deleted or foreign turns.

---

### User Story 4 - Retry, edit or delete the last turn (Priority: P2)

The user retries the last answer, edits the last question, or deletes the last turn. Only the latest,
terminal turn can be mutated; retry/edit go through the full send pipeline with a new server-generated
request id, preserve attachments, and leave the previous turn intact when the preflight rejects them.

**Independent Test**: build chats with several turns and mutate the latest and non-latest ones; check
statuses, SSE, `replaced_by_request_id`, copied attachment links and audit events.

**AC areas**: Turn Mutations; Cleanup & Recovery (summary invalidation).

**Acceptance Scenarios**:

1. **Given** a completed latest turn, **When** retried, **Then** an SSE stream with a new `request_id` is
   returned, the old turn is soft-deleted with `replaced_by_request_id` set, attachments are re-linked.
2. **Given** a non-latest or deleted turn, **Then** 409 `NOT_LATEST_TURN`; **given** a running target,
   **Then** 400 `failed_precondition` `turn_state/STATE`; **given** a concurrent race, **Then** 409
   `GENERATION_IN_PROGRESS` for the loser.
3. **Given** exhausted quota, **When** retry/edit is attempted, **Then** 429 and the old turn is unchanged.
4. **Given** a summary covering the mutated turn, **Then** the summary is deleted and messages are
   un-compressed in the mutation transaction.

---

### User Story 5 - Upload documents and images and use them in chat (Priority: P2)

The user uploads documents (indexed in the chat's vector store), spreadsheets (code interpreter) and
images (thumbnail, multimodal input), checks their status, references them in messages and deletes
unreferenced ones. Size, type and per-chat limits are enforced; provider failures and timeouts mark the
attachment `failed`; abandoned uploads are reaped.

**Independent Test**: multipart uploads against a mock Files/Vector Stores provider; GET/DELETE;
send messages with `attachment_ids` and inspect the provider request tools/inputs.

**AC areas**: Attachments; Principles (isolation); Web Search & tool availability.

**Acceptance Scenarios**:

1. **Given** a PDF/text upload, **Then** the provider file is created with `purpose=assistants`, a
   vector store is created once per chat, indexing is polled and 201 `status: ready` is returned.
2. **Given** an oversize file, unsupported MIME type, no file part, the per-chat document/storage limit,
   images while disabled, or XLSX while code interpreter is unavailable, **Then** the documented 400/429.
3. **Given** a provider failure, **Then** 503 with `Retry-After: 10` and the row remains `failed`.
4. **Given** a ready document, **When** a message is sent, **Then** the provider request includes
   `file_search` with the chat's vector store; images are sent as `input_image` items; XLSX files as
   `code_interpreter` container file ids.
5. **Given** an attachment referenced by a sent message, **When** deleted, **Then** 409
   `attachment_locked`; an unreferenced one is deleted with 204, idempotently, and an outbox cleanup event.

---

### User Story 6 - Quotas, models, reactions, web search (Priority: P2)

The user sees the enabled models, their own quota status per tier/period, likes or dislikes assistant
answers, and can enable web search per message. Premium usage is downgraded to standard when exhausted;
all-tier exhaustion yields 429; tool daily quotas and per-turn limits are enforced.

**Independent Test**: seed `quota_usage` rows and catalog configuration; exercise models, quota status,
reaction endpoints and web-search streams against the mock provider.

**AC areas**: Models API; Reactions API; Quota Status API; Quota Enforcement; Web Search.

**Acceptance Scenarios**:

1. **Given** premium daily quota exhausted, **When** sending on a premium chat, **Then** `done` shows
   `quota_decision=downgrade`, `downgrade_from`, `downgrade_reason=premium_quota_exhausted` and the
   standard model is sent to the provider.
2. **Given** all tiers exhausted, **Then** 429 `resource_exhausted` with `violations[0].subject=tokens`.
3. **Given** `web_search.enabled=true` on a supporting model, **Then** the provider request contains the
   `web_search` tool and the guard text; web citations are relayed; over the per-turn limit the stream
   ends with `web_search_calls_exceeded`; daily quota exhaustion yields 429 subject `web_search`.
4. **Given** an assistant message, **When** PUT/DELETE reaction, **Then** upsert/idempotent delete; on a
   user message 400 `reaction_target/STATE`; invalid value 400 `INVALID_REACTION`.

---

### User Story 7 - Background reliability (Priority: P3)

Chat deletion cleans provider files and the vector store via outbox handlers with retries; thread summaries
are generated asynchronously; stale running turns are failed by the orphan watchdog; usage and audit
events are published exactly once per turn.

**AC areas**: Cleanup & Recovery; Settlement & Finalization (publication); Turn Lifecycle.

**Acceptance Scenarios**:

1. **Given** a deleted chat with documents, **Then** provider files and the vector store are deleted and
   `cleanup_status` becomes `done`; the `chat_vector_stores` row is removed.
2. **Given** a turn that pushes context past the threshold, **Then** a summary task runs, stores a summary
   and marks the summarized messages compressed; provider failure keeps the old summary.
3. **Given** a running turn without progress beyond the timeout, **Then** it becomes `failed` with
   `orphan_timeout` and an estimated settlement.

### Edge Cases

- Concurrent sends to the same chat or the same request id (unique-index races) map to the documented 409s.
- Provider stream ends without usage; provider `incomplete` responses complete the turn.
- Chat model removed from the catalog after creation (400 `INVALID_MODEL` on send/upload) vs disabled
  (downgrade with `model_disabled`).
- Images on a turn downgraded to a non-vision model → 400 `VISION_NOT_SUPPORTED`.
- Non-UUID path parameters, malformed JSON (400), schema mismatch (422), non-JSON content type (415).
- Period boundaries are UTC calendar days/months; quota warnings skip periods with limit ≤ 0.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST expose the REST/SSE endpoints of DESIGN §3.3 under the gear URL prefix
  (default `/mini-chat`) with the request/response schemas of the generated OpenAPI reference.
- **FR-002**: The system MUST scope every resource to the owning tenant and user, returning 404 for hidden
  resources, 403 for PDP denial and 503 (`Retry-After: 5`) for PDP failure.
- **FR-003**: The system MUST validate chat titles, models, reactions, content, attachments and OData
  queries and return canonical `Problem` errors exactly as in the ADR-0004 table.
- **FR-004**: The system MUST resolve the default model deterministically and keep a chat's model immutable.
- **FR-005**: The system MUST stream responses without buffering and emit the SSE contract (events,
  ordering, payloads, ping rules, single terminal event, sanitized errors).
- **FR-006**: The system MUST run preflight (kill switches, quota cascade, tool daily quotas, image and
  input limits, context budget) before any provider call, and reserve quota atomically with creating the
  user message and turn.
- **FR-007**: The system MUST settle quota exactly once per reserved turn via a CAS-guarded finalization
  that also enqueues usage and audit outbox events, using actual or estimated usage per DESIGN §5.7–5.9.
- **FR-008**: The system MUST implement idempotent replay, parallel-turn rejection and their check order.
- **FR-009**: The system MUST implement retry/edit/delete of the latest terminal turn with the documented
  ordering, errors, attachment carry-over, new request id, summary invalidation and audit events.
- **FR-010**: The system MUST implement attachment upload/get/delete with MIME/size/per-chat limits,
  synchronous provider upload, vector-store indexing with the 25 s deadline and background continuation,
  thumbnails, outbox cleanup and the upload reaper.
- **FR-011**: The system MUST assemble provider requests (system prompt, guards, summary, recent messages,
  user message, images, tools `file_search`/`web_search`/`code_interpreter`, metadata, `user`) per DESIGN §4.
- **FR-012**: The system MUST provide the models, reactions and quota status APIs with the documented
  visibility, non-exposure and arithmetic rules.
- **FR-013**: The system MUST run outbox handlers for usage, audit, attachment cleanup, chat cleanup and
  thread summary, plus the orphan watchdog, with the documented retry/reject semantics.
- **FR-014**: The system MUST persist the DESIGN §3.7 schema (tables, columns, constraints) on SQLite and
  Postgres.
- **FR-015**: An SDK crate MUST expose the transport-agnostic contract (catalog/policy/audit plugin
  traits and types, usage/audit events, client trait) consumed by the gear and bundled plugins.
- **FR-016**: The gear MUST read configuration with the keys/defaults/validation of DESIGN Appendix B and
  register the bundled `static-mini-chat-model-policy-plugin` and `static-mini-chat-audit-plugin`.
- **FR-017 (Testing, explicitly required)**: Every item of `acceptance-criteria.md` MUST be covered by at
  least one automated test that passes: unit tests for pure domain rules (credit arithmetic, cascade,
  estimation, truncation, sanitization, error mapping, billing derivation) and integration tests that
  drive the HTTP/SSE API in-process against an in-memory database and a mock OpenAI-compatible provider,
  asserting responses, persisted rows and provider requests. Tests MUST NOT live under
  `testing/e2e/suites/mini_chat/`.

### Key Entities

- **Chat**: owned by a tenant+user; selected model, title, timestamps, soft delete.
- **Message**: user/assistant content, request id, effective model, token counts, compression flag.
- **Turn**: one user-initiated generation; state machine, reserve fields, error code, tool counters.
- **Attachment**: document/image file with status, purpose flags, provider file id (internal), thumbnail,
  cleanup state; linked to messages via message-attachment associations.
- **Thread summary**: per-chat compressed history with an inclusive frontier.
- **Chat vector store**: per-chat provider vector store mapping.
- **Quota usage**: per user/period/bucket credit and call counters.
- **Message reaction**: like/dislike per user per assistant message.
- **Usage / audit events**: outbox payloads published to the policy and audit plugins.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of `acceptance-criteria.md` items have at least one passing automated test.
- **SC-002**: The example server builds with the documented feature set and starts with the gear's
  configuration file.
- **SC-003**: Zero cross-tenant or cross-user data access in tests covering every endpoint.
- **SC-004**: Every reserved turn produces exactly one settlement and one usage event in all tested
  terminal paths (completed, failed, cancelled, orphan); replay produces none.
- **SC-005**: The first streamed token reaches the client without waiting for the full answer (deltas are
  observed before the provider stream finishes).

## Assumptions

- The hidden acceptance suite uses the configuration keys and defaults of DESIGN Appendix B, an
  OpenAI-compatible mock provider reached through OAGW, and the static authn/authz/single-tenant plugins.
- MCP, document summaries, hard purge, image quotas and other items marked "Not implemented"/"Future" in
  the ADRs are out of scope.
- Only the `openai_responses` adapter needs full fidelity; other adapter kinds are accepted in config.
- Metrics are recorded best-effort; they are not part of the black-box contract.
