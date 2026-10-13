# Outbound provider wire contract (OpenAI-compatible, via OAGW)

All calls: `ServiceGatewayClientV1::proxy_request` to `/{alias}{path}`.

## Chat (streaming) — `POST {api_path}` (default `/v1/responses`; `{model}` replaced)

```json
{
  "model": "<provider_model_id of effective model>",
  "instructions": "<system_prompt>[\n\n<file_search_guard>][\n\n<web_search_guard>]",
  "input": [
    {"role": "user", "content": [{"type":"input_text","text":"<preamble>\n\n<summary>"}]},   // if summary kept
    {"role": "user"|"assistant", "content": [{"type":"input_text"|"output_text","text":"..."}]}, // history
    {"role": "user", "content": [{"type":"input_text","text":"<msg>"}, {"type":"input_image","file_id":"file-.."}]}
  ],
  "stream": true,
  "max_output_tokens": <max_output_tokens_applied>,
  "max_tool_calls": <catalog max_tool_calls>,          // only when tools present
  "tools": [
    {"type":"file_search","vector_store_ids":["vs_.."],"max_num_results":N},
    {"type":"web_search","search_context_size":"low|medium|high"},
    {"type":"code_interpreter","container":{"type":"auto","file_ids":["file-.."]}}
  ],
  "include": ["code_interpreter_call.outputs"],         // when code_interpreter present
  "user": "<tenant_hex><user_hex>",
  "metadata": {"tenant_id","user_id","chat_id","request_type":"chat","feature":"none|file_search|..."},
  "store": false,
  "temperature"/"top_p"/...: from catalog api_params when set
}
```

Consumed SSE events (by `event:` or JSON `type`): `response.output_text.delta {delta}`,
`response.output_text.annotation.added`, `response.file_search_call.searching|completed`,
`response.web_search_call.searching|completed`, `response.code_interpreter_call.in_progress`,
`response.output_item.done {item}`, `response.completed {response{id,usage,output}}`,
`response.incomplete`, `response.failed {response{error,usage}}`, `error`.
Usage: `input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`,
`output_tokens_details.reasoning_tokens`.

## Thread summary — same path, `"stream": false`; response JSON `output[].content[].text`, `usage`.

## Files / vector stores (prefix `/v1` for openai, `/openai` + `?api-version=` for azure)

- `POST {p}/files` multipart: `purpose=assistants`, `file` (filename `{chat_id}_{attachment_id}.{ext}`) → `{id}`
- `POST {p}/vector_stores` `{name: "chat-{chat_id}"}` → `{id}`
- `POST {p}/vector_stores/{vs}/files` `{file_id, attributes:{attachment_id}}` → `{status}`
- `GET {p}/vector_stores/{vs}/files/{file_id}` → `{status}` (`in_progress|completed|failed|cancelled`)
- `DELETE {p}/files/{id}`, `DELETE {p}/vector_stores/{vs}` (2xx or 404 = success)
