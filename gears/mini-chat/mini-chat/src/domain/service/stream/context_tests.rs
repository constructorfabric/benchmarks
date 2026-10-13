#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::non_ascii_literal)]
//! Slice 2: context assembly and truncation
//! (`cpt-cf-mini-chat-algo-streaming-turns-assemble-context`) and the
//! `INPUT_TOO_LONG` check.
//!
//! The catalog entry of these tests estimates one token per byte (ratio 1,
//! no overhead, no margin) unless a test says otherwise, so every budget is
//! plain arithmetic.

use mini_chat_sdk::{EstimationBudgets, ModelTier};
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::test_support::model;
use crate::domain::service::stream::ports::{HistoryRole, MessageBoundary};
use crate::domain::service::stream::test_support::{at, history};
use crate::domain::service::stream::tools::ToolSpec;

fn entry(window: u32, max_input: u32) -> ModelCatalogEntry {
    let mut entry = model("m", ModelTier::Premium)
        .text_budgets(1, 0, 0)
        .surcharges(100, 11, 22, 33)
        .build();
    entry.context_window = window;
    entry.max_input_tokens = max_input;
    entry.system_prompt = "SYS".to_owned();
    entry
}

fn budgets(ratio: u32, overhead: u32, margin: u32) -> EstimationBudgets {
    let mut budgets = entry(1000, 0).estimation_budgets;
    budgets.bytes_per_token_conservative = ratio;
    budgets.fixed_overhead_tokens = overhead;
    budgets.safety_margin_pct = margin;
    budgets
}

fn bytes(n: usize) -> String {
    "x".repeat(n)
}

fn user(n: u128, size: usize, secs: i64) -> HistoryMessage {
    history(n, HistoryRole::User, &bytes(size), secs)
}

fn assistant(n: u128, size: usize, secs: i64) -> HistoryMessage {
    history(n, HistoryRole::Assistant, &bytes(size), secs)
}

fn image(n: u128) -> ImageAttachment {
    ImageAttachment {
        id: Uuid::from_u128(0x2000 + n),
        provider_file_id: Some(format!("file-img{n}")),
        secondary_file_id: None,
    }
}

fn summary(text: &str, token_estimate: i32) -> ThreadSummaryInfo {
    ThreadSummaryInfo {
        summary_text: text.to_owned(),
        frontier: MessageBoundary {
            created_at: at(-4000),
            id: Uuid::from_u128(0x77),
        },
        token_estimate,
    }
}

/// Inputs of one assembly with the knobs the tests turn.
struct Case {
    entry: ModelCatalogEntry,
    max_output: i64,
    tools: ToolsDecision,
    content: String,
    images: Vec<ImageAttachment>,
    summary: Option<ThreadSummaryInfo>,
    recent: Vec<HistoryMessage>,
}

impl Case {
    /// Window 1000, output 200: an input limit of 800, content `hello`.
    fn new() -> Self {
        Self {
            entry: entry(1000, 0),
            max_output: 200,
            tools: ToolsDecision::default(),
            content: "hello".to_owned(),
            images: Vec::new(),
            summary: None,
            recent: Vec::new(),
        }
    }

    fn assemble(&self) -> Result<ContextPlan, DomainError> {
        assemble_context(&ContextInput {
            entry: &self.entry,
            max_output_tokens_applied: self.max_output,
            tools: &self.tools,
            content: &self.content,
            images: &self.images,
            summary: self.summary.as_ref(),
            recent: &self.recent,
        })
    }

    fn plan(&self) -> ContextPlan {
        self.assemble().unwrap()
    }
}

fn kept_ids(plan: &ContextPlan) -> Vec<u128> {
    plan.history
        .iter()
        .map(|m| m.id.as_u128() - 0x4000)
        .collect()
}

// ---- the text estimate --------------------------------------------------------------------------

#[test]
fn the_text_estimate_follows_the_budget_formula() {
    // ceil((ceil(bytes / 4) + 100) * 110 / 100)
    assert_eq!(estimate_text_tokens(&budgets(4, 100, 10), 4000), 1210);
    assert_eq!(estimate_text_tokens(&budgets(4, 100, 10), 2000), 660);
    assert_eq!(estimate_text_tokens(&budgets(4, 100, 10), 0), 110);
    // Both ceilings: ceil(1 / 4) = 1, ceil(1 * 110 / 100) = 2.
    assert_eq!(estimate_text_tokens(&budgets(4, 0, 10), 1), 2);
    // A ratio of 0 counts as 1.
    assert_eq!(estimate_text_tokens(&budgets(0, 0, 0), 7), 7);
}

