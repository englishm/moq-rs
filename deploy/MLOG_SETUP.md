# MoQ Transport Logging (mlog)

mlog is structured logging for MoQ Transport protocol events. It records every control message and data stream event the relay processes for a given connection, using [JSON-SEQ](https://www.rfc-editor.org/rfc/rfc7464) (RFC 7464) so each record is prefixed with ASCII Record Separator (`0x1e`) and ends with a newline. The events are [qlog](https://datatracker.ietf.org/doc/draft-ietf-quic-qlog-main-schema/)-compatible, following the [`draft-pardue-moq-qlog-moq-events`](https://datatracker.ietf.org/doc/draft-pardue-moq-qlog-moq-events/) IETF draft.

Where QUIC qlog (see [QLOG_SETUP.md](QLOG_SETUP.md)) captures transport-layer events like handshakes, congestion, and packet loss, mlog captures the MoQ application layer: SETUP exchanges, namespace announcements, subscriptions, and object delivery.

## Quick Start: Verify Your Messages Reached the Relay

This walkthrough uses `moq-test-client` against a public interop relay to demonstrate the basics. The URL below is an example — check the [moq-interop-runner registry](https://github.com/englishm/moq-interop-runner/blob/main/implementations.json) for current endpoints. You can adapt the same technique for your own relay.

### Step 1: Run a test

```bash
cargo run --bin moq-test-client -- \
  --relay https://draft-18-interop.cloudflare.mediaoverquic.com:443 \
  --tls-disable-verify \
  --test setup-only
```

### Step 2: Find your Connection ID

The test client prints the Connection ID in its TAP output:

```
ok 1 - setup-only
  ---
  duration_ms: 521
  connection_id: 22c73802597dcd91ef662a3cd67a03e0
  ...
```

### Step 3: Fetch your mlog

```bash
curl https://draft-18-interop.cloudflare.mediaoverquic.com:443/mlog/22c73802597dcd91ef662a3cd67a03e0
```

### Step 4: Read the results

The output is JSON-SEQ: each record starts with ASCII Record Separator (`0x1e`) and contains a JSON object. The first record is a header; subsequent records are events:

```json
{"time":0.179,"name":"moqt:control_message_parsed","data":{"message_type":"client_setup","supported_versions":["DRAFT_16"],...}}
{"time":0.216,"name":"moqt:control_message_created","data":{"message_type":"server_setup","selected_version":"DRAFT_16",...}}
```

- `control_message_parsed` = the relay **received** a message from your client
- `control_message_created` = the relay **sent** a message to your client
- `time` = milliseconds since connection start

If you see `client_setup` parsed and `server_setup` created, your client is speaking valid MoQ Transport and the relay understood it.

## Worked Examples

### Example 1: SETUP handshake (`setup-only`)

The simplest test: connect, exchange CLIENT_SETUP/SERVER_SETUP, disconnect.

```bash
cargo run --bin moq-test-client -- \
  --relay https://draft-18-interop.cloudflare.mediaoverquic.com:443 \
  --tls-disable-verify \
  --test setup-only
```

**mlog output** (reformatted for readability):

```json
{
  "time": 0.179,
  "name": "moqt:control_message_parsed",
  "data": {
    "event_type": "control_message_parsed",
    "stream_id": 0,
    "message_type": "client_setup",
    "number_of_supported_versions": 1,
    "supported_versions": ["DRAFT_16"],
    "parameters": [["2", "100"]]
  }
}
```
The relay parsed your CLIENT_SETUP. It offered version DRAFT_16. The `parameters` array contains SETUP parameters as `[id, value]` pairs (here, PATH with max length 100).

```json
{
  "time": 0.216,
  "name": "moqt:control_message_created",
  "data": {
    "event_type": "control_message_created",
    "stream_id": 0,
    "message_type": "server_setup",
    "selected_version": "DRAFT_16",
    "parameters": [["2", "100"]]
  }
}
```
The relay responded with SERVER_SETUP, selecting DRAFT_16. The handshake is complete.

**What to look for:**
- If you see `client_setup` parsed but no `server_setup` created, the relay rejected your version or parameters
- If you see nothing at all, your connection didn't reach the MoQ layer (check QUIC connectivity)

### Example 2: Publishing a namespace (`publish-namespace-only`)

After SETUP, announce a namespace and verify the relay accepts it.

```bash
cargo run --bin moq-test-client -- \
  --relay https://draft-18-interop.cloudflare.mediaoverquic.com:443 \
  --tls-disable-verify \
  --test publish-namespace-only
```

**mlog output** (after the SETUP exchange):

```json
{
  "time": 64.779,
  "name": "moqt:control_message_parsed",
  "data": {
    "event_type": "control_message_parsed",
    "stream_id": 0,
    "message_type": "publish_namespace",
    "request_id": 0,
    "track_namespace": "/moq-test/interop",
    "parameters": []
  }
}
```
The relay received your PUBLISH_NAMESPACE for `/moq-test/interop`.

```json
{
  "time": 65.526,
  "name": "moqt:control_message_created",
  "data": {
    "event_type": "control_message_created",
    "stream_id": 0,
    "message_type": "request_ok",
    "request_id": 0,
    "request_kind": "publish_namespace",
    "parameters": []
  }
}
```
The relay accepted the namespace with REQUEST_OK for `request_kind: "publish_namespace"`. Your client is now registered as a publisher for this namespace, and the relay will route incoming subscriptions to you.

**What to look for:**
- `publish_namespace` parsed confirms the relay received your announcement
- `request_ok` created with `request_kind: "publish_namespace"` confirms it was accepted
- The `request_id` ties the response to the request

### Example 3: Full publish-subscribe flow (`publish-namespace-subscribe`)

This test uses two connections: a publisher and a subscriber. The test client reports both Connection IDs:

```bash
cargo run --bin moq-test-client -- \
  --relay https://draft-18-interop.cloudflare.mediaoverquic.com:443 \
  --tls-disable-verify \
  --test publish-namespace-subscribe
```

**TAP output:**

```
ok 1 - publish-namespace-subscribe
  ---
  duration_ms: 3436
  publisher_connection_id: 71d4b5eb1a807779af03331c330d5fa9
  subscriber_connection_id: 08d0b03ede133f0839435bff64ed2fc5
  ...
```

Fetch both mlogs to see each side of the exchange:

```bash
# Publisher's view
curl https://draft-18-interop.cloudflare.mediaoverquic.com:443/mlog/71d4b5eb1a807779af03331c330d5fa9

# Subscriber's view
curl https://draft-18-interop.cloudflare.mediaoverquic.com:443/mlog/08d0b03ede133f0839435bff64ed2fc5
```

**Publisher mlog** (after SETUP):

```json
{"time":231.481,"name":"moqt:control_message_parsed","data":{"message_type":"publish_namespace","request_id":0,"track_namespace":"/moq-test/interop",...}}
{"time":233.084,"name":"moqt:control_message_created","data":{"message_type":"request_ok","request_id":0,"request_kind":"publish_namespace","parameters":[]}}
```

The publisher announced `/moq-test/interop` and the relay accepted it.

**Subscriber mlog** (after SETUP):

```json
{"time":47.169,"name":"moqt:control_message_parsed","data":{"message_type":"subscribe","subscribe_id":0,"track_namespace":"/moq-test/interop","track_name":"test-track",...}}
{"time":48.622,"name":"moqt:control_message_created","data":{"message_type":"subscribe_ok","subscribe_id":0,"track_alias":0,...}}
```

The subscriber sent a SUBSCRIBE for track `test-track` under namespace `/moq-test/interop`, and the relay responded with SUBSCRIBE_OK. This confirms the relay successfully routed the subscription to the publisher's namespace.

**When data flows**, you'll also see data plane events in the mlog. In the publisher's mlog, `*_parsed` events show data the relay received:

```json
{"time":395.872,"name":"moqt:subgroup_header_parsed","data":{"header_type":"SubgroupIdExt","track_alias":0,"group_id":1,"publisher_priority":128}}
{"time":397.166,"name":"moqt:subgroup_object_parsed","data":{"group_id":1,"subgroup_id":0,"object_id":0,"object_payload_length":1024}}
```

In the subscriber's mlog, `*_created` events show data the relay forwarded:

```json
{"time":395.872,"name":"moqt:subgroup_header_created","data":{"header_type":"SubgroupIdExt","track_alias":0,"group_id":1,"publisher_priority":128,"subgroup_id":0}}
{"time":397.166,"name":"moqt:subgroup_object_created","data":{"group_id":1,"subgroup_id":0,"object_id":0,"object_payload_length":1024}}
```

- `object_payload_length` tells you the size of the payload the relay handled

## mlog Format Reference

### File format

mlog uses [JSON-SEQ (RFC 7464)](https://www.rfc-editor.org/rfc/rfc7464): each record is prefixed with ASCII Record Separator (`0x1e`) and ends with a newline.

**Line 1 — Header:**

```json
{
  "qlog_version": "0.3",
  "qlog_format": "JSON-SEQ",
  "title": "moq-relay",
  "description": "MoQ Transport events",
  "trace": {
    "vantage_point": { "type": "server" },
    "event_schemas": [
      "urn:ietf:params:qlog:events:loglevel",
      "urn:ietf:params:qlog:events:moqt"
    ]
  }
}
```

**Subsequent lines — Events:**

```json
{
  "time": <milliseconds since connection start>,
  "name": "<event_name>",
  "data": { ... }
}
```

### Event types

| Event Name | Direction | Description |
|------------|-----------|-------------|
| `moqt:control_message_parsed` | Received | Relay received a control message from the client |
| `moqt:control_message_created` | Sent | Relay sent a control message to the client |
| `moqt:subgroup_header_parsed` | Received | Relay received a subgroup stream header |
| `moqt:subgroup_header_created` | Sent | Relay sent (forwarded) a subgroup stream header |
| `moqt:subgroup_object_parsed` | Received | Relay received an object within a subgroup |
| `moqt:subgroup_object_created` | Sent | Relay sent (forwarded) an object within a subgroup |
| `moqt:object_datagram_parsed` | Received | Relay received a datagram-delivered object |
| `moqt:object_datagram_created` | Sent | Relay sent (forwarded) a datagram-delivered object |

**Naming convention:** `*_parsed` = the relay received and parsed this from the wire. `*_created` = the relay constructed and sent this on the wire.

### Control message types

These appear in the `message_type` field of control message events:

| Message Type | Protocol Reference |
|-------------|-------------------|
| `client_setup` / `server_setup` | MoQT §3.3 |
| `publish_namespace` / `request_ok` with `request_kind: "publish_namespace"` | MoQT §6.2, §9.7 |
| `subscribe` / `subscribe_ok` / `request_error` with `request_kind: "subscribe"` | MoQT §5.1, §9.8 |
| `unsubscribe` | MoQT §5.1 |

### Known limitations

- **`stream_id` is currently a placeholder.** The `stream_id` field in all events is `0`. Actual QUIC stream IDs are not yet plumbed through to the mlog layer. Don't use this field for stream correlation — use `subscribe_id`, `track_alias`, or `group_id` instead.

### File naming

mlog files are named `<connection_id>_server.mlog`, where the connection ID is the hex-encoded QUIC Connection ID (typically 32 hex characters).

## Running Your Own Relay with mlog

### Using the dev script

The simplest way to run a local relay with mlog enabled:

```bash
MLOG_DIR=/tmp/mlog ./dev/relay
```

This starts the relay with:
- mlog writing to `/tmp/mlog/`
- mlog HTTP serving enabled at `https://localhost:4443/mlog/<cid>`
- Self-signed TLS certificates (auto-generated)

Then run tests against it:

```bash
cargo run --bin moq-test-client -- \
  --relay https://localhost:4443 \
  --tls-disable-verify \
  --test setup-only
```

And fetch the mlog:

```bash
curl -k https://localhost:4443/mlog/<connection_id>
```

You can also pass extra arguments directly:

```bash
./dev/relay --mlog-dir /tmp/mlog --mlog-serve
```

### Relay flags

| Flag | Description |
|------|-------------|
| `--mlog-dir <path>` | Directory to write mlog files to |
| `--mlog-serve` | Serve mlog files over HTTPS at `/mlog/<cid>` |
| `--dev` | Required for `--mlog-serve` (enables the HTTP endpoint; already set by `./dev/relay`) |

Both `--mlog-dir` and `--mlog-serve` can be used independently: you can write mlog files to disk without serving them over HTTP, or you might only need the HTTP endpoint in some setups.

## Troubleshooting

**"Connection not found" when fetching mlog**
- mlog files on the interop relay use ephemeral storage and are lost on restart/redeploy
- If you get a 404, the relay may have been restarted since your test. Run the test again.

**Parsing JSON-SEQ with jq**
- Use `jq --seq` (jq 1.6+) to parse JSON-SEQ directly, or strip the Record Separator first: `tr -d '\x1e' < file.mlog | jq '.'`

**mlog is empty (header only, no events)**
- The QUIC connection was established but no MoQ messages were exchanged
- Check that your client is sending CLIENT_SETUP on the control stream after connecting

**No SERVER_SETUP in response to CLIENT_SETUP**
- The relay may not support the MoQ Transport version(s) your client offered
- Check the `supported_versions` array in your CLIENT_SETUP against what the relay supports

**Correlating with QUIC qlog**
- Both mlog and qlog use the same Connection ID
- qlog captures QUIC-layer events (handshake, streams, congestion); mlog captures MoQ-layer events
- For a complete picture, fetch both: `/qlog/<cid>` and `/mlog/<cid>`

## Further Reading

- [draft-pardue-moq-qlog-moq-events](https://datatracker.ietf.org/doc/draft-pardue-moq-qlog-moq-events/) — The IETF draft defining the mlog event schema
- [QLOG_SETUP.md](QLOG_SETUP.md) — QUIC qlog setup for the relay
- [moq-test-client README](../moq-test-client/README.md) — Test client documentation and available test cases
- [draft-ietf-moq-transport](https://datatracker.ietf.org/doc/draft-ietf-moq-transport/) — The MoQ Transport protocol specification
