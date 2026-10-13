//! Persistence error mapping.
//!
//! [`unique_violation_target`] names the unique constraint or index a
//! database error violated. It first looks for the driver error in the error
//! chain: the portable unique-violation signal (`DbErr::sql_err()`, SQLSTATE
//! `23505` on PostgreSQL, extended codes `2067` / `1555` on `SQLite`) decides
//! whether the error is a unique violation, and the constraint name the
//! driver reports (PostgreSQL) identifies the target. `SQLite` reports no
//! constraint name, so its message is parsed (the column list after
//! `UNIQUE constraint failed:`). Errors that carry only text (custom or wrapped
//! errors) fall back to the same message parsing, which also understands the
//! PostgreSQL message (`violates unique constraint "<name>"`). The domain
//! services use it to tell `turn_already_running`, `request_id_conflict` and
//! `GENERATION_IN_PROGRESS` apart.
//!
//! `From<DbError> for DomainError` and `From<ScopeError> for DomainError` turn
//! a unique violation into `already_exists` (`unique_violation`) on the
//! resource of the violated constraint and every other failure into
//! `internal`. Neither carries driver text: driver messages embed the
//! offending values, so the diagnostic is a fixed classification only.

use std::error::Error as StdError;

use sea_orm::{DbErr, RuntimeErr, SqlErr};
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

use crate::domain::error::{DomainError, ResourceKind};

/// A table of the gear's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbTable {
    Chats,
    Messages,
    ChatTurns,
    Attachments,
    MessageAttachments,
    ThreadSummaries,
    ChatVectorStores,
    QuotaUsage,
    MessageReactions,
}

impl DbTable {
    /// Every table, in migration order.
    pub const ALL: [Self; 9] = [
        Self::Chats,
        Self::Messages,
        Self::ChatTurns,
        Self::Attachments,
        Self::MessageAttachments,
        Self::ThreadSummaries,
        Self::ChatVectorStores,
        Self::QuotaUsage,
        Self::MessageReactions,
    ];

    /// SQL table name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chats => "chats",
            Self::Messages => "messages",
            Self::ChatTurns => "chat_turns",
            Self::Attachments => "attachments",
            Self::MessageAttachments => "message_attachments",
            Self::ThreadSummaries => "thread_summaries",
            Self::ChatVectorStores => "chat_vector_stores",
            Self::QuotaUsage => "quota_usage",
            Self::MessageReactions => "message_reactions",
        }
    }

    /// The table with SQL name `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|table| table.as_str() == name)
    }

    /// Resource a conflict on this table is reported on.
    #[must_use]
    pub const fn resource_kind(self) -> ResourceKind {
        match self {
            Self::Chats | Self::ThreadSummaries => ResourceKind::Chat,
            Self::Messages
            | Self::MessageAttachments
            | Self::QuotaUsage
            | Self::MessageReactions => ResourceKind::Message,
            Self::ChatTurns => ResourceKind::Turn,
            Self::Attachments | Self::ChatVectorStores => ResourceKind::Attachment,
        }
    }
}

/// The unique constraint or index a write violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniqueTarget {
    /// `uq_chat_turns_running`: `UNIQUE(chat_id) WHERE state = 'running' AND
    /// deleted_at IS NULL` (another turn is running in the chat).
    RunningTurn,
    /// `uq_chat_turns_chat_request`: `UNIQUE(chat_id, request_id)`.
    TurnRequestId,
    /// `uq_messages_chat_request_role`: `UNIQUE(chat_id, request_id, role)
    /// WHERE request_id IS NOT NULL AND deleted_at IS NULL`.
    MessageRequestRole,
    /// `uq_thread_summaries_chat`: one summary per chat.
    ThreadSummaryChat,
    /// `uq_chat_vector_stores_tenant_chat`: one vector store per chat.
    ChatVectorStore,
    /// `uq_quota_usage_bucket`: one row per
    /// `(tenant_id, user_id, period_type, period_start, bucket)`.
    QuotaUsageBucket,
    /// `uq_message_reactions_message_user`: one reaction per user and message.
    MessageReaction,
    /// `pk_message_attachments`: the message already links the attachment.
    MessageAttachment,
    /// Primary key (or the `(id, chat_id)` key backing the composite foreign
    /// keys) of a table.
    PrimaryKey(DbTable),
    /// A unique violation whose constraint could not be identified.
    Unknown,
}

