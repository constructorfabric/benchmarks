# mini-chat black-box suite (`mini_chat_dev`)

Self-managed (`launcher: pytest`) end-to-end suite for the `mini-chat` gear. It
runs the real server binary and checks the gear only through what a client and
an operator can observe:

- the HTTP / SSE API under `/mini-chat/v1`;
- the gear's SQLite database (`<home_dir>/mini-chat/mini_chat.db`);
- the requests the gear sends, through OAGW, to an OpenAI-compatible mock LLM
  provider.

## Running

```bash
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
python3 -m pytest testing/e2e/suites/mini_chat_dev -q
# or: make e2e-local SUITE=mini-chat-dev   (builds the same binary, sets E2E_BINARY)
```

The suite runs only when its directory is named on the pytest command line (or
`MINI_CHAT_DEV_E2E=1` is set). A whole-tree `pytest testing/e2e` run collects it
but skips it, and `make e2e-local` / `GEAR=` runs do not include it.

`E2E_BINARY` overrides the binary (default `target/debug/cf-gears-example-server`).
`MINI_CHAT_DEV_KEEP=1` keeps the temp server home (config, logs, DB) for debugging.

## How it works

- `conftest.py` renders `server_config.yaml.tmpl` with a temp `home_dir` and
  free ports, starts the server, writes its pid to
  `/tmp/mini_chat_dev_server.pid`, waits for `GET /mini-chat/v1/models` and
  stops it by that pid on teardown (never by process name). Nothing is written
  to the repository.
- Identities (static-authn `static_tokens`, api-gateway auth enabled): users
  `A` and `B` in tenant `T1`, user `C` in tenant `T2`.
- Two provider entries, both `kind: openai_responses`, point at the mock:
  `openai` (`storage_kind: openai`, `/v1/...`) and `azure`
  (`storage_kind: azure`, `/openai/v1/responses`, `/openai/...?api-version=`).
  Provider-parametrized tests run against both. OAGW runs with
  `allow_http_upstream: true` and `ssrf_policy.enabled: false`. The providers
  have no `auth_config` (credstore secrets are not readable in this
  environment); `test_server_starts_with_unreadable_provider_secret` covers
  the deferred-provisioning path.
- `mock_llm.py` is a threaded HTTP server with two listeners. It implements
  `POST /v1/responses` (SSE when `stream: true`, JSON for the thread summary),
  `POST /v1/files`, `POST /v1/vector_stores`,
  `POST|GET /v1/vector_stores/{vs}/files[/{fid}]` and the DELETEs, and records
  every request. Tests script responses with `mock_llm.script(...)` /
  `mock_llm.script_chat(chat_id, responder)`. Builders: `text_stream`,
  `sse_events`, `held_stream` (holds the stream open until released),
  `json_response`.
- Quota scenarios seed `quota_usage` rows directly (UUIDs are 16-byte BLOBs,
  timestamps fixed-width RFC 3339). They run as user `C` and clear its rows
  before and after each test.
- The worker scans run every second (`orphan_watchdog.scan_interval_secs`,
  `upload_reaper.scan_interval_secs`), so the tests can age rows in the DB and
  observe the watchdog and the reaper.

## Files

| File | Covers |
|---|---|
| `test_chats.py` | chat CRUD, `Location`, title/model validation, list OData filter/order/paging, isolation A/B/C, 401 |
| `test_streaming.py` | SSE order and payloads, persistence, provider request contents, web search, replay and 409s, provider errors and sanitization, cancel on disconnect, ping, wire robustness |
| `test_attachments.py` | upload/get/delete, thumbnails, `input_image`, `file_search` / `code_interpreter` tools, citations, validation, `attachment_locked`, indexing failure |
| `test_quota.py` | quota status, reserve/settle, downgrade, 429 (tokens / web search / code interpreter), settlement per outcome |
| `test_mutations.py` | retry / edit / delete rules, owner scoping, concurrency, attachment carry-over, preflight on mutation |
| `test_misc.py` | models, reactions, chat-delete cleanup, usage/audit events, thread summary, auth envelope, provisioning, orphan watchdog, upload reaper, config keys, kill switches, concurrent load |
