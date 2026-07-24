# Design: Direct Delivery (both-peers-online fast/private path)

The just-in-time design for the **online p2p path**: when a sender can reach a
recipient device directly, deliver the envelope peer-to-peer instead of parking
it in the recipient's relay mailbox. Downstream of
[live-delivery.md](./live-delivery.md) (the mailbox path this layers on top of)
and [mailbox-rendezvous-push.md](./mailbox-rendezvous-push.md). Shares the peer
ALPN introduced by **D0a** (sync primitives) and the peer connectivity from
**D0b**.

Status: **built (D5, 2026-07-24).** The one open decision (§3.1) resolved as
recommended: skip-on-direct-success. As-built deltas are marked
**As built** below.

---

## 1. Why — and why it isn't already done

Today *all* message delivery goes through the mailbox: `send` deposits every
envelope to each recipient's relay, and the recipient fetches it. "Online" is
not a different path — it's the same deposit, drained in real time via the C4
nudge (live-delivery.md §3). That unification was a deliberate simplicity win
and it's why delivery feels instant now.

But it means the **untrusted relay is on the path of every message**, even when
both peers are simultaneously online and could talk directly. That costs us two
things the design philosophy actually cares about:

- **Metadata.** The relay sees who deposits for whom, and when — the social
  graph in timing form. Ciphertext-only keeps *content* safe (tenet: relays
  untrusted), but the metadata leak is real and avoidable when peers are online.
- **Relay-independence.** Two peers who can reach each other shouldn't need a
  working relay to talk. P2p-first (tenet 1) wants the relay to be a *fallback*,
  not a dependency.

It is **not** primarily about latency: the nudge already delivers in seconds, so
speed is a minor bonus, not the motivation. SPEC §5.1 already frames fan-out as
running "over direct iroh connections" and §5.3 calls relay retention "a
reluctant, TTL-bounded fallback" — so this closes a gap between the spec's stated
intent and the shipped implementation, rather than adding a new capability.

**Non-goals.** No presence/online-status UI (structural, not a feature); no
gossip plane (SPEC §5.1 — fan-out already covers friend/family scale); no change
to the offline path (the mailbox stays exactly as-is); no NAT-traversal work
beyond what iroh already gives us.

---

## 2. The substrate: D0b connectivity + the D0a peer ALPN

Two pieces already on the roadmap make this cheap:

