# mini-chat black-box smoke test

`run_smoke.py` starts the real `cf-gears-example-server` binary and a mock
OpenAI-compatible provider. It then drives the mini-chat REST/SSE API through the
api-gateway, the same way an external client would. Every provider call goes through
OAGW to the mock. The test runs once per provider flavour:

| flavour | provider entry | wire format checked at the mock |
|---|---|---|
| `openai` | `storage_kind: openai`, `api_path: /v1/responses` | `/v1/...`, `authorization: Bearer <secret>` |
| `azure` | `storage_kind: azure`, `api_path: /openai/v1/responses?api-version=…`, `api_version` | `/openai/...?api-version=…`, `api-key: <secret>` |

It needs only the Python 3 stdlib (`http.server`, `urllib`, `sqlite3`).

## Run

```bash
# from the repository root; builds the server first when the binary is missing
python3 gears/mini-chat/e2e-smoke/run_smoke.py                    # both flavours
python3 gears/mini-chat/e2e-smoke/run_smoke.py --flavour azure    # one flavour
python3 gears/mini-chat/e2e-smoke/run_smoke.py --build --keep     # rebuild; keep run dirs
```

The script exits with code 0 when every check passes. Each flavour takes a few
seconds. The binary defaults to `target/debug/cf-gears-example-server`. `--build`
compiles it with:

```
cargo build --offline --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
```

Each flavour runs in a fresh temporary directory (`/tmp/mini-chat-smoke-<flavour>-*`)
that holds:
- the rendered `config.yaml`
- `server.log`
- `mock_requests.jsonl`, the requests the mock received
- the gear DB `mini-chat/mini_chat.db`

On success the directory is deleted, unless you pass `--keep`. On failure it is kept,
and the WARN/ERROR lines and the tail of the server log are printed.

The server is started with `subprocess.Popen` and stopped by its pid on every exit
path: SIGTERM first, then SIGKILL after 15 s. The script never matches processes by
command line.

## Files

- `mock_openai.py`: a threaded stdlib mock with these endpoints. It routes by path
  suffix, so the OpenAI and Azure URL shapes both work.
  - `POST …/responses`:
    - With `stream: true` it returns SSE text, "Hello" + " world", with usage 12/2.
    - Without `stream` it returns a JSON completion, used for the thread summary.
    - When the last user input contains `MOCK_PROVIDER_ERROR`, it answers
      `response.failed` with a message that contains a provider id, a URL and a key.
  - `POST …/files`, `POST …/vector_stores`, `POST …/vector_stores/{vs}/files` and
    `GET …/vector_stores/{vs}/files/{id}` all report `completed`.
  - Any `DELETE` returns 200.
  - Every request is recorded. The mock can also run standalone:
    `python3 mock_openai.py --port 18080 --record /tmp/mock.jsonl`.
- `config.template.yaml`: the server config. `@@HOME_DIR@@`, `@@API_PORT@@`,
  `@@PROVIDERS@@` and `@@PROVIDER_ID@@` are substituted per run. It sets up:
  - `static-authn-plugin` in `static_tokens` mode with tenant A user 1, tenant A user 2
    and tenant B, plus the `mini-chat` S2S credentials
  - OAGW with `allow_http_upstream: true` and `ssrf_policy.enabled: false`
  - the throttling zones `rl_mini_chat_chat` and `ifl_mini_chat_chat`
  - SQLite in the run directory
- `run_smoke.py`: the scenario.

## Operational notes

- **The provider secret is created over REST.** Secrets seeded in the
  `static-credstore-plugin` config are not readable through the credstore gateway.
  The gateway resolves secrets from its own DB metadata. So right after `/health`
  returns 200, the script sends `POST /credstore/v1/secrets`
  `{"reference": "<key>", "value": …, "sharing": "tenant"}`. It authenticates as
  tenant A user 1, which is also the identity that `client_credentials` maps to.
  OAGW provisioning is deferred at startup. It completes on demand with the first
  provider request.
