// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session and request authorization for the relay.
//!
//! # Model
//!
//! Authorization is a per-scope policy, not a relay-wide switch. After
//! [`Coordinator::resolve_scope`] identifies which scope a connection belongs
//! to, [`Coordinator::get_scope_config`] supplies that scope's
//! [`ScopeAuthConfig`], including the public keys used to verify tokens. A
//! scope that returns no policy runs exactly as it did before this module
//! existed.
//!
//! Where a policy is present, enforcement happens at two points:
//!
//! * **Setup.** The AUTHORIZATION TOKEN parameters from CLIENT_SETUP are
//!   decoded (see [`decode_setup_tokens`]) and handed to
//!   [`AuthHook::on_setup`]. A denial terminates the session before either
//!   half of it is constructed.
//! * **Request.** Every SUBSCRIBE, SUBSCRIBE_NAMESPACE, TRACK_STATUS,
//!   PUBLISH_NAMESPACE and PUBLISH is checked with [`AuthHook::on_request`]
//!   before the relay acts on it. A denial rejects that request and leaves the
//!   session running.
//!
//! Cryptography is confined to setup. The identity established there is
//! carried in a [`Principal`], so per-request checks are pure claim
//! evaluation. Expiry is re-checked on every request, so no *new* request is
//! authorized more than [`ScopeAuthConfig::clock_skew`] past the token's
//! `exp`. That tolerance exists to absorb clock differences between issuer
//! and relay, and it applies to the per-request check exactly as it does at
//! setup — so "expired" in practice means `exp + clock_skew`, not `exp`. The
//! CAT hook clamps the configured value so a scope cannot widen the window
//! indefinitely.
//!
//! # Revocation, and what it does not cover
//!
//! Rotating a scope's keys stops new sessions being established with the old
//! key within one [`ScopeAuthConfig::ttl`], but does **not** terminate
//! sessions already running: a session holds the hook it was admitted with.
//! Token expiry is therefore what bounds a compromised credential, which is
//! why the CAT hook requires `exp` and caps how far ahead it may be.
//!
//! Expiry bounds *admission*, not *delivery*. A SUBSCRIBE authorized before
//! the token expired keeps streaming afterwards, because nothing revisits an
//! established subscription — so a leaked token buys media for as long as the
//! session survives, not merely until `exp`. Tearing down in-flight
//! subscriptions at expiry, and re-resolving the hook when a scope's
//! configuration changes, are the two pieces that would close this; neither is
//! implemented.
//!
//! # Failing closed
//!
//! Once a scope has opted in, anything short of an explicit allow denies:
//!
//! * a coordinator that cannot be reached,
//! * a configuration with no keys, too many keys, or unparseable key material,
//! * a relay built without the scheme's feature,
//! * a hook that returns an error,
//! * a missing, malformed, expired or out-of-scope token.
//!
//! The one deliberate exception is a scope that never opted in, which is not
//! an authorization failure but the absence of a policy.
//!
//! [`Coordinator::resolve_scope`]: crate::Coordinator::resolve_scope
//! [`Coordinator::get_scope_config`]: crate::Coordinator::get_scope_config
//! [`ScopeAuthConfig`]: crate::ScopeAuthConfig

mod hook;
mod scope;
mod session;
mod token;
mod types;

#[cfg(feature = "auth-cat")]
mod cat;

pub use hook::{AllowAllAuthHook, AuthHook};
pub use token::{decode_setup_tokens, TokenError, CAT_TOKEN_TYPE, MAX_SETUP_TOKENS};
pub use types::{
    AuthDecision, AuthError, AuthRequest, AuthToken, AuthzOperation, DenyReason, Principal, Verdict,
};

#[cfg(feature = "auth-cat")]
pub use cat::CatAuthHook;

pub(crate) use hook::DenyAllAuthHook;
pub(crate) use scope::ScopeAuthorizer;
pub(crate) use session::{authorize, SessionAuth};

use crate::DEFAULT_SCOPE_AUTH_TTL;

const UNSCOPED: &str = "<unscoped>";

/// Session termination code for UNAUTHORIZED (draft-16 §13.4.1).
pub(crate) const SESSION_ERROR_UNAUTHORIZED: u32 = 0x2;
