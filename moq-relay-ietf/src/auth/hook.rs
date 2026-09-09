// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use async_trait::async_trait;

use super::{AuthDecision, AuthError, AuthRequest, AuthToken, DenyReason, Principal};
use crate::SessionContext;

/// Pluggable authorization for a MoQT session.
///
/// A hook is built per scope from [`ScopeConfig::auth`] and shared by every
/// session in that scope. Both methods are required rather than defaulted: a
/// default that allows would silently authorize everything in any
/// implementation that forgot to override it, so the compiler is made to
/// insist that every hook states a verdict. Use [`AllowAllAuthHook`] to opt
/// into permissive behaviour explicitly.
///
/// # Contract
///
/// * [`on_setup`] runs once per session, before either half of the session is
///   constructed. Denying it terminates the session.
/// * [`on_request`] runs before every authorization-relevant request, and is
///   given the [`Principal`] that `on_setup` established. Implementations
///   should treat it as a hot path and avoid cryptography there — verify at
///   setup and carry the validated state in the principal.
/// * Returning `Err` is never an allow. The relay treats it as a denial and
///   counts it separately as a fault.
///
/// [`ScopeConfig::auth`]: crate::ScopeConfig::auth
/// [`on_setup`]: Self::on_setup
/// [`on_request`]: Self::on_request
#[async_trait]
pub trait AuthHook: Send + Sync {
    /// Authorize session establishment and establish the peer's identity.
    ///
    /// `tokens` holds every AUTHORIZATION TOKEN decoded from CLIENT_SETUP, in
    /// the order sent, with aliases already resolved. It may be empty; a hook
    /// that requires a token must deny in that case.
    async fn on_setup(
        &self,
        session: &SessionContext,
        tokens: &[AuthToken],
    ) -> Result<AuthDecision, AuthError>;

    /// Authorize a single request against the identity from setup.
    async fn on_request(&self, request: &AuthRequest<'_>) -> Result<AuthDecision, AuthError>;
}

/// A hook that allows every operation.
///
/// Provided so that permissive behaviour is something a deployment names
/// explicitly. The relay does not install it by default: a scope with no
/// [`ScopeConfig::auth`] runs with no hook at all, which costs nothing on the
/// request path.
///
/// [`ScopeConfig::auth`]: crate::ScopeConfig::auth
pub struct AllowAllAuthHook;

#[async_trait]
impl AuthHook for AllowAllAuthHook {
    async fn on_setup(
        &self,
        _session: &SessionContext,
        _tokens: &[AuthToken],
    ) -> Result<AuthDecision, AuthError> {
        Ok(AuthDecision::allow(Principal::anonymous()))
    }

    async fn on_request(&self, _request: &AuthRequest<'_>) -> Result<AuthDecision, AuthError> {
        Ok(AuthDecision::allow(Principal::anonymous()))
    }
}

/// A hook that denies every operation.
///
/// Installed for a scope whose authorization configuration cannot be turned
/// into a working hook, so that a misconfiguration fails closed for the whole
/// scope instead of admitting sessions unauthenticated. The reason is captured
/// once at construction and reported on every denial.
pub(crate) struct DenyAllAuthHook {
    reason: String,
}

impl DenyAllAuthHook {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn deny(&self) -> AuthDecision {
        AuthDecision::deny(DenyReason::PolicyDenied {
            message: self.reason.clone(),
        })
    }
}

#[async_trait]
impl AuthHook for DenyAllAuthHook {
    async fn on_setup(
        &self,
        _session: &SessionContext,
        _tokens: &[AuthToken],
    ) -> Result<AuthDecision, AuthError> {
        Ok(self.deny())
    }

    async fn on_request(&self, _request: &AuthRequest<'_>) -> Result<AuthDecision, AuthError> {
        Ok(self.deny())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthzOperation;
    use moq_transport::coding::TrackNamespace;

    fn session() -> SessionContext {
        SessionContext::public(Some("scope-a".to_string()))
    }

    #[tokio::test]
    async fn allow_all_allows_setup_and_requests() {
        let hook = AllowAllAuthHook;
        let session = session();

        assert!(hook.on_setup(&session, &[]).await.unwrap().is_allowed());

        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let principal = Principal::anonymous();
        let request = AuthRequest {
            session: &session,
            principal: &principal,
            operation: AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            request_id: Some(1),
        };

        assert!(hook.on_request(&request).await.unwrap().is_allowed());
    }

    #[tokio::test]
    async fn deny_all_denies_setup_and_requests() {
        let hook = DenyAllAuthHook::new("no keys configured");
        let session = session();

        let decision = hook.on_setup(&session, &[]).await.unwrap();
        assert!(!decision.is_allowed());
        assert!(decision
            .deny_reason()
            .unwrap()
            .to_string()
            .contains("no keys"));

        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let principal = Principal::anonymous();
        let request = AuthRequest {
            session: &session,
            principal: &principal,
            operation: AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            request_id: Some(1),
        };

        assert!(!hook.on_request(&request).await.unwrap().is_allowed());
    }
}