#[test]
fn the_estimate_is_the_one_of_the_quota_reserve() {
    // 1000 message bytes with the quota fixture budgets (1 / 0 / 0) are 1000
    // tokens, the same number the reserve estimate uses.
    assert_eq!(
        estimate_text_tokens(&entry(1000, 0).estimation_budgets, 1000),
        1000
    );
}

// ---- INPUT_TOO_LONG -----------------------------------------------------------------------------

fn acceptance_entry(max_input: u32) -> ModelCatalogEntry {
    let mut entry = entry(128_000, max_input);
    entry.estimation_budgets = budgets(4, 100, 10);
    entry
}

#[test]
fn a_message_above_max_input_tokens_is_too_long() {
    let entry = acceptance_entry(1000);
    let error = check_input_length(&entry, &bytes(4000)).unwrap_err();
    assert!(matches!(error, DomainError::InputTooLong), "{error:?}");
}

#[test]
fn a_message_within_max_input_tokens_passes() {
    assert!(check_input_length(&acceptance_entry(1000), &bytes(2000)).is_ok());
}

#[test]
fn max_input_tokens_zero_disables_the_check() {
    assert!(check_input_length(&acceptance_entry(0), &bytes(4000)).is_ok());
}

#[test]
fn the_input_check_counts_the_utf8_bytes() {
    // 600 two-byte characters are 1200 bytes: (300 + 100) * 1.1 = 440 is fine,
    // 1200 ASCII bytes with a ratio of 1 would not be.
    let entry = acceptance_entry(500);
    assert!(check_input_length(&entry, &"é".repeat(600)).is_ok());
    assert!(check_input_length(&entry, &"é".repeat(1500)).is_err());
}

// ---- the plan -----------------------------------------------------------------------------------

#[test]
fn a_plain_turn_is_the_instructions_and_the_current_message() {
    let plan = Case::new().plan();
    assert_eq!(plan.instructions, "SYS");
    assert_eq!(plan.summary, None);
    assert!(plan.history.is_empty());
    let current = plan.current.unwrap();
    assert_eq!(current.content, "hello");
    assert!(current.images.is_empty());
    assert_eq!(plan.thread_summary_applied, None);
    assert!(!plan.messages_truncated);
    // 3 bytes of instructions + 5 bytes of content.
    assert_eq!(plan.assembled_context_tokens, 8);
    assert_eq!(plan.effective_input_budget, 800);
}

#[test]
fn the_instructions_are_the_system_prompt_and_the_guards_joined_by_a_blank_line() {
    let mut case = Case::new();
    case.tools.guards = vec!["FS-GUARD".to_owned(), "WS-GUARD".to_owned()];
    assert_eq!(case.plan().instructions, "SYS\n\nFS-GUARD\n\nWS-GUARD");
}

#[test]
fn without_a_system_prompt_or_guards_the_instructions_are_empty_and_cost_nothing() {
    let mut case = Case::new();
    case.entry.system_prompt = String::new();
    let plan = case.plan();
    assert_eq!(plan.instructions, "");
    assert_eq!(plan.assembled_context_tokens, 5, "only the content");
}

#[test]
fn history_that_fits_is_kept_in_chronological_order() {
    let mut case = Case::new();
    case.recent = vec![user(1, 20, 1), assistant(2, 30, 2), user(3, 10, 3)];
    let plan = case.plan();
    assert_eq!(kept_ids(&plan), [1, 2, 3]);
    assert!(!plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 8 + 20 + 30 + 10);
}

#[test]
fn the_oldest_messages_are_dropped_first_when_the_history_does_not_fit() {
    // Limit 300, mandatory 8: 292 left; 100 bytes per message keeps two.
    let mut case = Case::new();
    case.entry = entry(400, 0);
    case.max_output = 100;
    case.recent = vec![
        user(1, 100, 1),
        assistant(2, 100, 2),
        user(3, 100, 3),
        assistant(4, 100, 4),
    ];
    let plan = case.plan();
    assert_eq!(kept_ids(&plan), [3, 4]);
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 8 + 200);
}

