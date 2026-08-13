// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! File-based coordinator for multi-relay deployments.
//!
//! This coordinator uses a shared JSON file with file locking to coordinate
//! namespace registration across multiple relay instances. No separate
//! server process is required.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use fs2::FileExt;
use moq_native_ietf::quic::Client;
use moq_transport::coding::{TrackNamespace, TrackNamespacePrefix, TupleField};
use serde::{Deserialize, Serialize};
use url::Url;

use moq_relay_ietf::{
    Coordinator, CoordinatorContext, CoordinatorError, CoordinatorResult, NamespaceOrigin,
    NamespaceRegistration, NamespaceSubscription, RelayInfo, SessionInterface, TrackRegistration,
};

/// Data stored in the shared file
#[derive(Debug, Default, Serialize, Deserialize)]
struct CoordinatorData {
    /// Maps connection scope to namespace map
    #[serde(default)]
    namespaces: HashMap<String, HashMap<String, String>>,
    /// Maps connection scope to exact full-track map.
    /// Key format: `<namespace utf8 byte len>:<namespace utf8 path><track utf8/lossy>`.
    #[serde(default)]
    tracks: HashMap<String, HashMap<String, String>>,
    /// Maps connection scope to namespace-prefix interest map.
    /// Inner map: prefix key -> relay URL -> reference count.
    #[serde(default)]
    namespace_subscribers: HashMap<String, HashMap<String, HashMap<String, u64>>>,
}

impl CoordinatorData {
    fn scope_key(scope: Option<&str>) -> String {
        scope.unwrap_or("").to_string()
    }

    fn namespace_key(namespace: &TrackNamespace) -> String {
        tuple_key(&namespace.fields)
    }

    fn prefix_key(prefix: &TrackNamespacePrefix) -> String {
        tuple_key(&prefix.fields)
    }

    fn track_key(namespace: &TrackNamespace, track: &str) -> String {
        let namespace = namespace.to_utf8_path();
        format!("{}:{}{}", namespace.len(), namespace, track)
    }
}

/// Handle that unregisters a namespace when dropped
struct NamespaceUnregisterHandle {
    scope_key: String,
    namespace_key: String,
    file_path: PathBuf,
}

/// Handle that unregisters a track when dropped.
struct TrackUnregisterHandle {
    scope_key: String,
    track_key: String,
    file_path: PathBuf,
}

/// Handle that unregisters namespace interest when dropped.
struct NamespaceSubscriptionHandle {
    scope_key: String,
    prefix_key: String,
    relay_url: String,
    file_path: PathBuf,
}

impl Drop for NamespaceSubscriptionHandle {
    fn drop(&mut self) {
        if let Err(err) = unregister_namespace_subscription_sync(
            &self.file_path,
            &self.scope_key,
            &self.prefix_key,
            &self.relay_url,
        ) {
            tracing::warn!(prefix = %self.prefix_key, relay_url = %self.relay_url, error = %err, "failed to unregister namespace subscription on drop: {}", err);
        }
    }
}

impl Drop for TrackUnregisterHandle {
    fn drop(&mut self) {
        if let Err(err) = unregister_track_sync(&self.file_path, &self.scope_key, &self.track_key) {
            tracing::warn!(track = %self.track_key, error = %err, "failed to unregister track on drop: {}", err);
        }
    }
}

impl Drop for NamespaceUnregisterHandle {
    fn drop(&mut self) {
        if let Err(err) =
            unregister_namespace_sync(&self.file_path, &self.scope_key, &self.namespace_key)
        {
            tracing::warn!(namespace = %self.namespace_key, error = %err, "failed to unregister namespace on drop: {}", err);
        }
    }
}

/// Synchronous helper for unregistering namespace (used in Drop)
fn unregister_namespace_sync(file_path: &Path, scope_key: &str, namespace_key: &str) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(file_path)?;

    file.lock_exclusive()?;

    let mut data = read_data(&file)?;
    tracing::debug!(namespace = %namespace_key, scope = %scope_key, "unregistering namespace: {}", namespace_key);
    if let Some(bucket) = data.namespaces.get_mut(scope_key) {
        bucket.remove(namespace_key);
        if bucket.is_empty() {
            data.namespaces.remove(scope_key);
        }
    }

    write_data(&file, &data)?;
    file.unlock()?;

    Ok(())
}

