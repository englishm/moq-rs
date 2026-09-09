// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Resolves a scope's authorization policy into a hook, and caches it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::{AuthError, AuthHook, DenyAllAuthHook, UNSCOPED};
use crate::{Coordinator, ScopeAuthConfig};

/// How long an unusable configuration is cached before it is retried.
///
/// Short enough that fixing a misconfiguration takes effect promptly, long
/// enough that a broken scope under load does not turn into a retry storm
/// against the coordinator.
const BROKEN_CONFIG_TTL: Duration = Duration::from_secs(30);

/// Builds and caches the authorization hook for each scope.
///
/// [`Coordinator::get_scope_config`] is the source of truth for a scope's
/// keys, but calling it per connection would put a coordinator round-trip and
/// a set of PEM parses on the accept path. Results are therefore cached for
/// [`ScopeAuthConfig::ttl`], which also bounds how long a key rotation takes
/// to reach new sessions.
pub(crate) struct ScopeAuthorizer {
    coordinator: Arc<dyn Coordinator>,
    cache: RwLock<HashMap<Option<String>, CacheEntry>>,
}

#[derive(Clone)]
struct CacheEntry {
    state: CacheState,
    expires_at: Instant,
}

#[derive(Clone)]
enum CacheState {
    /// The scope requires token authorization.
    Enforced(Arc<dyn AuthHook>),
    /// The scope did not opt in; sessions skip authorization entirely.
    Disabled,
}

