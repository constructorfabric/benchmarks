use mini_chat_sdk::UsageTokens;
use uuid::Uuid;

use super::{
    BillingOutcome, TerminalState, UsageEventInput, build_mutation_audit_event, build_usage_event,
    derive_billing,
};
use crate::domain::service::quota::SettlementMethod;

fn usage(i: i64, o: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: i,
        output_tokens: o,
        ..UsageTokens::default()
    }
}

#[test]
fn billing_table() {
    use BillingOutcome as B;
    use SettlementMethod as M;
    use TerminalState as S;
    let cases: Vec<(S, Option<&str>, Option<UsageTokens>, B, M)> = vec![
        (S::Completed, None, None, B::Completed, M::Actual),
        (S::Completed, None, Some(usage(0, 0)), B::Completed, M::Actual),
        (S::Cancelled, None, Some(usage(5, 5)), B::Aborted, M::Estimated),
        (S::Failed, Some("orphan_timeout"), None, B::Aborted, M::Estimated),
        (S::Failed, Some("provider_error"), Some(usage(10, 0)), B::Failed, M::Actual),
        (S::Failed, Some("provider_error"), Some(usage(0, 0)), B::Failed, M::Estimated),
        (S::Failed, Some("rate_limited"), None, B::Failed, M::Estimated),
        (S::Failed, Some("web_search_calls_exceeded"), None, B::Failed, M::Estimated),
        (S::Failed, Some("turn_setup_failed"), None, B::Failed, M::Released),
        (S::Failed, Some("context_length_exceeded"), None, B::Failed, M::Released),
        (S::Failed, Some("something_new"), None, B::Failed, M::Estimated),
    ];
    for (s, code, u, eo, em) in cases {
        assert_eq!(derive_billing(s, code, u.as_ref()), (eo, em), "{s:?} {code:?} {u:?}");
    }
}

#[test]
fn usage_event_shape() {
    let t = Uuid::from_u128(1);
    let turn = Uuid::from_u128(2);
    let req = Uuid::from_u128(3);
    let ev = build_usage_event(&UsageEventInput {
        tenant_id: t,
        user_id: Some(Uuid::from_u128(4)),
        chat_id: Uuid::from_u128(5),
        turn_id: turn,
        request_id: req,
        effective_model: "m".into(),
        selected_model: "m".into(),
        terminal_state: TerminalState::Cancelled,
        outcome: BillingOutcome::Aborted,
        method: SettlementMethod::Estimated,
        usage: Some(usage(1, 1)),
        actual_credits_micro: 42,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
    });
    assert_eq!(
        ev.dedupe_key,
        format!("{}/{}/{}", t.as_simple(), turn.as_simple(), req.as_simple())
    );
    assert!(ev.usage.is_none(), "estimated settlements carry no usage");
    assert_eq!(ev.billing_outcome, "aborted");
    assert_eq!(ev.terminal_state, "cancelled");
    let v = serde_json::to_value(&ev).unwrap_or_default();
    assert!(v.get("system_task_type").is_none());
}

#[test]
fn mutation_audit_fields() {
    let ev = build_mutation_audit_event("turn_delete", Uuid::nil(), Uuid::nil(), Uuid::nil(), Uuid::from_u128(9), None);
    let v = serde_json::to_value(&ev).unwrap_or_default();
    assert_eq!(v["event_type"], "turn_delete");
    assert_eq!(v["request_id"], Uuid::from_u128(9).to_string());
    let ev = build_mutation_audit_event("turn_retry", Uuid::nil(), Uuid::nil(), Uuid::nil(), Uuid::from_u128(9), Some(Uuid::from_u128(10)));
    let v = serde_json::to_value(&ev).unwrap_or_default();
    assert_eq!(v["original_request_id"], Uuid::from_u128(9).to_string());
    assert_eq!(v["new_request_id"], Uuid::from_u128(10).to_string());
}

#[tokio::test]
async fn test_env_boots_and_outbox_delivers() {
    use crate::domain::service::test_support::TestEnv;
    let env = TestEnv::default_env().await;
    let ev = build_mutation_audit_event("turn_delete", Uuid::nil(), Uuid::nil(), Uuid::nil(), Uuid::from_u128(1), None);
    let outbox = std::sync::Arc::clone(&env.deps.outbox);
    let wake = env
        .deps
        .db
        .transaction(move |tx| {
            let outbox = std::sync::Arc::clone(&outbox);
            let ev = ev.clone();
            Box::pin(async move { outbox.enqueue_audit(tx, &ev).await })
        })
        .await
        .unwrap();
    wake.fire();
    let got = env.delivered_to("mini-chat.audit", 1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["event_type"], "turn_delete");
    env.shutdown().await;
}

#[test]
fn actual_settlement_without_provider_usage_has_null_usage() {
    let input = |usage: Option<UsageTokens>| UsageEventInput {
        tenant_id: Uuid::from_u128(1),
        user_id: Some(Uuid::from_u128(4)),
        chat_id: Uuid::from_u128(5),
        turn_id: Uuid::from_u128(2),
        request_id: Uuid::from_u128(3),
        effective_model: "m".into(),
        selected_model: "m".into(),
        terminal_state: TerminalState::Completed,
        outcome: BillingOutcome::Completed,
        method: SettlementMethod::Actual,
        usage,
        actual_credits_micro: 0,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
    };
    let ev = build_usage_event(&input(None));
    assert!(ev.usage.is_none());
    let v = serde_json::to_value(&ev).unwrap_or_default();
    assert!(v["usage"].is_null(), "{v}");
    let ev = build_usage_event(&input(Some(usage(7, 3))));
    assert_eq!(ev.usage, Some(usage(7, 3)));
}
