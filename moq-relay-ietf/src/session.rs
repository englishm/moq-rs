// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::session::{Publisher, SessionError, SessionId, Subscriber};
use tracing::Instrument;

use crate::{Consumer, CoordinatorContext, Producer, RelayInfo};

/// Well-known connection tag key identifying the session interface.
pub const TAG_INTERFACE: &str = "interface";

/// [`TAG_INTERFACE`] value for public, client-facing connections.
pub const INTERFACE_PUBLIC: &str = "public";

/// [`TAG_INTERFACE`] value for internal, relay-to-relay connections.
pub const INTERFACE_INTERNAL: &str = "internal";

/// Identifies whether a relay session came from a public client or another relay.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Default)]
pub enum SessionInterface {
    /// A public, client-facing connection.
    ///
    /// This is the default: an unclassified connection is treated as a public
    /// client (see [`ConnectionTags::interface`]).
    #[default]
    Public,
    /// An internal, relay-to-relay connection.
    Internal,
}

impl SessionInterface {
    /// The tag value for this interface: `"public"` or `"internal"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionInterface::Public => INTERFACE_PUBLIC,
            SessionInterface::Internal => INTERFACE_INTERNAL,
        }
    }
}

impl std::fmt::Display for SessionInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Key-value tags describing an accepted connection.
///
/// The relay only interprets the well-known [`TAG_INTERFACE`] key today, but
/// the map is intentionally open so embedders can attach their own metadata
/// from a [`ConnectionTagger`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionTags {
    tags: BTreeMap<String, String>,
}

impl ConnectionTags {
    /// Create an empty set of tags.
    ///
    /// An empty set resolves to [`SessionInterface::Public`] — see
    /// [`interface`](Self::interface).
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a tag, consuming and returning `self` for builder-style chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.insert(key.into(), value.into());
        self
    }

    /// Set the well-known [`TAG_INTERFACE`] tag.
    pub fn with_interface(self, interface: SessionInterface) -> Self {
        let value = match interface {
            SessionInterface::Public => INTERFACE_PUBLIC,
            SessionInterface::Internal => INTERFACE_INTERNAL,
        };
        self.with(TAG_INTERFACE, value)
    }

    /// Look up a tag value by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.tags.get(key).map(String::as_str)
    }

    /// Resolve the session interface from the well-known [`TAG_INTERFACE`] tag.
    ///
    /// Defaults to [`SessionInterface::Public`] when the tag is missing or
    /// holds an unrecognized value: an untagged connection is treated as a
    /// public client.
    pub fn interface(&self) -> SessionInterface {
        match self.get(TAG_INTERFACE) {
            Some(value) if value == INTERFACE_INTERNAL => SessionInterface::Internal,
            _ => SessionInterface::Public,
        }
    }
}

/// Metadata about an accepted connection, passed to a [`ConnectionTagger`].
///
/// All fields work uniformly across WebTransport and raw MoQT (`moqt://`)
/// connections, so taggers can match on them without caring about the
/// transport. (The WebTransport request URL is intentionally omitted: it is
/// synthetic for raw MoQT, so [`server_name`](Self::server_name) is the
/// portable way to match on host/authority.)
#[derive(Debug, Clone, Default)]
pub struct ConnectionMeta {
    /// Remote socket address of the peer, when known.
    pub remote_addr: Option<SocketAddr>,

    /// Local IP the connection was accepted on: the destination IP the peer
    /// targeted, when known.
    ///
    /// On a wildcard bind (`0.0.0.0` / `[::]`) this identifies which local
    /// address/interface (e.g. an anycast VIP) actually received the
    /// connection, letting a tagger classify by inbound interface. `None` when
    /// the platform does not expose the destination address.
    pub local_ip: Option<IpAddr>,

    /// TLS SNI (Server Name Indication) the client requested, when present.
    ///
    /// Available for both WebTransport and raw MoQT connections, so this is the
    /// portable signal for host/authority-based classification.
    pub server_name: Option<String>,

    /// Normalized connection path (WebTransport URL path or CLIENT_SETUP PATH
    /// parameter), when present.
    pub path: Option<String>,
}

impl ConnectionMeta {
    /// Create connection metadata from a remote address, TLS SNI, and path.
    ///
    /// Use [`with_local_ip`](Self::with_local_ip) to attach the local IP the
    /// connection was accepted on.
    pub fn new(
        remote_addr: Option<SocketAddr>,
        server_name: Option<String>,
        path: Option<String>,
    ) -> Self {
        Self {
            remote_addr,
            local_ip: None,
            server_name,
            path,
        }
    }

