use super::*;
use mini_chat_sdk::EstimationBudgets;

fn model(window: u32, max_in: u32) -> ModelCatalogEntry {
    let mut m: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": "m", "provider_model_id": "m", "display_name": "m", "provider_id": "p", "tier": "standard",
        "enabled": true, "system_prompt": "SYS"
    }))
    .unwrap();
    m.context_window = window;
    m.max_input_tokens = max_in;
    m.estimation_budgets = EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 10,
        tool_surcharge_tokens: 5,
        web_search_surcharge_tokens: 7,
        code_interpreter_surcharge_tokens: 9,
        minimal_generation_floor: 1,
    };
    m
}

fn msg(role: Role, s: &str) -> HistoryMessage {
    HistoryMessage { role, content: s.to_owned() }
}

fn inputs<'a>(m: &'a ModelCatalogEntry, recent: Vec<HistoryMessage>, current: &'a str) -> ContextInputs<'a> {
    ContextInputs {
        model: m,
        max_output_tokens_applied: 10,
        tools: ToolFlags::default(),
        knowledge_search: false,
        guards: ("FS", "WS", "KS"),
        summary: None,
        recent,
        current_text: current,
        current_images: vec![],
    }
}

#[test]
fn budget_formula() {
    assert_eq!(effective_budget(&model(100, 0), 10), 90);
    assert_eq!(effective_budget(&model(100, 50), 10), 50);
    assert_eq!(effective_budget(&model(0, 0), 10), i64::MAX);
}

#[test]
fn keeps_everything_when_it_fits() {
    let m = model(1000, 0);
    let plan = assemble(&inputs(&m, vec![msg(Role::User, "q1"), msg(Role::Assistant, "a1")], "q2")).unwrap();
    assert_eq!(plan.instructions, "SYS");
    assert_eq!(plan.input.len(), 3);
    assert!(!plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 3 + 2 + 2 + 2);
}

#[test]
fn guards_are_appended_for_sent_tools() {
    let m = model(1000, 0);
    let mut i = inputs(&m, vec![], "q");
    i.tools = ToolFlags { file_search: true, web_search: true, code_interpreter: false };
    let plan = assemble(&i).unwrap();
    assert_eq!(plan.instructions, "SYS\n\nFS\n\nWS");
}

#[test]
fn drops_oldest_whole_turns_first() {
    let m = model(40, 0); // budget = 30
    let recent = vec![
        msg(Role::User, "aaaaaaaaaa"),
        msg(Role::Assistant, "bbbbbbbbbb"),
        msg(Role::User, "cccccc"),
        msg(Role::Assistant, "dddddd"),
    ];
    // mandatory = 3 (SYS) + 4 (q) = 7; remaining 23 => keeps d(6), c(6), b(10) = 22 -> then a doesn't fit
    let plan = assemble(&inputs(&m, recent, "qqqq")).unwrap();
    assert!(plan.messages_truncated);
    // b (assistant) would start the kept range, so it is dropped too
    let texts: Vec<String> = plan
        .input
        .iter()
        .map(|im| match &im.parts[0] {
            ContentPart::Text(t) => t.clone(),
            ContentPart::Image { .. } => String::new(),
        })
        .collect();
    assert_eq!(texts, vec!["cccccc", "dddddd", "qqqq"]);
    assert_eq!(plan.input[0].role, Role::User);
}

#[test]
fn mandatory_overflow_is_rejected() {
    let m = model(20, 0);
    assert!(matches!(assemble(&inputs(&m, vec![], &"x".repeat(50))), Err(DomainError::ContextBudgetExceeded)));
    let m = model(10, 0); // max_output >= window
    assert!(matches!(assemble(&inputs(&m, vec![], "x")), Err(DomainError::ContextBudgetExceeded)));
    let m30 = model(30, 0);
    let mut img = inputs(&m30, vec![], "q");
    img.current_images = vec![ImageRef { file_id: "f".into(), secondary_file_id: None }; 2];
    assert!(matches!(assemble(&img), Err(DomainError::ContextBudgetExceeded)), "images count toward mandatory");
}

#[test]
fn summary_is_dropped_when_it_does_not_fit() {
    let m = model(1000, 0);
    let mut i = inputs(&m, vec![msg(Role::User, "q1")], "q2");
    i.summary = Some(("facts".into(), 12));
    let plan = assemble(&i).unwrap();
    assert_eq!(plan.summary_applied, Some(12));
    match &plan.input[0].parts[0] {
        ContentPart::Text(t) => assert!(t.starts_with(SUMMARY_PREAMBLE) && t.ends_with("facts")),
        ContentPart::Image { .. } => panic!(),
    }
    assert_eq!(plan.input[0].role, Role::User);
    let small = model(40, 0);
    let mut i2 = i;
    i2.model = &small;
    i2.summary = Some(("x".repeat(100), 1));
    let plan = assemble(&i2).unwrap();
    assert!(plan.summary_applied.is_none());
}

#[test]
fn trigger_rules() {
    let m = model(1000, 0);
    let plan = assemble(&inputs(&m, vec![], "q")).unwrap();
    assert!(!summary_trigger(&plan, false, 80));
    let mut p = plan;
    p.assembled_tokens = (p.effective_budget * 80).checked_div(100).unwrap();
    assert!(summary_trigger(&p, false, 80));
    assert!(!summary_trigger(&p, true, 80), "existing summary only re-triggers on truncation");
    p.messages_truncated = true;
    assert!(summary_trigger(&p, true, 80));
}
