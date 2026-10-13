# mini-chat-turn-mutations Specification

## Purpose
Retry, edit and deletion of the latest chat turn in the mini-chat gear.

## Requirements

### Requirement: Mutations only on the latest terminal turn
`POST /v1/chats/{id}/turns/{request_id}/retry`, `PATCH /v1/chats/{id}/turns/{request_id}` (`{content}`) and `DELETE /v1/chats/{id}/turns/{request_id}` SHALL act only on the non-deleted turn with the greatest `(started_at, id)`. A running target SHALL return 400 `failed_precondition` `turn_state/STATE` (checked before latest); a non-latest or deleted target SHALL return 409 `aborted` `NOT_LATEST_TURN`; a target of another requester 403 `permission_denied`; unknown chat/turn 404. Edit with empty content SHALL return 400 `EMPTY_CONTENT`. Delete returns 204.

#### Scenario: Retry non-latest
- **WHEN** retry is requested for the first of two completed turns
- **THEN** the response is 409 with `context.reason = NOT_LATEST_TURN`

#### Scenario: Delete latest
- **WHEN** the latest turn is deleted
- **THEN** the response is 204, its turn status returns 404 and its messages are no longer listed

### Requirement: Mutation goes through the full send pipeline with a new request id
Retry and edit SHALL validate read-only and run the quota preflight (kill switches, image guards on the original images) before committing; a rejection leaves the previous turn intact. The commit SHALL soft-delete the old turn and its messages, set `replaced_by_request_id`, insert the new user message (retry: original content; edit: new content) and a new `running` turn with a server-generated `request_id`, bump `chats.updated_at`, drop a covering thread summary and enqueue a `turn_retry`/`turn_edit` audit event; then context assembly, reserve and streaming run exactly as for `messages:stream`. A failure after the commit SHALL mark the new turn `failed` (`context_length_exceeded`, `quota_exceeded` or `turn_setup_failed`).

#### Scenario: Retry yields new request id
- **WHEN** the latest completed turn is retried
- **THEN** `stream_started.request_id` differs from the original, the old request id returns 404 on turn status and 409 `request_id_conflict` on `messages:stream`

#### Scenario: Quota rejection keeps previous answer
- **WHEN** retry is requested while all tiers are exhausted
- **THEN** the response is 429 and the previous turn is still `done`

### Requirement: Concurrent mutations resolve deterministically
The latest-turn check, terminal check, soft-delete and new running turn insert SHALL run in one transaction; losing the running-turn unique index race SHALL return 409 `GENERATION_IN_PROGRESS`.

#### Scenario: Two concurrent retries
- **WHEN** two retries of the same turn race
- **THEN** exactly one streams and the other gets 409 (`GENERATION_IN_PROGRESS` or `NOT_LATEST_TURN`)

### Requirement: Mutated turns carry forward attachments and tool usage
Retry and edit SHALL copy the non-deleted attachment associations of the old user message to the new user message, re-send its images and reuse the old turn's `web_search_enabled`.

#### Scenario: Attachments copied on edit
- **WHEN** a turn with one attachment is edited
- **THEN** the new user message lists the same attachment
