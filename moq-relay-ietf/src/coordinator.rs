// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::net::SocketAddr;

use async_trait::async_trait;
use moq_native_ietf::quic;
use moq_transport::coding::{TrackNamespace, TrackNamespacePrefix};
use url::Url;

use crate::session::SessionInterface;

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("namespace not found")]
    NamespaceNotFound,

    #[error("namespace already registered")]
    NamespaceAlreadyRegistered,

    #[error("Internal Error: {0}")]
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CoordinatorError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

impl From<tokio::task::JoinError> for CoordinatorError {
    fn from(err: tokio::task::JoinError) -> Self {
        Self::Other(err.into())
    }
}

impl From<std::io::Error> for CoordinatorError {
    fn from(err: std::io::Error) -> Self {
        Self::Other(err.into())
    }
}

pub type CoordinatorResult<T> = std::result::Result<T, CoordinatorError>;

/// Handle returned when a namespace is registered with the coordinator.
///
/// Dropping this handle automatically unregisters the namespace.
/// This provides RAII-based cleanup - when the publisher disconnects
/// or the namespace is no longer served, cleanup happens automatically.
pub struct NamespaceRegistration {
    _inner: Box<dyn Send + Sync>,
    _metadata: Option<Vec<(String, String)>>,
}

impl NamespaceRegistration {
    /// Create a new registration handle wrapping any Send + Sync type.
    ///
    /// The wrapped value's `Drop` implementation will be called when
    /// this registration is dropped.
    pub fn new<T: Send + Sync + 'static>(inner: T) -> Self {
        Self {
            _inner: Box::new(inner),
            _metadata: None,
        }
    }

    /// Add metadata as list of key value pair of string: string
    pub fn with_metadata(mut self, metadata: Vec<(String, String)>) -> Self {
        self._metadata = Some(metadata);
        self
    }
}

/// Result of a namespace lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceOrigin {
    /// The namespace of the track belongs to
    namespace: TrackNamespace,
    /// The URL of the relay serving this namespace
    /// If the relay is not discoverable via this URL, use `socket_addr`
    /// But you still have to pass a valid URL because the TLS verification
    /// happens for hostname
    url: Url,
    /// The socket address of the relay if the relay is not approachable
    /// via DNS lookup, This is to bypass DNS lookups.
    socket_addr: Option<SocketAddr>,
    /// Additional metadata associated with this namespace
    metadata: Option<Vec<(String, String)>>,
}

impl NamespaceOrigin {
    /// Create a new NamespaceOrigin.
    pub fn new(namespace: TrackNamespace, url: Url, addr: Option<SocketAddr>) -> Self {
        Self {
            namespace,
            url,
            socket_addr: addr,
            metadata: None,
        }
    }
    pub fn with_metadata(mut self, values: (String, String)) -> Self {
        if let Some(metadata) = &mut self.metadata {
            metadata.push(values);
        } else {
            self.metadata = Some(vec![values]);
        }
        self
    }

    /// Get the namespace.
    pub fn namespace(&self) -> &TrackNamespace {
        &self.namespace
    }

    /// Get the URL of the relay serving this namespace.
    pub fn url(&self) -> Url {
        self.url.clone()
    }

    pub fn addr(&self) -> Option<SocketAddr> {
        self.socket_addr
    }

    /// Get the metadata associated with this namespace.
    pub fn metadata(&self) -> Option<Vec<(String, String)>> {
        self.metadata.clone()
    }
}

/// Information about the resolved scope for a connection.
///
/// Returned by [`Coordinator::resolve_scope()`] to tell the relay:
/// - Which scope this connection belongs to (for routing and namespace isolation)
/// - What the connection is allowed to do (for permission enforcement)
///
/// Multiple connection paths can map to the same `scope_id` — for example,
/// a publisher path and a subscriber path that share a scope but have
/// different permissions.
#[derive(Debug, Clone)]
pub struct ScopeInfo {
    /// The resolved scope identity. Used as the key for namespace
    /// registration and lookup in all subsequent coordinator operations.
    ///
    /// Multiple connection paths can map to the same `scope_id`.
    pub scope_id: String,

    /// What this connection is allowed to do within the scope.
    pub permissions: ScopePermissions,
}

/// Permissions granted to a connection within its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopePermissions {
    /// Can both publish (PUBLISH_NAMESPACE) and subscribe (SUBSCRIBE/FETCH).
    ReadWrite,
    /// Can subscribe/fetch only. Publishing attempts will be rejected
    /// by the relay (the Consumer side of the session will not be created).
    ReadOnly,
}

impl ScopePermissions {
    /// Whether this permission level allows publishing (PUBLISH_NAMESPACE).
    pub fn can_publish(&self) -> bool {
        matches!(self, Self::ReadWrite)
    }

    /// Whether this permission level allows subscribing (SUBSCRIBE/FETCH).
    ///
    /// Always returns `true` — both `ReadWrite` and `ReadOnly` connections
    /// can subscribe. This is intentional: the asymmetry with [`can_publish()`]
    /// reflects that subscribing is the baseline capability, while publishing
    /// requires elevated permissions. If a future permission level needs to
    /// deny subscribing, a new variant should be added.
    ///
    /// [`can_publish()`]: ScopePermissions::can_publish
    pub fn can_subscribe(&self) -> bool {
        true
    }
}

// ============================================================================
// Types for extended Coordinator functionality
// ============================================================================

/// Per-scope configuration retrieved from the coordinator.
///
/// Called after [`Coordinator::resolve_scope()`] to get operational parameters
/// for the scope. This configuration applies to all sessions within the scope.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ScopeConfig {
    /// Origin server to fall back to when namespace not found locally or on
    /// other relays. The relay will attempt to subscribe from this origin
    /// before returning "not found" to the subscriber.
    pub origin_fallback: Option<Url>,

    /// Whether to pre-register subscriber interest for tracks that don't exist
    /// yet. When true, enables "subscriber-first" workflows where subscribers
    /// can wait for publishers that haven't connected yet.
    ///
    /// This corresponds to the "rendezvous" concept in the MoQT specification
    /// (`RENDEZVOUS_TIMEOUT` parameter, see moq-transport PR #1447). The
    /// "lingering subscribe" terminology from moq-transport issue #1402 is
    /// used here for consistency with existing implementations.
    ///
    /// Future: A `rendezvous_timeout` field may be added to control how long
    /// the relay waits for a publisher before giving up.
    pub lingering_subscribe: bool,
}

/// Result of subscribing to a namespace prefix via SUBSCRIBE_NAMESPACE.
///
/// The subscription remains active until this handle is dropped.
/// On drop, cleanup is performed (e.g., unregistering from the coordinator).
pub struct NamespaceSubscription {
    /// Peer relays that currently host namespaces matching the subscribed
    /// prefix. The subscribing relay opens upstream SUBSCRIBE_NAMESPACE
    /// sessions to these to receive live NAMESPACE / NAMESPACE_DONE updates.
    /// The coordinator excludes the caller and the inbound source peer, so
    /// every entry is a distinct upstream that is safe to pull from.
    ///
    /// The matching namespaces themselves are not returned here: they are
    /// delivered live over the upstream pull sessions (cross-relay) and served
    /// from relay-local state (same-relay), so a static snapshot would be both
    /// redundant and unable to reflect later withdrawals.
    pub upstream_relays: Vec<RelayInfo>,

    /// RAII handle — drop triggers unsubscription cleanup.
    _registration: Box<dyn Send + Sync>,
}

impl Default for NamespaceSubscription {
    fn default() -> Self {
        Self {
            upstream_relays: vec![],
            _registration: Box::new(()),
        }
    }
}

impl NamespaceSubscription {
    /// Create a new subscription with upstream relays and a cleanup handle.
    pub fn new<T: Send + Sync + 'static>(upstream_relays: Vec<RelayInfo>, inner: T) -> Self {
        Self {
            upstream_relays,
            _registration: Box::new(inner),
        }
    }
}

/// Information about a relay to forward messages to.
///
/// Returned by [`Coordinator::lookup_namespace_subscribers()`] and
/// [`Coordinator::lookup_track_subscribers()`] to tell the relay where
/// to forward PUBLISH_NAMESPACE or track availability notifications.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RelayInfo {
    /// Relay URL (used for TLS SNI and connection establishment).
    pub url: Url,

    /// Optional direct socket address (bypasses DNS resolution).
    pub addr: Option<SocketAddr>,
}

impl RelayInfo {
    /// Create a new RelayInfo with URL only.
    pub fn new(url: Url) -> Self {
        Self { url, addr: None }
    }

    /// Create a new RelayInfo with URL and direct socket address.
    pub fn with_addr(url: Url, addr: SocketAddr) -> Self {
        Self {
            url,
            addr: Some(addr),
        }
    }

    /// Create a `RelayInfo` identity from an inbound peer's socket address.
    ///
    /// Used when a relay accepts an internal connection and only knows the
    /// peer by its transport-level source address — it trusts the incoming
    /// socket address rather than asking the coordinator to resolve the
    /// peer's canonical URL. The [`url`](Self::url) is a synthetic
    /// `https://{addr}` identity; callers should treat [`addr`](Self::addr)
    /// as the authoritative field and must not assume the URL is dialable.
    pub fn from_socket_addr(addr: SocketAddr) -> Self {
        // A `SocketAddr` always renders as a valid `host:port` authority
        // (IPv6 is bracketed by its `Display`), so this parse is infallible.
        let url = Url::parse(&format!("https://{addr}"))
            .expect("socket address forms a valid https authority");
        Self {
            url,
            addr: Some(addr),
        }
    }
}