/// Named unique constraints and indexes, as declared by the migrations.
const NAMED_TARGETS: [(&str, UniqueTarget); 10] = [
    ("uq_chat_turns_running", UniqueTarget::RunningTurn),
    ("uq_chat_turns_chat_request", UniqueTarget::TurnRequestId),
    (
        "uq_messages_chat_request_role",
        UniqueTarget::MessageRequestRole,
    ),
    ("uq_thread_summaries_chat", UniqueTarget::ThreadSummaryChat),
    (
        "uq_chat_vector_stores_tenant_chat",
        UniqueTarget::ChatVectorStore,
    ),
    ("uq_quota_usage_bucket", UniqueTarget::QuotaUsageBucket),
    (
        "uq_message_reactions_message_user",
        UniqueTarget::MessageReaction,
    ),
    ("pk_message_attachments", UniqueTarget::MessageAttachment),
    (
        "uq_messages_id_chat",
        UniqueTarget::PrimaryKey(DbTable::Messages),
    ),
    (
        "uq_attachments_id_chat",
        UniqueTarget::PrimaryKey(DbTable::Attachments),
    ),
];

impl UniqueTarget {
    /// Name of the constraint or index (`None` for [`Self::PrimaryKey`] of a
    /// table with an unnamed primary key and for [`Self::Unknown`]).
    #[must_use]
    pub fn constraint_name(self) -> Option<&'static str> {
        NAMED_TARGETS
            .iter()
            .find(|(_, target)| *target == self)
            .map(|(name, _)| *name)
    }

    /// Table the constraint belongs to (`None` for [`Self::Unknown`]).
    #[must_use]
    pub const fn table(self) -> Option<DbTable> {
        match self {
            Self::RunningTurn | Self::TurnRequestId => Some(DbTable::ChatTurns),
            Self::MessageRequestRole => Some(DbTable::Messages),
            Self::ThreadSummaryChat => Some(DbTable::ThreadSummaries),
            Self::ChatVectorStore => Some(DbTable::ChatVectorStores),
            Self::QuotaUsageBucket => Some(DbTable::QuotaUsage),
            Self::MessageReaction => Some(DbTable::MessageReactions),
            Self::MessageAttachment => Some(DbTable::MessageAttachments),
            Self::PrimaryKey(table) => Some(table),
            Self::Unknown => None,
        }
    }

    /// Resource the conflict is reported on (`context.resource_type`).
    #[must_use]
    pub const fn resource_kind(self) -> ResourceKind {
        match self.table() {
            Some(table) => table.resource_kind(),
            None => ResourceKind::Chat,
        }
    }

    /// Target of a PostgreSQL constraint name (`<table>_pkey` for primary keys).
    fn from_constraint_name(name: &str) -> Self {
        if let Some((_, target)) = NAMED_TARGETS.iter().find(|(known, _)| *known == name) {
            return *target;
        }
        name.strip_suffix("_pkey")
            .and_then(DbTable::from_name)
            .map_or(Self::Unknown, Self::PrimaryKey)
    }

    /// Target of a `SQLite` column list (`table.col, table.col`).
    fn from_sqlite_columns(table: &str, columns: &[&str]) -> Self {
        let Some(table) = DbTable::from_name(table) else {
            return Self::Unknown;
        };
        match (table, columns) {
            (DbTable::ChatTurns, ["chat_id"]) => Self::RunningTurn,
            (DbTable::ChatTurns, ["chat_id", "request_id"]) => Self::TurnRequestId,
            (DbTable::Messages, ["chat_id", "request_id", "role"]) => Self::MessageRequestRole,
            (DbTable::ThreadSummaries, ["chat_id"]) => Self::ThreadSummaryChat,
            (DbTable::ChatVectorStores, ["tenant_id", "chat_id"]) => Self::ChatVectorStore,
            (
                DbTable::QuotaUsage,
                [
                    "tenant_id",
                    "user_id",
                    "period_type",
                    "period_start",
                    "bucket",
                ],
            ) => Self::QuotaUsageBucket,
            (DbTable::MessageReactions, ["message_id", "user_id"]) => Self::MessageReaction,
            (DbTable::MessageAttachments, ["chat_id", "message_id", "attachment_id"]) => {
                Self::MessageAttachment
            }
            (_, ["id"] | ["id", "chat_id"]) => Self::PrimaryKey(table),
            _ => Self::Unknown,
        }
    }
}

