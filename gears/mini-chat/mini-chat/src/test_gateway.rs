//! A stub `ServiceGatewayClientV1` for tests: serves canned `Responses` API
//! SSE streams and an in-memory Files/Vector Stores API.

use async_trait::async_trait;
use futures::StreamExt;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub use crate::test_support::StubGateway;

#[async_trait]
impl ServiceGatewayClientV1 for StubGateway {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        req: oagw_sdk::CreateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Upstream>, CanonicalError> {
        Ok(Vec::new())
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: oagw_sdk::UpdateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        Ok(())
    }

    async fn create_route(
        &self,
        _ctx: SecurityContext,
        _req: oagw_sdk::CreateRouteRequest,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn get_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Route>, CanonicalError> {
        Ok(Vec::new())
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: oagw_sdk::UpdateRouteRequest,
    ) -> Result<oagw_sdk::Route, CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Ok(())
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(oagw_sdk::Upstream, oagw_sdk::Route), CanonicalError> {
        Err(CanonicalError::internal("unused in tests").create())
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let path = req.uri().path().to_owned();
        let method = req.method().clone();
        if self.fail_429() && path.ends_with("/responses") && method == http::Method::POST {
            return Ok(http::Response::builder()
                .status(429)
                .header("retry-after", "7")
                .header("content-type", "application/json")
                .body(Body::from(
                    br#"{"error": {"message": "slow down"}}"#.to_vec(),
                ))
                .expect("response"));
        }
        if path.ends_with("/responses") && method == http::Method::POST {
            // Non-streaming: full JSON body.
            let is_stream = req.headers().get("content-type").is_some();
            drop(is_stream);
            let body = req.into_body().into_bytes().await.map_err(|e| {
                CanonicalError::internal(format!("read body: {e}")).create()
            })?;
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let streaming = value.get("stream").and_then(serde_json::Value::as_bool).unwrap_or(false);
            self.requests.lock().expect("lock").push(value);
            if !streaming {
                let text = self
                    .complete_response
                    .lock()
                    .expect("lock")
                    .clone()
                    .unwrap_or_else(|| "summary text".to_owned());
                let body = serde_json::json!({
                    "id": format!("resp-{}", Uuid::new_v4().as_simple()),
                    "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
                    "usage": {"input_tokens": 20, "output_tokens": 7},
                });
                return Ok(http::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .expect("response"));
            }
            let script = self
                .stream_script
                .lock()
                .expect("lock")
                .take()
                .unwrap_or_else(Self::default_stream);
            let hang = std::mem::take(&mut *self.hang.lock().expect("lock"));
            let mut payload = String::new();
            for (name, data) in script.events {
                payload.push_str(&format!("event: {name}\ndata: {data}\n\n"));
            }
            let stream: oagw_sdk::body::BodyStream = if hang {
                Box::pin(futures::stream::once(async move {
                    Ok(bytes::Bytes::from(payload))
                })
                .chain(futures::stream::pending()))
            } else {
                Box::pin(futures::stream::once(async move {
                    Ok(bytes::Bytes::from(payload))
                }))
            };
            return Ok(http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(stream))
                .expect("response"));
        }
        // Files API upload (multipart): record and return an id.
        if path.ends_with("/files") && !path.contains("/vector_stores/") && method == http::Method::POST {
            let bytes = req.into_body().into_bytes().await.map_err(|e| {
                CanonicalError::internal(format!("read body: {e}")).create()
            })?;
            let id = format!("file-{}", Uuid::new_v4().as_simple());
            self.files
                .lock()
                .expect("lock")
                .insert(id.clone(), ("upload".to_owned(), bytes));
            let body = serde_json::json!({"id": id, "purpose": "assistants"});
            return Ok(http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("response"));
        }
        if path.ends_with("/vector_stores") && method == http::Method::POST {
            let id = format!("vs-{}", Uuid::new_v4().as_simple());
            self.vector_stores
                .lock()
                .expect("lock")
                .insert(id.clone(), Vec::new());
            let body = serde_json::json!({"id": id});
            return Ok(http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("response"));
        }
        // Add file to vector store: /vs/{id}/files
        if path.contains("/vector_stores/") && path.ends_with("/files") && method == http::Method::POST
        {
            let body = req.into_body().into_bytes().await.map_err(|e| {
                CanonicalError::internal(format!("read body: {e}")).create()
            })?;
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let vs = path
                .split("/vector_stores/")
                .nth(1)
                .and_then(|rest| rest.strip_suffix("/files"))
                .unwrap_or_default()
                .to_owned();
            let file_id = value
                .get("file_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            self.vector_stores
                .lock()
                .expect("lock")
                .entry(vs.clone())
                .or_default()
                .push(file_id.clone());
            let status = self
                .forced_vs_status
                .lock()
                .expect("lock")
                .clone()
                .unwrap_or_else(|| "completed".to_owned());
            self.vs_file_status
                .lock()
                .expect("lock")
                .insert((vs, file_id.clone()), status.clone());
            let body = serde_json::json!({"id": format!("vsf-{}", Uuid::new_v4().as_simple()), "status": status});
            return Ok(http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("response"));
        }
        // Vector store file status.
        if path.contains("/vector_stores/") && path.contains("/files/") && method == http::Method::GET
        {
            let parts: Vec<&str> = path.split('/').collect();
            let (vs, file) = (
                parts[parts.len() - 3].to_owned(),
                parts[parts.len() - 1].to_owned(),
            );
            let status = self
                .vs_file_status
                .lock()
                .expect("lock")
                .get(&(vs, file))
                .cloned()
                .unwrap_or_else(|| "completed".to_owned());
            let body = serde_json::json!({"status": status});
            return Ok(http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("response"));
        }
        // Deletes succeed; record them.
        if method == http::Method::DELETE {
            if path.contains("/vector_stores/") {
                let id = path.rsplit('/').next().unwrap_or_default().to_owned();
                self.deleted_vector_stores.lock().expect("lock").push(id);
            } else if path.contains("/files/") {
                let id = path.rsplit('/').next().unwrap_or_default().to_owned();
                self.deleted_files.lock().expect("lock").push(id);
            }
            return Ok(http::Response::builder()
                .status(200)
                .body(Body::Empty)
                .expect("response"));
        }
        Ok(http::Response::builder()
            .status(404)
            .header("content-type", "application/json")
            .body(Body::from(b"{\"error\": {\"message\": \"not found\"}}".to_vec()))
            .expect("response"))
    }
}
