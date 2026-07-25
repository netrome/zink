# Fast failure — latency diagnosis, with the e2e suite as the probe

Status: **diagnosis, 2026-07-25.** No code changed. Downstream of
[direct-delivery.md §5](./direct-delivery.md) (which measured and half-fixed
this) and of the build plan's **De4** entry (which read the same symptom as
harness-shaped). Proposes slices; decides nothing that touches the wire.

## 1. Why this doc

The CLI e2e suite is slow — `groups` ~10 s, `multi_device` ~5.7 s — while the
in-process client suite does comparable work in 1.5 s. **De4** attributed that
to the harness: a subprocess per step, ~30 full client opens per test, fixed
sleeps around background `listen`. That is true and is part of the answer.

But this project has twice found real product latency by pulling on exactly this
thread (D5's blind dial showed up as `groups` 13 → 38 s *before* it showed up in
the field; the 10 s image send was found the same way), so the suite was treated
here as a **probe of the product**, not as a chore to speed up. The question
asked was: are we polling where we could be reactive, and implicitly waiting
where something could be explicitly signaled?

The answer is yes, and it has one shape.

## 2. Measured baseline

Per-binary, debug profile, this machine, `cargo test` serial per binary:

| Binary | Tests | Time |
|---|---|---|
| `zink-protocol` (lib) | 97 | **0.05 s** |
| `zink-relay` mailbox / blobs | 6 / 3 | 0.15 / 0.56 s |
| `zink-client` (lib, incl. in-process e2e) | 54 | **1.47 s** |
| `zink-cli` groups | 2 | **9.84 s** |
| `zink-cli` multi_device | 1 | 5.73 s |
| `zink-cli` who_is | 1 | 3.58 s |
| `zink-cli` outbox / live / history / contacts | 2 / 1 / 3 / 1 | 1.34 / 1.13 / 1.08 / 0.94 s |
| `zink-cli` threading / fanout / avatar / images / walking_skeleton | 1 / 2 / 1 / 1 / 1 | 0.75 / 0.49 / 0.50 / 0.43 / 0.39 s |

≈28 s total, of which ≈26 s is `zink-cli`. The pure core is instant and DI is
doing its job — the slowness is entirely in the full-stack CLI tests.

## 3. Measured decomposition

Method: a real `zink-relay` on fixed ports (`--port 14400 --relay-port 14401`),
two profiled CLI identities as mutual contacts, each command timed as a fresh
process. Scripts in the session scratchpad; reproduce with any local relay.

**Production defaults** (`connect_timeout` 10 s, `close_deadline` 5 s):

| Command | Peer/relay state | Time |
|---|---|---|
| `pubkey` (no client open) | — | **5 ms** |
| `conversations`, `contacts` (local reads) | — | **68 ms** |
| `recv` | relay up | **86 ms** |
| `send` | relay up, recipient **online** | **134 ms** |
| `who-is` | subject **online** | **111 ms** |
| `send` | relay up, recipient **offline** | **3 684 ms** |
| `who-is` | subject **offline** | **8 064 ms** |
| `send` | relay **down** | **10 657 ms** |
| `recv` | relay **down** | **10 072 ms** |

Every reachable path is ~100 ms. Every unreachable path costs a deadline.

**Isolating the two deadlines** (send to an offline recipient):

| `ZINK_CONNECT_TIMEOUT_MS` (close=200) | 50 | 100 | 300 | 500 |
|---|---|---|---|---|
| send | 356 ms | 401 ms | 581 ms | 796 ms |

| `ZINK_CLOSE_DEADLINE_MS` (connect=500) | 200 | 1000 | 3000 | default 5000 |
|---|---|---|---|---|
| send | 803 ms | 1 584 ms | 3 564 ms | 3 591 ms |

Both are paid **1:1 with the deadline** — the dial deadline in full, and the
close drain in full up to its real ~3 s. So:

