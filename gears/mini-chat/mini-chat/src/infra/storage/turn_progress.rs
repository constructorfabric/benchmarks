//! The durable progress and counter updates of a running turn over the Secure ORM (slice
//! S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`, algorithm
//! `tool-accounting`): [`TurnProgress`] for the database provider.

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::{OffsetDateTime, UtcOffset};
use toolkit_db::DBProvider;
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::streaming::{ToolKind, TurnProgress};
use crate::infra::storage::entity::chat_turn;

/// The `chat_turns.state` of a turn that may still make progress.
const STATE_RUNNING: &str = "running";

/// The turn of the tenant while it is `running`: the guard of every progress write.
fn running_turn(turn_id: Uuid) -> Condition {
    Condition::all()
        .add(chat_turn::Column::Id.eq(turn_id))
        .add(chat_turn::Column::State.eq(STATE_RUNNING))
}

/// The completed counter column of a tool: `search_knowledge` counts as `file_search`.
fn counter_column(tool: ToolKind) -> chat_turn::Column {
    match tool {
        ToolKind::WebSearch => chat_turn::Column::WebSearchCompletedCount,
        ToolKind::CodeInterpreter => chat_turn::Column::CodeInterpreterCompletedCount,
        ToolKind::FileSearch | ToolKind::SearchKnowledge => {
            chat_turn::Column::FileSearchCompletedCount
        }
    }
}

#[async_trait]
impl TurnProgress for DBProvider<DomainError> {
    async fn record_tool_done(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        tool: ToolKind,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-durable-count
        // one atomic increment guarded by `state = 'running'`: the orphan watchdog can recover
        // the counts and a turn that left `running` keeps its counters
        let column = counter_column(tool);
        let conn = self.conn()?;
        chat_turn::Entity::update_many()
            .col_expr(
                column,
                Expr::col((chat_turn::Entity, column)).add(Expr::value(1_i32)),
            )
            .filter(running_turn(turn_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-durable-count
        Ok(())
    }

    async fn touch_progress(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        at: OffsetDateTime,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-progress-write
        // the application time, guarded by `state = 'running'`
        let at = at.to_offset(UtcOffset::UTC);
        let conn = self.conn()?;
        chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::LastProgressAt, Expr::value(Some(at)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(at))
            .filter(running_turn(turn_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-progress-write
        Ok(())
    }
}

#[cfg(test)]
#[path = "turn_progress_tests.rs"]
mod turn_progress_tests;