/// Per-call context describing how a coordinator operation reached this relay.
///
/// Passed alongside `scope` to routing/registration methods so a coordinator
/// can distinguish public client traffic from internal relay-to-relay traffic
/// and, for the latter, know which peer relay it came from.
///
/// The relay's own URL (the "caller") is intentionally **not** part of this
/// context: each coordinator instance is constructed knowing its own relay
/// URL, so it does not need to be told on every call.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CoordinatorContext {
    /// Whether the originating session is public client-facing or internal
    /// relay-to-relay. Defaults to [`SessionInterface::Public`].
    pub interface: SessionInterface,

    /// The peer relay this operation was received from, for internal
    /// sessions. `None` for public client sessions.
    ///
    /// Derived by the relay from the inbound socket address (see
    /// [`RelayInfo::from_socket_addr`]); the coordinator is not asked to
    /// resolve peer identity.
    pub source: Option<RelayInfo>,
}

impl CoordinatorContext {
    /// A public, client-facing context with no peer relay.
    pub fn public() -> Self {
        Self {
            interface: SessionInterface::Public,
            source: None,
        }
    }

    /// An internal, relay-to-relay context originating from `source`.
    pub fn internal(source: Option<RelayInfo>) -> Self {
        Self {
            interface: SessionInterface::Internal,
            source,
        }
    }
}

/// Handle returned when a track is registered with the coordinator.
///
/// Dropping this handle automatically unregisters the track.
/// This provides RAII-based cleanup for track-level PUBLISH.
pub struct TrackRegistration {
    _registration: Box<dyn Send + Sync>,
}

impl Default for TrackRegistration {
    fn default() -> Self {
        Self {
            _registration: Box::new(()),
        }
    }
}

impl TrackRegistration {
    /// Create a new track registration handle wrapping any Send + Sync type.
    pub fn new<T: Send + Sync + 'static>(inner: T) -> Self {
        Self {
            _registration: Box::new(inner),
        }
    }
}

/// Information about a registered track.
///
/// Returned by [`Coordinator::list_tracks()`] to describe tracks
/// registered under a namespace.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TrackEntry {
    /// The namespace this track belongs to.
    pub namespace: TrackNamespace,

    /// The track name within the namespace.
    pub name: String,
}

impl TrackEntry {
    /// Create a new TrackEntry.
    pub fn new(namespace: TrackNamespace, name: String) -> Self {
        Self { namespace, name }
    }
}

/// Handle returned when subscribing to a track for rendezvous/lingering subscriber support.
///
/// Dropping this handle automatically unregisters the track subscription.
/// This provides RAII-based cleanup for pre-registered subscriber interest
/// (the "rendezvous" concept from MoQT's `RENDEZVOUS_TIMEOUT` parameter).
pub struct TrackSubscription {
    _registration: Box<dyn Send + Sync>,
}

impl Default for TrackSubscription {
    fn default() -> Self {
        Self {
            _registration: Box::new(()),
        }
    }
}

impl TrackSubscription {
    /// Create a new track subscription handle wrapping any Send + Sync type.
    pub fn new<T: Send + Sync + 'static>(inner: T) -> Self {
        Self {
            _registration: Box::new(inner),
        }
    }
}

/// Coordinator handles namespace registration/discovery across relays.
///
/// Implementations are responsible for:
/// - Resolving connection paths to scopes (identity + permissions)
/// - Tracking which namespaces are served locally
/// - Caching remote namespace lookups
/// - Communicating with external registries (HTTP API, Redis, etc.)
/// - Periodic refresh/heartbeat of registrations
/// - Cleanup when registrations are dropped
///
/// # Thread Safety
///
/// All methods take `&self` and implementations must be thread-safe.
/// Multiple tasks will call these methods concurrently.
///
/// ## Scope Resolution
///
/// When a new session is accepted, the relay calls [`resolve_scope()`] with
/// the raw connection path (from WebTransport URL or CLIENT_SETUP PATH
/// parameter). The coordinator returns a [`ScopeInfo`] containing:
///
/// - **`scope_id`**: The resolved scope identity, used as the key for all
///   subsequent `register_namespace()` and `lookup()` calls. This is
///   intentionally separate from the raw connection path — multiple paths
///   can map to the same scope.
///
/// - **`permissions`**: What the connection is allowed to do. The relay
///   enforces permissions by selectively enabling the publish and/or
///   subscribe sides of the session.
///
/// If `resolve_scope()` returns `None`, the session is unscoped — all
/// subsequent operations use `scope: None` and both publish and subscribe
/// are allowed.
///
/// [`resolve_scope()`]: Coordinator::resolve_scope
#[async_trait]
pub trait Coordinator: Send + Sync {
    /// Resolve a connection path to scope information.
    ///
    /// Called once per accepted session, before any register/lookup calls.
    /// The relay uses the returned [`ScopeInfo`] to:
    /// - Scope all subsequent coordinator operations to `scope_id`
    /// - Enforce permissions (e.g., skip creating the publish side for
    ///   `ReadOnly` connections)
    ///
    /// # Arguments
    ///
    /// * `connection_path` - The raw connection path from the WebTransport
    ///   URL or CLIENT_SETUP PATH parameter. `None` if no path was present.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(ScopeInfo))` - Connection is scoped with the given
    ///   identity and permissions.
    /// - `Ok(None)` - Connection is unscoped. The relay will pass
    ///   `scope: None` to all subsequent coordinator calls and allow
    ///   both publish and subscribe.
    /// - `Err(...)` - Connection should be rejected (e.g., unrecognized
    ///   path, unauthorized).
    ///
    /// # Default Implementation
    ///
    /// Passes through the connection path as the `scope_id` with
    /// `ReadWrite` permissions. Connections without a path are unscoped.
    async fn resolve_scope(
        &self,
        connection_path: Option<&str>,
    ) -> CoordinatorResult<Option<ScopeInfo>> {
        Ok(connection_path.map(|path| ScopeInfo {
            scope_id: path.to_string(),
            permissions: ScopePermissions::ReadWrite,
        }))
    }

