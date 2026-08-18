# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.25](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.24...moq-relay-ietf-v0.7.25) - 2026-07-31

### Added

- add metrics for lock poisoning and broadcast lag

### Fixed

- decide upstream wait outcome by branch, not error variant
- label cancelled subscribes distinctly from upstream failures
- wait for upstream subscription before sending SUBSCRIBE_OK
- release upstream subscriptions for idle cached tracks
- send log output to stderr instead of stdout

### Other

- stream namespace paths into the formatter

## [0.7.24](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.23...moq-relay-ietf-v0.7.24) - 2026-07-20

### Added

- forward local accept IP to the connection tagger

## [0.7.23](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.22...moq-relay-ietf-v0.7.23) - 2026-07-19

### Added

- *(moq-relay-ietf)* coalesce upstream subscribe-namespace prefixes
- *(moq-relay-ietf)* track local and remote namespace sources
- *(moq-relay-ietf)* pull upstream namespaces for subscribe_namespace
- *(moq-relay-ietf)* return no upstream relays for internal subscribe_namespace
- *(moq-relay-ietf)* return upstream relays from coordinator subscribe_namespace
- *(moq-relay-ietf)* fan out publish namespaces to subscribers
- *(moq-relay-ietf)* make relay-to-relay sessions full bidirectional peers
- *(moq-relay-ietf)* pass connection interface and peer source to coordinator
- *(moq-relay-ietf)* classify inbound connections as public or internal

### Fixed

- *(moq-relay-ietf)* isolate best-effort namespace fan-out failures
- *(moq-relay-ietf)* share reserved reader for concurrent pull-through requests

### Other

- *(moq-relay-ietf)* align subscribe-namespace connection bindings
- *(moq-relay-ietf)* build resync_publish_tracks set in one pass
- *(moq-relay-ietf)* read-mostly RwLock for track and namespace registries
- *(moq-relay-ietf)* name namespace/track broadcast channel capacities
- *(moq-relay-ietf)* prune and match tracks in a single locked pass
- *(moq-relay-ietf)* serve subscribe-namespace from local state only
- *(moq-relay-ietf)* cover cross-relay subscribe-namespace choreography
- honor subscribe namespace prefixes
- register namespace subscription interest
- persist file coordinator namespace interest
- return file coordinator namespace matches
- filter namespace fanout to published tracks
- fan out publish tracks for namespace subscriptions
- serve namespace subscriptions
- track namespace changes in locals

## [0.7.22](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.21...moq-relay-ietf-v0.7.22) - 2026-07-09

### Fixed

- *(moq-transport)* send publish done after serve completion

### Other

- address PUBLISH review feedback
- track pending request responses
- route PUBLISH tracks by full track name

## [0.7.21](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.20...moq-relay-ietf-v0.7.21) - 2026-07-08

### Added

- *(moq-relay-ietf)* expose relay session config

### Other

- *(moq-relay-ietf)* remove redundant session config binding
- *(moq-relay-ietf)* remove manual changelog entry

## [0.7.20](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.19...moq-relay-ietf-v0.7.20) - 2026-07-08

### Added

- route track-level PUBLISH registrations so relays can serve exact pushed tracks before falling back to namespace routing

### Other

- Merge pull request #170 from itzmanish/draft-16-rewrite
- Update relay dependencies for the draft-16 transport/native stack.

## [0.7.19](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.18...moq-relay-ietf-v0.7.19) - 2026-06-10

### Other

- updated the following local packages: moq-native-ietf

## [0.7.18](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.17...moq-relay-ietf-v0.7.18) - 2026-05-20

### Fixed

- tokio utils use default features
- suggestions from opencode reviewers
- apply suggestions from opencode review

### Other

- check for cancelled of cancellation token when waiting for subscribe open
- keep the comments for readability purpose
- Merge branch 'main' of github.com:itzmanish/moq-rs into feat/remote-manager-rewrite

## [0.7.17](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.16...moq-relay-ietf-v0.7.17) - 2026-04-13

### Fixed

- always register in coordinator after registering in local

### Other

- Merge branch 'main' of github.com:itzmanish/moq-rs into fix-register-order-namespace

## [0.7.16](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.15...moq-relay-ietf-v0.7.16) - 2026-04-10

### Fixed

- cross-platform dual-stack binding for IPv6 sockets

### Other

- Merge pull request #151 from englishm-cloudflare/me/ipv6-dual-stack-binding

## [0.7.15](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.14...moq-relay-ietf-v0.7.15) - 2026-04-09

### Fixed

- include destination address in upstream connection cache key

## [0.7.14](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.13...moq-relay-ietf-v0.7.14) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant
- Bring copyright notices, license docs up to date

## [0.7.13](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.12...moq-relay-ietf-v0.7.13) - 2026-03-27

### Added

- actively reject unauthorized control messages on permission-gated sessions
- add scope-aware namespace isolation to ApiCoordinator
- add Coordinator stubs for SUBSCRIBE_NAMESPACE, track PUBLISH, and lingering subscriber support
- add resolve_scope() to Coordinator trait with permission-gated sessions
- add scope parameter to Coordinator trait and thread through relay
- add Transport enum and connection path extraction

