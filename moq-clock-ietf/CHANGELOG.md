# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.6](https://github.com/englishm/moq-rs/compare/moq-clock-ietf-v0.6.5...moq-clock-ietf-v0.6.6) - 2025-11-01

### Other

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
