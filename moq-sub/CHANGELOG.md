# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.15](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.14...moq-sub-v0.4.15) - 2026-08-28

### Added

- *(moq-transport)* thread SessionId through Session, Publisher and Subscriber

### Fixed

- *(moq-transport)* preserve session constructor compatibility

## [0.4.14](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.13...moq-sub-v0.4.14) - 2026-07-31

### Fixed

- send log output to stderr instead of stdout

## [0.4.13](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.12...moq-sub-v0.4.13) - 2026-07-20

### Other

- updated the following local packages: moq-native-ietf

## [0.4.12](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.11...moq-sub-v0.4.12) - 2026-07-19

### Other

- updated the following local packages: moq-transport

## [0.4.11](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.10...moq-sub-v0.4.11) - 2026-07-09

### Fixed

- *(moq-transport)* send publish done after serve completion

## [0.4.10](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.9...moq-sub-v0.4.10) - 2026-07-08

### Other

- update Cargo.lock dependencies
- Update subscriber CLI dependencies for the draft-16 transport/native stack.

## [0.4.9](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.8...moq-sub-v0.4.9) - 2026-06-10

### Other

- updated the following local packages: moq-native-ietf

## [0.4.8](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.7...moq-sub-v0.4.8) - 2026-05-20

### Fixed

- subscribe cleaning on drop

## [0.4.7](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.6...moq-sub-v0.4.7) - 2026-04-10

### Fixed

- cross-platform dual-stack binding for IPv6 sockets

### Other

- Merge pull request #151 from englishm-cloudflare/me/ipv6-dual-stack-binding

## [0.4.6](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.5...moq-sub-v0.4.6) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant
- Bring copyright notices, license docs up to date

## [0.4.5](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.4...moq-sub-v0.4.5) - 2026-03-27

### Added

- add Transport enum and connection path extraction

## [0.4.4](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.3...moq-sub-v0.4.4) - 2026-02-18

### Other

- update Cargo.lock dependencies

## [0.4.3](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.2...moq-sub-v0.4.3) - 2026-02-18

### Other

- migrate from log crate to tracing

## [0.4.2](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.1...moq-sub-v0.4.2) - 2026-02-03

### Other

- update Cargo.lock dependencies

## [0.4.1](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.4.0...moq-sub-v0.4.1) - 2025-12-18

### Other

- Merge pull request #118 from itzmanish/feat/multi-relay

## [0.4.0](https://github.com/cloudflare/moq-rs/compare/moq-sub-v0.3.4...moq-sub-v0.4.0) - 2025-12-18

### Other

- Add support for nested namespaces
- Log CID
- Print CID for clock sessions
- Add --qlog-dir CLI argument to QUIC configuration
- Merge branch 'main' into sub-catalog

## [0.3.4](https://github.com/englishm/moq-rs/compare/moq-sub-v0.3.3...moq-sub-v0.3.4) - 2025-09-15

### Other

- Cleanup linter warnings
- Start updating control messaging to draft-13 level

## [0.3.3](https://github.com/englishm/moq-rs/compare/moq-sub-v0.3.2...moq-sub-v0.3.3) - 2025-02-24

### Other

- updated the following local packages: moq-transport

## [0.3.2](https://github.com/englishm/moq-rs/compare/moq-sub-v0.3.1...moq-sub-v0.3.2) - 2025-01-16

### Other

- cargo fmt
- Change type of namespace to tuple
- s/group/subgroup/g

## [0.3.1](https://github.com/englishm/moq-rs/compare/moq-sub-v0.3.0...moq-sub-v0.3.1) - 2024-10-31

### Other

- update Cargo.lock dependencies

## [0.2.1](https://github.com/kixelated/moq-rs/compare/moq-sub-v0.2.0...moq-sub-v0.2.1) - 2024-10-01

### Fixed

- don't interleave groups ([#188](https://github.com/kixelated/moq-rs/pull/188))

### Other

- Allow moqt URL scheme for QUIC ([#187](https://github.com/kixelated/moq-rs/pull/187))

## [0.1.1](https://github.com/kixelated/moq-rs/compare/moq-sub-v0.1.0...moq-sub-v0.1.1) - 2024-07-24

### Other
- update Cargo.lock dependencies