fn unregister_track_sync(file_path: &Path, scope_key: &str, track_key: &str) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(file_path)?;

    file.lock_exclusive()?;

    let mut data = read_data(&file)?;
    tracing::debug!(track = %track_key, scope = %scope_key, "unregistering track: {}", track_key);
    if let Some(bucket) = data.tracks.get_mut(scope_key) {
        bucket.remove(track_key);
        if bucket.is_empty() {
            data.tracks.remove(scope_key);
        }
    }

    write_data(&file, &data)?;
    file.unlock()?;

    Ok(())
}

fn unregister_namespace_subscription_sync(
    file_path: &Path,
    scope_key: &str,
    prefix_key: &str,
    relay_url: &str,
) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(file_path)?;

    file.lock_exclusive()?;

    let mut data = read_data(&file)?;
    if let Some(scope_bucket) = data.namespace_subscribers.get_mut(scope_key) {
        if let Some(prefix_bucket) = scope_bucket.get_mut(prefix_key) {
            match prefix_bucket.get_mut(relay_url) {
                Some(count) if *count > 1 => *count -= 1,
                Some(_) => {
                    prefix_bucket.remove(relay_url);
                }
                None => {}
            }

            if prefix_bucket.is_empty() {
                scope_bucket.remove(prefix_key);
            }
        }

        if scope_bucket.is_empty() {
            data.namespace_subscribers.remove(scope_key);
        }
    }

    write_data(&file, &data)?;
    file.unlock()?;

    Ok(())
}

/// Read coordinator data from file
fn read_data(file: &File) -> Result<CoordinatorData> {
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    if contents.is_empty() {
        return Ok(CoordinatorData::default());
    }

    let data: CoordinatorData =
        serde_json::from_str(&contents).context("failed to parse coordinator data")?;
    Ok(data)
}

/// Write coordinator data to file
fn write_data(file: &File, data: &CoordinatorData) -> Result<()> {
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;

    let json = serde_json::to_string_pretty(data)?;
    file.write_all(json.as_bytes())?;
    file.flush()?;

    Ok(())
}

fn namespace_key_has_prefix(namespace_key: &str, prefix_key: &str) -> bool {
    let namespace_fields = key_fields(namespace_key);
    let prefix_fields = key_fields(prefix_key);
    prefix_fields.len() <= namespace_fields.len()
        && prefix_fields
            .iter()
            .zip(namespace_fields.iter())
            .all(|(prefix, namespace)| prefix == namespace)
}

fn tuple_key(fields: &[TupleField]) -> String {
    fields
        .iter()
        .map(|field| hex::encode(&field.value))
        .collect::<Vec<_>>()
        .join(".")
}

fn key_fields(key: &str) -> Vec<&str> {
    if key.is_empty() {
        Vec::new()
    } else {
        key.split('.').collect()
    }
}

fn key_field_count(key: &str) -> usize {
    key_fields(key).len()
}

fn decode_namespace_key(key: &str) -> Result<TrackNamespace> {
    let fields = key_fields(key)
        .into_iter()
        .map(|field| {
            hex::decode(field)
                .map(|value| TupleField { value })
                .context("failed to decode namespace key field")
        })
        .collect::<Result<Vec<_>>>()?;

    TrackNamespace::try_from(fields).context("failed to decode namespace key")
}

/// A coordinator that uses a shared file for state storage.
///
/// Multiple relay instances can use the same file to share namespace/track
/// registration data. File locking ensures safe concurrent access.
pub struct FileCoordinator {
    /// Path to the shared coordination file
    file_path: PathBuf,
    /// URL of this relay (used when registering namespaces)
    relay_url: Url,
}

impl FileCoordinator {
    /// Create a new file-based coordinator.
    ///
    /// # Arguments
    /// * `file_path` - Path to the shared coordination file
    /// * `relay_url` - URL of this relay instance (advertised to other relays)
    pub fn new(file_path: impl AsRef<Path>, relay_url: Url) -> Self {
        Self {
            file_path: file_path.as_ref().to_path_buf(),
            relay_url,
        }
    }
}

