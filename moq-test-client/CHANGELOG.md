# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.13](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.12...moq-test-client-v0.1.13) - 2026-08-28

### Added

- *(moq-transport)* thread SessionId through Session, Publisher and Subscriber

### Fixed

- *(moq-transport)* preserve session constructor compatibility

## [0.1.12](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.11...moq-test-client-v0.1.12) - 2026-07-31

### Fixed

- send log output to stderr instead of stdout

## [0.1.11](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.10...moq-test-client-v0.1.11) - 2026-07-20

### Other

- updated the following local packages: moq-native-ietf

## [0.1.10](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.9...moq-test-client-v0.1.10) - 2026-07-19

### Other

- updated the following local packages: moq-transport

## [0.1.9](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.8...moq-test-client-v0.1.9) - 2026-07-09

### Fixed

- *(moq-transport)* send publish done after serve completion

### Other

- *(moq-test-client)* add direct publish interop scenarios

### Added

- Add direct PUBLISH interop scenarios for publisher-only completion and exact-track subscriber routing.

## [0.1.8](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.7...moq-test-client-v0.1.8) - 2026-07-08

### Other

- Merge pull request #170 from itzmanish/draft-16-rewrite
- Update the interoperability test client for the draft-16 transport/native stack.

## [0.1.7](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.6...moq-test-client-v0.1.7) - 2026-06-10

### Other

- updated the following local packages: moq-native-ietf

## [0.1.6](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.5...moq-test-client-v0.1.6) - 2026-05-20

### Other

- update Cargo.lock dependencies

## [0.1.5](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.4...moq-test-client-v0.1.5) - 2026-04-10

### Fixed

- cross-platform dual-stack binding for IPv6 sockets

### Other

- Merge pull request #151 from englishm-cloudflare/me/ipv6-dual-stack-binding

## [0.1.4](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.3...moq-test-client-v0.1.4) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant
- Bring copyright notices, license docs up to date

## [0.1.3](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.2...moq-test-client-v0.1.3) - 2026-03-27

### Added

- add Transport enum and connection path extraction

## [0.1.2](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.1...moq-test-client-v0.1.2) - 2026-02-18

### Other

- Upgrade web-transport crates to v0.10.1

## [0.1.1](https://github.com/cloudflare/moq-rs/compare/moq-test-client-v0.1.0...moq-test-client-v0.1.1) - 2026-02-18

### Other

- migrate from log crate to tracing
- add run-level TAP comments
- add error message to YAML diagnostics
- add connection_id to YAML diagnostics
- add duration_ms YAML diagnostic
- output TAP version 14 format
- release

## [0.1.0](https://github.com/cloudflare/moq-rs/releases/tag/moq-test-client-v0.1.0) - 2026-02-03

### Other

- Fix cargo fmt formatting
- Add moq-test-client crate for interoperability testing
