# Quickstart / Validation Guide: Mini Chat Gear

## Prerequisites

- Rust toolchain from `rust-toolchain.toml` (offline cargo; registry cache populated).
- Python 3.11 with `pytest`, `httpx`, `aiohttp` (see `testing/e2e/requirements.txt`).

## 1. Build gate

```
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
```

Expected: success; the binary is `target/debug/cf-gears-example-server`.

## 2. Unit and in-process integration tests

```
cargo test -p cf-gears-mini-chat-sdk
cargo test -p cf-gears-mini-chat
```

Expected: all pass. The gear tests cover pure logic (credits, cascade, estimation, truncation,
sanitization, SSE parsing, billing outcome) and router-level HTTP tests that run the real REST
handlers over an in-memory SQLite database with a fake provider gateway and fake policy plugin.

## 3. Black-box suite (real server + mock OpenAI-compatible provider)

```
python3 -m pytest gears/mini-chat/mini-chat/tests/blackbox -q
```

The suite starts a mock provider (aiohttp) on a random port, writes a server config based on
`config/mini-chat.yaml` pointing `providers.*` at the mock (`use_http: true`), launches the debug
binary (recording its pid, stopping it by pid), and drives the HTTP/SSE API. It inspects the SQLite
database file and the requests the mock received.

Expected: every test passes; `specs/001-mini-chat-gear/traceability.md` maps each
acceptance-criteria item to tests.

## 4. Manual smoke

```
./target/debug/cf-gears-example-server --config <cfg> run >/tmp/server.log 2>&1 & echo $! >/tmp/server.pid
curl -s -XPOST localhost:8087/cf/mini-chat/v1/chats -H 'Authorization: Bearer t' \
     -H 'content-type: application/json' -d '{"title":"hi"}'
kill "$(cat /tmp/server.pid)"
```
