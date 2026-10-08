"""Server configuration for the mini-chat E2E suite.

The configuration uses the documented mini-chat keys (DESIGN Appendix B) and
points both providers at the local mock (`mock_llm.py`).
"""

from __future__ import annotations

import uuid

TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"
TENANT_B = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
NUM_USERS = 250


def user_id(tenant_idx: int, n: int) -> str:
    return str(uuid.UUID(int=(0x5000 + tenant_idx) << 96 | n))


def token(tenant_idx: int, n: int) -> str:
    return f"tok-t{tenant_idx}-u{n}"


def _tokens_yaml() -> str:
    lines = []
    for t_idx, tenant in ((0, TENANT_A), (1, TENANT_B)):
        for n in range(NUM_USERS):
            lines.append(
                f"""        - token: "{token(t_idx, n)}"
          identity:
            subject_id: "{user_id(t_idx, n)}"
            subject_tenant_id: "{tenant}"
            subject_type: "gts.cf.core.security.subject_user.v1~"
            token_scopes: ["*"]"""
            )
    lines.append(
        """        - token: "tok-nil-tenant"
          identity:
            subject_id: "33333333-6a88-4768-9dfc-6bcd5187d9ed"
            subject_tenant_id: "00000000-0000-0000-0000-000000000000"
            token_scopes: ["*"]"""
    )
    return "\n".join(lines)


def _model(
    mid: str,
    tier: str,
    *,
    enabled: bool = True,
    vision: bool = True,
    web: bool = True,
    files: bool = True,
    code: bool = True,
    default: bool = False,
    context_window: int = 128000,
    max_output: int = 4096,
    max_input: int = 120000,
    in_mult: int = 1_000_000,
    out_mult: int = 2_000_000,
    sort: int = 0,
) -> str:
    caps = '["VISION_INPUT"]' if vision else "[]"
    return f"""        - id: "{mid}"
          provider_model_id: "{mid}-provider"
          display_name: "{mid.upper()}"
          description: "Test model {mid}"
          provider_id: "openai"
          provider_display_name: "OpenAI"
          icon: ""
          tier: {tier}
          enabled: {str(enabled).lower()}
          system_prompt: "You are {mid}."
          thread_summary_prompt: ""
          multimodal_capabilities: {caps}
          context_window: {context_window}
          max_output_tokens: {max_output}
          max_input_tokens: {max_input}
          input_tokens_credit_multiplier_micro: {in_mult}
          output_tokens_credit_multiplier_micro: {out_mult}
          multiplier_display: "1x"
          estimation_budgets:
            bytes_per_token_conservative: 4
            fixed_overhead_tokens: 100
            safety_margin_pct: 10
            image_token_budget: 1000
            tool_surcharge_tokens: 500
            web_search_surcharge_tokens: 500
            code_interpreter_surcharge_tokens: 1000
            minimal_generation_floor: 50
          max_num_results: 5
          web_search_context_size: low
          max_tool_calls: 2
          general_config:
            type: ""
            available_from: "1970-01-01T00:00:00Z"
            max_file_size_mb: 25
            api_params:
              temperature: 0.7
              stop: []
            features:
              streaming: true
              structured_output: true
            tool_support:
              web_search: {str(web).lower()}
              file_search: {str(files).lower()}
              image_generation: false
              code_interpreter: {str(code).lower()}
              mcp: false
            supported_endpoints:
              chat_completions: true
              responses: true
              embeddings: false
              image_generation: false
              audio_speech_generation: false
              audio_transcription: false
              audio_translation: false
          preference:
            is_default: {str(default).lower()}
            sort_order: {sort}"""


def catalog_yaml() -> str:
    return "\n".join(
        [
            _model("gpt-premium", "Premium", default=True, in_mult=3_000_000, out_mult=15_000_000, sort=0),
            _model("gpt-standard", "Standard", sort=1),
            _model("gpt-novision", "Standard", vision=False, web=False, files=False, code=False, sort=2),
            _model("gpt-disabled", "Premium", enabled=False, sort=3),
            _model(
                "gpt-tiny",
                "Standard",
                context_window=4096,
                max_output=1024,
                max_input=3072,
                web=False,
                files=False,
                code=False,
                sort=4,
            ),
            _model("gpt-4.1-mini", "Standard", web=False, files=False, code=False, sort=9),
        ]
    )


