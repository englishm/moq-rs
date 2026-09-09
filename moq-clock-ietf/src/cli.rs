// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::Parser;
use std::{net, str::FromStr};
use url::Url;

use base64::{engine::general_purpose, Engine};

#[derive(Clone)]
pub struct AuthorizationTokenArg {
    pub token_type: u64,
    pub value: Vec<u8>,
}

impl FromStr for AuthorizationTokenArg {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (token_type, value) = input
            .split_once(':')
            .ok_or_else(|| "expected TYPE:BASE64URL".to_string())?;
        let token_type = if let Some(hex) = token_type.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
        } else {
            token_type.parse()
        }
        .map_err(|_| "token type must be a decimal or 0x-prefixed integer".to_string())?;
        if token_type > (1 << 62) - 1 {
            return Err("token type exceeds the QUIC varint range".to_string());
        }
        let value = general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .or_else(|_| general_purpose::URL_SAFE.decode(value))
            .map_err(|_| "token value must be base64url encoded".to_string())?;

        Ok(Self { token_type, value })
    }
}

#[derive(Parser)]
pub struct Cli {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Connect to the given URL starting with https://
    #[arg()]
    pub url: Url,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,

    /// Publish the current time to the relay, otherwise only subscribe.
    #[arg(long)]
    pub publish: bool,

    /// The name of the clock track.
    #[arg(long, default_value = "clock")]
    pub namespace: String,

    /// The name of the clock track.
    #[arg(long, default_value = "now")]
    pub track: String,

    /// Enable sending of TRACK_STATUS before Subscribe for testing purposes only.
    /// Only works if publish is false.
    #[arg(long)]
    pub track_status: bool,

    /// Use datagrams instead of streams for the clock publisher.
    #[arg(long)]
    pub datagrams: bool,

    /// Authorization token as TYPE:BASE64URL. May be repeated. Testing only:
    /// the token is visible in the process arguments.
    #[arg(long = "auth-token", value_name = "TYPE:BASE64URL")]
    pub authorization_tokens: Vec<AuthorizationTokenArg>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_generic_authorization_tokens() {
        let decimal: AuthorizationTokenArg = "1:AQI".parse().unwrap();
        assert_eq!(decimal.token_type, 1);
        assert_eq!(decimal.value, [1, 2]);

        let hex: AuthorizationTokenArg = "0x63346d:c2VjcmV0".parse().unwrap();
        assert_eq!(hex.token_type, 0x63346d);
        assert_eq!(hex.value, b"secret");
    }

    #[test]
    fn parses_repeated_authorization_token_flags() {
        let cli = Cli::try_parse_from([
            "moq-clock-ietf",
            "https://example.com",
            "--auth-token",
            "1:AQI",
            "--auth-token",
            "0x40:AQI=",
        ])
        .unwrap();

        assert_eq!(cli.authorization_tokens.len(), 2);
        assert_eq!(cli.authorization_tokens[0].token_type, 1);
        assert_eq!(cli.authorization_tokens[0].value, [1, 2]);
        assert_eq!(cli.authorization_tokens[1].token_type, 0x40);
        assert_eq!(cli.authorization_tokens[1].value, [1, 2]);
    }

    #[test]
    fn rejects_malformed_authorization_tokens() {
        assert!("1".parse::<AuthorizationTokenArg>().is_err());
        assert!("1:not+base64".parse::<AuthorizationTokenArg>().is_err());
        assert!("4611686018427387904:AQ"
            .parse::<AuthorizationTokenArg>()
            .is_err());
    }
}
