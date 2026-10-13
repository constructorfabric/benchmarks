use mini_chat_sdk::EstimationBudgets;
use time::Duration;
use time::macros::datetime;
use uuid::Uuid;

use super::{
    ContextInput, ContextPlan, ContextRole, HistoryMessage, ModelLimits, PlannedMessage,
    SECTION_SEPARATOR, SummaryForContext, SummaryTrigger, THREAD_SUMMARY_PREAMBLE, ToolSurcharges,
    assemble, check_input_length, effective_budget, estimate_item_tokens, select_guards,
    summary_message_content, summary_trigger, token_budget,
};
use crate::config::{ContextConfig, KnowledgeSearchConfig};
use crate::domain::error::{DomainError, OutOfRangeReason};

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Fixed overhead deducted from the budget in the assembly fixtures.
const OVERHEAD: u32 = 10;
/// Output reservation of the assembly fixtures.
const OUTPUT: u32 = 100;

/// One token per byte, no margin: item estimates equal byte lengths.
fn flat_budgets() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: OVERHEAD,
        safety_margin_pct: 0,
        image_token_budget: 7,
        tool_surcharge_tokens: 500,
        web_search_surcharge_tokens: 300,
        code_interpreter_surcharge_tokens: 1000,
        ..EstimationBudgets::default()
    }
}

/// Limits whose truncation budget (with [`flat_budgets`], no surcharges) is
/// exactly `budget`.
fn limits_for_budget(budget: u32) -> ModelLimits {
    ModelLimits {
        context_window: budget + OVERHEAD + OUTPUT,
        max_input_tokens: 0,
        max_output_tokens_applied: OUTPUT,
    }
}

fn text(len: usize) -> String {
    "x".repeat(len)
}

fn message(n: u16, role: ContextRole, len: usize) -> HistoryMessage {
    HistoryMessage {
        id: Uuid::from_u128(u128::from(n)),
        role,
        content: format!("{n:02}{}", text(len.saturating_sub(2))),
        created_at: datetime!(2026-01-01 00:00 UTC) + Duration::seconds(i64::from(n)),
    }
}

fn user(n: u16, len: usize) -> HistoryMessage {
    message(n, ContextRole::User, len)
}

fn assistant(n: u16, len: usize) -> HistoryMessage {
    message(n, ContextRole::Assistant, len)
}

/// System prompt of 10 bytes, user message of 10 bytes, no images.
fn input(budget: u32, recent: Vec<HistoryMessage>) -> ContextInput {
    ContextInput {
        system_prompt: text(10),
        guards: Vec::new(),
        summary: None,
        recent,
        user_message: text(10),
        image_count: 0,
        limits: limits_for_budget(budget),
        budgets: flat_budgets(),
        surcharges: ToolSurcharges::default(),
    }
}

fn summary(len: usize) -> SummaryForContext {
    SummaryForContext {
        text: text(len),
        token_estimate: 42,
    }
}

fn summary_cost(len: usize) -> u32 {
    u32::try_from(summary_message_content(&text(len)).len()).unwrap()
}

fn contents(plan: &ContextPlan) -> Vec<&str> {
    plan.messages.iter().map(|m| m.content.as_str()).collect()
}

fn planned(messages: &[HistoryMessage]) -> Vec<PlannedMessage> {
    messages
        .iter()
        .map(|m| PlannedMessage {
            role: m.role,
            content: m.content.clone(),
        })
        .collect()
}

fn is_budget_exceeded(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::OutOfRange {
            reason: OutOfRangeReason::ContextBudgetExceeded,
            field: "content",
        }
    )
}

// ── effective_budget / token_budget ──────────────────────────────────────

#[test]
fn effective_budget_takes_the_smaller_limit() {
    // Window limit 1000 - 200 = 800.
    assert_eq!(effective_budget(1000, 500, 200), Some(500));
    assert_eq!(effective_budget(1000, 900, 200), Some(800));
    assert_eq!(effective_budget(1000, 800, 200), Some(800));
}

#[test]
fn effective_budget_zero_max_input_means_window_only() {
    assert_eq!(effective_budget(1000, 0, 200), Some(800));
    let limits = ModelLimits {
        context_window: 128_000,
        max_input_tokens: 0,
        max_output_tokens_applied: 4096,
    };
    assert_eq!(limits.effective_budget(), Some(123_904));
}

