# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.16.2](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.16.1...moq-transport-v0.16.2) - 2026-08-28

### Added

- *(moq-transport)* tie the client-visible error id to the session
- *(moq-transport)* log session_id on stream and request-id paths
- *(moq-transport)* log session_id on publisher and subscriber paths
- *(moq-transport)* log session_id on all 35 control-plane sites
- *(moq-transport)* thread SessionId through Session, Publisher and Subscriber
- *(moq-transport)* add SessionId correlation id

### Fixed

- *(tracing)* harden session log context
- *(tracing)* harden reason logs and share root spans
- *(moq-transport)* preserve session constructor compatibility
- *(moq-transport)* fix TrackName test assertions and pre-existing clippy lints

### Other

- *(moq-transport)* correct QUIC connection ID bounds
- Merge pull request #156 from itzmanish/integrity-tests

## [0.16.1](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.16.0...moq-transport-v0.16.1) - 2026-07-31

### Fixed

- reset subgroup streams instead of FIN-ing mid-object

### Other

- stream namespace paths into the formatter

## [0.16.0](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.15.1...moq-transport-v0.16.0) - 2026-07-19

### Added

- *(moq-transport)* add namespace request stream cancellation

### Fixed

- *(moq-transport)* bound SubscribedNamespace outgoing queue
- *(moq-transport)* bound inbound subscribe-namespace stream resources
- *(moq-transport)* reconcile subscribe namespace cherry-pick

### Other

- *(moq-transport)* document received_above_low_water window bound
- *(moq-transport)* explain subscribe-namespace cleanup guard
- add subscribe namespace mlog events
- allow reordered incoming request ids
- cover subscribe namespace overlap
- handle inbound subscribe namespace
- add outbound subscribe namespace
- add namespace prefix helpers

## [0.15.1](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.15.0...moq-transport-v0.15.1) - 2026-07-09

### Fixed

- *(moq-transport)* treat track ended as clean completion
- *(moq-transport)* send publish done after serve completion

### Other

- address PUBLISH review feedback
- resolve clippy warnings
- track pending request responses
- route PUBLISH tracks by full track name
- align PUBLISH session handling
- add inbound PUBLISH handling

## [0.15.0](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.14.2...moq-transport-v0.15.0) - 2026-07-08

### Added

- add draft-16 PUBLISH, PUBLISH_OK, and PUBLISH_DONE support for publisher-initiated track delivery

### Fixed

- ignore stale unsubscribe ids
- handle request response routing
- clean up unsubscribe state

### Other

- resolve clippy warnings and untrack AGENTS.md
- use correct loglevel for logs for subgroups
- Implement draft-16 subscription parameters and TrackName type
- Merge remote-tracking branch 'manish/main' into mpandit/RT-469

## [0.14.2](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.14.1...moq-transport-v0.14.2) - 2026-05-20

### Fixed

- subscribe cleaning on drop
- apply suggestions from opencode review

## [0.14.1](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.14.0...moq-transport-v0.14.1) - 2026-03-31

### Other

- Make repo REUSE v3.3 compliant
- Bring copyright notices, license docs up to date

## [0.14.0](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.13.1...moq-transport-v0.14.0) - 2026-03-27

### Added

- add Transport enum and connection path extraction

## [0.13.1](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.13.0...moq-transport-v0.13.1) - 2026-03-02

### Fixed

- TrackReader::is_closed() false positive after mode transition

### Other

- Merge pull request #148 from englishm-cloudflare/me/fix-is-closed-false-positive

## [0.13.0](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.12.3...moq-transport-v0.13.0) - 2026-02-18

### Fixed

- handle WebTransport graceful close in is_graceful_close()

### Other

- soften absolute claims about error conversion paths
- remove unused direct deps from moq-transport
- clarify graceful close semantics for WebTransport vs raw QUIC
- Upgrade web-transport crates to v0.10.1

## [0.12.3](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.12.2...moq-transport-v0.12.3) - 2026-02-18

### Added

- add additional debug logging for troubleshooting
- add structured debug logging for MoQT control messages
- *(metrics)* distinguish graceful close from connection errors

### Fixed

- cargo fmt and clippy lints
- *(metrics)* address review feedback for metrics instrumentation

### Other

- migrate from log crate to tracing

## [0.12.2](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.12.1...moq-transport-v0.12.2) - 2026-01-29

### Fixed

- detect and evict stale cached TrackReaders

## [0.12.1](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.12.0...moq-transport-v0.12.1) - 2025-12-18

### Other

- Merge pull request #118 from itzmanish/feat/multi-relay

## [0.12.0](https://github.com/cloudflare/moq-rs/compare/moq-transport-v0.11.0...moq-transport-v0.12.0) - 2025-12-18

### Fixed

- fix issues noticed by CoPilot

### Other

- - register for notification before checking map to avoid race
- If we receive a stream header with an unknown track alias, then wait for up to 1 second for SubscribeOk to arrive
- cargo fmt
- Improve error expressivity and safety
- Address PR feedback
- Use correlation IDs in errors
- Add error correlation ids
- Align error codes with draft-14
- Consistently handle unimplemented features
- Add extension header support to datagrams
- Add support for Immutable Extension headers
- Add logging for immutable extension headers
- Fix last commit
- Fixup KVPs Parsing
- Merge pull request #108 from cloudflare/me/more-kvp-parsing-logging
- Merge pull request #102 from sgodin/datagram-logging
- -fix typo in error definition
- - enable trace level logging on fly
- cargo fmt
- Add support for nested namespaces
- Revert "Add support for namespace hierachies"
- Address PR feedback
- cargo fmt
- Add support for namespace hierachies
- Fix Datagram Support
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