def render(
    *,
    port: int,
    mock_port: int,
    home_dir: str,
    kill_switches: dict[str, bool] | None = None,
    standard_daily: int = 100_000_000,
    premium_daily: int = 50_000_000,
    extra_mini_chat: str = "",
    storage_kind: str = "openai",
    api_version: str | None = None,
) -> str:
    ks = kill_switches or {}
    ks_yaml = "\n".join(f"        {k}: {str(v).lower()}" for k, v in ks.items()) or "        disable_images: false"
    return f"""
server:
  home_dir: "{home_dir}"

database:
  servers:
    sqlite_main:
      engine: "sqlite"
      params:
        WAL: "true"
        synchronous: "NORMAL"
        busy_timeout: "10000"
      pool:
        max_conns: 8
        acquire_timeout: "30s"

logging:
  default:
    console_level: info
    file: "logs/server.log"
    file_level: info
  mini_chat:
    console_level: debug
    file: "logs/mini-chat.log"
    file_level: debug

gears:
  api-gateway:
    config:
      bind_addr: "127.0.0.1:{port}"
      enable_docs: false
      cors_enabled: false
      defaults:
        body_limit_bytes: 64000000
      auth_disabled: false
      require_auth_by_default: true

  gear-orchestrator:
    config: {{}}

  grpc-hub:
    config:
      listen_addr: "uds://{home_dir}/grpc.sock"

  types-registry:
    database:
      server: "sqlite_main"
      file: "types_registry.db"
    config: {{}}

  authn-resolver:
    config:
      vendor: "constructorfabric"

  authz-resolver:
    config:
      vendor: "constructorfabric"

  static-authn-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100
      mode: static_tokens
      tokens:
{_tokens_yaml()}
      s2s_credentials:
        - client_id: "mini-chat"
          client_secret: "mini-chat-dev-secret"

  static-authz-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100

  tenant-resolver:
    config:
      vendor: "constructorfabric"

  single-tenant-tr-plugin:
    config:
      vendor: "constructorfabric"

  credstore:
    database:
      server: "sqlite_main"
      file: "credstore.db"
    config:
      vendor: "constructorfabric"

  static-credstore-plugin:
    config:
      secrets: []

  oagw:
    config:
      proxy_timeout_secs: 30
      allow_http_upstream: true
      ssrf_policy:
        enabled: false

  mini-chat:
    database:
      server: "sqlite_main"
      file: "mini_chat.db"
    config:
      vendor: "constructorfabric"
      client_credentials:
        client_id: "mini-chat"
        client_secret: "mini-chat-dev-secret"
      streaming:
        sse_ping_interval_seconds: 5
      orphan_watchdog:
        scan_interval_secs: 2
        timeout_secs: 90
      upload_reaper:
        scan_interval_secs: 2
        stale_after_secs: 60
      thread_summary_worker:
        enabled: true
        summary_model_id: "gpt-4.1-mini"
      providers:
        openai:
          kind: openai_responses
          storage_kind: {storage_kind}
{f'          api_version: "{api_version}"' if api_version else ""}
          host: "127.0.0.1"
          port: {mock_port}
          use_http: true
          upstream_alias: "mock-openai"
          api_path: "/v1/responses"
{extra_mini_chat}

  static-mini-chat-audit-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100
      enabled: true

  static-mini-chat-model-policy-plugin:
    config:
      vendor: "constructorfabric"
      priority: 100
      default_standard_limits:
        limit_daily_credits_micro: {standard_daily}
        limit_monthly_credits_micro: 1000000000000
      default_premium_limits:
        limit_daily_credits_micro: {premium_daily}
        limit_monthly_credits_micro: 500000000000
      kill_switches:
{ks_yaml}
      model_catalog:
{catalog_yaml()}

opentelemetry:
  tracing:
    enabled: false
  metrics:
    enabled: false
"""
