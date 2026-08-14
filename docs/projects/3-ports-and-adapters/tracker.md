# Ports & adapters: time and transport behind traits

> **Status: 📝 scoping (2026-08-12).** Project **3-ports-and-adapters**. Not yet
> started — this is the plan. The goal is to finish the ports/adapters story on
> the **client** side: put *time* and *network transport* behind traits so the
> domain logic (fan-out, reachability, delivery) is testable in-process,
> deterministically, in milliseconds — without giving up a thin tier of real
> end-to-end tests.

Governed by the standard slice discipline (AGENTS.md): small vertical slices,
one per turn, each runnable and measured before the next. The invariants in
AGENTS.md always hold; in particular this project touches **no wire format, no
`zink-protocol` core, and adds no runtime dependency** (test-only deps must be
justified).

## 1. Why — and what this is *not*

The relay crate already does this right: `zink-relay/src/clock.rs` defines a
`Clock`/`WallClock` port with a `TestClock`, and the relay's retention tests
assert 100-second windows in ~4 ms by advancing a mock clock. The
**`zink-client` / `zink-cli` layer never got the same treatment** — it calls
`Instant::now()` / `SystemTime::now()` / `n0_future::time::sleep` directly
(`client.rs:1048, 1102, 3364, 3768, 4414, 4431, 6265`), reaches iroh directly
for all networking, and papers over the resulting untestability with a
production env-var back-channel (`ZINK_CONNECT_TIMEOUT_MS` /
`ZINK_CLOSE_DEADLINE_MS`, read at `zink-cli/src/main.rs:958–968`, set by the
e2e harness at `crates/zink-cli/tests/common/mod.rs`). That env-var override is
the tell: production code reading test-only knobs is the abstraction leaking.

**This is not a product-latency project.** That thread was already pulled, twice,
and the fixable half is done — see [`docs/design/fast-failure.md`](../../design/fast-failure.md)
(root cause: *"failure is only ever learned by deadline expiry; nothing signals
it"*) and its landed fixes De6a–d (persisted negative reach evidence, `recv`
partial-failure, parallel per-relay fan-out, explicit reachability signal). The
one remaining "wait a timeout instead of getting a response" case (relay presence
query, **De6e**) was **deliberately declined** on privacy grounds (a pollable
presence API leaks the social graph in sleep/wake timing) and is a SPEC §11
decision, out of scope here.