    /// Register a namespace as locally available on this relay.
    ///
    /// Called when a publisher sends PUBLISH_NAMESPACE.
    /// The coordinator should:
    /// 1. Record the namespace as locally available
    /// 2. Advertise to external registry if configured
    /// 3. Start any refresh/heartbeat tasks
    /// 4. Return a handle that unregisters on drop
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity from [`resolve_scope()`],
    ///   or `None` for unscoped sessions. Used to isolate namespace
    ///   registrations — the same namespace in different scopes may
    ///   route independently.
    /// * `namespace` - The namespace being registered
    /// * `context` - Whether the registering session is a public client or an
    ///   internal relay peer (and which peer). Coordinators may use this to
    ///   avoid, for example, re-advertising registrations that arrived from
    ///   another relay.
    ///
    /// # Returns
    ///
    /// A `NamespaceRegistration` handle. The namespace remains registered
    /// as long as this handle is held. Dropping it unregisters the namespace.
    ///
    /// [`resolve_scope()`]: Coordinator::resolve_scope
    async fn register_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        context: &CoordinatorContext,
    ) -> CoordinatorResult<NamespaceRegistration>;

    /// Unregister a namespace.
    ///
    /// Called when a publisher sends PUBLISH_NAMESPACE_DONE.
    /// This is an explicit unregistration - the registration handle may still exist
    /// but the namespace should be removed from the registry.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace to unregister
    async fn unregister_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<()>;

    /// Lookup where a namespace is served from.
    ///
    /// Called when a subscriber requests a namespace.
    /// The coordinator should check in order:
    /// 1. Local registrations (return `Local`)
    /// 2. Cached remote lookups (return `Remote(url)` if not expired)
    /// 3. External registry (cache and return result)
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped
    ///   sessions. Coordinators use this to scope lookups (e.g., to route
    ///   to the correct origin for a particular application).
    /// * `namespace` - The namespace to look up
    ///
    /// # Returns
    ///
    /// - `Ok(NamespaceOrigin, Option<quic::Client>)` - Namespace origin and optional client if available
    /// - `Err` - Namespace not found anywhere
    async fn lookup(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)>;

    /// Lookup where a specific track is served from.
    ///
    /// Called when a subscriber requests a full track name. Implementations
    /// should prefer exact track registrations created by PUBLISH, then fall
    /// back to namespace routing created by PUBLISH_NAMESPACE. This keeps the
    /// common caller ergonomic while still allowing a single PUBLISH track to be
    /// routed without pretending the whole namespace was published.
    ///
    /// # Default Implementation
    ///
    /// Falls back to [`lookup`], so existing coordinators that only implement
    /// namespace-level routing retain their current behavior.
    async fn lookup_track(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        _track: &str,
    ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)> {
        self.lookup(scope, namespace).await
    }

    /// Graceful shutdown of the coordinator.
    ///
    /// Called when the relay is shutting down. Implementations should:
    /// - Unregister all local namespaces and tracks
    /// - Cancel refresh tasks
    /// - Close connections to external registries
    async fn shutdown(&self) -> CoordinatorResult<()> {
        Ok(())
    }

    // ========================================================================
    // Scope configuration
    // ========================================================================

    /// Get configuration for a resolved scope.
    ///
    /// Called after [`resolve_scope()`] to retrieve operational parameters
    /// for the scope, such as origin fallback URLs and lingering subscriber
    /// settings.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity from [`resolve_scope()`],
    ///   or `None` for unscoped sessions.
    ///
    /// # Default Implementation
    ///
    /// Returns default configuration (no origin fallback, lingering subscribe
    /// disabled).
    ///
    /// [`resolve_scope()`]: Coordinator::resolve_scope
    async fn get_scope_config(&self, _scope: Option<&str>) -> CoordinatorResult<ScopeConfig> {
        Ok(ScopeConfig::default())
    }

    // ========================================================================
    // SUBSCRIBE_NAMESPACE support
    // ========================================================================

    /// Register interest in a namespace prefix (SUBSCRIBE_NAMESPACE).
    ///
    /// Called when a subscriber sends SUBSCRIBE_NAMESPACE. The coordinator
    /// should:
    /// 1. Record that this relay is interested in the prefix
    /// 2. Return the peer relays that host currently-matching namespaces (the
    ///    upstream pull targets)
    /// 3. Return an RAII handle for cleanup on disconnect
    ///
    /// The subscribing relay opens upstream SUBSCRIBE_NAMESPACE sessions to the
    /// returned [`NamespaceSubscription::upstream_relays`] to receive live
    /// NAMESPACE / NAMESPACE_DONE updates for namespaces that already exist
    /// (the publish-before-subscribe case). Separately, when publishers *later*
    /// register namespaces matching this prefix, the origin relay uses
    /// [`lookup_namespace_subscribers()`] to find interested relays and push
    /// PUBLISH_NAMESPACE to them (the subscribe-before-publish case).
    ///
    /// The subscribing relay applies **no** routing or topology policy of its
    /// own and does not interpret the session interface: it simply opens an
    /// upstream pull to each relay in `upstream_relays`, and an empty list means
    /// "serve from local state only, do not connect upstream". All routing
    /// policy therefore lives in the coordinator.
    ///
    /// To decide what to return, the coordinator MAY use the informational
    /// [`CoordinatorContext`] — in particular the session
    /// [`interface`][CoordinatorContext::interface] (public client vs. internal
    /// relay peer) and the [`source`][CoordinatorContext::source] peer. For
    /// example, the built-in implementations exclude both the caller (this
    /// relay's own endpoint) and the source peer, so a relay is never told to
    /// pull from itself or from the peer the SUBSCRIBE_NAMESPACE just arrived
    /// from; and they return an empty list for internal sessions, so a pull
    /// opened on another relay's behalf does not itself fan out upstream (which
    /// would otherwise make relays recurse — and, in a mesh, cycle). Those are
    /// policy choices of the built-in implementations, not requirements of this
    /// trait. Interest is still recorded for the push path regardless of
    /// interface.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `prefix` - The namespace prefix to subscribe to.
    /// * `context` - Whether the subscribing session is a public client or an
    ///   internal relay peer (and which peer).
    ///
    /// # Default Implementation
    ///
    /// Returns an empty subscription (no upstream relays, no-op cleanup).
    ///
    /// [`lookup_namespace_subscribers()`]: Coordinator::lookup_namespace_subscribers
    async fn subscribe_namespace(
        &self,
        _scope: Option<&str>,
        _prefix: &TrackNamespacePrefix,
        _context: &CoordinatorContext,
    ) -> CoordinatorResult<NamespaceSubscription> {
        Ok(NamespaceSubscription::default())
    }

    /// Unregister interest in a namespace prefix (UNSUBSCRIBE_NAMESPACE).
    ///
    /// Called when a subscriber sends UNSUBSCRIBE_NAMESPACE or disconnects.
    /// This is an explicit unregistration — the subscription handle may still
    /// exist but interest should be removed from the registry.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `prefix` - The namespace prefix to unsubscribe from.
    ///
    /// # Default Implementation
    ///
    /// No-op (returns success).
    async fn unsubscribe_namespace(
        &self,
        _scope: Option<&str>,
        _prefix: &TrackNamespacePrefix,
    ) -> CoordinatorResult<()> {
        Ok(())
    }

    /// Find relays interested in a namespace (reverse lookup).
    ///
    /// Called when a publisher registers a new namespace. The relay uses this
    /// to find other relays that have active SUBSCRIBE_NAMESPACE subscriptions
    /// matching this namespace, then forwards PUBLISH_NAMESPACE to them.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The newly-registered namespace.
    /// * `context` - Origin of the PUBLISH_NAMESPACE that triggered this lookup.
    ///   Implementations MUST use this (together with their own relay identity)
    ///   to exclude relays that would create forwarding loops:
    ///   - the **caller** (this relay's own endpoint), so a relay never forwards
    ///     PUBLISH_NAMESPACE to itself; and
    ///   - the **source** ([`CoordinatorContext::source`]), so a PUBLISH_NAMESPACE
    ///     is never echoed back to the peer relay it just arrived from.
    ///
    /// # Returns
    ///
    /// List of peer relay endpoints to forward PUBLISH_NAMESPACE to, with the
    /// caller and source excluded.
    ///
    /// # Default Implementation
    ///
    /// Returns an empty list (no subscribers).
    async fn lookup_namespace_subscribers(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
        _context: &CoordinatorContext,
    ) -> CoordinatorResult<Vec<RelayInfo>> {
        Ok(vec![])
    }

    // ========================================================================
    // Track-level PUBLISH support
    // ========================================================================

    /// Register a track as available on this relay (track-level PUBLISH).
    ///
    /// Called when a publisher sends PUBLISH for a specific track. This
    /// provides finer-grained routing than namespace-level registration.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace the track belongs to.
    /// * `track` - The track name within the namespace.
    ///
    /// # Returns
    ///
    /// A `TrackRegistration` handle. The track remains registered as long as
    /// this handle is held. Dropping it unregisters the track.
    ///
    /// # Default Implementation
    ///
    /// Returns a no-op registration handle.
    async fn register_track(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
        _track: &str,
    ) -> CoordinatorResult<TrackRegistration> {
        Ok(TrackRegistration::default())
    }

    /// Unregister a track.
    ///
    /// Called when a publisher sends PUBLISH_DONE or disconnects.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace the track belongs to.
    /// * `track` - The track name to unregister.
    ///
    /// # Default Implementation
    ///
    /// No-op (returns success).
    async fn unregister_track(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
        _track: &str,
    ) -> CoordinatorResult<()> {
        Ok(())
    }

    /// List tracks registered under a namespace.
    ///
    /// Used for track discovery within a namespace, supporting SUBSCRIBE_NAMESPACE
    /// workflows where subscribers need to know what tracks are available.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace to list tracks from.
    ///
    /// # Default Implementation
    ///
    /// Returns an empty list.
    async fn list_tracks(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
    ) -> CoordinatorResult<Vec<TrackEntry>> {
        Ok(vec![])
    }

    // ========================================================================
    // Lingering subscriber / rendezvous support
    // ========================================================================
    //
    // These methods implement the "rendezvous" concept from the MoQT
    // specification (RENDEZVOUS_TIMEOUT parameter, moq-transport PR #1447),
    // also known as "lingering subscribe" (moq-transport issue #1402) or
    // "early media" (Cisco's original framing at IETF 122).
    //
    // The relay uses these to pre-register subscriber interest before a
    // publisher exists, enabling subscriber-first workflows.
    //
    // Future: timeout handling for rendezvous — how long to wait before
    // giving up on a publisher.

    /// Pre-register interest in a track that may not exist yet (rendezvous).
    ///
    /// Enables "subscriber-first" workflows where a subscriber can wait for a
    /// publisher that hasn't connected yet. Called when
    /// [`ScopeConfig::lingering_subscribe`] is true and a subscriber requests
    /// a track that doesn't exist.
    ///
    /// Also known as "lingering subscribe" (moq-transport issue #1402) or
    /// "rendezvous" (MoQT spec's `RENDEZVOUS_TIMEOUT` parameter).
    ///
    /// When a publisher later registers the track, the relay uses
    /// [`lookup_track_subscribers()`] to find waiting subscribers.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace the track would belong to.
    /// * `track` - The track name to pre-register interest in.
    ///
    /// # Returns
    ///
    /// A `TrackSubscription` handle. Interest remains registered as long as
    /// this handle is held. Dropping it removes the interest.
    ///
    /// # Default Implementation
    ///
    /// Returns a no-op subscription handle.
    ///
    /// [`lookup_track_subscribers()`]: Coordinator::lookup_track_subscribers
    async fn subscribe_track(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
        _track: &str,
    ) -> CoordinatorResult<TrackSubscription> {
        Ok(TrackSubscription::default())
    }

    /// Unregister track subscription interest.
    ///
    /// Called when a subscriber disconnects or no longer needs the track.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace the track belongs to.
    /// * `track` - The track name to unsubscribe from.
    ///
    /// # Default Implementation
    ///
    /// No-op (returns success).
    async fn unsubscribe_track(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
        _track: &str,
    ) -> CoordinatorResult<()> {
        Ok(())
    }

    /// Find relays with subscribers waiting for a track (reverse lookup).
    ///
    /// Called when a publisher registers a track, to notify lingering
    /// subscribers that the track is now available.
    ///
    /// # Arguments
    ///
    /// * `scope` - The resolved scope identity, or `None` for unscoped sessions.
    /// * `namespace` - The namespace the track belongs to.
    /// * `track` - The track name.
    ///
    /// # Returns
    ///
    /// List of relay endpoints with waiting subscribers.
    ///
    /// # Default Implementation
    ///
    /// Returns an empty list.
    async fn lookup_track_subscribers(
        &self,
        _scope: Option<&str>,
        _namespace: &TrackNamespace,
        _track: &str,
    ) -> CoordinatorResult<Vec<RelayInfo>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ========================================================================
    // Test helpers and fixtures
    // ========================================================================

    /// Helper to build a TrackNamespace from slash-separated path segments.
    fn ns(path: &str) -> TrackNamespace {
        TrackNamespace::from_utf8_path(path)
    }

    fn prefix(path: &str) -> TrackNamespacePrefix {
        TrackNamespacePrefix::from_utf8_path(path)
    }

    /// Returns true if `namespace` starts with all the fields in `prefix`.
    fn ns_has_prefix(namespace: &TrackNamespace, prefix: &TrackNamespacePrefix) -> bool {
        prefix.is_prefix_of(namespace)
    }

    fn ns_has_namespace_prefix(namespace: &TrackNamespace, prefix: &TrackNamespace) -> bool {
        namespace.fields.len() >= prefix.fields.len()
            && prefix
                .fields
                .iter()
                .zip(namespace.fields.iter())
                .all(|(p, n)| p == n)
    }

    // --------------------------------------------------------------------
    // MockCoordinator — a fully in-memory Coordinator for testing
    //
    // The in-tree FileCoordinator and ApiCoordinator only implement the
    // three required methods (register_namespace, unregister_namespace,
    // lookup) and rely on defaults for all new stub methods. This mock
    // provides a complete reference implementation of the full trait,
    // including SUBSCRIBE_NAMESPACE, track-level PUBLISH, and lingering
    // subscriber support.
    //
    // It serves two purposes:
    //   1. Executable documentation of intended method semantics for
    //      external implementors who can't see the bin-only coordinators
    //   2. A test fixture that exercises non-trivial behavior for the new
    //      methods (which no existing coordinator implements yet)
    //
    // Test data models a broadcast/live-streaming scenario:
    //   - Scopes represent content providers or tenants
    //     (e.g., "content-provider-123")
    //   - Namespaces represent broadcast events or channels
    //     (e.g., "sports/football/match-42", "sports/football/match-42/camera-1")
    //   - Tracks represent individual media renditions
    //     (e.g., "video-1080p", "video-480p", "audio-en", "audio-es")
    //   - Multiple relays in a CDN cluster subscribe to namespace prefixes
    //     to discover new broadcasts and forward them to edge viewers
    // --------------------------------------------------------------------

    /// In-memory state for the mock coordinator.
    struct MockState {
        /// Maps scope → registered namespaces (keyed by TrackNamespace)
        /// Value is the relay URL that registered it.
        namespaces: HashMap<String, HashMap<TrackNamespace, String>>,

        /// Maps scope → registered tracks → relay URL
        /// Key: (namespace, track_name)
        tracks: HashMap<String, HashMap<(TrackNamespace, String), String>>,

        /// Maps scope → SUBSCRIBE_NAMESPACE prefixes → list of relay URLs
        namespace_subscribers: HashMap<String, HashMap<TrackNamespacePrefix, Vec<String>>>,

        /// Maps scope → subscribed tracks → list of relay URLs
        /// Key: (namespace, track_name)
        track_subscribers: HashMap<String, HashMap<(TrackNamespace, String), Vec<String>>>,

        /// Maps scope → ScopeConfig
        scope_configs: HashMap<String, ScopeConfig>,

        /// Maps raw connection path → ScopeInfo (for resolve_scope)
        path_to_scope: HashMap<String, ScopeInfo>,
    }

    impl MockState {
        fn scope_key(scope: Option<&str>) -> String {
            scope.unwrap_or("").to_string()
        }

        fn track_key(namespace: &TrackNamespace, track: &str) -> (TrackNamespace, String) {
            (namespace.clone(), track.to_string())
        }
    }

    /// Drop-based handle for namespace unregistration.
    struct MockNamespaceHandle {
        state: std::sync::Arc<Mutex<MockState>>,
        scope_key: String,
        namespace: TrackNamespace,
    }

    impl Drop for MockNamespaceHandle {
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.namespaces.get_mut(&self.scope_key) {
                bucket.remove(&self.namespace);
            }
        }
    }

    /// Drop-based handle for track unregistration.
    struct MockTrackHandle {
        state: std::sync::Arc<Mutex<MockState>>,
        scope_key: String,
        track_key: (TrackNamespace, String),
    }

    impl Drop for MockTrackHandle {
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.tracks.get_mut(&self.scope_key) {
                bucket.remove(&self.track_key);
            }
        }
    }

    /// Drop-based handle for namespace subscription cleanup.
    struct MockNamespaceSubHandle {
        state: std::sync::Arc<Mutex<MockState>>,
        scope_key: String,
        prefix: TrackNamespacePrefix,
        relay_url: String,
    }

    impl Drop for MockNamespaceSubHandle {
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.namespace_subscribers.get_mut(&self.scope_key) {
                if let Some(relays) = bucket.get_mut(&self.prefix) {
                    relays.retain(|r| r != &self.relay_url);
                }
            }
        }
    }

    /// Drop-based handle for track subscription cleanup.
    struct MockTrackSubHandle {
        state: std::sync::Arc<Mutex<MockState>>,
        scope_key: String,
        track_key: (TrackNamespace, String),
        relay_url: String,
    }

    impl Drop for MockTrackSubHandle {
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.track_subscribers.get_mut(&self.scope_key) {
                if let Some(relays) = bucket.get_mut(&self.track_key) {
                    relays.retain(|r| r != &self.relay_url);
                }
            }
        }
    }

    /// A mock coordinator that stores all state in memory.
    ///
    /// Provides a complete reference implementation of the Coordinator trait
    /// including all new stub methods. Useful for testing relay integration
    /// and as executable documentation of the intended method semantics.
    struct MockCoordinator {
        state: std::sync::Arc<Mutex<MockState>>,
        /// URL of "this" relay (used when registering namespaces/tracks)
        relay_url: Url,
    }

    impl MockCoordinator {
        fn new(relay_url: &str) -> Self {
            Self {
                state: std::sync::Arc::new(Mutex::new(MockState {
                    namespaces: HashMap::new(),
                    tracks: HashMap::new(),
                    namespace_subscribers: HashMap::new(),
                    track_subscribers: HashMap::new(),
                    scope_configs: HashMap::new(),
                    path_to_scope: HashMap::new(),
                })),
                relay_url: Url::parse(relay_url).unwrap(),
            }
        }

        /// Create another relay's view of the *same* shared oracle state.
        ///
        /// The returned coordinator shares this instance's registry but reports
        /// a different `relay_url`, modelling a distinct relay talking to the
        /// same coordinator (as separate relay processes do with a shared
        /// [`FileCoordinator`] file).
        fn peer(&self, relay_url: &str) -> Self {
            Self {
                state: self.state.clone(),
                relay_url: Url::parse(relay_url).unwrap(),
            }
        }

        /// Configure scope resolution: connection path → ScopeInfo.
        fn add_path_mapping(&self, path: &str, scope_id: &str, permissions: ScopePermissions) {
            let mut state = self.state.lock().unwrap();
            state.path_to_scope.insert(
                path.to_string(),
                ScopeInfo {
                    scope_id: scope_id.to_string(),
                    permissions,
                },
            );
        }

        /// Configure per-scope settings.
        fn set_scope_config(&self, scope: &str, config: ScopeConfig) {
            let mut state = self.state.lock().unwrap();
            state.scope_configs.insert(scope.to_string(), config);
        }
    }

    #[async_trait]
    impl Coordinator for MockCoordinator {
        async fn resolve_scope(
            &self,
            connection_path: Option<&str>,
        ) -> CoordinatorResult<Option<ScopeInfo>> {
            let state = self.state.lock().unwrap();
            match connection_path {
                Some(path) => {
                    state
                        .path_to_scope
                        .get(path)
                        .cloned()
                        .map(Some)
                        .ok_or(CoordinatorError::Other(anyhow::anyhow!(
                            "unknown path: {}",
                            path
                        )))
                }
                None => Ok(None),
            }
        }

        async fn register_namespace(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            _context: &CoordinatorContext,
        ) -> CoordinatorResult<NamespaceRegistration> {
            let scope_key = MockState::scope_key(scope);
            let relay_url = self.relay_url.to_string();

            {
                let mut state = self.state.lock().unwrap();
                let bucket = state.namespaces.entry(scope_key.clone()).or_default();
                if bucket.contains_key(namespace) {
                    return Err(CoordinatorError::NamespaceAlreadyRegistered);
                }
                bucket.insert(namespace.clone(), relay_url);
            }

            let handle = MockNamespaceHandle {
                state: self.state.clone(),
                scope_key,
                namespace: namespace.clone(),
            };
            Ok(NamespaceRegistration::new(handle))
        }

        async fn unregister_namespace(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
        ) -> CoordinatorResult<()> {
            let scope_key = MockState::scope_key(scope);
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.namespaces.get_mut(&scope_key) {
                bucket.remove(namespace);
            }
            Ok(())
        }

        async fn lookup(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
        ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)> {
            let scope_key = MockState::scope_key(scope);
            let state = self.state.lock().unwrap();

            let bucket = state
                .namespaces
                .get(&scope_key)
                .ok_or(CoordinatorError::NamespaceNotFound)?;

            // Exact match first
            if let Some(relay_url) = bucket.get(namespace) {
                let url = Url::parse(relay_url).unwrap();
                return Ok((NamespaceOrigin::new(namespace.clone(), url, None), None));
            }

            // Prefix match (longest wins)
            let mut best: Option<(&TrackNamespace, &String)> = None;
            for (registered, url) in bucket {
                if ns_has_namespace_prefix(namespace, registered) {
                    match &best {
                        Some((prev, _)) if registered.fields.len() > prev.fields.len() => {
                            best = Some((registered, url));
                        }
                        None => {
                            best = Some((registered, url));
                        }
                        _ => {}
                    }
                }
            }

            match best {
                Some((matched_ns, relay_url)) => {
                    let url = Url::parse(relay_url).unwrap();
                    Ok((NamespaceOrigin::new(matched_ns.clone(), url, None), None))
                }
                None => Err(CoordinatorError::NamespaceNotFound),
            }
        }

        async fn lookup_track(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<(NamespaceOrigin, Option<quic::Client>)> {
            let scope_key = MockState::scope_key(scope);
            let track_key = MockState::track_key(namespace, track);

            {
                let state = self.state.lock().unwrap();
                if let Some(relay_url) = state
                    .tracks
                    .get(&scope_key)
                    .and_then(|bucket| bucket.get(&track_key))
                {
                    let url = Url::parse(relay_url).unwrap();
                    return Ok((NamespaceOrigin::new(namespace.clone(), url, None), None));
                }
            }

            self.lookup(scope, namespace).await
        }

        async fn get_scope_config(&self, scope: Option<&str>) -> CoordinatorResult<ScopeConfig> {
            let state = self.state.lock().unwrap();
            let scope_key = MockState::scope_key(scope);
            Ok(state
                .scope_configs
                .get(&scope_key)
                .cloned()
                .unwrap_or_default())
        }

        async fn subscribe_namespace(
            &self,
            scope: Option<&str>,
            prefix: &TrackNamespacePrefix,
            context: &CoordinatorContext,
        ) -> CoordinatorResult<NamespaceSubscription> {
            let scope_key = MockState::scope_key(scope);
            let relay_url = self.relay_url.to_string();

            // Exclude the caller (this relay) and the inbound source peer so we
            // never return ourselves or the relay the SUBSCRIBE_NAMESPACE
            // arrived from as an upstream to pull from.
            let caller = relay_url.clone();
            let source = context.source.as_ref().map(|relay| relay.url.to_string());

            let mut state = self.state.lock().unwrap();

            // Find the distinct relays hosting namespaces that match the prefix
            // (the upstream pull targets), excluding the caller and source. Only
            // public sessions fan out: an internal (relay-to-relay) pull must
            // not recurse, so it is served from local state alone (empty
            // upstream_relays breaks the loop). Interest is still recorded below
            // for the push path regardless of interface.
            let mut upstream_relays = Vec::new();
            if context.interface == SessionInterface::Public {
                let mut seen_relays = std::collections::HashSet::new();
                if let Some(bucket) = state.namespaces.get(&scope_key) {
                    for (ns, hosting_relay) in bucket {
                        if !ns_has_prefix(ns, prefix) {
                            continue;
                        }
                        if hosting_relay == &caller || Some(hosting_relay) == source.as_ref() {
                            continue;
                        }
                        if seen_relays.insert(hosting_relay.clone()) {
                            upstream_relays
                                .push(RelayInfo::new(Url::parse(hosting_relay).unwrap()));
                        }
                    }
                }
            }

            // Register this relay as interested in the prefix
            state
                .namespace_subscribers
                .entry(scope_key.clone())
                .or_default()
                .entry(prefix.clone())
                .or_default()
                .push(relay_url.clone());

            let handle = MockNamespaceSubHandle {
                state: self.state.clone(),
                scope_key,
                prefix: prefix.clone(),
                relay_url,
            };

            Ok(NamespaceSubscription::new(upstream_relays, handle))
        }

        async fn unsubscribe_namespace(
            &self,
            scope: Option<&str>,
            prefix: &TrackNamespacePrefix,
        ) -> CoordinatorResult<()> {
            let scope_key = MockState::scope_key(scope);
            let relay_url = self.relay_url.to_string();
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.namespace_subscribers.get_mut(&scope_key) {
                if let Some(relays) = bucket.get_mut(prefix) {
                    relays.retain(|r| r != &relay_url);
                }
            }
            Ok(())
        }

        async fn lookup_namespace_subscribers(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            context: &CoordinatorContext,
        ) -> CoordinatorResult<Vec<RelayInfo>> {
            let scope_key = MockState::scope_key(scope);
            let state = self.state.lock().unwrap();

            // Exclude the caller (this relay) and the inbound source peer so a
            // forwarded PUBLISH_NAMESPACE is never sent to ourselves or echoed
            // back to the relay it just arrived from.
            let caller = self.relay_url.to_string();
            let source = context.source.as_ref().map(|relay| relay.url.to_string());

            let mut seen = std::collections::HashSet::new();
            let mut relays = Vec::new();
            if let Some(subs) = state.namespace_subscribers.get(&scope_key) {
                for (prefix, relay_urls) in subs {
                    if !ns_has_prefix(namespace, prefix) {
                        continue;
                    }
                    for url_str in relay_urls {
                        if url_str == &caller || Some(url_str) == source.as_ref() {
                            continue;
                        }
                        if seen.insert(url_str.clone()) {
                            relays.push(RelayInfo::new(Url::parse(url_str).unwrap()));
                        }
                    }
                }
            }
            Ok(relays)
        }

        async fn register_track(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<TrackRegistration> {
            let scope_key = MockState::scope_key(scope);
            let track_key = MockState::track_key(namespace, track);

            {
                let mut state = self.state.lock().unwrap();
                state
                    .tracks
                    .entry(scope_key.clone())
                    .or_default()
                    .insert(track_key.clone(), self.relay_url.to_string());
            }

            let handle = MockTrackHandle {
                state: self.state.clone(),
                scope_key,
                track_key,
            };
            Ok(TrackRegistration::new(handle))
        }

        async fn unregister_track(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<()> {
            let scope_key = MockState::scope_key(scope);
            let track_key = MockState::track_key(namespace, track);
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.tracks.get_mut(&scope_key) {
                bucket.remove(&track_key);
            }
            Ok(())
        }

        async fn list_tracks(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
        ) -> CoordinatorResult<Vec<TrackEntry>> {
            let scope_key = MockState::scope_key(scope);
            let state = self.state.lock().unwrap();

            let entries = state
                .tracks
                .get(&scope_key)
                .map(|bucket| {
                    bucket
                        .keys()
                        .filter_map(|(ns, track_name)| {
                            if ns == namespace {
                                Some(TrackEntry::new(ns.clone(), track_name.clone()))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            Ok(entries)
        }

        async fn subscribe_track(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<TrackSubscription> {
            let scope_key = MockState::scope_key(scope);
            let track_key = MockState::track_key(namespace, track);
            let relay_url = self.relay_url.to_string();

            {
                let mut state = self.state.lock().unwrap();
                state
                    .track_subscribers
                    .entry(scope_key.clone())
                    .or_default()
                    .entry(track_key.clone())
                    .or_default()
                    .push(relay_url.clone());
            }

            let handle = MockTrackSubHandle {
                state: self.state.clone(),
                scope_key,
                track_key,
                relay_url,
            };
            Ok(TrackSubscription::new(handle))
        }

        async fn unsubscribe_track(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<()> {
            let scope_key = MockState::scope_key(scope);
            let track_key = MockState::track_key(namespace, track);
            let relay_url = self.relay_url.to_string();
            let mut state = self.state.lock().unwrap();
            if let Some(bucket) = state.track_subscribers.get_mut(&scope_key) {
                if let Some(relays) = bucket.get_mut(&track_key) {
                    relays.retain(|r| r != &relay_url);
                }
            }
            Ok(())
        }

        async fn lookup_track_subscribers(
            &self,
            scope: Option<&str>,
            namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<Vec<RelayInfo>> {
            let scope_key = MockState::scope_key(scope);
            let track_key = MockState::track_key(namespace, track);
            let state = self.state.lock().unwrap();

            let relays = state
                .track_subscribers
                .get(&scope_key)
                .and_then(|bucket| bucket.get(&track_key))
                .map(|urls| {
                    urls.iter()
                        .map(|u| RelayInfo::new(Url::parse(u).unwrap()))
                        .collect()
                })
                .unwrap_or_default();

            Ok(relays)
        }
    }

    // ========================================================================
    // Type construction and defaults
    // ========================================================================

    #[test]
    fn scope_config_defaults() {
        let config = ScopeConfig::default();
        assert!(config.origin_fallback.is_none());
        assert!(!config.lingering_subscribe);
    }

    #[test]
    fn scope_config_with_origin_fallback() {
        let config = ScopeConfig {
            origin_fallback: Some(Url::parse("https://origin.example.com").unwrap()),
            lingering_subscribe: true,
        };
        assert_eq!(
            config.origin_fallback.unwrap().as_str(),
            "https://origin.example.com/"
        );
        assert!(config.lingering_subscribe);
    }

    #[test]
    fn relay_info_without_addr() {
        let info = RelayInfo::new(Url::parse("https://relay-us-east.example.com").unwrap());
        assert_eq!(info.url.as_str(), "https://relay-us-east.example.com/");
        assert!(info.addr.is_none());
    }

    #[test]
    fn relay_info_with_direct_addr() {
        let addr: SocketAddr = "10.0.1.5:4443".parse().unwrap();
        let info = RelayInfo::with_addr(
            Url::parse("https://relay-us-east.example.com").unwrap(),
            addr,
        );
        assert_eq!(info.url.as_str(), "https://relay-us-east.example.com/");
        assert_eq!(info.addr.unwrap(), addr);
    }

    #[test]
    fn track_entry_construction() {
        let entry = TrackEntry::new(ns("sports/football/match-42"), "video-1080p".to_string());
        assert_eq!(entry.namespace.to_utf8_path(), "/sports/football/match-42");
        assert_eq!(entry.name, "video-1080p");
    }

    #[test]
    fn namespace_subscription_default_is_empty() {
        let sub = NamespaceSubscription::default();
        assert!(sub.upstream_relays.is_empty());
    }

    #[test]
    fn track_registration_default_is_no_op() {
        // Default handle should not panic on drop
        let _reg = TrackRegistration::default();
    }

    #[test]
    fn track_subscription_default_is_no_op() {
        // Default handle should not panic on drop
        let _sub = TrackSubscription::default();
    }

    #[test]
    fn scope_permissions_publish_and_subscribe() {
        assert!(ScopePermissions::ReadWrite.can_publish());
        assert!(ScopePermissions::ReadWrite.can_subscribe());
        assert!(!ScopePermissions::ReadOnly.can_publish());
        assert!(ScopePermissions::ReadOnly.can_subscribe());
    }

    // ========================================================================
    // Scope resolution
    // ========================================================================

    #[tokio::test]
    async fn resolve_scope_maps_path_to_scope_identity() {
        // A broadcast platform might use connection paths that encode
        // a content provider identity and role. Multiple paths can map
        // to the same scope with different permissions.
        let coord = MockCoordinator::new("https://relay-1.example.com");
        coord.add_path_mapping(
            "/provider/acme-sports/ingest",
            "content-provider-123",
            ScopePermissions::ReadWrite,
        );
        coord.add_path_mapping(
            "/provider/acme-sports/watch",
            "content-provider-123",
            ScopePermissions::ReadOnly,
        );

        let ingest_scope = coord
            .resolve_scope(Some("/provider/acme-sports/ingest"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ingest_scope.scope_id, "content-provider-123");
        assert!(ingest_scope.permissions.can_publish());

        let watch_scope = coord
            .resolve_scope(Some("/provider/acme-sports/watch"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(watch_scope.scope_id, "content-provider-123");
        assert!(!watch_scope.permissions.can_publish());
        assert!(watch_scope.permissions.can_subscribe());
    }

    #[tokio::test]
    async fn resolve_scope_none_path_returns_unscoped() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let result = coord.resolve_scope(None).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_scope_unknown_path_returns_error() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let result = coord.resolve_scope(Some("/unknown/path")).await;
        assert!(result.is_err());
    }

    // ========================================================================
    // Scope configuration
    // ========================================================================

    #[tokio::test]
    async fn get_scope_config_returns_configured_settings() {
        // A content provider with an origin ingest server and lingering
        // subscriber support (viewers can tune in before the broadcast starts)
        let coord = MockCoordinator::new("https://relay-1.example.com");
        coord.set_scope_config(
            "content-provider-123",
            ScopeConfig {
                origin_fallback: Some(Url::parse("https://ingest.example.com/origin").unwrap()),
                lingering_subscribe: true,
            },
        );

        let config = coord
            .get_scope_config(Some("content-provider-123"))
            .await
            .unwrap();
        assert!(config.lingering_subscribe);
        assert!(config.origin_fallback.is_some());
    }

    #[tokio::test]
    async fn get_scope_config_unconfigured_returns_defaults() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let config = coord.get_scope_config(Some("unknown-scope")).await.unwrap();
        assert!(!config.lingering_subscribe);
        assert!(config.origin_fallback.is_none());
    }

    // ========================================================================
    // RelayInfo / CoordinatorContext
    // ========================================================================

    #[test]
    fn relay_info_from_socket_addr_synthesizes_https_identity() {
        let v4: SocketAddr = "203.0.113.7:4443".parse().unwrap();
        let info = RelayInfo::from_socket_addr(v4);
        assert_eq!(info.addr, Some(v4));
        assert_eq!(info.url.as_str(), "https://203.0.113.7:4443/");

        // IPv6 authorities are bracketed by SocketAddr's Display.
        let v6: SocketAddr = "[2001:db8::1]:4443".parse().unwrap();
        let info6 = RelayInfo::from_socket_addr(v6);
        assert_eq!(info6.addr, Some(v6));
        assert_eq!(info6.url.as_str(), "https://[2001:db8::1]:4443/");
    }

    #[test]
    fn coordinator_context_constructors() {
        let public = CoordinatorContext::public();
        assert_eq!(public.interface, SessionInterface::Public);
        assert!(public.source.is_none());

        let addr: SocketAddr = "10.0.0.7:4443".parse().unwrap();
        let internal = CoordinatorContext::internal(Some(RelayInfo::from_socket_addr(addr)));
        assert_eq!(internal.interface, SessionInterface::Internal);
        assert_eq!(internal.source.unwrap().addr, Some(addr));

        // Default is public with no source.
        let default = CoordinatorContext::default();
        assert_eq!(default.interface, SessionInterface::Public);
        assert!(default.source.is_none());
    }

    // ========================================================================
    // Namespace registration and lookup
    // ========================================================================

    #[tokio::test]
    async fn register_and_lookup_namespace() {
        // Ingest server registers a broadcast namespace; edge relay looks it up
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");

        let _reg = coord
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        let (origin, _client) = coord
            .lookup(scope, &ns("sports/football/match-42"))
            .await
            .unwrap();

        assert_eq!(origin.url().as_str(), "https://relay-1.example.com/");
    }

    #[tokio::test]
    async fn lookup_prefix_matching() {
        // A broadcaster registers a top-level event namespace; subscribers
        // looking up specific camera angles under it should still resolve.
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");

        let _reg = coord
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // Lookup a more specific namespace (camera angle) under the event
        let (origin, _) = coord
            .lookup(scope, &ns("sports/football/match-42/camera-1"))
            .await
            .unwrap();
        assert_eq!(origin.url().as_str(), "https://relay-1.example.com/");
    }

    #[tokio::test]
    async fn lookup_not_found() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let result = coord
            .lookup(Some("content-provider-123"), &ns("nonexistent"))
            .await;
        assert!(matches!(result, Err(CoordinatorError::NamespaceNotFound)));
    }

    #[tokio::test]
    async fn scopes_are_isolated() {
        // Two content providers using the same namespace structure should
        // not see each other's registrations.
        let coord = MockCoordinator::new("https://relay-1.example.com");

        let _reg = coord
            .register_namespace(
                Some("provider-abc"),
                &ns("live/main"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // Different provider can't see it
        let result = coord.lookup(Some("provider-xyz"), &ns("live/main")).await;
        assert!(matches!(result, Err(CoordinatorError::NamespaceNotFound)));

        // Same provider can
        let (origin, _) = coord
            .lookup(Some("provider-abc"), &ns("live/main"))
            .await
            .unwrap();
        assert_eq!(origin.url().as_str(), "https://relay-1.example.com/");
    }

    #[tokio::test]
    async fn duplicate_registration_rejected() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");

        let _reg = coord
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        let result = coord
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await;
        assert!(matches!(
            result,
            Err(CoordinatorError::NamespaceAlreadyRegistered)
        ));
    }

    #[tokio::test]
    async fn namespace_unregistered_on_handle_drop() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");

        {
            let _reg = coord
                .register_namespace(
                    scope,
                    &ns("sports/football/match-42"),
                    &CoordinatorContext::public(),
                )
                .await
                .unwrap();

            // Should be findable while registration is held
            assert!(coord
                .lookup(scope, &ns("sports/football/match-42"))
                .await
                .is_ok());
        }
        // _reg dropped — broadcast ended, ingest disconnected

        let result = coord.lookup(scope, &ns("sports/football/match-42")).await;
        assert!(matches!(result, Err(CoordinatorError::NamespaceNotFound)));
    }

    // ========================================================================
    // SUBSCRIBE_NAMESPACE — namespace prefix subscriptions
    // ========================================================================

    #[tokio::test]
    async fn subscribe_namespace_returns_hosting_relays_for_matching_prefix() {
        // Two football matches are live on origin-a; an unrelated tennis match
        // is live on origin-b. An edge relay subscribes to the "sports/football"
        // prefix and must be told to pull from origin-a only — deduped across
        // both football matches, and never origin-b, whose namespace does not
        // match the prefix.
        let origin_a = MockCoordinator::new("https://origin-a.example.com");
        let origin_b = origin_a.peer("https://origin-b.example.com");
        let edge = origin_a.peer("https://edge.example.com");
        let scope = Some("content-provider-123");

        // Two football matches, both hosted by origin-a.
        let _reg_match42 = origin_a
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        let _reg_match43 = origin_a
            .register_namespace(
                scope,
                &ns("sports/football/match-43"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // A tennis match on a different origin (should NOT match the football
        // prefix, so origin-b must not appear as an upstream).
        let _reg_tennis = origin_b
            .register_namespace(
                scope,
                &ns("sports/tennis/open-7"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // Subscribe to the football prefix from a third relay.
        let sub = edge
            .subscribe_namespace(
                scope,
                &prefix("sports/football"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        let urls: Vec<&str> = sub
            .upstream_relays
            .iter()
            .map(|relay| relay.url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://origin-a.example.com/"]);
    }

    #[tokio::test]
    async fn subscribe_namespace_empty_prefix_matches_all_namespaces() {
        // An empty prefix matches every namespace, so the subscriber must be
        // told to pull from every distinct hosting relay.
        let origin_a = MockCoordinator::new("https://origin-a.example.com");
        let origin_b = origin_a.peer("https://origin-b.example.com");
        let edge = origin_a.peer("https://edge-us-west.example.com");
        let scope = Some("content-provider-123");

        let _football = origin_a
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        let _tennis = origin_b
            .register_namespace(
                scope,
                &ns("sports/tennis/open-7"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        let sub = edge
            .subscribe_namespace(
                scope,
                &TrackNamespacePrefix::new(),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        let mut urls: Vec<String> = sub
            .upstream_relays
            .iter()
            .map(|relay| relay.url.to_string())
            .collect();
        urls.sort();

        assert_eq!(
            urls,
            vec![
                "https://origin-a.example.com/".to_string(),
                "https://origin-b.example.com/".to_string(),
            ]
        );

        // The edge's interest also persists for the reverse (push) direction:
        // origin-a's lookup finds the edge subscriber.
        let interested = origin_a
            .lookup_namespace_subscribers(
                scope,
                &ns("sports/football/match-44"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        assert_eq!(interested.len(), 1);
        assert_eq!(
            interested[0].url.as_str(),
            "https://edge-us-west.example.com/"
        );
    }

    #[tokio::test]
    async fn internal_subscribe_namespace_returns_no_upstream_relays() {
        // A pull opened on another relay's behalf arrives as an internal
        // session. Even though a peer hosts a matching namespace, the
        // coordinator returns no upstream relays, so the pull is served from
        // local state and does not recurse -- this is the loop-breaker. The
        // source peer is deliberately a *third* relay that does not host the
        // namespace, so only the interface (not caller/source exclusion) can
        // explain the empty result.
        let origin = MockCoordinator::new("https://origin.example.com");
        let edge = origin.peer("https://edge.example.com");
        let scope = Some("content-provider-123");

        let _registration = origin
            .register_namespace(scope, &ns("room/alice"), &CoordinatorContext::public())
            .await
            .unwrap();

        // A public subscribe from the edge is told to pull from the origin...
        let public_sub = edge
            .subscribe_namespace(scope, &prefix("room"), &CoordinatorContext::public())
            .await
            .unwrap();
        assert_eq!(
            public_sub.upstream_relays.len(),
            1,
            "public sessions fan out to hosting relays"
        );

        // ...but an internal subscribe (a relay pulling on another's behalf) is
        // given nothing to pull, so it serves from local state only.
        let internal_sub = edge
            .subscribe_namespace(
                scope,
                &prefix("room"),
                &CoordinatorContext::internal(Some(RelayInfo::new(
                    Url::parse("https://other.example.com").unwrap(),
                ))),
            )
            .await
            .unwrap();
        assert!(
            internal_sub.upstream_relays.is_empty(),
            "internal sessions are served from local state, breaking the pull loop"
        );
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_finds_interested_relays() {
        // An edge relay has subscribers interested in football broadcasts.
        // When a new match starts (namespace registered), the origin relay
        // calls lookup_namespace_subscribers to discover interested edge
        // relays and forward PUBLISH_NAMESPACE to them.
        let coord = MockCoordinator::new("https://edge-us-west.example.com");
        let scope = Some("content-provider-123");

        // Edge relay subscribes to the football prefix
        let _sub = coord
            .subscribe_namespace(
                scope,
                &prefix("sports/football"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // New match starts — the origin relay (a different relay than the
        // subscribing edge) asks the oracle who needs to know. The edge
        // subscriber is returned; the origin itself (the caller) is excluded.
        let origin = coord.peer("https://origin.example.com");
        let interested = origin
            .lookup_namespace_subscribers(
                scope,
                &ns("sports/football/match-44"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        assert_eq!(interested.len(), 1);
        assert_eq!(
            interested[0].url.as_str(),
            "https://edge-us-west.example.com/"
        );
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_excludes_caller() {
        // A relay that subscribed and is now performing the lookup must not be
        // returned to itself; otherwise it would forward PUBLISH_NAMESPACE to
        // its own endpoint and loop forever.
        let coord = MockCoordinator::new("https://edge-us-west.example.com");
        let scope = Some("content-provider-123");

        let _sub = coord
            .subscribe_namespace(
                scope,
                &prefix("sports/football"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        let interested = coord
            .lookup_namespace_subscribers(
                scope,
                &ns("sports/football/match-44"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        assert!(
            interested.is_empty(),
            "caller relay must be excluded from its own lookup results"
        );
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_excludes_source() {
        // A PUBLISH_NAMESPACE that arrived from a peer relay must not be echoed
        // back to that same peer.
        let origin = MockCoordinator::new("https://origin.example.com");
        let edge = origin.peer("https://edge-us-west.example.com");
        let scope = Some("content-provider-123");

        // The edge relay is the only subscriber.
        let _sub = edge
            .subscribe_namespace(
                scope,
                &prefix("sports/football"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // With no source, the origin sees the edge subscriber.
        let interested = origin
            .lookup_namespace_subscribers(
                scope,
                &ns("sports/football/match-44"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        assert_eq!(interested.len(), 1);

        // When the PUBLISH_NAMESPACE arrived *from* the edge relay, it must not
        // be echoed back to it.
        let from_edge = CoordinatorContext::internal(Some(RelayInfo::new(
            Url::parse("https://edge-us-west.example.com").unwrap(),
        )));
        let interested = origin
            .lookup_namespace_subscribers(scope, &ns("sports/football/match-44"), &from_edge)
            .await
            .unwrap();
        assert!(
            interested.is_empty(),
            "source relay must be excluded to avoid echoing PUBLISH_NAMESPACE back"
        );
    }

    #[tokio::test]
    async fn namespace_subscription_cleaned_up_on_drop() {
        let coord = MockCoordinator::new("https://edge-us-west.example.com");
        let scope = Some("content-provider-123");

        // The origin relay (distinct from the subscribing edge) performs the
        // lookups, so the edge subscriber is visible until it disconnects.
        let origin = coord.peer("https://origin.example.com");
        {
            let _sub = coord
                .subscribe_namespace(
                    scope,
                    &prefix("sports/football"),
                    &CoordinatorContext::public(),
                )
                .await
                .unwrap();

            let interested = origin
                .lookup_namespace_subscribers(
                    scope,
                    &ns("sports/football/match-44"),
                    &CoordinatorContext::public(),
                )
                .await
                .unwrap();
            assert_eq!(interested.len(), 1);
        }
        // _sub dropped — edge relay disconnected

        let interested = origin
            .lookup_namespace_subscribers(
                scope,
                &ns("sports/football/match-44"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        assert!(interested.is_empty());
    }

    // ========================================================================
    // Cross-relay SUBSCRIBE_NAMESPACE choreography (two relays, one oracle)
    //
    // These integration-style tests play out the full two-relay handshake the
    // relay performs across a shared coordinator oracle, in both orderings.
    // Each step is annotated with the production call site that drives it, so
    // the tests pin the oracle contract that `Consumer::serve` (consumer.rs) and
    // `Producer::serve_subscribe_namespace` (producer.rs) rely on: which peer
    // relay a subscriber pulls from (publish-before-subscribe) and which peer
    // relays an origin pushes PUBLISH_NAMESPACE to (subscribe-before-publish).
    //
    // A live two-relay MoQT session is intentionally out of scope: there is no
    // in-memory session harness, so `Consumer`/`Producer` cannot be built
    // without standing up real QUIC endpoints. We validate the coordination
    // decisions here; the transport plumbing is covered elsewhere.
    // ========================================================================

    #[tokio::test]
    async fn two_relay_subscribe_before_publish_routes_publish_namespace() {
        // Ordering: edge subscribes first, publisher arrives at the origin
        // later. The origin's reverse lookup must discover the waiting edge so
        // it pushes PUBLISH_NAMESPACE to it (the publish-after-subscribe path).
        let origin = MockCoordinator::new("https://origin.example.com");
        let edge = origin.peer("https://edge.example.com");
        let scope = Some("content-provider-123");

        // Edge relay: a client sent SUBSCRIBE_NAMESPACE, so the edge records its
        // interest with the oracle. Maps to Producer::serve_subscribe_namespace
        // -> coordinator.subscribe_namespace. Nothing is published yet, so there
        // is no upstream relay to pull from.
        let sub = edge
            .subscribe_namespace(scope, &prefix("room"), &CoordinatorContext::public())
            .await
            .unwrap();
        assert!(
            sub.upstream_relays.is_empty(),
            "no namespaces exist yet, so there is no upstream relay to pull from"
        );

        // Origin relay: a publisher sent PUBLISH_NAMESPACE, registering the
        // namespace. Maps to Consumer::serve -> coordinator.register_namespace.
        let _registration = origin
            .register_namespace(scope, &ns("room/alice"), &CoordinatorContext::public())
            .await
            .unwrap();

        // Origin relay: still inside Consumer::serve, it asks the oracle who is
        // interested (coordinator.lookup_namespace_subscribers). The waiting edge
        // must be returned and the origin (the caller) excluded, so the origin
        // pushes exactly one PUBLISH_NAMESPACE — to the edge.
        let interested = origin
            .lookup_namespace_subscribers(scope, &ns("room/alice"), &CoordinatorContext::public())
            .await
            .unwrap();
        assert_eq!(interested.len(), 1);
        assert_eq!(interested[0].url.as_str(), "https://edge.example.com/");
    }

    #[tokio::test]
    async fn two_relay_publish_before_subscribe_returns_upstream_relay() {
        // Ordering: the publisher is already live at the origin before any
        // subscriber. This pins two independent oracle guarantees: the
        // coordinator returns the origin as an upstream relay for the new
        // subscriber to pull from, and the subscriber's interest persists so
        // later namespaces still route to it via the reverse (push) lookup.
        let origin = MockCoordinator::new("https://origin.example.com");
        let edge = origin.peer("https://edge.example.com");
        let scope = Some("content-provider-123");

        // Origin relay: publisher registers the namespace first. Maps to
        // Consumer::serve -> coordinator.register_namespace.
        let _registration = origin
            .register_namespace(scope, &ns("room/alice"), &CoordinatorContext::public())
            .await
            .unwrap();

        // Edge relay: a client subscribes to the prefix afterwards. Maps to
        // Producer::serve_subscribe_namespace -> coordinator.subscribe_namespace.
        // The oracle reports the origin as the upstream relay to open an upstream
        // SUBSCRIBE_NAMESPACE session to (the pull path that carries the
        // already-published namespace, plus live updates, downstream).
        let sub = edge
            .subscribe_namespace(scope, &prefix("room"), &CoordinatorContext::public())
            .await
            .unwrap();
        let urls: Vec<&str> = sub
            .upstream_relays
            .iter()
            .map(|relay| relay.url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://origin.example.com/"]);

        // The edge's interest also persists for the reverse (push) direction: a
        // *different* namespace published later under the same prefix must still
        // discover the edge. Maps to Consumer::serve ->
        // coordinator.lookup_namespace_subscribers.
        let interested = origin
            .lookup_namespace_subscribers(
                scope,
                &ns("room/beatrice"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        assert_eq!(interested.len(), 1);
        assert_eq!(interested[0].url.as_str(), "https://edge.example.com/");
    }

    // ========================================================================
    // Track-level PUBLISH registration
    // ========================================================================

    #[tokio::test]
    async fn register_and_list_tracks() {
        // A broadcaster publishes multiple renditions (video qualities,
        // audio languages) as individual tracks under a match namespace.
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");
        let match_ns = ns("sports/football/match-42");

        let _reg_1080 = coord
            .register_track(scope, &match_ns, "video-1080p")
            .await
            .unwrap();
        let _reg_480 = coord
            .register_track(scope, &match_ns, "video-480p")
            .await
            .unwrap();
        let _reg_audio = coord
            .register_track(scope, &match_ns, "audio-en")
            .await
            .unwrap();

        let tracks = coord.list_tracks(scope, &match_ns).await.unwrap();
        assert_eq!(tracks.len(), 3);

        let names: Vec<&str> = tracks.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"video-1080p"));
        assert!(names.contains(&"video-480p"));
        assert!(names.contains(&"audio-en"));
    }

    #[tokio::test]
    async fn lookup_track_finds_exact_registered_track() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");
        let match_ns = ns("sports/football/match-42");

        let _reg = coord
            .register_track(scope, &match_ns, "video-1080p")
            .await
            .unwrap();

        let (origin, client) = coord
            .lookup_track(scope, &match_ns, "video-1080p")
            .await
            .unwrap();

        assert_eq!(origin.namespace(), &match_ns);
        assert_eq!(origin.url().as_str(), "https://relay-1.example.com/");
        assert!(client.is_none());
    }

    #[tokio::test]
    async fn lookup_track_falls_back_to_namespace_registration() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");
        let match_ns = ns("sports/football/match-42");

        let _namespace = coord
            .register_namespace(scope, &match_ns, &CoordinatorContext::public())
            .await
            .unwrap();

        let (origin, client) = coord
            .lookup_track(scope, &match_ns, "video-1080p")
            .await
            .unwrap();
        assert_eq!(origin.namespace(), &match_ns);
        assert_eq!(origin.url().as_str(), "https://relay-1.example.com/");
        assert!(client.is_none());
    }

    #[tokio::test]
    async fn track_unregistered_on_handle_drop() {
        let coord = MockCoordinator::new("https://relay-1.example.com");
        let scope = Some("content-provider-123");
        let match_ns = ns("sports/football/match-42");

        {
            let _reg = coord
                .register_track(scope, &match_ns, "video-1080p")
                .await
                .unwrap();

            let tracks = coord.list_tracks(scope, &match_ns).await.unwrap();
            assert_eq!(tracks.len(), 1);
        }
        // _reg dropped — broadcaster stopped the 1080p rendition

        let tracks = coord.list_tracks(scope, &match_ns).await.unwrap();
        assert!(tracks.is_empty());
    }

    // ========================================================================
    // Lingering subscriber / rendezvous
    // ========================================================================

    #[tokio::test]
    async fn subscribe_track_before_publisher_exists() {
        // A viewer tunes into a pre-game show before the main broadcast has
        // started. The edge relay pre-registers interest in the track so
        // that when the broadcaster begins, it can be notified immediately.
        let coord = MockCoordinator::new("https://edge-eu-west.example.com");
        let scope = Some("content-provider-123");
        let match_ns = ns("sports/football/match-42");

        // Viewer's edge relay pre-registers interest (lingering/rendezvous)
        let _sub = coord
            .subscribe_track(scope, &match_ns, "video-1080p")
            .await
            .unwrap();

        // Broadcaster starts — origin relay checks who's waiting
        let waiting = coord
            .lookup_track_subscribers(scope, &match_ns, "video-1080p")
            .await
            .unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].url.as_str(), "https://edge-eu-west.example.com/");

        // No one is waiting for the Spanish audio (not pre-subscribed)
        let waiting_es = coord
            .lookup_track_subscribers(scope, &match_ns, "audio-es")
            .await
            .unwrap();
        assert!(waiting_es.is_empty());
    }

    #[tokio::test]
    async fn track_subscription_cleaned_up_on_drop() {
        let coord = MockCoordinator::new("https://edge-eu-west.example.com");
        let scope = Some("content-provider-123");
        let match_ns = ns("sports/football/match-42");

        {
            let _sub = coord
                .subscribe_track(scope, &match_ns, "video-1080p")
                .await
                .unwrap();

            let waiting = coord
                .lookup_track_subscribers(scope, &match_ns, "video-1080p")
                .await
                .unwrap();
            assert_eq!(waiting.len(), 1);
        }
        // _sub dropped — viewer left

        let waiting = coord
            .lookup_track_subscribers(scope, &match_ns, "video-1080p")
            .await
            .unwrap();
        assert!(waiting.is_empty());
    }

    // ========================================================================
    // End-to-end scenario: live broadcast across a relay cluster
    // ========================================================================

    #[tokio::test]
    async fn broadcast_multi_relay_scenario() {
        // A content provider ("content-provider-123") broadcasts a football
        // match through a relay cluster:
        //
        //   origin relay (us-east): broadcaster ingests video + audio
        //   edge relay (eu-west): viewers in Europe subscribe, including
        //     one who tunes in before halftime coverage starts
        //
        // This exercises namespace registration, SUBSCRIBE_NAMESPACE for
        // event discovery, track-level PUBLISH, and lingering subscriber
        // for pre-broadcast rendezvous.

        let origin = MockCoordinator::new("https://relay-us-east.example.com");
        let edge = MockCoordinator::new("https://edge-eu-west.example.com");

        let scope = Some("content-provider-123");

        // --- Origin relay: broadcaster starts the match ---

        // Register the match namespace
        let _match_reg = origin
            .register_namespace(
                scope,
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // Register individual track renditions
        let _video_1080 = origin
            .register_track(scope, &ns("sports/football/match-42"), "video-1080p")
            .await
            .unwrap();
        let _video_480 = origin
            .register_track(scope, &ns("sports/football/match-42"), "video-480p")
            .await
            .unwrap();
        let _audio_en = origin
            .register_track(scope, &ns("sports/football/match-42"), "audio-en")
            .await
            .unwrap();

        // --- Edge relay: viewers subscribe ---

        // Edge subscribes to all football events (SUBSCRIBE_NAMESPACE)
        let _football_sub = edge
            .subscribe_namespace(
                scope,
                &prefix("sports/football"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();

        // A viewer pre-subscribes to halftime analysis (not started yet)
        let _halftime_sub = edge
            .subscribe_track(
                scope,
                &ns("sports/football/match-42/halftime"),
                "video-720p",
            )
            .await
            .unwrap();

        // When halftime coverage starts, the origin can find waiting viewers
        let waiting = edge
            .lookup_track_subscribers(
                scope,
                &ns("sports/football/match-42/halftime"),
                "video-720p",
            )
            .await
            .unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].url.as_str(), "https://edge-eu-west.example.com/");

        // Verify the origin's track inventory
        let tracks = origin
            .list_tracks(scope, &ns("sports/football/match-42"))
            .await
            .unwrap();
        assert_eq!(tracks.len(), 3);

        // Verify scope isolation — a different provider sees nothing
        let other_result = origin
            .lookup(Some("other-provider"), &ns("sports/football/match-42"))
            .await;
        assert!(matches!(
            other_result,
            Err(CoordinatorError::NamespaceNotFound)
        ));
    }

    // ========================================================================
    // Default trait implementations (no-op behavior)
    // ========================================================================

    /// A minimal coordinator that only implements the required methods.
    /// Used to verify that all defaulted methods work correctly — this is
    /// what existing implementors experience after upgrading.
    struct MinimalCoordinator;

    #[async_trait]
    impl Coordinator for MinimalCoordinator {
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
    }

    #[tokio::test]
    async fn default_resolve_scope_passes_through_path() {
        let coord = MinimalCoordinator;
        let scope = coord
            .resolve_scope(Some("/provider/acme-sports"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope.scope_id, "/provider/acme-sports");
        assert!(scope.permissions.can_publish());
        assert!(scope.permissions.can_subscribe());
    }

    #[tokio::test]
    async fn default_resolve_scope_none_is_unscoped() {
        let coord = MinimalCoordinator;
        let result = coord.resolve_scope(None).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn default_get_scope_config_returns_defaults() {
        let coord = MinimalCoordinator;
        let config = coord.get_scope_config(Some("any-scope")).await.unwrap();
        assert!(!config.lingering_subscribe);
        assert!(config.origin_fallback.is_none());
    }

    #[tokio::test]
    async fn default_subscribe_namespace_returns_empty() {
        let coord = MinimalCoordinator;
        let sub = coord
            .subscribe_namespace(
                Some("scope"),
                &prefix("sports/football"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        assert!(sub.upstream_relays.is_empty());
    }

    #[tokio::test]
    async fn default_lookup_namespace_subscribers_returns_empty() {
        let coord = MinimalCoordinator;
        let relays = coord
            .lookup_namespace_subscribers(
                Some("scope"),
                &ns("sports/football/match-42"),
                &CoordinatorContext::public(),
            )
            .await
            .unwrap();
        assert!(relays.is_empty());
    }

    #[tokio::test]
    async fn default_register_track_returns_no_op_handle() {
        let coord = MinimalCoordinator;
        let _reg = coord
            .register_track(
                Some("scope"),
                &ns("sports/football/match-42"),
                "video-1080p",
            )
            .await
            .unwrap();
        // Handle drops without panic
    }

    #[tokio::test]
    async fn default_list_tracks_returns_empty() {
        let coord = MinimalCoordinator;
        let tracks = coord
            .list_tracks(Some("scope"), &ns("sports/football/match-42"))
            .await
            .unwrap();
        assert!(tracks.is_empty());
    }

    #[tokio::test]
    async fn default_subscribe_track_returns_no_op_handle() {
        let coord = MinimalCoordinator;
        let _sub = coord
            .subscribe_track(
                Some("scope"),
                &ns("sports/football/match-42"),
                "video-1080p",
            )
            .await
            .unwrap();
        // Handle drops without panic
    }

    #[tokio::test]
    async fn default_lookup_track_subscribers_returns_empty() {
        let coord = MinimalCoordinator;
        let relays = coord
            .lookup_track_subscribers(
                Some("scope"),
                &ns("sports/football/match-42"),
                "video-1080p",
            )
            .await
            .unwrap();
        assert!(relays.is_empty());
    }
}
