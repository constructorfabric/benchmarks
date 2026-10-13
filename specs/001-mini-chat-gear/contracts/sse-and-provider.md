# Contract: SSE stream and LLM provider wire format

## SSE (client side) — DESIGN §3.3 "SSE Event Definitions"

Order: `stream_started ping* (delta | tool)* citations? (done | error)`; connection closed right after
the terminal event; `ping` only before the first delta/tool (every `sse_ping_interval_seconds`).

- `stream_started`: `{request_id, message_id, is_new_turn, thread_summary_applied?{token_estimate}}`
- `delta`: `{type: "text"|"reasoning", content}`
- `tool`: `{phase: "start"|"done", name, details}`
- `citations`: `{items: [{source, title, snippet, url?, attachment_id?, span?{start,end}}]}`
- `done`: `{usage{input_tokens,output_tokens}, effective_model, selected_model, quota_decision,
  downgrade_from?, downgrade_reason?, quota_warnings?[{tier,period,remaining_percentage,warning,
  exhausted,next_reset?}]}`
- `error`: `{code, message}` codes: provider_error, provider_timeout, rate_limited,
  web_search_calls_exceeded, code_interpreter_calls_exceeded, agentic_iterations_exceeded,
  unexpected_tool_use, message_persistence_failed, finalization_failed, stream_interrupted.
- Replay: `stream_started{is_new_turn:false, message_id: persisted id}`, one `delta` with full text,
  `done` (rebuilt decision, no `downgrade_reason`, no `quota_warnings`).

## Provider (OpenAI Responses adapter, `kind: openai_responses`)

Request: `POST {alias}{api_path}` via OAGW (`api_path` default `/v1/responses`; `{model}` replaced by
`provider_model_id`), JSON body:

```
{ "model": provider_model_id, "instructions": system_prompt + guards,
  "input": [ {role:"user", content:"<summary preamble>\n\n<summary>"}?,
             {role:"user"|"assistant", content:"..."}...,
             {role:"user", content:"text" | [{type:"input_text"},{type:"input_image",file_id}...]} ],
  "stream": true, "store": false, "max_output_tokens": max_output_tokens_applied,
  "max_tool_calls": catalog max_tool_calls (only when tools are sent),
  "tools": [ {"type":"file_search","vector_store_ids":[vs],"max_num_results":k},
             {"type":"web_search","search_context_size":"low|medium|high"},
             {"type":"code_interpreter","container":{"type":"auto","file_ids":[...]}} ]?,
  "include": ["code_interpreter_call.outputs"]? ,
  "user": "{tenant_hex}{user_hex}",
  "metadata": {tenant_id,user_id,chat_id,request_type:"chat",feature},
  temperature/top_p/frequency_penalty/presence_penalty (when set), extra_body keys merged }
```

Stream events parsed (event name from `event:` line or `data.type`): `response.output_text.delta`,
`response.output_text.annotation.added`, `response.file_search_call.searching|completed`,
`response.web_search_call.searching|completed`, `response.code_interpreter_call.in_progress`,
`response.output_item.done` (code_interpreter_call), `response.completed`, `response.incomplete`,
`response.failed`, `error`. Usage from `response.usage` (`input_tokens`, `output_tokens`,
`input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens`).

Thread summary call: same endpoint, `stream: false`, `metadata.request_type = "summary"`,
`feature = "none"`, system user id `11111111-6a88-4768-9dfc-6bcd5187d9ed`.

Files/vector stores (`storage_kind: openai` prefix `/v1`; `azure` prefix `/openai` + `?api-version=`):
`POST {prefix}/files` (multipart `purpose=assistants`, `file`), `DELETE {prefix}/files/{id}`,
`POST {prefix}/vector_stores`, `POST {prefix}/vector_stores/{vs}/files` (`{file_id, attributes:
{attachment_id}}`), `GET {prefix}/vector_stores/{vs}/files/{file_id}`, `DELETE
{prefix}/vector_stores/{vs}`.
