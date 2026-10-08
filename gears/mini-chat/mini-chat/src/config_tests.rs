#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cognitive_complexity)]

use super::*;

fn parse(yaml: &str) -> Result<MiniChatConfig, String> {
    serde_saphyr::from_str::<MiniChatConfig>(yaml).map_err(|e| e.to_string())
}

const MINIMAL: &str = r#"
client_credentials:
  client_id: "mini-chat"
  client_secret: "secret"
"#;

#[test]
fn defaults_follow_appendix_b() {
    let cfg = parse(MINIMAL).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32_768);
    assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 50);
    assert!((cfg.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(cfg.quota.warning_threshold_pct, 80);
    assert_eq!(cfg.quota.web_search_max_calls_per_message, 2);
    assert_eq!(cfg.quota.web_search_daily_quota, 75);
    assert_eq!(cfg.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(cfg.quota.code_interpreter_daily_quota, 50);
    assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(
        cfg.outbox.cleanup_queue_name,
        "mini-chat.attachment_cleanup"
    );
    assert_eq!(cfg.outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(
        cfg.outbox.thread_summary_queue_name,
        "mini-chat.thread_summary"
    );
    assert_eq!(cfg.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(cfg.outbox.num_partitions, 4);
    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert_eq!(cfg.rag.max_documents_per_chat, 50);
    assert_eq!(cfg.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(cfg.rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(cfg.rag.uploaded_image_max_size_kb, 5_120);
    assert_eq!(cfg.rag.max_images_per_message, 4);
    assert_eq!(cfg.rag.max_concurrent_uploads, 10);
    assert!(cfg.rag.allow_csv_upload);
    assert_eq!(cfg.thumbnail.width, 128);
    assert_eq!(cfg.thumbnail.max_bytes, 131_072);
    assert!(cfg.orphan_watchdog.enabled);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 60);
    assert!(cfg.upload_reaper.enabled);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);
    assert!(cfg.thread_summary_worker.enabled);
    assert_eq!(cfg.thread_summary_worker.compression_threshold_pct, 80);
    assert_eq!(cfg.thread_summary_worker.claim_timeout_secs, 300);
    assert_eq!(cfg.thread_summary_worker.max_attempts, 3);
    assert_eq!(cfg.thread_summary_worker.summary_model(), "gpt-4.1-mini");
    assert_eq!(cfg.cleanup_worker.max_attempts, 5);
    assert!(!cfg.knowledge_search.enabled);
    let openai = cfg.providers.get("openai").unwrap();
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(openai.storage_kind, Some(StorageKind::Openai));
    assert_eq!(cfg.metrics_prefix(), "mini_chat");
}

#[test]
fn unknown_top_level_keys_are_rejected() {
    let yaml = format!("{MINIMAL}\nmcp:\n  enabled: true\n");
    assert!(parse(&yaml).is_err());
}

#[test]
fn removed_fields_fail_startup() {
    let yaml = format!("{MINIMAL}\nstreaming:\n  web_search_context_size: low\n");
    assert!(parse(&yaml).is_err());
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let yaml = format!("{MINIMAL}\norphan_watchdog:\n  something_else: 1\n");
    assert!(parse(&yaml).is_ok());
}

#[test]
fn ranges_are_validated() {
    let cases = [
        "streaming:\n  sse_ping_interval_seconds: 4\n",
        "streaming:\n  sse_channel_capacity: 65\n",
        "quota:\n  overshoot_tolerance_factor: 1.6\n",
        "quota:\n  warning_threshold_pct: 100\n",
        "outbox:\n  num_partitions: 3\n",
        "orphan_watchdog:\n  timeout_secs: 89\n",
        "upload_reaper:\n  stale_after_secs: 59\n",
        "thread_summary_worker:\n  compression_threshold_pct: 0\n",
        "context:\n  recent_messages_limit: 101\n",
        "estimation_budgets:\n  minimal_generation_floor: 0\n",
    ];
    for case in cases {
        let cfg = parse(&format!("{MINIMAL}\n{case}")).unwrap();
        assert!(cfg.validate().is_err(), "expected invalid: {case}");
    }
}

#[test]
fn client_credentials_are_required() {
    let cfg = parse("vendor: x\n").unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn azure_storage_requires_api_version_and_rag_provider_must_exist() {
    let yaml = format!(
        "{MINIMAL}\nproviders:\n  az:\n    kind: openai_responses\n    host: az.example.com\n    storage_kind: azure\n"
    );
    assert!(parse(&yaml).unwrap().validate().is_err());

    let yaml = format!(
        "{MINIMAL}\nproviders:\n  claude:\n    kind: anthropic_messages\n    host: api.anthropic.com\n    rag_provider: missing\n"
    );
    assert!(parse(&yaml).unwrap().validate().is_err());

    let yaml = format!(
        "{MINIMAL}\nproviders:\n  oai:\n    kind: openai_responses\n    host: api.openai.com\n    storage_kind: openai\n  claude:\n    kind: anthropic_messages\n    host: api.anthropic.com\n    api_path: /v1/messages\n    rag_provider: oai\n"
    );
    parse(&yaml).unwrap().validate().unwrap();
}

#[test]
fn host_characters_are_restricted() {
    let yaml = format!(
        "{MINIMAL}\nproviders:\n  bad:\n    kind: openai_responses\n    host: \"evil.com/path\"\n    storage_kind: openai\n"
    );
    assert!(parse(&yaml).unwrap().validate().is_err());
}

#[test]
fn deployment_config_parses() {
    let yaml = r#"
vendor: "constructorfabric"
orphan_watchdog:
  scan_interval_secs: 10
  timeout_secs: 90
client_credentials:
  client_id: "mini-chat"
  client_secret: "mini-chat-dev-secret"
thread_summary_worker:
  enabled: true
  summary_model_id: "gpt-4.1-mini"
  compression_threshold_pct: 80
  summary_system_prompt: "summarize"
providers:
  azure_openai:
    kind: openai_responses
    storage_kind: azure
    host: "example.openai.azure.com"
    api_path: "/openai/v1/responses"
    api_version: "2025-03-01-preview"
    auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    auth_config:
      header: "api-key"
      prefix: ""
      secret_ref: "azure-openai-key"
"#;
    let cfg = parse(yaml).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.providers.len(), 1);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
}

#[test]
fn deprecated_fields_produce_warnings() {
    let cfg = parse(&format!(
        "{MINIMAL}\ncleanup_worker:\n  batch_size: 7\nestimation_budgets:\n  safety_margin_pct: 20\n"
    ))
    .unwrap();
    let warnings = cfg.deprecation_warnings();
    assert!(warnings.iter().any(|w| w.contains("batch_size")));
    assert!(warnings.iter().any(|w| w.contains("safety_margin_pct")));
}

#[test]
fn every_documented_key_is_accepted() {
    let yaml = r#"
url_prefix: "/mini-chat"
vendor: "constructorfabric"
client_credentials:
  client_id: "mini-chat"
  client_secret: "secret"
metrics:
  prefix: "mini_chat"
streaming:
  sse_ping_interval_seconds: 15
  sse_channel_capacity: 32
  max_output_tokens: 32768
estimation_budgets:
  minimal_generation_floor: 50
  bytes_per_token_conservative: 4
  fixed_overhead_tokens: 100
  safety_margin_pct: 10
  image_token_budget: 1000
  tool_surcharge_tokens: 500
  web_search_surcharge_tokens: 500
  code_interpreter_surcharge_tokens: 1000
quota:
  overshoot_tolerance_factor: 1.1
  warning_threshold_pct: 80
  web_search_max_calls_per_message: 2
  web_search_daily_quota: 75
  code_interpreter_max_calls_per_message: 10
  code_interpreter_daily_quota: 50
outbox:
  queue_name: "mini-chat.usage_snapshot"
  cleanup_queue_name: "mini-chat.attachment_cleanup"
  chat_cleanup_queue_name: "mini-chat.chat_cleanup"
  thread_summary_queue_name: "mini-chat.thread_summary"
  audit_queue_name: "mini-chat.audit"
  num_partitions: 4
context:
  recent_messages_limit: 10
  web_search_guard: "w"
  file_search_guard: "f"
rag:
  max_documents_per_chat: 50
  max_total_upload_mb_per_chat: 100
  allow_csv_upload: true
  max_concurrent_uploads: 10
  uploaded_file_max_size_kb: 25600
  uploaded_image_max_size_kb: 5120
  max_images_per_message: 4
thumbnail:
  width: 128
  height: 128
  max_bytes: 131072
  max_pixels: 100000000
  max_decode_bytes: 33554432
providers:
  openai:
    kind: openai_responses
    host: "api.openai.com"
    port: 443
    use_http: false
    upstream_alias: "api.openai.com"
    api_path: "/v1/responses"
    auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    auth_config:
      header: "Authorization"
      prefix: "Bearer "
      secret_ref: "openai-key"
    storage_kind: openai
    storage_backend: "openai"
    tenant_overrides:
      "00000000-0000-0000-0000-00000000000a":
        host: "tenant-a.example.com"
        upstream_alias: "tenant-a.example.com"
        auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        auth_config:
          secret_ref: "k"
  azure:
    kind: openai_responses
    host: "x.openai.azure.com"
    api_path: "/openai/v1/responses"
    storage_kind: azure
    api_version: "2025-03-01-preview"
  claude:
    kind: anthropic_messages
    host: "api.anthropic.com"
    api_path: "/v1/messages"
    rag_provider: openai
  chat:
    kind: openai_chat_completions
    host: "chat.example.com"
    api_path: "/v1/chat/completions"
    rag_provider: openai
  vllm:
    kind: vllm_responses
    host: "vllm.example.com"
    port: 8000
    use_http: true
    api_path: "/v1/responses"
    rag_provider: openai
orphan_watchdog:
  enabled: true
  timeout_secs: 300
  scan_interval_secs: 60
upload_reaper:
  enabled: true
  scan_interval_secs: 60
  stale_after_secs: 300
thread_summary_worker:
  enabled: true
  claim_timeout_secs: 300
  max_attempts: 3
  compression_threshold_pct: 80
  summary_model_id: "gpt-4.1-mini"
  summary_system_prompt: "p"
  message_content_limit: 4000
  reconcile_interval_secs: 60
cleanup_worker:
  max_attempts: 5
  enabled: true
  poll_interval_secs: 60
  reconcile_interval_secs: 300
  stale_in_progress_timeout_secs: 900
  batch_size: 32
knowledge_search:
  enabled: true
  vector_store_id: "vs_kb"
  provider_id: "azure"
  max_calls_per_message: 3
  top_k: 5
  max_chunk_chars: 2000
  guard: "g"
"#;
    let cfg = parse(yaml).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.providers.len(), 5);
}