impl ScopeAuthorizer {
    pub(crate) fn new(coordinator: Arc<dyn Coordinator>) -> Self {
        Self {
            coordinator,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// The hook for a scope, or `None` if the scope does not require
    /// authorization.
    ///
    /// Returns `Err` only when the scope's policy could not be determined at
    /// all — the caller must fail closed, since it cannot know whether the
    /// scope requires a token.
    pub(crate) async fn resolve(
        &self,
        scope: Option<&str>,
    ) -> Result<Option<Arc<dyn AuthHook>>, AuthError> {
        if let Some(entry) = self.cached(scope) {
            return Ok(entry.into_hook());
        }

        let config = self
            .coordinator
            .get_scope_config(scope)
            .await
            .map_err(|err| AuthError::Backend(format!("get_scope_config failed: {err}")))?;

        let (state, ttl) = match config.auth {
            None => {
                // Logged once per TTL rather than per connection. "No policy"
                // is a legitimate configuration, but it is indistinguishable
                // from a coordinator that forgot to implement
                // `get_scope_config`, so it should be visible in logs rather
                // than inferred from the absence of anything.
                tracing::info!(
                    scope = scope.unwrap_or(UNSCOPED),
                    "scope has no token authorization policy; admitting sessions on \
                     scope permissions alone"
                );
                (CacheState::Disabled, super::DEFAULT_SCOPE_AUTH_TTL)
            }
            Some(auth) => {
                let ttl = auth.effective_ttl();
                match build_hook(&auth, scope) {
                    Ok(hook) => (CacheState::Enforced(hook), ttl),
                    // A scope that asked for authorization but cannot have it
                    // must refuse every session, not run unauthenticated.
                    Err(err) => {
                        tracing::error!(
                            scope = scope.unwrap_or(UNSCOPED),
                            error = %err,
                            "scope requires token authorization but its configuration is unusable; \
                             refusing all sessions in this scope"
                        );
                        metrics::counter!(
                            "moq_relay_auth_errors_total",
                            "stage" => "scope_config"
                        )
                        .increment(1);
                        (
                            CacheState::Enforced(Arc::new(DenyAllAuthHook::new(err.to_string()))),
                            BROKEN_CONFIG_TTL,
                        )
                    }
                }
            }
        };

        self.store(scope, state.clone(), ttl);

        Ok(CacheEntry {
            state,
            expires_at: Instant::now() + ttl,
        }
        .into_hook())
    }

    fn cached(&self, scope: Option<&str>) -> Option<CacheEntry> {
        let cache = self.cache.read().unwrap_or_else(|err| err.into_inner());
        // `Option<&str>` cannot look up an `Option<String>` key directly, and
        // the map is small (one entry per active scope), so an equality scan
        // avoids allocating a key on every cache hit.
        cache
            .iter()
            .find(|(key, _)| key.as_deref() == scope)
            .map(|(_, entry)| entry)
            .filter(|entry| entry.expires_at > Instant::now())
            .cloned()
    }

    fn store(&self, scope: Option<&str>, state: CacheState, ttl: Duration) {
        let mut cache = self.cache.write().unwrap_or_else(|err| err.into_inner());
        cache.insert(
            scope.map(str::to_string),
            CacheEntry {
                state,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// Drop a scope's cached policy, forcing the next session to re-fetch it.
    #[cfg(test)]
    pub(crate) fn invalidate(&self, scope: Option<&str>) {
        let mut cache = self.cache.write().unwrap_or_else(|err| err.into_inner());
        cache.retain(|key, _| key.as_deref() != scope);
    }
}

impl CacheEntry {
    fn into_hook(self) -> Option<Arc<dyn AuthHook>> {
        match self.state {
            CacheState::Enforced(hook) => Some(hook),
            CacheState::Disabled => None,
        }
    }
}

/// Build the hook a scope's configuration calls for.
///
/// Separated from [`ScopeAuthorizer::resolve`] so that the feature boundary is
/// a single function rather than a `cfg` scattered through the cache logic.
fn build_hook(
    config: &ScopeAuthConfig,
    scope: Option<&str>,
) -> Result<Arc<dyn AuthHook>, AuthError> {
    #[cfg(feature = "auth-cat")]
    {
        Ok(Arc::new(super::CatAuthHook::new(config, scope)?))
    }

    #[cfg(not(feature = "auth-cat"))]
    {
        // Silence unused-parameter warnings without changing the signature.
        let _ = (config, scope);
        Err(AuthError::Configuration(
            "relay was built without the `auth-cat` feature, so it cannot verify \
             the tokens this scope requires"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use moq_native_ietf::quic;
    use moq_transport::coding::TrackNamespace;

    use crate::{
        AuthPublicKey, CoordinatorContext, CoordinatorError, CoordinatorResult, NamespaceOrigin,
        NamespaceRegistration, ScopeConfig,
    };

    /// A coordinator that returns a scripted config and counts calls, so the
    /// tests can assert on caching rather than on wall-clock behaviour.
    struct ScriptedCoordinator {
        config: std::sync::Mutex<ScopeConfig>,
        calls: AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
    }

    impl ScriptedCoordinator {
        fn new(config: ScopeConfig) -> Self {
            Self {
                config: std::sync::Mutex::new(config),
                calls: AtomicUsize::new(0),
                fail: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// Only the key-rotation test needs to change the scripted config, and
        /// that test requires a hook that actually builds.
        #[cfg(feature = "auth-cat")]
        fn set_config(&self, config: ScopeConfig) {
            *self.config.lock().unwrap() = config;
        }

        fn set_failing(&self, failing: bool) {
            self.fail.store(failing, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Coordinator for ScriptedCoordinator {
        async fn register_namespace(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
            _context: &CoordinatorContext,
        ) -> CoordinatorResult<NamespaceRegistration> {
            Ok(NamespaceRegistration::new(()))
        }

        async fn unregister_namespace(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
        ) -> CoordinatorResult<()> {
            Ok(())
        }

        async fn lookup(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
        ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)> {
            Err(CoordinatorError::NamespaceNotFound)
        }

        async fn get_scope_config(&self, _scope: Option<&str>) -> CoordinatorResult<ScopeConfig> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(CoordinatorError::Other(anyhow::anyhow!("backend down")));
            }
            Ok(self.config.lock().unwrap().clone())
        }
    }

    /// A syntactically valid ES256 public key, so configuration parses.
    fn valid_pem() -> String {
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::EncodePublicKey;

        let signing = SigningKey::random(&mut rand_core_compat());
        p256::ecdsa::VerifyingKey::from(&signing)
            .to_public_key_pem(Default::default())
            .expect("pem")
    }

    /// `p256` re-exports the RNG its own API expects; using it avoids pinning a
    /// separate `rand` version in dev-dependencies.
    fn rand_core_compat() -> impl p256::elliptic_curve::rand_core::CryptoRngCore {
        p256::elliptic_curve::rand_core::OsRng
    }

    fn enforced_config() -> ScopeConfig {
        ScopeConfig {
            auth: Some(ScopeAuthConfig::new(vec![
                AuthPublicKey::es256(valid_pem()),
            ])),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_scope_without_auth_config_needs_no_hook() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig::default()));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        assert!(authorizer.resolve(Some("open")).await.unwrap().is_none());
        assert_eq!(coordinator.calls(), 1);
    }

    #[tokio::test]
    async fn results_are_cached_per_scope() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig::default()));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        for _ in 0..5 {
            authorizer.resolve(Some("tenant")).await.unwrap();
        }
        assert_eq!(coordinator.calls(), 1, "repeat resolves must hit the cache");

        // A different scope is a separate cache entry.
        authorizer.resolve(Some("other")).await.unwrap();
        assert_eq!(coordinator.calls(), 2);

        // The unscoped session is its own entry, distinct from any named scope.
        authorizer.resolve(None).await.unwrap();
        assert_eq!(coordinator.calls(), 3);
        authorizer.resolve(None).await.unwrap();
        assert_eq!(coordinator.calls(), 3);
    }

    /// A usable policy is cached for the TTL the coordinator asked for, so an
    /// already-elapsed TTL forces a refetch. Needs the feature, since without
    /// it the policy is unbuildable and takes the broken-config path below.
    #[cfg(feature = "auth-cat")]
    #[tokio::test]
    async fn an_expired_entry_is_refetched() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig {
            auth: Some(
                ScopeAuthConfig::new(vec![AuthPublicKey::es256(valid_pem())])
                    // Already elapsed by the time the next resolve runs.
                    .with_ttl(Duration::from_nanos(1)),
            ),
            ..Default::default()
        }));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        authorizer.resolve(Some("tenant")).await.ok();
        tokio::time::sleep(Duration::from_millis(5)).await;
        authorizer.resolve(Some("tenant")).await.ok();

        assert_eq!(coordinator.calls(), 2, "an expired entry must be refetched");
    }

    /// An unusable policy is cached too, on its own shorter TTL, so a broken
    /// scope under load does not turn into a retry storm against the
    /// coordinator. It must still deny throughout.
    #[tokio::test]
    async fn a_broken_config_is_cached_and_keeps_denying() {
        // Unbuildable in both configurations: with the feature because the key
        // list is empty, without it because the scheme is unavailable.
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig {
            auth: Some(
                ScopeAuthConfig::new(vec![])
                    // Ignored in favour of BROKEN_CONFIG_TTL.
                    .with_ttl(Duration::from_nanos(1)),
            ),
            ..Default::default()
        }));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());
        let session = crate::SessionContext::public(Some("tenant".to_string()));

        for _ in 0..3 {
            let hook = authorizer.resolve(Some("tenant")).await.unwrap().unwrap();
            assert!(
                !hook.on_setup(&session, &[]).await.unwrap().is_allowed(),
                "a broken scope must deny on every resolve"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        assert_eq!(
            coordinator.calls(),
            1,
            "a broken config must be cached, not refetched per connection"
        );
    }

    #[tokio::test]
    async fn a_coordinator_failure_fails_closed() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig::default()));
        coordinator.set_failing(true);
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        // Not `Ok(None)`: the relay must not admit a session merely because it
        // could not find out whether the scope requires a token.
        let err = authorizer
            .resolve(Some("tenant"))
            .await
            .err()
            .expect("a backend failure must surface");
        assert!(matches!(err, AuthError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_failure_is_not_cached_as_permissive() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig::default()));
        coordinator.set_failing(true);
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        assert!(authorizer.resolve(Some("tenant")).await.is_err());

        // Once the backend recovers the scope resolves normally: the failure
        // left no poisoned entry behind.
        coordinator.set_failing(false);
        assert!(authorizer.resolve(Some("tenant")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unusable_config_denies_rather_than_disables() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig {
            // Enabled but with no keys: unusable.
            auth: Some(ScopeAuthConfig::new(vec![])),
            ..Default::default()
        }));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        let hook = authorizer
            .resolve(Some("tenant"))
            .await
            .unwrap()
            .expect("a broken scope must still get a hook, not be waved through");

        let session = crate::SessionContext::public(Some("tenant".to_string()));
        assert!(!hook.on_setup(&session, &[]).await.unwrap().is_allowed());
    }