#[async_trait]
impl Coordinator for FileCoordinator {
    async fn register_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        _context: &CoordinatorContext,
    ) -> CoordinatorResult<NamespaceRegistration> {
        let scope_key = CoordinatorData::scope_key(scope);
        let namespace_key = CoordinatorData::namespace_key(namespace);
        let relay_url = self.relay_url.to_string();
        let file_path = self.file_path.clone();

        // Run blocking file I/O in a separate thread
        let scope_clone = scope_key.clone();
        let key_clone = namespace_key.clone();
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_path)?;

            file.lock_exclusive()?;

            let mut data = read_data(&file)?;
            tracing::info!(namespace = %key_clone, scope = %scope_clone, relay_url = %relay_url, "registering namespace: {} -> {}", key_clone, relay_url);
            data
                .namespaces
                .entry(scope_clone)
                .or_default()
                .insert(key_clone, relay_url);

            write_data(&file, &data)?;
            file.unlock()?;

            Ok::<_, anyhow::Error>(())
        })
        .await??;

        let handle = NamespaceUnregisterHandle {
            scope_key,
            namespace_key,
            file_path: self.file_path.clone(),
        };

        Ok(NamespaceRegistration::new(handle))
    }

    // FIXME(itzmanish): Not being called currently but we need to call this on publish_namespace_done
    // currently unregister happens on drop of namespace
    async fn unregister_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<()> {
        let scope_key = CoordinatorData::scope_key(scope);
        let namespace_key = CoordinatorData::namespace_key(namespace);
        let file_path = self.file_path.clone();

        tokio::task::spawn_blocking(move || {
            unregister_namespace_sync(&file_path, &scope_key, &namespace_key)
        })
        .await??;

        Ok(())
    }

    async fn lookup(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> CoordinatorResult<(NamespaceOrigin, Option<Client>)> {
        let namespace = namespace.clone();
        let scope_key = CoordinatorData::scope_key(scope);
        let namespace_key = CoordinatorData::namespace_key(&namespace);
        let file_path = self.file_path.clone();

        let result = tokio::task::spawn_blocking(
            move || -> Result<Option<(NamespaceOrigin, Option<Client>)>> {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&file_path)?;

                file.lock_shared()?;

                let data = read_data(&file)?;
                tracing::debug!(namespace = %namespace_key, scope = %scope_key, "looking up namespace: {}", namespace_key);

                let Some(bucket) = data.namespaces.get(&scope_key) else {
                    file.unlock()?;
                    return Ok(None);
                };

                // Try exact match first
                if let Some(relay_url) = bucket.get(&namespace_key) {
                    file.unlock()?;
                    let url = Url::parse(relay_url)?;
                    return Ok(Some((NamespaceOrigin::new(namespace, url, None), None)));
                }

                // Try prefix matching (find longest matching prefix)
                let mut best_match: Option<(String, String)> = None;
                for (registered_key, url) in bucket {
                    let is_prefix = namespace_key_has_prefix(&namespace_key, registered_key);
                    match best_match {
                        Some((ns, _))
                            if is_prefix && key_field_count(&ns) < key_field_count(registered_key) =>
                        {
                            best_match = Some((registered_key.clone(), url.clone()));
                        }
                        None if is_prefix => {
                            best_match = Some((registered_key.clone(), url.clone()));
                        }
                        _ => {}
                    }
                }

                file.unlock()?;

                if let Some((matched_key, relay_url)) = best_match {
                    let matched_ns = decode_namespace_key(&matched_key)?;
                    let url = Url::parse(&relay_url)?;
                    return Ok(Some((NamespaceOrigin::new(matched_ns, url, None), None)));
                }

                Ok(None)
            },
        )
        .await??;

        result.ok_or(CoordinatorError::NamespaceNotFound)
    }

    async fn subscribe_namespace(
        &self,
        scope: Option<&str>,
        prefix: &TrackNamespacePrefix,
        context: &CoordinatorContext,
    ) -> CoordinatorResult<NamespaceSubscription> {
        let scope_key = CoordinatorData::scope_key(scope);
        let prefix_key = CoordinatorData::prefix_key(prefix);
        let relay_url = self.relay_url.to_string();
        let file_path = self.file_path.clone();

        // Exclude the caller (this relay) and the inbound source peer so we
        // never return ourselves or the relay the SUBSCRIBE_NAMESPACE arrived
        // from as an upstream pull target. The caller string matches the value
        // format written by register_namespace (self.relay_url.to_string()).
        let caller = relay_url.clone();
        let source = context.source.as_ref().map(|relay| relay.url.to_string());
        let is_public = context.interface == SessionInterface::Public;

        let upstream_relays = tokio::task::spawn_blocking({
            let scope_key = scope_key.clone();
            let prefix_key = prefix_key.clone();
            let relay_url = relay_url.clone();
            move || -> Result<Vec<RelayInfo>> {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&file_path)?;

                file.lock_exclusive()?;

                let mut data = read_data(&file)?;

                // The distinct relays hosting a currently-matching namespace are
                // the upstream pull targets. Only public sessions fan out: an
                // internal (relay-to-relay) pull must not recurse, so it is
                // served from local state alone (empty upstream_relays breaks the
                // loop). Exclude ourselves and the inbound source peer so we
                // never pull from the caller or the relay the SUBSCRIBE_NAMESPACE
                // arrived from. Interest is still recorded below for the push
                // path regardless of interface.
                let mut upstream_relays = Vec::new();
                if is_public {
                    let mut seen_relays = HashSet::new();
                    if let Some(bucket) = data.namespaces.get(&scope_key) {
                        for (namespace_key, hosting_relay) in bucket {
                            if !namespace_key_has_prefix(namespace_key, &prefix_key) {
                                continue;
                            }
                            if hosting_relay == &caller || Some(hosting_relay) == source.as_ref() {
                                continue;
                            }
                            if seen_relays.insert(hosting_relay.clone()) {
                                upstream_relays.push(RelayInfo::new(Url::parse(hosting_relay)?));
                            }
                        }
                    }
                }

                let count = data
                    .namespace_subscribers
                    .entry(scope_key)
                    .or_default()
                    .entry(prefix_key)
                    .or_default()
                    .entry(relay_url)
                    .or_default();
                *count = count.saturating_add(1);

                write_data(&file, &data)?;

                file.unlock()?;

                Ok(upstream_relays)
            }
        })
        .await??;

        Ok(NamespaceSubscription::new(
            upstream_relays,
            NamespaceSubscriptionHandle {
                scope_key,
                prefix_key,
                relay_url,
                file_path: self.file_path.clone(),
            },
        ))
    }

    async fn lookup_namespace_subscribers(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        context: &CoordinatorContext,
    ) -> CoordinatorResult<Vec<RelayInfo>> {
        let scope_key = CoordinatorData::scope_key(scope);
        let namespace_key = CoordinatorData::namespace_key(namespace);
        let file_path = self.file_path.clone();

        // Exclude the caller (this relay) and the inbound source peer so a
        // forwarded PUBLISH_NAMESPACE is never sent to ourselves or echoed back
        // to the relay it just arrived from. The caller string matches the key
        // format written by subscribe_namespace (self.relay_url.to_string()).
        let caller = self.relay_url.to_string();
        let source = context.source.as_ref().map(|relay| relay.url.to_string());

        let subscribers = tokio::task::spawn_blocking(move || -> Result<Vec<RelayInfo>> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_path)?;

            file.lock_shared()?;

            let data = read_data(&file)?;
            let mut urls = HashSet::new();
            let mut subscribers = Vec::new();
            if let Some(bucket) = data.namespace_subscribers.get(&scope_key) {
                for (prefix_key, relays) in bucket {
                    if !namespace_key_has_prefix(&namespace_key, prefix_key) {
                        continue;
                    }

                    for relay_url in relays.keys() {
                        if relay_url == &caller || Some(relay_url) == source.as_ref() {
                            continue;
                        }
                        if urls.insert(relay_url.clone()) {
                            subscribers.push(RelayInfo::new(Url::parse(relay_url)?));
                        }
                    }
                }
            }

            file.unlock()?;

            Ok(subscribers)
        })
        .await??;

        Ok(subscribers)
    }

    async fn register_track(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        track: &str,
    ) -> CoordinatorResult<TrackRegistration> {
        let scope_key = CoordinatorData::scope_key(scope);
        let track_key = CoordinatorData::track_key(namespace, track);
        let relay_url = self.relay_url.to_string();
        let file_path = self.file_path.clone();

        let scope_clone = scope_key.clone();
        let key_clone = track_key.clone();
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_path)?;

            file.lock_exclusive()?;

            let mut data = read_data(&file)?;
            tracing::info!(track = %key_clone, scope = %scope_clone, relay_url = %relay_url, "registering track: {} -> {}", key_clone, relay_url);
            data.tracks
                .entry(scope_clone)
                .or_default()
                .insert(key_clone, relay_url);

            write_data(&file, &data)?;
            file.unlock()?;

            Ok::<_, anyhow::Error>(())
        })
        .await??;

        let handle = TrackUnregisterHandle {
            scope_key,
            track_key,
            file_path: self.file_path.clone(),
        };

        Ok(TrackRegistration::new(handle))
    }

    async fn unregister_track(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        track: &str,
    ) -> CoordinatorResult<()> {
        let scope_key = CoordinatorData::scope_key(scope);
        let track_key = CoordinatorData::track_key(namespace, track);
        let file_path = self.file_path.clone();

        tokio::task::spawn_blocking(move || {
            unregister_track_sync(&file_path, &scope_key, &track_key)
        })
        .await??;

        Ok(())
    }

    async fn lookup_track(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        track: &str,
    ) -> CoordinatorResult<(NamespaceOrigin, Option<Client>)> {
        let namespace = namespace.clone();
        let scope_key = CoordinatorData::scope_key(scope);
        let namespace_key = CoordinatorData::namespace_key(&namespace);
        let track_key = CoordinatorData::track_key(&namespace, track);
        let file_path = self.file_path.clone();

        let result = tokio::task::spawn_blocking(
            move || -> Result<Option<(NamespaceOrigin, Option<Client>)>> {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&file_path)?;

                file.lock_shared()?;

                let data = read_data(&file)?;
                tracing::debug!(track = %track_key, scope = %scope_key, "looking up track: {}", track_key);

                if let Some(relay_url) = data
                    .tracks
                    .get(&scope_key)
                    .and_then(|bucket| bucket.get(&track_key))
                {
                    let url = Url::parse(relay_url)?;
                    file.unlock()?;
                    return Ok(Some((NamespaceOrigin::new(namespace.clone(), url, None), None)));
                }

                let Some(bucket) = data.namespaces.get(&scope_key) else {
                    file.unlock()?;
                    return Ok(None);
                };

                if let Some(relay_url) = bucket.get(&namespace_key) {
                    let url = Url::parse(relay_url)?;
                    file.unlock()?;
                    return Ok(Some((NamespaceOrigin::new(namespace.clone(), url, None), None)));
                }

                let mut best_match: Option<(String, String)> = None;
                for (registered_key, url) in bucket {
                    let is_prefix = namespace_key_has_prefix(&namespace_key, registered_key);
                    match best_match {
                        Some((ns, _))
                            if is_prefix && key_field_count(&ns) < key_field_count(registered_key) =>
                        {
                            best_match = Some((registered_key.clone(), url.clone()));
                        }
                        None if is_prefix => {
                            best_match = Some((registered_key.clone(), url.clone()));
                        }
                        _ => {}
                    }
                }

                file.unlock()?;

                if let Some((matched_key, relay_url)) = best_match {
                    let matched_ns = decode_namespace_key(&matched_key)?;
                    let url = Url::parse(&relay_url)?;
                    return Ok(Some((NamespaceOrigin::new(matched_ns, url, None), None)));
                }

                Ok(None)
            },
        )
        .await??;

        result.ok_or(CoordinatorError::NamespaceNotFound)
    }

    async fn shutdown(&self) -> CoordinatorResult<()> {
        // Nothing to clean up - file will be unlocked automatically
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("moq-file-coordinator-{name}-{nanos}.json"))
    }

    fn prefix(path: &str) -> TrackNamespacePrefix {
        TrackNamespacePrefix::from_utf8_path(path)
    }

    fn namespace_from_fields(fields: &[&str]) -> TrackNamespace {
        TrackNamespace::try_from(
            fields
                .iter()
                .map(|field| TupleField::from_utf8(field))
                .collect::<Vec<_>>(),
        )
        .expect("test namespace should be valid")
    }

    #[tokio::test]
    async fn lookup_track_finds_registered_track() {
        let file = temp_file_path("track-lookup");
        let relay_url = Url::parse("https://relay.example.com").unwrap();
        let coordinator = FileCoordinator::new(&file, relay_url.clone());
        let namespace = TrackNamespace::from_utf8_path("room/123");

        let _track = coordinator
            .register_track(Some("scope-a"), &namespace, "audio")
            .await
            .expect("track should register");

        let (origin, client) = coordinator
            .lookup_track(Some("scope-a"), &namespace, "audio")
            .await
            .expect("track lookup should find registration");

        assert_eq!(origin.namespace(), &namespace);
        assert_eq!(origin.url(), relay_url);
        assert!(client.is_none());

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn lookup_track_distinguishes_separator_text_in_namespace_and_track() {
        let file = temp_file_path("track-key-collision");
        let relay_a = Url::parse("https://relay-a.example.com").unwrap();
        let relay_b = Url::parse("https://relay-b.example.com").unwrap();
        let coordinator_a = FileCoordinator::new(&file, relay_a.clone());
        let coordinator_b = FileCoordinator::new(&file, relay_b.clone());
        let namespace_a = TrackNamespace::from_utf8_path("room--audio");
        let namespace_b = TrackNamespace::from_utf8_path("room");

        let _track_a = coordinator_a
            .register_track(Some("scope-a"), &namespace_a, "main")
            .await
            .expect("first track should register");
        let _track_b = coordinator_b
            .register_track(Some("scope-a"), &namespace_b, "audio--main")
            .await
            .expect("second track should register");

        let (origin_a, _) = coordinator_a
            .lookup_track(Some("scope-a"), &namespace_a, "main")
            .await
            .expect("first track lookup should find original relay");
        let (origin_b, _) = coordinator_a
            .lookup_track(Some("scope-a"), &namespace_b, "audio--main")
            .await
            .expect("second track lookup should find original relay");

        assert_eq!(origin_a.url(), relay_a);
        assert_eq!(origin_b.url(), relay_b);

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn lookup_track_falls_back_to_namespace() {
        let file = temp_file_path("track-namespace-fallback");
        let relay_url = Url::parse("https://relay.example.com").unwrap();
        let coordinator = FileCoordinator::new(&file, relay_url.clone());
        let namespace = TrackNamespace::from_utf8_path("room/123");

        let _namespace = coordinator
            .register_namespace(Some("scope-a"), &namespace, &CoordinatorContext::public())
            .await
            .expect("namespace should register");

        let (origin, client) = coordinator
            .lookup_track(Some("scope-a"), &namespace, "audio")
            .await
            .expect("track lookup should fall back to namespace");

        assert_eq!(origin.namespace(), &namespace);
        assert_eq!(origin.url(), relay_url);
        assert!(client.is_none());

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn subscribe_namespace_returns_hosting_relays_for_matching_prefix() {
        let file = temp_file_path("subscribe-namespace-existing");
        let edge = FileCoordinator::new(&file, Url::parse("https://edge.example.com").unwrap());

        // The origin hosts two namespaces under the prefix; a third relay hosts
        // a non-matching namespace that must be filtered out.
        let origin = FileCoordinator::new(&file, Url::parse("https://origin.example.com").unwrap());
        let other_origin = FileCoordinator::new(
            &file,
            Url::parse("https://other-origin.example.com").unwrap(),
        );

        let _room = origin
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("room should register");
        let _camera = origin
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123/camera"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("camera should register");
        let _other = other_origin
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("other/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("other should register");

        // Subscribing to the prefix returns the distinct relays hosting a
        // matching namespace. The origin hosts two matches but is deduplicated
        // to a single entry; other-origin hosts only a non-matching namespace
        // and is excluded.
        let subscription = edge
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("namespace subscription should succeed");
        let upstream: Vec<String> = subscription
            .upstream_relays
            .iter()
            .map(|relay| relay.url.to_string())
            .collect();

        assert_eq!(upstream, vec!["https://origin.example.com/".to_string()]);

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn subscribe_namespace_keeps_scopes_isolated() {
        let file = temp_file_path("subscribe-namespace-scopes");
        let edge = FileCoordinator::new(&file, Url::parse("https://edge.example.com").unwrap());

        // Each scope's namespace is hosted by a different origin relay, so scope
        // isolation is observable through which relay is returned as an upstream
        // pull target.
        let origin_global = FileCoordinator::new(
            &file,
            Url::parse("https://origin-global.example.com").unwrap(),
        );
        let origin_scoped = FileCoordinator::new(
            &file,
            Url::parse("https://origin-scoped.example.com").unwrap(),
        );

        let _global = origin_global
            .register_namespace(
                None,
                &TrackNamespace::from_utf8_path("room/global"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("global should register");
        let _scoped = origin_scoped
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/scoped"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("scoped should register");

        // The global (unscoped) subscription only sees the global origin.
        let global = edge
            .subscribe_namespace(None, &prefix("room"), &CoordinatorContext::public())
            .await
            .expect("global namespace subscription should succeed");
        let global_upstream: Vec<String> = global
            .upstream_relays
            .iter()
            .map(|relay| relay.url.to_string())
            .collect();
        assert_eq!(
            global_upstream,
            vec!["https://origin-global.example.com/".to_string()]
        );

        // The scope-a subscription only sees the scoped origin.
        let scoped = edge
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("scoped namespace subscription should succeed");
        let scoped_upstream: Vec<String> = scoped
            .upstream_relays
            .iter()
            .map(|relay| relay.url.to_string())
            .collect();
        assert_eq!(
            scoped_upstream,
            vec!["https://origin-scoped.example.com/".to_string()]
        );

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_finds_matching_prefixes() {
        let file = temp_file_path("namespace-subscribers");
        let relay_url = Url::parse("https://relay.example.com").unwrap();
        let coordinator = FileCoordinator::new(&file, relay_url.clone());

        let _subscription = coordinator
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("namespace subscription should register");

        // A second relay (the origin) shares the same coordinator file and
        // performs the lookups, so the subscribing relay is returned rather
        // than filtered out as the caller.
        let looker = FileCoordinator::new(&file, Url::parse("https://origin.example.com").unwrap());

        let subscribers = looker
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123/camera"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");

        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].url, relay_url);

        let no_match = looker
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/456"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");
        assert!(no_match.is_empty());

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn namespace_subscription_drop_unregisters_with_refcount() {
        let file = temp_file_path("namespace-subscriber-refcount");
        let relay_url = Url::parse("https://relay.example.com").unwrap();
        let coordinator = FileCoordinator::new(&file, relay_url.clone());
        let namespace_prefix = prefix("room/123");
        let namespace = TrackNamespace::from_utf8_path("room/123/camera");

        let first = coordinator
            .subscribe_namespace(
                Some("scope-a"),
                &namespace_prefix,
                &CoordinatorContext::public(),
            )
            .await
            .expect("first subscription should register");
        let second = coordinator
            .subscribe_namespace(
                Some("scope-a"),
                &namespace_prefix,
                &CoordinatorContext::public(),
            )
            .await
            .expect("second subscription should register");

        drop(first);

        // A second relay (the origin) shares the coordinator file and performs
        // the lookups, so the subscribing relay is visible while its refcount
        // is positive and disappears once it hits zero.
        let looker = FileCoordinator::new(&file, Url::parse("https://origin.example.com").unwrap());

        let subscribers = looker
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &namespace,
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");
        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].url, relay_url);

        drop(second);

        let subscribers = looker
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &namespace,
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");
        assert!(subscribers.is_empty());

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn subscribe_namespace_empty_prefix_matches_all_namespaces() {
        let file = temp_file_path("subscribe-namespace-empty-prefix");
        let edge_url = Url::parse("https://edge.example.com").unwrap();
        let edge = FileCoordinator::new(&file, edge_url.clone());

        // Two different origin relays each host a namespace under the scope.
        let origin_a =
            FileCoordinator::new(&file, Url::parse("https://origin-a.example.com").unwrap());
        let origin_b =
            FileCoordinator::new(&file, Url::parse("https://origin-b.example.com").unwrap());

        let _room = origin_a
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("room should register");
        let _other = origin_b
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("other/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("other should register");

        // An empty prefix matches every namespace in the scope, so both hosting
        // relays are returned as upstream pull targets.
        let subscription = edge
            .subscribe_namespace(
                Some("scope-a"),
                &TrackNamespacePrefix::new(),
                &CoordinatorContext::public(),
            )
            .await
            .expect("empty prefix namespace subscription should succeed");
        let mut upstream: Vec<String> = subscription
            .upstream_relays
            .iter()
            .map(|relay| relay.url.to_string())
            .collect();
        upstream.sort();

        assert_eq!(
            upstream,
            vec![
                "https://origin-a.example.com/".to_string(),
                "https://origin-b.example.com/".to_string(),
            ]
        );

        // An origin performing the reverse lookup discovers the subscribing
        // edge; the origin (caller) is excluded, leaving the edge.
        let subscribers = origin_a
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123/camera"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");

        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].url, edge_url);

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn subscribe_namespace_matches_tuple_fields_not_slash_strings() {
        let file = temp_file_path("subscribe-namespace-tuple-prefix");
        let edge = FileCoordinator::new(&file, Url::parse("https://edge.example.com").unwrap());

        let single_field_namespace = namespace_from_fields(&["room/123"]);
        let two_field_namespace = namespace_from_fields(&["room", "123"]);

        // Host each namespace on a distinct origin so the matched namespace is
        // identifiable by which relay is returned as an upstream pull target.
        let origin_single = FileCoordinator::new(
            &file,
            Url::parse("https://origin-single.example.com").unwrap(),
        );
        let origin_two =
            FileCoordinator::new(&file, Url::parse("https://origin-two.example.com").unwrap());

        let _single = origin_single
            .register_namespace(
                Some("scope-a"),
                &single_field_namespace,
                &CoordinatorContext::public(),
            )
            .await
            .expect("single-field namespace should register");
        let _two = origin_two
            .register_namespace(
                Some("scope-a"),
                &two_field_namespace,
                &CoordinatorContext::public(),
            )
            .await
            .expect("two-field namespace should register");

        // The prefix has a single field "room". It matches the two-field
        // namespace ["room", "123"] but NOT the single-field namespace
        // ["room/123"], so only the two-field namespace's origin is returned
        // (tuple-aware matching, not slash-string matching).
        let subscription = edge
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("namespace subscription should succeed");

        let upstream: Vec<String> = subscription
            .upstream_relays
            .iter()
            .map(|relay| relay.url.to_string())
            .collect();

        assert_eq!(
            upstream,
            vec!["https://origin-two.example.com/".to_string()]
        );

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn internal_subscribe_namespace_returns_no_upstream_relays() {
        // A pull opened on another relay's behalf arrives as an internal
        // session, so the coordinator returns no upstream relays even though a
        // peer hosts a matching namespace: the pull is served from local state
        // and does not recurse (the loop-breaker). The source is a *third* relay
        // that does not host the namespace, so only the interface -- not
        // caller/source exclusion -- can explain the empty result.
        let file = temp_file_path("subscribe-namespace-internal");
        let edge = FileCoordinator::new(&file, Url::parse("https://edge.example.com").unwrap());
        let origin = FileCoordinator::new(&file, Url::parse("https://origin.example.com").unwrap());

        let _registration = origin
            .register_namespace(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("origin should register");

        // A public subscribe is told to pull from the origin...
        let public_sub = edge
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("public subscription should succeed");
        assert_eq!(
            public_sub.upstream_relays.len(),
            1,
            "public sessions fan out to hosting relays"
        );

        // ...but an internal subscribe is given nothing to pull.
        let internal_sub = edge
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room"),
                &CoordinatorContext::internal(Some(RelayInfo::new(
                    Url::parse("https://other.example.com").unwrap(),
                ))),
            )
            .await
            .expect("internal subscription should succeed");
        assert!(
            internal_sub.upstream_relays.is_empty(),
            "internal sessions are served from local state, breaking the pull loop"
        );

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_keeps_scopes_isolated() {
        let file = temp_file_path("namespace-subscriber-scopes");
        let relay_url = Url::parse("https://relay.example.com").unwrap();
        let coordinator = FileCoordinator::new(&file, relay_url);

        let _global = coordinator
            .subscribe_namespace(None, &prefix("room/global"), &CoordinatorContext::public())
            .await
            .expect("global subscription should register");
        let _scoped = coordinator
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room/scoped"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("scoped subscription should register");

        // A second relay (the origin) shares the coordinator file and performs
        // the lookups, so the subscribing relay is returned rather than filtered
        // out as the caller.
        let looker = FileCoordinator::new(&file, Url::parse("https://origin.example.com").unwrap());

        let global = looker
            .lookup_namespace_subscribers(
                None,
                &TrackNamespace::from_utf8_path("room/global/cam"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("global lookup should succeed");
        assert_eq!(global.len(), 1);

        let scoped = looker
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/scoped/cam"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("scoped lookup should succeed");
        assert_eq!(scoped.len(), 1);

        let no_cross_scope = looker
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/global/cam"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("scoped lookup should succeed");
        assert!(no_cross_scope.is_empty());

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_excludes_caller() {
        // A relay that subscribed and then performs the lookup itself must not
        // be returned to itself; otherwise it would forward PUBLISH_NAMESPACE to
        // its own endpoint and loop forever.
        let file = temp_file_path("namespace-subscribers-exclude-caller");
        let relay_url = Url::parse("https://relay.example.com").unwrap();
        let coordinator = FileCoordinator::new(&file, relay_url);

        let _subscription = coordinator
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("namespace subscription should register");

        let subscribers = coordinator
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123/camera"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");

        assert!(
            subscribers.is_empty(),
            "caller relay must be excluded from its own lookup results"
        );

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn lookup_namespace_subscribers_excludes_source() {
        // A PUBLISH_NAMESPACE that arrived from a peer relay must not be echoed
        // back to that same peer.
        let file = temp_file_path("namespace-subscribers-exclude-source");
        let edge_url = Url::parse("https://edge.example.com").unwrap();
        let edge = FileCoordinator::new(&file, edge_url.clone());
        let origin = FileCoordinator::new(&file, Url::parse("https://origin.example.com").unwrap());

        let _subscription = edge
            .subscribe_namespace(
                Some("scope-a"),
                &prefix("room/123"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("namespace subscription should register");

        // Without a source, the origin sees the edge subscriber.
        let subscribers = origin
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123/camera"),
                &CoordinatorContext::public(),
            )
            .await
            .expect("subscriber lookup should succeed");
        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].url, edge_url);

        // When the PUBLISH_NAMESPACE arrived *from* the edge relay, it must be
        // excluded so it is not echoed back.
        let from_edge = CoordinatorContext::internal(Some(RelayInfo::new(edge_url.clone())));
        let subscribers = origin
            .lookup_namespace_subscribers(
                Some("scope-a"),
                &TrackNamespace::from_utf8_path("room/123/camera"),
                &from_edge,
            )
            .await
            .expect("subscriber lookup should succeed");
        assert!(
            subscribers.is_empty(),
            "source relay must be excluded to avoid echoing PUBLISH_NAMESPACE back"
        );

        let _ = std::fs::remove_file(file);
    }
}
