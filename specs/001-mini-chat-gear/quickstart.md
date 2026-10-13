# Quickstart / validation guide

## 1. Build

```
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
```

## 2. Automated tests (cover every acceptance-criteria item)

```
cargo test -p cf-gears-mini-chat-sdk
cargo test -p cf-gears-mini-chat
```

`gears/mini-chat/mini-chat/tests/` contains the in-process integration suites, one file per AC area
(`chats.rs`, `messages.rs`, `streaming.rs`, `idempotency.rs`, `mutations.rs`, `attachments.rs`,
`quota.rs`, `models_reactions.rs`, `cleanup.rs`, `authz.rs`), plus a shared harness in
`tests/common/` with an in-memory SQLite database, a mock PDP and a fake OpenAI-compatible provider
behind `ServiceGatewayClientV1`. `specs/001-mini-chat-gear/tasks.md` maps each AC item to the tests
that cover it.

## 3. Smoke run against a mock provider

1. Start a mock OpenAI-compatible server on `127.0.0.1:<port>`:
   `python3 gears/mini-chat/mini-chat/tests/smoke/mock_openai.py <port>`.
   It serves `/v1/responses` (SSE), `/v1/files`, and `/v1/vector_stores*`.
2. Write a config based on `config/mini-chat.yaml` with these changes:
   - set `providers.openai` to `{kind: openai_responses, storage_kind: openai, host: 127.0.0.1, port: <port>, use_http: true}`
   - set `oagw.config.allow_http_upstream: true`
   - set `home_dir` to a temporary directory
3. Run `./target/debug/cf-gears-example-server --config <file> run >/tmp/server.log 2>&1 & echo $! >/tmp/server.pid`.
4. Exercise the API:
   - `curl -XPOST localhost:8087/mini-chat/v1/chats -H 'content-type: application/json' -d '{}'` → 201
   - `curl -N -XPOST .../chats/<id>/messages:stream -H 'content-type: application/json' -d '{"content":"hi"}'`
     → `stream_started`, `delta`…, `done`
   - check `sqlite3 <home>/mini-chat/mini_chat.db 'select state from chat_turns'` → `completed`
5. Stop the server with `kill "$(cat /tmp/server.pid)"`.