#[test]
fn an_older_message_is_not_kept_after_a_newer_one_was_dropped() {
    // The newest (100) fits, the next (200) does not, the oldest (10) would fit
    // but must not be kept: truncation drops "every older message".
    let mut case = Case::new();
    case.entry = entry(400, 0);
    case.max_output = 100;
    case.recent = vec![user(1, 10, 1), assistant(2, 200, 2), user(3, 100, 3)];
    let plan = case.plan();
    assert_eq!(kept_ids(&plan), [3]);
    assert!(plan.messages_truncated);
}

#[test]
fn a_message_that_exactly_fills_the_budget_is_kept() {
    // 292 left after the mandatory 8.
    let mut case = Case::new();
    case.entry = entry(400, 0);
    case.max_output = 100;
    case.recent = vec![user(1, 292, 1)];
    let plan = case.plan();
    assert_eq!(kept_ids(&plan), [1]);
    assert!(!plan.messages_truncated);
}

#[test]
fn an_answer_is_never_sent_without_its_question() {
    // Newest to oldest: a2 (100), u2 (50), a1 (100) fit, u1 (100) does not, so
    // the kept history would start with the assistant message a1: it is dropped.
    let mut case = Case::new();
    case.entry = entry(400, 0);
    case.max_output = 100;
    case.recent = vec![
        user(1, 100, 1),
        assistant(2, 100, 2),
        user(3, 50, 3),
        assistant(4, 100, 4),
    ];
    let plan = case.plan();
    assert_eq!(kept_ids(&plan), [3, 4]);
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 8 + 50 + 100);
}

#[test]
fn a_history_that_is_only_assistant_messages_is_dropped_entirely() {
    let mut case = Case::new();
    case.recent = vec![assistant(1, 10, 1), assistant(2, 10, 2)];
    let plan = case.plan();
    assert!(plan.history.is_empty());
    assert!(plan.messages_truncated);
}

// ---- the thread summary -------------------------------------------------------------------------

fn summary_case(text: &str) -> Case {
    let mut case = Case::new();
    case.entry = entry(2000, 0);
    case.max_output = 200;
    case.summary = Some(summary(text, 42));
    case
}

#[test]
fn a_kept_summary_is_a_user_item_with_the_preamble_and_is_reported() {
    let plan = summary_case(&bytes(100)).plan();
    let item = plan.summary.unwrap();
    assert!(item.text.starts_with(SUMMARY_PREAMBLE));
    assert_eq!(
        item.text,
        format!("{SUMMARY_PREAMBLE}\n\n{}", bytes(100)),
        "preamble, a blank line, the summary"
    );
    assert_eq!(plan.thread_summary_applied, Some(42));
}

#[test]
fn the_preamble_is_the_b55_text() {
    assert_eq!(
        SUMMARY_PREAMBLE,
        "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after."
    );
}

#[test]
fn a_kept_summary_is_counted_before_the_history() {
    // Limit 1800, mandatory 8. The summary item is preamble + 2 + 100 bytes.
    let item = SUMMARY_PREAMBLE.len() + 2 + 100;
    let history_budget = 1800 - 8 - item;
    let mut case = summary_case(&bytes(100));
    // A message that fits only without the summary: one byte too many.
    case.recent = vec![user(1, history_budget + 1, 1), assistant(2, 10, 2)];
    let plan = case.plan();
    assert!(plan.summary.is_some());
    assert!(
        plan.history.is_empty(),
        "the answer is dropped with its question: {:?}",
        kept_ids(&plan)
    );
    assert!(plan.messages_truncated);
    assert_eq!(
        plan.assembled_context_tokens,
        i64::try_from(8 + item).unwrap()
    );
}

#[test]
fn a_summary_that_does_not_fit_is_dropped_entirely() {
    let mut case = summary_case(&bytes(5000));
    case.recent = vec![user(1, 10, 1), assistant(2, 10, 2)];
    let plan = case.plan();
    assert_eq!(plan.summary, None);
    assert_eq!(plan.thread_summary_applied, None);
    assert_eq!(kept_ids(&plan), [1, 2], "the history still uses the budget");
    assert!(
        !plan.messages_truncated,
        "a dropped summary is not a truncation"
    );
}

#[test]
fn a_summary_that_exactly_fits_is_kept() {
    let item = SUMMARY_PREAMBLE.len() + 2;
    let text = bytes(1800 - 8 - item);
    let plan = summary_case(&text).plan();
    assert!(plan.summary.is_some());
}

// ---- budget errors ------------------------------------------------------------------------------

