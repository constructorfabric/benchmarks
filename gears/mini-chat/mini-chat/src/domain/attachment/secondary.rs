//! Secondary copy of uploaded images in the Anthropic Files API (DESIGN "File storage (P1)",
//! ADR-0005 "Storage dispatch"): an image uploaded to a chat whose model is served by an
//! `anthropic_messages` provider is also uploaded to that provider's Files API, so the Anthropic
//! adapter can reference it. Documents and images larger than `thumbnail.max_decode_bytes` get no
//! copy. The copy is best effort: a failure is recorded (`secondary_status = failed`) and the
//! upload goes on.

use std::sync::Arc;

use bytes::Bytes;
use tokio::time::Instant;
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AttachmentService;
use crate::config::ProviderKind;
use crate::infra::db::entity::chats;
use crate::infra::db::repo;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx;
use crate::infra::storage::AnthropicFiles;

/// The file to copy.
pub(super) struct SecondaryCopy<'a> {
    pub scope: &'a AccessScope,
    pub tenant_id: Uuid,
    pub attachment_id: Uuid,
    /// Provider of the chat's model.
    pub provider_id: &'a str,
    pub filename: &'a str,
    pub content_type: &'a str,
    pub bytes: &'a Bytes,
    /// The upload request's deadline (shared with document indexing).
    pub deadline: Instant,
}

impl AttachmentService {
    /// Uploads the secondary copy of an image when the chat's provider is `anthropic_messages`
    /// and records the outcome on the `uploaded` row. A copy whose row was withdrawn meanwhile
    /// is deleted again (best effort).
    pub(super) async fn copy_secondary(&self, copy: SecondaryCopy<'_>) {
        let Some((files, alias)) = self.secondary_target(&copy) else {
            return;
        };
        let attachment_id = copy.attachment_id;
        let upload = files.upload(&alias, copy.filename, copy.content_type, copy.bytes.clone());
        let file_id = match tokio::time::timeout_at(copy.deadline, upload).await {
            Ok(Ok(id)) => Some(id),
            Ok(Err(err)) => {
                tracing::warn!(%attachment_id, error = %err, "secondary (Anthropic) upload failed");
                None
            }
            Err(_) => {
                // The provider may still store the file; nobody learns its id (best effort).
                tracing::warn!(%attachment_id, "secondary (Anthropic) upload timed out at the request deadline");
                None
            }
        };
        let recorded = self
            .record_secondary(copy.scope, attachment_id, file_id.clone())
            .await;
        if let (false, Some(file_id)) = (recorded, file_id) {
            // Detached on purpose: the request is answered without waiting, and the copy is
            // known to nobody else (the row is gone), so a failed delete is only logged.
            tokio::spawn(async move {
                if let Err(err) = files.delete(&alias, &file_id).await {
                    tracing::warn!(%attachment_id, error = %err, "best-effort secondary file delete failed");
                }
            });
        }
    }

    /// The files client and upstream alias of the copy, or `None` when the image gets none.
    fn secondary_target(&self, copy: &SecondaryCopy<'_>) -> Option<(Arc<AnthropicFiles>, String)> {
        let files = self.deps.anthropic_files.as_ref()?;
        let alias = self.anthropic_alias(copy.provider_id, copy.tenant_id)?;
        if copy.bytes.len() > self.deps.cfg.thumbnail.max_decode_bytes {
            tracing::debug!(attachment_id = %copy.attachment_id,
                "image above thumbnail.max_decode_bytes: no secondary copy");
            return None;
        }
        Some((Arc::clone(files), alias))
    }

    /// The upstream alias of provider `provider_id` for `tenant_id` when it is an
    /// `anthropic_messages` provider: where the copies of its chats' images are stored.
    pub(super) fn anthropic_alias(&self, provider_id: &str, tenant_id: Uuid) -> Option<String> {
        self.deps
            .providers
            .chat_target(provider_id, tenant_id)
            .ok()
            .filter(|t| t.kind == ProviderKind::AnthropicMessages)
            .map(|t| t.alias)
    }

