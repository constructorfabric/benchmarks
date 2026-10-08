# mini-chat black-box E2E suite

A Python end-to-end suite for the `mini-chat` gear. It starts the real server binary
against an in-process mock of an OpenAI-compatible provider, reached through OAGW, and checks
the public REST/SSE contract, the gear's SQLite state and the requests the provider received.
The suite does not import or change any Rust code.

## Running

```bash
# 1. build the server (from the repository root)
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore

# 2. run the suite (from the repository root)
python -m pytest testing/e2e/mini_chat_impl -x -q

# self-tests only (no server needed): mock provider + helpers
python -m pytest testing/e2e/mini_chat_impl -m noserver -q

# check that every file imports
python -m pytest --collect-only -q testing/e2e/mini_chat_impl
```

Requirements: Python 3.11 with `pytest`, `pytest-asyncio`, `pytest-timeout`, `httpx` and
`PyYAML`. The only network traffic is to localhost.

Environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `MC_E2E_BINARY` | `target/debug/cf-gears-example-server` | server binary |
| `MC_E2E_STARTUP_TIMEOUT` | `120` | seconds to wait for `/healthz` |
| `MC_E2E_KEEP_HOME` | unset | keep the temporary `server.home_dir` (config, logs, DBs) |
| `MC_E2E_LOG_LEVEL` | `info` | file log level of the server |

`pytest.ini` sets `asyncio_mode = auto` and a default timeout of 120 s per test. Tests that
may hang have their own `pytest.mark.timeout`. Tests marked `slow` take more than about 20 s
(document indexing deadline, summary retry).

## How it works

* `conftest.py` picks free ports and starts the mock (`mock_llm` fixture). It writes a server
  YAML config to a temporary `server.home_dir` and starts
  `cf-gears-example-server --config <file> run` with `subprocess.Popen`. It waits for
  `GET /healthz` and then for `GET /mini-chat/v1/models` to answer 200. At teardown it stops
  the server **only through its Popen handle** (terminate, then kill); an `atexit` hook does
  the same as a safety net. A failed start also stops the process.
* `ks_server` is a second server, started on first use, whose static model policy plugin has
  `kill_switches: {disable_web_search, disable_images, disable_code_interpreter,
  force_standard_tier}` set to `true`. Only the kill-switch tests use it.
* `mchelpers.py` provides:
  * `Api`: an httpx client bound to a token. It has `stream`, `retry` and `edit` methods that
    collect `(event, data)` pairs, and it can disconnect after K events or when a predicate
    matches.
  * `BackgroundStream`: a stream running in a thread. `stop()` shuts the socket down, so the
    server sees the client disconnect even on a silent stream.
  * `DB`: sqlite3 access with `busy_timeout`. UUIDs are passed as 16-byte blobs (`ub()`). It
    can seed `quota_usage` rows and searches outbox payloads loosely.
  * `assert_problem`: checks canonical `Problem` responses (category in `type`,
    `field_violations[].reason`, `violations[].subject/type`, `reason`, `resource_type`,
    `resource_name`).
  * `assert_sse_grammar`: checks `stream_started ping* (delta|tool)* citations? (done|error)`.
* Each test uses a nonce in its message text, so the provider requests of that test can be
  found in the mock's records. Quota-sensitive tests use their own users (`tok-q*`, `tok-k*`,
  `tok-l*`), so seeded `quota_usage` rows do not affect other tests.

## Mock provider directive protocol (`mock_llm.py`)

The mock picks the behaviour of `POST /v1/responses` from directives in the **last user
message** of the request `input`. The content can be a string or a list of `input_text` parts.