So the residual slowness is genuinely **test-architecture**: the e2e suite runs
protocol-*logic* assertions through the slowest possible layer (a CLI subprocess
per step, each a cold iroh endpoint + QUIC handshake). This project moves that
logic in-process behind ports. The honest ROI is **determinism and the end of
timing-based flakiness**, plus coverage of failure scenarios a real network
won't produce on command — *not* shaving the first few seconds, which the product
fixes already took (fast-failure.md §7: the in-process harness "buys the last
~3 s, not the first ~6 s").

## 2. Goal & non-goals

**Goal.** Time and network transport in `zink-client` sit behind traits.
Fan-out, reachability, and delivery logic are tested in-process with a mock
clock and small, purpose-built transport doubles — a peer going offline mid-send,
a relay flapping, a message that times out and *then* arrives, reordering within
tolerance — all controlled deterministically, no real time, no real sockets.
A thin, explicit real-iroh smoke tier remains for the things only a real network
proves. The `ZINK_*_MS` env back-channel is decommissioned.

**Non-goals.**
- No product-latency work — done in De6a–d.
- No relay presence query (De6e) — declined, SPEC §11 decision.
- No shortening of production `connect_timeout` / `close_deadline` (fast-failure.md §9).
- No wire/protocol/`zink-protocol` change; no new wire fields.
- **No `client.rs` / `lib.rs` module split** — that is the *next* project (4),
  deliberately sequenced after this one so the split lands on the clean seams
  this project creates (see §6 header).
- No single monolithic "transport simulator" (§4).

## 3. Baseline (measured 2026-08-12, `cargo nextest`, debug)

Whole workspace: **229 tests, ~6.0 s wall** (parallel). Clippy clean, 0 TODO/FIXME.
The cost is a small tail of e2e tests; the floor is set by two things only:

| Layer | Cost per unit | Evidence |
|---|---|---|
| Pure logic (protocol / DAG / crypto) | **< 30 ms** | 37 protocol tests in 0.09 s |
| One real iroh QUIC connection (loopback) | **~60–130 ms** | `zink-relay::mailbox` wire tests |
| One `zink-client` in-proc test (real relay + real deadline) | **0.3–1.3 s** | `delivery__should_pay_one_deadline` 1.04 s |
| One `zink-cli` e2e test (~30 CLI subprocesses, each a cold endpoint) | **1–6 s** | `groups__should_grow_thread_and_shrink` 5.96 s |

Slow tail: `zink-cli` `groups` 5.96 s · `groups/auto_query` 3.84 s ·
`multi_device` 3.66 s · `who_is` 2.39 s. Per-binary serial numbers and the full
decomposition are in fast-failure.md §2–3. Re-measure at the end of each slice;
the point is the *shape* changing (logic tests leaving the subprocess layer),
not a single headline number.

## 4. Guardrails — the traps to avoid

The transport-trait design is the load-bearing slice (S3) and is **deliberately
not fleshed out here**. What *is* fixed now is the set of constraints it must
satisfy, so we don't design it into a corner:

- **The stable artifact is the trait, not a fake.** The port is a narrow trait
  plus an explicit contract of what a caller may assume — and may *not*. Test
  implementations are small, per-scenario, and composable, **never** one
  full-behavior simulator of iroh (which is bound to drift and lie). "Reusable
  primitive" ✅; "monolithic emulator" ❌.
- **Don't bake in guarantees the real system can't keep.** No implicit
  in-order delivery, no "a connected send always succeeds," no "dial fails
  fast." The contract promises only what iroh actually promises. This is the
  single biggest risk and the reason S3 exists.
- **Capabilities may split into separate traits.** Sending bytes to a pubkey and
  accepting an inbound stream are different capabilities; forcing them into one
  trait may be wrong. Decide in S3 — the guardrail is only that the split serves
  testability and keeps each trait's contract honest.
- **The seam sits *below* the domain logic.** Fan-out, reachability
  (reachability-by-*trying* — a fact the client owns, fast-failure.md Option B),
  negative-evidence cooldown, and per-relay strategy stay in `zink-client` and
  stay in-process-tested. The port abstracts the byte/stream transport keyed by
  pubkey, not the delivery decisions above it.
- **Failure is first-class.** Peer offline, dial → timeout, mid-send failure,
  per-peer / per-relay down, and the three-way readiness state
  (`Reachable::{ByKey, NoHomeRelay, NotYet}`; iroh's `online()` never resolves
  without a relay) must all be *expressible in the doubles*. A double that can't
  model these hides exactly the paths De6a–d fixed.
- **Time and transport are co-designed to be driven together.** The archetype
  test holds the transport silent, advances the mock clock past the deadline to
  fire the timeout deterministically, asserts the fallback, then pushes the
  message to prove recovery. Neither port alone gives you that.
- **Never zero real-network tests.** Keep a thin, explicit smoke tier on real
  iroh (handshake, holepunch/relay-fallback, `online()` readiness, blob
  streaming, graceful-close drain). The in-process layer is a scalpel for
  scenarios a real network won't produce on command — *not* a new home for tests
  that were always unit-level (DAG merge, BORSH determinism, crypto roundtrips
  stay plain unit tests, neither port in sight).

## 5. Graduation plan (separate the log from the knowledge)

This tracker is the *log* (what/when). The durable *how-and-why* graduates as it
lands:
- **`docs/design/transport.md`** — the transport trait(s), their contract, the
  seam placement, and the test-double kit + discipline. Written in S3, edited as
  the adapters land. Cited from `//!` in the client transport module.
- **`docs/design/` clock note** — either extend the relay's clock rationale or a
  short client-side note on the injected `Clock` (small; may just be a `//!`).
- **An ADR under `docs/decisions/`** — "time and transport behind ports in the
  client," tied to the *I/O-at-the-edges* tenet, recording *why* (testability,
  determinism, no timing flakiness) and the rejected alternative (env-var
  timeout knobs / one big fake).

## 6. Slices

Ordered so each is runnable and measured. **DoD (every slice):** builds ·
`cargo fmt` + `clippy` clean · `cargo test` / `cargo nextest` green · timing
re-measured where relevant · this tracker updated · durable bits graduated per §5.

