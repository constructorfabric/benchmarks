//! OAGW permission scopes and checks (DESIGN §3.3 "Authentication &
//! Authorization").
//!
//! Scope identifiers follow the GTS permission grammar
//! `gts.cf.core.oagw.{resource}.v1~:{action}`. Checks pass when the bearer
//! token lists the exact scope or the first-party wildcard `*` (the static
//! authn plugin issues `*` by default).

use toolkit_security::SecurityContext;

use crate::domain::error::{OagwError, OagwResult};

/// Upstream management scopes.
pub mod upstream {
    pub const CREATE: &str = "gts.cf.core.oagw.upstream.v1~:create";
    pub const OVERRIDE: &str = "gts.cf.core.oagw.upstream.v1~:override";
    pub const READ: &str = "gts.cf.core.oagw.upstream.v1~:read";
    pub const DELETE: &str = "gts.cf.core.oagw.upstream.v1~:delete";
    /// Required to create an upstream whose alias matches an ancestor's
    /// ("bind" semantics, DESIGN §3.3 CRUD Semantics).
    pub const BIND: &str = "gts.cf.core.oagw.upstream.v1~:bind";
    /// Required to override an inherited ancestor auth config.
    pub const OVERRIDE_AUTH: &str = "gts.cf.core.oagw.upstream.v1~:override_auth";
    /// Required to specify own rate limits under an inherited ancestor.
    pub const OVERRIDE_RATE: &str = "gts.cf.core.oagw.upstream.v1~:override_rate";
    /// Required to append own plugins to an inherited chain.
    pub const ADD_PLUGINS: &str = "gts.cf.core.oagw.upstream.v1~:add_plugins";
}

/// Route management scopes.
pub mod route {
    pub const CREATE: &str = "gts.cf.core.oagw.route.v1~:create";
    pub const OVERRIDE: &str = "gts.cf.core.oagw.route.v1~:override";
    pub const READ: &str = "gts.cf.core.oagw.route.v1~:read";
    pub const DELETE: &str = "gts.cf.core.oagw.route.v1~:delete";
}

/// Custom plugin family scopes.
pub mod plugin {
    /// Auth plugin family.
    pub mod auth {
        pub const CREATE: &str = "gts.cf.core.oagw.auth_plugin.v1~:create";
        pub const READ: &str = "gts.cf.core.oagw.auth_plugin.v1~:read";
        pub const DELETE: &str = "gts.cf.core.oagw.auth_plugin.v1~:delete";
    }
    /// Guard plugin family.
    pub mod guard {
        pub const CREATE: &str = "gts.cf.core.oagw.guard_plugin.v1~:create";
        pub const READ: &str = "gts.cf.core.oagw.guard_plugin.v1~:read";
        pub const DELETE: &str = "gts.cf.core.oagw.guard_plugin.v1~:delete";
    }
    /// Transform plugin family.
    pub mod transform {
        pub const CREATE: &str = "gts.cf.core.oagw.transform_plugin.v1~:create";
        pub const READ: &str = "gts.cf.core.oagw.transform_plugin.v1~:read";
        pub const DELETE: &str = "gts.cf.core.oagw.transform_plugin.v1~:delete";
    }
}

/// Proxy data-plane scope.
pub const PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

/// Whether the token may *read* a given custom-plugin family.
#[must_use]
pub fn has_scope_for_family_read(ctx: &SecurityContext, family: &str) -> bool {
    let scope = match family {
        "auth" => plugin::auth::READ,
        "guard" => plugin::guard::READ,
        "transform" => plugin::transform::READ,
        _ => return false,
    };
    has_scope(ctx, scope)
}

/// Whether the token carries `required` (case-sensitive exact match) or `*`.
#[must_use]
pub fn has_scope(ctx: &SecurityContext, required: &str) -> bool {
    ctx.token_scopes().iter().any(|s| s == "*" || s == required)
}

/// Require `required` scope or the `*` wildcard, else 403 permission denied.
///
/// # Errors
///
/// Returns [`OagwError::Forbidden`] when the scope is missing.
pub fn require_scope(ctx: &SecurityContext, required: &str) -> OagwResult<()> {
    if has_scope(ctx, required) {
        Ok(())
    } else {
        Err(OagwError::Forbidden {
            detail: format!("the bearer token lacks required permission {required}"),
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use toolkit_security::context::SecurityContextBuilder;

    fn ctx(scopes: Vec<&str>) -> SecurityContext {
        SecurityContextBuilder::default()
            .subject_id(uuid::Uuid::nil())
            .subject_tenant_id(uuid::Uuid::nil())
            .token_scopes(scopes.into_iter().map(str::to_owned).collect())
            .build()
            .expect("valid builder")
    }

    #[test]
    fn wildcard_passes_everything() {
        let ctx = ctx(vec!["*"]);
        assert!(has_scope(&ctx, upstream::CREATE));
        assert!(has_scope(&ctx, PROXY_INVOKE));
        assert!(require_scope(&ctx, PROXY_INVOKE).is_ok());
    }

    #[test]
    fn exact_scope_or_denied() {
        let ctx = ctx(vec![upstream::CREATE]);
        assert!(has_scope(&ctx, upstream::CREATE));
        assert!(!has_scope(&ctx, upstream::DELETE));
        assert!(require_scope(&ctx, upstream::DELETE).is_err());
    }
}