#[test]
fn output_tokens_at_or_above_the_window_leave_no_room() {
    for output in [1000, 1001, 5000] {
        let mut case = Case::new();
        case.max_output = output;
        assert!(
            matches!(case.assemble(), Err(DomainError::ContextBudgetExceeded)),
            "output {output}"
        );
    }
}

#[test]
fn mandatory_items_above_the_budget_exceed_it() {
    let mut case = Case::new();
    case.content = bytes(798); // 3 + 798 = 801 > 800
    assert!(matches!(
        case.assemble(),
        Err(DomainError::ContextBudgetExceeded)
    ));
    case.content = bytes(797); // exactly 800
    assert!(case.assemble().is_ok());
}

#[test]
fn a_huge_system_prompt_exceeds_the_budget_it_is_never_truncated() {
    let mut case = Case::new();
    case.entry.system_prompt = bytes(900);
    assert!(matches!(
        case.assemble(),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn images_are_mandatory_items_of_image_token_budget_each() {
    let mut case = Case::new();
    case.images = vec![image(1), image(2)];
    let plan = case.plan();
    assert_eq!(plan.assembled_context_tokens, 8 + 200);
    assert_eq!(plan.current.unwrap().images, vec![image(1), image(2)]);
    // Eight images make 800 tokens on their own: the mandatory items overflow.
    case.images = (1..=8).map(image).collect();
    assert!(matches!(
        case.assemble(),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn tool_surcharges_are_taken_from_the_budget() {
    // Mandatory 783 (instructions 3 + content 780) of a limit of 800.
    let mut case = Case::new();
    case.content = bytes(780);
    assert!(case.assemble().is_ok());
    // file_search: 11 -> 789 left, still fits.
    case.tools.tools = vec![ToolSpec::FileSearch {
        vector_store_id: "vs".to_owned(),
        max_num_results: 5,
    }];
    assert!(case.assemble().is_ok());
    // plus web_search 22 and code_interpreter 33: 734 left, does not fit.
    case.tools.tools.push(ToolSpec::WebSearch {
        search_context_size: mini_chat_sdk::WebSearchContextSize::Low,
    });
    case.tools.tools.push(ToolSpec::CodeInterpreter {
        file_ids: vec!["f".to_owned()],
    });
    assert!(matches!(
        case.assemble(),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn search_knowledge_has_no_surcharge() {
    let mut case = Case::new();
    case.content = bytes(797); // exactly the limit
    case.tools.tools = vec![ToolSpec::SearchKnowledge];
    assert!(case.assemble().is_ok());
}

#[test]
fn the_fixed_overhead_is_taken_from_the_budget() {
    let mut case = Case::new();
    case.entry.estimation_budgets.fixed_overhead_tokens = 400;
    // budget 800 - 400; instructions 3 + 400 and content 5 + 400 are 808 > 400.
    assert!(matches!(
        case.assemble(),
        Err(DomainError::ContextBudgetExceeded)
    ));
    // An overhead as large as the limit leaves a budget of 0.
    case.entry.estimation_budgets.fixed_overhead_tokens = 800;
    assert!(matches!(
        case.assemble(),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

// ---- the input limit ----------------------------------------------------------------------------

#[test]
fn max_input_tokens_lowers_the_input_limit_when_smaller() {
    let mut case = Case::new();
    case.entry = entry(1000, 500);
    assert_eq!(case.plan().effective_input_budget, 500);
}

#[test]
fn max_input_tokens_zero_or_larger_leaves_the_window_limit() {
    let mut case = Case::new();
    case.entry = entry(1000, 0);
    assert_eq!(case.plan().effective_input_budget, 800);
    case.entry = entry(1000, 900);
    assert_eq!(case.plan().effective_input_budget, 800);
}

#[test]
fn the_history_budget_follows_the_lowered_limit() {
    let mut case = Case::new();
    case.entry = entry(1000, 100); // limit 100, mandatory 8: 92 left
    case.recent = vec![user(1, 50, 1), assistant(2, 50, 2)];
    let plan = case.plan();
    assert!(plan.history.is_empty(), "{:?}", kept_ids(&plan));
    assert!(plan.messages_truncated);
}

// ---- determinism --------------------------------------------------------------------------------

#[test]
fn identical_inputs_give_identical_plans() {
    let mut case = summary_case(&bytes(50));
    case.recent = vec![user(1, 30, 1), assistant(2, 40, 2), user(3, 20, 3)];
    case.images = vec![image(1)];
    case.tools.guards = vec!["G".to_owned()];
    assert_eq!(case.plan(), case.plan());
}