```
send ≈ 110 ms of work
     + (dial deadline, iff the recipient is unreachable)
     + (~3 s iroh drain bounded by close_deadline, iff a dial failed)
```

## 4. The root cause

**Failure is only ever learned by deadline expiry. Nothing signals it.**

That single sentence covers every slow path measured. A reachable peer answers
in ~5 ms; an unreachable one is indistinguishable from a slow one until a timer
we chose fires. Since D5, sends speculatively dial peers; since always, deposits
and drains dial relays. Each such dial is a bet whose losing side is paid in
full, and the losses are then made **additive** by serial per-relay loops.

This is not a harness artifact. It is why an unreachable relay costs a real user
10 s per relay, and it is why the suite is slow: the tests spend most of their
time on peers who are deliberately offline, which is exactly the case the system
has no fast answer for.

## 5. Findings, ranked

### F1 · Negative evidence does not survive a process ✅ fixed (De6b)

`direct_budget`'s cooldown — "a peer whose dial just failed gets no dial at all
for 60 s" — lives in an in-memory `sync::ReachMap`. Every CLI invocation is a
fresh process, so the cooldown never applies. Measured, three consecutive sends
to the same offline peer:

```
send to offline bob        802 ms
send to offline bob (2nd)  798 ms
send to offline bob (3rd)  794 ms
```

direct-delivery.md §5 anticipated this exactly — "persisting the failure
cooldown would remove it, if it ever annoys" — and scoped it as a dev-tool cost.
It is more than that:

- It is the **largest single line item in the suite**: `groups` runs ~6 sends to
  offline recipients, ~500 ms of pure dial deadline each ≈ 2.5–3 s of 9.8 s.
- The app pays it too, once per peer per process lifetime — i.e. **on every app
  start**, which on mobile is often. Off the render path since the
  `stage_send`/`deliver` split, but still spent.

