//! Gear configuration contract (DESIGN Appendix B.1-B.9): every documented key
//! is accepted with its documented default, an absent section uses the
//! defaults, unknown keys are rejected exactly where B.1 says so, and the
//! shipped `config/*.yaml` `mini-chat.config` blocks load.
//!
//! The test-facing API is `mini_chat::config::MiniChatConfig`
//! (`serde::Deserialize` + `validate()`); see `S1-test-interface.md`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use mini_chat::config::{MiniChatConfig, ProviderKind, StorageKind};
use serde::Deserialize;
use serde_json::Value;
use toolkit::var_expand::ExpandVars;

/// The only mandatory section: S2S credentials.
const CREDENTIALS: &str =
    "client_credentials:\n  client_id: mini-chat\n  client_secret: s3cr3t-value\n";

/// Parse + validate, flattening both failure points into one `Err(String)`.
fn load(yaml: &str) -> Result<MiniChatConfig, String> {
    let cfg: MiniChatConfig = serde_saphyr::from_str(yaml).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

fn load_with(extra: &str) -> Result<MiniChatConfig, String> {
    load(&format!("{CREDENTIALS}{extra}"))
}

fn defaults() -> MiniChatConfig {
    load(CREDENTIALS).expect("credentials alone are a valid configuration")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ── defaults ─────────────────────────────────────────────────────────────────

#[test]
fn top_level_defaults() {
    let cfg = defaults();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.metrics.prefix, "", "empty prefix means `mini_chat`");
    assert_eq!(cfg.client_credentials.client_id, "mini-chat");
}

#[test]
fn absent_provider_section_yields_the_single_openai_entry() {
    let cfg = defaults();
    assert_eq!(cfg.providers.len(), 1);
    let openai = cfg.providers.get("openai").expect("default `openai` entry");
    assert!(matches!(openai.kind, ProviderKind::OpenAiResponses));
    assert_eq!(openai.host, "api.openai.com");
    assert!(matches!(openai.storage_kind, StorageKind::OpenAi));
    assert_eq!(openai.effective_port(), 443);
    assert!(!openai.use_http);
    assert_eq!(openai.api_path, "/v1/responses");
    assert!(
        format!("{:?}", openai.auth_config).contains("openai-key"),
        "API-key auth reads the credstore secret `cred://openai-key`: {:?}",
        openai.auth_config
    );
}

#[test]
fn streaming_defaults() {
    let s = defaults().streaming;
    assert_eq!(s.sse_ping_interval_seconds, 15);
    assert_eq!(s.sse_channel_capacity, 32);
    assert_eq!(s.max_output_tokens, 32768);
}

#[test]
fn estimation_budget_defaults() {
    let b = defaults().estimation_budgets;
    assert_eq!(b.minimal_generation_floor, 50);
    assert_eq!(b.bytes_per_token_conservative, 4);
    assert_eq!(b.fixed_overhead_tokens, 100);
    assert_eq!(b.safety_margin_pct, 10);
    assert_eq!(b.image_token_budget, 1000);
    assert_eq!(b.tool_surcharge_tokens, 500);
    assert_eq!(b.web_search_surcharge_tokens, 500);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1000);
}

#[test]
fn quota_defaults() {
    let q = defaults().quota;
    assert!(close(q.overshoot_tolerance_factor, 1.10));
    assert_eq!(q.warning_threshold_pct, 80);
    assert_eq!(q.web_search_max_calls_per_message, 2);
    assert_eq!(q.web_search_daily_quota, 75);
    assert_eq!(q.code_interpreter_max_calls_per_message, 10);
    assert_eq!(q.code_interpreter_daily_quota, 50);
}

