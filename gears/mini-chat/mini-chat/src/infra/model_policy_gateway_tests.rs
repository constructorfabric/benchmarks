#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::ModelPolicyPluginError;

use super::map_snapshot_error;
use crate::domain::error::DomainError;

#[test]
fn an_unavailable_plugin_is_a_transient_policy_unavailable() {
    let err = map_snapshot_error(&ModelPolicyPluginError::Unavailable("down".to_owned()));

    assert!(
        matches!(&err, DomainError::PolicyUnavailable { diagnostic } if diagnostic.contains("down")),
        "{err:?}"
    );
}

#[test]
fn an_unknown_policy_version_is_not_retryable() {
    let err =
        map_snapshot_error(&ModelPolicyPluginError::PolicyVersionNotFound { policy_version: 7 });

    assert!(
        matches!(&err, DomainError::Internal { diagnostic } if diagnostic.contains('7')),
        "{err:?}"
    );
}

#[test]
fn a_plugin_internal_failure_is_not_retryable() {
    let err = map_snapshot_error(&ModelPolicyPluginError::Internal("corrupt".to_owned()));

    assert!(
        matches!(&err, DomainError::Internal { diagnostic } if diagnostic.contains("corrupt")),
        "{err:?}"
    );
}