    /// The alias of the copy of an image of `chat` at delete time: the upload's rule (the
    /// provider of the chat's model); when the model can no longer be resolved, the alias shared
    /// by every `anthropic_messages` entry (if they share one).
    pub(super) async fn secondary_alias_for_delete(
        &self,
        ctx: &SecurityContext,
        chat: &chats::Model,
    ) -> Option<String> {
        let tenant_id = chat.tenant_id;
        match self.deps.models.resolve_chat_model(ctx, &chat.model).await {
            Ok((_, model)) => self.anthropic_alias(&model.provider_id, tenant_id),
            Err(err) => {
                tracing::warn!(chat_id = %chat.id, error = %err,
                    "chat model not resolvable: using the shared Anthropic files alias");
                None
            }
        }
        .or_else(|| self.deps.providers.anthropic_files_alias(tenant_id))
    }

    /// Records the copy's outcome; `false` when the row was withdrawn (or the write failed).
    async fn record_secondary(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        file_id: Option<String>,
    ) -> bool {
        let scope = scope.clone();
        let recorded = write_tx(&self.deps.db, move |tx| {
            let (scope, id) = (scope.clone(), file_id.clone());
            Box::pin(async move {
                repo::attachments::set_secondary(tx, &scope, attachment_id, id.as_deref(), db_now())
                    .await
            })
        })
        .await;
        recorded.unwrap_or_else(|err| {
            tracing::warn!(%attachment_id, error = %err, "secondary copy not recorded");
            false
        })
    }
}

#[cfg(test)]
mod tests {
    use http::Method;
    use serde_json::json;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::config::MiniChatConfig;
    use crate::test_support::app::{
        ANTHROPIC_ALIAS, TestApp, anthropic_config, anthropic_provider, ctx,
    };
    use crate::test_support::attachments::{
        CLEANUP_QUEUE, PDF, VS_ID, attachment_row, attachment_uri, fast_timings, id_of, png,
        script_files, script_vector_store, upload,
    };
    use crate::test_support::catalog::{anthropic_model, test_catalog};
    use crate::test_support::gateway::Responder;
    use crate::test_support::stream::{
        ANTHROPIC_MESSAGES_PATH, anthropic_answer, create_chat, stream_uri,
    };

    fn anthropic_files() -> String {
        format!("/{ANTHROPIC_ALIAS}/v1/files")
    }

    fn user() -> SecurityContext {
        ctx(Uuid::new_v4(), Uuid::new_v4())
    }

    async fn anthropic_app(cfg: MiniChatConfig) -> TestApp {
        let mut catalog = test_catalog();
        catalog.push(anthropic_model("claude"));
        TestApp::builder()
            .config(cfg)
            .catalog(catalog)
            .build()
            .await
    }

    fn secondary_upload_answers(app: &TestApp, responder: Responder) {
        app.gateway.on(Method::POST, &anthropic_files(), responder);
    }

