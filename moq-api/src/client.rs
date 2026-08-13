// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

use url::Url;

use crate::{ApiError, Origin};

/// How long an idle connection is kept in the pool.
///
/// Origin lookups can be sparse while still being latency sensitive, so an
/// aggressive timeout just means the next lookup pays for a fresh TCP connect
/// and TLS handshake before the request is even sent.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Idle connections kept per host. All requests target a single API host, so a
/// small pool absorbs concurrent lookups without holding many sockets open.
const POOL_MAX_IDLE_PER_HOST: usize = 8;

/// TCP keepalive interval, so a connection sitting idle in the pool is not
/// silently dropped by a NAT or load balancer between requests.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Client {
    // The address of the moq-api server
    url: Url,

    client: reqwest::Client,
}

impl Client {
    pub fn new(url: Url) -> Self {
        // Mirrors reqwest::Client::new(): building only fails if the TLS backend or the
        // system resolver cannot be initialized, neither of which these options affect.
        let client = reqwest::Client::builder()
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .tcp_keepalive(TCP_KEEPALIVE)
            .build()
            .expect("failed to build HTTP client");

        Self { url, client }
    }

    pub async fn get_origin(&self, namespace: &str) -> Result<Option<Origin>, ApiError> {
        let url = self.url.join(&format!("origin/{namespace}"))?;
        let resp = self.client.get(url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let origin: Origin = resp.json().await?;
        Ok(Some(origin))
    }

    pub async fn set_origin(&self, namespace: &str, origin: Origin) -> Result<(), ApiError> {
        let url = self.url.join(&format!("origin/{namespace}"))?;

        let resp = self.client.post(url).json(&origin).send().await?;
        resp.error_for_status()?;

        Ok(())
    }

    pub async fn delete_origin(&self, namespace: &str) -> Result<(), ApiError> {
        let url = self.url.join(&format!("origin/{namespace}"))?;

        let resp = self.client.delete(url).send().await?;
        resp.error_for_status()?;

        Ok(())
    }

    pub async fn patch_origin(&self, namespace: &str, origin: Origin) -> Result<(), ApiError> {
        let url = self.url.join(&format!("origin/{namespace}"))?;

        let resp = self.client.patch(url).json(&origin).send().await?;
        resp.error_for_status()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_builds_client() {
        let url = Url::parse("http://localhost:4442/").unwrap();
        let client = Client::new(url.clone());

        assert_eq!(client.url, url);
    }
}
