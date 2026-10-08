//! `OData` list query extractor: the platform `OData` extractor plus a rewrite
//! of quoted UUID literals compared with `id` (`id eq '<uuid>'`).

use axum::extract::FromRequestParts;
use http::request::Parts;
use toolkit::api::odata::OData;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::ODataQuery;
use toolkit_odata::ast::{Expr, Value};
use uuid::Uuid;

/// `OData` query of the list endpoints.
#[derive(Debug, Clone)]
pub struct ListQuery(pub ODataQuery);

fn is_id(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(name) if name == "id")
}

fn uuid_literal(e: Expr) -> Expr {
    match e {
        Expr::Value(Value::String(s)) => match Uuid::parse_str(s.trim()) {
            Ok(u) => Expr::Value(Value::Uuid(u)),
            Err(_) => Expr::Value(Value::String(s)),
        },
        other => other,
    }
}

/// Rewrite quoted UUID literals compared with `id`.
#[must_use]
pub fn rewrite(e: Expr) -> Expr {
    match e {
        Expr::And(a, b) => Expr::And(Box::new(rewrite(*a)), Box::new(rewrite(*b))),
        Expr::Or(a, b) => Expr::Or(Box::new(rewrite(*a)), Box::new(rewrite(*b))),
        Expr::Not(a) => Expr::Not(Box::new(rewrite(*a))),
        Expr::Compare(l, op, r) => {
            if is_id(&l) {
                Expr::Compare(l, op, Box::new(uuid_literal(*r)))
            } else if is_id(&r) {
                Expr::Compare(Box::new(uuid_literal(*l)), op, r)
            } else {
                Expr::Compare(l, op, r)
            }
        }
        Expr::In(l, items) => {
            if is_id(&l) {
                Expr::In(l, items.into_iter().map(uuid_literal).collect())
            } else {
                Expr::In(l, items)
            }
        }
        other => other,
    }
}

impl<S> FromRequestParts<S> for ListQuery
where
    S: Send + Sync,
{
    type Rejection = CanonicalError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let OData(mut q) = OData::from_request_parts(parts, state).await?;
        if let Some(f) = q.filter.take() {
            q.filter = Some(Box::new(rewrite(*f)));
        }
        Ok(Self(q))
    }
}
