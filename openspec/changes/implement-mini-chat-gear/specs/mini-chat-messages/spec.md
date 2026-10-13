## ADDED Requirements

### Requirement: Message listing
`GET /v1/chats/{id}/messages` SHALL return non-deleted messages of the chat ordered by `created_at asc, id asc` by default, with cursor pagination (default 20, clamp 100, `0` → 400), `$filter`/`$orderby` on `created_at`, `id`, `role`, and the same OData error mapping as the chat list.

#### Scenario: Filter by role
- **WHEN** the user lists with `$filter=role eq 'assistant'`
- **THEN** only assistant messages are returned

#### Scenario: Filter by id
- **WHEN** the user lists with `$filter=id eq '<assistant_message_id>'`
- **THEN** exactly that message is returned

### Requirement: Message DTO contract
Each message SHALL include `id`, `request_id` (non-null; user and assistant of a turn share it), `role`, `content`, `attachments` (always an array of `{attachment_id, kind, filename, status, img_thumbnail?}` from `message_attachments` excluding deleted attachments), `my_reaction` (always present, `like`/`dislike`/`null`), `created_at`, and optionally `model` (assistant), `input_tokens`/`output_tokens` (omitted when 0). Provider identifiers are never present.

#### Scenario: Assistant message fields
- **WHEN** a turn completes with usage 10/5 and the messages are listed
- **THEN** the assistant message has `model` = effective model, `input_tokens = 10`, `output_tokens = 5`, `attachments = []`, `my_reaction = null`, and the user message has no `model`

### Requirement: Message count and chronology across turns
`message_count` SHALL count non-deleted messages of the chat, and messages SHALL be listed in chronological order across turns, with soft-deleted turns' messages excluded.

#### Scenario: Two turns
- **WHEN** two turns complete
- **THEN** `message_count = 4` and the messages alternate user/assistant in send order

### Requirement: Reactions on assistant messages only
`PUT /v1/chats/{id}/messages/{msg_id}/reaction` with `{"reaction":"like|dislike"}` SHALL upsert the caller's reaction and return 200 `{message_id, reaction, created_at}`; `DELETE` SHALL return 204 whether or not a reaction existed. Invalid values return 400 `INVALID_REACTION` (checked before authorization); user/system targets return 400 `failed_precondition` with `violations[{subject: reaction_target, type: STATE}]`; unknown messages return 404 with the message resource type.

#### Scenario: Idempotent set and change
- **WHEN** the user PUTs `like` twice and then `dislike`
- **THEN** each call returns 200 and the message lists `my_reaction = "dislike"`

#### Scenario: Reaction on user message
- **WHEN** the user PUTs or DELETEs a reaction on their own user message
- **THEN** the response is 400 `failed_precondition` with subject `reaction_target`