| Directive | Provider behaviour |
|---|---|
| none | `response.created`, deltas `"Hello"` and `" world"`, `response.completed` with usage 10/5 |
| `[[usage:I:O]]` | custom usage on the terminal event |
| `[[long:N]]` | N deltas |
| `[[slow]]` / `[[slow:N:S]]` | first delta at once, then one delta every S s (default 20 deltas, 3 s apart) |
| `[[delay:S]]` | waits S s before the first delta (ping tests) |
| `[[hang]]` | one delta, then keeps the stream open (SSE comments every 1 s) until the client disconnects or the mock is reset/released |
| `[[fail]]` | `response.failed` with a message that contains `file-abcdefghijklmnopqrstu`, a `resp_` id, a URL and an `sk-` key |
| `[[error_event]]` | top-level SSE `error` event (message contains a `vs_` id and a URL) |
| `[[http500]]` / `[[http429]]` | plain HTTP 500 JSON error / HTTP 429 with `Retry-After: 7` |
| `[[websearch:N]]` | N `response.web_search_call.searching`/`.completed` pairs plus one `url_citation` (`https://example.com/mock-article`, span 0..5) |
| `[[filecite]]` | `file_search_call` events plus a `file_citation` for the newest file in the vector store named by the request's `file_search` tool (fallback: the newest uploaded file); `[[filecite:unknown]]` cites an unknown file id |
| `[[codeint:N]]` | N `code_interpreter_call.in_progress` events plus `response.output_item.done` items with logs `ci-output-<i>` |
| `[[incomplete]]` | deltas, then `response.incomplete` |
| `[[empty]]` | `response.completed` without deltas |

Every response id is `resp_` followed by 24 alphanumeric characters, and it is recorded on
the request record (`response_id`). A non-streaming `/v1/responses` request (`stream` false or
absent; the thread-summary worker) returns
`<analysis>…</analysis><summary>MOCK-SUMMARY-MARKER …</summary>`.

Files and vector stores:

* `POST /v1/files` (multipart; plain or chunked body) returns `file-<24>`.
* `DELETE /v1/files/{id}` returns 200, or 404 for an unknown or already deleted file.
* `POST /v1/vector_stores` returns `vs_<24>`.
* `POST /v1/vector_stores/{vs}/files` returns a status taken from `index_status`.
* `GET /v1/vector_stores/{vs}/files/{id}` returns the stored status, or `poll_status` when it
  is set.
* `DELETE /v1/vector_stores/{vs}` returns 200 or 404.

The mock routes on the part of the path after `/v1`.

Controls (`MockLLM.configure(...)` or `POST /__mock/config`): `files_fail`, `files_delay`,
`file_delete_fail_count`, `vs_create_fail`, `vs_file_fail`, `index_status`, `poll_status`,
`summary_fail: {match, count}`. Other control endpoints are `GET /__mock/requests`,
`POST /__mock/reset`, `POST /__mock/release` (ends hanging streams) and `GET /__mock/state`.
Every request is recorded with its method, path, query, lower-cased headers (including
`authorization`), JSON body and multipart metadata.

## Server configuration written by the fixture

* `database.servers.sqlite_mc` (SQLite, WAL). Gear databases: `mini_chat.db`,
  `credstore.db` and `types_registry.db`.
* `api-gateway`: `127.0.0.1:<port>`, auth on, `require_auth_by_default`, a 64 MB body limit,
  and the `rl_mini_chat_chat` / `ifl_mini_chat_chat` throttling zones with generous limits.
* `static-authn-plugin` in `static_tokens` mode:
  * `tok-a` (user `11111111-…`, tenant A `00000000-df51-…`)
  * `tok-a2` (user `44444444-…`, tenant A)
  * `tok-b` (user `22222222-…`, tenant `bbbbbbbb-…`)
  * `tok-q1..12`, `tok-l1..4` and `tok-k1..4` (tenant-A users for quota, list and isolation
    tests)
  * S2S client `mini-chat`