    /// Attach the local IP the connection was accepted on (the destination IP
    /// the peer targeted, from `ConnInfo::local_ip`). Returns `self` for
    /// builder-style chaining. See [`local_ip`](Self::local_ip).
    pub fn with_local_ip(mut self, local_ip: Option<IpAddr>) -> Self {
        self.local_ip = local_ip;
        self
    }
}

/// Classifies accepted connections into [`ConnectionTags`].
///
/// The relay calls this once for every **inbound** connection it accepts, to
/// decide whether the peer is a public client or an internal relay-to-relay
/// peer, via the well-known [`TAG_INTERFACE`] tag. Embedders implement it to
/// match on the remote socket address, TLS SNI, or connection path.
///
/// The library ships **no** concrete implementation: it owns only the trait and
/// the [`TAG_INTERFACE`] contract, while the embedder owns the site-specific
/// policy of what counts as "internal". A relay with no tagger
/// (`RelayConfig.connection_tagger = None`) treats every inbound connection as
/// public. Interface classification is deliberately separate from
/// [`Coordinator::resolve_scope`], which resolves *identity and permissions*;
/// the coordinator cannot know the transport interface a connection arrived on.
///
/// # Where classification happens (important)
///
/// The `public`/`internal` label is **local to the relay that accepts the
/// connection and is never sent over the wire**. Each relay classifies its own
/// inbound connections independently:
///
/// * Connections the relay dials itself (`--announce`, [`RemoteManager`]) are
///   tagged [`SessionInterface::Internal`] at dial time and never reach a
///   tagger.
/// * When relay A dials relay B, the MoQT handshake carries **no** "I am a
///   relay" signal. Relay B must recognize relay A purely from the raw
///   connection attributes in [`ConnectionMeta`]. If relay B has no tagger, it
///   treats relay A as a public client.
///
/// So to make a relay-to-relay link internal on the accepting end, the tagger
/// must identify the dialing relay from what actually crosses the wire:
///
/// * [`ConnectionMeta::remote_addr`] — the dialer's source IP/port (e.g. match
///   an internal subnet or allowlist).
/// * [`ConnectionMeta::local_ip`] — the local IP/interface the connection was
///   accepted on (e.g. match an internal-facing VIP on a multi-homed or
///   anycast host).
/// * [`ConnectionMeta::server_name`] — the TLS SNI the dialer presents, derived
///   from the host of the URL it dialed, so relays that dial an internal
///   hostname can be matched here.
/// * [`ConnectionMeta::path`] — the connection path from the dialed URL.
///
/// For a mesh where both relays should see each other as internal, both relays
/// need a tagger, and each must dial the other on an address/SNI/path the
/// peer's tagger recognizes.
///
/// # Implementing a tagger
///
/// Return [`ConnectionTags`] carrying the [`TAG_INTERFACE`] tag (via
/// [`ConnectionTags::with_interface`]); empty/untagged results resolve to
/// [`SessionInterface::Public`]. Then wire the tagger into the relay through
/// `RelayConfig.connection_tagger`.
///
/// ```
/// use std::net::{IpAddr, SocketAddr};
/// use std::sync::Arc;
///
/// use moq_relay_ietf::{ConnectionMeta, ConnectionTagger, ConnectionTags, SessionInterface};
///
/// /// Treats peers on the 10.0.0.0/8 mesh subnet, or presenting the mesh SNI,
/// /// as internal relay-to-relay; everything else stays public.
/// struct MeshTagger;
///
/// impl ConnectionTagger for MeshTagger {
///     fn tag(&self, meta: &ConnectionMeta) -> ConnectionTags {
///         let from_mesh_subnet = matches!(
///             meta.remote_addr.map(|addr| addr.ip()),
///             Some(IpAddr::V4(ip)) if ip.octets()[0] == 10
///         );
///         let from_mesh_sni = meta.server_name.as_deref() == Some("mesh.internal");
///
///         if from_mesh_subnet || from_mesh_sni {
///             ConnectionTags::new().with_interface(SessionInterface::Internal)
///         } else {
///             // Empty tags resolve to SessionInterface::Public.
///             ConnectionTags::new()
///         }
///     }
/// }
///
/// // Hand it to the relay as `RelayConfig { connection_tagger: Some(tagger), .. }`.
/// let tagger: Arc<dyn ConnectionTagger> = Arc::new(MeshTagger);
///
/// let mesh_peer: SocketAddr = "10.0.0.7:4443".parse().unwrap();
/// assert_eq!(
///     tagger
///         .tag(&ConnectionMeta::new(Some(mesh_peer), None, None))
///         .interface(),
///     SessionInterface::Internal,
/// );
///
/// let public_peer: SocketAddr = "203.0.113.7:4443".parse().unwrap();
/// assert_eq!(
///     tagger
///         .tag(&ConnectionMeta::new(Some(public_peer), None, None))
///         .interface(),
///     SessionInterface::Public,
/// );
/// ```
///
/// [`Coordinator::resolve_scope`]: crate::Coordinator::resolve_scope
/// [`RemoteManager`]: crate::RemoteManager
pub trait ConnectionTagger: Send + Sync {
    /// Classify a connection from its metadata.
    fn tag(&self, meta: &ConnectionMeta) -> ConnectionTags;
}

