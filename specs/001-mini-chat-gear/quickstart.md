# Quickstart / validation guide: mini-chat

## Build
```bash
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
```

## Automated tests (acceptance coverage)
```bash
cargo test -p cf-gears-mini-chat-sdk
cargo test -p cf-gears-mini-chat
```
Expected: all tests pass; tasks.md maps each acceptance-criteria item to the tests covering it.

## Manual smoke run against a mock provider
1. Start a mock OpenAI-compatible provider on 127.0.0.1:<port> serving `POST /v1/responses` (SSE),
   `POST /v1/files`, `POST /v1/vector_stores`, `POST/GET /v1/vector_stores/{id}/files[/{fid}]`, `DELETE ...`.
2. Copy `config/mini-chat.yaml`, point `providers.<id>` at the mock (`host: 127.0.0.1`, `port`,
   `use_http: true`, `storage_kind: openai`, `api_path: /v1/responses`) and set OAGW to allow HTTP upstreams.
3. Start: `./target/debug/cf-gears-example-server --config <file> run >/tmp/server.log 2>&1 & echo $! >/tmp/server.pid`
4. Exercise:
   - `POST /mini-chat/v1/chats` → 201 + Location
   - `POST /mini-chat/v1/chats/{id}/messages:stream` → `stream_started`, `delta`…, `done`
   - replay with the same `request_id` → `is_new_turn:false`, no new provider request
   - `GET /mini-chat/v1/chats/{id}/messages`, `/turns/{request_id}`, `/quota/status`, `/models`
5. Stop: `kill "$(cat /tmp/server.pid)"`.

References: [contracts/openapi-mini-chat.json](./contracts/openapi-mini-chat.json),
[contracts/sse.md](./contracts/sse.md), [contracts/errors.md](./contracts/errors.md),
[contracts/config.md](./contracts/config.md), [data-model.md](./data-model.md).