#[test]
fn effective_budget_is_none_without_room_for_input() {
    assert_eq!(effective_budget(1000, 0, 1000), None);
    assert_eq!(effective_budget(1000, 500, 1001), None);
    assert_eq!(effective_budget(1000, 0, 999), Some(1));
}

#[test]
fn token_budget_deducts_surcharges_and_overhead() {
    let budgets = flat_budgets();
    let limits = ModelLimits {
        context_window: 10_000,
        max_input_tokens: 5000,
        max_output_tokens_applied: 1000,
    };
    let surcharges = ToolSurcharges::for_tools(&budgets, true, true, true);
    // min(5000, 9000) - (500 + 300 + 1000) - 10.
    assert_eq!(token_budget(limits, surcharges, &budgets).unwrap(), 3190);
    let none = ToolSurcharges::default();
    assert_eq!(token_budget(limits, none, &budgets).unwrap(), 4990);
}

#[test]
fn token_budget_with_zero_max_input_uses_window() {
    let budgets = flat_budgets();
    let limits = ModelLimits {
        context_window: 10_000,
        max_input_tokens: 0,
        max_output_tokens_applied: 1000,
    };
    assert_eq!(
        token_budget(limits, ToolSurcharges::default(), &budgets).unwrap(),
        8990
    );
}

#[test]
fn token_budget_fails_when_output_fills_the_window() {
    let budgets = flat_budgets();
    for output in [1000, 1001] {
        let limits = ModelLimits {
            context_window: 1000,
            max_input_tokens: 0,
            max_output_tokens_applied: output,
        };
        let err = token_budget(limits, ToolSurcharges::default(), &budgets).unwrap_err();
        assert!(is_budget_exceeded(&err), "{err:?}");
    }
}

#[test]
fn token_budget_fails_when_deductions_reach_the_input_limit() {
    let budgets = flat_budgets();
    let surcharges = ToolSurcharges::for_tools(&budgets, true, false, false);
    // Deductions 500 + 10 = 510.
    let at_limit = ModelLimits {
        context_window: 2000,
        max_input_tokens: 510,
        max_output_tokens_applied: 100,
    };
    let err = token_budget(at_limit, surcharges, &budgets).unwrap_err();
    assert!(is_budget_exceeded(&err));
    let one_left = ModelLimits {
        max_input_tokens: 511,
        ..at_limit
    };
    assert_eq!(token_budget(one_left, surcharges, &budgets).unwrap(), 1);
}

#[test]
fn surcharges_apply_only_to_tools_sent() {
    let budgets = flat_budgets();
    let none = ToolSurcharges::for_tools(&budgets, false, false, false);
    assert_eq!(none, ToolSurcharges::default());
    assert_eq!(none.total(), 0);
    let web = ToolSurcharges::for_tools(&budgets, false, true, false);
    assert_eq!(
        web,
        ToolSurcharges {
            file_search: 0,
            web_search: 300,
            code_interpreter: 0,
        }
    );
    assert_eq!(
        ToolSurcharges::for_tools(&budgets, true, true, true).total(),
        1800
    );
}

// ── Estimation ───────────────────────────────────────────────────────────

#[test]
fn item_estimate_applies_ratio_and_margin_without_overhead() {
    let budgets = EstimationBudgets::default(); // 4 bytes/token, 100 overhead, 10%.
    assert_eq!(estimate_item_tokens("", &budgets), 0);
    // ceil(10 / 4) = 3; ceil(3 * 110 / 100) = 4.
    assert_eq!(estimate_item_tokens(&text(10), &budgets), 4);
    // ceil(400 / 4) = 100; 100 * 110 / 100 = 110.
    assert_eq!(estimate_item_tokens(&text(400), &budgets), 110);
}

#[test]
fn item_estimate_counts_utf8_bytes_and_clamps_zero_ratio() {
    let budgets = EstimationBudgets {
        bytes_per_token_conservative: 0,
        safety_margin_pct: 0,
        ..EstimationBudgets::default()
    };
    assert_eq!(estimate_item_tokens("abc", &budgets), 3);
    // "\u{e9}" is two UTF-8 bytes.
    assert_eq!(estimate_item_tokens("\u{e9}", &budgets), 2);
}

// ── check_input_length ───────────────────────────────────────────────────