/// Context carried by relay-side producer and consumer tasks for one MoQT session.
#[derive(Debug, Clone)]
pub struct SessionContext {
    /// Scope used for routing and coordinator calls, if any.
    ///
    /// This value may contain credentials and must not be emitted in telemetry.
    pub scope: Option<String>,

    /// Whether this session is public client-facing or internal relay-to-relay.
    pub interface: SessionInterface,

    /// Relay identity for internal sessions, when known.
    ///
    /// Populated for outbound connections the relay dials itself, where the
    /// destination URL is known, and derived from the remote socket address for
    /// inbound connections classified as internal. Public sessions always
    /// leave this `None`.
    pub peer: Option<RelayInfo>,
}

impl SessionContext {
    /// Build a public, client-facing context for the given scope.
    pub fn public(scope: Option<String>) -> Self {
        Self {
            scope,
            interface: SessionInterface::Public,
            peer: None,
        }
    }

    /// Build an internal, relay-to-relay context for the given scope and peer.
    ///
    /// The peer is forwarded as the [`CoordinatorContext::source`].
    pub fn internal(scope: Option<String>, peer: Option<RelayInfo>) -> Self {
        Self {
            scope,
            interface: SessionInterface::Internal,
            peer,
        }
    }

    /// Build an inbound context from a resolved scope, connection tags, and
    /// the peer's socket address.
    ///
    /// The interface is taken from the tags (see [`ConnectionTags::interface`]).
    /// For connections classified [`SessionInterface::Internal`], the peer
    /// identity is derived from `remote_addr` (the relay trusts the inbound
    /// socket address rather than asking the coordinator to resolve it; see
    /// [`RelayInfo::from_socket_addr`]). Public connections, and internal
    /// connections with no known address, leave `peer` as `None`.
    pub fn from_tags(
        scope: Option<String>,
        tags: &ConnectionTags,
        remote_addr: Option<SocketAddr>,
    ) -> Self {
        let interface = tags.interface();
        let peer = match interface {
            SessionInterface::Internal => remote_addr.map(RelayInfo::from_socket_addr),
            SessionInterface::Public => None,
        };
        Self {
            scope,
            interface,
            peer,
        }
    }

    /// Build the tracing span that every log record for this session inherits.
    ///
    /// Relay-side correlation is carried by this span rather than by a field on
    /// each call site: the fields are recorded once per session instead of being
    /// re-formatted per record, and log lines added later inherit them for free.
    ///
    /// The span is an explicit root (`parent: None`) so an upstream session
    /// created while serving another session does not inherit the requesting
    /// session's identity. The full scope value is never recorded because it
    /// may contain credentials. `relay_uid` holds only the first path segment
    /// of a path-format scope URL (e.g. `/relay_uid/jwt-token`), which is the
    /// internal customer/relay account identifier and carries no credential
    /// material. `scope_present` is a boolean flag for the case where
    /// `relay_uid` falls back to `"-"` (non-path scope).
    /// Target-specific filters that enable other relay modules must also enable
    /// `moq_relay_ietf::session`, because a disabled span cannot carry context.
    pub fn span(&self, session_id: &SessionId) -> tracing::Span {
        crate::enabled_root_span!(
            target: module_path!(),
            "moq_session",
            session_id = %session_id,
            interface = %self.interface,
            // relay_uid: first path segment of the scope URL — the internal
            // customer/relay account identifier. Non-path scopes → "-".
            relay_uid = self.relay_uid(),
            // scope_present: signals a scope exists even when relay_uid is "-".
            scope_present = self.scope.is_some(),
        )
    }