const PG_UNIQUE_MARKER: &str = "violates unique constraint \"";
const SQLITE_UNIQUE_MARKER: &str = "unique constraint failed: ";

/// SQLSTATE of a PostgreSQL unique violation.
const PG_UNIQUE_SQLSTATE: &str = "23505";
/// `SQLite` extended result codes of a unique (`2067`) or primary key
/// (`1555`) constraint violation.
const SQLITE_UNIQUE_CODES: [&str; 2] = ["2067", "1555"];

/// The unique constraint `err` violated, or `None` when `err` is not a
/// unique violation.
///
/// Works on [`DbError`], `sea_orm::DbErr`, `toolkit_db::secure::ScopeError`,
/// `sqlx` errors and any error whose `source()` chain contains one of them;
/// other errors are classified from their message chain. Recognizes
/// PostgreSQL constraint and index names and `SQLite` column lists; an
/// unrecognized unique violation yields [`UniqueTarget::Unknown`].
#[must_use]
pub fn unique_violation_target<E>(err: &E) -> Option<UniqueTarget>
where
    E: StdError + 'static,
{
    if let Some(driver) = driver_classification(err) {
        return driver;
    }
    text_classification(&error_chain_text(err))
}

/// Outcome of the structured inspection: `Some(None)` when a driver error
/// says "not a unique violation", `None` when no structured error was found.
type DriverOutcome = Option<Option<UniqueTarget>>;