    #[tokio::test]
    async fn too_many_keys_denies_rather_than_disables() {
        let keys = (0..crate::MAX_SCOPE_AUTH_KEYS + 1)
            .map(|_| AuthPublicKey::es256(valid_pem()))
            .collect();
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig {
            auth: Some(ScopeAuthConfig::new(keys)),
            ..Default::default()
        }));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        let hook = authorizer.resolve(Some("tenant")).await.unwrap().unwrap();
        let session = crate::SessionContext::public(Some("tenant".to_string()));
        assert!(!hook.on_setup(&session, &[]).await.unwrap().is_allowed());
    }

    /// Without the feature, a scope that requires tokens cannot be served, so
    /// it must refuse sessions rather than run unauthenticated.
    #[cfg(not(feature = "auth-cat"))]
    #[tokio::test]
    async fn a_scope_requiring_tokens_is_refused_without_the_feature() {
        let coordinator = Arc::new(ScriptedCoordinator::new(enforced_config()));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        let hook = authorizer.resolve(Some("tenant")).await.unwrap().unwrap();
        let session = crate::SessionContext::public(Some("tenant".to_string()));
        let decision = hook.on_setup(&session, &[]).await.unwrap();

        assert!(!decision.is_allowed());
        assert!(
            decision
                .deny_reason()
                .unwrap()
                .to_string()
                .contains("auth-cat"),
            "the denial should name the missing feature"
        );

        // The request path must deny too. A hook that refused setup but waved
        // requests through would be no protection at all for any session that
        // reached the request stage by another route.
        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let track = moq_transport::coding::TrackName::from("video");
        let auth = crate::auth::SessionAuth::new(hook, crate::auth::Principal::anonymous());

        for operation in [
            crate::auth::AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            crate::auth::AuthzOperation::Publish {
                namespace: &namespace,
                track: &track,
            },
            crate::auth::AuthzOperation::Subscribe {
                namespace: &namespace,
                track: &track,
            },
        ] {
            let label = operation.label();
            assert!(
                crate::auth::authorize(Some(&auth), &session, operation, None)
                    .await
                    .is_err(),
                "{label} must be denied without the feature"
            );
        }
    }

    #[cfg(feature = "auth-cat")]
    #[tokio::test]
    async fn a_well_formed_scope_gets_a_working_hook() {
        let coordinator = Arc::new(ScriptedCoordinator::new(enforced_config()));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        let hook = authorizer.resolve(Some("tenant")).await.unwrap().unwrap();
        let session = crate::SessionContext::public(Some("tenant".to_string()));

        // No token presented, so denied - but by the CAT hook, on the merits.
        let decision = hook.on_setup(&session, &[]).await.unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(crate::auth::DenyReason::TokenMissing)
        ));
    }

    #[cfg(feature = "auth-cat")]
    #[tokio::test]
    async fn rotating_keys_reaches_new_sessions_after_the_ttl() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig {
            auth: Some(
                ScopeAuthConfig::new(vec![AuthPublicKey::es256(valid_pem())])
                    .with_ttl(Duration::from_nanos(1)),
            ),
            ..Default::default()
        }));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        authorizer.resolve(Some("tenant")).await.unwrap();

        // Rotate to a configuration the relay cannot use; after the TTL the
        // scope must pick it up and start refusing.
        coordinator.set_config(ScopeConfig {
            auth: Some(ScopeAuthConfig::new(vec![])),
            ..Default::default()
        });
        tokio::time::sleep(Duration::from_millis(5)).await;

        let hook = authorizer.resolve(Some("tenant")).await.unwrap().unwrap();
        let session = crate::SessionContext::public(Some("tenant".to_string()));
        assert!(!hook.on_setup(&session, &[]).await.unwrap().is_allowed());
        assert_eq!(coordinator.calls(), 2);
    }

    #[tokio::test]
    async fn concurrent_resolves_are_consistent() {
        // The cache is shared across connection tasks; this exercises the lock
        // discipline under contention rather than asserting a call count, since
        // concurrent misses may legitimately each fetch.
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig::default()));
        let authorizer = Arc::new(ScopeAuthorizer::new(coordinator.clone()));

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let authorizer = authorizer.clone();
            tasks.push(tokio::spawn(async move {
                authorizer.resolve(Some("tenant")).await.unwrap().is_none()
            }));
        }

        for task in tasks {
            assert!(task.await.unwrap(), "every resolve must agree");
        }
    }

    #[tokio::test]
    async fn invalidate_forces_a_refetch() {
        let coordinator = Arc::new(ScriptedCoordinator::new(ScopeConfig::default()));
        let authorizer = ScopeAuthorizer::new(coordinator.clone());

        authorizer.resolve(Some("tenant")).await.unwrap();
        authorizer.resolve(Some("tenant")).await.unwrap();
        assert_eq!(coordinator.calls(), 1);

        authorizer.invalidate(Some("tenant"));
        authorizer.resolve(Some("tenant")).await.unwrap();
        assert_eq!(coordinator.calls(), 2);
    }
}
