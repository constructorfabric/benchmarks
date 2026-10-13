# Research & Decisions: mini-chat gear

Inputs: gear docs (PRD/DESIGN/ADRs, condensed in research-design-notes.md), generated OpenAPI
(contracts/openapi-mini-chat.json), platform research (research-platform-rest-db.md,
research-platform-integrations.md).

## D1. Wire contract source
- Decision: Treat `docs/api/api.json` mini-chat paths/schemas as the exact DTO shapes (field names, required
  vs optional, nullability), and DESIGN/PRD for semantics (omit-when-null rules, error codes).
- Rationale: the OpenAPI was generated from the reference implementation; it removes guesswork.

## D2. Throttling binding on create_chat
- Decision: bind `POST /v1/chats` to `ThrottlingSpec { rate_limit_zone: "rl_mini_chat_chat",
  in_flight_limit_zone: "ifl_mini_chat_chat", require_security_context: false, dry_run: true }`.
- Rationale: quickstart.yaml documents this binding as wired in the gear; config/mini-chat.yaml defines the
  zones ("must be defined or gateway startup fails validation"). Undefined zones fail gateway startup, so
  the hidden config (built for the reference implementation) must define them.
- Risk: a config without the zones fails startup; accepted to match reference behaviour.

## D3. Provider adapters
- Decision: full `openai_responses` adapter (streaming + non-streaming summary call, Files API, Vector
  Stores API, both `openai` and `azure` storage kinds). `openai_chat_completions`, `vllm_responses`,
  `anthropic_messages` accepted by config validation; implemented as thinner adapters (Chat Completions
  streaming parser; vLLM = Responses without tools/metadata; Anthropic Messages streaming basic).
- Rationale: hidden suite uses an OpenAI-compatible mock (ADR-0005: E2E uses `openai_responses` only).

## D4. Policy snapshot access
- Decision: no cache (ADR-0008); call the policy plugin per request via a gateway that resolves the plugin
  instance through types-registry by vendor (lowest priority wins) and ClientHub scoped client.

## D5. Outbox
- Decision: use toolkit-db shared outbox with 5 queues and leased handlers (see platform research for API).
  Partition keys: usage/audit by tenant_id; chat cleanup/thread summary by chat_id; attachment cleanup by tenant.

## D6. Testing approach
- Decision: in-crate unit tests for pure logic + integration tests (gear `tests/` dir) that build the domain
  services with in-memory SQLite, a fake provider transport (scripted Responses SSE), the static policy
  plugin, and fake authz. An acceptance-criteria → test traceability table lives in tasks.md.
- Alternatives: python e2e against the real server — rejected as primary (cannot live in
  testing/e2e/suites/mini_chat; slower), but a manual smoke run against the built server with a mock
  provider is part of quickstart.md.

## D7. License gate
- Decision: declare the platform base license feature requirement on every route via the operation
  builder (api-gateway license middleware enforces it), per ADR-0008 interim gate.

## D8. Leader election
- Decision: no-op elector (always leader) without `k8s` feature; with `k8s` feature compile a Lease-based
  elector only if kube APIs are straightforward, otherwise also no-op (CAS guards keep correctness).
