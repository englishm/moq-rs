# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.10.0](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.9.3...moq-native-ietf-v0.10.0) - 2026-07-20

### Added

- forward local accept IP to the connection tagger

## [0.9.3](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.9.2...moq-native-ietf-v0.9.3) - 2026-07-19

### Added

- *(moq-relay-ietf)* classify inbound connections as public or internal

## [0.9.2](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.9.1...moq-native-ietf-v0.9.2) - 2026-07-09

### Fixed

- *(moq-transport)* send publish done after serve completion

## [0.9.1](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.9.0...moq-native-ietf-v0.9.1) - 2026-07-08

### Other

- Merge pull request #170 from itzmanish/draft-16-rewrite
- Update native QUIC/WebTransport helpers to use the moq-transport 0.15 draft-16 session stack.

## [0.9.0](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.8.0...moq-native-ietf-v0.9.0) - 2026-06-10

### Other

- Use Box+impl Fn for socket wrapper; add passthrough test
- Add socket_wrapper hook to quic Config

## [0.8.0](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.7.5...moq-native-ietf-v0.8.0) - 2026-04-10

### Fixed

- cross-platform dual-stack binding for IPv6 sockets

### Other

- Merge pull request #151 from englishm-cloudflare/me/ipv6-dual-stack-binding

## [0.7.5](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.7.4...moq-native-ietf-v0.7.5) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant

## [0.7.4](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.7.3...moq-native-ietf-v0.7.4) - 2026-03-27

### Added

- add Transport enum and connection path extraction

## [0.7.3](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.7.2...moq-native-ietf-v0.7.3) - 2026-02-18

### Other

- Upgrade web-transport crates to v0.10.1

## [0.7.2](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.7.1...moq-native-ietf-v0.7.2) - 2026-02-18

### Other

- migrate from log crate to tracing

## [0.7.1](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.7.0...moq-native-ietf-v0.7.1) - 2026-02-03

### Other

- Add moq-test-client crate for interoperability testing

## [0.7.0](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.6.0...moq-native-ietf-v0.7.0) - 2025-12-18

### Other

- Merge pull request #118 from itzmanish/feat/multi-relay

## [0.6.0](https://github.com/cloudflare/moq-rs/compare/moq-native-ietf-v0.5.5...moq-native-ietf-v0.6.0) - 2025-12-18

### Other

- Update moq-native-ietf/src/quic.rs
- Print CID for clock sessions
- cargo fmt
- Refactor mlog feature for better layering
- First pass of 'mlog' support
- Implement per-connection qlog file generation
- Thread qlog_dir and base_server_config to accept_session
- Store qlog_dir and base_server_config in Server struct
- Validate qlog directory exists at startup
- Add --qlog-dir CLI argument to QUIC configuration
- Enable qlog feature of quinn
- Log QUIC CIDs for accepted connections
- Use newer quinn

## [0.5.5](https://github.com/englishm/moq-rs/compare/moq-native-ietf-v0.5.4...moq-native-ietf-v0.5.5) - 2025-09-15

### Other

- updated the following local packages: moq-transport

## [0.5.4](https://github.com/englishm/moq-rs/compare/moq-native-ietf-v0.5.3...moq-native-ietf-v0.5.4) - 2025-02-24

### Other

- updated the following local packages: moq-transport

## [0.5.3](https://github.com/englishm/moq-rs/compare/moq-native-ietf-v0.5.2...moq-native-ietf-v0.5.3) - 2025-01-16

### Other

- cargo fmt

## [0.5.2](https://github.com/englishm/moq-rs/compare/moq-native-ietf-v0.5.1...moq-native-ietf-v0.5.2) - 2024-10-31

### Other

- updated the following local packages: moq-transport

## [0.5.1](https://github.com/englishm/moq-rs/compare/moq-native-ietf-v0.5.0...moq-native-ietf-v0.5.1) - 2024-10-31

### Other

- release

## [0.5.0](https://github.com/englishm/moq-rs/releases/tag/moq-native-ietf-v0.5.0) - 2024-10-23

### Other

- Update repository URLs for all crates
- Rename crate

## [0.2.2](https://github.com/kixelated/moq-rs/compare/moq-native-v0.2.1...moq-native-v0.2.2) - 2024-07-24

### Other
- Add sslkeylogfile envvar for debugging ([#173](https://github.com/kixelated/moq-rs/pull/173))

## [0.2.1](https://github.com/kixelated/moq-rs/compare/moq-native-v0.2.0...moq-native-v0.2.1) - 2024-06-03

### Other
- Revert "filter DNS query results to only include addresses that our quic endpoint can use ([#166](https://github.com/kixelated/moq-rs/pull/166))"
- filter DNS query results to only include addresses that our quic endpoint can use ([#166](https://github.com/kixelated/moq-rs/pull/166))
- Remove Cargo.lock from moq-transport
