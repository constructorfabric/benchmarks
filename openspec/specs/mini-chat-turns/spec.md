# mini-chat-turns Specification

## Purpose
Turn lifecycle and turn mutations of the mini-chat gear: turn status API, the running → terminal state machine, and retry / edit / delete of the latest turn.

## Requirements

### Requirement: Turn status API
`GET /v1/chats/{id}/turns/{request_id}` SHALL return `{request_id, state, error_code?, assistant_message_id?, updated_at}` with state mapping `running→running`, `completed→done`, `failed→error`, `cancelled→cancelled`; soft-deleted turns and turns of other users' chats return 404 with the turn resource type.

#### Scenario: Completed turn
- **WHEN** a turn completed
- **THEN** state is `done`, `assistant_message_id` is set and `error_code` is absent

### Requirement: Turn state machine
Turns SHALL move only `running → completed | failed | cancelled` through a CAS update guarded by `state = 'running'`; terminal states are immutable and exactly one finalizer wins.

#### Scenario: Late finalizer loses
- **WHEN** the orphan watchdog finalized a turn and the stream later tries to finalize it
- **THEN** the stream's CAS affects 0 rows and no second settlement or outbox event happens

### Requirement: Partial and null content
A cancelled turn with accumulated text SHALL persist that text as the assistant message and set `assistant_message_id`; a cancelled turn without text and every failed turn (even after partial deltas were streamed) SHALL have `assistant_message_id = null`; a completed turn always has an assistant message (possibly empty content).

#### Scenario: Failure before text
- **WHEN** the provider fails before any delta
- **THEN** the turn is `failed` and no assistant message row exists

### Requirement: Mutations only on the latest terminal turn
Retry (`POST .../turns/{rid}/retry`), edit (`PATCH .../turns/{rid}` with `{content}`) and delete (`DELETE .../turns/{rid}`) SHALL act only on the latest non-deleted turn by `(started_at, id)`: a running target returns 400 `failed_precondition` `turn_state`/`STATE` (checked first), a non-latest or deleted target returns 409 `aborted` `NOT_LATEST_TURN`, a turn requested by another user returns 403, unknown returns 404. Delete soft-deletes the turn and its messages and returns 204.

#### Scenario: Retry older turn
- **WHEN** a user retries the first of two completed turns
- **THEN** the response is 409 `NOT_LATEST_TURN`

### Requirement: Mutations use the full send pipeline with a new request id
Retry and edit SHALL run the mutation preview and quota preflight (kill switches, image guards, quota cascade) before committing; a rejection leaves the previous turn intact. The commit soft-deletes the old turn and its messages, sets `replaced_by_request_id`, inserts a new user message (copy or edited content) and a new `running` turn with a server-generated request id, bumps `chats.updated_at` and enqueues `turn_retry`/`turn_edit` audit; then context assembly, provider resolution and the reserve run and the response is the same SSE stream. A failure after commit marks the new turn `failed` with `context_length_exceeded`, `quota_exceeded` or `turn_setup_failed`. Edit with empty content returns 400 `EMPTY_CONTENT`.

#### Scenario: Retry gets new request id
- **WHEN** a user retries the latest completed turn
- **THEN** `stream_started.request_id` differs from the old one, the old turn returns 404 on status and 409 `request_id_conflict` on resend, and its `replaced_by_request_id` is the new id

### Requirement: Concurrent mutations resolve deterministically
Concurrent retry/edit of the same turn SHALL produce exactly one new running turn; the loser gets 409 (`GENERATION_IN_PROGRESS` when it lost the running-turn insert race, `NOT_LATEST_TURN` when the target was already replaced).

#### Scenario: Two retries at once
- **WHEN** two retries of the same turn are sent concurrently
- **THEN** one streams and the other returns 409 `aborted`

### Requirement: Mutations carry attachment and tool history
Retry and edit SHALL copy the original user message's non-deleted attachment associations to the new user message, re-send its images with the same image guards, and reuse the turn's `web_search_enabled` flag.

#### Scenario: Retry with image
- **WHEN** a turn whose message referenced an image is retried
- **THEN** the new user message lists the image and the provider request contains its `input_image`
