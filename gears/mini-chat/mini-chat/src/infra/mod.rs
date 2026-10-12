//! Infrastructure layer: adapters to storage, plugins and platform services.
//!
//! - [`db`] — persistence (entities, migrations, repositories);
//! - [`plugins`] — the bundled static model-policy and audit plugins;
//! - [`policy_gateway`] — model-policy plugin resolution and delegation;
//! - [`audit_gateway`] — audit plugin resolution and delivery outcome;
//! - [`outbox`] — outbox queues, payloads and the transactional producer;
//! - [`outbox_handlers`] — the usage, audit and attachment cleanup queue
//!   handlers;
//! - [`outbox_pipeline`] — queue registration, handlers and the outbox
//!   workers' start and stop;
//! - [`llm`] — the `llm_provider` library (resolver, adapters);
//! - [`oagw_provisioning`] — OAGW upstream and route registration;
//! - [`summary_handler`] — the thread-summary queue handler;
//! - [`thumbnail`] — WebP preview thumbnails of image uploads.

pub mod audit_gateway;
pub mod chat_cleanup_handler;
pub mod db;
pub mod llm;
pub mod oagw_provisioning;
pub mod outbox;
pub mod outbox_handlers;
pub mod outbox_pipeline;
pub mod plugins;
pub mod policy_gateway;
pub mod summary_handler;
pub mod thumbnail;
pub mod workers;