/// Classifies the first database error found in the chain of `err` from its
/// structured fields.
fn driver_classification(err: &(dyn StdError + 'static)) -> DriverOutcome {
    let mut current: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(candidate) = current {
        if let Some(outcome) = classify_known(candidate) {
            return Some(outcome);
        }
        current = candidate.source();
    }
    None
}

/// Structured classification of one error of a known type.
fn classify_known(err: &(dyn StdError + 'static)) -> DriverOutcome {
    if let Some(db_error) = err.downcast_ref::<DbError>() {
        return match db_error {
            DbError::Sea(db_err) => Some(classify_db_err(db_err)),
            DbError::Sqlx(sqlx_err) => classify_sqlx(sqlx_err),
            DbError::Other(other) => driver_classification(other.as_ref()),
            _ => None,
        };
    }
    if let Some(scope_error) = err.downcast_ref::<ScopeError>() {
        return match scope_error {
            ScopeError::Db(db_err) => Some(classify_db_err(db_err)),
            _ => Some(None),
        };
    }
    if let Some(db_err) = err.downcast_ref::<DbErr>() {
        return Some(classify_db_err(db_err));
    }
    if let Some(sqlx_err) = err.downcast_ref::<sea_orm::sqlx::Error>() {
        return classify_sqlx(sqlx_err);
    }
    None
}

/// Classifies a `SeaORM` error: the driver error when present, else the
/// `sql_err()` signal plus message parsing, else the message alone.
fn classify_db_err(err: &DbErr) -> Option<UniqueTarget> {
    if let DbErr::Exec(RuntimeErr::SqlxError(sqlx_err))
    | DbErr::Query(RuntimeErr::SqlxError(sqlx_err))
    | DbErr::Conn(RuntimeErr::SqlxError(sqlx_err)) = err
        && let Some(outcome) = classify_sqlx(sqlx_err)
    {
        return outcome;
    }
    match err.sql_err() {
        Some(SqlErr::UniqueConstraintViolation(message)) => Some(
            text_classification(&message)
                .or_else(|| text_classification(&err.to_string()))
                .unwrap_or(UniqueTarget::Unknown),
        ),
        Some(SqlErr::ForeignKeyConstraintViolation(_)) => None,
        _ => text_classification(&error_chain_text(err)),
    }
}

/// Classifies a `sqlx` database error from its code and constraint name;
/// `None` when it is not a database error (no structured data).
fn classify_sqlx(err: &sea_orm::sqlx::Error) -> DriverOutcome {
    let sea_orm::sqlx::Error::Database(db) = err else {
        return None;
    };
    let code = db.code();
    let unique = db.is_unique_violation()
        || code
            .as_deref()
            .is_some_and(|code| code == PG_UNIQUE_SQLSTATE || SQLITE_UNIQUE_CODES.contains(&code));
    if !unique {
        return Some(None);
    }
    if let Some(name) = db.constraint() {
        return Some(Some(UniqueTarget::from_constraint_name(
            &name.to_lowercase(),
        )));
    }
    Some(Some(
        text_classification(db.message()).unwrap_or(UniqueTarget::Unknown),
    ))
}

/// Message-based classification of an error text (case-insensitive).
fn text_classification(text: &str) -> Option<UniqueTarget> {
    let text = text.to_lowercase();

    if let Some(start) = text.find(PG_UNIQUE_MARKER) {
        let rest = &text[start + PG_UNIQUE_MARKER.len()..];
        let name = rest.split('"').next().unwrap_or_default();
        return Some(UniqueTarget::from_constraint_name(name));
    }

    if let Some(start) = text.find(SQLITE_UNIQUE_MARKER) {
        let rest = &text[start + SQLITE_UNIQUE_MARKER.len()..];
        return Some(parse_sqlite_columns(rest));
    }

    if text.contains("duplicate key value") || text.contains("unique constraint") {
        return Some(UniqueTarget::Unknown);
    }
    None
}

/// Parses `<table>.<col>, <table>.<col>...` (or `index '<name>'`) at the
/// start of `rest`.
fn parse_sqlite_columns(rest: &str) -> UniqueTarget {
    let line = rest.lines().next().unwrap_or_default();

    if let Some(index) = line.strip_prefix("index '") {
        let name = index.split('\'').next().unwrap_or_default();
        return UniqueTarget::from_constraint_name(name);
    }

    let mut table_name: Option<&str> = None;
    let mut columns: Vec<&str> = Vec::new();
    for item in line.split(',') {
        let qualified = item
            .trim()
            .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'))
            .next()
            .unwrap_or_default();
        let Some((table, column)) = qualified.split_once('.') else {
            break;
        };
        match table_name {
            None => table_name = Some(table),
            Some(known) if known != table => return UniqueTarget::Unknown,
            Some(_) => {}
        }
        columns.push(column);
    }

    table_name.map_or(UniqueTarget::Unknown, |table| {
        UniqueTarget::from_sqlite_columns(table, &columns)
    })
}

/// `err` and its `source()` chain, one message per line.
fn error_chain_text<E>(err: &E) -> String
where
    E: StdError + ?Sized,
{
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push('\n');
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Fixed, driver-free classification of a database failure.
fn failure_class(err: &DbError) -> &'static str {
    match err {
        DbError::Sea(db_err) if toolkit_db::secure::is_foreign_key_violation(db_err) => {
            "foreign key violation"
        }
        DbError::Sea(_) | DbError::Sqlx(_) => "query failed",
        DbError::Lock(_) => "lock failed",
        DbError::ConnRequestedInsideTx => "connection requested inside a transaction",
        DbError::Io(_) => "i/o failure",
        DbError::Other(_) => "database failure",
        DbError::UnknownDsn(_)
        | DbError::FeatureDisabled(_)
        | DbError::InvalidConfig(_)
        | DbError::ConfigConflict(_)
        | DbError::InvalidSqlitePragma { .. }
        | DbError::UnknownSqlitePragma(_)
        | DbError::InvalidParameter(_)
        | DbError::SqlitePragma(_)
        | DbError::EnvVar { .. }
        | DbError::UrlParse(_) => "database configuration",
    }
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match unique_violation_target(&err) {
            Some(target) => Self::unique_violation(target.resource_kind()),
            None => Self::internal(format!("database error: {}", failure_class(&err))),
        }
    }
}

/// Fixed, driver-free classification of a Secure ORM failure.
fn scope_failure_class(err: &ScopeError) -> &'static str {
    match err {
        ScopeError::Db(db_err) if toolkit_db::secure::is_foreign_key_violation(db_err) => {
            "foreign key violation"
        }
        ScopeError::Db(_) => "query failed",
        ScopeError::TenantNotInScope { .. } | ScopeError::Denied(_) => "write outside the scope",
        _ => "invalid scope",
    }
}

impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        match unique_violation_target(&err) {
            Some(target) => Self::unique_violation(target.resource_kind()),
            None => Self::internal(format!("database error: {}", scope_failure_class(&err))),
        }
    }
}
