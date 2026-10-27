//! Shared service context: repositories, gateways, PEP, config.

use std::sync::Arc;

use crate::config::MiniChatConfig;
use crate::domain::authz::Pep;
use crate::infra::gateways::{AuditPort, PolicyPort};
use crate::infra::storage::repo::Db;
use crate::infra::storage::repo::attachments::AttachmentRepo;
use crate::infra::storage::repo::chats::ChatRepo;
use crate::infra::storage::repo::messages::MessageRepo;
use crate::infra::storage::repo::quota::QuotaRepo;
use crate::infra::storage::repo::reactions::ReactionRepo;
use crate::infra::storage::repo::summaries::{MessageAttachmentRepo, SummaryRepo};
use crate::infra::storage::repo::turns::TurnRepo;
use crate::infra::storage::repo::vector_stores::VectorStoreRepo;

/// Aggregated repositories.
#[derive(Clone)]
pub struct Repos {
    /// Chats.
    pub chats: ChatRepo,
    /// Messages.
    pub messages: MessageRepo,
    /// Turns.
    pub turns: TurnRepo,
    /// Attachments.
    pub attachments: AttachmentRepo,
    /// Message-attachment links.
    pub message_attachments: MessageAttachmentRepo,
    /// Thread summaries.
    pub summaries: SummaryRepo,
    /// Chat vector stores.
    pub vector_stores: VectorStoreRepo,
    /// Quota usage.
    pub quota: QuotaRepo,
    /// Reactions.
    pub reactions: ReactionRepo,
}

impl Repos {
    /// Build all repositories over the gear database.
    #[must_use]
    pub fn new(db: &Db) -> Self {
        Self {
            chats: ChatRepo::new(db.clone()),
            messages: MessageRepo::new(db.clone()),
            turns: TurnRepo::new(db.clone()),
            attachments: AttachmentRepo::new(db.clone()),
            message_attachments: MessageAttachmentRepo::new(db.clone()),
            summaries: SummaryRepo::new(db.clone()),
            vector_stores: VectorStoreRepo::new(db.clone()),
            quota: QuotaRepo::new(db.clone()),
            reactions: ReactionRepo::new(db.clone()),
        }
    }
}

/// The shared service context.
pub struct ServiceContext {
    /// Gear configuration.
    pub config: Arc<MiniChatConfig>,
    /// The gear database.
    pub db: Db,
    /// Repositories.
    pub repos: Repos,
    /// Policy enforcement point.
    pub pep: Arc<Pep>,
    /// Model policy gateway.
    pub policy: Arc<dyn PolicyPort>,
    /// Audit gateway.
    pub audit: Arc<dyn AuditPort>,
    /// Knowledge search runtime (only when `knowledge_search.enabled`).
    pub knowledge: Option<crate::infra::llm::knowledge::KnowledgeSearchRuntime>,
    /// Metrics instruments.
    pub metrics: crate::infra::metrics::Metrics,
    /// The outbox enqueuer (installed when the pipeline starts).
    pub outbox: Arc<crate::infra::outbox::SharedEnqueuer>,
}

/// The application service registry handed to REST handlers.
pub struct AppServices {
    /// Chat service.
    pub chats: Arc<crate::domain::chat_service::ChatService>,
    /// Model service.
    pub models: Arc<crate::domain::model_service::ModelService>,
    /// Stream service.
    pub stream: Arc<crate::domain::stream_service::StreamService>,
    /// Turn service.
    pub turns: Arc<crate::domain::turn_service::TurnService>,
    /// Attachment service.
    pub attachments: Arc<crate::domain::attachment_service::AttachmentService>,
    /// Gear configuration.
    pub config: Arc<MiniChatConfig>,
}

impl AppServices {
    /// The SSE ping interval.
    #[must_use]
    pub fn ping_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.config.streaming.sse_ping_interval_seconds))
    }
}

/// Quota period boundaries (UTC calendar periods).
pub mod periods {
    use time::{Date, OffsetDateTime};

    /// The daily period start for a timestamp (UTC calendar day).
    #[must_use]
    pub fn daily_start(ts: OffsetDateTime) -> Date {
        ts.date()
    }

    /// The monthly period start for a timestamp (1st of the month).
    #[must_use]
    pub fn monthly_start(ts: OffsetDateTime) -> Date {
        ts.date().replace_day(1).unwrap_or_else(|_| ts.date())
    }

    /// Next reset of the daily period (midnight UTC tomorrow).
    #[must_use]
    pub fn daily_next_reset(now: OffsetDateTime) -> OffsetDateTime {
        let next = now.date().next_day().unwrap_or(now.date());
        next.with_hms(0, 0, 0)
            .map_or(now, time::PrimitiveDateTime::assume_utc)
    }

    /// Next reset of the monthly period (1st of next month, midnight UTC).
    #[must_use]
    pub fn monthly_next_reset(now: OffsetDateTime) -> OffsetDateTime {
        let date = now.date();
        let (year, month) = (date.year(), date.month());
        let (ny, nm) = if month == time::Month::December {
            (year + 1, time::Month::January)
        } else {
            (year, month.next())
        };
        Date::from_calendar_date(ny, nm, 1)
            .ok()
            .and_then(|d| d.with_hms(0, 0, 0).ok())
            .map_or(now, time::PrimitiveDateTime::assume_utc)
    }
}
