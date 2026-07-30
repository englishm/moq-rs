// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use moq_transport::{
    coding::{TrackNamespace, TrackNamespacePrefix},
    message::SubscribeOptions,
    session::{NamespaceEvent, SubscribeNamespace},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use url::Url;

use crate::{
    covering_prefix_set::{CoveringPrefixSet, RootDelta},
    Coordinator, CoordinatorContext, Locals, NamespaceSubscription, RelayInfo, RemoteManager,
    RemoteNamespaceRegistration, SessionContext, SessionInterface,
};

const TRANSITION_TIMEOUT: Duration = Duration::from_secs(60);
const CANCELLED_STREAM_CODE: u32 = 0x1;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(100);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Eq, Hash, PartialEq)]
struct GroupKey {
    scope: Option<String>,
    interface: SessionInterface,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct PrefixKey {
    group: GroupKey,
    prefix: TrackNamespacePrefix,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RelayKey {
    url: Url,
    addr: Option<SocketAddr>,
}

impl From<&RelayInfo> for RelayKey {
    fn from(relay: &RelayInfo) -> Self {
        Self {
            url: relay.url.clone(),
            addr: relay.addr,
        }
    }
}

struct InterestSlot {
    generation: u64,
    interest: Weak<PrefixInterest>,
}

struct LeaseRegistry {
    slots: Mutex<HashMap<PrefixKey, InterestSlot>>,
    next_generation: AtomicU64,
    commands: mpsc::UnboundedSender<Command>,
}

struct PrefixInterest {
    key: PrefixKey,
    generation: u64,
    registry: Weak<LeaseRegistry>,
}

impl Drop for PrefixInterest {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let Ok(mut slots) = registry.slots.lock() else {
            tracing::error!("upstream namespace lease registry lock poisoned");
            return;
        };

        let current = slots
            .get(&self.key)
            .is_some_and(|slot| slot.generation == self.generation);
        if !current {
            return;
        }

        slots.remove(&self.key);
        let _ = registry.commands.send(Command::Unsubscribe {
            key: self.key.clone(),
            generation: self.generation,
        });
    }
}

/// Keeps one exact downstream namespace-prefix interest alive.
pub(crate) struct PrefixLease {
    _interest: Arc<PrefixInterest>,
}

/// Relay-wide handle for shared upstream namespace discovery.
#[derive(Clone)]
pub(crate) struct UpstreamNamespaces {
    registry: Arc<LeaseRegistry>,
}

impl UpstreamNamespaces {
    pub(crate) fn new(
        locals: Locals,
        remotes: RemoteManager,
        coordinator: Arc<dyn Coordinator>,
    ) -> (Self, UpstreamNamespacesRunner) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let manager = Self::from_sender(commands.clone());
        let runner = UpstreamNamespacesRunner {
            receiver,
            commands: commands.downgrade(),
            locals,
            remotes,
            coordinator,
            covering: HashMap::new(),
            active: HashMap::new(),
            roots: HashMap::new(),
            root_ids: HashMap::new(),
            next_root_id: 0,
            next_pull_id: 0,
            transition: None,
            deferred: VecDeque::new(),
        };
        (manager, runner)
    }

    fn from_sender(commands: mpsc::UnboundedSender<Command>) -> Self {
        Self {
            registry: Arc::new(LeaseRegistry {
                slots: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(0),
                commands,
            }),
        }
    }

    pub(crate) fn subscribe(
        &self,
        context: &SessionContext,
        prefix: TrackNamespacePrefix,
    ) -> anyhow::Result<PrefixLease> {
        let key = PrefixKey {
            group: GroupKey {
                scope: context.scope.clone(),
                interface: context.interface,
            },
            prefix,
        };
        let mut slots = self
            .registry
            .slots
            .lock()
            .map_err(|_| anyhow::anyhow!("upstream namespace lease registry lock poisoned"))?;

        if let Some(interest) = slots.get(&key).and_then(|slot| slot.interest.upgrade()) {
            return Ok(PrefixLease {
                _interest: interest,
            });
        }

        let generation = self
            .registry
            .next_generation
            .fetch_add(1, Ordering::Relaxed);
        let interest = Arc::new(PrefixInterest {
            key: key.clone(),
            generation,
            registry: Arc::downgrade(&self.registry),
        });
        slots.insert(
            key.clone(),
            InterestSlot {
                generation,
                interest: Arc::downgrade(&interest),
            },
        );

        let send_result = self.registry.commands.send(Command::Subscribe {
            key: key.clone(),
            generation,
            context: context.coordinator_context(),
        });
        if send_result.is_err() {
            slots.remove(&key);
            drop(slots);
            return Err(anyhow::anyhow!("upstream namespace manager is closed"));
        }

        Ok(PrefixLease {
            _interest: interest,
        })
    }
}

