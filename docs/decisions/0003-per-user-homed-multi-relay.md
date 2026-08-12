# 0003 · Per-user homed, multi-relay connectivity (never a shared relay)

- **Status:** Accepted
- **Date:** 2026-07-18
- **Tenets:** 5 (relays are untrusted infrastructure), 9 (p2p when possible, relayed when necessary)
- **Where it landed:** project 1-mvp · D0b (design: [sync-primitives.md §4](../design/sync-primitives.md)); manual cross-NAT run verified

## Context

Before D0b the client was dial-only: it could reach a peer only at an explicit
`ip:port`. Cross-NAT peer dialing was therefore impossible, which blocks
everything that dials a peer *by key* — auto-sync, `who-is-this`, and direct
delivery. iroh solves NAT traversal, but it needs each endpoint **homed** to a
relay it can be reached through.

The tempting shortcut — home everyone to one shared relay — would recreate the
central chokepoint the whole design rejects.

## Decision

- Run the **iroh relay server inside the `zink-relay` binary** (one service =
  iroh relaying + mailbox + blobs; `tls: None`, so no domain/cert for native
  clients).
- Clients **home to their *own* relays** via `RelayMode::Custom` — still
  **multi-relay**, **never a single shared relay**.
- Dial a peer **by key** through their `RelayUrl`; iroh **hole-punches** to a
  direct P2P path when it can and **falls back to relaying the (encrypted) QUIC**
  through the relay when it can't.
- The `RelayUrl` is paired with its mailbox dial string in **one structured
  `RelayEntry { mailbox, relay_url }`** in the `ContactRecord` — they describe the
  same relay service, so parallel vecs would drift.

## Consequences

- A peer stays reachable across NATs without assuming direct connectivity and
  without routing plaintext.
- Homing applies **at endpoint bind**; a runtime `set_profile` delta (D0d/De5)
  later made relay changes apply without a restart.
- Edges that round-trip the profile must use `home_relay_specs()` (key + URL), not
  the mailbox-only `home_relays()`, or the URL silently drops.
- Follow-ups this unlocked: D0c serving gate, D1 `who-is-this`, D5 direct delivery.

## Ties to the philosophy

Tenet 9 says direct when the transport allows, relayed only when it doesn't —
which is exactly iroh's hole-punch-then-fall-back behaviour, made the default.
Tenet 5 is what makes the fallback acceptable: even when QUIC is relayed, the
relay moves only ciphertext and authenticates nothing about content, so leaning
on it for connectivity costs no privacy. "Home to your **own** relays, never one
shared relay" is the operational form of "relays are untrusted infrastructure" —
no relay is ever a place everyone must trust or route through.
