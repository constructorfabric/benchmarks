#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Concurrent mutations on a file-backed WAL database with a pool, so the
//! transactions really run on different connections and contend for the file
//! lock: exactly one wins, the loser gets a conflict (never a server error) and
//! leaves nothing behind.

use crate::domain::error::DomainError;
use crate::domain::service::mutations::ports::MutationRepository;
use crate::domain::service::mutations::types::MutationOp;
use crate::mutation_test_env::{MutationEnv, SeedTurn};

const ROUNDS: usize = 3;

fn is_conflict(error: &DomainError) -> bool {
    matches!(
        error,
        DomainError::UniqueViolation | DomainError::NotLatestTurn
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_replacements_of_the_same_turn_commit_exactly_one() {
    for round in 0..ROUNDS {
        let env = MutationEnv::file().await;
        let old = SeedTurn::completed(1, 10).attached(&[]);
        env.seed_turn(&old).await;
        let (first, second) = (
            env.replacement_plan(&old, MutationOp::Retry, "question 1"),
            env.replacement_plan(&old, MutationOp::Edit, "edited"),
        );

        let (a, b) = tokio::join!(
            env.repo.replace_turn(&first),
            env.repo.replace_turn(&second)
        );

        let outcomes = [a, b];
        let wins = outcomes.iter().filter(|r| r.is_ok()).count();
        assert_eq!(wins, 1, "round {round}: {outcomes:?}");
        for error in outcomes.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                is_conflict(error),
                "round {round}: a conflict, not {error:?}"
            );
        }
        assert_eq!(
            env.live_turn_requests().await.len(),
            1,
            "exactly one live new turn"
        );
        assert_eq!(
            env.count("chat_turns").await,
            2,
            "the old turn and one new turn"
        );
        assert_eq!(env.mutation_events().await.len(), 1, "one audit event");
        assert_eq!(
            env.count("messages").await,
            3,
            "two old messages and one new user message"
        );
        let winner = if outcomes[0].is_ok() { &first } else { &second };
        assert_eq!(
            env.replaced_by_request(old.request_id()).await,
            Some(winner.new_request_id)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_deletes_commit_exactly_one_and_the_loser_is_not_the_latest_turn() {
    for round in 0..ROUNDS {
        let env = MutationEnv::file().await;
        let old = SeedTurn::completed(1, 10);
        env.seed_turn(&old).await;
        let (first, second) = (env.delete_plan(&old), env.delete_plan(&old));

        let (a, b) = tokio::join!(env.repo.delete_turn(&first), env.repo.delete_turn(&second));

        let outcomes = [a, b];
        assert_eq!(
            outcomes.iter().filter(|r| r.is_ok()).count(),
            1,
            "round {round}: {outcomes:?}"
        );
        for error in outcomes.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                matches!(error, DomainError::NotLatestTurn),
                "round {round}: {error:?}"
            );
        }
        assert_eq!(env.mutation_events().await.len(), 1);
        assert!(env.live_turn_requests().await.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_replacement_and_delete_never_both_commit() {
    for round in 0..ROUNDS {
        let env = MutationEnv::file().await;
        let old = SeedTurn::completed(1, 10);
        env.seed_turn(&old).await;
        let (replace, delete) = (
            env.replacement_plan(&old, MutationOp::Retry, "question 1"),
            env.delete_plan(&old),
        );

        let (a, b) = tokio::join!(
            env.repo.replace_turn(&replace),
            env.repo.delete_turn(&delete)
        );

        assert_eq!(
            usize::from(a.is_ok()) + usize::from(b.is_ok()),
            1,
            "round {round}: exactly one of them commits: {a:?} / {b:?}"
        );
        if let Err(error) = &a {
            assert!(is_conflict(error), "{error:?}");
        }
        if let Err(error) = &b {
            assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
        }
        assert_eq!(env.mutation_events().await.len(), 1, "one audit event");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_loser_of_a_race_leaves_no_row_behind() {
    let env = MutationEnv::file().await;
    let old = SeedTurn::completed(1, 10);
    env.seed_turn(&old).await;
    let (first, second) = (
        env.replacement_plan(&old, MutationOp::Retry, "question 1"),
        env.replacement_plan(&old, MutationOp::Retry, "question 1"),
    );

    let (a, _b) = tokio::join!(
        env.repo.replace_turn(&first),
        env.repo.replace_turn(&second)
    );

    let (winner, loser) = if a.is_ok() {
        (&first, &second)
    } else {
        (&second, &first)
    };
    assert_eq!(env.messages_of(loser.new_request_id).await.len(), 0);
    assert_eq!(env.messages_of(winner.new_request_id).await.len(), 1);
    assert_eq!(
        env.quota
            .db
            .query_i64(&format!(
                "SELECT COUNT(*) FROM chat_turns WHERE request_id = {}",
                crate::chat_test_env::blob(loser.new_request_id)
            ))
            .await,
        0
    );
    assert_eq!(env.links_of(loser.new_user_message_id).await.len(), 0);
}
