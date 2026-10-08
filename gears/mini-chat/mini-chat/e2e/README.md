# mini-chat end-to-end tests

Black-box tests of the `mini-chat` gear through its REST/SSE API, its SQLite
database and the requests it sends to an OpenAI-compatible mock provider.

- `mock_llm.py` — mock provider (Responses API, Chat Completions, Files, Vector
  Stores). The behaviour of a chat request is selected by directives in the
  user text (`MOCK_SLOW`, `MOCK_FAILED`, `MOCK_HTTP_429`, `MOCK_WEB=n`, ...; see
  the module docstring). `/_mock/requests`, `/_mock/config` and `/_mock/reset`
  are test controls.
- `base_config.yaml` — server configuration; `conftest.py` starts one server
  per fixture (own port, home directory and SQLite files) with per-test
  overrides, and stops each process by its own pid.

Run:

```bash
cargo build --bin cf-gears-example-server --no-default-features \
    --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
cd gears/mini-chat/mini-chat/e2e
python3 -m pytest -q
```

`MINI_CHAT_SERVER_BIN` overrides the server binary, `MINI_CHAT_MOCK_PORT` the
mock port (default 18090) and `MINI_CHAT_KEEP_HOME=1` keeps the server home
directories (logs, databases) after the run.
