use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

use super::*;

fn model(context_window: u32, max_input: u32, system_prompt: &str) -> ModelCatalogEntry {
    let mut m: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": "m",
        "provider_model_id": "m",
        "display_name": "m",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": context_window,
        "max_output_tokens": 1000,
        "max_input_tokens": max_input,
        "input_tokens_credit_multiplier_micro": 1,
        "output_tokens_credit_multiplier_micro": 1,
        "max_num_results": 5,
        "general_config": {}
    }))
    .unwrap();
    m.system_prompt = system_prompt.to_owned();
    // 1 byte = 1 token, no overhead, no margin: easy arithmetic
    m.estimation_budgets = EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 100,
        tool_surcharge_tokens: 50,
        web_search_surcharge_tokens: 30,
        code_interpreter_surcharge_tokens: 20,
        minimal_generation_floor: 50,
    };
    m
}

fn msg(role: &str, len: usize, tag: char) -> HistoryMessage {
    HistoryMessage {
        role: role.to_owned(),
        content: tag.to_string().repeat(len),
    }
}

fn inputs<'a>(
    m: &'a ModelCatalogEntry,
    user: &'a str,
    recent: Vec<HistoryMessage>,
    summary: Option<&str>,
) -> ContextInputs<'a> {
    ContextInputs {
        model: m,
        max_output_tokens_applied: 1000,
        tools: ToolSet::default(),
        system_instructions: system_instructions(
            m,
            ToolSet::default(),
            false,
            "WEB",
            "FILE",
            "KNOW",
        ),
        summary: summary.map(str::to_owned),
        recent,
        user_message: user,
        image_count: 0,
    }
}

#[test]
fn input_limit_is_min_of_max_input_and_window_minus_output() {
    assert_eq!(input_limit(&model(4096, 3072, ""), 1024), Ok(3072));
    assert_eq!(input_limit(&model(4096, 3500, ""), 1024), Ok(3072));
    assert_eq!(input_limit(&model(4096, 0, ""), 1000), Ok(3096));
    assert_eq!(
        input_limit(&model(4096, 0, ""), 4096),
        Err(ContextBudgetExceeded)
    );
}

#[test]
fn everything_fits() {
    let m = model(3000, 0, "SYS");
    let recent = vec![msg("user", 100, 'a'), msg("assistant", 100, 'b')];
    let plan = assemble(inputs(&m, "hello", recent.clone(), Some("old"))).unwrap();
    assert_eq!(plan.recent, recent);
    assert!(!plan.messages_truncated);
    assert_eq!(plan.instructions, "SYS");
    let s = plan.summary.unwrap();
    assert!(s.starts_with(SUMMARY_PREAMBLE) && s.ends_with("old"));
    assert_eq!(plan.effective_budget, 2000);
    assert_eq!(
        plan.assembled_context_tokens,
        3 + 5 + i64::try_from(s.len()).unwrap() + 200
    );
}

#[test]
fn oldest_messages_dropped_first_by_whole_turns() {
    // budget 2000 - mandatory (3 + 5)
    let m = model(3000, 0, "SYS");
    let recent = vec![
        msg("user", 700, 'a'),
        msg("assistant", 700, 'b'),
        msg("user", 700, 'c'),
        msg("assistant", 700, 'd'),
        msg("user", 300, 'e'),
        msg("assistant", 300, 'f'),
    ];
    let plan = assemble(inputs(&m, "hello", recent.clone(), None)).unwrap();
    // 'f','e','d' fit (1300); 'c' (700) would exceed 1992 -> dropped with all older,
    // then the leading assistant 'd' is dropped too.
    assert_eq!(plan.recent, recent[4..].to_vec());
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 8 + 600);
    // deterministic
    assert_eq!(assemble(inputs(&m, "hello", recent, None)).unwrap(), plan);
}

