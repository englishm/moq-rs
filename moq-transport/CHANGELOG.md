# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.0](https://github.com/englishm/moq-rs/compare/moq-transport-v0.8.1...moq-transport-v0.9.0) - 2025-01-16

### Fixed

- fixes to moq-transport, relay compiles

### Other

- Fix some clippy warnings
- MaxSubscribeId message coding
- Update SETUP message tests
- Add length of params to SubscribeOk
- Add length field to client and server setup
- Add lengths to control messages
- Renumber stream type ids
- Add payload length to datagrams
- Change type of namespace to tuple
- Add Tuple type
- first stab at subscribe namespace messages
- Add new error type
- Remove object/stream (gone in -06)
- remove comment
- more fixes
- rename groups to subgroups
- Bump target draft version to 06

## [0.8.1](https://github.com/englishm/moq-rs/compare/moq-transport-v0.8.0...moq-transport-v0.8.1) - 2024-11-14

### Other

- Defend crash due to probable buffer issues while attempting to decode u8 ([#4](https://github.com/englishm/moq-rs/pull/4))

## [0.8.0](https://github.com/englishm/moq-rs/compare/moq-transport-v0.7.1...moq-transport-v0.8.0) - 2024-10-31

### Other

- Add GroupOrder to SubscribeOk

## [0.7.1](https://github.com/englishm/moq-rs/compare/moq-transport-v0.7.0...moq-transport-v0.7.1) - 2024-10-31

### Other

- Fix u8 encoding

## [0.5.3](https://github.com/kixelated/moq-rs/compare/moq-transport-v0.5.2...moq-transport-v0.5.3) - 2024-07-24

### Other
- Fixed typo in definitions of STREAM_HEADER_TRACK and STREAM_HEADER_GROUP ([#175](https://github.com/kixelated/moq-rs/pull/175))

## [0.5.2](https://github.com/kixelated/moq-rs/compare/moq-transport-v0.5.1...moq-transport-v0.5.2) - 2024-06-03

### Other
- Make listings accessible ([#167](https://github.com/kixelated/moq-rs/pull/167))
