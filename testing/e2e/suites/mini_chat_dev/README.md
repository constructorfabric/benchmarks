# mini-chat black-box e2e suite (`mini_chat_dev`)

Black-box tests of the mini-chat gear: a real `cf-gears-example-server` process
is booted with a generated config, and a stdlib mock OpenAI/Azure provider
stands in for the LLM. Tests talk to the gear over HTTP and may inspect the
gear's SQLite DB. The suite is self-contained (it does not use the repo-wide
`test_env` fixtures) and may coexist with other suites in one pytest session.

## Running

```bash
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
cd testing/e2e && python3 -m pytest suites/mini_chat_dev -q
```

| Env var | Default | Meaning |
|---|---|---|
| `E2E_BINARY` | `target/debug/cf-gears-example-server` | server binary |
| `MINI_CHAT_E2E_PORT` | `8086` | gear HTTP port (dedicated; mock uses a free port) |
| `MINI_CHAT_E2E_HEALTH_TIMEOUT` | `90` | seconds to wait for `/healthz` |
| `MINI_CHAT_E2E_TEST_TIMEOUT` | `120` | per-test pytest-timeout (repo `pytest.ini` says 10) |
| `MINI_CHAT_E2E_KEEP` | unset | keep the temp dir (config, `server.log`, DB) after the run |

The server is stopped by the pid the fixture started (never `pkill`).

Import suite modules package-relatively (`from .helpers import api, create_chat`, `from . import mock_provider as mp`): the repo-wide `testing/e2e/helpers/` package would shadow a top-level `helpers`.

## Fixtures (`conftest.py`)