- **D0b relay-coordinated connectivity** (sync-primitives.md §4.1) makes a peer
  reachable by key: dial `EndpointAddr::new(recipient_key).with_relay_url(their
  relay)` (from the recipient's `ContactRecord`), and iroh holepunches to a
  direct path or relays as fallback. So *reachability is the presence signal*: if
  the connect succeeds within a short timeout the device is online and dialable;
  if not, fall back to the mailbox. No separate presence protocol, and — key
  point — this is relay-coordinated, **not** a DNS/pkarr discovery service.
- **D0a stands up a peer-served ALPN** for `get` / `get-successors`
  (request/response pull, SPEC §5.2). Direct delivery adds one **push** op to
  that same ALPN — a client accepting an envelope addressed to it. The client
  becomes, in effect, its own mailbox when online. No new endpoint, no new
  connection type; a peer that speaks the sync ALPN gains a `Deliver` op.

So direct delivery is a small additive slice *on top of D0a + D0b*, not a
parallel stack. It should not be scheduled before that connectivity exists.

---

## 3. The delivery decision (the one real design choice)

Per recipient **device**, at send time, the sender chooses direct vs mailbox.
The safe, offline-correct shape:

```
for each recipient device:
    try: dial device directly (peer ALPN), short timeout
         → push envelope, await application-level ack (durably stored)
         → delivered directly; do NOT deposit to that device's relay ⚠️
    on any failure (not online / not dialable / no ack in time):
         → deposit to the device's relay mailbox, exactly as today
```

The **application-level ack is load-bearing.** A direct push may be accepted at
the QUIC layer while the recipient app never durably stores it (crash, disk
error). Skipping the mailbox on a transport-only success would lose the message
with no fallback copy — a silent delivery hole, the exact failure the C4 outbox
exists to prevent. So the recipient must confirm a durable store (mirroring the
mailbox's `Deposited` result) *before* the sender skips the mailbox. No ack in
time ⇒ treat as undelivered ⇒ mailbox deposit. This keeps "honesty over false
delivery" (tenet 6) intact.

**Dedup is free.** A message content-addressed by BLAKE3 id already dedups
across relays (rendezvous §receiver-side). A message that arrives both directly
*and* (racily) via a mailbox fetch is the same free merge — no new bookkeeping.

**Outbox integration.** The C4 outbox ledger (live-delivery.md §2) is the
natural home: an entry is "owed" until *some* path (direct or relay) confirms
delivery for that recipient. Direct delivery just adds a second way to discharge
an entry. Store-before-network and the give-up window are unchanged.

**As built — the granularity trap.** An outbox entry is per **(message,
relay)**, but a direct ack is per **recipient device**, and one relay can host
several recipients (a single deposit fans out to all of them). So the skip rule
is: skip a relay's deposit iff **every** recipient it hosts acked directly —
*all*, not *any*. Skipping on "any" would silently drop the un-acked members'
only copy in a group send; that hazard is pinned by a regression test
(`send__should_not_skip_a_relay_hosting_a_recipient_that_did_not_ack`).

**As built — blobs still ride the relay.** A recipient fetches blob bytes from
its own relay's cache (C3a), so an image message needs the push regardless;
pushing blobs while skipping the deposit would buy little (the relay sees the
sender either way). A message **with blobs therefore takes the pre-D5 path
exactly** — direct delivery is still attempted (so it lands instantly, and
lands *at all* when the relay is down), but no deposit is skipped. Text — the
dominant case — gets the full win. Closing the gap needs peer blob transfer:
a later slice, not a protocol question.

**As built — who may push.** `Deliver` is gated like history (D0c): contacts,
recognized own devices, self. A stranger's push is declined (`NotHeld`) and
falls back to their mailbox deposit, where the relay's caps (C0) and the parked
quarantine view are the policy for unknown senders — direct-to-disk from an
unknown key would route around both. One-way adds are unaffected: the mailbox
path is exactly what they already use.

### 3.1 Resolved (2026-07-24): skip-the-mailbox on a durable ack

Two variants, and they trade metadata-minimization against robustness:

- **Skip-on-direct-success (recommended target).** Direct ack ⇒ no deposit. The
  relay sees *nothing* for online conversations — the real philosophy win. Risk:
  relies on the direct ack being as trustworthy as a mailbox `Deposited`, and on
  the recipient not going offline in the gap between "acked" and "durably useful"
  (covered by the ack meaning *durably stored*, not *received*).
- **Always-deposit + opportunistic-direct (fallback if the above proves flaky).**
  Always deposit to the mailbox; *also* push direct for speed. Simple and
  maximally robust, but the relay still sees all metadata — so it buys almost
  nothing over the existing nudge and largely defeats the point. Only worth it as
  a stepping stone if skip-on-success shows delivery gaps in practice.

**Recommendation:** ship skip-on-direct-success, because it's the only variant
that delivers the metadata/independence goals; keep always-deposit in the back
pocket as a one-line policy fallback if real-world testing shows the direct ack
can't be trusted. Resolve after the first on-device test.

**Resolved: skip-on-direct-success shipped.** The ack is only sent after the
recipient's durable store returns, so it carries the same weight as a mailbox
`Deposited`. Verified end-to-end at the disk level: a direct send leaves the
recipient's relay mailbox with **zero** items, the same send with the recipient
offline leaves exactly one. Always-deposit remains a one-line change
(`discharged()` → `false`) if field use ever shows the ack can't be trusted.

---

## 4. Receiver side

- The peer ALPN handler (D0) gains a `Deliver { envelope }` op alongside
  `get`/`get-successors`. On receipt: verify the envelope (same checks the
  mailbox-fetch path runs — never trust a dialer more than a relay, tenet:
  verify before trusting), store it, hand it to the edge (notify + re-render,
  same as a nudge drain), and return an ack **only after the durable store
  succeeds**.
- **No new trust.** A direct dialer is authenticated (its connection key) but
  *not* trusted for content any more than a relay is — the envelope's own
  signature + key-commit are the gate, unchanged. A hostile dialer can at worst
  deliver something we'd have accepted from the mailbox anyway, or spam us (same
  as a hostile deposit; relay/peer rate limits are policy).
- **Discretion.** A client MAY decline direct connections (e.g. only accept from
  known contacts) — serving discretion, same as `who-is-this`/`get`. Declining
  just falls the sender back to the mailbox.
- **As built — the checks, in order:** the caller passes the D0c gate; the
  envelope carries a version we speak; `verify()` passes (signature over the
  recomputed id); our key is in `core.recipients`; the body opens for us. Only
  then `remember()`, and only after that an ack. The recipients check is the one
  the pull ops don't need: a contact we serve history to must not be able to
  write *arbitrary* conversations into our store — not even our own relay can do
  that, since it indexes deposits per recipient key. The body-opens check
  mirrors the mailbox drain, which likewise stores only what it can read.
- **As built — no `Client` in the router.** The handler holds state and a key,
  not a client, so it stores + acks inline and hands the batch to an edge sink
  (`Client::on_direct_delivery`). The edge then calls `Client::after_direct` for
  the post-drain healing (auto-sync / scoped who-is / re-wrap) — the same seam
  `recv` and `subscribe` run, deliberately at the edge because the lib spawns no
  tasks of its own. It matters here: a direct arrival produces no nudge, so
  without that call an orphaned conversation would wait for a drain that, with
  the relay down, may never come.

---

## 5. Complexity & cost

Moderate, and bounded — most of it rides on D0:

| Piece | Cost |
|---|---|
| Peer ALPN + connection handling | **Comes with D0** (get/get-successors). |
| `Deliver` op + ack | Small — mirrors the mailbox `Deposit`/`Deposited` pair. |
| Send-path branching (dial-then-fallback) | Moderate — parallel dial with timeout, per device. |
| Outbox integration | Small — one more way to discharge an entry. |
| Dedup | **Free** — content-addressing. |
| NAT traversal | **Free** — iroh; note peer↔peer holepunch fails more often than peer↔relay, which is exactly why the mailbox fallback stays. |

The subtle cost is **connection management**: dialing every recipient device on
every send adds connection churn and a per-send timeout budget when a device is
*not* reachable. Mitigations (pick during implementation, don't pre-build):
attempt the direct dial *in parallel* with preparing the mailbox deposit so a
dial timeout never serially delays the fallback; and/or reuse an already-open
direct connection when the conversation is active. Keep it simple first — a
short dial timeout with mailbox fallback is correct if unoptimized.

**As built:** kept simple, as recommended. Recipients are dialed **concurrently**
(`n0_future::join_all`, the De3 shape — one offline recipient never serializes the
rest) with the deadline capped at `min(connect_timeout, 3 s)`: a speculative dial
must not inherit a send's full patience, because the mailbox behind it is fast
and reliable. The remaining cost is honest and known: a send to an offline
recipient pays up to that cap before falling back. Worth revisiting if it's felt
in the field — reusing a live connection for an active conversation is the next
lever, and iroh already caches the path, so a warm peer is cheap.

---

## 6. Slicing & sequencing

- **Prerequisites: D0a** (peer ALPN + get/get-successors) **and D0b**
  (relay-coordinated connectivity — how a peer is reached by key). Direct
  delivery is the `Deliver` op on that ALPN plus the send-path decision — do not
  start it before both exist.
- Then a single slice (**D5** in the plan): `Deliver` op + ack, send-path
  dial-then-fallback, outbox discharge, dedup test. CLI-testable headless:
  two clients online with no relay reachable → A `send`s → B receives directly;
  kill B → A `send`s → deposits to mailbox → B fetches on return.
- **Not on the social-features critical path.** D1–D4 (identity, multi-device,
  groups, web-of-trust) don't depend on this; it's a p2p/metadata optimization
  scheduled independently, once D0a + D0b exist.

**As built (one slice, 2026-07-24).** Acceptance ran **in-process** in the
client crate rather than through CLI subprocesses (the De4 lesson: the flows are
hundreds-of-ms fast; the CLI harness is what's slow). Five tests, ~2 s total:
direct delivery with the mailbox unreachable; **both relays shut down
mid-conversation and the next send still lands directly** — the "restart the
relay without disrupting messages" case; the offline-peer fallback keeping C4a's
"queued, not lost" semantics; the shared-relay all-not-any rule; and two decline
cases (stranger, not-addressed-to-us). Plus a manual CLI run against a real
relay, checked at the disk level (mailbox item counts).

### 6.1 What this does *not* buy: cold reachability

Rendezvous still needs a relay. Dial-by-key resolves a peer as *key + their
relay URL*, so two peers who have never connected cannot find each other with
every relay down — there is no address hint, no LAN discovery, nothing else in
the record. What D5 buys is precisely: **the relay is not on the message path**
(no deposit, no metadata, no dependency once a path exists) — not "the relay can
be off before you start". A conversation that is *already* up survives the relay
going away; a cold start does not. Worth stating plainly because the difference
is easy to over-claim.

The philosophy-consistent way to improve cold reachability is *more relays,
socially hosted* — relays are per-person infrastructure named in records, and a
friend can host yours — not peers gossiping addresses. Address hints from mutual
contacts would be a small `WhoIs`-shaped extension, but they only help on a LAN
or an easy NAT (a stale address can't coordinate a holepunch), and they'd turn
your contacts into a log of where you connect from. Peer-as-signaling-relay is
the gossip plane SPEC §5.1 rules out of the core. Noted, not scheduled.

## 7. Doc touchpoints when this lands — done

- SPEC §5.1/§5.3: note that fan-out delivers direct-when-online, mailbox-when-not
  (closes the intent/implementation gap named in §1). ✅
- mailbox-wire-protocol.md / the D0 peer-ALPN doc: the `Deliver` op + ack. ✅
  (sync-primitives.md §3 — the op family lives on that ALPN.)
- live-delivery.md §3: the nudge is now the *mailbox-path* live signal; direct
  delivery is the no-relay live path (cross-reference). ✅
