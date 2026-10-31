# Vendored vnt-core

Source: https://github.com/vnt-dev/vnt
Revision: 18d9aea5e365006ed67c5e4d7be02eb6af0c95cb
Version: 2.0.11
License: Apache-2.0 (LICENSE).

Local changes add remote desktop application channels and client registration.
Builds use this checked-in source; no sibling checkout is required.

This vendored fork provides remote desktop application channels, server
registration/subscriptions, P2P discovery, NAT punch, server transports, encryption
and compression. Application QUIC IO lives in `src/application_io`; public network
readiness is `wait_network_ready()`. It requires no virtual network interface,
OS routing changes or sibling checkout. Media FEC is owned by `vnt-net`.

Desktop registration uses connection IDs. Subscription settings are validated
against the remote application configuration fields. P2P direct handshakes require
full node identity and a matching network code; UDP punch packets require the
network-code hash. Punch policy is an explicit bit set, with zero enabling no modes.
The matching vnts server must support remote connection groups and the v2
subscription format.
