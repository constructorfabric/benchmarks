# mini-chat E2E tests

Black-box tests of the `mini-chat` gear over HTTP/SSE, its SQLite database
and the requests it sends to an OpenAI-compatible mock provider
(`mock_llm.py`). `conftest.py` renders `config.template.yaml`, starts the mock
and `cf-gears-example-server` for the session (extra servers with other
configurations are started per module, see `test_variants.py`) and stops them
by PID.

```bash
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
python3 -m pytest gears/mini-chat/mini-chat/tests/e2e
```

`MINI_CHAT_SERVER_BIN` overrides the server binary path.
