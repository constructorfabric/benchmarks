use uuid::Uuid;

use super::*;

fn entry_json() -> serde_json::Value {
    serde_json::json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "description": "Most capable model",
        "provider_id": "azure_openai",
        "tier": "Premium",
        "enabled": true,
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 1_047_576,
        "max_output_tokens": 32768,
        "max_input_tokens": 1_047_576,
        "input_tokens_credit_multiplier_micro": 3_000_000,
        "output_tokens_credit_multiplier_micro": 15_000_000,
        "multiplier_display": "3x",
        "general_config": {"tool_support": {"web_search": true}, "unknown_key": 1},
        "preference": {"is_default": true, "sort_order": 0}
    })
}

#[test]
fn catalog_entry_roundtrip_and_defaults() {
    let entry: ModelCatalogEntry = serde_json::from_value(entry_json()).unwrap();
    assert_eq!(entry.tier, ModelTier::Premium);
    assert!(entry.supports_vision());
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.estimation_budgets, EstimationBudgets::default());
    assert!(entry.general_config.tool_support.web_search);
    let back: ModelCatalogEntry =
        serde_json::from_value(serde_json::to_value(&entry).unwrap()).unwrap();
    assert_eq!(back, entry);
}

#[test]
fn enabled_defaults_to_false() {
    let mut v = entry_json();
    v.as_object_mut().unwrap().remove("enabled");
    let entry: ModelCatalogEntry = serde_json::from_value(v).unwrap();
    assert!(!entry.enabled);
}

#[test]
fn kill_switches_require_every_field() {
    let missing = serde_json::json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false
    });
    assert!(serde_json::from_value::<KillSwitches>(missing).is_err());
    let full = serde_json::to_value(KillSwitches::default()).unwrap();
    assert!(serde_json::from_value::<KillSwitches>(full).is_ok());
}

#[test]
fn snapshot_default_model_prefers_is_default() {
    let mut a: ModelCatalogEntry = serde_json::from_value(entry_json()).unwrap();
    a.id = "a".into();
    a.preference.is_default = false;
    let mut b = a.clone();
    b.id = "b".into();
    b.preference.is_default = true;
    let mut c = a.clone();
    c.id = "c".into();
    c.enabled = false;
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![c, a, b],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snap.default_model().unwrap().id, "b");
    assert!(snap.enabled_model("c").is_none());
    assert!(snap.model("c").is_some());
}

#[test]
fn system_task_usage_event_omits_user_fields() {
    let tenant = Uuid::new_v4();
    let rid = Uuid::new_v4();
    let ev = UsageEvent {
        tenant_id: tenant,
        user_id: None,
        chat_id: Uuid::new_v4(),
        turn_id: None,
        request_id: rid,
        effective_model: "m".into(),
        selected_model: "m".into(),
        terminal_state: "completed".into(),
        billing_outcome: "system_task".into(),
        usage: Some(UsageTokens::default()),
        actual_credits_micro: 0,
        settlement_method: "none".into(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: time::OffsetDateTime::now_utc(),
        requester_type: "system".into(),
        dedupe_key: system_task_dedupe_key(tenant, "thread_summary_update", rid),
        system_task_type: Some("thread_summary_update".into()),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert!(v.get("user_id").is_none());
    assert!(v.get("turn_id").is_none());
    assert_eq!(v["requester_type"], "system");
    assert_eq!(
        v["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            tenant.as_simple(),
            rid.as_simple()
        )
    );
}

#[test]
fn turn_dedupe_key_uses_simple_uuids() {
    let t = Uuid::nil();
    let key = turn_dedupe_key(t, t, t);
    assert_eq!(key.len(), 32 * 3 + 2);
    assert!(!key.contains('-'));
}

#[test]
fn mutation_audit_event_fields() {
    let ev = TurnMutationAuditEvent {
        event_type: "turn_retry".into(),
        tenant_id: Uuid::new_v4(),
        actor_user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: Some(Uuid::new_v4()),
        new_request_id: Some(Uuid::new_v4()),
        request_id: None,
        timestamp: time::OffsetDateTime::now_utc(),
    };
    let v = serde_json::to_value(AuditEnvelope::Mutation(ev)).unwrap();
    for key in [
        "event_type",
        "actor_user_id",
        "chat_id",
        "original_request_id",
        "new_request_id",
        "timestamp",
    ] {
        assert!(v.get(key).is_some(), "missing {key}");
    }
}
