#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the `$filter` complexity guard.

use std::time::Duration;

use tokio::sync::Semaphore;

use super::{
    MAX_CONCURRENT_STRUCTURE_CHECKS, MAX_GUARDED_FILTER_LEN, MAX_LOGICAL_OPERATORS, MAX_NESTING,
    Overrun, Rejected, can_nest, check_filter, check_structure_on_big_stack, check_structure_with,
    count_logical_keywords,
};

fn chain(op: &str, terms: usize) -> String {
    vec!["title eq 'a'"; terms].join(&format!(" {op} "))
}

#[test]
fn a_filter_at_the_operator_bound_passes_and_one_more_is_rejected() {
    let at_bound = chain("or", MAX_LOGICAL_OPERATORS + 1);
    let over = chain("or", MAX_LOGICAL_OPERATORS + 2);

    assert_eq!(check_filter(&at_bound), Ok(()));
    assert_eq!(check_filter(&over), Err(Overrun::Operators));
}

#[test]
fn and_or_and_not_are_all_counted_in_any_case() {
    let mixed = format!(
        "{} AND not title eq 'a' Or title eq 'a'",
        chain("and", MAX_LOGICAL_OPERATORS - 1)
    );

    assert_eq!(check_filter(&mixed), Err(Overrun::Operators));
}

#[test]
fn operators_inside_string_literals_are_data() {
    let text = "or and not ".repeat(100);
    let filter = format!("title eq '{}'", &text[..text.len().min(1500)]);

    assert_eq!(check_filter(&filter), Ok(()));
    assert_eq!(check_filter("title eq 'it''s and or not'"), Ok(()));
}

#[test]
fn nesting_beyond_the_bound_is_rejected() {
    let ok = format!(
        "{}title eq 'a'{}",
        "(".repeat(MAX_NESTING),
        ")".repeat(MAX_NESTING)
    );
    let deep = format!(
        "{}title eq 'a'{}",
        "(".repeat(MAX_NESTING + 1),
        ")".repeat(MAX_NESTING + 1)
    );

    assert_eq!(check_filter(&ok), Ok(()));
    assert_eq!(check_filter(&deep), Err(Overrun::Nesting));
}

#[test]
fn a_filter_longer_than_the_guard_bound_is_rejected_and_one_beyond_the_platform_limit_is_left_to_it()
 {
    let long = format!("title eq '{}'", "x".repeat(MAX_GUARDED_FILTER_LEN));
    let beyond_platform = format!("title eq '{}'", "x".repeat(9000));

    assert_eq!(check_filter(&long), Err(Overrun::Length));
    assert_eq!(check_filter(&beyond_platform), Ok(()));
}

#[test]
fn the_wide_filters_that_crash_the_parser_are_rejected_before_it_runs() {
    assert_eq!(check_filter(&chain("or", 250)), Err(Overrun::Length));
    assert_eq!(check_filter(&chain("or", 400)), Err(Overrun::Length));
    // Short terms keep a wide chain under the length bound: the operator
    // bound still rejects it.
    let short = vec!["id eq 1"; 120].join(" or ");
    assert!(short.len() < MAX_GUARDED_FILTER_LEN);
    assert_eq!(check_filter(&short), Err(Overrun::Operators));
}

#[test]
fn keywords_are_counted_inside_alphabetic_runs_without_overlap_in_any_case() {
    assert_eq!(count_logical_keywords(""), 0);
    assert_eq!(count_logical_keywords("and"), 1);
    assert_eq!(count_logical_keywords("ortrue"), 1);
    assert_eq!(count_logical_keywords("ortrueortrue"), 2);
    assert_eq!(count_logical_keywords("nottrue"), 1);
    assert_eq!(count_logical_keywords("NoTOr"), 2);
    assert_eq!(count_logical_keywords("orid"), 1);
    assert_eq!(count_logical_keywords("notnotnot"), 3);
    assert_eq!(count_logical_keywords("andornot"), 3);
    // The vocabulary of both lists contains none of them.
    for word in [
        "updated_at",
        "created_at",
        "id",
        "title",
        "role",
        "contains",
        "startswith",
        "endswith",
        "eq",
        "ne",
        "gt",
        "ge",
        "lt",
        "le",
        "true",
        "false",
        "null",
    ] {
        for part in word.split('_') {
            assert_eq!(count_logical_keywords(part), 0, "{word}");
        }
    }
}

#[test]
fn glued_operator_chains_are_counted_by_the_lexical_check() {
    let ortrue = format!("true{}", "ortrue".repeat(339));
    let nots = format!("{}true", "not".repeat(300));
    let glued_ids = format!("id eq 1{}", "orid eq 1".repeat(200));

    assert!(ortrue.len() <= MAX_GUARDED_FILTER_LEN);
    assert!(nots.len() <= MAX_GUARDED_FILTER_LEN);
    assert_eq!(check_filter(&ortrue), Err(Overrun::Operators));
    assert_eq!(check_filter(&nots), Err(Overrun::Operators));
    assert_eq!(check_filter(&glued_ids), Err(Overrun::Operators));
}

