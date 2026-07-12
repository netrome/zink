# Design: Direct Delivery (both-peers-online fast/private path)

The just-in-time design for the **online p2p path**: when a sender can reach a
recipient device directly, deliver the envelope peer-to-peer instead of parking
it in the recipient's relay mailbox. Downstream of
[live-delivery.md](./live-delivery.md) (the mailbox path this layers on top of)
and [mailbox-rendezvous-push.md](./mailbox-rendezvous-push.md). Shares the peer
ALPN introduced by **D0** (sync primitives).

Status: **draft for discussion.** ⚠️ marks open decisions.

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

## 2. The substrate: iroh discovery + the D0 peer ALPN

Two pieces already on the roadmap make this cheap:

- **iroh discovery resolves `key → address` for *online* nodes** (SPEC §3.6:
  "iroh discovery resolves key→address for online nodes, while `relays` is how a
  sender finds your inbox when you're offline"). So *reachability is the presence
  signal*: if `endpoint.connect(recipient_device_key)` succeeds within a short
  timeout, that device is online and dialable; if it doesn't, fall back to the
  mailbox. No separate presence protocol.
- **D0 stands up a peer-served ALPN** for `get` / `get-successors`
  (request/response pull, SPEC §5.2). Direct delivery adds one **push** op to
  that same ALPN — a client accepting an envelope addressed to it. The client
  becomes, in effect, its own mailbox when online. No new endpoint, no new
  connection type; a peer that speaks the D0 ALPN gains a `Deliver` op.

So direct delivery is a small additive slice *on top of D0*, not a parallel
stack. It should not be scheduled before D0 exists.

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

### 3.1 ⚠️ Open decision: skip-the-mailbox, or belt-and-suspenders?

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

---

## 6. Slicing & sequencing

- **Prerequisite: D0** (peer ALPN + get/get-successors). Direct delivery is the
  `Deliver` op on that ALPN plus the send-path decision — do not start it before
  D0.
- Then a single slice (**D5** in the plan): `Deliver` op + ack, send-path
  dial-then-fallback, outbox discharge, dedup test. CLI-testable headless:
  two clients online with no relay reachable → A `send`s → B receives directly;
  kill B → A `send`s → deposits to mailbox → B fetches on return.
- **Not on the social-features critical path.** D1–D4 (identity, multi-device,
  groups, web-of-trust) don't depend on this; it's a p2p/metadata optimization
  scheduled independently, once D0's peer ALPN exists.

## 7. Doc touchpoints when this lands

- SPEC §5.1/§5.3: note that fan-out delivers direct-when-online, mailbox-when-not
  (closes the intent/implementation gap named in §1).
- mailbox-wire-protocol.md / the D0 peer-ALPN doc: the `Deliver` op + ack.
- live-delivery.md §3: the nudge is now the *mailbox-path* live signal; direct
  delivery is the no-relay live path (cross-reference).