#[test]
fn input_length_boundary_uses_full_estimate() {
    let budgets = EstimationBudgets::default();
    // ceil(40 / 4) + 100 = 110; ceil(110 * 110 / 100) = 121.
    let message = text(40);
    assert!(check_input_length(&message, 121, &budgets).is_ok());
    assert!(check_input_length(&message, 1000, &budgets).is_ok());
    let err = check_input_length(&message, 120, &budgets).unwrap_err();
    assert!(matches!(
        err,
        DomainError::OutOfRange {
            reason: OutOfRangeReason::InputTooLong,
            field: "content",
        }
    ));
}

#[test]
fn input_length_zero_limit_means_no_limit() {
    let budgets = EstimationBudgets::default();
    assert!(check_input_length(&text(1_000_000), 0, &budgets).is_ok());
}

// ── Guards and instructions ──────────────────────────────────────────────

#[test]
fn guards_follow_tool_order_and_selection() {
    let context = ContextConfig {
        recent_messages_limit: 10,
        web_search_guard: "web".to_owned(),
        file_search_guard: "file".to_owned(),
    };
    let knowledge = KnowledgeSearchConfig {
        guard: "knowledge".to_owned(),
        ..KnowledgeSearchConfig::default()
    };
    assert_eq!(
        select_guards(true, true, true, &context, &knowledge),
        ["file", "web", "knowledge"]
    );
    assert_eq!(
        select_guards(false, true, false, &context, &knowledge),
        ["web"]
    );
    assert_eq!(
        select_guards(true, false, true, &context, &knowledge),
        ["file", "knowledge"]
    );
    assert!(select_guards(false, false, false, &context, &knowledge).is_empty());
}

#[test]
fn blank_guards_are_skipped() {
    let context = ContextConfig {
        recent_messages_limit: 10,
        web_search_guard: "  ".to_owned(),
        file_search_guard: String::new(),
    };
    let knowledge = KnowledgeSearchConfig::default();
    let guards = select_guards(true, true, true, &context, &knowledge);
    assert_eq!(guards, [knowledge.guard]);
}

#[test]
fn default_guards_are_the_built_in_texts() {
    let context = ContextConfig::default();
    let knowledge = KnowledgeSearchConfig::default();
    let guards = select_guards(false, true, false, &context, &knowledge);
    assert_eq!(guards, [crate::config::DEFAULT_WEB_SEARCH_GUARD]);
}

#[test]
fn instructions_join_prompt_and_guards_with_blank_lines() {
    let mut req = input(1000, Vec::new());
    req.system_prompt = "You are helpful.".to_owned();
    req.guards = vec!["file".to_owned(), "web".to_owned()];
    let plan = assemble(req).unwrap();
    assert_eq!(plan.instructions, "You are helpful.\n\nfile\n\nweb");
}

#[test]
fn instructions_skip_an_empty_system_prompt() {
    let mut req = input(1000, Vec::new());
    req.system_prompt = String::new();
    req.guards = vec!["web".to_owned()];
    assert_eq!(assemble(req).unwrap().instructions, "web");

    let mut bare = input(1000, Vec::new());
    bare.system_prompt = String::new();
    assert_eq!(assemble(bare).unwrap().instructions, "");
}

// ── Budget in assemble ───────────────────────────────────────────────────

#[test]
fn plan_reports_both_budgets() {
    let mut req = input(1000, Vec::new());
    req.surcharges = ToolSurcharges::for_tools(&req.budgets, false, true, false);
    // effective = window - output = 1000 + OVERHEAD; token budget minus 300 and overhead.
    let plan = assemble(req).unwrap();
    assert_eq!(plan.effective_budget, 1010);
    assert_eq!(plan.token_budget, 700);
}

#[test]
fn max_input_tokens_caps_the_assembly_budget() {
    let mut req = input(1000, vec![user(1, 30), assistant(2, 30)]);
    req.limits.max_input_tokens = 60 + OVERHEAD; // token budget 60.
    let plan = assemble(req).unwrap();
    assert_eq!(plan.effective_budget, 70);
    assert_eq!(plan.token_budget, 60);
    // Mandatory 20, remaining 40: only the newest message fits, and as an
    // assistant message without its question it is dropped too.
    assert!(plan.messages.is_empty());
    assert!(plan.messages_truncated);
}

