# REST API contract (mini-chat)

Base prefix: `url_prefix` (default `/mini-chat`), all routes `/v1/...`. Schemas: the mini-chat subset
of `docs/api/api.json` in `../openapi-mini-chat.json` (operation ids `mini_chat.*`, tags
`Mini Chat Chats|Messages|Attachments|Turns|Reactions|Models|Quotas`). All routes require
authentication and the platform base license feature. Errors: RFC 9457 `Problem`
(`application/problem+json`) per ADR-0004.

| Method | Path | Op id | Success | Authz action (resource) |
|---|---|---|---|---|
| POST | /v1/chats | create_chat | 201 ChatDetailDto + `Location: /mini-chat/v1/chats/{id}` | create (chat) |
| GET | /v1/chats | list_chats | 200 Page_ChatDetailDto | list (chat) |
| GET | /v1/chats/{id} | get_chat | 200 ChatDetailDto | read |
| PATCH | /v1/chats/{id} | update_chat | 200 ChatDetailDto | update |
| DELETE | /v1/chats/{id} | delete_chat | 204 | delete |
| GET | /v1/chats/{id}/messages | list_messages | 200 Page_MiniChatMessageDto | list_messages |
| POST | /v1/chats/{id}/messages:stream | stream_message | 200 text/event-stream | send_message |
| POST | /v1/chats/{id}/attachments | upload_attachment | 201 AttachmentDetailDto | upload_attachment |
| GET | /v1/chats/{id}/attachments/{attachment_id} | get_attachment | 200 AttachmentDetailDto | read_attachment |
| DELETE | /v1/chats/{id}/attachments/{attachment_id} | delete_attachment | 204 | delete_attachment |
| GET | /v1/chats/{id}/turns/{request_id} | get_turn | 200 TurnStatusResponse | read_turn |
| POST | /v1/chats/{id}/turns/{request_id}/retry | retry_turn | 200 SSE | retry_turn |
| PATCH | /v1/chats/{id}/turns/{request_id} | edit_turn | 200 SSE | edit_turn |
| DELETE | /v1/chats/{id}/turns/{request_id} | delete_turn | 204 | delete_turn |
| PUT | /v1/chats/{id}/messages/{msg_id}/reaction | put_reaction | 200 MiniChatReactionDto | set_reaction |
| DELETE | /v1/chats/{id}/messages/{msg_id}/reaction | delete_reaction | 204 | delete_reaction |
| GET | /v1/models | list_models | 200 ModelListDto | list (model, no constraints) |
| GET | /v1/models/{id} | get_model | 200 ModelDto | read (model, no constraints) |
| GET | /v1/quota/status | get_quota_status | 200 QuotaStatusResponse | read (user_quota) |

Resource types: chat `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`, model
`gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~`, user quota
`gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`.
Not-found resource types in errors: `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`.

Error mapping table: ADR-0004 (copied verbatim into implementation as the error module's tests).
Optional JSON fields are omitted when null (title, description, error_code, assistant_message_id,
model, input_tokens/output_tokens when 0, img_thumbnail, doc_summary, summary_updated_at).
