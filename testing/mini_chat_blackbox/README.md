# mini-chat black-box suite

End-to-end tests of the `mini-chat` gear through the real example server
(`cf-gears-example-server`), with every provider call answered by a local scripted
OpenAI-compatible mock. No network access beyond `127.0.0.1` is needed.

## Run

```sh
testing/mini_chat_blackbox/run.sh            # build the server, then run the suite
SKIP_BUILD=1 testing/mini_chat_blackbox/run.sh -k reaction   # reuse the binary, filter tests
```

Requirements: Python 3 with `aiohttp`, `httpx`, `pytest`. The server is built with
`--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore`.
`MINI_CHAT_SERVER_BIN` overrides the binary path; `MINI_CHAT_BB_KEEP_LOG=<file>` copies the
server log there after the run.

## How it works

- `conftest.py` (session fixtures) starts `mock_provider.py` on a free port, writes a server
  config into a temporary directory and starts the server with
  `cf-gears-example-server --config <file> run`, then waits for `GET /mini-chat/v1/models` → 200.
  Both child processes are stopped by their recorded pid on teardown (also on failure); the
  temporary directory (server home with `mini-chat/mini_chat.db`, logs) is removed afterwards.
- Server config: api-gateway on a free port with `auth_disabled: true`,
  `defaults.body_limit_bytes: 64000000` and both mini-chat throttling zones; `oagw` with
  `allow_http_upstream: true` and `ssrf_policy.enabled: false`; mini-chat with one provider
  `openai` (`openai_responses`, storage `openai`, `127.0.0.1:<mock port>` over HTTP, no auth
  plugin); static authn (`accept_all` + S2S credentials), static authz, single tenant,
  credstore + static credstore; the static model policy plugin with a premium (`bb-premium`,
  default) and a standard (`bb-standard`) model.
- `mock_provider.py` implements `POST /v1/responses` (scripted Responses SSE: `Hello from mock`,
  usage 12/3; a request containing `[slow]` is held before the first delta until the test calls
  `POST /_mock/release`; a release that arrives before the held request is kept for the next
  `[slow]` request), `POST /v1/files`,
  `DELETE /v1/files/{id}`, `POST /v1/vector_stores`, `POST /v1/vector_stores/{id}/files`,
  `GET /v1/vector_stores/{id}/files/{fid}`, `DELETE /v1/vector_stores/{id}`, and records every
  request (`GET /_mock/requests`).
- `test_blackbox.py` drives the REST/SSE API with `httpx` and reads persisted state from
  `mini_chat.db` with `sqlite3` (UUID columns are 16-byte BLOBs).

## Coverage

Chat create/list/get/rename/delete; streaming a message (SSE order, `done` usage, turn
`completed`, user + assistant messages, settled `quota_usage` without reserve); replay of a
completed turn (no provider call); turn status; quota status; models list/get; PDF and image
upload (provider file names, vector store, thumbnail, no provider ids in the API); chat delete →
the mock receives `DELETE /v1/vector_stores/{id}` and the file delete; a parallel turn in one chat
→ 409 `turn_already_running` with no leftover turn or reserve; reaction set/replace/list/delete.
