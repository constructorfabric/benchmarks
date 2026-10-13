//! OAGW upstream / route provisioning for every provider entry (ADR-0005).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID,
    HeadersConfig, HttpMatch, HttpMethod, ListQuery, MatchRules, PassthroughMode, PathSuffixMode,
    RequestHeaderRules, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::{ProviderEntry, StorageKind};

/// One upstream to provision (a provider entry or one of its tenant overrides).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTarget {
    pub key: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub alias: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<HashMap<String, String>>,
    pub api_path: String,
    pub storage_kind: Option<StorageKind>,
}

/// Builds the upstream targets of all entries (aliases default to the host).
#[must_use]
pub fn targets<S: std::hash::BuildHasher>(
    entries: &HashMap<String, ProviderEntry, S>,
) -> Vec<UpstreamTarget> {
    let mut ids: Vec<&String> = entries.keys().collect();
    ids.sort();
    let mut out = Vec::new();
    for id in ids {
        let e = &entries[id];
        out.push(UpstreamTarget {
            key: id.clone(),
            host: e.host.clone(),
            port: e.effective_port(),
            use_http: e.use_http,
            alias: e.effective_alias().to_owned(),
            auth_plugin_type: e.auth_plugin_type.clone(),
            auth_config: e.auth_config.clone(),
            api_path: e.effective_api_path().to_owned(),
            storage_kind: e.storage_kind,
        });
        let mut tids: Vec<&String> = e.tenant_overrides.keys().collect();
        tids.sort();
        for tid in tids {
            let o = &e.tenant_overrides[tid];
            let host = o.host.clone().unwrap_or_else(|| e.host.clone());
            let alias = o
                .upstream_alias
                .clone()
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| host.clone());
            out.push(UpstreamTarget {
                key: format!("{id}/{tid}"),
                host,
                port: e.effective_port(),
                use_http: e.use_http,
                alias,
                auth_plugin_type: o
                    .auth_plugin_type
                    .clone()
                    .or_else(|| e.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().or_else(|| e.auth_config.clone()),
                api_path: e.effective_api_path().to_owned(),
                storage_kind: e.storage_kind,
            });
        }
    }
    out
}

/// Route prefix and query allowlist derived from `api_path`.
#[must_use]
pub fn chat_route(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = path.split("{model}").next().unwrap_or(path);
    let prefix = if prefix.len() > 1 {
        prefix.trim_end_matches('/')
    } else {
        prefix
    };
    let mut allow: Vec<String> = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    if !allow.iter().any(|k| k == "api-version") {
        allow.push("api-version".to_owned());
    }
    (
        if prefix.is_empty() {
            "/".to_owned()
        } else {
            prefix.to_owned()
        },
        allow,
    )
}

#[derive(Debug)]
pub enum ProvisionError {
    /// Secret not readable yet or another transient condition — retry later.
    Deferred(String),
}

