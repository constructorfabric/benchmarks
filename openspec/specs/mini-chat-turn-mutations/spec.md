# mini-chat-turn-mutations Specification

## Purpose

Retry, edit and delete of the latest terminal turn through the full send pipeline.

## Requirements

### Requirement: Latest terminal turn only
Retry (`POST /v1/chats/{id}/turns/{request_id}/retry`), edit (`PATCH /v1/chats/{id}/turns/{request_id}` with `{content}`) and delete (`DELETE /v1/chats/{id}/turns/{request_id}`) SHALL act only on the latest non-deleted turn (greatest `(started_at, id)`) in a terminal state. A running target SHALL return 400 `failed_precondition` (`violations[{subject: turn_state, type: STATE}]`, checked before the latest-turn check); a target that is not the latest (including an already deleted turn or one superseded by a newer running turn) SHALL return 409 `aborted` `NOT_LATEST_TURN`; a turn of another requester SHALL return 403; an unknown turn or chat SHALL return 404. Edit with empty content SHALL return 400 `EMPTY_CONTENT`.

#### Scenario: Retry non-latest
- **WHEN** retry is requested for the first of two completed turns
- **THEN** the response is 409 with `context.reason = NOT_LATEST_TURN`

#### Scenario: Delete latest
- **WHEN** the latest completed turn is deleted
- **THEN** the response is 204, the turn and its messages are soft-deleted, its status returns 404 and its request id on `messages:stream` returns 409 `request_id_conflict`

### Requirement: Mutation uses the full send pipeline
Retry and edit SHALL validate read-only, run the quota preflight (cascade, kill switches, image guards for the original images) before committing, then in one transaction soft-delete the old turn and its messages (setting `replaced_by_request_id`), insert the new user message (retry copies content; edit uses the new content), copy non-deleted attachment links, insert a new `running` turn with a server-generated UUID v4 `request_id`, invalidate a thread summary covering the turn, bump `chats.updated_at` and enqueue the `turn_retry`/`turn_edit` audit event; then context assembly and the quota reserve run and the response streams exactly like `messages:stream` (`stream_started.request_id` = new id). Failures after the commit SHALL mark the new turn `failed` with `context_length_exceeded`, `quota_exceeded` or `turn_setup_failed` and return a JSON error. Delete SHALL enqueue a `turn_delete` audit event.

#### Scenario: Retry streams with new request id
- **WHEN** the latest completed turn is retried
- **THEN** the SSE `stream_started.request_id` differs from the original, the old turn has `deleted_at` and `replaced_by_request_id` set, and the chat lists the new user/assistant pair only

#### Scenario: Preflight rejection keeps previous answer
- **WHEN** a retry is rejected by quota (429)
- **THEN** the previous turn is unchanged and still the latest

### Requirement: Deterministic concurrency
The latest-turn check, terminal check, soft-delete and new-turn insert SHALL run in one transaction; a concurrent mutation that loses the one-running-turn unique index SHALL return 409 `aborted` `GENERATION_IN_PROGRESS`, so exactly one of two concurrent retries succeeds.

#### Scenario: Two concurrent retries
- **WHEN** two retries of the same latest turn race
- **THEN** exactly one streams and the other gets 409 (`GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`)

### Requirement: Carry-forward of attachments and tool history
The new user message SHALL be linked to the original message's non-deleted attachments; images among them SHALL be re-sent as multimodal input; the original turn's `web_search_enabled` flag SHALL be reused.

#### Scenario: Retry with image
- **WHEN** a turn whose user message referenced an image is retried
- **THEN** the new user message lists the same attachment and the provider request contains the image as `input_image`
