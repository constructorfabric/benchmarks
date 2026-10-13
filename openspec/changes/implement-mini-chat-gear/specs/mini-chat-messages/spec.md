## ADDED Requirements

### Requirement: List messages
`GET /v1/chats/{id}/messages` SHALL return the chat's non-deleted messages as `{items, page_info}` in `created_at asc, id asc` order by default, with `limit` (default 20, clamped to 100, `0` → 400), cursor pagination and OData `$filter`/`$orderby` over `created_at`, `id`, `role` (`$select` accepted and ignored). Errors follow the chat-list OData rules; a missing/foreign chat returns 404 (chat resource type).

#### Scenario: Filter by role
- **WHEN** messages are listed with `$filter=role eq 'assistant'`
- **THEN** only assistant messages are returned

#### Scenario: Filter by id
- **WHEN** messages are listed with `$filter=id eq '<assistant_message_id>'`
- **THEN** exactly that message is returned

### Requirement: Message response contract
Every message item SHALL contain `id`, `request_id` (never null; user and assistant messages of a turn share it), `role`, `content`, `attachments` (always an array of `{attachment_id, kind, filename, status, img_thumbnail?}` built from `message_attachments` joined to non-deleted attachments; `img_thumbnail` only for ready images), `my_reaction` (always present: `like`, `dislike` or `null`), `created_at`, and optionally `model` (assistant messages), `input_tokens` and `output_tokens` (omitted when 0). Provider identifiers SHALL never appear.

#### Scenario: User message fields
- **WHEN** a completed turn's messages are listed
- **THEN** the user message has `attachments: []`, `my_reaction: null`, no `model`, and the same `request_id` as the assistant message, which carries `model`, `input_tokens` and `output_tokens`

### Requirement: Message counting and chronology
`message_count` SHALL count non-deleted messages of the chat; each completed turn adds a user and an assistant message in chronological order; soft-deleted (mutated) turns' messages SHALL be excluded from lists and counts.

#### Scenario: Two turns
- **WHEN** two turns complete in a chat
- **THEN** `message_count` is 4 and the messages are listed user, assistant, user, assistant

### Requirement: Reactions
`PUT /v1/chats/{id}/messages/{msg_id}/reaction` with `{"reaction": "like"|"dislike"}` SHALL upsert the caller's reaction on an assistant message and return 200 `{message_id, reaction, created_at}`; `DELETE` SHALL remove it and return 204 whether or not one existed. A value other than `like`/`dislike` SHALL return 400 `INVALID_REACTION` (before authorization); a body without `reaction` SHALL return 422. A user or system message target SHALL return 400 `failed_precondition` (`violations[{subject: reaction_target, type: STATE}]`) for both PUT and DELETE; an unknown message SHALL return 404 (message resource type) and an unknown chat 404 (chat resource type). The reaction SHALL be visible as `my_reaction` in message lists.

#### Scenario: Change reaction
- **WHEN** a user puts `like` then `dislike` on the same assistant message
- **THEN** both return 200 and the message lists `my_reaction: "dislike"`, with one row in `message_reactions`

#### Scenario: Reaction on user message
- **WHEN** a reaction is put on a user message
- **THEN** the response is 400 `failed_precondition` with subject `reaction_target`