#[test]
fn outbox_defaults() {
    let o = defaults().outbox;
    assert_eq!(o.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(o.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(o.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(o.thread_summary_queue_name, "mini-chat.thread_summary");
    assert_eq!(o.audit_queue_name, "mini-chat.audit");
    assert_eq!(o.num_partitions, 4);
}

#[test]
fn context_defaults() {
    let c = defaults().context;
    assert_eq!(c.recent_messages_limit, 10);
    assert!(
        !c.web_search_guard.trim().is_empty(),
        "built-in web-search guard text"
    );
    assert!(
        !c.file_search_guard.trim().is_empty(),
        "built-in file-search guard text"
    );
}

#[test]
fn rag_defaults() {
    let r = defaults().rag;
    assert_eq!(r.max_documents_per_chat, 50);
    assert_eq!(r.max_total_upload_mb_per_chat, 100);
    assert!(r.allow_csv_upload);
    assert_eq!(r.max_concurrent_uploads, 10);
    assert_eq!(r.uploaded_file_max_size_kb, 25600);
    assert_eq!(r.uploaded_image_max_size_kb, 5120);
    assert_eq!(r.max_images_per_message, 4);
}

#[test]
fn thumbnail_defaults() {
    let t = defaults().thumbnail;
    assert_eq!(t.width, 128);
    assert_eq!(t.height, 128);
    assert_eq!(t.max_bytes, 131_072);
    assert_eq!(t.max_pixels, 100_000_000);
    assert_eq!(t.max_decode_bytes, 33_554_432);
}

#[test]
fn orphan_watchdog_defaults() {
    let w = defaults().orphan_watchdog;
    assert!(w.enabled);
    assert_eq!(w.timeout_secs, 300);
    assert_eq!(w.scan_interval_secs, 60);
}

#[test]
fn upload_reaper_defaults() {
    let r = defaults().upload_reaper;
    assert!(r.enabled);
    assert_eq!(r.scan_interval_secs, 60);
    assert_eq!(r.stale_after_secs, 300);
}

#[test]
fn thread_summary_worker_defaults() {
    let t = defaults().thread_summary_worker;
    assert!(t.enabled);
    assert_eq!(t.claim_timeout_secs, 300);
    assert_eq!(t.max_attempts, 3);
    assert_eq!(t.compression_threshold_pct, 80);
    assert_eq!(
        t.summary_model_id, "",
        "empty means the built-in gpt-4.1-mini"
    );
    assert!(
        t.summary_system_prompt
            .starts_with("You are a conversation summarizer."),
        "built-in summary prompt (DESIGN B.5.5): {}",
        t.summary_system_prompt
    );
    assert_eq!(t.message_content_limit, 4000);
    assert_eq!(t.reconcile_interval_secs, 60);
}

#[test]
fn cleanup_worker_defaults() {
    let c = defaults().cleanup_worker;
    assert_eq!(c.max_attempts, 5);
    assert!(c.enabled);
    assert_eq!(c.poll_interval_secs, 60);
    assert_eq!(c.reconcile_interval_secs, 300);
    assert_eq!(c.stale_in_progress_timeout_secs, 900);
    assert_eq!(c.batch_size, 32);
}

#[test]
fn knowledge_search_defaults() {
    let k = defaults().knowledge_search;
    assert!(!k.enabled);
    assert!(k.vector_store_id.is_none());
    assert!(k.provider_id.is_none());
    assert_eq!(k.max_calls_per_message, 3);
    assert_eq!(k.top_k, 5);
    assert_eq!(k.max_chunk_chars, 2000);
    assert!(!k.guard.trim().is_empty(), "built-in guard text");
}

#[test]
fn default_trait_matches_an_empty_section_set() {
    // The `Default` impl is what the toolkit uses when the whole `config:`
    // block is absent; it must agree with parsing an empty document for every
    // documented default (credentials are the only thing that differs).
    let d = MiniChatConfig::default();
    let p = defaults();
    assert_eq!(d.url_prefix, p.url_prefix);
    assert_eq!(d.vendor, p.vendor);
    assert_eq!(
        d.streaming.sse_channel_capacity,
        p.streaming.sse_channel_capacity
    );
    assert_eq!(d.outbox.num_partitions, p.outbox.num_partitions);
    assert_eq!(
        d.orphan_watchdog.timeout_secs,
        p.orphan_watchdog.timeout_secs
    );
    assert!(d.providers.contains_key("openai"));
}

#[test]
fn client_credentials_are_required() {
    assert!(
        load("vendor: constructorfabric\n").is_err(),
        "client_credentials has no default (B.1)"
    );
    assert!(MiniChatConfig::default().validate().is_err());
}

// ── every documented key is accepted ─────────────────────────────────────────

/// Every key of Appendix B.1-B.9, set to a non-default value.
fn documented_yaml() -> String {
    format!(
        r#"{CREDENTIALS}
url_prefix: /chat
vendor: acme
metrics:
  prefix: my_chat
providers:
  openai:
    kind: openai_responses
    host: api.openai.com
    port: 443
    use_http: false
    upstream_alias: openai-alias
    api_path: /v1/responses
    auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    auth_config:
      header: Authorization
      prefix: "Bearer "
      secret_ref: openai-key
    storage_kind: openai
    storage_backend: openai-files
  local_vllm:
    kind: vllm_responses
    host: vllm.internal
    port: 8000
    use_http: true
    storage_kind: openai
  claude:
    kind: anthropic_messages
    host: api.anthropic.com
    storage_kind: openai
    rag_provider: openai
  azure:
    kind: openai_chat_completions
    host: acme.openai.azure.com
    storage_kind: azure
    api_version: 2025-03-01-preview
    tenant_overrides:
      "00000000-0000-0000-0000-0000000000aa":
        host: tenant-a.openai.azure.com
        upstream_alias: tenant-a
        auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        auth_config:
          header: api-key
          secret_ref: azure-tenant-a
streaming:
  sse_ping_interval_seconds: 20
  sse_channel_capacity: 48
  max_output_tokens: 16384
estimation_budgets:
  minimal_generation_floor: 64
  bytes_per_token_conservative: 3
  fixed_overhead_tokens: 90
  safety_margin_pct: 15
  image_token_budget: 800
  tool_surcharge_tokens: 400
  web_search_surcharge_tokens: 450
  code_interpreter_surcharge_tokens: 900
quota:
  overshoot_tolerance_factor: 1.25
  warning_threshold_pct: 70
  web_search_max_calls_per_message: 3
  web_search_daily_quota: 100
  code_interpreter_max_calls_per_message: 8
  code_interpreter_daily_quota: 40
outbox:
  queue_name: q.usage
  cleanup_queue_name: q.attachment
  chat_cleanup_queue_name: q.chat
  thread_summary_queue_name: q.summary
  audit_queue_name: q.audit
  num_partitions: 8
context:
  recent_messages_limit: 20
  web_search_guard: "Use web search sparingly."
  file_search_guard: "Cite files."
rag:
  max_documents_per_chat: 20
  max_total_upload_mb_per_chat: 50
  allow_csv_upload: false
  max_concurrent_uploads: 5
  uploaded_file_max_size_kb: 10240
  uploaded_image_max_size_kb: 2048
  max_images_per_message: 2
thumbnail:
  width: 64
  height: 96
  max_bytes: 65536
  max_pixels: 50000000
  max_decode_bytes: 16777216
orphan_watchdog:
  enabled: false
  timeout_secs: 600
  scan_interval_secs: 30
upload_reaper:
  enabled: false
  scan_interval_secs: 120
  stale_after_secs: 600
thread_summary_worker:
  enabled: false
  claim_timeout_secs: 120
  max_attempts: 5
  compression_threshold_pct: 70
  summary_model_id: gpt-4.1-mini
  summary_system_prompt: "Summarize."
  message_content_limit: 0
  reconcile_interval_secs: 30
cleanup_worker:
  max_attempts: 7
  enabled: false
  poll_interval_secs: 30
  reconcile_interval_secs: 100
  stale_in_progress_timeout_secs: 600
  batch_size: 16
knowledge_search:
  enabled: true
  vector_store_id: vs_123
  provider_id: openai
  max_calls_per_message: 4
  top_k: 8
  max_chunk_chars: 1500
  guard: "Search knowledge only when asked."
"#
    )
}

fn documented() -> MiniChatConfig {
    load(&documented_yaml()).unwrap_or_else(|e| panic!("documented keys must load: {e}"))
}

#[test]
fn every_documented_key_is_accepted() {
    documented();
}

#[test]
fn documented_top_level_and_provider_keys_are_applied() {
    let cfg = documented();
    assert_eq!(cfg.url_prefix, "/chat");
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.metrics.prefix, "my_chat");
    assert_eq!(cfg.providers.len(), 4);
    assert_eq!(cfg.providers["local_vllm"].effective_port(), 8000);
    assert!(cfg.providers["local_vllm"].use_http);
    assert!(matches!(
        cfg.providers["claude"].kind,
        ProviderKind::AnthropicMessages
    ));
    assert!(matches!(
        cfg.providers["azure"].storage_kind,
        StorageKind::Azure
    ));
}

#[test]
fn documented_streaming_quota_and_outbox_keys_are_applied() {
    let cfg = documented();
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 20);
    assert_eq!(cfg.streaming.sse_channel_capacity, 48);
    assert_eq!(cfg.streaming.max_output_tokens, 16384);
    assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 64);
    assert!(close(cfg.quota.overshoot_tolerance_factor, 1.25));
    assert_eq!(cfg.quota.warning_threshold_pct, 70);
    assert_eq!(cfg.outbox.queue_name, "q.usage");
    assert_eq!(cfg.outbox.num_partitions, 8);
}