## [0.7.12](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.11...moq-relay-ietf-v0.7.12) - 2026-02-18

### Other

- update Cargo.toml dependencies

## [0.7.11](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.10...moq-relay-ietf-v0.7.11) - 2026-02-18

### Added

- add additional debug logging for troubleshooting
- add structured fields to high-value log messages
- *(metrics)* add describe_metrics() for Prometheus HELP text
- *(metrics)* distinguish graceful close from connection errors
- *(moq-relay-ietf)* add optional prometheus exporter for metrics validation
- *(moq-relay-ietf)* add metrics instrumentation via metrics crate facade

### Fixed

- cargo fmt and clippy lints
- *(metrics)* move upstream_connections gauge after successful connect
- *(metrics)* address review feedback for metrics instrumentation

### Other

- migrate from log crate to tracing
- *(metrics)* make metrics always-on, remove feature gate

## [0.7.10](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.9...moq-relay-ietf-v0.7.10) - 2026-01-29

### Other

- fix unnecessary_unwrap clippy lint

## [0.7.9](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.8...moq-relay-ietf-v0.7.9) - 2025-12-19

### Added

- use socket address from coordinator if available to connect
- bypass DNS lookup on relay URL

### Other

- better comment for url in NamespaceOrigin

## [0.7.8](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.7...moq-relay-ietf-v0.7.8) - 2025-12-18

### Other

- update Cargo.lock dependencies

## [0.7.7](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.6...moq-relay-ietf-v0.7.7) - 2025-12-18

### Added

- add file-based coordinator and rewrote remote for handling remote streams

### Fixed

- ci
- linter
- seperate RemoteManager rewrite to different PR
- remove once_cell to pass the test
- clippy unused imports
- clippy warnings
- race and proper task shutdown
- if host is IpAddr construct socket addr else resolve dns
- update lookup signature to return owned Client instead of reference
- prevent file truncation when opening for read/write in FileCoordinator
- add lifetime parameter to lookup method signature for proper borrow checking
- return clients on lookup for coordinator and misc fix

### Other

- Merge pull request #118 from itzmanish/feat/multi-relay
- remove track registration from coordinator interface and file implementation
- clarify coordinator file usage in CLI help text and add FIXME for unregister_namespace
- restructure relay into lib/bin and add coordinator interface

## [0.7.6](https://github.com/cloudflare/moq-rs/compare/moq-relay-ietf-v0.7.5...moq-relay-ietf-v0.7.6) - 2025-12-18

### Other

- Use correlation IDs in errors
- cargo fmt
- Add support for nested namespaces
- Revert "Add support for namespace hierachies"
- Address PR feedback
- cargo fmt
- Add support for namespace hierachies
- Wire Up Track Status Handling
- moq-relay-ietf variable renames and comments added
- Update moq-relay-ietf/src/relay.rs
- Print CID for clock sessions
- Add --mlog-serve
- Refactor mlog feature for better layering
- First pass of 'mlog' support
- Allow either CID or CID_server.qlog paths
- Add --qlog-serve
- Wire qlog_dir CLI argument through moq-relay-ietf
- Add --qlog-dir CLI argument to QUIC configuration

## [0.7.5](https://github.com/englishm/moq-rs/compare/moq-relay-ietf-v0.7.4...moq-relay-ietf-v0.7.5) - 2025-09-15

### Other

- cargo fmt
- Start updating control messaging to draft-13 level

## [0.7.4](https://github.com/englishm/moq-rs/compare/moq-relay-ietf-v0.7.3...moq-relay-ietf-v0.7.4) - 2025-02-24

### Other

- updated the following local packages: moq-transport

## [0.7.3](https://github.com/englishm/moq-rs/compare/moq-relay-ietf-v0.7.2...moq-relay-ietf-v0.7.3) - 2025-01-16

### Other

- cargo fmt
- Change type of namespace to tuple

## [0.7.2](https://github.com/englishm/moq-rs/compare/moq-relay-ietf-v0.7.1...moq-relay-ietf-v0.7.2) - 2024-10-31

### Other

- updated the following local packages: moq-transport

## [0.7.1](https://github.com/englishm/moq-rs/compare/moq-relay-ietf-v0.7.0...moq-relay-ietf-v0.7.1) - 2024-10-31

### Other

- release

## [0.7.0](https://github.com/englishm/moq-rs/releases/tag/moq-relay-ietf-v0.7.0) - 2024-10-23

### Other

- Update repository URLs for all crates
- Rename crate

## [0.6.1](https://github.com/kixelated/moq-rs/compare/moq-relay-v0.6.0...moq-relay-v0.6.1) - 2024-10-01

### Other

- update Cargo.lock dependencies

## [0.5.1](https://github.com/kixelated/moq-rs/compare/moq-relay-v0.5.0...moq-relay-v0.5.1) - 2024-07-24

### Other
- update Cargo.lock dependencies