    /// The `relay_uid` extracted from a path-format scope, safe to log.
    ///
    /// The `relay_uid` is the internal customer/relay account identifier that
    /// lives as the first path segment of the scope URL when the scope is
    /// path-formatted (e.g. `/relay_uid/jwt-token` → `"relay_uid"`).
    ///
    /// Non-path scopes (no leading `/`) are treated as potentially containing
    /// raw credentials and are returned as `"-"` rather than logged verbatim.
    fn relay_uid(&self) -> &str {
        self.scope
            .as_deref()
            .filter(|s| s.starts_with('/'))
            .and_then(|s| s.split('/').find(|seg| !seg.is_empty()))
            .unwrap_or("-")
    }

    /// The scope used for routing and coordinator calls, if any.
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// Build the [`CoordinatorContext`] for coordinator calls made on behalf
    /// of this session.
    ///
    /// Maps the session interface through unchanged and forwards [`peer`] as
    /// the operation's `source`.
    ///
    /// [`peer`]: Self::peer
    pub fn coordinator_context(&self) -> CoordinatorContext {
        CoordinatorContext {
            interface: self.interface,
            source: self.peer.clone(),
        }
    }
}

pub struct Session {
    pub session: moq_transport::session::Session,
    pub producer: Option<Producer>,
    pub consumer: Option<Consumer>,

    /// When `consumer` is `None` (publish not permitted), the transport
    /// `Subscriber` half still exists and will queue incoming
    /// PUBLISH_NAMESPACEs from the peer. We hold it here so we can
    /// actively drain and reject those messages instead of silently
    /// ignoring them.
    pub reject_publishes: Option<Subscriber>,

    /// When `producer` is `None` (subscribe not permitted), the transport
    /// `Publisher` half still exists and will queue incoming SUBSCRIBEs
    /// from the peer. We hold it here so we can actively drain and reject
    /// those messages instead of silently ignoring them.
    pub reject_subscribes: Option<Publisher>,
}

impl Session {
    /// Run the session, producer, and consumer as necessary.
    pub async fn run(self) -> Result<(), SessionError> {
        self.run_inner().await
    }

    /// Run the session inside a root span carrying its relay context.
    ///
    /// Every record emitted by the transport, producer and consumer tasks
    /// inherits the session id without each call site naming it.
    pub async fn run_with_context(self, context: &SessionContext) -> Result<(), SessionError> {
        let span = context.span(self.session.session_id());
        async move {
            tracing::info!(
                publish_permitted = self.consumer.is_some(),
                subscribe_permitted = self.producer.is_some(),
                "session established"
            );
            self.run_inner().await
        }
        .instrument(span)
        .await
    }

    async fn run_inner(self) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        tasks.push(self.session.run().boxed());

        if let Some(producer) = self.producer {
            tasks.push(producer.run().boxed());
        }

        if let Some(consumer) = self.consumer {
            tasks.push(consumer.run().boxed());
        }

        // Reject unauthorized messages for disabled session halves.
        // Without these, a peer that sends a disallowed control message
        // would get no response (no OK, no error) because nobody is
        // draining the transport queue for that message type.
        if let Some(subscriber) = self.reject_publishes {
            tasks.push(Self::drain_and_reject_publishes(subscriber).boxed());
        }

        if let Some(publisher) = self.reject_subscribes {
            tasks.push(Self::drain_and_reject_subscribes(publisher).boxed());
        }