#[test]
fn documented_context_rag_and_thumbnail_keys_are_applied() {
    let cfg = documented();
    assert_eq!(cfg.context.recent_messages_limit, 20);
    assert_eq!(cfg.context.web_search_guard, "Use web search sparingly.");
    assert!(!cfg.rag.allow_csv_upload);
    assert_eq!(cfg.thumbnail.height, 96);
}

#[test]
fn documented_worker_and_knowledge_search_keys_are_applied() {
    let cfg = documented();
    assert!(!cfg.orphan_watchdog.enabled);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 600);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 600);
    assert_eq!(cfg.thread_summary_worker.message_content_limit, 0);
    assert_eq!(cfg.cleanup_worker.batch_size, 16);
    assert!(cfg.knowledge_search.enabled);
    assert_eq!(
        cfg.knowledge_search.vector_store_id.as_deref(),
        Some("vs_123")
    );
}

#[test]
fn provider_port_defaults_depend_on_use_http() {
    let cfg = load_with(
        "providers:\n  a:\n    kind: openai_responses\n    host: a.example\n    storage_kind: openai\n  b:\n    kind: openai_responses\n    host: b.example\n    use_http: true\n    storage_kind: openai\n  c:\n    kind: openai_responses\n    host: c.example\n    port: 9443\n    storage_kind: openai\n",
    )
    .unwrap();
    assert_eq!(cfg.providers["a"].effective_port(), 443);
    assert_eq!(cfg.providers["b"].effective_port(), 80);
    assert_eq!(cfg.providers["c"].effective_port(), 9443);
    assert_eq!(cfg.providers["a"].api_path, "/v1/responses");
}

