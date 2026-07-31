# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.20](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.19...moq-clock-ietf-v0.6.20) - 2026-07-31

### Fixed

- send log output to stderr instead of stdout

## [0.6.19](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.18...moq-clock-ietf-v0.6.19) - 2026-07-20

### Other

- updated the following local packages: moq-native-ietf

## [0.6.18](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.17...moq-clock-ietf-v0.6.18) - 2026-07-19

### Other

- updated the following local packages: moq-transport

## [0.6.17](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.16...moq-clock-ietf-v0.6.17) - 2026-07-09

### Fixed

- *(moq-transport)* send publish done after serve completion

## [0.6.16](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.15...moq-clock-ietf-v0.6.16) - 2026-07-08

### Other

- Merge pull request #170 from itzmanish/draft-16-rewrite
- Update the clock example to run on the draft-16 transport/native stack.

## [0.6.15](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.14...moq-clock-ietf-v0.6.15) - 2026-06-10

### Other

- updated the following local packages: moq-native-ietf

## [0.6.14](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.13...moq-clock-ietf-v0.6.14) - 2026-05-20

### Other

- update Cargo.lock dependencies

## [0.6.13](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.12...moq-clock-ietf-v0.6.13) - 2026-04-10

### Fixed

- cross-platform dual-stack binding for IPv6 sockets

### Other

- Merge pull request #151 from englishm-cloudflare/me/ipv6-dual-stack-binding

## [0.6.12](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.11...moq-clock-ietf-v0.6.12) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant

## [0.6.11](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.10...moq-clock-ietf-v0.6.11) - 2026-03-27

### Added

- add Transport enum and connection path extraction

## [0.6.10](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.9...moq-clock-ietf-v0.6.10) - 2026-02-18

### Other

- update Cargo.lock dependencies

## [0.6.9](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.8...moq-clock-ietf-v0.6.9) - 2026-02-18

### Other

- migrate from log crate to tracing

## [0.6.8](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.7...moq-clock-ietf-v0.6.8) - 2026-02-03

### Other

- update Cargo.lock dependencies

## [0.6.7](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.6...moq-clock-ietf-v0.6.7) - 2025-12-18

### Other

- Merge pull request #118 from itzmanish/feat/multi-relay

## [0.6.6](https://github.com/cloudflare/moq-rs/compare/moq-clock-ietf-v0.6.5...moq-clock-ietf-v0.6.6) - 2025-12-18

### Other

- Add extension header support to datagrams
- Fix Datagram Support
- Wire Up Track Status Handling
- moq-clock-ietf variable renames and comments added
- Print CID for clock sessions
- Add --qlog-dir CLI argument to QUIC configuration
- -clock demo - task out  reception of new streams so we don't need to wait for previous stream to end

## [0.6.5](https://github.com/englishm/moq-rs/compare/moq-clock-ietf-v0.6.4...moq-clock-ietf-v0.6.5) - 2025-09-15

### Other

- cargo fmt
- Cleanup linter warnings
- Start updating control messaging to draft-13 level

## [0.6.4](https://github.com/englishm/moq-rs/compare/moq-clock-ietf-v0.6.3...moq-clock-ietf-v0.6.4) - 2025-02-24

### Other

- updated the following local packages: moq-transport

## [0.6.3](https://github.com/englishm/moq-rs/compare/moq-clock-ietf-v0.6.2...moq-clock-ietf-v0.6.3) - 2025-01-16

### Other

- cargo fmt
- Change type of namespace to tuple
- Remove object/stream (gone in -06)
- cargo fmt
- s/group/subgroup/g

## [0.6.2](https://github.com/englishm/moq-rs/compare/moq-clock-ietf-v0.6.1...moq-clock-ietf-v0.6.2) - 2024-10-31

### Other

- updated the following local packages: moq-transport

## [0.6.1](https://github.com/englishm/moq-rs/compare/moq-clock-ietf-v0.6.0...moq-clock-ietf-v0.6.1) - 2024-10-31

### Other

- release

## [0.6.0](https://github.com/englishm/moq-rs/releases/tag/moq-clock-ietf-v0.6.0) - 2024-10-23

### Other

- Update repository URLs for all crates
- Rename crate

## [0.5.1](https://github.com/kixelated/moq-rs/compare/moq-clock-v0.5.0...moq-clock-v0.5.1) - 2024-10-01

### Other

- update Cargo.lock dependencies

## [0.4.2](https://github.com/kixelated/moq-rs/compare/moq-clock-v0.4.1...moq-clock-v0.4.2) - 2024-07-24

### Other
- update Cargo.lock dependencies

## [0.4.1](https://github.com/kixelated/moq-rs/compare/moq-clock-v0.4.0...moq-clock-v0.4.1) - 2024-06-03

### Other
- Initial gstreamer support ([#163](https://github.com/kixelated/moq-rs/pull/163))
- Add an index server (moq-dir) that lists all announcements ([#160](https://github.com/kixelated/moq-rs/pull/160))