        tasks.select_next_some().await
    }

    /// Drain incoming PUBLISH_NAMESPACE and PUBLISH requests and reject each one.
    ///
    /// Dropping a `PublishedNamespace` without calling `ok()` triggers its
    /// `Drop` impl, which sends REQUEST_ERROR back to the peer.
    async fn drain_and_reject_publishes(subscriber: Subscriber) -> Result<(), SessionError> {
        let mut namespace_subscriber = subscriber.clone();
        let mut publish_subscriber = subscriber;

        loop {
            tokio::select! {
                Some(published_ns) = namespace_subscriber.published_namespace() => {
                    tracing::debug!(
                        namespace = %published_ns.namespace,
                        "rejecting PUBLISH_NAMESPACE: publish not permitted for this session"
                    );
                    drop(published_ns);
                },
                Some(publish) = publish_subscriber.publish_received() => {
                    tracing::debug!(
                        namespace = %publish.namespace(),
                        track = %publish.name(),
                        "rejecting PUBLISH: publish not permitted for this session"
                    );
                    drop(publish);
                },
                else => return Ok(()),
            }
        }
    }

    /// Drain incoming SUBSCRIBE and SUBSCRIBE_NAMESPACE requests and reject each one.
    ///
    /// The transport `Publisher` queues incoming SUBSCRIBE messages as
    /// `Subscribed` events. Dropping a `Subscribed` without calling `ok()`
    /// triggers its `Drop` impl, which sends SUBSCRIBE_ERROR back to the
    /// peer.
    async fn drain_and_reject_subscribes(mut publisher: Publisher) -> Result<(), SessionError> {
        loop {
            let mut namespace_publisher = publisher.clone();
            tokio::select! {
                Some(subscribed) = publisher.subscribed() => {
                    tracing::debug!(
                        namespace = %subscribed.track_namespace,
                        track = %subscribed.track_name,
                        "rejecting SUBSCRIBE: subscribe not permitted for this session"
                    );
                    drop(subscribed);
                }
                Some(subscribed_namespace) = namespace_publisher.subscribed_namespace() => {
                    tracing::debug!(
                        namespace_prefix = %subscribed_namespace.namespace_prefix,
                        "rejecting SUBSCRIBE_NAMESPACE: subscribe not permitted for this session"
                    );
                    drop(subscribed_namespace);
                }
                else => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing::Instrument;
    use url::Url;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_output(filter: &str, emit: impl FnOnce()) -> String {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, emit);

        let output = capture.0.lock().unwrap().clone();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn connection_tags_default_interface_is_public() {
        // An untagged connection is treated as a public client.
        assert_eq!(ConnectionTags::new().interface(), SessionInterface::Public);
        assert_eq!(
            ConnectionTags::default().interface(),
            SessionInterface::Public
        );
    }

    #[test]
    fn connection_tags_with_interface_roundtrips() {
        assert_eq!(
            ConnectionTags::new()
                .with_interface(SessionInterface::Internal)
                .interface(),
            SessionInterface::Internal
        );
        assert_eq!(
            ConnectionTags::new()
                .with_interface(SessionInterface::Public)
                .interface(),
            SessionInterface::Public
        );
    }

    #[test]
    fn connection_tags_unknown_interface_value_falls_back_to_public() {
        let tags = ConnectionTags::new().with(TAG_INTERFACE, "bogus");
        assert_eq!(tags.interface(), SessionInterface::Public);
    }

    #[test]
    fn connection_tags_store_and_retrieve_arbitrary_keys() {
        let tags = ConnectionTags::new()
            .with("tenant", "acme")
            .with_interface(SessionInterface::Internal);
        assert_eq!(tags.get("tenant"), Some("acme"));
        assert_eq!(tags.get(TAG_INTERFACE), Some(INTERFACE_INTERNAL));
        assert_eq!(tags.get("missing"), None);
    }

    // ========================================================================
    // relay_uid()
    // ========================================================================

    #[test]
    fn relay_uid_extracts_first_path_segment() {
        let make = |scope: Option<&str>| SessionContext::public(scope.map(str::to_string));

        // Normal case: /relay_uid/jwt-token → relay_uid
        assert_eq!(
            make(Some("/relay-uid/eyJhbGciOiJIUzI1NiJ9.secret")).relay_uid(),
            "relay-uid"
        );
        // Non-path scope (no leading slash): treated as potentially sensitive, returns "-".
        assert_eq!(make(Some("just-an-id")).relay_uid(), "-");
        // None → fallback sentinel
        assert_eq!(make(None).relay_uid(), "-");
        // Empty string → fallback sentinel
        assert_eq!(make(Some("")).relay_uid(), "-");
        // Root slash only → fallback sentinel
        assert_eq!(make(Some("/")).relay_uid(), "-");
        // Multiple leading slashes
        assert_eq!(make(Some("//foo")).relay_uid(), "foo");
    }

    #[test]
    fn relay_uid_hides_non_path_scopes() {
        // A non-path scope (no leading '/') cannot be safely decomposed into
        // uid vs. credential segments, so relay_uid() returns "-" rather than
        // risking leaking the full value into logs.
        let ctx = SessionContext::public(Some("eyJhbGciOiJIUzI1NiJ9.payload.sig".to_string()));
        assert_eq!(
            ctx.relay_uid(),
            "-",
            "non-path scope must not be logged verbatim"
        );
    }

    #[test]
    fn session_context_public_has_no_peer_or_coordinator_source() {
        let context = SessionContext::public(Some("scope-a".to_string()));
        assert_eq!(context.scope(), Some("scope-a"));
        assert_eq!(context.interface, SessionInterface::Public);
        assert!(context.peer.is_none());

        let coordinator = context.coordinator_context();
        assert_eq!(coordinator.interface, SessionInterface::Public);
        assert!(coordinator.source.is_none());
    }

    #[test]
    fn session_context_internal_carries_peer() {
        let url = Url::parse("https://relay.example.com/live").unwrap();
        let addr: SocketAddr = "10.0.0.8:4443".parse().unwrap();
        let context = SessionContext::internal(
            Some("scope-b".to_string()),
            Some(RelayInfo::with_addr(url.clone(), addr)),
        );
        assert_eq!(context.scope(), Some("scope-b"));
        assert_eq!(context.interface, SessionInterface::Internal);
        assert_eq!(context.peer.unwrap().url, url);
    }

    #[test]
    fn session_context_from_tags_populates_peer_for_internal() {
        let addr: SocketAddr = "10.0.0.7:4443".parse().unwrap();

        // Internal + known address → peer derived from the socket address.
        let internal = SessionContext::from_tags(
            Some("scope-c".to_string()),
            &ConnectionTags::new().with_interface(SessionInterface::Internal),
            Some(addr),
        );
        assert_eq!(internal.interface, SessionInterface::Internal);
        assert_eq!(internal.scope(), Some("scope-c"));
        let peer = internal.peer.expect("internal context should carry a peer");
        assert_eq!(peer.addr, Some(addr));
        assert_eq!(peer.url.as_str(), "https://10.0.0.7:4443/");

        // Internal but no known address → no peer.
        let internal_no_addr = SessionContext::from_tags(
            None,
            &ConnectionTags::new().with_interface(SessionInterface::Internal),
            None,
        );
        assert_eq!(internal_no_addr.interface, SessionInterface::Internal);
        assert!(internal_no_addr.peer.is_none());

        // A public transport address never becomes a relay identity.
        let public = SessionContext::from_tags(None, &ConnectionTags::new(), Some(addr));
        assert_eq!(public.interface, SessionInterface::Public);
        assert!(public.scope().is_none());
        assert!(public.peer.is_none());
        assert!(public.coordinator_context().source.is_none());
    }

    #[test]
    fn coordinator_context_forwards_interface_and_peer() {
        let addr: SocketAddr = "10.0.0.7:4443".parse().unwrap();
        let internal = SessionContext::from_tags(
            None,
            &ConnectionTags::new().with_interface(SessionInterface::Internal),
            Some(addr),
        );
        let ctx = internal.coordinator_context();
        assert_eq!(ctx.interface, SessionInterface::Internal);
        assert_eq!(ctx.source.expect("source should be set").addr, Some(addr));

        let public = SessionContext::public(None);
        let ctx = public.coordinator_context();
        assert_eq!(ctx.interface, SessionInterface::Public);
        assert!(ctx.source.is_none());
    }

    /// Relay correlation relies on span fields reaching formatted output rather
    /// than on each call site naming `session_id`. That only holds if the
    /// subscriber renders span context, so assert it end to end: a bare
    /// `tracing::info!` with no fields of its own must still carry the id.
    #[test]
    fn session_span_fields_reach_a_record_that_names_none_of_them() {
        let addr: SocketAddr = "10.0.0.7:4443".parse().unwrap();
        let context = SessionContext::from_tags(
            Some("scope-z".to_string()),
            &ConnectionTags::new().with_interface(SessionInterface::Internal),
            Some(addr),
        );
        let session_id = SessionId::new("deadbeefcafe1234");

        let out = capture_output("info", || {
            let span = context.span(&session_id);
            // A record naming none of the correlation fields itself.
            futures::executor::block_on(
                async {
                    tracing::info!("session established");
                }
                .instrument(span),
            );
        });

        assert!(
            out.contains("deadbeefcafe1234"),
            "session id missing from output: {out}"
        );
        assert!(
            out.contains("internal"),
            "interface missing from output: {out}"
        );
        // scope is Some("scope-z") which has no leading slash, so relay_uid()
        // returns "-" (non-path scopes are hidden to prevent credential leaks).
        // The tracing formatter quotes the string value.
        assert!(
            out.contains(r#"relay_uid="-""#),
            "relay_uid missing from output: {out}"
        );
        // scope_present is the companion boolean: scope is Some(...) → true.
        assert!(
            out.contains("scope_present=true"),
            "scope_present missing from output: {out}"
        );
        assert!(
            !out.contains("10.0.0.7:4443"),
            "peer address unexpectedly present: {out}"
        );
    }

    #[test]
    fn session_span_never_formats_scope_or_relay_url_secrets() {
        const SCOPE_SECRET: &str = "scope-credential-bearer-7f31";
        const URL_SECRET: &str = "relay-url-secret-path-9ac2";

        let url = Url::parse(&format!("https://relay.example.com/{URL_SECRET}")).unwrap();
        let context = SessionContext::internal(
            Some(SCOPE_SECRET.to_string()),
            Some(RelayInfo::with_addr(url, "10.0.0.9:4443".parse().unwrap())),
        );
        let session_id = SessionId::new("secret-redaction-session");

        let out = capture_output("info", || {
            futures::executor::block_on(
                async {
                    tracing::info!("secret-free session event");
                }
                .instrument(context.span(&session_id)),
            );
        });

        assert!(
            !out.contains(SCOPE_SECRET),
            "scope secret leaked into output: {out}"
        );
        assert!(
            !out.contains(URL_SECRET),
            "relay URL secret leaked into output: {out}"
        );
        assert!(
            !out.contains("10.0.0.9:4443"),
            "peer address unexpectedly present: {out}"
        );
    }

    #[test]
    fn session_span_fields_survive_warn_and_error_filters() {
        let context = SessionContext::public(None);
        let session_id = SessionId::new("filtered-session-correlation");

        let warn_out = capture_output("warn", || {
            futures::executor::block_on(
                async {
                    tracing::warn!("warning under warn filter");
                }
                .instrument(context.span(&session_id)),
            );
        });
        assert!(
            warn_out.contains("filtered-session-correlation"),
            "session id missing under warn filter: {warn_out}"
        );

        let error_out = capture_output("error", || {
            futures::executor::block_on(
                async {
                    tracing::error!("error under error filter");
                }
                .instrument(context.span(&session_id)),
            );
        });
        assert!(
            error_out.contains("filtered-session-correlation"),
            "session id missing under error filter: {error_out}"
        );
    }

    #[test]
    fn session_span_fields_survive_target_specific_filters() {
        let context = SessionContext::public(None);
        let session_id = SessionId::new("target-filtered-session-correlation");

        let out = capture_output(
            "off,moq_relay_ietf::session=warn,moq_relay_ietf::consumer=warn",
            || {
                futures::executor::block_on(
                    async {
                        tracing::warn!(
                            target: "moq_relay_ietf::consumer",
                            "consumer warning under target filter"
                        );
                    }
                    .instrument(context.span(&session_id)),
                );
            },
        );

        assert!(
            out.contains("target-filtered-session-correlation"),
            "session id missing under target-specific filter: {out}"
        );
    }

    fn assert_session_span_level(filter: &str, expected: tracing::Level) {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let context = SessionContext::public(None);
            let session_id = SessionId::new("metadata-level-session");
            let span = context.span(&session_id);
            let metadata = span.metadata().expect("session span should be enabled");
            assert_eq!(*metadata.level(), expected);
        });
    }

    #[test]
    fn session_span_uses_enabled_filter_level_for_metadata() {
        assert_session_span_level("info", tracing::Level::INFO);
        assert_session_span_level("warn", tracing::Level::WARN);
        assert_session_span_level("error", tracing::Level::ERROR);
    }

    #[test]
    fn session_span_does_not_inherit_the_current_session() {
        let downstream = SessionContext::public(None);
        let upstream = SessionContext::internal(None, None);
        let downstream_id = SessionId::new("outer-downstream-session");
        let upstream_id = SessionId::new("nested-upstream-session");

        let out = capture_output("warn", || {
            let downstream_span = downstream.span(&downstream_id);
            let _downstream_guard = downstream_span.enter();

            // Construct and enter the upstream span while the downstream span
            // is current; it must still be an independent root.
            let upstream_span = upstream.span(&upstream_id);
            let _upstream_guard = upstream_span.enter();
            tracing::warn!("upstream session event");
        });

        assert!(
            out.contains("nested-upstream-session"),
            "upstream session id missing from output: {out}"
        );
        assert!(
            !out.contains("outer-downstream-session"),
            "upstream span inherited downstream session id: {out}"
        );
    }

    /// The relay_uid is the first path segment of the scope URL. The JWT
    /// credential in later segments must never appear in log output.
    #[test]
    fn session_span_relay_uid_strips_jwt_segment() {
        // Scope with a JWT in the second segment — only the first should appear.
        let context = SessionContext::public(Some(
            "/relay-uid-abc/eyJhbGciOiJIUzI1NiJ9.secret".to_string(),
        ));
        let session_id = SessionId::generate();

        let out = capture_output("info", || {
            futures::executor::block_on(
                async {
                    tracing::info!("test");
                }
                .instrument(context.span(&session_id)),
            );
        });

        assert!(
            out.contains("relay-uid-abc"),
            "relay_uid missing from output: {out}"
        );
        assert!(
            !out.contains("eyJhbGciOiJIUzI1NiJ9"),
            "JWT token must not appear in log output: {out}"
        );
    }

    #[test]
    fn connection_meta_new_sets_fields() {
        let addr: SocketAddr = "203.0.113.5:4433".parse().unwrap();
        let meta = ConnectionMeta::new(
            Some(addr),
            Some("relay.example.com".to_string()),
            Some("/tenant/stream".to_string()),
        );
        assert_eq!(meta.remote_addr, Some(addr));
        assert_eq!(meta.server_name.as_deref(), Some("relay.example.com"));
        assert_eq!(meta.path.as_deref(), Some("/tenant/stream"));
        // local_ip is opt-in and defaults to None.
        assert_eq!(meta.local_ip, None);

        let local: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(meta.with_local_ip(Some(local)).local_ip, Some(local));
    }

    /// Example tagger that marks connections internal when they arrive on a
    /// known internal SNI, otherwise leaves them public. Mirrors how an
    /// embedder wires relay-to-relay classification.
    struct SniTagger {
        internal_server_name: &'static str,
    }

    impl ConnectionTagger for SniTagger {
        fn tag(&self, meta: &ConnectionMeta) -> ConnectionTags {
            match meta.server_name.as_deref() {
                Some(name) if name == self.internal_server_name => {
                    ConnectionTags::new().with_interface(SessionInterface::Internal)
                }
                _ => ConnectionTags::new(),
            }
        }
    }

    #[test]
    fn connection_tagger_classifies_by_server_name() {
        let tagger = SniTagger {
            internal_server_name: "mesh.internal",
        };

        let internal = tagger.tag(&ConnectionMeta::new(
            None,
            Some("mesh.internal".to_string()),
            None,
        ));
        assert_eq!(internal.interface(), SessionInterface::Internal);

        let public = tagger.tag(&ConnectionMeta::new(
            None,
            Some("public.example.com".to_string()),
            None,
        ));
        assert_eq!(public.interface(), SessionInterface::Public);

        // Missing SNI (e.g. no server name) defaults to public.
        let no_sni = tagger.tag(&ConnectionMeta::new(None, None, None));
        assert_eq!(no_sni.interface(), SessionInterface::Public);
    }

    /// Example tagger that marks connections internal when accepted on a known
    /// internal-facing local IP (e.g. a private VIP), otherwise public.
    /// Exercises the `local_ip` plumbing all the way to a tagger.
    struct LocalIpTagger {
        internal_local_ip: IpAddr,
    }

    impl ConnectionTagger for LocalIpTagger {
        fn tag(&self, meta: &ConnectionMeta) -> ConnectionTags {
            match meta.local_ip {
                Some(ip) if ip == self.internal_local_ip => {
                    ConnectionTags::new().with_interface(SessionInterface::Internal)
                }
                _ => ConnectionTags::new(),
            }
        }
    }

    #[test]
    fn connection_tagger_classifies_by_local_ip() {
        let internal_vip: IpAddr = "10.0.0.9".parse().unwrap();
        let public_vip: IpAddr = "203.0.113.9".parse().unwrap();
        let tagger = LocalIpTagger {
            internal_local_ip: internal_vip,
        };

        // Accepted on the internal VIP -> internal.
        let internal =
            tagger.tag(&ConnectionMeta::new(None, None, None).with_local_ip(Some(internal_vip)));
        assert_eq!(internal.interface(), SessionInterface::Internal);

        // Accepted on a public VIP -> public.
        let public =
            tagger.tag(&ConnectionMeta::new(None, None, None).with_local_ip(Some(public_vip)));
        assert_eq!(public.interface(), SessionInterface::Public);

        // Unknown local IP defaults to public.
        let unknown = tagger.tag(&ConnectionMeta::new(None, None, None));
        assert_eq!(unknown.interface(), SessionInterface::Public);
    }
}