#[test]
fn budget_errors_surface_from_assemble() {
    let mut req = input(1000, Vec::new());
    req.limits.max_output_tokens_applied = req.limits.context_window;
    assert!(is_budget_exceeded(&assemble(req).unwrap_err()));

    let mut tools = input(1000, Vec::new());
    tools.limits.max_input_tokens = 1810; // = 500 + 300 + 1000 + OVERHEAD.
    tools.surcharges = ToolSurcharges::for_tools(&tools.budgets, true, true, true);
    assert!(is_budget_exceeded(&assemble(tools).unwrap_err()));
}

// ── Mandatory items ──────────────────────────────────────────────────────

#[test]
fn mandatory_items_over_budget_are_rejected() {
    // Mandatory: 10 (system) + 10 (user) = 20.
    let err = assemble(input(19, Vec::new())).unwrap_err();
    assert!(is_budget_exceeded(&err));
}

#[test]
fn mandatory_items_exactly_at_budget_are_accepted() {
    let plan = assemble(input(20, vec![user(1, 5)])).unwrap();
    assert_eq!(plan.assembled_tokens, 20);
    assert!(plan.messages.is_empty());
    assert!(plan.messages_truncated);
}

#[test]
fn guards_and_images_are_mandatory() {
    let mut req = input(40, Vec::new());
    req.guards = vec![text(4)];
    req.image_count = 2;
    // 10 + "\n\n" (2) + 4 + user 10 + 2 * 7 images = 40.
    let plan = assemble(req.clone()).unwrap();
    assert_eq!(plan.assembled_tokens, 40);

    req.limits = limits_for_budget(39);
    assert!(is_budget_exceeded(&assemble(req).unwrap_err()));
}

#[test]
fn large_user_message_is_never_truncated() {
    let mut req = input(1000, vec![user(1, 10), assistant(2, 10)]);
    req.user_message = text(975);
    let plan = assemble(req).unwrap();
    // Mandatory 985 leaves 15: neither history message (10 + 10) fits whole.
    assert_eq!(contents(&plan), Vec::<&str>::new());
    assert_eq!(plan.assembled_tokens, 985);
    assert!(plan.messages_truncated);
}

// ── Thread summary ───────────────────────────────────────────────────────

#[test]
fn kept_summary_is_a_user_message_with_preamble() {
    let mut req = input(1000, Vec::new());
    req.summary = Some(SummaryForContext {
        text: "Earlier the user asked about Rust.".to_owned(),
        token_estimate: 9,
    });
    let plan = assemble(req).unwrap();
    assert_eq!(plan.summary_applied, Some(9));
    assert_eq!(plan.messages.len(), 1);
    assert_eq!(plan.messages[0].role, ContextRole::User);
    assert_eq!(
        plan.messages[0].content,
        format!("{THREAD_SUMMARY_PREAMBLE}{SECTION_SEPARATOR}Earlier the user asked about Rust.")
    );
}

#[test]
fn summary_preamble_counts_toward_its_size() {
    let cost = summary_cost(20);
    assert!(cost > 20);
    // Exactly enough for mandatory (20) + summary with preamble.
    let mut fits = input(20 + cost, Vec::new());
    fits.summary = Some(summary(20));
    let plan = assemble(fits).unwrap();
    assert_eq!(plan.summary_applied, Some(42));
    assert_eq!(plan.assembled_tokens, u64::from(20 + cost));

    // One token short: the summary text alone would fit, but not with its preamble.
    let mut short = input(20 + cost - 1, Vec::new());
    short.summary = Some(summary(20));
    let plan = assemble(short).unwrap();
    assert_eq!(plan.summary_applied, None);
    assert!(plan.messages.is_empty());
    assert_eq!(plan.assembled_tokens, 20);
}

#[test]
fn summary_too_large_is_dropped_but_recent_messages_still_fit() {
    let recent = vec![user(1, 10), assistant(2, 10)];
    let mut req = input(100, recent.clone());
    req.summary = Some(summary(1000));
    let plan = assemble(req).unwrap();
    assert_eq!(plan.summary_applied, None);
    assert_eq!(plan.messages, planned(&recent));
    assert!(!plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 40);
}