    #[tokio::test]
    async fn anthropic_secondary_copy_on_image_upload() {
        let app = anthropic_app(anthropic_config()).await;
        let who = user();
        let chat = create_chat(&app, &who, Some("claude")).await;
        script_files(&app, &["file-img1"]);
        secondary_upload_answers(
            &app,
            Responder::json(200, json!({"id": "file_011sec", "type": "file"})),
        );

        let res = upload(&app, &who, chat, "photo.png", "image/png", &png(8, 8)).await;

        assert_eq!(res.status, 201, "{}", res.json);
        assert_eq!(res.json["status"], "ready");
        assert!(
            !res.json.to_string().contains("file_011sec"),
            "{}",
            res.json
        );
        let att = id_of(&res);
        let row = attachment_row(&app, chat, att).await;
        assert_eq!(row.provider_file_id.as_deref(), Some("file-img1"));
        assert_eq!(row.secondary_file_id.as_deref(), Some("file_011sec"));
        assert_eq!(row.secondary_status, "uploaded");
        assert_eq!(row.secondary_provider_kind.as_deref(), Some("anthropic"));

        let calls = app.gateway.requests_to(&Method::POST, &anthropic_files());
        assert_eq!(calls.len(), 1);
        let header = |name: &str| calls[0].headers.get(name).map(|v| v.to_str().unwrap());
        assert_eq!(header("anthropic-beta"), Some("files-api-2025-04-14"));
        assert_eq!(header("anthropic-version"), Some("2023-06-01"));
        let body = String::from_utf8_lossy(&calls[0].body);
        assert!(body.contains("name=\"file\""), "{body}");
        assert!(body.contains(&format!("{chat}_{att}.png")), "{body}");
        assert!(!body.contains("purpose"), "{body}");
        // the primary copy went to the RAG provider (`rag_provider = openai`)
        assert_eq!(
            app.gateway
                .requests_to(&Method::POST, "/127.0.0.1/v1/files")
                .len(),
            1
        );

        // a message with the image references the Anthropic copy
        app.gateway.on(
            Method::POST,
            ANTHROPIC_MESSAGES_PATH,
            anthropic_answer(&["a cat"], 3),
        );
        let frames = app
            .stream(
                "POST",
                &stream_uri(chat),
                &who,
                json!({"content": "what is it", "attachment_ids": [att]}),
            )
            .await
            .expect("stream");
        assert_eq!(frames.last().unwrap().event, "done", "{frames:?}");
        let sent = app
            .gateway
            .requests_to(&Method::POST, ANTHROPIC_MESSAGES_PATH);
        let content = &sent[0].json.as_ref().unwrap()["messages"][0]["content"];
        assert_eq!(
            content[1],
            json!({"type": "image", "source": {"type": "file", "file_id": "file_011sec"}}),
            "{content}"
        );
    }

    #[tokio::test]
    async fn anthropic_secondary_copy_skips_documents_other_chats_and_large_images() {
        let mut cfg = anthropic_config();
        cfg.thumbnail.max_decode_bytes = 64;
        let app = anthropic_app(cfg).await;
        let who = user();
        let claude = create_chat(&app, &who, Some("claude")).await;
        let openai = create_chat(&app, &who, None).await;
        secondary_upload_answers(&app, Responder::json(200, json!({"id": "file_011sec"})));

        // a document in an Anthropic chat
        script_files(&app, &["file-doc1", "file-img2", "file-img3"]);
        script_vector_store(
            &app,
            VS_ID,
            std::time::Duration::ZERO,
            json!({"id": "file-doc1", "status": "completed"}),
        );
        let doc = upload(&app, &who, claude, "a.pdf", PDF, b"%PDF-1.4").await;
        assert_eq!(doc.status, 201, "{}", doc.json);
        // an image larger than `thumbnail.max_decode_bytes`
        let big = png(32, 32);
        assert!(big.len() > 64);
        let large = upload(&app, &who, claude, "big.png", "image/png", &big).await;
        assert_eq!(large.status, 201, "{}", large.json);
        // an image in a chat served by `openai_responses`
        let other = upload(&app, &who, openai, "p.png", "image/png", &png(2, 2)).await;
        assert_eq!(other.status, 201, "{}", other.json);

        assert!(
            app.gateway
                .requests_to(&Method::POST, &anthropic_files())
                .is_empty()
        );
        for (chat, res) in [(claude, &doc), (claude, &large), (openai, &other)] {
            let row = attachment_row(&app, chat, id_of(res)).await;
            assert_eq!(row.secondary_status, "not_attempted");
            assert!(row.secondary_file_id.is_none() && row.secondary_provider_kind.is_none());
        }
    }

    #[tokio::test]
    async fn anthropic_secondary_copy_failure_keeps_the_image_ready() {
        let app = anthropic_app(anthropic_config()).await;
        let who = user();
        let chat = create_chat(&app, &who, Some("claude")).await;
        script_files(&app, &["file-img1"]);
        secondary_upload_answers(
            &app,
            Responder::json(500, json!({"error": {"message": "boom"}})),
        );

        let res = upload(&app, &who, chat, "p.png", "image/png", &png(2, 2)).await;

        assert_eq!(res.status, 201, "{}", res.json);
        assert_eq!(res.json["status"], "ready");
        let row = attachment_row(&app, chat, id_of(&res)).await;
        assert_eq!(row.secondary_status, "failed");
        assert!(row.secondary_file_id.is_none());
        assert_eq!(row.secondary_provider_kind.as_deref(), Some("anthropic"));
    }

