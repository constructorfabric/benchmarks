//! Conversions between persistence entities and domain entities, and the
//! translation of Secure ORM failures.

use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

use crate::domain::error::DomainError;
use crate::domain::model::{
    Attachment, Chat, ChatTurn, ChatVectorStore, Message, MessageAttachment, MessageReaction,
    QuotaUsage, ThreadSummary,
};
use crate::infra::db::entities::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, message_reaction,
    quota_usage, thread_summary,
};

/// Maps a Secure ORM failure to the domain error. The cause is logged here and
/// never reaches a response.
pub fn scope_error(error: &ScopeError) -> DomainError {
    if error.is_unique_violation() {
        // Resolved by the caller (the reserve transaction re-reads the key); the
        // driver text and the constraint name are never inspected or kept.
        return DomainError::UniqueViolation;
    }
    tracing::error!(%error, "secure query failed");
    DomainError::Database(error.to_string())
}

/// Lets `DBProvider<DomainError>` open connections and transactions; the cause
/// is logged and never reaches a response.
impl From<DbError> for DomainError {
    fn from(error: DbError) -> Self {
        if let DbError::Sea(db_error) = &error
            && toolkit_db::secure::is_unique_violation(db_error)
        {
            return Self::UniqueViolation;
        }
        tracing::error!(%error, "database operation failed");
        Self::Database(format!("{error}"))
    }
}

impl From<chat::Model> for Chat {
    fn from(row: chat::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            user_id: row.user_id,
            model: row.model,
            title: row.title,
            is_temporary: row.is_temporary,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl From<message::Model> for Message {
    fn from(row: message::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            request_id: row.request_id,
            role: row.role,
            content: row.content,
            created_at: row.created_at,
        }
    }
}

impl From<chat_turn::Model> for ChatTurn {
    fn from(row: chat_turn::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            request_id: row.request_id,
            state: row.state,
            started_at: row.started_at,
        }
    }
}

impl From<attachment::Model> for Attachment {
    fn from(row: attachment::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            filename: row.filename,
            status: row.status,
            kind: row.attachment_kind,
        }
    }
}

impl From<message_attachment::Model> for MessageAttachment {
    fn from(row: message_attachment::Model) -> Self {
        Self {
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            message_id: row.message_id,
            attachment_id: row.attachment_id,
            created_at: row.created_at,
        }
    }
}

impl From<thread_summary::Model> for ThreadSummary {
    fn from(row: thread_summary::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            summary_text: row.summary_text,
            summarized_up_to_created_at: row.summarized_up_to_created_at,
            summarized_up_to_message_id: row.summarized_up_to_message_id,
        }
    }
}

impl From<chat_vector_store::Model> for ChatVectorStore {
    fn from(row: chat_vector_store::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            vector_store_id: row.vector_store_id,
            provider: row.provider,
        }
    }
}

impl From<quota_usage::Model> for QuotaUsage {
    fn from(row: quota_usage::Model) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            user_id: row.user_id,
            period_type: row.period_type,
            bucket: row.bucket,
            spent_credits_micro: row.spent_credits_micro,
            reserved_credits_micro: row.reserved_credits_micro,
        }
    }
}

impl From<message_reaction::Model> for MessageReaction {
    fn from(row: message_reaction::Model) -> Self {
        Self {
            id: row.id,
            message_id: row.message_id,
            user_id: row.user_id,
            tenant_id: row.tenant_id,
            reaction: row.reaction,
        }
    }
}

#[cfg(test)]
#[path = "mapping_tests.rs"]
mod tests;
