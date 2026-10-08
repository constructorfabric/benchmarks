# mini-chat black-box E2E suite

Starts the debug example server against a scriptable OpenAI-compatible mock
provider (`mock_llm.py`) and checks the gear through HTTP/SSE, its SQLite
database and the requests the mock received.

```sh
# from the repository root
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
python3 -m pytest gears/mini-chat/mini-chat/tests/e2e -q
```

- `config_template.py` renders the server config (documented DESIGN Appendix B
  keys, static tokens `tok-t{0|1}-u{n}`, a test model catalog).
- `conftest.py` starts one server per preset (`default`, `limits`, `kill`,
  `tight_quota`, `azure`, `knowledge`) on free ports, each with its own home
  directory, and stops it by pid. `MINI_CHAT_E2E_KEEP=1` keeps the temp dirs.
- Every test uses a fresh user so quota and chat state never leak between tests.
- The mock resets before every test; `/__mock/responses` queues chat scripts,
  `/__mock/summary_responses` queues thread-summary answers (optionally per chat),
  `/__mock/config` sets file / vector-store behaviour.
