// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::session::{Publisher, SessionError, Subscriber};

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
    /// The resolved scope identity for this session, if any.
    pub scope: Option<String>,

    /// Whether this session is public client-facing or internal relay-to-relay.
    pub interface: SessionInterface,

    /// Relay endpoint for internal sessions, when known.
    ///
    /// Populated for outbound connections the relay dials itself, where the
    /// destination URL is known. Inbound connections leave this `None` even
    /// when classified internal, since only the remote socket address is known.
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
    /// [`RelayInfo::from_socket_addr`]). Public connections — and internal
    /// connections with no known address — leave `peer` as `None`.
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

    /// The resolved scope identity, if any.
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
    use url::Url;

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

    #[test]
    fn session_context_public_has_no_peer() {
        let context = SessionContext::public(Some("scope-a".to_string()));
        assert_eq!(context.scope(), Some("scope-a"));
        assert_eq!(context.interface, SessionInterface::Public);
        assert!(context.peer.is_none());
    }

    #[test]
    fn session_context_internal_carries_peer() {
        let url = Url::parse("https://relay.example.com/live").unwrap();
        let context = SessionContext::internal(
            Some("scope-b".to_string()),
            Some(RelayInfo::new(url.clone())),
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

        // Public → never carries a peer, even with a known address.
        let public = SessionContext::from_tags(None, &ConnectionTags::new(), Some(addr));
        assert_eq!(public.interface, SessionInterface::Public);
        assert!(public.scope().is_none());
        assert!(public.peer.is_none());
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