#[test]
fn keywords_in_string_literals_and_names_do_not_count_when_glued_to_text() {
    assert_eq!(check_filter("title eq 'rock and roll'"), Ok(()));
    assert_eq!(check_filter("contains(title,'or')"), Ok(()));
    assert_eq!(check_filter("title eq 'a' and id eq 1orid eq 1"), Ok(()));
}

fn too_complex(result: &Result<(), Rejected>) -> bool {
    matches!(result, Err(Rejected::TooComplex(Overrun::Structure)))
}

#[tokio::test]
async fn the_structural_check_rejects_a_deep_or_wide_ast_the_lexical_check_passes() {
    let deep = vec!["id eq 1"; 40].join(" or ");
    let wide = format!("title in ({})", vec!["'a'"; 250].join(","));
    let nine_nodes = "title eq 'a' and id eq 1 or role eq 'user'";

    assert_eq!(check_filter(&deep), Ok(()));
    assert_eq!(check_filter(&wide), Ok(()));
    assert!(too_complex(&check_structure_on_big_stack(&deep).await));
    assert!(too_complex(&check_structure_on_big_stack(&wide).await));
    assert!(check_structure_on_big_stack(nine_nodes).await.is_ok());
}

#[tokio::test]
async fn the_structural_check_rejects_an_unparsable_filter_as_invalid_filter() {
    for raw in [
        "title eq",
        "title eq )",
        &format!("{}1 eq{}", "not(".repeat(32), ")".repeat(32)),
    ] {
        assert!(
            matches!(
                check_structure_on_big_stack(raw).await,
                Err(Rejected::Unparsable(_))
            ),
            "{raw}"
        );
    }
    assert!(check_structure_on_big_stack("").await.is_err());
}

#[tokio::test]
async fn an_unparsable_filter_gets_the_problem_the_platform_extractor_gives() {
    let raw = "title eq";
    let Err(rejected) = check_structure_on_big_stack(raw).await else {
        panic!("must be rejected");
    };
    let platform = toolkit_canonical_errors::CanonicalError::from(
        toolkit_odata::parse_filter_string(raw).unwrap_err(),
    );

    assert_eq!(
        format!("{:?}", rejected.into_problem()),
        format!("{platform:?}")
    );
}

#[test]
fn only_a_parenthesis_or_a_not_can_nest() {
    assert!(!can_nest("title eq 'a' and id eq 1 or role eq 'user'"));
    assert!(can_nest("(title eq 'a')"));
    assert!(can_nest("contains(title,'a')"));
    assert!(can_nest("not title eq 'a'"));
    assert!(can_nest("NOTtrue"));
    assert!(can_nest("truenottrue"));
}

fn leak_permits(count: usize) -> &'static Semaphore {
    Box::leak(Box::new(Semaphore::new(count)))
}

#[tokio::test]
async fn without_a_free_slot_the_check_gives_up_after_the_wait_as_unavailable() {
    let permits = leak_permits(MAX_CONCURRENT_STRUCTURE_CHECKS);
    let held = permits
        .acquire_many(u32::try_from(MAX_CONCURRENT_STRUCTURE_CHECKS).unwrap())
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let nesting = check_structure_with("(id eq 1)", permits, Duration::from_millis(50)).await;

    assert!(matches!(nesting, Err(Rejected::Unavailable)));
    assert!(started.elapsed() < Duration::from_secs(2));
    // A flat chain needs no thread, so no slot.
    assert!(
        check_structure_with("id eq 1", permits, Duration::from_millis(50))
            .await
            .is_ok()
    );

    drop(held);
    assert!(
        check_structure_with("(id eq 1)", permits, Duration::from_millis(50))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn slots_are_returned_after_a_burst_larger_than_the_bound() {
    let permits = leak_permits(2);
    let filter = format!("{}id eq 1{}", "(".repeat(20), ")".repeat(20));

    let results = futures::future::join_all(
        (0..16).map(|_| check_structure_with(&filter, permits, Duration::from_secs(30))),
    )
    .await;

    assert!(results.iter().all(Result::is_ok));
    // Each thread gives its slot back after the permit holder is dropped.
    for _ in 0..200 {
        if permits.available_permits() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(permits.available_permits(), 2);
}

#[tokio::test]
async fn a_closed_semaphore_is_unavailable() {
    let permits = leak_permits(1);
    permits.close();

    let result = check_structure_with("(id eq 1)", permits, Duration::from_millis(50)).await;

    assert!(matches!(result, Err(Rejected::Unavailable)));
}
