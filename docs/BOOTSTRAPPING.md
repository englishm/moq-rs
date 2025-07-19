# Bootstrapping Guide for `moq-rs` Protocol Development

This document is intended to help new contributors, both human and AI, quickly get up to speed with the `moq-rs` codebase, particularly for tasks involving updates to the MoQ Transport (MoQT) protocol specification.

## Core Crates

Two crates are central to protocol development:

1.  **`moq-transport`**: This is the core library that provides a general-purpose implementation of the MoQT wire protocol. Nearly all changes related to the protocol specification (message formats, handshake logic, etc.) will be made here.
2.  **`moq-relay-ietf`**: This is the primary application crate, implementing a MoQT relay. It serves as the main consumer of the `moq-transport` library and is the reference for how the library is used in practice.

## Understanding the Control Flow

A typical MoQT session involves a handshake followed by a stream of control messages. Understanding how the codebase handles this flow is key.

1.  **Entry Point**: The application starts in `moq-relay-ietf/src/main.rs`. It parses command-line arguments and initializes a `Relay` struct from `moq-relay-ietf/src/relay.rs`.

2.  **Connection Handling**: The main server loop resides in the `Relay::run` method. It listens for and accepts new incoming QUIC connections.

3.  **MoQT Handshake**: For each accepted connection, the relay passes it to the transport layer by calling `moq_transport::session::Session::accept`. This function call is the bridge between the application and the protocol library.

4.  **Session Management**: All session lifecycle logic is handled within the `moq-transport/src/session/mod.rs` file. This includes the initial `SETUP` message exchange and the subsequent processing of control messages.

## Locating Protocol-Specific Code

When updating the protocol, you will primarily work within the `moq-transport` crate.

### Handshake and `SETUP` Messages

The initial MoQT handshake is critical for establishing a compatible session.

-   **Logic**: The handshake logic is located in `moq_transport::session::Session::accept_role` (for servers) and `moq_transport::session::Session::connect_role` (for clients).
-   **Version Negotiation**: The first step in updating to a new draft is typically to add a new version constant. This is done in `moq-transport/src/setup/version.rs`. The handshake logic must then be updated to offer and accept this new version.
-   **Message Format**: The `Client` and `Server` `SETUP` message structures are defined in the `moq-transport/src/setup/` directory.

### Control Messages (`ANNOUNCE`, `SUBSCRIBE`, etc.)

After the handshake, the client and server exchange control messages to manage tracks and objects.

-   **Definitions**: Each control message is defined in its own file within the `moq-transport/src/message/` directory (e.g., `subscribe.rs`, `announce.rs`).
-   **Serialization**: Messages are serialized to and deserialized from the wire format using the `Encode` and `Decode` traits, defined in `moq-transport/src/coding/`. When a message's structure changes in the spec, you must update the struct definition and its corresponding `Encode`/`Decode` implementations.

By following this trail from the relay to the transport layer, a contributor can quickly pinpoint the exact code that needs to be modified to align with a new version of the MoQT specification.
