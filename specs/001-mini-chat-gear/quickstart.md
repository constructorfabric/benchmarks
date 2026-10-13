# Quickstart / Validation Guide: mini-chat

## Prerequisites

- Toolchain from `rust-toolchain.toml`; offline cargo registry cache (lockfile crates only).
- Python 3.11 with `pytest` and `requests` (already installed) for the smoke run.

## 1. Build

```bash
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
```

Expected: build succeeds (Gate 1).

## 2. Unit and integration tests

```bash
cargo test -p cf-gears-mini-chat-sdk
cargo test -p cf-gears-mini-chat
```

Expected: all tests pass (Gate 2). The integration tests (`gears/mini-chat/mini-chat/tests/`) boot
the gear's services against a temporary file-backed SQLite database, the OAGW data plane from
`oagw::test_support::build_test_gateway` and an in-process mock OpenAI-compatible provider; the
acceptance item → test mapping (AC-01 … AC-48, see [spec.md](./spec.md)) is in
`gears/mini-chat/mini-chat/tests/README.md`.

## 3. End-to-end smoke run against the real server

```bash
cd gears/mini-chat/mini-chat/tests/e2e_smoke
./run_smoke.sh          # starts mock_provider.py + the server (pid-tracked), runs pytest, stops both
```

The script renders `smoke-config.yaml` (mini-chat providers `openai` and `azure` pointing at the
mock with `use_http: true`, OAGW `allow_http_upstream: true`, static authn tokens for two users
in two tenants), starts the server with
`./target/debug/cf-gears-example-server --config <file> run`, records the pid in
`/tmp/server.pid`, and stops it with `kill "$(cat /tmp/server.pid)"`.

Expected outcomes:

- `POST /mini-chat/v1/chats` → 201 with `Location`.
- `POST …/messages:stream` → `stream_started`, `delta`s, `done`; the mock recorded a
  `/v1/responses` request with `stream: true`, `max_output_tokens`, `user` (64 hex chars).
- Replay with the same `request_id` → `is_new_turn: false`, no new mock request.
- Upload a PDF → 201 `status: ready`; the mock recorded file upload, vector store create and add.
- `DELETE /mini-chat/v1/chats/{id}` → 204, then 404; the mock receives file and vector store
  deletes within a few seconds (outbox cleanup).

## 4. Contracts

- REST: [contracts/rest-api.md](./contracts/rest-api.md), schemas in
  [openapi-mini-chat.json](./openapi-mini-chat.json)
- SSE: [contracts/sse-events.md](./contracts/sse-events.md)
- Provider wire format: [contracts/provider-wire.md](./contracts/provider-wire.md)
- Configuration: [contracts/config.md](./contracts/config.md)
- SDK and plugins: [contracts/plugins-sdk.md](./contracts/plugins-sdk.md)
- Data model: [data-model.md](./data-model.md)
