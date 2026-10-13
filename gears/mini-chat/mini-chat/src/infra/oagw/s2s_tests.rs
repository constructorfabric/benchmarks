#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::S2sContext;
use crate::domain::error::DomainError;
use crate::testing::TestUser;

#[test]
fn empty_holder_is_internal_error() {
    assert!(matches!(
        S2sContext::new().get(),
        Err(DomainError::Internal(_))
    ));
}

#[test]
fn set_then_get_returns_context() {
    let s2s = S2sContext::new();
    s2s.set(TestUser::S2S.security_context());
    let ctx = s2s.get().unwrap();
    assert_eq!(ctx.subject_tenant_id(), TestUser::S2S.tenant_id);
    assert_eq!(ctx.subject_id(), TestUser::S2S.user_id);
}

mod bootstrap {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use authn_resolver_sdk::{
        AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
    };
    use secrecy::ExposeSecret;

    use super::super::S2sBootstrap;
    use crate::config::ClientCredentialsConfig;
    use crate::testing::TestUser;

    /// Answers the scripted errors first, then the S2S identity.
    struct FakeAuthn {
        errors: Mutex<VecDeque<AuthNResolverError>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeAuthn {
        fn new(errors: Vec<AuthNResolverError>) -> Arc<Self> {
            Arc::new(Self {
                errors: Mutex::new(errors.into()),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AuthNResolverClient for FakeAuthn {
        async fn authenticate(
            &self,
            _bearer_token: &str,
        ) -> Result<AuthenticationResult, AuthNResolverError> {
            unreachable!("not used")
        }

        async fn exchange_client_credentials(
            &self,
            request: &ClientCredentialsRequest,
        ) -> Result<AuthenticationResult, AuthNResolverError> {
            self.calls.lock().unwrap().push((
                request.client_id.clone(),
                request.client_secret.expose_secret().to_owned(),
            ));
            if let Some(err) = self.errors.lock().unwrap().pop_front() {
                return Err(err);
            }
            Ok(AuthenticationResult {
                security_context: TestUser::S2S.security_context(),
            })
        }
    }

    fn creds() -> ClientCredentialsConfig {
        ClientCredentialsConfig {
            client_id: "mini-chat".to_owned(),
            client_secret: "mini-chat-dev-secret".to_owned().into(),
        }
    }

    fn fast() -> S2sBootstrap {
        S2sBootstrap {
            deadline: Duration::from_secs(2),
            interval: Duration::from_millis(10),
        }
    }

    #[tokio::test]
    async fn exchange_retries_while_the_plugin_is_not_available() {
        let authn = FakeAuthn::new(vec![
            AuthNResolverError::NoPluginAvailable,
            AuthNResolverError::ServiceUnavailable("starting".to_owned()),
            AuthNResolverError::Internal("types registry not ready".to_owned()),
        ]);
        let ctx = fast()
            .run(authn.clone() as Arc<dyn AuthNResolverClient>, &creds())
            .await
            .unwrap();
        assert_eq!(ctx.subject_id(), TestUser::S2S.user_id);
        let calls = authn.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls[0],
            ("mini-chat".to_owned(), "mini-chat-dev-secret".to_owned())
        );
    }

    #[tokio::test]
    async fn exchange_fails_at_once_on_rejected_credentials() {
        let authn = FakeAuthn::new(vec![AuthNResolverError::TokenAcquisitionFailed(
            "unknown client".to_owned(),
        )]);
        let err = fast()
            .run(authn.clone() as Arc<dyn AuthNResolverClient>, &creds())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("client_credentials"), "{err}");
        assert_eq!(authn.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn exchange_gives_up_after_the_deadline() {
        let authn = FakeAuthn::new(
            (0..1000)
                .map(|_| AuthNResolverError::NoPluginAvailable)
                .collect(),
        );
        let bootstrap = S2sBootstrap {
            deadline: Duration::from_millis(100),
            interval: Duration::from_millis(10),
        };
        let err = bootstrap
            .run(authn.clone() as Arc<dyn AuthNResolverClient>, &creds())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no plugin available"), "{err}");
        assert!(authn.calls.lock().unwrap().len() > 1);
    }
}
