// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! API-based coordinator for multi-relay deployments.
//!
//! This coordinator uses the moq-api HTTP server as a centralized registry
//! to coordinate namespace registration across multiple relay instances.
//! It provides:
//!
//! - HTTP-based namespace lookups via moq-api
//! - A short-lived cache in front of those lookups
//! - Automatic TTL refresh to maintain registrations
//! - High availability when using the moq-api server

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use moq_api::{Client, Origin};
use moq_native_ietf::quic;
use moq_transport::coding::TrackNamespace;
use tokio::time::Instant;
use url::Url;

use moq_relay_ietf::{
    Coordinator, CoordinatorError, CoordinatorResult, NamespaceOrigin, NamespaceRegistration,
};

/// Default TTL for namespace registrations (in seconds)
/// moq-api server uses 600 seconds (10 minutes) TTL
const DEFAULT_REGISTRATION_TTL_SECS: u64 = 600;

/// Default TTL for cached namespace lookups (in seconds)
///
/// Deliberately tiny next to the registration TTL: long enough to collapse the
/// lookups one subscriber makes while subscribing to several tracks, short
/// enough that a registration change is picked up almost immediately.
const DEFAULT_LOOKUP_CACHE_TTL_SECS: u64 = 2;

/// Negative lookups expire this many times sooner than positive ones
const NEGATIVE_LOOKUP_TTL_DIVISOR: u32 = 10;

/// Sweep expired entries once the lookup cache holds this many namespaces
const LOOKUP_CACHE_SWEEP_THRESHOLD: usize = 1024;

/// Configuration for the API coordinator
#[derive(Debug, Clone)]
pub struct ApiCoordinatorConfig {
    /// URL of the moq-api server (e.g., "http://localhost:8080")
    pub api_url: Url,
    /// URL of this relay (advertised to other relays)
    pub relay_url: Url,
    /// TTL for namespace registrations in seconds
    pub registration_ttl_secs: u64,
    /// Interval for refreshing registrations (should be less than TTL)
    pub refresh_interval_secs: u64,
    /// TTL for cached namespace lookups in seconds (0 disables the cache)
    pub lookup_cache_ttl_secs: u64,
}

impl ApiCoordinatorConfig {
    /// Create a new configuration with default TTL values
    pub fn new(api_url: Url, relay_url: Url) -> Self {
        Self {
            api_url,
            relay_url,
            registration_ttl_secs: DEFAULT_REGISTRATION_TTL_SECS,
            // Refresh at half the TTL to ensure we don't expire
            refresh_interval_secs: DEFAULT_REGISTRATION_TTL_SECS / 2,
            lookup_cache_ttl_secs: DEFAULT_LOOKUP_CACHE_TTL_SECS,
        }
    }

    /// Set custom TTL for registrations
    pub fn with_ttl(mut self, ttl_secs: u64) -> Self {
        self.registration_ttl_secs = ttl_secs;
        self.refresh_interval_secs = ttl_secs / 2;
        self
    }

    /// Set custom TTL for cached lookups, where 0 disables the cache.
    ///
    /// Keep this well below the registration TTL, otherwise a lookup can be
    /// served from cache after the registration behind it has expired.
    pub fn with_lookup_cache_ttl(mut self, ttl_secs: u64) -> Self {
        self.lookup_cache_ttl_secs = ttl_secs;
        self
    }
}

/// A namespace lookup outcome worth remembering for a short while
#[derive(Debug, Clone, PartialEq, Eq)]
enum CachedLookup {
    Found(Url),
    NotFound,
}

impl CachedLookup {
    fn into_result(
        self,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)> {
        match self {
            Self::Found(url) => Ok((NamespaceOrigin::new(namespace.clone(), url, None), None)),
            Self::NotFound => Err(CoordinatorError::NamespaceNotFound),
        }
    }
}

struct CacheEntry {
    lookup: CachedLookup,
    expires_at: Instant,
}

/// Short-lived cache in front of the moq-api lookup endpoint.
///
/// A relay looks a namespace up once per track it cannot serve locally, so a
/// subscriber asking for several tracks of the same namespace produces that
/// many identical requests, each on the critical path of a cold subscribe.
///
/// The TTL is the only invalidation: it is also what bounds how long a lookup
/// can point at a relay that has stopped serving the namespace, so it is kept
/// far below the registration TTL rather than being explicitly purged.
///
/// Misses are not coalesced — concurrent misses for the same namespace each
/// reach the API, and the first response to land populates the entry.
struct LookupCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    ttl: Duration,
    negative_ttl: Duration,
}

