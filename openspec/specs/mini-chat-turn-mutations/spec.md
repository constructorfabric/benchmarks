# mini-chat-turn-mutations Specification

## Purpose
Retry, edit and delete of the latest terminal turn.

## Requirements

### Requirement: Latest terminal turn only
Retry (`POST /v1/chats/{id}/turns/{request_id}/retry`), edit (`PATCH /v1/chats/{id}/turns/{request_id}` with `{content}`) and delete (`DELETE /v1/chats/{id}/turns/{request_id}`) SHALL act only on the latest non-deleted turn (greatest `(started_at, id)`). A `running` target SHALL return 400 `failed_precondition` `violations[{subject: turn_state, type: STATE}]` (checked before the latest-turn check); a non-latest or already deleted target SHALL return 409 `aborted` `context.reason = NOT_LATEST_TURN`; a turn whose `requester_user_id` is not the caller SHALL return 403; an unknown turn or chat SHALL return 404.

#### Scenario: Older turn
- **WHEN** retry targets the first of two completed turns
- **THEN** the response is 409 `NOT_LATEST_TURN`

#### Scenario: Running turn
- **WHEN** delete targets a running turn
- **THEN** the response is 400 with subject `turn_state`

### Requirement: Mutation goes through the send pipeline
Retry and edit SHALL validate the mutation and run the quota preflight (kill switches, image guards for the original message's images, 429) before committing; a rejection SHALL leave the previous turn unchanged. The commit SHALL soft-delete the old turn (setting `replaced_by_request_id`) and its messages, insert a new user message (retry copies the content, edit uses the new `content`; empty → 400 `EMPTY_CONTENT`), copy non-deleted attachment links, insert a new `running` turn with a server-generated `request_id`, bump `chats.updated_at`, drop a covering thread summary and enqueue a `turn_retry`/`turn_edit` audit event. Context assembly, provider resolution and the quota reserve SHALL then run; a failure marks the new turn `failed` (`context_length_exceeded`, `quota_exceeded` or `turn_setup_failed`) and returns a JSON error. Streaming SHALL use the same SSE contract as send with the new `request_id` in `stream_started`. Delete SHALL soft-delete the turn and its messages, enqueue a `turn_delete` audit event and return 204.

#### Scenario: Retry
- **WHEN** the latest completed turn is retried
- **THEN** a new stream starts with a new `request_id`, the old turn has `deleted_at` and `replaced_by_request_id`, and the old `request_id` returns 404 on the turn status API and 409 `request_id_conflict` on send

#### Scenario: Edit content
- **WHEN** the latest turn is edited with `{content: "new"}`
- **THEN** the provider receives `new` as the current user message and the old messages are not listed

### Requirement: Deterministic concurrent mutations
The latest/terminal checks, old-turn soft-delete and new-turn insert SHALL execute in one transaction; a concurrent mutation that loses the running-turn unique index SHALL return 409 `aborted` `context.reason = GENERATION_IN_PROGRESS`.

#### Scenario: Two concurrent retries
- **WHEN** two retries of the same turn race
- **THEN** exactly one streams and the other gets 409 (`GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`)

### Requirement: Carry-forward of attachments and tools
The new user message of retry/edit SHALL be linked to the original message's non-deleted attachments; images of the original message SHALL be re-sent; the new turn SHALL reuse `web_search_enabled` of the original turn.

#### Scenario: Retry with web search
- **WHEN** a turn sent with `web_search.enabled = true` is retried
- **THEN** the provider request of the retry includes the `web_search` tool and the new user message lists the same attachments
