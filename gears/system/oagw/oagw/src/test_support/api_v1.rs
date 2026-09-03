//! API v1 namespace with endpoint factory methods.

use http::Method;

use super::harness::AppHarness;
use super::request::RequestCase;

/// Endpoint factory for the `/oagw/v1/` API surface.
pub struct ApiV1<'a> {
    harness: &'a AppHarness,
}

impl<'a> ApiV1<'a> {
    pub(crate) fn new(harness: &'a AppHarness) -> Self {
        Self { harness }
    }

    // -- Upstream CRUD --

    pub fn post_upstream(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::POST, "/oagw/v1/upstreams")
    }

    pub fn get_upstream(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::GET,
            format!("/oagw/v1/upstreams/{id}"),
        )
    }

    pub fn list_upstreams(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, "/oagw/v1/upstreams")
    }

    pub fn put_upstream(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::PUT,
            format!("/oagw/v1/upstreams/{id}"),
        )
    }

    pub fn delete_upstream(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::DELETE,
            format!("/oagw/v1/upstreams/{id}"),
        )
    }

    // -- Route CRUD --

    pub fn post_route(&self) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::POST, "/oagw/v1/routes")
    }

    pub fn get_route(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::GET, format!("/oagw/v1/routes/{id}"))
    }

    pub fn list_routes(&self, upstream_id: Option<&str>) -> RequestCase<'a> {
        let url = match upstream_id {
            Some(id) => format!("/oagw/v1/routes?upstream_id={id}"),
            None => "/oagw/v1/routes".to_string(),
        };
        RequestCase::new(self.harness, Method::GET, url)
    }

    pub fn put_route(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(self.harness, Method::PUT, format!("/oagw/v1/routes/{id}"))
    }

    pub fn delete_route(&self, id: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            Method::DELETE,
            format!("/oagw/v1/routes/{id}"),
        )
    }

    // -- Proxy --

    pub fn proxy(&self, method: Method, alias: &str, path: &str) -> RequestCase<'a> {
        RequestCase::new(
            self.harness,
            method,
            format!("/oagw/v1/proxy/{alias}/{path}"),
        )
    }

    pub fn proxy_post(&self, alias: &str, path: &str) -> RequestCase<'a> {
        self.proxy(Method::POST, alias, path)
    }

    pub fn proxy_get(&self, alias: &str, path: &str) -> RequestCase<'a> {
        self.proxy(Method::GET, alias, path)
    }
}