#[test]
fn summary_has_priority_over_recent_messages() {
    let cost = summary_cost(10);
    // Mandatory 20 + summary + room for 25 more tokens.
    let mut req = input(
        20 + cost + 25,
        vec![user(1, 10), assistant(2, 10), user(3, 10), assistant(4, 10)],
    );
    req.summary = Some(summary(10));
    let plan = assemble(req).unwrap();
    assert_eq!(plan.summary_applied, Some(42));
    assert_eq!(plan.messages.len(), 3);
    assert_eq!(plan.messages[0].role, ContextRole::User);
    assert!(
        plan.messages[0]
            .content
            .starts_with(THREAD_SUMMARY_PREAMBLE)
    );
    assert_eq!(
        plan.messages[1..],
        planned(&[user(3, 10), assistant(4, 10)])
    );
    assert!(plan.messages_truncated);
}

// ── Recent messages ──────────────────────────────────────────────────────

#[test]
fn everything_fits_without_truncation() {
    let recent = vec![user(1, 10), assistant(2, 15), user(3, 5), assistant(4, 20)];
    let plan = assemble(input(1000, recent.clone())).unwrap();
    assert_eq!(plan.messages, planned(&recent));
    assert!(!plan.messages_truncated);
    assert_eq!(plan.summary_applied, None);
    // 20 mandatory + 50 history.
    assert_eq!(plan.assembled_tokens, 70);
}

#[test]
fn empty_history_is_not_truncation() {
    let plan = assemble(input(1000, Vec::new())).unwrap();
    assert!(plan.messages.is_empty());
    assert!(!plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 20);
}

#[test]
fn oldest_messages_are_dropped_first() {
    let recent = vec![
        user(1, 10),
        assistant(2, 10),
        user(3, 10),
        assistant(4, 10),
        user(5, 10),
        assistant(6, 10),
    ];
    // Mandatory 20 + room for exactly four messages.
    let plan = assemble(input(60, recent)).unwrap();
    assert_eq!(
        plan.messages,
        planned(&[user(3, 10), assistant(4, 10), user(5, 10), assistant(6, 10)])
    );
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 60);
}

#[test]
fn first_non_fitting_message_drops_all_older_ones() {
    // The large user(3) does not fit; the small, older turn must not be kept.
    let recent = vec![
        user(1, 2),
        assistant(2, 2),
        user(3, 100),
        assistant(4, 10),
        user(5, 10),
        assistant(6, 10),
    ];
    let plan = assemble(input(70, recent)).unwrap();
    assert_eq!(plan.messages, planned(&[user(5, 10), assistant(6, 10)]));
    assert!(plan.messages_truncated);
    // assistant(4) fitted the walk but was dropped as a leading answer.
    assert_eq!(plan.assembled_tokens, 40);
}

#[test]
fn kept_range_never_starts_with_an_assistant_message() {
    let recent = vec![user(1, 10), assistant(2, 10), user(3, 10), assistant(4, 10)];
    // Room for three messages: assistant(2), user(3), assistant(4).
    let plan = assemble(input(50, recent)).unwrap();
    assert_eq!(plan.messages, planned(&[user(3, 10), assistant(4, 10)]));
    assert_eq!(plan.messages[0].role, ContextRole::User);
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 40);
}

#[test]
fn consecutive_leading_assistant_messages_are_all_dropped() {
    let recent = vec![
        assistant(1, 10),
        assistant(2, 10),
        user(3, 10),
        assistant(4, 10),
    ];
    let plan = assemble(input(1000, recent)).unwrap();
    assert_eq!(plan.messages, planned(&[user(3, 10), assistant(4, 10)]));
    // The window was cut mid-turn: dropping the orphan answers is truncation.
    assert!(plan.messages_truncated);
}

#[test]
fn all_assistant_window_keeps_nothing() {
    let plan = assemble(input(1000, vec![assistant(1, 10), assistant(2, 10)])).unwrap();
    assert!(plan.messages.is_empty());
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 20);
}

#[test]
fn trailing_user_message_without_answer_is_kept() {
    // A failed earlier turn leaves a user message without an answer.
    let recent = vec![user(1, 10), assistant(2, 10), user(3, 10)];
    let plan = assemble(input(1000, recent.clone())).unwrap();
    assert_eq!(plan.messages, planned(&recent));
    assert!(!plan.messages_truncated);
}

#[test]
fn zero_cost_messages_always_fit() {
    let recent = vec![user(1, 0), assistant(2, 0)];
    let mut req = input(20, recent);
    req.recent[0].content = String::new();
    req.recent[1].content = String::new();
    let plan = assemble(req).unwrap();
    assert_eq!(plan.messages.len(), 2);
    assert!(!plan.messages_truncated);
}

