# Contract: Mini Chat REST API

Reference schema: `docs/api/api.json` (operations `mini_chat.*`), paths served under the gear
`url_prefix` (default `/mini-chat`). Behavior: DESIGN §3.3, §3.9; errors: ADR-0004.

| Method | Path | operationId | Success | Body / Response DTO |
|---|---|---|---|---|
| GET | /v1/chats | mini_chat.list_chats | 200 | `Page_ChatDetailDto` (limit, cursor, $filter, $orderby) |
| POST | /v1/chats | mini_chat.create_chat | 201 + `Location` | `CreateChatReq` → `ChatDetailDto` (throttling zones `rl_mini_chat_chat` / `ifl_mini_chat_chat`, dry run) |
| GET | /v1/chats/{id} | mini_chat.get_chat | 200 | `ChatDetailDto` |
| PATCH | /v1/chats/{id} | mini_chat.update_chat | 200 | `UpdateChatReq` → `ChatDetailDto` |
| DELETE | /v1/chats/{id} | mini_chat.delete_chat | 204 | — |
| GET | /v1/chats/{id}/messages | mini_chat.list_messages | 200 | `Page_MiniChatMessageDto` |
| POST | /v1/chats/{id}/messages:stream | mini_chat.stream_message | 200 SSE | `StreamMessageRequest` → `MiniChatSseEvent`* |
| POST | /v1/chats/{id}/attachments | mini_chat.upload_attachment | 201 | multipart `file` → `AttachmentDetailDto` |
| GET | /v1/chats/{id}/attachments/{attachment_id} | mini_chat.get_attachment | 200 | `AttachmentDetailDto` |
| DELETE | /v1/chats/{id}/attachments/{attachment_id} | mini_chat.delete_attachment | 204 | — |
| GET | /v1/chats/{id}/turns/{request_id} | mini_chat.get_turn | 200 | `TurnStatusResponse` |
| POST | /v1/chats/{id}/turns/{request_id}/retry | mini_chat.retry_turn | 200 SSE | — |
| PATCH | /v1/chats/{id}/turns/{request_id} | mini_chat.edit_turn | 200 SSE | `EditTurnRequest` |
| DELETE | /v1/chats/{id}/turns/{request_id} | mini_chat.delete_turn | 204 | — |
| PUT | /v1/chats/{id}/messages/{msg_id}/reaction | mini_chat.put_reaction | 200 | `SetReactionReq` → `MiniChatReactionDto` |
| DELETE | /v1/chats/{id}/messages/{msg_id}/reaction | mini_chat.delete_reaction | 204 | — |
| GET | /v1/models | mini_chat.list_models | 200 | `ModelListDto` |
| GET | /v1/models/{id} | mini_chat.get_model | 200 | `ModelDto` |
| GET | /v1/quota/status | mini_chat.get_quota_status | 200 | `QuotaStatusResponse` |

All routes: authenticated, license feature `gts.cf.core.lic.feat.v1~cf.core.global.base.v1`.

JSON omission rules: `ChatDetailDto.title` omitted when null; `MiniChatMessageDto` always has
`attachments` and `my_reaction` (nullable), omits `model`/`input_tokens`/`output_tokens` when absent
or zero; `AttachmentDetailDto` omits null optionals; `TurnStatusResponse` omits null
`error_code`/`assistant_message_id`; `DoneData.downgrade_*` and `quota_warnings` omitted when not
applicable; `QuotaWarning.next_reset` only when warning or exhausted.

Error mapping: the full table of DESIGN §3.3 "Error Codes" / ADR-0004 (category, status, reason).
Not-found resource types: `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`.