#[test]
fn explicit_providers_replace_the_default_openai_entry() {
    let cfg = load_with(
        "providers:\n  mock:\n    kind: openai_responses\n    host: 127.0.0.1\n    port: 18080\n    use_http: true\n    storage_kind: openai\n",
    )
    .unwrap();
    assert_eq!(cfg.providers.keys().collect::<Vec<_>>(), vec!["mock"]);
}

// ── unknown keys ─────────────────────────────────────────────────────────────

#[test]
fn unknown_top_level_key_is_rejected() {
    assert!(load_with("bogus_key: 1\n").is_err());
}

#[test]
fn mcp_is_not_a_config_section() {
    // B.7.1: "because the gear configuration rejects unknown keys, a config
    // containing `mcp` fails startup".
    assert!(load_with("mcp:\n  enabled: true\n").is_err());
}

#[test]
fn unknown_key_is_rejected_in_every_strict_section() {
    for section in [
        "streaming",
        "estimation_budgets",
        "quota",
        "outbox",
        "context",
        "rag",
        "metrics",
        "thumbnail",
        "knowledge_search",
    ] {
        let yaml = format!("{section}:\n  bogus_key: 1\n");
        assert!(
            load_with(&yaml).is_err(),
            "unknown key in `{section}` must fail startup"
        );
    }
}

#[test]
fn unknown_key_in_client_credentials_is_rejected() {
    let yaml =
        "client_credentials:\n  client_id: a\n  client_secret: b\n  bogus_key: c\n".to_owned();
    assert!(load(&yaml).is_err());
}

