# Provider wire contract (OpenAI-compatible, through OAGW)

All requests: `ServiceGatewayClientV1::proxy_request` to `/{alias}{path}`; OAGW injects credentials.

## Chat (kind `openai_responses`): `POST {alias}{api_path}` (`{model}` → provider_model_id)

```json
{
  "model": "<provider_model_id>",
  "instructions": "<system_prompt>[\n\n<file_search_guard>][\n\n<web_search_guard>][\n\n<knowledge guard>]",
  "input": [
    {"role": "user", "content": "<thread summary preamble>\n\n<summary>"},
    {"role": "user", "content": "<history user text>"},
    {"role": "assistant", "content": "<history assistant text>"},
    {"role": "user", "content": [
      {"type": "input_text", "text": "<current message>"},
      {"type": "input_image", "file_id": "<provider_file_id>"}
    ]}
  ],
  "stream": true,
  "store": false,
  "max_output_tokens": "<max_output_tokens_applied>",
  "tools": [
    {"type": "file_search", "vector_store_ids": ["<vs id>"], "max_num_results": "<catalog max_num_results>"},
    {"type": "web_search", "search_context_size": "low|medium|high"},
    {"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["..."]}}
  ],
  "max_tool_calls": "<catalog max_tool_calls>",
  "include": ["code_interpreter_call.outputs"],
  "user": "<tenant_hex><user_hex>",
  "metadata": {"tenant_id": "...", "user_id": "...", "chat_id": "...", "request_type": "chat", "feature": "none|file_search|web_search|..."},
  "temperature": 0.7, "top_p": 1.0
}
```

`tools`/`max_tool_calls`/`include` are omitted when no tool is sent. Optional `api_params` are sent
only when set; `extra_body` keys merged at top level except reserved keys.

SSE events parsed (event name from `event:` line or `type`): `response.created`,
`response.output_text.delta` (→ delta), `response.output_text.annotation.added` and
`response.output_item.done`/`response.completed` output annotations (→ citations),
`response.file_search_call.searching|completed`, `response.web_search_call.searching|completed`,
`response.code_interpreter_call.in_progress`, `response.output_item.done` (code_interpreter_call
logs), `response.completed` / `response.incomplete` (usage → done), `response.failed`, `error`.
HTTP 429 → `rate_limited` (Retry-After), other non-2xx → `provider_error`, gateway timeout / 504
`deadline_exceeded` → `provider_timeout`.

Thread summary: same endpoint, `stream: false`, `request_type: "summary"`, system user id
`11111111-6a88-4768-9dfc-6bcd5187d9ed`, `max_output_tokens` = summary model catalog value.

## Files / vector stores

| Operation | storage_kind openai | storage_kind azure |
|---|---|---|
| upload file | `POST {alias}/v1/files` multipart (`purpose=assistants`, `file`) | `POST {alias}/openai/files?api-version=V` |
| delete file | `DELETE {alias}/v1/files/{id}` | `DELETE {alias}/openai/files/{id}?api-version=V` |
| create vector store | `POST {alias}/v1/vector_stores` `{name, metadata}` | `POST {alias}/openai/vector_stores?api-version=V` |
| add file | `POST {alias}/v1/vector_stores/{vs}/files` `{file_id, attributes: {attachment_id}}` | azure equivalent |
| file status | `GET {alias}/v1/vector_stores/{vs}/files/{file_id}` | azure equivalent |
| delete vector store | `DELETE {alias}/v1/vector_stores/{vs}` | azure equivalent |

Provider-side filename: `{chat_id}_{attachment_id}.{ext}`. 2xx and 404 on delete = success.