* `static-credstore-plugin`: the shared secret `openai-key = sk-test-e2e-fake-key`.
* `oagw`: `allow_http_upstream`, SSRF policy off, proxy timeout 30 s.
* `mini-chat`:
  * provider `openai` (`openai_responses`, `127.0.0.1:<mock>`, `use_http`, alias
    `mock-openai`, API-key auth `authorization: Bearer cred://openai-key`, `storage_kind:
    openai`)
  * `orphan_watchdog {scan 1 s, timeout 90 s}`
  * `upload_reaper {scan 1 s, stale 60 s}`
  * `thread_summary_worker {enabled, summary_model_id: std-tiny}`
  * `streaming.sse_ping_interval_seconds: 5`
  * `quota.web_search_daily_quota: 75`
  * `rag {max_documents_per_chat: 3, max_images_per_message: 2, uploaded_file_max_size_kb:
    2048, uploaded_image_max_size_kb: 256, max_total_upload_mb_per_chat: 5}`
* `static-mini-chat-model-policy-plugin`: default limits are 100/1000 credits (standard,
  daily/monthly) and 50/500 credits (premium). The catalog, all with `provider_id: openai`:

  | id | tier | notes |
  |---|---|---|
  | `prem` | Premium | `is_default`; vision; web/file search and code interpreter; multipliers 3e6/15e6; `"3x"`; context 128000, max output 4096, max input 100000 |
  | `std` | Standard | vision; all tools; multipliers 1e6/3e6; first Standard entry, so it is the downgrade target |
  | `std-novision` | Standard | no vision, no tools |
  | `std-tiny` | Standard | context 4096, max output 1024, max input 3072; summary model; context-budget tests |
  | `std-budget` | Standard | context 4096, max output 3000, max input 3000: input budget is about 1000 while `INPUT_TOO_LONG` applies only above 3000 (`CONTEXT_BUDGET_EXCEEDED` test) |
  | `off-model` | Standard | `enabled: false` |
  | `prem-novision` | Premium | no vision |

  The provider model id is `mock-<id>`, so the mock can tell which effective model was used.
  The system prompt contains `E2E-SYSPROMPT model=<id>`.

## Spec ambiguities and how the suite resolves them

* **Recent-messages limit vs. current message**: the suite accepts 10 history items (current
  message counted separately, as in DESIGN §4) or 8 (current message counted inside K, then
  whole-turn trimming). Either way the history must be the most recent whole turns.
* **`INPUT_TOO_LONG` on edit**: it is not clear whether this check runs before or after the
  mutation commit. The test requires the 400 and that no turn is left `running`. It checks
  that the old turn is still `done` only when that turn is still visible.
* **`CONTEXT_BUDGET_EXCEEDED` on edit** happens after the commit (DESIGN §3.9 rule 8). The
  suite expects a JSON 400 and, if a new turn row exists, `failed` / `context_length_exceeded`.
* **Outbox assertions** are tolerant: delivered messages may be vacuumed. The suite searches
  `toolkit_outbox_*` tables that have a `payload` column for the request id (hex) together
  with `billing_outcome`, and asserts "at most one" usage message per turn. It also checks that
  replays add none.
* **Orphan watchdog**: the test keeps a turn running with `[[hang]]`. It moves only
  `chat_turns.last_progress_at` one year back, keeping the stored text format; `started_at`
  is not touched so settlement still targets the current period. It then expects `error` /
  `orphan_timeout` and the released reserve.
* **Upload reaper**: a dropped upload is simulated by setting a ready row back to `uploaded`
  with an old `updated_at`.
* **Quota seeding**: the suite runs a cheap turn first so the user's `quota_usage` rows exist,
  then UPDATEs them. If a row is missing, it inserts one modelled on an existing row.
* **Instructions**: the system prompt and the tool guards are looked up in `instructions` or
  in `system`/`developer` input items.
* **Thumbnails**: a valid PNG must get a WebP thumbnail that fits 128×128 and keeps the aspect
  ratio.
* **403 for "another requester"** on turn mutations cannot be reached through the API, because
  chats are owner-only and foreign chats return 404. The suite asserts 404.
