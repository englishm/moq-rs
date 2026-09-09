// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-session authorization state, carried by the producer and consumer.

use std::sync::Arc;

use super::{AuthHook, AuthRequest, AuthzOperation, DenyReason, Principal, UNSCOPED};
use crate::SessionContext;

/// The authorization state of one established session.
///
/// Created after [`AuthHook::on_setup`] allows the session, then cloned into
/// the producer and consumer. Sessions in a scope that does not require
/// authorization carry `None` instead, so the request path costs a single
/// `Option` check.
#[derive(Clone)]
pub(crate) struct SessionAuth {
    hook: Arc<dyn AuthHook>,
    principal: Principal,
}

impl SessionAuth {
    pub(crate) fn new(hook: Arc<dyn AuthHook>, principal: Principal) -> Self {
        Self { hook, principal }
    }

    /// The authenticated subject, for logging.
    pub(crate) fn subject(&self) -> Option<&str> {
        self.principal.subject()
    }

    /// Authorize one operation.
    ///
    /// A hook fault is reported as a denial: the relay must not proceed with
    /// an operation it failed to authorize. The two cases are still
    /// distinguished in logs and metrics, because a fault indicates a relay or
    /// configuration problem rather than a misbehaving peer.
    pub(crate) async fn authorize(
        &self,
        session: &SessionContext,
        operation: AuthzOperation<'_>,
        request_id: Option<u64>,
    ) -> Result<(), DenyReason> {
        let label = operation.label();

        let request = AuthRequest {
            session,
            principal: &self.principal,
            operation,
            request_id,
        };

        let decision = match self.hook.on_request(&request).await {
            Ok(decision) => decision,
            Err(err) => {
                tracing::error!(
                    scope = session.scope().unwrap_or(UNSCOPED),
                    subject = self.subject(),
                    operation = label,
                    error = %err,
                    "authorization hook failed; denying"
                );
                metrics::counter!(
                    "moq_relay_auth_errors_total",
                    "stage" => "request"
                )
                .increment(1);
                return Err(DenyReason::HookFault {
                    message: "authorization unavailable".to_string(),
                });
            }
        };

        match decision.into_principal() {
            Ok(_) => Ok(()),
            Err(reason) => {
                tracing::debug!(
                    scope = session.scope().unwrap_or(UNSCOPED),
                    subject = self.subject(),
                    operation = label,
                    reason = %reason,
                    "request denied"
                );
                metrics::counter!(
                    "moq_relay_auth_denied_total",
                    "phase" => "request",
                    "operation" => label,
                    "reason" => reason.label(),
                )
                .increment(1);
                Err(reason)
            }
        }
    }
}

/// Authorize an operation when the session may not require authorization.
///
/// Sessions in scopes without an authorization policy carry no [`SessionAuth`]
/// and are always permitted. Keeping that check in one place stops each call
/// site from re-deriving the `None` case.
pub(crate) async fn authorize(
    auth: Option<&SessionAuth>,
    session: &SessionContext,
    operation: AuthzOperation<'_>,
    request_id: Option<u64>,
) -> Result<(), DenyReason> {
    match auth {
        Some(auth) => auth.authorize(session, operation, request_id).await,
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AllowAllAuthHook, AuthDecision, AuthError, AuthToken, DenyAllAuthHook};

    use async_trait::async_trait;
    use moq_transport::coding::TrackNamespace;

    struct FaultyHook;

    #[async_trait]
    impl AuthHook for FaultyHook {
        async fn on_setup(
            &self,
            _session: &SessionContext,
            _tokens: &[AuthToken],
        ) -> Result<AuthDecision, AuthError> {
            Err(AuthError::Backend("boom".to_string()))
        }

        async fn on_request(&self, _request: &AuthRequest<'_>) -> Result<AuthDecision, AuthError> {
            Err(AuthError::Backend("boom".to_string()))
        }
    }

    fn session() -> SessionContext {
        SessionContext::public(Some("scope".to_string()))
    }

    #[tokio::test]
    async fn no_auth_state_permits_the_operation() {
        let namespace = TrackNamespace::from_utf8_path("sports");
        let result = authorize(
            None,
            &session(),
            AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            None,
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn an_allowing_hook_permits_the_operation() {
        let auth = SessionAuth::new(Arc::new(AllowAllAuthHook), Principal::anonymous());
        let namespace = TrackNamespace::from_utf8_path("sports");

        assert!(authorize(
            Some(&auth),
            &session(),
            AuthzOperation::PublishNamespace {
                namespace: &namespace
            },
            Some(1),
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn a_denying_hook_blocks_the_operation() {
        let auth = SessionAuth::new(
            Arc::new(DenyAllAuthHook::new("nope")),
            Principal::anonymous(),
        );
        let namespace = TrackNamespace::from_utf8_path("sports");

        assert!(authorize(
            Some(&auth),
            &session(),
            AuthzOperation::Subscribe {
                namespace: &namespace,
                track: &moq_transport::coding::TrackName::from("video"),
            },
            Some(1),
        )
        .await
        .is_err());
    }

    /// A hook that errors must not be treated as an allow.
    #[tokio::test]
    async fn a_hook_fault_denies() {
        let auth = SessionAuth::new(Arc::new(FaultyHook), Principal::anonymous());
        let namespace = TrackNamespace::from_utf8_path("sports");

        let err = authorize(
            Some(&auth),
            &session(),
            AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            None,
        )
        .await
        .expect_err("a fault must deny");

        // The peer learns nothing about why: the reason is generic.
        assert!(!err.to_string().contains("boom"), "{err}");
    }
}
