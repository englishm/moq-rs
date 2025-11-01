# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.1](https://github.com/englishm/moq-rs/compare/moq-transport-v0.11.0...moq-transport-v0.11.1) - 2025-11-01

### Fixed

- fix issues noticed by CoPilot

### Other

- Wire Up Track Status Handling
- cargo fmt
- Add separators when printing multiple kv pairs
- Print max 16 bytes of BytesValues when debugging
- Cleanup mlog serialization
- Appease Copilot with more comments
- Improve err handling of push_and_wait_until_popped
- Fix comment typo
- Fix comment typo
- Fixup Subscribe Alias Handling
- -fix spelling errors found by CoPilot
- moq-transport variable renames and comments added
- Use FilterType::LargestObject for subscribe
- Fix param types to match draft-14
- cargo fmt
- Add MoQT qlog events and TODOs for remainder
- cargo fmt
- cargo clippy --fix
- Add more qlog logging to 'mlog' session logs
- Add qlog events for generic logs
- Add some events for subgroup headers and objects
- Add more moqt qlog events
- Emit subscribe and subscribe_ok moqt qlog events
- Add more moqt qlog events
- Refactor mlog feature for better layering
- cargo fmt
- First pass of 'mlog' support
- Initial mlog scaffolding
- Add/bump serde for mlog in moq-transport
- Merge pull request #78 from sgodin/moq-interim-updates-2
- cargo fmt
- Fix lint nit
- Fix lint nit
- Add extra logging
- cargo fmt
- - send track_alias in SubscribeOk to match what is sent in the stream header
- cargo fmt
- Appease linter
- -clock demo - task out  reception of new streams so we don't need to wait for previous stream to end
- Tidy versions test fixture
- Tidy track namespace test fixture
- Tidy tuple test fixture
- Setup message test formatting
- Fix comment placement in Location test
- Fix comment placement in KeyValuePair tests
- VarInt tests - use binary literals for readability

## [0.11.0](https://github.com/englishm/moq-rs/compare/moq-transport-v0.10.0...moq-transport-v0.11.0) - 2025-09-15

### Other

- cargo fmt
- Appease lints
- Cleanup linter warnings
- Implement proper stream header parsing for draft-14
- - only support draft 14 in setup negotiations
- Target only draft-14 support
- Migrate messaging to draft-14
- Migrate data messaging to draft-13
- small change to get clock sample programming working again
- Complete Updating all Control Messaging to Draft-13
- Start updating control messaging to draft-13 level
- Cleanup of Base coding structures
- Start migrating MOQ messaging to draft-13 messaging

## [0.10.0](https://github.com/englishm/moq-rs/compare/moq-transport-v0.9.0...moq-transport-v0.10.0) - 2025-02-24

### Fixed

- fixed clippy warning

### Other

- Merge pull request [#32](https://github.com/englishm/moq-rs/pull/32) from englishm/me/draft-07
- added required fields for announce_cancel
- cargo fmt
- Fix linter nit: unused variable
- Cleaned up and uncommmented error.rs, started adding max_subscribe_id

## [0.9.0](https://github.com/englishm/moq-rs/compare/moq-transport-v0.8.1...moq-transport-v0.9.0) - 2025-01-16

### Fixed

- fixes to moq-transport, relay compiles

### Other

- cargo fmt
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
