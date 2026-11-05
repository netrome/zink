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

## Addendum: timing entropy (2026-08-15, project 4)

The same discipline was extended to the third ambient effect: randomness
used for **timing decisions**. `subscribe`'s reconnect jitter drew `OsRng`
directly in domain code, through a float detour ending in panic-capable
`Duration` arithmetic. Now `ports/rng.rs` defines `Draw`
(`draw(bound) → u64`, uniform in `[0, bound)`, timing decisions only),
`adapters/system_rng.rs` implements it over the OS RNG, and `Client` gained
a fourth defaulted parameter (`R: Draw = SystemRng`) under the same
one-parameter-per-port rule. The jitter policy itself is a pure
integer-millisecond function pinned by band-edge tests.

**Cryptographic randomness is deliberately not behind this port**: sealing
and key generation pass a `CryptoRngCore` explicitly at the call site —
`zink-protocol`'s own testable seam — because a scriptable crypto-entropy
port would be a footgun, not a feature. The adapters audit line now reads
"no iroh type, real clock, or timing entropy outside `adapters/`".

## Addendum: storage (2026-11-05, project 9)

The fourth edge joins: client state I/O. ADR 0004 originally left storage
out on purpose — local-disk fs was already deterministic under temp dirs —
but `state.rs` read as layout, encoding and fs mechanics at once, the
crash-ordering claims in its comments were untestable, and the browser
client cannot exist on `std::fs`. Now `ports/store.rs` defines four verbs —
`Get`, `Put`, `Delete`, `List` — under typed, path-like `Key`s (segments
that are legal plain file names by construction), composed as `Store`;
`adapters/fs_store.rs` implements them, and `ClientState<S = FsStore>` is
the typed layer above (layout, encoding, cross-key invariants, no I/O).
The rules, in the same spirit as the three above:

- **Async**, because the browser's backends are (IndexedDB; OPFS sync
  handles live only in workers). The ripple through `Client` and both
  edges was mechanical; paying it again at the PWA would not have been.
- **The port promises little**: atomic replace per key, absent-is-not-an-
  error, a sorted listing — and *no* atomicity across keys and *no*
  durability. Ordering multi-key writes so every crash point stays safe
  is the typed layer's job, never the adapter's.
- **The layout is data, not a migration**: `FsStore` maps a key 1:1 onto a
  path, and a golden test pins every path and byte the pre-port code
  wrote. A mismatch means existing stores no longer read back.
- **Time is data**: `ClientState` takes timestamps as arguments; the last
  ambient wall-clock read in the core (`system_clock::now_ms()`) is gone.

**The double is a journal, not a fault script.** `TestStore` records every
completed write; `crashed_after(n)` is exactly what a crash after the n-th
write leaves (a prefix is all the contract permits assuming), and the
kit's `crash_states` reopens a client on *every* prefix an act spans —
exhaustive, so a claim is checked at every crash point, not only the one
its comment names. A probe or crash test over nothing refuses to run.
On first use it found two real bugs, both fixed by reordering writes:
staging stored a message before its outbox entry (a crash between left a
message rendered as sent that nothing would deliver), and a contact
re-key dropped the old entry before re-pointing its person membership (a
crash between broke the cluster).

**Tiers**: tests construct clients on the doubles by default (the loopback
kit — `TestTransport` + `TestStore`, no temp dir, no key file); real iroh
+ `FsStore` is the named smoke tier plus the adapter's own contract suite,
which runs the same contract clauses against both implementations.

**Not behind the port, deliberately**: `keystore.rs`. Private-key storage
carries its own at-rest-encryption questions and is not a file-per-fact
store; it gets its own decision when the PWA needs it. Wasm `Send` bounds
on the store futures are shared with the transport ports and settled with
the PWA client. The adapters audit line now reads "no iroh type, real
clock, timing entropy, or filesystem access outside `adapters/` — the
keystore aside".