#[test]
fn unknown_key_in_a_provider_entry_or_tenant_override_is_rejected() {
    let provider = "providers:\n  p:\n    kind: openai_responses\n    host: h.example\n    storage_kind: openai\n    bogus_key: 1\n";
    assert!(load_with(provider).is_err());

    let override_ = "providers:\n  p:\n    kind: openai_responses\n    host: h.example\n    storage_kind: openai\n    tenant_overrides:\n      \"00000000-0000-0000-0000-0000000000aa\":\n        host: t.example\n        bogus_key: 1\n";
    assert!(load_with(override_).is_err());
}

#[test]
fn removed_keys_fail_startup() {
    // B.1: `providers.<id>.supports_file_search_filters` and
    // `streaming.web_search_context_size` were removed.
    let provider = "providers:\n  p:\n    kind: openai_responses\n    host: h.example\n    storage_kind: openai\n    supports_file_search_filters: true\n";
    assert!(load_with(provider).is_err());
    assert!(load_with("streaming:\n  web_search_context_size: low\n").is_err());
}

#[test]
fn worker_sections_accept_and_ignore_unknown_keys() {
    for section in [
        "orphan_watchdog",
        "upload_reaper",
        "thread_summary_worker",
        "cleanup_worker",
    ] {
        let yaml = format!("{section}:\n  some_future_key: 1\n");
        let cfg = load_with(&yaml)
            .unwrap_or_else(|e| panic!("`{section}` must ignore unknown keys: {e}"));
        // The ignored key does not disturb the defaults.
        assert_eq!(cfg.vendor, "constructorfabric");
    }
}

#[test]
fn deprecated_estimation_budget_fields_are_parsed_but_not_validated() {
    // B.5.2: only `minimal_generation_floor` is validated; the rest are
    // "parsed, not validated" (zero must not fail startup).
    let cfg = load_with(
        "estimation_budgets:\n  bytes_per_token_conservative: 0\n  fixed_overhead_tokens: 0\n  safety_margin_pct: 0\n  image_token_budget: 0\n  tool_surcharge_tokens: 0\n  web_search_surcharge_tokens: 0\n  code_interpreter_surcharge_tokens: 0\n",
    )
    .expect("deprecated fields are not validated");
    assert_eq!(cfg.estimation_budgets.bytes_per_token_conservative, 0);
}

#[test]
fn client_secret_is_redacted_in_debug_output() {
    let cfg = defaults();
    let rendered = format!("{cfg:?}");
    assert!(
        !rendered.contains("s3cr3t-value"),
        "client_secret must be redacted in logs (B.1): {rendered}"
    );
}

// ── ${VAR} expansion ─────────────────────────────────────────────────────────

const PLACEHOLDER_YAML: &str = r#"
client_credentials:
  client_id: "${MC_TEST_CLIENT_ID}"
  client_secret: "${MC_TEST_CLIENT_SECRET}"
providers:
  p:
    kind: openai_responses
    storage_kind: openai
    host: "${MC_TEST_HOST}"
    auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    auth_config:
      header: Authorization
      secret_ref: "${MC_TEST_SECRET_REF}"
    tenant_overrides:
      "00000000-0000-0000-0000-0000000000aa":
        host: "${MC_TEST_TENANT_HOST}"
        auth_config:
          secret_ref: "${MC_TEST_TENANT_SECRET_REF}"
"#;

fn placeholder_env() -> [(&'static str, Option<&'static str>); 6] {
    [
        ("MC_TEST_CLIENT_ID", Some("expanded-client")),
        ("MC_TEST_CLIENT_SECRET", Some("expanded-secret")),
        ("MC_TEST_HOST", Some("expanded.example")),
        ("MC_TEST_SECRET_REF", Some("expanded-key")),
        ("MC_TEST_TENANT_HOST", Some("tenant.expanded.example")),
        ("MC_TEST_TENANT_SECRET_REF", Some("tenant-key")),
    ]
}