* `mock` (session) - started `MockProvider`; `mock.port`, `mock.base_url`, `mock.enqueue(...)`, `mock.requests(...)`, `mock.reset()`.
* `reset_mock` (function) - resets the mock before and after the test, yields it. Use it in every test that scripts or inspects the mock.
* `server` (session) - writes `config/e2e-local.yaml` patched for mini-chat, boots the server, waits for `/healthz`, yields `base_url`. Request it (or `pytestmark = pytest.mark.usefixtures("server")`) in every test that talks to the gear.
* `server_ctl` (function) - the running `ServerProcess`: `crash_and_restart()` (SIGKILL by the server's own pid, then boot again over the same config and DB and wait for `/healthz`), `kill()`, `start()`, `stop()`. A test that restarts the server must leave it running for the rest of the session (`test_recovery.py`).
* `outbox_capture` (module) - copies every outbox message enqueued while the module runs into the table named by the module's `CAPTURE` constant (AFTER INSERT trigger on `toolkit_outbox_incoming`; handlers vacuum outbox rows within seconds); trigger and table are dropped at module teardown. Yields the table name; read it with `captured_outbox(table, payload_type=None)` -> `[(queue, payload_type, payload_dict)]`.

Generated config (see `build_config`): `HOME`/`server.home_dir` is a temp dir (DB at `<tmp>/.cf-gears/mini_chat/mini_chat.db`); `types-registry.config.entities: []` (the seeded entity needs an account-management schema that is not in this feature set); `oagw.config` `proxy_timeout_secs: 10`, `allow_http_upstream: true`, SSRF off; `rl_mini_chat_chat` throttle lifted to 1000/s; grpc-hub UDS path in the temp dir; mini-chat providers

| provider | kind | upstream alias | storage | api path |
|---|---|---|---|---|
| `openai` | `openai_responses` | `mock-openai` | `openai` | `/v1/responses` |
| `azure` | `openai_responses` | `mock-azure` | `azure` (`api_version: 2025-03-01-preview`) | `/openai/v1/responses` |

(both `127.0.0.1:<mock port>`, plain HTTP, no auth), `streaming.sse_ping_interval_seconds: 5` (pings are only observable below OAGW's 10 s proxy timeout), `orphan_watchdog {timeout_secs: 90, scan_interval_secs: 1}`, `upload_reaper {scan_interval_secs: 1, stale_after_secs: 60}`.

Model catalog (static policy plugin):

| model | tier | provider | notes |
|---|---|---|---|
| `gpt-premium` | Premium | openai | default; vision; web_search + file_search + code_interpreter |
| `gpt-standard` | Standard | openai | vision; file_search + code_interpreter |
| `gpt-novision` | Standard | openai | no vision |
| `gpt-azure` | Standard | azure | vision |
| `gpt-tiny` | Standard | openai | `context_window 4096`, `max_output_tokens 1024`, `max_input_tokens 3072` |
| `gpt-disabled` | Standard | openai | `enabled: false` (not listed) |
| `gpt-4.1-mini` | Standard | openai | enabled; summary model |

Tokens (static-authn): `e2e-token-tenant-a` (user `1111...`), `e2e-token-tenant-a-reviewer` (another user, same tenant), `e2e-token-tenant-b` (other tenant).

## Helpers (`helpers.py`)

* `api(token=TOKEN_A)` - `requests` session with Bearer auth and a base URL; use relative URLs (`api().get("/mini-chat/v1/models")`). `api(None)` sends no auth. Constants `TOKEN_A`, `TOKEN_A_REVIEWER`, `TOKEN_B`, `PREFIX = "/mini-chat/v1"`.
* `create_chat(session, **body)` - `POST /chats`, asserts 201, returns the JSON.
* `stream(session, chat_id, body, path=None) -> SseResult` - runs `messages:stream` (or `path`) to completion. `SseResult.status`, `.events: list[(name, data)]`, `.raw`, `.names()`, `.of(name)`, `.terminal`, `.text()`. A non-200 or non-SSE answer gives empty `events`.
* `db()` - sqlite3 connection (Row factory) to the gear DB, located by searching the home dir for `mini_chat.db`; `uuid_bytes(u)` converts a UUID string to the 16-byte BLOB used by id columns.
* `wait_until(fn, timeout=15, interval=0.1, message=...)` - poll until truthy (returns the value) or raise `AssertionError`.
* `assert_problem(resp, status, category, **context_checks)` - asserts a canonical Problem (`application/problem+json`, `type` = `gts://gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~`, no top-level `code`) and returns the JSON. `reason=` matches `context.reason` or any `field_violations[].reason`; `field=`; `violation_type=`; other kwargs compare `context[key]` (e.g. `resource_type=`).
* Quota rows of a chat's owner: `OWNER_QUOTA` / `owner_args(chat_id)` (SQL predicate + binds), `CURRENT_PERIODS` (today / this month), `owner_quota_rows(chat_id, columns, where, group_by)`, `reserved_credits(chat_id)`, and `exhausted_quota(chat_id, where)` (context manager: absurd spend on the matching rows, restored on exit).

## Direct seeding (`_seed.py`)

For rows the API cannot create yet: `insert_message(chat_id, role, created_at, ...)`, `insert_turn(chat_id, request_id, state, ...)`, and `ts(second, micros)` (gear timestamp text format, so seeded rows sort like gear-written ones).

## Mock provider (`mock_provider.py`)

Stdlib threaded server on a free port; accepts both OpenAI (`/v1/...`) and Azure (`/openai/v1/...`, `/openai/files`, ...) shapes (the optional `/openai` and `/v1` prefixes are stripped), with or without `?api-version=`. No auth is checked (headers are recorded).

Data plane: `POST /responses` (SSE if body `"stream": true`, else JSON), `POST /files` (multipart -> `{"id": "file-<24 alnum>"}`), `DELETE /files/{id}`, `POST /vector_stores` (`vs_<24 alnum>`), `POST /vector_stores/{id}/files` and `GET /vector_stores/{id}/files/{fid}` (status from script, default `completed`), `DELETE /vector_stores/{id}`. SSE frames are `event: <type>\ndata: {"type": <type>, "sequence_number": n, ...}\n\n` (chunked transfer).

Control plane (also available as Python methods on the `mock` fixture):

* `POST /__control/reset` / `mock.reset()` - clears queues, recorded requests, vector-store file state, and releases hanging requests.
* `POST /__control/enqueue {"route": key, "script": {...}, "repeat": 1}` / `mock.enqueue(key, script, repeat=1)` - FIFO per route key; `repeat` <= 0 means "forever" (sticky default).
* `GET /__control/requests[?method=&path=&route=]` / `mock.requests(method=, path=, route=)` - recorded requests: `seq, method, path, query (dict of lists), headers (lower-case), json, multipart_fields, multipart [{name, filename, content_type, size, value}], route, response_status, client_disconnected, events_sent`.

Route keys: `responses`, `summary`, `files`, `vector_stores` (store creation), `vector_store_file_status`, `delete_file`, `delete_vector_store`. A `/responses` request whose body `model` is `gpt-4.1-mini` (the summary model) uses the `summary` queue and defaults to a `<analysis>..</analysis><summary>..</summary>` text with usage 20/8; everything else uses `responses`.

### Default `responses` script
`response.created`, `response.output_text.delta` ("Hello"), `response.output_text.delta` (" world"), `response.completed` with `response.usage = {input_tokens: 12, output_tokens: 5}` (non-stream JSON: `{"id", "object": "response", "output_text": "Hello world", "output": [...], "usage"}`). `response.id` is generated once per request and injected into every event whose data has a `response` object.

### Script keys
* `responses` / `summary`: `events` (list of `{"type", "data": {...}, "event": <sse name override>, "delay_ms": <delay before this event>}`; build them with `ev_*` helpers), `delay_ms` (gap between consecutive events), `start_delay_ms` (before the HTTP response), `status` >= 400 + `body` (+ `headers`) for a plain JSON HTTP error (no stream), `failed: {"code", "message", "usage"?}` (appends `response.failed`; with no `events` the stream is `response.created` + `response.failed`), `hang: true` (send `events` and then neither terminal event nor EOF), `disconnect: true` (send `events` then drop the connection without the terminating chunk), `headers`, and for non-stream requests `body`.
* all other routes: `status` (HTTP, default 200), `body` (JSON), `headers`, `delay_ms`, `hang`; `files`/`vector_stores` also `id`; `vector_store_file_status` also `file_status` (`in_progress|completed|failed|cancelled`; both the POST attach and the GET poll pop one entry; once drained GET returns the last status recorded for that file).

Event builders (all return JSON-serialisable dicts): `ev(type, data)`, `ev_created`, `ev_delta(text)`, `ev_completed(usage, text)`, `ev_incomplete`, `ev_failed`, `ev_file_search(done)`, `ev_web_search(done)`, `ev_code_interpreter_start`, `ev_code_interpreter_done(logs)`, `ev_annotation`, `ev_url_citation`, `ev_file_citation`, `text_events(*chunks, usage=)`.

```python
def test_provider_failure(server, reset_mock):
    reset_mock.enqueue("responses", {"failed": {"code": "server_error", "message": "boom"}})
    chat = create_chat(api())
    res = stream(api(), chat["id"], {"content": "hi"})
    assert res.terminal[0] == "error"
    assert reset_mock.requests(path="/responses")[0]["json"]["model"] == "gpt-premium"
```

`test_mock_provider.py` drives the mock directly (no gear server) and documents its contract.