    #[tokio::test]
    async fn anthropic_secondary_copy_is_named_in_the_cleanup_event() {
        let app = anthropic_app(anthropic_config()).await;
        let who = user();
        let chat = create_chat(&app, &who, Some("claude")).await;
        script_files(&app, &["file-img1"]);
        secondary_upload_answers(&app, Responder::json(200, json!({"id": "file_011sec"})));
        let att = id_of(&upload(&app, &who, chat, "p.png", "image/png", &png(2, 2)).await);

        let res = app
            .call("DELETE", &attachment_uri(chat, att), &who, None)
            .await;
        assert_eq!(res.status, 204, "{}", res.json);

        TestApp::wait_until("cleanup event", || async {
            !app.outbox_payloads(CLEANUP_QUEUE).is_empty()
        })
        .await;
        let payload = &app.outbox_payloads(CLEANUP_QUEUE)[0];
        assert_eq!(
            payload["secondary_ref"],
            json!({"file_id": "file_011sec", "provider_kind": "anthropic",
                   "upstream_alias": ANTHROPIC_ALIAS})
        );
    }

    /// With two Anthropic entries on different aliases the delete names the alias the copy was
    /// uploaded to (the provider of the chat's model), not a shared one.
    #[tokio::test]
    async fn anthropic_secondary_copy_delete_uses_the_upload_alias() {
        let mut cfg = anthropic_config();
        let mut second = anthropic_provider();
        second.host = "anthropic2.test".to_owned();
        cfg.providers.insert("anthropic2".to_owned(), second);
        let mut catalog = test_catalog();
        let mut claude2 = anthropic_model("claude2");
        claude2.provider_id = "anthropic2".to_owned();
        catalog.push(claude2);
        let app = TestApp::builder()
            .config(cfg)
            .catalog(catalog)
            .build()
            .await;
        let who = user();
        let chat = create_chat(&app, &who, Some("claude2")).await;
        script_files(&app, &["file-img1"]);
        app.gateway.on(
            Method::POST,
            "/anthropic2.test/v1/files",
            Responder::json(200, json!({"id": "file_022sec"})),
        );
        let att = id_of(&upload(&app, &who, chat, "p.png", "image/png", &png(2, 2)).await);
        assert_eq!(
            attachment_row(&app, chat, att)
                .await
                .secondary_file_id
                .as_deref(),
            Some("file_022sec")
        );

        let res = app
            .call("DELETE", &attachment_uri(chat, att), &who, None)
            .await;
        assert_eq!(res.status, 204, "{}", res.json);

        TestApp::wait_until("cleanup event", || async {
            !app.outbox_payloads(CLEANUP_QUEUE).is_empty()
        })
        .await;
        assert_eq!(
            app.outbox_payloads(CLEANUP_QUEUE)[0]["secondary_ref"],
            json!({"file_id": "file_022sec", "provider_kind": "anthropic",
                   "upstream_alias": "anthropic2.test"})
        );
    }

    #[tokio::test]
    async fn anthropic_secondary_copy_is_bounded_by_the_request_deadline() {
        let mut catalog = test_catalog();
        catalog.push(anthropic_model("claude"));
        // request deadline 200 ms
        let app = TestApp::builder()
            .config(anthropic_config())
            .catalog(catalog)
            .indexing_timings(fast_timings(std::time::Duration::from_secs(1)))
            .build()
            .await;
        let who = user();
        let chat = create_chat(&app, &who, Some("claude")).await;
        script_files(&app, &["file-img1"]);
        secondary_upload_answers(
            &app,
            Responder::Delayed(
                std::time::Duration::from_secs(30),
                Box::new(Responder::json(200, json!({"id": "file_011sec"}))),
            ),
        );

        let started = std::time::Instant::now();
        let res = upload(&app, &who, chat, "p.png", "image/png", &png(2, 2)).await;

        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(res.status, 201, "{}", res.json);
        assert_eq!(res.json["status"], "ready");
        let row = attachment_row(&app, chat, id_of(&res)).await;
        assert_eq!(row.secondary_status, "failed");
        assert!(row.secondary_file_id.is_none());
    }
}