impl LookupCache {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            // A namespace that does not exist yet is far more likely to show up
            // than a live one is to move, so misses are forgotten much sooner.
            negative_ttl: ttl / NEGATIVE_LOOKUP_TTL_DIVISOR,
        }
    }

    /// Look up a cached outcome, dropping it if it has expired
    fn get(&self, key: &str) -> Option<CachedLookup> {
        let now = Instant::now();
        let mut entries = self.lock();

        match entries.get(key) {
            Some(entry) if entry.expires_at > now => Some(entry.lookup.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Remember an outcome until its TTL elapses; a zero TTL caches nothing
    fn insert(&self, key: &str, lookup: CachedLookup) {
        let ttl = match lookup {
            CachedLookup::Found(_) => self.ttl,
            CachedLookup::NotFound => self.negative_ttl,
        };

        if ttl.is_zero() {
            return;
        }

        let now = Instant::now();
        let mut entries = self.lock();

        // Expired entries are otherwise only dropped when looked up again, so a
        // relay seeing many distinct namespaces would accumulate them forever.
        if entries.len() >= LOOKUP_CACHE_SWEEP_THRESHOLD {
            entries.retain(|_, entry| entry.expires_at > now);
        }

        entries.insert(
            key.to_owned(),
            CacheEntry {
                lookup,
                expires_at: now + ttl,
            },
        );
    }

    /// The cache holds no invariant a panic could break, so recover from
    /// poisoning instead of losing lookups for the rest of the process.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.entries.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

/// Handle that unregisters a namespace when dropped and manages TTL refresh
struct NamespaceUnregisterHandle {
    namespace_key: String,
    client: Client,
    /// Channel to signal the refresh task to stop (wrapped in Option so we can take it in drop)
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for NamespaceUnregisterHandle {
    fn drop(&mut self) {
        // Signal the refresh task to stop
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        let namespace_key = self.namespace_key.clone();
        let client = self.client.clone();

        // Spawn a task to unregister since we can't do async in drop
        tokio::spawn(async move {
            if let Err(err) = unregister_namespace_async(&client, &namespace_key).await {
                tracing::warn!(namespace = %namespace_key, error = %err, "failed to unregister namespace on drop: {}", err);
            }
        });
    }
}

/// Async helper for unregistering a namespace
async fn unregister_namespace_async(client: &Client, namespace_key: &str) -> Result<()> {
    tracing::debug!(namespace = %namespace_key, "unregistering namespace from API: {}", namespace_key);

    client
        .delete_origin(namespace_key)
        .await
        .context("failed to delete namespace from API")?;

    Ok(())
}

/// A coordinator that uses moq-api for state storage.
///
/// Multiple relay instances can connect to the same moq-api server to
/// coordinate namespace registration and discovery. Features:
///
/// - HTTP-based registration and lookup
/// - TTL-based automatic expiration of stale registrations
/// - Background refresh tasks to maintain registrations
/// - A short-lived lookup cache, so repeat subscribes skip the API
///
/// # Scope handling
///
/// Registry keys encode the scope and namespace into a single collision-free
/// string. Namespace tuple fields are hex-encoded to handle arbitrary bytes
/// (MoQT namespaces are tuples of byte arrays, not strings). See
/// [`ApiCoordinator::registry_key()`] for format details.
pub struct ApiCoordinator {
    /// moq-api client
    client: Client,
    /// Configuration
    config: ApiCoordinatorConfig,
    /// Recently resolved namespace lookups
    lookups: LookupCache,
}

impl ApiCoordinator {
    /// Build the moq-api registry key for a namespace, scoped if applicable.
    ///
    /// The key unambiguously encodes `(scope, namespace)` into a single string
    /// that can be used as an opaque key in the moq-api HTTP registry.
    ///
    /// ## Format
    ///
    /// Each namespace tuple field is hex-encoded and fields are joined with `.`.
    /// The scope (if present) is prepended with a `:` separator:
    ///
    /// - Scoped:   `"{scope}:{hex_field0}.{hex_field1}..."`
    /// - Unscoped: `":{hex_field0}.{hex_field1}..."`
    ///
    /// ## Why this is collision-free
    ///
    /// - Hex encoding (`[0-9a-f]`) preserves arbitrary bytes without ambiguity
    /// - `.` separates tuple fields (can't appear in hex output)
    /// - `:` separates scope from namespace (can't appear in hex output, and
    ///   scopes are validated connection paths that don't contain `:`)
    /// - The leading `:` on unscoped keys prevents collision with scoped keys
    ///   (scopes always start with `/` per `normalize_connection_path`)
    /// - Different tuple field counts produce different keys (e.g., one field
    ///   `"ab"` → `"6162"` vs two fields `"a","b"` → `"61.62"`)
    fn registry_key(scope: Option<&str>, namespace: &TrackNamespace) -> String {
        let ns_hex: String = namespace
            .fields
            .iter()
            .map(|f| hex::encode(&f.value))
            .collect::<Vec<_>>()
            .join(".");
        match scope {
            Some(s) => format!("{s}:{ns_hex}"),
            None => format!(":{ns_hex}"),
        }
    }

    /// Create a new API-based coordinator.
    ///
    /// # Arguments
    /// * `config` - Configuration for the API coordinator
    ///
    /// # Returns
    /// A new `ApiCoordinator` instance
    pub fn new(config: ApiCoordinatorConfig) -> Self {
        let client = Client::new(config.api_url.clone());
        let lookups = LookupCache::new(Duration::from_secs(config.lookup_cache_ttl_secs));

        Self {
            client,
            config,
            lookups,
        }
    }

    /// Start a background task to refresh namespace registration
    fn start_refresh_task(
        client: Client,
        namespace_key: String,
        relay_url: Url,
        refresh_interval: Duration,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(refresh_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let origin = Origin { url: relay_url.clone() };

                        match client.patch_origin(&namespace_key, origin).await {
                            Ok(()) => {
                                tracing::trace!(namespace = %namespace_key, "refreshed namespace registration: {}", namespace_key);
                            }
                            Err(err) => {
                                tracing::warn!(namespace = %namespace_key, error = %err, "failed to refresh namespace registration: {}", err);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::debug!("namespace refresh task shutting down");
                        break;
                    }
                }
            }
        });
    }
}

#[async_trait]
impl Coordinator for ApiCoordinator {
    async fn register_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<NamespaceRegistration> {
        let namespace_str = Self::registry_key(scope, namespace);
        let origin = Origin {
            url: self.config.relay_url.clone(),
        };

        tracing::info!(
            namespace = %namespace_str,
            relay_url = %self.config.relay_url,
            "registering namespace in API: {} -> {}",
            namespace_str,
            self.config.relay_url
        );

        // Register the namespace with the API
        self.client
            .set_origin(&namespace_str, origin)
            .await
            .context("failed to register namespace in API")
            .map_err(CoordinatorError::Other)?;

        // Create shutdown channel for the refresh task
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        // Start background refresh task
        Self::start_refresh_task(
            self.client.clone(),
            namespace_str.clone(),
            self.config.relay_url.clone(),
            Duration::from_secs(self.config.refresh_interval_secs),
            shutdown_rx,
        );

        let handle = NamespaceUnregisterHandle {
            namespace_key: namespace_str,
            client: self.client.clone(),
            shutdown_tx: Some(shutdown_tx),
        };

        Ok(NamespaceRegistration::new(handle))
    }

    async fn unregister_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<()> {
        let namespace_str = Self::registry_key(scope, namespace);
        tracing::info!(namespace = %namespace_str, "unregistering namespace from API: {}", namespace_str);

        self.client
            .delete_origin(&namespace_str)
            .await
            .context("failed to unregister namespace from API")
            .map_err(CoordinatorError::Other)?;

        Ok(())
    }

    async fn lookup(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)> {
        let namespace_str = Self::registry_key(scope, namespace);

        if let Some(cached) = self.lookups.get(&namespace_str) {
            tracing::trace!(namespace = %namespace_str, "serving cached namespace lookup: {}", namespace_str);
            return cached.into_result(namespace);
        }

        tracing::debug!(scope = scope.unwrap_or("<unscoped>"), namespace = %namespace_str, "looking up namespace in API: {}", namespace_str);

        // Query the API for the namespace
        let result = self
            .client
            .get_origin(&namespace_str)
            .await
            .context("failed to lookup namespace in API")
            .map_err(CoordinatorError::Other)?;

        // Only answers are cached; a failed request propagates above so that a
        // transient API error is retried by the next subscriber.
        let lookup = match result {
            Some(origin) => {
                tracing::debug!(namespace = %namespace_str, origin_url = %origin.url, "found namespace {} at {}", namespace_str, origin.url);
                CachedLookup::Found(origin.url)
            }
            None => {
                tracing::debug!(namespace = %namespace_str, "namespace not found: {}", namespace_str);
                CachedLookup::NotFound
            }
        };

        self.lookups.insert(&namespace_str, lookup.clone());

        lookup.into_result(namespace)
    }

    async fn shutdown(&self) -> CoordinatorResult<()> {
        tracing::info!("shutting down API coordinator");
        // The moq-api client uses reqwest which handles connection cleanup internally
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    const CACHE_TTL: Duration = Duration::from_secs(DEFAULT_LOOKUP_CACHE_TTL_SECS);

    fn origin() -> CachedLookup {
        CachedLookup::Found(Url::parse("https://origin.example.com").unwrap())
    }

    #[test]
    fn test_config_new() {
        let api_url = Url::parse("http://localhost:8080").unwrap();
        let relay_url = Url::parse("https://relay.example.com").unwrap();

        let config = ApiCoordinatorConfig::new(api_url.clone(), relay_url.clone());

        assert_eq!(config.api_url, api_url);
        assert_eq!(config.relay_url, relay_url);
        assert_eq!(config.registration_ttl_secs, DEFAULT_REGISTRATION_TTL_SECS);
        assert_eq!(
            config.refresh_interval_secs,
            DEFAULT_REGISTRATION_TTL_SECS / 2
        );
        assert_eq!(config.lookup_cache_ttl_secs, DEFAULT_LOOKUP_CACHE_TTL_SECS);
    }

    #[test]
    fn test_config_with_ttl() {
        let api_url = Url::parse("http://localhost:8080").unwrap();
        let relay_url = Url::parse("https://relay.example.com").unwrap();

        let config = ApiCoordinatorConfig::new(api_url, relay_url).with_ttl(120);

        assert_eq!(config.registration_ttl_secs, 120);
        assert_eq!(config.refresh_interval_secs, 60);
    }

    #[test]
    fn test_config_with_lookup_cache_ttl() {
        let api_url = Url::parse("http://localhost:8080").unwrap();
        let relay_url = Url::parse("https://relay.example.com").unwrap();

        let config = ApiCoordinatorConfig::new(api_url, relay_url).with_lookup_cache_ttl(0);

        assert_eq!(config.lookup_cache_ttl_secs, 0);
        assert_eq!(config.registration_ttl_secs, DEFAULT_REGISTRATION_TTL_SECS);
    }

    #[test]
    fn test_coordinator_is_shareable_across_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<ApiCoordinator>();
    }

    #[tokio::test(start_paused = true)]
    async fn test_cache_serves_a_repeat_lookup_within_the_ttl() {
        let cache = LookupCache::new(CACHE_TTL);
        cache.insert("ns", origin());

        tokio::time::sleep(CACHE_TTL / 2).await;

        assert_eq!(cache.get("ns"), Some(origin()));
    }

    #[tokio::test(start_paused = true)]
    async fn test_cache_forgets_an_entry_once_the_ttl_elapses() {
        let cache = LookupCache::new(CACHE_TTL);
        cache.insert("ns", origin());

        tokio::time::sleep(CACHE_TTL + Duration::from_millis(1)).await;

        assert_eq!(cache.get("ns"), None);
        assert_eq!(cache.len(), 0, "an expired entry should not be retained");
    }

    #[tokio::test(start_paused = true)]
    async fn test_cache_forgets_a_miss_sooner_than_a_hit() {
        let cache = LookupCache::new(CACHE_TTL);
        cache.insert("missing", CachedLookup::NotFound);
        cache.insert("present", origin());

        tokio::time::sleep(CACHE_TTL / NEGATIVE_LOOKUP_TTL_DIVISOR + Duration::from_millis(1))
            .await;

        // A namespace registered moments ago must not stay invisible.
        assert_eq!(cache.get("missing"), None);
        assert_eq!(cache.get("present"), Some(origin()));
    }

    #[tokio::test(start_paused = true)]
    async fn test_zero_ttl_disables_the_cache() {
        let cache = LookupCache::new(Duration::ZERO);
        cache.insert("present", origin());
        cache.insert("missing", CachedLookup::NotFound);

        assert_eq!(cache.get("present"), None);
        assert_eq!(cache.get("missing"), None);
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_cache_sweeps_expired_entries_as_it_grows() {
        let cache = LookupCache::new(CACHE_TTL);

        for i in 0..LOOKUP_CACHE_SWEEP_THRESHOLD {
            cache.insert(&format!("ns-{i}"), origin());
        }
        assert_eq!(cache.len(), LOOKUP_CACHE_SWEEP_THRESHOLD);

        tokio::time::sleep(CACHE_TTL + Duration::from_millis(1)).await;
        cache.insert("fresh", origin());

        assert_eq!(cache.len(), 1, "expired entries should not accumulate");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_cache_is_shared_between_concurrent_lookups() {
        let cache = Arc::new(LookupCache::new(CACHE_TTL));

        let tasks: Vec<_> = (0..32)
            .map(|i| {
                let cache = cache.clone();
                tokio::spawn(async move {
                    let key = format!("ns-{}", i % 4);
                    cache.insert(&key, origin());
                    cache.get(&key)
                })
            })
            .collect();

        for task in tasks {
            assert_eq!(task.await.unwrap(), Some(origin()));
        }
        assert_eq!(cache.len(), 4);
    }
}