// ── Determinism ──────────────────────────────────────────────────────────

#[test]
fn identical_inputs_give_identical_plans() {
    let mut req = input(
        20 + summary_cost(15) + 35,
        vec![user(1, 10), assistant(2, 10), user(3, 10), assistant(4, 10)],
    );
    req.summary = Some(summary(15));
    req.guards = vec![text(3)];
    req.limits.context_window += 5; // room for the guard and its separator.
    let first = assemble(req.clone()).unwrap();
    for _ in 0..5 {
        assert_eq!(assemble(req.clone()).unwrap(), first);
    }
}

#[test]
fn history_is_ordered_by_created_at_then_id() {
    let base = datetime!(2026-01-01 00:00 UTC);
    let make = |id: u128, role, content: &str, secs: i64| HistoryMessage {
        id: Uuid::from_u128(id),
        role,
        content: content.to_owned(),
        created_at: base + Duration::seconds(secs),
    };
    let chronological = vec![
        make(9, ContextRole::User, "q1", 1),
        make(1, ContextRole::Assistant, "a1", 2),
        make(2, ContextRole::User, "q2", 2),
        make(3, ContextRole::Assistant, "a2", 3),
    ];
    let mut shuffled = chronological.clone();
    shuffled.reverse();
    shuffled.swap(0, 2);

    let sorted_plan = assemble(input(1000, chronological)).unwrap();
    let shuffled_plan = assemble(input(1000, shuffled)).unwrap();
    assert_eq!(sorted_plan, shuffled_plan);
    assert_eq!(contents(&sorted_plan), ["q1", "a1", "q2", "a2"]);
}

// ── Summary trigger ──────────────────────────────────────────────────────

#[test]
fn trigger_disabled_never_fires() {
    for (exists, truncated) in [(false, false), (false, true), (true, false), (true, true)] {
        assert_eq!(
            summary_trigger(1_000, 100, 80, exists, truncated, false),
            SummaryTrigger::None
        );
    }
}

#[test]
fn trigger_matrix() {
    // (assembled, summary_exists, truncated) -> expected; budget 1000, 80%.
    let cases = [
        (799, false, false, SummaryTrigger::None),
        (800, false, false, SummaryTrigger::Proactive),
        (950, false, false, SummaryTrigger::Proactive),
        (799, true, false, SummaryTrigger::None),
        (950, true, false, SummaryTrigger::None),
        (100, false, true, SummaryTrigger::Urgent),
        (100, true, true, SummaryTrigger::Urgent),
        (950, true, true, SummaryTrigger::Urgent),
        (950, false, true, SummaryTrigger::Urgent),
    ];
    for (assembled, exists, truncated, expected) in cases {
        assert_eq!(
            summary_trigger(assembled, 1000, 80, exists, truncated, true),
            expected,
            "assembled={assembled} exists={exists} truncated={truncated}"
        );
    }
}

#[test]
fn trigger_threshold_is_exact_percentage() {
    // 80% of 1001 = 800.8: 800 is below the threshold, 801 reaches it.
    assert_eq!(
        summary_trigger(800, 1001, 80, false, false, true),
        SummaryTrigger::None
    );
    assert_eq!(
        summary_trigger(801, 1001, 80, false, false, true),
        SummaryTrigger::Proactive
    );
    assert_eq!(
        summary_trigger(1001, 1001, 100, false, false, true),
        SummaryTrigger::Proactive
    );
    assert_eq!(
        summary_trigger(u64::MAX, u64::MAX, 100, false, false, true),
        SummaryTrigger::Proactive
    );
}

#[test]
fn trigger_uses_plan_values() {
    let recent = vec![user(1, 10), assistant(2, 10), user(3, 10), assistant(4, 10)];
    let plan = assemble(input(50, recent)).unwrap();
    assert!(plan.messages_truncated);
    assert_eq!(
        summary_trigger(
            plan.assembled_tokens,
            plan.effective_budget,
            80,
            false,
            plan.messages_truncated,
            true,
        ),
        SummaryTrigger::Urgent
    );

    let small = assemble(input(1000, vec![user(1, 10), assistant(2, 10)])).unwrap();
    assert_eq!(
        summary_trigger(
            small.assembled_tokens,
            small.effective_budget,
            80,
            false,
            small.messages_truncated,
            true,
        ),
        SummaryTrigger::None
    );
}