**Tier 1 — Clock port into the client**
- [x] **P1 · `Clock` port in `zink-client`.** ✅ 2026-08-13. New
  `crates/zink-client/src/ports/clock.rs` (at `clock.rs` until the P4-era
  `ports`/`adapters` split) mirrors the relay's `clock.rs`: `Clock`
  (monotonic `now` + `sleep`) and `WallClock` (`now_ms`) traits, `SystemClock`
  implementing both. Injected into `Client` as **one generic parameter with a
  default** — `Client<C: Clock + WallClock = SystemClock>` — mirroring the
  relay's `InMemoryStore<C = SystemClock>`, *not* a trait object. Edges
  (`zink-cli`, the excluded Tauri app) keep writing bare `Client`/`Arc<Client>`
  and needed **zero** changes; production monomorphizes to `SystemClock` with
  no allocation or type erasure, and `sleep` is `impl Future` (RPITIT) rather
  than boxed. All production time calls now route through `self.clock`; `fmt`
  + `clippy --all-targets` clean; 229/229 tests in ~6.0 s (unchanged — P1 is
  behavior-preserving); native + `wasm32` both compile. *Done:* no
  `Instant::now` / `SystemTime::now` / `sleep` outside the adapter in
  production code.
  - **Scope corrections vs. the original line-number list:** of the seven
    `client.rs` sites, four (`3768/4414/4431/6265`) were **`#[cfg(test)]`
    elapsed-assertions**, deliberately left for P2 to convert into *scheduling*
    assertions. The real production monotonic sites were just two (backoff
    `sleep`, elapsed logging). The wall site (`now_ms()`) had **12 callers
    across three files**; the ten inside `Client` now use `self.clock`, while
    the two that live outside a `Client` and whose timing nothing asserts
    (`state.rs` first-seen, `sync.rs` reach-seen) stay on a `SystemClock`-backed
    free `now_ms()` — threading a clock into `ClientState`/`SyncHandler` for
    those is deferred until a slice needs it (would require `ClientState<W>` /
    `SyncHandler<W>`, out of P1's scope).
- [x] **P2 · `TestClock` + migrate in-proc time waits.** Split into two:
  - [x] **P2a · Time doubles.** ✅ 2026-08-13. In `ports/clock/test_clock.rs`
    (behind `#[cfg(test)]`), **one double per port** — wall and monotonic time
    move independently in the real world, so tests can drive them apart (e.g.
    a wall rewind under monotonic progress). `TestClock` (monotonic): `advance`
    moves time and fires parked `sleep`s; `sleep` registers on first poll and
    deregisters on drop (so a race's losing timer stops counting);
    `wait_for_sleepers(n)` resolves once `n` timers are parked. **Scoped, not
    global** — it drives a deadline while real iroh I/O on the same runtime
    keeps its own timers (tokio's `pause()` can't: it needs a current-thread
    runtime and would freeze iroh too; tests here are `rt-multi-thread`).
    `TestWallClock`: a settable value — `set_ms` jumps forward or backward,
    which is how the real wall clock misbehaves. `Client` takes one type
    parameter per port (see decisions log). Unit tests deterministic in ~0 ms,
    incl. the archetype "two concurrent sleeps fire on a single advance"
    (serial code parks only one, hanging `wait_for_sleepers(2)` — that *is*
    the concurrency assertion). First consumer: `load_unreachable` now
    **drops future-dated negative evidence** (`checked_sub`, not
    `saturating_sub`) — under a wall rewind the old filter aged persisted
    failures to 0 ms forever, suppressing dials to reachable peers for the
    whole rewound span; a `TestWallClock` rewind test pins the fix. 234/234
    green, clippy clean, wasm unaffected (`client.rs`/`clock.rs` are
    native-only).
  - [x] **P2b · Migrate the in-proc time waits.** ✅ 2026-08-13. `Clock` gains
    a provided `timeout` — raced against `sleep`, so a `TestClock::advance`
    fires it deterministically — and every production deadline routes through
    it: `net::{connect, connect_addr, deposit_with_retry}` and
    `blobs::{push_blobs, fetch_encrypted}` take `&impl Clock`, and `Client`'s
    `close`/`online` waits use `self.clock`. No `n0_future::time` left in
    production code. Also fixed a P1 leak: `subscribe_once` logged a
    `self.clock.now()` start against *real* time via `Instant::elapsed`.
    `delivery__should_pay_one_deadline_for_two_dead_relays` is the archetype
    conversion: a `TestClock`-built client, `wait_for_sleepers(2)` as the
    parallelism assertion, one `advance` past a 10 s (fake) deadline —
    **1.04 s → 0.04 s**, `elapsed <` checks retired. 234/234 green, clippy
    clean, wasm unaffected.
  - *Done when (revised):* the pure-deadline waits are gone. The remaining
    real-time waits (`who_is` concurrency, `unreachable_peer`, the
    mailbox-fallback sends, the `online` smoke) each mix a **live** dial with
    dead ones on one clock — advancing a shared `TestClock` would fire the
    live dial's deadline too. They migrate with the P5 transport doubles
    (dead peers modeled in the transport, not on the clock); the `online`
    smoke stays real-network (P7).

**Tier 2 — Transport seam (design + first cut)**
- [x] **P3 · Design the transport trait(s).** ✅ 2026-08-13. The guarded design
  slice — [`docs/design/transport.md`](../../design/transport.md) written and
  checked against every §4 guardrail; no code changed (per DoD). The shape:
  **verb-named capability traits** (`Dial`, `Request`, `AcceptUni`,
  `DialBlobs`, `PushBlob`, `FetchBlob`, `Accept`, `Respond`, `Home`,
  `InsertRelay`, `RemoveRelay`, `Close`),
  aggregated by a blanket-impl'd `Transport` supertrait into one `Client`
  parameter (`Client<C, W, N: Transport = IrohTransport>`); connections are
  concrete noun-objects implementing the verbs. **Frame-level, not
  stream-level** (every mailbox/sync exchange is one framed request per
  bi-stream — the inventory showed no call site needs raw streams); **blob
  ops at intent level** (durable-receipt push / hash-verified fetch;
  iroh-blobs mechanics stay in the adapter — re-modelling bitfields in doubles
  is the §4 emulator trap); **pull-style inbound** (`accept()` yields
  `Inbound { peer, frame, reply }` into a domain-owned serve loop); **no time
  inside the port** — no `Duration` params, no adapter timers, every deadline
  a domain-side `clock.timeout` race, which is what lets `TestClock` drive
  transport waits. Errors are variant-free (`DialError`/`ConnError`) — the
  domain maps failures structurally by which port call failed; a variant is
  added only when logic branches on it. Local addressing fell off the ports
  (CLI-print-only → inherent on `IrohTransport`, production-only impl block).
- [x] **P4 · Real iroh adapter behind the trait(s).** ✅ 2026-08-13. New
  `ports/transport.rs` (the ports, verbatim from transport.md) and
  `adapters/iroh.rs`: `IrohTransport` (endpoint + router + inbound queue,
  cheaply clonable) wears every port; `IrohConn`/`IrohBlobConn`/`IrohReply`
  are the concrete connection objects. The P3 watch-item resolved cleanly —
  the `Router` did **not** fight the pull surface: a small `ForwardHandler`
  reads one request per bi-stream and forwards `(peer, frame, reply)` into a
  bounded mpsc that `accept()` pulls (tokio's `sync` feature added as a
  direct native dep — already compiled into every build via iroh; justified
  in Cargo.toml). `net.rs`/`blobs.rs` are now port-generic domain helpers
  taking the narrowest verb (`connect(&impl Dial, …)`,
  `push_blobs(&impl DialBlobs, …)`); `sync.rs` keeps `SyncHandler` as domain
  logic over `Inbound` frames, with `serve()` a domain-owned pull loop
  spawning per request (cross-connection concurrency preserved). `Client` is
  `Client<C, W, N: Transport = IrohTransport>`; **zero iroh types remain in
  the production code of `client.rs`/`net.rs`/`blobs.rs`/`sync.rs`** — the
  dial-spec/relay-URL parsers live in the adapter as plain functions (their
  string formats embed iroh's id/url encodings). Local addressing landed as
  designed: `sync_address` is adapter-inherent, surfaced by a production-only
  `impl<C, W> Client<C, W, IrohTransport>` block. **Proof: 234/234 through
  the adapter**, clippy clean, `wasm32` compiles, suite wall time unchanged
  (`groups` 5.96 s). Deliberate micro-changes, none test-visible: the serving
  gate + inbound reach note resolve per *request* rather than per connection
  (a mid-connection contact add now serves immediately; a connection that
  never sends a request leaves no reach evidence); requests within one
  connection may overlap (requesters are serial per connection in practice);
  the nudge accept now reads the zero-length uni frame (64-byte backstop
  cap); blob staging is per-push inside the adapter (no shared `MemStore` —
  `restage_owed` became the synchronous `reload_owed`); `set_profile`'s relay
  diff compares parser-normalized URL strings.
- [x] **P5 · Test-double kit + first migration.** ✅ 2026-08-14. The kit, in
  `ports/transport/test_transport.rs`: `TestTransport { dial, blobs, accept,
  home }` (one `Clone`-shared handle scripts and inspects everything) with
  `ScriptedDial` (per-key connect/refuse/hold queues + a `dialed` counter),
  `TestConn` (exact-frame replies, fail/hold, request recorder, uni sender),
  `ScriptedDialBlobs`/`TestBlobConn`, `ChannelAccept` (inject → served
  response), `TestHome` (`set_online`). One design refinement over the P3
  sketch, recorded in transport.md §7: instead of literal `Unused` stubs,
  **honest defaults** — remote-initiated capabilities default to silence
  (`accept`/`accept_uni`/`online` pend, which the always-running serve loop
  requires), unscripted domain-initiated actions panic (an `Err` would vanish
  into best-effort handling; silence would hang the test, not fail it).
  `Client` gained `assemble` (shared wiring) and a `#[cfg(test)]`
  `with_transport` constructor — no endpoint, no I/O. Migrated + landed, each
  deterministic in **≤ 9 ms**: the §4 archetype
  (`delivery__should_recover_when_a_dead_relay_returns` — silence → one
  `advance` → outbox fallback → relay returns → flush recovers; a scenario a
  real network can't produce on command), `who_is` concurrency (1.3 s → 7 ms,
  `wait_for_sleepers(3)` as the assertion, elapsed-bound retired),
  `unreachable_peer` persistence (real relay + two homed clients → 9 ms, and
  the assertion sharpened from a budget check to `dialed(&absent) == 0`), and
  two-dead-relays (off TEST-NET sockets entirely). P2b's stragglers are now
  two: the mailbox-fallback sends and `delivery__should_keep_delivering…`
  need two in-process clients (the P6 loopback); the `online` smoke stays
  real-network (P7). Suite: 237/237, wall ~6.0 s (the tail is zink-cli
  subprocess e2e — P6's target).

**Tier 3 — Complete the migration + smoke tier**
- [ ] **P6 🎯 · Migrate remaining logic assertions.** Groups grow/shrink,
  multi-device carry, `recv` partial-failure → in-process transport + clock.
  Delete the pure-logic subprocess e2e they replace. **Decommission
  `ZINK_CONNECT_TIMEOUT_MS` / `ZINK_CLOSE_DEADLINE_MS`** (production reads and
  harness sets). From the P5 review: subscribe-loop migrations put dials in
  *spawned* tasks, where an unscripted-dial panic hangs instead of failing —
  see transport.md §7 before writing those; and **delete any kit control
  still `#[allow(dead_code)]` when this slice closes** (built to §7's design,
  not to a proven need — they don't get to linger).
- [ ] **P7 · Define the real-network smoke tier.** The minimal, explicitly
  labelled set that stays on real iroh (§4 list). *Done when:* the real-network
  tests are few, named as smokes, and everything else is in-process.
- [ ] **P8 · Re-measure + graduate.** Update §3’s table; land the design doc,
  clock note, and ADR (§5); record the test-double discipline.

## 7. Decisions log

| Decision | Resolution |
|---|---|
| Layer | Ports live in `zink-client`; `zink-protocol` core stays pure (tenet: I/O at the edges). |
| Time | A `Clock` port mirroring the relay's, injected into `Client`; `TestClock` advanceable. |
| Injection mechanism | **Generics with default type parameters, one parameter per port** (`Client<C: Clock = SystemClock, W: WallClock = SystemClock>`), *not* `dyn`/`Arc<dyn>` and *not* one `C: Clock + WallClock`. No heap allocation or type erasure; defaults mean edges keep writing bare `Client`, as the relay's `InMemoryStore<C = SystemClock>`; `sleep` is `impl Future` (RPITIT), free across a generic seam. Separate parameters because wall and monotonic time are separate dependencies: impossible to confuse, a test injects exactly the half it drives, helpers receive only the capability they need. |
| Test doubles for time | **One double per port**: monotonic `TestClock` (advance + parked-sleep registry) and settable `TestWallClock` (`set_ms` — jumps, not flow). Test under adversarial conditions (wall rewind while monotonic advances), not idealized ones — a lockstep double makes the rewind inexpressible. Share *mechanism* (the waker registry, whose behavior *is* the port contract); keep *scenario* in each test by how it drives the double. |
| Wall-clock reach | Two un-asserted wall sites (`state.rs` first-seen, `sync.rs` reach-seen) stay on a `SystemClock`-backed free `now_ms()`; injecting them would need `ClientState<W>`/`SyncHandler<W>` and buys no current test — deferred, not in P1. |
| Transport | **Resolved in P3** — see [transport.md](../../design/transport.md). Verb-named capability traits (`Dial`/`Request`/`AcceptUni`/`DialBlobs`/`PushBlob`/`FetchBlob`/`Accept`/`Respond`/`Home`/`InsertRelay`/`RemoveRelay`/`Close`), frame-level, keyed by `Peer` (the pubkey identity + fallible route hints); seam below fan-out & reachability; one `Client` parameter via a blanket `Transport` supertrait (facets of one endpoint — unlike the two clocks). |
| Blob ops | Intent-level (`PushBlob` = durable receipt, `FetchBlob` = hash-verified bytes), not stream-level; iroh-blobs mechanics are adapter detail, real streaming covered by the P7 smoke tier. |
| Time in transport | No `Duration` params, no timers in adapters; every deadline is a domain-side `clock.timeout` race — the rule that keeps `TestClock` sovereign over all waits. |
| Accept style | Pull: `accept()` yields `Inbound { peer, frame, reply }` to a domain-owned serve loop; adapter forwards `Router` accepts into the pull surface. |
| Test doubles | Small composable per-scenario doubles, not one simulator (§4); one double per capability, controls (hold / connect-after-hold / fail), real BORSH frames. Honest defaults: silence for remote-initiated capabilities, a loud panic for unscripted domain-initiated actions — discipline in transport.md §7. |
| Module layout | `ports/{clock,transport}.rs` (traits + plain data; doubles as `#[cfg(test)]` submodules of their port — the port's contract kit) vs `adapters/{iroh,system_clock}.rs` (the real world, one file per technology — no per-port nesting, we've sworn off a second transport). The tree carries the dependency direction; "no iroh outside `adapters/`" is a one-glob audit. An adapter as a *child* of its port (the P4 first cut) inverted that story. |
| Smoke tier | Retained and explicit; never zero real-network tests. |
| Env knobs | `ZINK_*_MS` decommissioned once the subprocess logic tests are migrated (P6). |
| De6e | Out of scope — SPEC §11 decision, declined on privacy grounds. |
| Sequencing | Before the module split (project 4): ports move the seams, so the split lands clean. |

## 8. Follow-ups / parked

- **Project 4 — module split** (`client.rs` ~6.8 k lines, `app/ui/src/lib.rs`
  ~2.4 k). Sequenced after this; the port seams become the module lines.
  Fold in a **comment-pruning pass** while there: many `client.rs` docstrings
  carry "why"/mechanism/testing narration that belongs in design docs, not the
  code (Kevlin-lean — a docstring says what a caller needs, nothing more). The
  split already rewrites every file, so prune then rather than sweep twice.
- **Lazy endpoint bind** (fast-failure.md F5 / Option E) — local reads pay a full
  endpoint bind; unscheduled, CLI-only beneficiary.
- **De6e advisory reach bit** — SPEC §11 decision if ever revisited.
- **Mid-run wall-rewind exposure.** `load_unreachable` now drops future-dated
  persisted evidence, but the *in-run* ages still use `saturating_sub` against
  live wall time (`client.rs` ~807 flush triage, ~2918/2921 `direct_budget`
  cooldown/known checks): a rewind mid-process makes in-memory failures look
  fresh (age 0) until wall time catches up. The deeper question is whether
  in-run evidence ages should be *monotonic* (elapsed-since is a process-local
  concept; only persistence needs wall). Needs the `Reach` struct to carry an
  instant or both stamps — a design decision, not a drive-by; revisit when P2b
  touches this code.
