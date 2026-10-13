## ADDED Requirements

### Requirement: List messages
`GET /mini-chat/v1/chats/{id}/messages` SHALL return the chat's non-deleted messages in a `Page` (`items`, `page_info`), default order `created_at asc` (id tiebreak), default limit 20, max 100 (clamped), `limit=0` → 400 `INVALID_LIMIT`, OData `$filter` over `created_at`, `id`, `role` and `$orderby` over the same fields, cursor pagination, and OData errors as 400 with resource type `gts.cf.core.odata.query.v1~`. A missing/foreign/deleted chat returns 404 (chat resource type).

#### Scenario: Chronological listing
- **WHEN** two turns completed
- **THEN** the list returns user, assistant, user, assistant in that order

#### Scenario: Filter by id
- **WHEN** `$filter=id eq '<assistant_message_id>'` is requested
- **THEN** exactly that message is returned

### Requirement: Message response contract
Each message SHALL carry `id`, `request_id` (non-null; user and assistant messages of a turn share it), `role`, `content`, `attachments` (always an array of `{attachment_id, kind, filename, status, img_thumbnail?}` from `message_attachments`, non-deleted attachments only, thumbnail only for ready images), `my_reaction` (always present, `"like"`, `"dislike"` or `null`), `created_at`; and optional `model`, `input_tokens`, `output_tokens` omitted when absent or zero.

#### Scenario: User message fields
- **WHEN** messages are listed after a turn without attachments
- **THEN** the user message has `attachments: []`, `my_reaction: null`, no `model`, no `input_tokens`

#### Scenario: Assistant message fields
- **WHEN** messages are listed after a completed turn
- **THEN** the assistant message has `model` = effective model and the provider-reported token counts

### Requirement: Reactions
`PUT /chats/{id}/messages/{msg_id}/reaction` with `{"reaction":"like"|"dislike"}` SHALL upsert the caller's reaction on an assistant message and return 200 `{message_id, reaction, created_at}`. An invalid value returns 400 `INVALID_REACTION` (before authorization); a missing `reaction` field returns 422. `DELETE` SHALL remove it and return 204 whether or not one existed. Both SHALL return 400 `failed_precondition` with `violations[{subject: reaction_target, type: STATE}]` for user/system messages, 404 (message resource type) for unknown messages, and 404 (chat) for foreign chats.

#### Scenario: Idempotent like
- **WHEN** `like` is PUT twice and then `dislike`
- **THEN** each returns 200 and listing shows `my_reaction: "dislike"`

#### Scenario: Reaction on user message
- **WHEN** a reaction is PUT on a user message
- **THEN** the response is 400 `failed_precondition` with subject `reaction_target`
