# Remote session transport

A remote session owns its authentication, clipboard operations and media state.
Sockets belong to replaceable channel generations; losing a socket does not drop
an application session. The desktop host and Windows service Broker both enter
through `vnt_net::session::SessionListener`; named-pipe IPC uses
local process authentication.

## Channel ownership

All remote TCP sockets connect to the configured port. The first framed message
selects a new `Hello`, control recovery, or an auxiliary binding. The protocol's
`session::channel_for` and `allowed_on` define channel ownership in one place.

| Channel                  | Messages                                                                                                                                |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| Control TCP              | Initial authentication, input, heartbeat, telemetry, clipboard text/offer/image descriptor/cancel, file requests, connection management |
| File TCP                 | All file replies (including empty acknowledgements and errors), PNG chunks                                                              |
| Media TCP, TCP mode only | StreamReset, AudioState, video and audio frames                                                                                         |
| UDP, hybrid mode         | Video/audio packets and UDP binding                                                                                                     |

Each lane has an independent bounded mailbox and socket writer. Receive queues
are independent too: a full file queue parks that socket's reader, not the control
supervisor. Media configuration uses a priority application mailbox and then the
same media socket as frames. A reset drains obsolete queued frames and waits for
a keyframe. The Windows service uses separate user-agent pipe sessions for audio
and clipboard; its file forwarding task cannot wait inside the main control loop.

Clipboard image chunks wait for `ClipboardImageReady`, emitted after the receiving
clipboard actor creates its assembly. Image IDs and file-connection generations
reject stale confirmations. File replies bypass disk processing queues.

QUIC uses separate reliable control/file streams and media DATAGRAMs. VNT uses
its reliable application channels and media datagrams. Both follow the same
application session ownership and recovery rules; neither uses native UDP binding.
See [direct QUIC](direct-quic.md).

## Credentials

Initial authentication uses a fresh nonce and HMAC-SHA256 password proof.
A blank configured password permits passwordless access. Welcome carries a
random 32-byte session token, kept only in memory for this session.
Recovery proves possession with HMAC-SHA256 over a domain marker, session ID,
random 16-byte attempt ID and fresh 32-byte server nonce. The original session
token never changes and is not sent again during recovery.

File/media binding grants are different random 32-byte secrets, scoped to the
session, channel and generation. Grants expire in ten seconds, are consumed once,
and are invalidated on control failure. TCP attachments also require the original
control peer's IP. UDP continues to use a separate bind token.

These mechanisms authenticate channel ownership. Direct TCP/UDP is unencrypted;
QUIC uses TLS 1.3 and VNT uses the core transport encryption.
Do not log token/proof message bodies. The desktop protocol version is 1; both endpoints must use the same version.

## Recovery and synchronization

Clients reconnect immediately, then after 0.5, 1, 2, and at most 4 seconds. A single
connection/handshake is bounded to five seconds. A continuous outage has one fixed
30-second deadline, not extended by failed attempts.

On control loss, release all held remote input and suspend native keyboard capture.
Keep healthy file/media sockets and the local WebSocket bridge. Drop stale input,
coalesce clipboard publications to the latest state, and suspend new file requests.
Already accepted requests can still complete over the healthy file socket.

After recovery authentication, atomically replace the control generation. Replay
current state before a synchronization marker; the client acknowledges that marker
after its application reader has applied the snapshot. If the snapshot revision
changed in the meantime, synchronize again. The server commits the acknowledged
revision before either endpoint reports control as available; only then resume
control state and any paused UDP configuration. File operations retain IDs
and offsets. Unanswered logical requests are reconciled with their original IDs;
the source caches bounded replies so a Start or Read cannot be applied twice.
Old input and physical socket queues are never replayed.

If the file socket itself fails, cancel file/image operations and wake waiters,
but keep text/metadata synchronization enabled. Rebinding republishes the current
file offer and restarts the current image; files require a new paste. A failed
media socket drops old generations and playback state and restarts from current
configuration plus a keyframe. Healthy auxiliary sockets survive control recovery.
A broken auxiliary socket waits for control recovery before requesting a new grant,
then gets its own 30-second recovery window.

Explicit SessionEnd, user disconnect, service shutdown and fatal session errors
end the session immediately. Missing/expired sessions are denied without falling
back to the password. Final cleanup revokes credentials, cancels readers/writers,
releases file/input resources and ends retry tasks. Server restart intentionally
invalidates all in-memory recovery tokens.

## Validation

- `cargo test -p vnt-proto -p vnt-net`
- `cargo test -p vnt-host --lib`
- `cargo test -p vnt-remote --lib`
- `npm test` and `npx vue-tsc --noEmit`

Network tests use real loopback TCP sockets, including repeated control recovery,
independent file recovery, bounded receive backpressure, one-use/expired binding,
invalid proofs and a virtual-clock 30-second cleanup test. Clipboard and Windows
IPC tests isolate publication sinks to avoid replacing the user's clipboard.
Actual separate-machine LAN/firewall and installed Windows service interaction
remain manual integration checks.
