//! The no-operation auth plugin (`cpt-cf-oagw-algo-noop-auth`,
//! `cpt-cf-oagw-dod-static-auth-plugins`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use crate::error::OagwError;

/// Completes the auth phase without reading any configuration key,
/// resolving any credential reference, contacting any external service, or
/// mutating any header or query parameter (`cpt-cf-oagw-algo-noop-auth`).
///
/// # Errors
///
/// Never returns an error; the `Result` return type matches every other
/// auth plugin's signature so the chain-execution call site stays uniform.
// @cpt-begin:cpt-cf-oagw-algo-noop-auth:p2:inst-noop-auth-fn-01
#[allow(clippy::unnecessary_wraps)]
pub fn authenticate() -> Result<(), OagwError> {
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-noop-auth:p2:inst-noop-auth-fn-01

#[cfg(test)]
mod tests {
    use super::authenticate;

    // @cpt-begin:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-noop-auth-test-01
    #[test]
    fn noop_auth_always_succeeds() {
        assert!(authenticate().is_ok());
    }
    // @cpt-end:cpt-cf-oagw-dod-static-auth-plugins:p2:inst-noop-auth-test-01
}
