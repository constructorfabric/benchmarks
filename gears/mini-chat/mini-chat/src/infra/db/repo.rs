//! Repositories: implementations of the domain persistence ports for every
//! secure database runner (`DbConn` and `DbTx`). Every query goes through the
//! Secure ORM (`.secure().scope_with(scope)`); the scope is chosen by the
//! domain service (owner-narrowed chat scope for `chats`, tenant scope for the
//! chat children after the owner-scoped chat lookup succeeded).

mod attachments;
mod chats;
mod messages;
mod odata;
mod quota;
mod reactions;
mod thread_summaries;
mod turns;
mod vector_stores;

use toolkit_db::odata::LimitCfg;

use crate::domain::chats::{PAGE_DEFAULT_LIMIT, PAGE_MAX_LIMIT};

/// Page size rules of the list endpoints: default 20, clamped to 100.
const fn limit_cfg() -> LimitCfg {
    LimitCfg {
        default: PAGE_DEFAULT_LIMIT,
        max: PAGE_MAX_LIMIT,
    }
}
