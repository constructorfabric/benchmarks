#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

use super::*;

fn entry(window: u32, max_output: u32, max_input: u32) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": "m", "provider_model_id": "pm", "display_name": "M", "provider_id": "p",
        "provider_display_name": "P", "tier": "premium", "enabled": true,
        "context_window": window, "max_output_tokens": max_output, "max_input_tokens": max_input,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": { "web_search": true, "file_search": true,
                "image_generation": false, "code_interpreter": true, "mcp": false },
            "supported_endpoints": {}
        }
    }))
    .unwrap()
}

fn plan(assembled: i64, truncated: bool) -> ContextPlan {
    ContextPlan {
        summary_message: None,
        summary_tokens: None,
        history: Vec::new(),
        messages_truncated: truncated,
        assembled_context_tokens: assembled,
    }
}

fn config(pct: u32, enabled: bool) -> ThreadSummaryWorkerConfig {
    ThreadSummaryWorkerConfig {
        enabled,
        compression_threshold_pct: pct,
        ..ThreadSummaryWorkerConfig::default()
    }
}

#[test]
fn the_threshold_is_a_percentage_of_the_effective_budget() {
    let entry = entry(4000, 1000, 0);
    assert_eq!(
        evaluate(&config(92, true), &entry, 1000, &plan(2770, false)),
        Some(SummaryTrigger {
            proactive: true,
            urgent: false
        })
    );
    assert_eq!(
        evaluate(&config(93, true), &entry, 1000, &plan(2770, false)),
        None
    );
}

#[test]
fn max_input_tokens_caps_the_budget() {
    let entry = entry(128_000, 1000, 1000);
    assert!(evaluate(&config(80, true), &entry, 1000, &plan(800, false)).is_some());
    assert!(evaluate(&config(80, true), &entry, 1000, &plan(799, false)).is_none());
}

#[test]
fn truncation_is_urgent_whatever_the_size() {
    let entry = entry(128_000, 4096, 0);
    assert_eq!(
        evaluate(&config(80, true), &entry, 4096, &plan(10, true)),
        Some(SummaryTrigger {
            proactive: false,
            urgent: true
        })
    );
}

#[test]
fn a_disabled_worker_never_triggers() {
    let entry = entry(4000, 1000, 0);
    assert_eq!(
        evaluate(&config(80, false), &entry, 1000, &plan(2999, true)),
        None
    );
}

fn frontier(second: u8, id: u128) -> Frontier {
    Frontier {
        created_at: datetime!(2026-01-01 00:00:00 UTC) + time::Duration::seconds(i64::from(second)),
        message_id: Uuid::from_u128(id),
    }
}

#[test]
fn proactive_needs_no_stored_summary_and_urgent_does_not() {
    let proactive = SummaryTrigger {
        proactive: true,
        urgent: false,
    };
    let urgent = SummaryTrigger {
        proactive: false,
        urgent: true,
    };
    let latest = Some(frontier(5, 9));
    assert_eq!(frozen_target(proactive, None, latest), latest);
    assert_eq!(frozen_target(proactive, Some(frontier(1, 1)), latest), None);
    assert_eq!(frozen_target(urgent, Some(frontier(1, 1)), latest), latest);
}

#[test]
fn nothing_is_scheduled_without_an_earlier_message_or_when_the_frontier_is_at_the_target() {
    let urgent = SummaryTrigger {
        proactive: false,
        urgent: true,
    };
    assert_eq!(frozen_target(urgent, None, None), None);
    assert_eq!(
        frozen_target(urgent, Some(frontier(5, 9)), Some(frontier(5, 9))),
        None
    );
    assert_eq!(
        frozen_target(urgent, Some(frontier(6, 1)), Some(frontier(5, 9))),
        None
    );
}

#[test]
fn the_frontier_order_breaks_timestamp_ties_by_id() {
    assert!(frontier(1, 2) > frontier(1, 1));
    assert!(frontier(2, 1) > frontier(1, 9));
}