enum Command {
    Subscribe {
        key: PrefixKey,
        generation: u64,
        context: CoordinatorContext,
    },
    Unsubscribe {
        key: PrefixKey,
        generation: u64,
    },
    PullEvent {
        root_id: u64,
        pull_id: u64,
        event: NamespaceEvent,
    },
    PullEnded {
        root_id: u64,
        pull_id: u64,
        outcome: PullOutcome,
    },
}

struct ActiveInterest {
    generation: u64,
    context: CoordinatorContext,
}

struct PullState {
    relay: RelayKey,
    cancel: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Drop for PullState {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RootState {
    id: u64,
    key: PrefixKey,
    _subscription: Option<NamespaceSubscription>,
    relays: Vec<RelayInfo>,
    pulls: HashMap<u64, PullState>,
    sources: HashMap<u64, HashMap<TrackNamespace, RemoteNamespaceRegistration>>,
    retained: HashMap<TrackNamespace, Vec<RemoteNamespaceRegistration>>,
    retiring: bool,
}

struct Transition {
    removed: HashSet<u64>,
    old_pulls: HashMap<u64, RelayKey>,
    relays: HashMap<RelayKey, RelayTransition>,
}

struct RelayTransition {
    waiting: HashSet<u64>,
    targets: Vec<(u64, RelayInfo)>,
    started: bool,
}

#[derive(Clone, Copy)]
enum PullOutcome {
    Intentional { timed_out: bool },
    Rejected,
    OpenFailed,
    Ended,
}

pub(crate) struct UpstreamNamespacesRunner {
    receiver: mpsc::UnboundedReceiver<Command>,
    commands: mpsc::WeakUnboundedSender<Command>,
    locals: Locals,
    remotes: RemoteManager,
    coordinator: Arc<dyn Coordinator>,
    covering: HashMap<GroupKey, CoveringPrefixSet>,
    active: HashMap<PrefixKey, ActiveInterest>,
    roots: HashMap<u64, RootState>,
    root_ids: HashMap<PrefixKey, u64>,
    next_root_id: u64,
    next_pull_id: u64,
    transition: Option<Transition>,
    deferred: VecDeque<Command>,
}

impl UpstreamNamespacesRunner {
    pub(crate) async fn run(mut self) {
        loop {
            let command = if self.transition.is_none() {
                self.deferred.pop_front()
            } else {
                None
            };
            let command = match command {
                Some(command) => Some(command),
                None => self.receiver.recv().await,
            };
            let Some(command) = command else {
                return;
            };

            if self.transition.is_some()
                && matches!(
                    command,
                    Command::Subscribe { .. } | Command::Unsubscribe { .. }
                )
            {
                self.deferred.push_back(command);
                continue;
            }

            match command {
                Command::Subscribe {
                    key,
                    generation,
                    context,
                } => self.add_interest(key, generation, context).await,
                Command::Unsubscribe { key, generation } => {
                    self.remove_interest(key, generation).await
                }
                Command::PullEvent {
                    root_id,
                    pull_id,
                    event,
                } => self.handle_pull_event(root_id, pull_id, event),
                Command::PullEnded {
                    root_id,
                    pull_id,
                    outcome,
                } => self.handle_pull_ended(root_id, pull_id, outcome),
            }
        }
    }

    async fn add_interest(&mut self, key: PrefixKey, generation: u64, context: CoordinatorContext) {
        if self
            .active
            .get(&key)
            .is_some_and(|active| active.generation >= generation)
        {
            return;
        }
        let first = self
            .active
            .insert(
                key.clone(),
                ActiveInterest {
                    generation,
                    context,
                },
            )
            .is_none();
        if !first {
            return;
        }

        let delta = self
            .covering
            .entry(key.group.clone())
            .or_default()
            .add(key.prefix.clone());
        self.apply_delta(key.group, delta).await;
    }

    async fn remove_interest(&mut self, key: PrefixKey, generation: u64) {
        if self
            .active
            .get(&key)
            .is_none_or(|active| active.generation != generation)
        {
            return;
        }
        self.active.remove(&key);

        let Some(covering) = self.covering.get_mut(&key.group) else {
            return;
        };
        let delta = covering.remove(&key.prefix);
        self.apply_delta(key.group, delta).await;
    }

    async fn apply_delta(&mut self, group: GroupKey, delta: RootDelta) {
        if delta.added.is_empty() && delta.removed.is_empty() {
            return;
        }

        let removed = delta
            .removed
            .iter()
            .filter_map(|prefix| {
                self.root_ids
                    .get(&PrefixKey {
                        group: group.clone(),
                        prefix: prefix.clone(),
                    })
                    .copied()
            })
            .collect::<HashSet<_>>();

        let mut added = Vec::new();
        for prefix in delta.added {
            let key = PrefixKey {
                group: group.clone(),
                prefix,
            };
            added.push(self.prepare_root(key).await);
        }

        if removed.is_empty() {
            for root in added {
                self.start_root(root);
            }
            return;
        }

        for root_id in &removed {
            let Some(root) = self.roots.get_mut(root_id) else {
                continue;
            };
            root.retiring = true;

            let sources = std::mem::take(&mut root.sources);
            for registrations in sources.into_values() {
                for (namespace, registration) in registrations {
                    transfer_registration(namespace, registration, root, &mut added);
                }
            }
            let retained = std::mem::take(&mut root.retained);
            for (namespace, registrations) in retained {
                for registration in registrations {
                    transfer_registration(namespace.clone(), registration, root, &mut added);
                }
            }
        }

        let added_ids = added.iter().map(|root| root.id).collect::<HashSet<_>>();
        for root in added {
            let root_id = root.id;
            self.root_ids.insert(root.key.clone(), root_id);
            self.roots.insert(root_id, root);
        }

        let old = removed.iter().flat_map(|root_id| {
            self.roots
                .get(root_id)
                .into_iter()
                .flat_map(|root| root.pulls.iter())
                .map(|(pull_id, pull)| (*pull_id, pull.relay.clone()))
        });
        let targets = added_ids.iter().flat_map(|root_id| {
            self.roots
                .get(root_id)
                .into_iter()
                .flat_map(|root| root.relays.iter())
                .map(|relay| (*root_id, relay.clone()))
        });
        let (old_pulls, relays) = plan_relay_transitions(old, targets);

        self.transition = Some(Transition {
            removed: removed.clone(),
            old_pulls,
            relays,
        });

        let mut completed_pulls = Vec::new();
        for root_id in removed {
            let Some(root) = self.roots.get_mut(&root_id) else {
                continue;
            };
            let mut completed = Vec::new();
            for (pull_id, pull) in &mut root.pulls {
                let sent = pull
                    .cancel
                    .take()
                    .is_some_and(|cancel| cancel.send(()).is_ok());
                if !sent {
                    completed.push(*pull_id);
                }
            }
            for pull_id in completed {
                root.pulls.remove(&pull_id);
                completed_pulls.push(pull_id);
            }
        }
        for pull_id in completed_pulls {
            self.mark_old_pull_complete(pull_id);
        }

        self.start_ready_relay_transitions();
        self.finish_transition_if_ready();
    }

    async fn prepare_root(&mut self, key: PrefixKey) -> RootState {
        let id = self.next_root_id;
        self.next_root_id = self.next_root_id.wrapping_add(1);
        let context = self
            .active
            .get(&key)
            .map(|active| active.context.clone())
            .unwrap_or_default();

        let subscription = match self
            .coordinator
            .subscribe_namespace(key.group.scope.as_deref(), &key.prefix, &context)
            .await
        {
            Ok(subscription) => Some(subscription),
            Err(error) => {
                tracing::error!(
                    scope = key.group.scope.as_deref().unwrap_or("<unscoped>"),
                    prefix = %key.prefix,
                    error = %error,
                    "failed to register shared upstream namespace interest"
                );
                None
            }
        };
        let relays = subscription
            .as_ref()
            .map(|subscription| subscription.upstream_relays.clone())
            .unwrap_or_default();

        RootState {
            id,
            key,
            _subscription: subscription,
            relays,
            pulls: HashMap::new(),
            sources: HashMap::new(),
            retained: HashMap::new(),
            retiring: false,
        }
    }

    fn start_root(&mut self, root: RootState) {
        let root_id = root.id;
        let key = root.key.clone();
        let relays = root.relays.clone();
        self.root_ids.insert(key, root_id);
        self.roots.insert(root_id, root);

        for relay in relays {
            self.start_pull(root_id, relay);
        }
    }

    fn start_pull(&mut self, root_id: u64, relay: RelayInfo) {
        let Some(root) = self.roots.get(&root_id) else {
            return;
        };
        let prefix = root.key.prefix.clone();
        let pull_id = self.next_pull_id;
        self.next_pull_id = self.next_pull_id.wrapping_add(1);
        let (cancel, recv_cancel) = oneshot::channel();
        let relay_key = RelayKey::from(&relay);
        let task = tokio::spawn(run_pull(
            self.remotes.clone(),
            self.commands.clone(),
            root_id,
            pull_id,
            relay,
            prefix,
            recv_cancel,
        ));
        if let Some(root) = self.roots.get_mut(&root_id) {
            root.pulls.insert(
                pull_id,
                PullState {
                    relay: relay_key,
                    cancel: Some(cancel),
                    task,
                },
            );
        }
    }

    fn handle_pull_event(&mut self, root_id: u64, pull_id: u64, event: NamespaceEvent) {
        let Some(root) = self.roots.get_mut(&root_id) else {
            return;
        };
        if root.retiring || !root.pulls.contains_key(&pull_id) {
            return;
        }

        match event {
            NamespaceEvent::Added(namespace) => {
                if !root.key.prefix.is_prefix_of(&namespace) {
                    return;
                }
                let sources = root.sources.entry(pull_id).or_default();
                if sources.contains_key(&namespace) {
                    return;
                }
                match self
                    .locals
                    .register_remote_namespace(root.key.group.scope.as_deref(), namespace.clone())
                {
                    Ok(registration) => {
                        sources.insert(namespace.clone(), registration);
                        root.retained.remove(&namespace);
                    }
                    Err(error) => {
                        tracing::error!(
                            namespace = %namespace,
                            error = %error,
                            "failed to register upstream namespace source"
                        );
                    }
                }
            }
            NamespaceEvent::Removed(namespace) => {
                if let Some(sources) = root.sources.get_mut(&pull_id) {
                    sources.remove(&namespace);
                }
            }
        }
    }

    fn handle_pull_ended(&mut self, root_id: u64, pull_id: u64, outcome: PullOutcome) {
        let Some(root) = self.roots.get_mut(&root_id) else {
            return;
        };
        root.pulls.remove(&pull_id);
        root.sources.remove(&pull_id);

        match outcome {
            PullOutcome::Intentional { timed_out: true } => {
                tracing::error!(root_id, pull_id, "upstream namespace transition timed out");
            }
            PullOutcome::Rejected => {
                tracing::warn!(root_id, pull_id, "upstream namespace request was rejected");
            }
            PullOutcome::OpenFailed => {
                tracing::warn!(
                    root_id,
                    pull_id,
                    "upstream namespace request failed to open"
                );
            }
            PullOutcome::Intentional { timed_out: false } | PullOutcome::Ended => {}
        }

        self.mark_old_pull_complete(pull_id);
        self.start_ready_relay_transitions();
        self.finish_transition_if_ready();
    }

    fn mark_old_pull_complete(&mut self, pull_id: u64) {
        let Some(transition) = self.transition.as_mut() else {
            return;
        };
        complete_old_pull(transition, pull_id);
    }

    fn start_ready_relay_transitions(&mut self) {
        let ready = self
            .transition
            .as_mut()
            .map(take_ready_targets)
            .unwrap_or_default();

        for (root_id, relay) in ready {
            self.start_pull(root_id, relay);
        }
    }

    fn finish_transition_if_ready(&mut self) {
        let ready = self
            .transition
            .as_ref()
            .is_some_and(|transition| transition.old_pulls.is_empty());
        if !ready {
            return;
        }

        let Some(transition) = self.transition.take() else {
            return;
        };
        for root_id in transition.removed {
            if let Some(root) = self.roots.remove(&root_id) {
                self.root_ids.remove(&root.key);
            }
        }
    }
}

fn complete_old_pull(transition: &mut Transition, pull_id: u64) {
    let Some(relay) = transition.old_pulls.remove(&pull_id) else {
        return;
    };
    if let Some(relay_transition) = transition.relays.get_mut(&relay) {
        relay_transition.waiting.remove(&pull_id);
    }
}

fn plan_relay_transitions(
    old: impl IntoIterator<Item = (u64, RelayKey)>,
    targets: impl IntoIterator<Item = (u64, RelayInfo)>,
) -> (HashMap<u64, RelayKey>, HashMap<RelayKey, RelayTransition>) {
    let mut old_pulls = HashMap::new();
    let mut relays = HashMap::<RelayKey, RelayTransition>::new();
    for (pull_id, relay) in old {
        old_pulls.insert(pull_id, relay.clone());
        relays
            .entry(relay)
            .or_insert_with(|| RelayTransition {
                waiting: HashSet::new(),
                targets: Vec::new(),
                started: false,
            })
            .waiting
            .insert(pull_id);
    }
    for (root_id, relay) in targets {
        relays
            .entry(RelayKey::from(&relay))
            .or_insert_with(|| RelayTransition {
                waiting: HashSet::new(),
                targets: Vec::new(),
                started: false,
            })
            .targets
            .push((root_id, relay));
    }
    (old_pulls, relays)
}

fn take_ready_targets(transition: &mut Transition) -> Vec<(u64, RelayInfo)> {
    transition
        .relays
        .values_mut()
        .filter(|relay| !relay.started && relay.waiting.is_empty())
        .flat_map(|relay| {
            relay.started = true;
            std::mem::take(&mut relay.targets)
        })
        .collect()
}

fn transfer_registration(
    namespace: TrackNamespace,
    registration: RemoteNamespaceRegistration,
    old_root: &mut RootState,
    added: &mut [RootState],
) {
    if let Some(root) = added
        .iter_mut()
        .find(|root| root.key.prefix.is_prefix_of(&namespace))
    {
        root.retained
            .entry(namespace)
            .or_default()
            .push(registration);
    } else {
        old_root
            .retained
            .entry(namespace)
            .or_default()
            .push(registration);
    }
}

async fn run_pull(
    remotes: RemoteManager,
    commands: mpsc::WeakUnboundedSender<Command>,
    root_id: u64,
    pull_id: u64,
    relay: RelayInfo,
    prefix: TrackNamespacePrefix,
    mut cancel: oneshot::Receiver<()>,
) {
    let mut backoff = INITIAL_RETRY_BACKOFF;
    let outcome = loop {
        let outcome = run_pull_inner(
            &remotes,
            &commands,
            root_id,
            pull_id,
            &relay,
            prefix.clone(),
            &mut cancel,
        )
        .await;

        // Only requests that fail before a subscription is established are
        // retried, so no upstream namespace sources are ever left registered
        // across attempts. Cancellation and clean/streamed endings fall through.
        //
        // TODO(subscribe-namespace): this is an optimistic blanket retry. It
        // should branch on the rejection/error kind and, for a cleanly rejected
        // coalesced (broad) request, restore the narrower pulls that were
        // coalesced away instead of retrying the broad request indefinitely.
        if !is_retryable(outcome) {
            break outcome;
        }

        tokio::select! {
            _ = &mut cancel => break PullOutcome::Intentional { timed_out: false },
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = next_backoff(backoff);
    };

    send_command(
        &commands,
        Command::PullEnded {
            root_id,
            pull_id,
            outcome,
        },
    );
}

fn is_retryable(outcome: PullOutcome) -> bool {
    matches!(outcome, PullOutcome::Rejected | PullOutcome::OpenFailed)
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_BACKOFF)
}

async fn run_pull_inner(
    remotes: &RemoteManager,
    commands: &mpsc::WeakUnboundedSender<Command>,
    root_id: u64,
    pull_id: u64,
    relay: &RelayInfo,
    prefix: TrackNamespacePrefix,
    cancel: &mut oneshot::Receiver<()>,
) -> PullOutcome {
    let handle = tokio::select! {
        _ = &mut *cancel => return PullOutcome::Intentional { timed_out: false },
        result = remotes.subscribe_namespace(relay, prefix, SubscribeOptions::Namespace) => {
            match result {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(upstream = %relay.url, error = %error, "failed to open shared upstream SUBSCRIBE_NAMESPACE");
                    return PullOutcome::OpenFailed;
                }
            }
        }
    };
    let mut handle = handle;

    let accepted = tokio::select! {
        _ = &mut *cancel => return cancel_pull(&mut handle, relay).await,
        result = handle.ok() => result,
    };
    if let Err(error) = accepted {
        tracing::debug!(upstream = %relay.url, error = %error, "shared upstream SUBSCRIBE_NAMESPACE rejected");
        return PullOutcome::Rejected;
    }

    loop {
        tokio::select! {
            _ = &mut *cancel => return cancel_pull(&mut handle, relay).await,
            event = handle.next() => {
                match event {
                    Ok(Some(event)) => send_command(commands, Command::PullEvent { root_id, pull_id, event }),
                    Ok(None) => return PullOutcome::Ended,
                    Err(error) => {
                        tracing::debug!(upstream = %relay.url, error = %error, "shared upstream SUBSCRIBE_NAMESPACE ended");
                        return PullOutcome::Ended;
                    }
                }
            }
        }
    }
}

async fn cancel_pull(handle: &mut SubscribeNamespace, relay: &RelayInfo) -> PullOutcome {
    if let Err(error) = handle.finish_request() {
        tracing::debug!(upstream = %relay.url, error = %error, "failed to finish upstream namespace request stream");
        return PullOutcome::Intentional { timed_out: false };
    }

    let drain = async {
        loop {
            match handle.next().await {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    };
    if tokio::time::timeout(TRANSITION_TIMEOUT, drain)
        .await
        .is_ok()
    {
        return PullOutcome::Intentional { timed_out: false };
    }

    metrics::counter!("moq_relay_namespace_transition_timeouts_total").increment(1);
    tracing::error!(
        upstream = %relay.url,
        timeout_seconds = TRANSITION_TIMEOUT.as_secs(),
        "timed out draining upstream namespace transition; resetting request stream"
    );
    handle.reset_request(CANCELLED_STREAM_CODE);
    PullOutcome::Intentional { timed_out: true }
}

fn send_command(commands: &mpsc::WeakUnboundedSender<Command>, command: Command) {
    if let Some(commands) = commands.upgrade() {
        let _ = commands.send(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(name: &str) -> RelayInfo {
        RelayInfo::new(Url::parse(&format!("https://{name}.example.com/live")).expect("relay URL"))
    }

    #[test]
    fn duplicate_leases_share_automatic_ownership() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let manager = UpstreamNamespaces::from_sender(commands);
        let context = SessionContext::public(None);
        let prefix = TrackNamespacePrefix::from_utf8_path("foo/bar");

        let first = manager
            .subscribe(&context, prefix.clone())
            .expect("first lease");
        let first_generation = match receiver.try_recv().expect("first subscribe") {
            Command::Subscribe {
                key, generation, ..
            } => {
                assert_eq!(key.prefix, prefix);
                generation
            }
            _ => panic!("expected subscribe command"),
        };

        let second = manager
            .subscribe(&context, prefix.clone())
            .expect("second lease");
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        drop(first);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        drop(second);
        match receiver.try_recv().expect("last-owner unsubscribe") {
            Command::Unsubscribe {
                key, generation, ..
            } => {
                assert_eq!(key.prefix, prefix);
                assert_eq!(generation, first_generation);
            }
            _ => panic!("expected unsubscribe command"),
        }
    }

    #[test]
    fn recreated_interest_uses_a_new_generation() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let manager = UpstreamNamespaces::from_sender(commands);
        let context = SessionContext::public(None);
        let prefix = TrackNamespacePrefix::from_utf8_path("foo");

        let first = manager
            .subscribe(&context, prefix.clone())
            .expect("first lease");
        let first_generation = match receiver.try_recv().expect("first subscribe") {
            Command::Subscribe { generation, .. } => generation,
            _ => panic!("expected subscribe command"),
        };
        drop(first);
        let _ = receiver.try_recv().expect("first unsubscribe");

        let _second = manager.subscribe(&context, prefix).expect("second lease");
        let second_generation = match receiver.try_recv().expect("second subscribe") {
            Command::Subscribe { generation, .. } => generation,
            _ => panic!("expected subscribe command"),
        };

        assert_ne!(first_generation, second_generation);
    }

    #[test]
    fn relay_upgrades_progress_independently() {
        let relay_a = relay("a");
        let relay_b = relay("b");
        let relay_c = relay("c");
        let (old_pulls, relays) = plan_relay_transitions(
            [
                (10, RelayKey::from(&relay_a)),
                (20, RelayKey::from(&relay_b)),
            ],
            [
                (30, relay_a.clone()),
                (30, relay_b.clone()),
                (30, relay_c.clone()),
            ],
        );
        let mut transition = Transition {
            removed: HashSet::new(),
            old_pulls,
            relays,
        };

        let ready = take_ready_targets(&mut transition);
        assert_eq!(ready.len(), 1);
        assert_eq!(RelayKey::from(&ready[0].1), RelayKey::from(&relay_c));

        complete_old_pull(&mut transition, 10);
        let ready = take_ready_targets(&mut transition);
        assert_eq!(ready.len(), 1);
        assert_eq!(RelayKey::from(&ready[0].1), RelayKey::from(&relay_a));

        assert!(take_ready_targets(&mut transition).is_empty());
        complete_old_pull(&mut transition, 20);
        let ready = take_ready_targets(&mut transition);
        assert_eq!(ready.len(), 1);
        assert_eq!(RelayKey::from(&ready[0].1), RelayKey::from(&relay_b));
    }

    #[test]
    fn relay_downgrade_starts_all_non_overlapping_roots_together() {
        let relay = relay("origin");
        let (old_pulls, relays) = plan_relay_transitions(
            [(10, RelayKey::from(&relay))],
            [(20, relay.clone()), (30, relay)],
        );
        let mut transition = Transition {
            removed: HashSet::new(),
            old_pulls,
            relays,
        };

        assert!(take_ready_targets(&mut transition).is_empty());
        complete_old_pull(&mut transition, 10);
        let mut roots = take_ready_targets(&mut transition)
            .into_iter()
            .map(|(root_id, _)| root_id)
            .collect::<Vec<_>>();
        roots.sort_unstable();
        assert_eq!(roots, vec![20, 30]);
    }

    #[test]
    fn only_pre_subscription_failures_are_retried() {
        assert!(is_retryable(PullOutcome::Rejected));
        assert!(is_retryable(PullOutcome::OpenFailed));
        assert!(!is_retryable(PullOutcome::Ended));
        assert!(!is_retryable(PullOutcome::Intentional { timed_out: false }));
        assert!(!is_retryable(PullOutcome::Intentional { timed_out: true }));
    }

    #[test]
    fn backoff_doubles_and_saturates_at_max() {
        let mut backoff = INITIAL_RETRY_BACKOFF;
        let mut seen = Vec::new();
        for _ in 0..20 {
            seen.push(backoff);
            backoff = next_backoff(backoff);
        }

        for pair in seen.windows(2) {
            assert!(pair[1] >= pair[0], "backoff must be monotonic");
        }
        assert!(seen.iter().all(|delay| *delay <= MAX_RETRY_BACKOFF));
        assert_eq!(backoff, MAX_RETRY_BACKOFF);
        assert_eq!(next_backoff(MAX_RETRY_BACKOFF), MAX_RETRY_BACKOFF);
    }
}