#[test]
fn placeholders_are_expanded_in_provider_host_auth_config_overrides_and_credentials() {
    // B.1: `${VAR}` expansion is supported in provider `host` and `auth_config`,
    // the same two fields of `tenant_overrides.<tenant_id>`, and `client_credentials`.
    temp_env::with_vars(placeholder_env(), || {
        let mut cfg: MiniChatConfig = serde_saphyr::from_str(PLACEHOLDER_YAML).expect("parses");
        cfg.expand_vars().expect("every variable is set");
        cfg.validate().expect("valid after expansion");

        assert_eq!(cfg.client_credentials.client_id, "expanded-client");
        let p = &cfg.providers["p"];
        assert_eq!(p.host, "expanded.example");
        assert!(
            format!("{:?}", p.auth_config).contains("expanded-key"),
            "auth_config values are expanded: {:?}",
            p.auth_config
        );
        let o = &p.tenant_overrides["00000000-0000-0000-0000-0000000000aa"];
        assert_eq!(o.host.as_deref(), Some("tenant.expanded.example"));
        assert!(format!("{:?}", o.auth_config).contains("tenant-key"));
    });
}

#[test]
fn unexpanded_placeholders_do_not_pass_validation() {
    // A provider host that still contains `${...}` would change the OAGW alias path.
    temp_env::with_vars(placeholder_env(), || {
        let cfg: MiniChatConfig = serde_saphyr::from_str(PLACEHOLDER_YAML).expect("parses");
        assert!(cfg.validate().is_err());
    });
}

#[test]
fn a_missing_environment_variable_fails_expansion() {
    let mut env = placeholder_env().to_vec();
    env[2] = ("MC_TEST_HOST", None);
    temp_env::with_vars(env, || {
        let mut cfg: MiniChatConfig = serde_saphyr::from_str(PLACEHOLDER_YAML).expect("parses");
        assert!(cfg.expand_vars().is_err());
    });
}

// ── shipped configuration files ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ConfigFile {
    gears: BTreeMap<String, Value>,
}

fn repo_config(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../config")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `gears.mini-chat.config` block of a shipped config, as a YAML string
/// with `${VAR}` placeholders substituted the way the config expander would.
fn mini_chat_block(file: &str, substitutions: &[(&str, &str)]) -> String {
    let mut text = repo_config(file);
    for (var, value) in substitutions {
        text = text.replace(&format!("${{{var}}}"), value);
    }
    let parsed: ConfigFile = serde_saphyr::from_str(&text)
        .unwrap_or_else(|e| panic!("{file} is valid YAML with a `gears` map: {e}"));
    let block = parsed
        .gears
        .get("mini-chat")
        .unwrap_or_else(|| panic!("{file} has no gears.mini-chat"))
        .get("config")
        .unwrap_or_else(|| panic!("{file} has no gears.mini-chat.config"))
        .clone();
    serde_json::to_string(&block).expect("json")
}

fn load_json(json: &str) -> Result<MiniChatConfig, String> {
    let cfg: MiniChatConfig = serde_json::from_str(json).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

#[test]
fn config_mini_chat_yaml_block_parses() {
    let json = mini_chat_block(
        "mini-chat.yaml",
        &[
            ("AZURE_OPENAI_API_HOST", "example.openai.azure.com"),
            ("AZURE_OPENAI_API_KEY", "unused"),
        ],
    );
    let cfg = load_json(&json).unwrap_or_else(|e| panic!("config/mini-chat.yaml: {e}"));

    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.client_credentials.client_id, "mini-chat");
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 10);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
    assert!(cfg.thread_summary_worker.enabled);
    assert_eq!(cfg.thread_summary_worker.summary_model_id, "gpt-4.1-mini");
    assert_eq!(cfg.thread_summary_worker.compression_threshold_pct, 80);

    let azure = cfg
        .providers
        .get("azure_openai")
        .expect("azure_openai provider");
    assert!(matches!(azure.kind, ProviderKind::OpenAiResponses));
    assert!(matches!(azure.storage_kind, StorageKind::Azure));
    assert_eq!(azure.host, "example.openai.azure.com");
    assert_eq!(azure.api_path, "/openai/v1/responses");
    assert_eq!(azure.api_version.as_deref(), Some("2025-03-01-preview"));
    assert!(
        !cfg.providers.contains_key("openai"),
        "an explicit `providers` map replaces the default entry"
    );
}

#[test]
fn config_e2e_local_yaml_block_parses_and_uses_all_defaults() {
    let json = mini_chat_block("e2e-local.yaml", &[]);
    let cfg = load_json(&json).unwrap_or_else(|e| panic!("config/e2e-local.yaml: {e}"));
    assert_eq!(cfg.client_credentials.client_id, "mini-chat");
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert!(cfg.providers.contains_key("openai"));
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
}
