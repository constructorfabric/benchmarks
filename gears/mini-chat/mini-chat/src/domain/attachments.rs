//! Attachments and file search (FEATURE 0005), slice S1-upload-lifecycle: the upload and get
//! contracts (intake, purpose routing, per-chat limits, provider file upload, vector store
//! provisioning, indexing wait, background indexing, thumbnail, secondary copy, upload failure
//! and completion, detail projection); slice S2-deletion-cleanup-tools: the delete transaction,
//! the cleanup handler, the upload reaper, the fact algorithms, the tool contributions and the
//! knowledge search loop.

pub mod background;
pub mod cleanup;
pub mod completion;
pub mod contributions;
pub mod facts;
pub mod failure;
pub mod indexing;
pub mod intake;
pub mod knowledge;
pub mod limits;
pub mod metrics;
pub mod model;
pub mod provider;
pub mod reaper;
pub mod service;
pub mod store;
pub mod thumbnail;
pub mod timing;
pub mod upload_body;
pub mod vector_store;

pub use metrics::AttachmentMetrics;
pub use model::{
    AttachmentKind, AttachmentRecord, AttachmentStatus, SecondaryStatus, StoredThumbnail, Thumbnail,
};
pub use service::{AttachmentSettings, AttachmentsDeps, AttachmentsService, UploadInput};
