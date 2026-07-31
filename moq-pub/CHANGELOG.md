# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.3](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.9.2...moq-pub-v0.9.3) - 2026-07-31

### Fixed

- send log output to stderr instead of stdout

## [0.9.2](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.9.1...moq-pub-v0.9.2) - 2026-07-20

### Other

- updated the following local packages: moq-native-ietf

## [0.9.1](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.9.0...moq-pub-v0.9.1) - 2026-07-19

### Other

- updated the following local packages: moq-transport

## [0.9.0](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.16...moq-pub-v0.9.0) - 2026-07-09

### Fixed

- *(moq-transport)* send publish done after serve completion

### Other

- address PUBLISH review feedback
- track pending request responses
- route PUBLISH tracks by full track name

## [0.8.16](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.15...moq-pub-v0.8.16) - 2026-07-08

### Added

- add `--publish` mode to push newly created tracks with PUBLISH after announcing the namespace

### Other

- Merge pull request #170 from itzmanish/draft-16-rewrite
- Update publisher CLI dependencies for the draft-16 transport/native stack.

## [0.8.15](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.14...moq-pub-v0.8.15) - 2026-06-10

### Other

- updated the following local packages: moq-native-ietf

## [0.8.14](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.13...moq-pub-v0.8.14) - 2026-05-20

### Other

- update Cargo.lock dependencies

## [0.8.13](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.12...moq-pub-v0.8.13) - 2026-04-10

### Fixed

- cross-platform dual-stack binding for IPv6 sockets

### Other

- Merge pull request #151 from englishm-cloudflare/me/ipv6-dual-stack-binding

## [0.8.12](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.11...moq-pub-v0.8.12) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant
- Bring copyright notices, license docs up to date

## [0.8.11](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.10...moq-pub-v0.8.11) - 2026-03-27

### Added

- add Transport enum and connection path extraction

### Fixed

- *(moq-pub)* combine moof+mdat into single MoQ Object

## [0.8.10](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.9...moq-pub-v0.8.10) - 2026-02-18

### Other

- update Cargo.lock dependencies

## [0.8.9](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.8...moq-pub-v0.8.9) - 2026-02-18

### Other

- migrate from log crate to tracing

## [0.8.8](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.7...moq-pub-v0.8.8) - 2026-01-29

### Other

- fix unnecessary_unwrap clippy lint

## [0.8.7](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.6...moq-pub-v0.8.7) - 2025-12-18

### Other

- Merge pull request #118 from itzmanish/feat/multi-relay

## [0.8.6](https://github.com/cloudflare/moq-rs/compare/moq-pub-v0.8.5...moq-pub-v0.8.6) - 2025-12-18

### Other

- moq-transport variable renames and comments added
- Log CID
- Print CID for clock sessions
- Add --qlog-dir CLI argument to QUIC configuration

## [0.8.5](https://github.com/englishm/moq-rs/compare/moq-pub-v0.8.4...moq-pub-v0.8.5) - 2025-09-15

### Other

- cargo fmt
- Cleanup linter warnings
- Start updating control messaging to draft-13 level

## [0.8.4](https://github.com/englishm/moq-rs/compare/moq-pub-v0.8.3...moq-pub-v0.8.4) - 2025-02-24

### Other

- updated the following local packages: moq-transport

## [0.8.3](https://github.com/englishm/moq-rs/compare/moq-pub-v0.8.2...moq-pub-v0.8.3) - 2025-01-16

### Other

- cargo fmt
- Change type of namespace to tuple
- more fixes
- moq-pub uses subgroups

## [0.8.2](https://github.com/englishm/moq-rs/compare/moq-pub-v0.8.1...moq-pub-v0.8.2) - 2024-10-31

### Other

- updated the following local packages: moq-transport

## [0.8.1](https://github.com/englishm/moq-rs/compare/moq-pub-v0.8.0...moq-pub-v0.8.1) - 2024-10-31

### Other

- update Cargo.lock dependencies

## [0.7.1](https://github.com/kixelated/moq-rs/compare/moq-pub-v0.7.0...moq-pub-v0.7.1) - 2024-10-01

### Fixed

- add a way to reset Media object ([#189](https://github.com/kixelated/moq-rs/pull/189))

## [0.6.1](https://github.com/kixelated/moq-rs/compare/moq-pub-v0.6.0...moq-pub-v0.6.1) - 2024-07-24

### Other
- Fix some catalog bugs involving the name. ([#178](https://github.com/kixelated/moq-rs/pull/178))

## [0.5.1](https://github.com/kixelated/moq-rs/compare/moq-pub-v0.5.0...moq-pub-v0.5.1) - 2024-06-03

### Other
- Fix audio in the BBB demo. ([#164](https://github.com/kixelated/moq-rs/pull/164))
