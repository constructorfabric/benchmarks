# Quickstart / Validation Guide: Mini Chat Gear

## Prerequisites
- Pinned Rust toolchain; offline cargo cache (no new crates).
- Python 3 for the optional manual smoke run with a mock provider.

## Build
```bash
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
```
Expected: builds without errors.

## Automated tests
```bash
cargo test -p cf-gears-mini-chat-sdk
cargo test -p cf-gears-mini-chat
```
Expected: all pass. Integration tests (`gears/mini-chat/mini-chat/tests/`) cover every item of `gears/mini-chat/docs/acceptance-criteria.md`; the mapping is in `tasks.md`.

## Manual smoke run (optional)
1. Start a mock OpenAI-compatible provider on `127.0.0.1:<port>` implementing `POST /v1/responses` (SSE), `POST /v1/files`, `/v1/vector_stores*`.
2. Write a config based on `config/mini-chat.yaml` with `providers.<id>: {kind: openai_responses, host: 127.0.0.1, port: <port>, use_http: true, storage_kind: openai}`, `oagw.config.allow_http_upstream: true`, `ssrf_policy.enabled: false`, and a model catalog whose `provider_id` matches.
3. `./target/debug/cf-gears-example-server --config <file> run >/tmp/server.log 2>&1 & echo $! >/tmp/server.pid`
4. Validate:
   - `POST /mini-chat/v1/chats` → 201 + `Location`
   - `POST /mini-chat/v1/chats/{id}/messages:stream` → `stream_started`, `delta`…, `done`
   - `GET /mini-chat/v1/chats/{id}/messages` → user + assistant messages
   - `GET /mini-chat/v1/quota/status` → tiers `premium` and `total`
   - `<home_dir>/mini-chat/mini_chat.db` contains `chats`, `chat_turns`, `messages`, `quota_usage` rows
5. `kill "$(cat /tmp/server.pid)"`

Contracts: [contracts/openapi.json](./contracts/openapi.json), [contracts/sse-and-outbox.md](./contracts/sse-and-outbox.md). Data model: [data-model.md](./data-model.md).