The TTL argument for keeping evidence in memory ("reachability is a fact about
*now*, so a fresh process starts from 'don't know'") is right about *positive*
evidence and wrong about *negative*: a timestamped failure is falsifiable on its
face — if it is older than the cooldown it is simply ignored. Persisting
`{key → last_failure_ms}` cannot produce a stale opinion, only skip a dial that
was already known to be a coin-flip.

**Fixed in De6b** (2026-07-25): `unreachable.keys` in the client state dir,
loaded into the reach map at open with cooled-down entries dropped, rewritten
once per fan-out from the live map (which prunes and clears as a side effect).
Consecutive CLI sends to an offline peer went 3 674 → **68 / 86 / 83 ms**;
`groups` 9.96 → **7.97 s**. Details and the accepted trade in
[direct-delivery.md §5](./direct-delivery.md).

### F2 · `recv` aborts the whole drain on the first unreachable relay ✅ fixed (De6a)

`crates/zink-client/src/client.rs:781` — `net::connect(…).await?` inside
`for relay in relays`. The `?` propagates out of `recv`, so with two home relays
and the first one down, **the second is never drained**: messages sitting in a
perfectly healthy mailbox stay invisible until the unrelated relay returns.
Cost of learning this is 10 s, and then the caller gets an error rather than the
mail that was available.

C4a fixed precisely this shape for `send` ("one relay failing no longer aborts
the rest of the fan-out", `SendReceipt.pending_relays`) — `recv` never got the
same treatment. Multi-relay is a tenet, so this is a reliability finding, not
just a latency one. `register_at_home_relays` (`:1267`) has the same abort-on-
first-failure shape, though it fails loudly rather than silently.

**Fixed in De6a** (2026-07-25): per-relay work moved into `drain_relay`, `recv`
returns `RecvReport { received, failed }`, and it errors only when *no* relay
could be drained (first failure verbatim, so the single-relay case is unchanged).
`register_at_home_relays` deliberately left as is: failing loudly there is what
keeps a published record from naming a mailbox that doesn't exist.

### F3 · Serial per-relay work makes deadlines additive ✅ fixed (De6d)

| Path | Shape |
|---|---|
| `deliver_direct` (D5) | **concurrent** (`join_all`) ✅ |
| `who_is` (De3) | **concurrent** (`join_all`) ✅ |
| `recv` (`:781`) | serial, **aborts** on first failure → De6a, then De6d ✅ |
| `deliver` (`:539`) | serial, continues → De6d ✅ |
| `flush_outbox` (`:727`) | serial per entry → De6d ✅ (chunked at 8) |
| `register_at_home_relays` (`:1267`) | serial, aborts → De6d ✅ (still all-or-error) |
| blob fetch (`:1023`) | serial try-in-turn (correct: first success wins) — left alone |

The two paths already fixed were fixed reactively, after each was felt in the
field. The rest are the same bet: *n* unreachable relays cost *n* × deadline
instead of one. direct-delivery.md §5.1 already measured this on the blob path
(the 10 026 ms image send — "sequentially per relay").

**Fixed in De6d** (2026-07-25): `recv`, `deliver`, `flush_outbox` and
`register_at_home_relays` all fan out with `n0_future::join_all`. Semantics
preserved in each case — `register` stays all-or-error (publishing a record
naming a mailbox you don't have is still a lie, it just costs one deadline to
find out), `deliver` keeps its discharged-skip and `AllRelaysPending` rule,
`recv` keeps exactly-once cross-relay dedup via a shared set whose `insert`
*is* the dedup point. Two things worth knowing:

- **`flush_outbox` is chunked** (8 at a time), not unbounded. Each in-flight
  entry holds its message's blob bytes twice — loaded from the cache and
  staged — so a long backlog of images fanned out without limit would spike
  memory where the serial version never did. `n` deadlines become
  `ceil(n / 8)`.
- **Blob fetch stays serial and should.** It is a try-in-turn fallback where
  the first success wins, so racing every relay would pull the same bytes
  several times. Its cost is the same additive deadline, but the fix there is
  per-relay negative evidence (§6A), not concurrency.

Measured: `fanout` 2.13 → **1.52 s** (its both-relays-down phase was paying
two deadlines).

### F4 · No "reachable by key" readiness signal ✅ fixed (De6c)

Measured from `listen` start:

```
stdout says "listening on N relay(s)…"      78 ms
actually dialable by key (who-is answers)  991 ms
```

`listen` announces readiness ~900 ms before the thing a peer needs is true —
the endpoint has to home to its relay before anyone can reach it by key. There
is no signal for the transition, so:

- **The tests poll.** `groups`, `who_is`, `live`, `multi_device` all spin
  `sleep(250 ms)` against a 15 s deadline, and each probe is a *fresh CLI
  process* costing ~700 ms. This is the answer to "are we polling anywhere":
  yes, here, and it is the classic case of polling for something that could be
  signaled.
- **The product has a matching hole.** For ~1 s after start, who-is, backfill
  and direct delivery silently fail against this device, and nothing surfaces
  it. iroh exposes `Endpoint::online()`; De2's timing test already uses it.
  `listen` awaiting it and printing an explicit `reachable by key` line makes
  the tests reactive (block on a line) and the state honest.

**Fixed in De6c** (2026-07-25): `Client::await_reachable` — bounded, and
three-way (`Reachable::{ByKey, NoHomeRelay, NotYet}`), because
`Endpoint::online()` **never resolves at all** when no relay is configured, so
awaiting it bare would hang a profile-less client forever. `listen` prints one
`reachability:` verdict line; the four tests block on it (and `live.rs` on the
listener's own arrival lines) instead of sleeping. **Zero `sleep`s remain in
the CLI e2e suite.** `who_is` 3.60 → 2.92 s, `multi_device` 5.37 → 4.65 s,
`live` 1.09 → 0.77 s, `groups` 7.97 → 7.02 s.

Two things the work surfaced that the diagnosis had merged into one:
readiness is **two** notions, not one. "Reachable by key" needs a home relay
connection; "subscribed and nudgeable" needs only a mailbox registration,
which is a direct dial and needs no homing at all. `live.rs` wants the second
and is *correctly* never dialable by key (mailbox-only relay) — so demanding
homing there was wrong, and the helper split into `spawn_listener` (verdict
reported, whatever it is) and `spawn_homed_listener` (verdict must be
positive). The ordering in `listen` reflects the same fact: subscriptions are
spawned first and the mailbox drain lands *before* the reachability verdict.

### F5 · Local reads pay a full endpoint bind and graceful close

`conversations`, `history`, `contacts` touch no network, but every CLI command
goes through `open_client` → `bind_endpoint` (QUIC socket, relay transport, QAD
TLS config) + `spawn_sync_router` + `close()`. Measured: **68 ms** of ceremony
around a local file read, versus 5 ms for `pubkey`, which opens no client. At
~10 local invocations per e2e test that is ~0.7 s per test; for the app it is
paid once at start, which is fine.

### F6 · The ~3 s close drain after a failed dial

Known and bounded (`close_deadline`, D5), harness set to 200 ms. Worth
restating only because it is still paid *in full* whenever a dial failed, and
it is the reason an interactive CLI send to an offline peer costs 3.7 s. F1
removes most of its occurrences by not making the failed dial in the first
place.

## 6. Options, and what they imply

**A · Persist negative reach evidence** (F1). ✅ **Done in De6b.**
`{key → last_failure_ms}` in the client state dir, read at `Client::open`,
honoured by `direct_budget`; entries older than the cooldown ignored. No
protocol change, no new dependency, pure policy — `direct_budget` stayed
untouched, since seeding its input was enough. Biggest win per unit of risk, as
predicted. Two threads left hanging deliberately:

- The same trick applies per *relay* (the cheap version of the deposit cost),
  but that one interacts with C4a's reliability stance and should be argued
  separately.
- `direct_budget` checks the cooldown **before** positive evidence, so a peer
  that connects to us inside the window still doesn't re-license a dial.
  Persistence makes that slightly more visible (a restart no longer clears the
  slate). The fix is one line — `reach.failed_ms > reach.seen_ms &&` — but the
  D5 unit test deliberately pins the current precedence (`just_failed` carries
  a *newer* `seen_ms` and asserts `None`), so changing it revises a resolved
  policy rather than implementing this one. Worth a decision, not a drive-by.

**B · Ask instead of guessing** (the true reactive fix). The relay already keeps
a live-connection map per registered mailbox — C4b built it to route nudges. An
additive mailbox op ("is this key connected here?") answers in one ~5 ms
round-trip what a speculative dial takes 600 ms to fail at, and the answer is
*fresher* than any cached evidence. Implications to weigh, which is why this
stays a proposal and not a decision:

- **Privacy.** It asks a relay to report on a third party's presence. The relay
  already knows (it maintains the map for nudges), and the asker is already
  authorized to deposit into that mailbox — but "the relay already knows" is
  not the same as "we built a presence API on top of it", and presence leaks
  are exactly what tenet-level metadata minimization is about. A conservative
  form answers only about mailboxes the asker can already deposit to, and
  says nothing about *when* the peer was last seen.
- **It is a wire addition** — additive to `zink-mailbox/1`, old clients
  unaffected, but SPEC §11 and the wire doc move.
- It does not remove the need for A: a relay we can't reach can't answer.

**C · Parallelize the serial loops** (F3) and **fix `recv`'s abort** (F2).
✅ **Done in De6a (abort) and De6d (concurrency).** Turns *n* × deadline into
max(). F2 went first as the correctness fix it was.

**D · Explicit readiness signal** (F4). ✅ **Done in De6c.** `listen` awaits
`Endpoint::online()` (bounded) and prints a `reachability:` verdict; tests
block on that line instead of polling. Removed ~1 s and all 250 ms-granularity
jitter from four tests. The app surface — showing its own first second honestly
— is **not** wired: `Client::await_reachable` exists for it, but a UI
indicator is its own slice, and only a multi-relay or fresh-profile user would
notice. Worth doing when the profile/pairing screens next get attention (it is
the same window De5 chased).

**E · Lazy endpoint bind** (F5). Open the endpoint on first network use, so
local-only commands skip it. Contained (`open_client` / `with_device`), but it
touches the one place `spawn_sync_router` lives, so it is not free — and its
only real beneficiary is the CLI. Lowest priority; listed for completeness.

**F · De4's in-process harness.** Still worth doing, but its framing changes:
with A–D landed, most of what De4 was going to hide stops existing. Re-measure
before spending it — the estimate below suggests De4 buys the last ~3 s, not
the first ~6 s.

## 7. Estimated effect on `groups` (9.84 s)

| Step | Estimate | Actual |
|---|---|---|
| Today | 9.8 s | 9.96 s |
| + A (persisted cooldown; ~6 sends × ~500 ms) | ~7 s | **7.97 s** (De6b) |
| + D (readiness signal replaces the poll loops) | ~6 s | **7.02 s** (De6c) |
| + C (parallel relay work) | ~5.5 s | **7.02 s** (De6d — no change *here*) |
| Floor with the current harness (~30 × 50 ms open + close, ~8 recv × 90 ms) | ~3.5 s | — |
| + F (De4 in-process) | <1 s | — |

De6d moved `fanout` (2.13 → 1.52 s) but not `groups`, and that is the honest
result: `groups` sends to *reachable* relays, mostly one at a time, so it had
no additive deadlines to lose. The estimate credited De6d with ~0.5 s of
`groups` and was simply wrong about where its win lands — the win is real, it
just accrues to multi-relay and down-relay paths, which is where users feel it
too (an unreachable home relay no longer taxes a drain twice).

**Suite total: 28.5 s → 24.7 s** across De6a–d (summed per binary, run
serially). The movers: `groups` −2.8 s, `multi_device` −1.2 s, `who_is`
−0.7 s, `live` −0.4 s, and `fanout` **+1.0 s** — a new regression test that
costs 1.5 s, itself down from 2.1 s once De6d removed its serial deadlines.

Rough, but the ordering is the point: the product fixes are worth more than the
harness rewrite, and they are worth something to users as well as to CI.

## 8. Proposed slicing

Small, independently runnable, each with its own measurement:

- **De6a · `recv` partial failure.** Continue past an unreachable relay, drain
  the healthy ones, report which failed (the `SendReceipt.pending_relays`
  shape). Correctness first, independent of everything else.
- **De6b · Persist negative reach evidence.** Option A. Measured before/after
  on repeated sends to an offline peer and on the suite.
- **De6c · Explicit reachability signal.** Option D, plus moving the four test
  poll loops onto it.
- **De6d · Parallel per-relay work.** Option C for `deliver`, `flush_outbox`,
  `register_at_home_relays`.
- **De6e 🎯 · Relay presence query.** Option B — design decision first
  (privacy stance, wire shape, SPEC §11 row). Only if A–D leave a felt gap;
  possibly never, which is a fine outcome.

Then re-measure and decide whether **De4** still earns its keep.

## 9. Non-goals

- Shortening the production `connect_timeout`. It buys durability on flaky
  cellular; direct-delivery.md §5.1 already argued that trade and parked it.
  Nothing here changes that argument — the fixes above avoid *making* doomed
  dials rather than giving up on honest ones sooner.
- Reusing live connections for active conversations (direct-delivery.md's "next
  lever"). Orthogonal, and only helps paths that are already fast.
- Touching the app's 60 s foreground backstop poll: deliberate, documented in
  the rendezvous doc §8, and not on any measured path.
