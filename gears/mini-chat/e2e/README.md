# mini-chat black-box tests

Runs the real `cf-gears-example-server` debug binary against an OpenAI-compatible mock
provider (`mock_provider.py`) and checks the gear through HTTP/SSE, its SQLite database and
the requests the mock received.

```
cargo build --bin cf-gears-example-server --no-default-features \
  --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
cd gears/mini-chat/e2e && python3 -m pytest -q
```

Layout:

* `harness.py` — server launcher (generated config), HTTP/SSE client (`stream`, `sse`, incremental
  `sse_iter`/`stream_events`), mock control, DB access (`query`/`execute`, `ub`), outbox capture
  (`outbox_events`: SQLite triggers copy every `toolkit_outbox_body` row and its queue into
  `e2e_outbox_capture`, because the outbox vacuums processed bodies), `server_log`.
* `helpers.py` — Problem/SSE assertions, mock script builders (`stream_script`, `http_error`,
  tool events), file builders (PNG/PDF/XLSX), DB/quota helpers, background streams.
* `conftest.py` — `mc`/`fresh` (shared server), `mc_factory`, and dedicated servers:
  `qs` (quota accounting), `ks` (kill switches), `fs_srv` (force_standard_tier),
  `lim` (small RAG limits, 5 s ping interval, budget-test models).
* `test_*.py` — one module per acceptance-criteria area (`docs/acceptance-criteria.md`); each
  module docstring lists the criteria it covers.

Tests use a new chat per test, reset the mock per test, seed/restore quota rows on the dedicated
quota server, and keep provider pauses below the OAGW read timeout (10 s).