async fn ensure_route(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    upstream_id: uuid::Uuid,
    path: &str,
    methods: Vec<HttpMethod>,
    allow: Vec<String>,
) -> Result<(), ProvisionError> {
    let req = CreateRouteRequest::builder(
        upstream_id,
        MatchRules {
            http: Some(HttpMatch {
                methods,
                path: path.to_owned(),
                query_allowlist: allow,
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
    )
    .tags(vec!["mini-chat".to_owned()])
    .build();
    match gw.create_route(s2s.clone(), req).await {
        Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => Ok(()),
        Err(e) => Err(ProvisionError::Deferred(format!("route {path}: {e}"))),
    }
}

/// Creates (or reuses) the upstream and its routes.
///
/// # Errors
/// Deferred provisioning.
pub async fn provision_target(
    gw: &dyn ServiceGatewayClientV1,
    s2s: &SecurityContext,
    t: &UpstreamTarget,
) -> Result<(), ProvisionError> {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if t.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: t.host.clone(),
            port: t.port,
        }],
    };
    let mut b = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID)
        .alias(t.alias.clone())
        .headers(HeadersConfig {
            request: Some(RequestHeaderRules {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["accept".to_owned()],
                ..RequestHeaderRules::default()
            }),
            response: None,
        })
        .tags(vec![
            "mini-chat".to_owned(),
            format!("mini-chat-provider:{}", t.key),
        ]);
    if let Some(pt) = &t.auth_plugin_type {
        b = b.auth(AuthConfig {
            plugin_type: pt.clone(),
            sharing: SharingMode::Inherit,
            config: t.auth_config.clone(),
        });
    }
    let upstream_id = match gw.create_upstream(s2s.clone(), b.build()).await {
        Ok(u) => u.id,
        Err(CanonicalError::AlreadyExists { .. }) => {
            let all = gw
                .list_upstreams(s2s.clone(), &ListQuery { top: 1000, skip: 0 })
                .await
                .map_err(|e| ProvisionError::Deferred(e.to_string()))?;
            let alias = t.alias.to_ascii_lowercase();
            all.into_iter()
                .find(|u| u.alias.to_ascii_lowercase() == alias)
                .map(|u| u.id)
                .ok_or_else(|| {
                    ProvisionError::Deferred("upstream exists but is not listed".to_owned())
                })?
        }
        Err(e) => return Err(ProvisionError::Deferred(e.to_string())),
    };
    let (chat_prefix, allow) = chat_route(&t.api_path);
    ensure_route(
        gw,
        s2s,
        upstream_id,
        &chat_prefix,
        vec![HttpMethod::Post],
        allow,
    )
    .await?;
    if let Some(kind) = t.storage_kind {
        let prefix = match kind {
            StorageKind::Openai => "/v1",
            StorageKind::Azure => "/openai",
        };
        let allow = vec!["api-version".to_owned()];
        ensure_route(
            gw,
            s2s,
            upstream_id,
            &format!("{prefix}/files"),
            vec![HttpMethod::Post, HttpMethod::Delete, HttpMethod::Get],
            allow.clone(),
        )
        .await?;
        ensure_route(
            gw,
            s2s,
            upstream_id,
            &format!("{prefix}/vector_stores"),
            vec![HttpMethod::Post, HttpMethod::Delete, HttpMethod::Get],
            allow,
        )
        .await?;
    }
    Ok(())
}

/// Provisions every target; deferred ones are retried in the background (2 s doubling to 60 s).
pub async fn provision_all(
    gw: Arc<dyn ServiceGatewayClientV1>,
    s2s: SecurityContext,
    all: Vec<UpstreamTarget>,
    cancel: CancellationToken,
) {
    let mut pending = Vec::new();
    for t in all {
        match provision_target(gw.as_ref(), &s2s, &t).await {
            Ok(()) => {
                tracing::info!(provider = %t.key, alias = %t.alias, "mini-chat: OAGW upstream provisioned");
            }
            Err(ProvisionError::Deferred(e)) => {
                tracing::warn!(provider = %t.key, error = %e, "mini-chat: OAGW provisioning deferred");
                pending.push(t);
            }
        }
    }
    if pending.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let mut warned = false;
        while !pending.is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            let mut still = Vec::new();
            for t in pending {
                match provision_target(gw.as_ref(), &s2s, &t).await {
                    Ok(()) => {
                        tracing::info!(provider = %t.key, "mini-chat: deferred OAGW upstream provisioned");
                    }
                    Err(_) => still.push(t),
                }
            }
            pending = still;
            delay = (delay * 2).min(Duration::from_secs(60));
            if !warned && started.elapsed() > Duration::from_secs(120) && !pending.is_empty() {
                warned = true;
                let names: Vec<&str> = pending.iter().map(|t| t.key.as_str()).collect();
                tracing::warn!(providers = ?names, "mini-chat: providers still not provisioned");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_route_prefix_and_query() {
        assert_eq!(
            chat_route("/v1/responses"),
            ("/v1/responses".to_owned(), vec!["api-version".to_owned()])
        );
        assert_eq!(
            chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21"),
            (
                "/openai/deployments".to_owned(),
                vec!["api-version".to_owned()]
            )
        );
    }
}