#[test]
fn summary_dropped_when_it_does_not_fit_and_counts_preamble() {
    let m = model(3000, 0, "SYS");
    let long_summary = "s".repeat(1995);
    let plan = assemble(inputs(&m, "hello", vec![], Some(&long_summary))).unwrap();
    assert!(plan.summary.is_none());
    assert!(plan.summary_token_estimate.is_none());
    let plan = assemble(inputs(
        &m,
        "hello",
        vec![msg("user", 10, 'x')],
        Some("short"),
    ))
    .unwrap();
    assert!(plan.summary_token_estimate.unwrap() > i64::try_from("short".len()).unwrap());
    // the summary takes priority over recent messages
    let plan = assemble(inputs(
        &m,
        "hello",
        vec![msg("user", 1500, 'x')],
        Some(&"s".repeat(800)),
    ))
    .unwrap();
    assert!(plan.summary.is_some());
    assert!(plan.recent.is_empty() && plan.messages_truncated);
}

#[test]
fn mandatory_items_over_budget_fail() {
    let m = model(3000, 0, "SYS");
    let big = "u".repeat(1998);
    assert_eq!(
        assemble(inputs(&m, &big, vec![], None)),
        Err(ContextBudgetExceeded)
    );
    let ok = "u".repeat(1997);
    assert!(assemble(inputs(&m, &ok, vec![], None)).is_ok());
    // images count as mandatory
    let mut inp = inputs(&m, "hello", vec![], None);
    inp.image_count = 20;
    assert_eq!(assemble(inp), Err(ContextBudgetExceeded));
}

#[test]
fn tool_surcharges_and_overhead_reduce_the_budget() {
    let mut m = model(3000, 0, "");
    m.estimation_budgets.fixed_overhead_tokens = 0;
    let mut inp = inputs(&m, "u", vec![msg("user", 1990, 'x')], None);
    assert_eq!(assemble(inp.clone()).unwrap().recent.len(), 1);
    inp.tools = ToolSet {
        file_search: true,
        web_search: true,
        code_interpreter: true,
    };
    let plan = assemble(inp).unwrap();
    assert!(plan.recent.is_empty() && plan.messages_truncated);
    // deductions reaching the limit fail
    let mut m2 = model(1100, 0, "");
    m2.estimation_budgets.fixed_overhead_tokens = 0;
    let mut inp = inputs(&m2, "u", vec![], None);
    inp.tools.file_search = true;
    assert!(assemble(inp).is_ok());
    m2.estimation_budgets.fixed_overhead_tokens = 60;
    m2.estimation_budgets.tool_surcharge_tokens = 40;
    let mut inp = inputs(&m2, "u", vec![], None);
    inp.tools.file_search = true;
    assert_eq!(assemble(inp), Err(ContextBudgetExceeded));
}

#[test]
fn system_instructions_include_guards_for_enabled_tools_only() {
    let m = model(3000, 0, "SYS");
    let none = system_instructions(&m, ToolSet::default(), false, "WEB", "FILE", "KNOW");
    assert_eq!(none, "SYS");
    let all = system_instructions(
        &m,
        ToolSet {
            file_search: true,
            web_search: true,
            code_interpreter: true,
        },
        true,
        "WEB",
        "FILE",
        "KNOW",
    );
    assert_eq!(all, "SYS\n\nFILE\n\nWEB\n\nKNOW");
    let m = model(3000, 0, "  ");
    let web = system_instructions(
        &m,
        ToolSet {
            web_search: true,
            ..ToolSet::default()
        },
        false,
        "WEB",
        "FILE",
        "KNOW",
    );
    assert_eq!(web, "WEB");
}

#[test]
fn summary_trigger_rules() {
    let plan = |assembled: i64, truncated: bool| ContextPlan {
        instructions: String::new(),
        summary: None,
        summary_token_estimate: None,
        recent: vec![],
        messages_truncated: truncated,
        assembled_context_tokens: assembled,
        effective_budget: 1000,
    };
    assert!(!summary_trigger(&plan(799, false), false, 80));
    assert!(summary_trigger(&plan(800, false), false, 80));
    // an existing summary suppresses the proactive trigger
    assert!(!summary_trigger(&plan(900, false), true, 80));
    // truncation always triggers
    assert!(summary_trigger(&plan(10, true), true, 80));
}
