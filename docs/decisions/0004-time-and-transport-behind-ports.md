# 0004 · Time and transport behind ports in the client

- **Status:** Accepted
- **Date:** 2026-08-14
- **Tenets:** 6 (best-effort over guarantees), 7 (honesty over false order) — the port
  contracts encode both; the architectural driver is STYLE.md's *"business logic is
  pure and testable; I/O lives at the edges."*
- **Where it landed:** project 3-ports-and-adapters, P1–P7 (design:
  [`docs/design/transport.md`](../design/transport.md))

## Context

`zink-client`'s domain logic — fan-out, reachability-by-trying, delivery, drains,
who-is — called `Instant::now()`/`sleep` and iroh directly. Every failure-path test
needed a real endpoint, a deliberately dead network, and real time; protocol-logic
assertions ran through CLI subprocesses (a cold endpoint + QUIC handshake per step);
and production code read test-only env knobs (`ZINK_CONNECT_TIMEOUT_MS` /
`ZINK_CLOSE_DEADLINE_MS`) — the tell that the abstraction was leaking. Scenarios a
real network won't produce on command — a dial that outlives its deadline and *then*
completes, a relay flapping mid-send, a wall-clock rewind — were untestable.

## Decision

Time and network transport in `zink-client` sit behind **ports**: verb-named
capability traits in `ports/{clock,transport}.rs`, with the real world confined to
`adapters/{system_clock,iroh}.rs` ("no iroh outside `adapters/`" is a one-glob
audit). Injection is by generics with defaults
(`Client<C: Clock, W: WallClock, N: Transport = IrohTransport>`), never trait
objects. Three rules carry the value:

- **No time inside ports or adapters.** Every deadline is a domain-side
  `Clock::timeout` race, derived from `sleep` — so a test clock's `advance` fires
  any timeout deterministically. A timer inside an adapter would be invisible to
  the test clock and end determinism.
- **Ports promise only what iroh promises.** No ordering, no bounded time, no
  learning from an error whether the remote processed anything — the per-capability
  contract is transport.md §3, and a change to it goes through that doc first.
- **Doubles are per-capability controls, never a simulator.** Scripts are data
  (exact BORSH frames, hold/connect queues); two in-process clients talk over a
  dumb loopback and both run their real handlers.

A named **real-network smoke tier** (transport.md §8) keeps what only a real
network proves — homing, holepunching, blob streaming, established-path survival —
each labelled in code. Everything else asserts in-process.

## Consequences

- Protocol-logic tests are deterministic and run in milliseconds; the workspace
  suite went from ~28 s (pre-De6) and ~6 s (project start) to **~1 s wall**, and
  timing flakiness is structurally gone from the in-process tier.
- The `ZINK_*_MS` env back-channel is decommissioned — edge tuning is a documented
  CLI flag, an interface instead of a side door.
- Failure scenarios are constructed, not awaited: held dials, scripted frames,
  wall rewinds, peers that go offline for exactly one dial.
- The smoke tier must stay small and premise-pinned: a flaky smoke gets its racy
  premise made into an observable, awaited fact — never a retry, which would mask
  the very regression the smoke watches (transport.md §8).
- Rejected along the way: env-var timeout knobs (production reading test-only
  config) and one full-behavior iroh fake (bound to drift and lie) — the stable
  artifact is the trait plus its contract, not any double.
