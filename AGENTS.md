<!--
SPDX-FileCopyrightText: 2026 Cloudflare Inc.
SPDX-License-Identifier: MIT OR Apache-2.0
-->

# Working in this repository

Media over QUIC Transport (MoQT) in Rust. Targets
[draft-ietf-moq-transport-16](https://datatracker.ietf.org/doc/draft-ietf-moq-transport/16/).

## CI must pass

CI (`.github/workflows/pr.yml`) runs these at the **workspace root**. Run them
before pushing; they are the whole gate.

```sh
cargo test --verbose      # every crate, default features
cargo clippy --no-deps    # warnings are not denied, but keep it clean
cargo fmt --check
cargo machete             # unused dependencies
```

REUSE compliance is also checked: every new file needs an
`SPDX-FileCopyrightText` and `SPDX-License-Identifier` header. Match the header
on a neighbouring file in the same crate.

Optional features are **not** covered by the default CI run. If you touch code
behind one, test it explicitly:

```sh
cargo test -p moq-relay-ietf --features auth-cat
cargo clippy --no-deps --all-targets -p moq-relay-ietf --features auth-cat
```

A feature-gated module still has to compile and behave correctly with the
feature *off* — that is usually where the bugs are.

## Workspace layout

| Crate | Responsibility |
|---|---|
| `moq-transport` | The protocol. Wire encoding, control messages, session state machine, `serve` model. No policy, no I/O beyond the QUIC/WebTransport handle. |
| `moq-relay-ietf` | The relay. Routing, caching, namespace/track registration, authorization. Both a library and the `moq-relay-ietf` binary. |
| `moq-native-ietf` | QUIC/TLS setup shared by the native binaries (endpoints, certificates, ALPN). |
| `moq-api` | HTTP API server for cluster-wide origin registration. |
| `moq-pub` / `moq-sub` | Reference publisher and subscriber CLIs. |
| `moq-clock-ietf` | Minimal publisher/subscriber used as a smoke test. |
| `moq-catalog` | Catalog format types. |
| `moq-test-client` | Interop test client. |

Dependencies point one way: `moq-transport` knows nothing about the relay.
Keep it that way — if the relay needs something from the transport, add an
accessor rather than moving policy down.

## Protocol references

Cite the section, not just the document, when a decision follows from the spec.

- **[draft-ietf-moq-transport-16](https://datatracker.ietf.org/doc/draft-ietf-moq-transport/16/)** —
  the transport. Frequently needed: §9.2.2.1 (AUTHORIZATION TOKEN), §9.3.1
  (setup parameters), §13.1 (auth token alias types), §13.4.1 (session
  termination codes), §13.4.2 (REQUEST_ERROR codes).
- **[draft-ietf-moq-c4m-01](https://datatracker.ietf.org/doc/draft-ietf-moq-c4m/)** —
  Common Access Token authorization. §2 (the `moqt` claim and its matching
  rules), §3.1.1 (DPoP), §7.1 (token type registry).
- **RFC 9052 / 8392 / 8747** — COSE, CWT, and CWT proof-of-possession, for
  anything touching token encoding.

The drafts are the authority. Where this implementation knowingly diverges, the
divergence is documented at the site — search for "Known gap".

## Conventions

**Comments explain why.** The what is in the code. A comment earns its place by
recording a constraint, a spec requirement, or a decision someone would
otherwise undo. Prefer citing a draft section over restating the line below.

**Fail closed.** Anything on an authorization or admission path treats an error
as a denial, never as a pass. `Result` in that code is not an escape hatch:
`unwrap_or_default`, `.ok()` and a permissive `_ =>` arm are how these bugs get
written.

**Tests should fail when the behaviour breaks.** For a security control, check
that: remove the control, confirm a test goes red, put it back. A test that
still passes without the thing it is testing is worse than no test, because it
implies coverage that does not exist.

**Errors sent to a peer carry a code and a fixed phrase.** Diagnostic detail
goes to logs. Never put validation internals, claim contents, or token bytes in
a wire message — or in a `Debug` impl that might reach one.

## Authorization

Lives in `moq-relay-ietf/src/auth/`. Read `auth/mod.rs` first; it states the
model and the fail-closed contract.

Keys and policy come from the coordinator per scope
(`Coordinator::get_scope_config`), never from CLI flags — a relay serves
multiple tenants and rotates keys without restarting. A scope that returns no
policy is unauthenticated by design and must keep working exactly as before.

Enforcement points are `AuthHook::on_setup` (once, before either session half
exists) and `AuthHook::on_request` (before SUBSCRIBE, SUBSCRIBE_NAMESPACE,
TRACK_STATUS, PUBLISH_NAMESPACE, PUBLISH). Each runs *before* the corresponding
lookup or registration; for SUBSCRIBE and TRACK_STATUS that ordering is a
correctness property, since deciding afterwards turns them into existence
oracles.

Adding an enforcement point means adding an `AuthzOperation` variant, which is
deliberately a compile error everywhere it must be handled.

## Gotchas

- `moq-transport`'s `Publisher`/`Subscriber` cannot be constructed outside that
  crate, so relay code holding one is not unit-testable. Extract the decision
  into a free function and test that.
- `KeyValuePairs::get` returns only the first match. Parameters that may
  legitimately repeat (AUTHORIZATION TOKEN, §9.2.2.1) must be read by iterating
  `.0`.
- `ReasonPhrase` is capped at 1024 bytes and *fails to encode* above it. An
  encode failure on the control stream propagates out of `Session::run` and
  ends the session, so it is not merely a lost message. Build one with
  `ReasonPhrase::new`, which truncates on a character boundary; the tuple
  constructor is only safe for string literals.
- Dropping a request handle (`Subscribed`, `PublishedNamespace`,
  `PublishReceived`) sends a rejection to the peer. Check the `Drop` impl
  before assuming a code or reason reaches the wire.
- `cargo fmt` reformats the whole workspace. Check `git diff --stat` and revert
  unrelated churn before committing.