- The mock listens on `127.0.0.1` on an ephemeral port. The provider entry uses
  `use_http: true` and `upstream_alias: "127.0.0.1"`.

## What is checked (per flavour)

1. Secret creation returns 201. `GET /models` lists the enabled catalog and no
   internal fields. `GET /quota/status` shows zero usage.
2. Creating a chat returns 201, with the default model and a `Location` header.
3. Streaming a message:
   - Events arrive in the order `stream_started` (`is_new_turn: true`, our
     `request_id`), `delta`*, `done`.
   - `done.usage` is `{12, 2}`, `effective_model`/`selected_model` is `gpt-4.1`, and
     `quota_decision` is `allow`.
   - The mock got exactly one request: the right path, query and auth header,
     `stream: true`, `store: false`, `user` = tenant hex + user hex,
     `metadata.chat_id`, and no tools.
4. Replaying the same `request_id` gives `stream_started` (`is_new_turn: false`, the
   persisted `message_id`), one `delta` and `done`. The provider gets no new request.
5. The messages list has 2 messages that share the `request_id`. The assistant
   message has `model`, `attachments: []` and `my_reaction: null`. The turn status
   is `done` with the `assistant_message_id`.
6. Tenant B and tenant A user 2 get 404 on get chat, messages, turn status, delete
   and stream. None of these requests reach the provider.
7. Uploading `note.txt` returns 201 with `status: ready`, `kind: document`. The file
   upload, the vector store creation and the vector-store file add all reach the
   mock with the flavour's path, query and auth.
8. Streaming with the attachment sends a `file_search` tool carrying the chat's
   vector store id and `max_num_results` 5, plus `max_tool_calls` 10 from the
   catalog. The user message lists the attachment.
9. Reacting `like` on the assistant message returns 200 and is idempotent. Reacting
   on a user message returns 400. The messages list shows `my_reaction: like`.
10. Web search with the kill switch off sends the `web_search` tool with
    `search_context_size: low`, next to `file_search`.
11. Retrying that turn streams with a new server `request_id` and makes one
    provider call. The call still carries both `web_search` and `file_search`,
    because retry reuses `web_search_enabled`. The old turn's status returns 404,
    and its messages are hidden.
12. A provider failure ends the stream with a terminal SSE
    `error {code: provider_error}`. The message keeps the provider text with the
    id, URL and key redacted. The turn status becomes `error` with
    `error_code: provider_error`.
13. Quota status shows the `premium` and `total` tiers. Each period's
    `used_credits_micro` equals `spent + reserved` of the matching `quota_usage`
    bucket, and `remaining = limit - used`.
14. DB checks (`sqlite3`, read-only):
    - `chat_turns`: three turns are `completed` with `effective_model`; the retried
      turn is soft-deleted with `replaced_by_request_id`; the failed turn is
      `failed` with `provider_error`. The retry keeps `web_search_enabled`.
    - `messages`: `model`, the token columns and `provider_response_id` are set.
    - `quota_usage`: the bucket `total` has a daily and a monthly row with 5 calls,
      0 reserved and the token sums.
    - outbox: exactly 5 `usage_snapshot` events, all processed, none dead-lettered.
15. Deleting the chat returns 204, and a later GET returns 404. The outbox chat
    cleanup sends `DELETE` for the provider file and the vector store to the mock,
    with the flavour's auth. The `chat_vector_stores` row is removed.
16. A second chat checks that:
    - `GET /models/{id}` returns 200, and an unknown id returns 404
    - `PATCH` trims the title
    - the most recently active chat is listed first
    - tenant B's list is empty
    - edit streams a new `request_id` and sends only the new content
    - deleting the last turn returns 204 and hides its messages
    - deleting it again returns 409 `NOT_LATEST_TURN`
17. No API response or SSE body contains a mock provider identifier (`resp_mock`,
    `msg_mock`, `file-mock`, `vs_mock`, `sk-mock`).
