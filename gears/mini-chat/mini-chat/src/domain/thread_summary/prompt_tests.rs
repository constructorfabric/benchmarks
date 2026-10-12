use super::*;

fn entries(texts: &[(&str, HistoryRole)]) -> Vec<Entry> {
    texts
        .iter()
        .map(|(content, role)| Entry {
            role: *role,
            content: (*content).to_owned(),
        })
        .collect()
}

#[test]
fn an_answer_is_reduced_to_its_summary_block() {
    assert_eq!(
        parse_summary("<analysis>a</analysis>\n<summary>\nText\n</summary>"),
        "Text"
    );
    assert_eq!(parse_summary("plain text"), "plain text");
    assert_eq!(
        parse_summary("<summary>one\n\n\n\ntwo</summary>"),
        "one\n\ntwo"
    );
}

#[test]
fn markup_that_is_left_over_makes_the_summary_empty() {
    for answer in [
        "",
        "  \n ",
        "<summary></summary>",
        "<analysis>only</analysis>",
        "<summary>open",
        "<analysis>open",
        "stray <summary tag",
    ] {
        assert_eq!(parse_summary(answer), "", "{answer:?}");
    }
}

#[test]
fn a_message_is_cut_to_the_limit_in_characters() {
    assert_eq!(cut("abcdefghij", 10), "abcdefghij");
    assert_eq!(cut("abcdefghijk", 10), "abcdefg...");
    assert_eq!(
        cut(&"\u{e9}".repeat(20), 10),
        format!("{}...", "\u{e9}".repeat(7))
    );
    assert_eq!(cut("abcdefghijk", 0), "abcdefghijk");
}

#[test]
fn the_prompt_lists_the_entries_between_the_opening_and_the_instruction() {
    let list = entries(&[("q", HistoryRole::User), ("a", HistoryRole::Assistant)]);
    let prompt = build_prompt(None, &list, 0);
    assert!(prompt.starts_with(FRESH_OPENING));
    assert!(prompt.contains("User: q\n\nAssistant: a\n\n"));
    assert!(prompt.ends_with("Respond with an <analysis> block followed by a <summary> block."));
    let merged = build_prompt(Some("old"), &list, 0);
    assert!(merged.contains("<existing_summary>\nold\n</existing_summary>"));
}

#[test]
fn fitting_keeps_two_entries_and_a_suffix() {
    let list: Vec<Entry> = (0..10)
        .map(|n| Entry {
            role: HistoryRole::User,
            content: format!("{n}{}", "x".repeat(600)),
        })
        .collect();
    let budget = FitBudget {
        tokens: 0,
        bytes_per_token: 4,
    };
    let fitted = fit(list.clone(), None, "system", 0, budget);
    assert_eq!(fitted, list[8..]);
    let roomy = FitBudget {
        tokens: 1_000_000,
        bytes_per_token: 4,
    };
    assert_eq!(fit(list.clone(), None, "system", 0, roomy), list);
    assert_eq!(oldest_fifth(10), 2);
    assert_eq!(oldest_fifth(3), 1);
    assert_eq!(oldest_fifth(2), 0);
}
