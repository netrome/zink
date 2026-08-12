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
  `crates/zink-client/src/clock.rs` mirrors the relay's `clock.rs`: `Clock`
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
- [ ] **P2 · `TestClock` + migrate in-proc time waits.** Advanceable clock;
  the `zink-client` lib tests that currently wait real backoff/deadline advance
  mock time instead. Convert `delivery__should_pay_one_deadline` to assert
  *scheduling* (two concurrent timers, clock advanced once), not wall-clock.
  *Done when:* the client-crate suite has no real-time waits.

**Tier 2 — Transport seam (design + first cut)**
- [ ] **P3 🎯 · Design the transport trait(s).** The guarded design slice (§4).
  Produce `docs/design/transport.md`: the trait split (send-to-pubkey vs
  accept-inbound), the contract (no ordering/no guaranteed-success/dial→
  success|timeout), seam placement below fan-out & reachability, and the
  test-double kit shape. *Done when:* the design doc exists and passes the §4
  guardrails; no code need change yet.
- [ ] **P4 · Real iroh adapter behind the trait(s).** Extract the iroh usage in
  `zink-client` behind the P3 trait(s); the production adapter implements them;
  existing tests pass *through* the adapter (proves the seam is right, no
  behavior change).
- [ ] **P5 · Test-double kit + first migration.** Small composable doubles
  (`VecTransport` / `ChannelTransport` / `ControllableTransport`), composed with
  `TestClock`. Migrate a first batch of logic tests (fan-out, who-is resolution,
  offline-then-arrive) off subprocess/real-iroh to in-process. *Done when:* the
  archetype scenario (§4) runs in-process in ms.

**Tier 3 — Complete the migration + smoke tier**
- [ ] **P6 · Migrate remaining logic assertions.** Groups grow/shrink,
  multi-device carry, `recv` partial-failure → in-process transport + clock.
  Delete the pure-logic subprocess e2e they replace. **Decommission
  `ZINK_CONNECT_TIMEOUT_MS` / `ZINK_CLOSE_DEADLINE_MS`** (production reads and
  harness sets).
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
| Injection mechanism | **Generics with a default type parameter** (`Client<C: Clock + WallClock = SystemClock>`), *not* `dyn`/`Arc<dyn>`. Rationale: no heap allocation, no type erasure, full monomorphization; a default type param means edges keep writing bare `Client` (zero edge churn), exactly as the relay's `InMemoryStore<C = SystemClock>`. `sleep` is `impl Future` (RPITIT), which the generic seam makes free. Decided P1 (2026-08-13). |
| Wall-clock reach | Two un-asserted wall sites (`state.rs` first-seen, `sync.rs` reach-seen) stay on a `SystemClock`-backed free `now_ms()`; injecting them would need `ClientState<W>`/`SyncHandler<W>` and buys no current test — deferred, not in P1. |
| Transport | A narrow byte/stream port keyed by pubkey, seam *below* fan-out & reachability. Trait split (send vs accept) decided in P3. |
| Test doubles | Small composable per-scenario doubles, not one simulator (§4). |
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
