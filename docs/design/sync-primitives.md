# Design: Sync Primitives (D0 — peer `get` / `get-successors`)

The just-in-time design for **D0**: content-addressed history sync over a
peer-served ALPN (SPEC §5.2). The immediate driver is a concrete correctness
hole — a device added to an existing conversation receives a non-genesis
message, has no genesis on disk, so `ClientState::load_dag` fails and it
**cannot thread a reply** (noted in B5; `state.rs:133` "a missing genesis is
unrecoverable"). More broadly this is the substrate for D2 backfill, D4 backlog
serving, and D5 direct delivery.

Downstream of [dag-store.md](./dag-store.md) and
[mailbox-wire-protocol.md](./mailbox-wire-protocol.md). Status: **draft for
discussion.** ⚠️ marks open decisions.

---

## 1. What the late joiner actually needs

To *reply*, a client needs to build the `ConversationDag`: the genesis (to
`new()` the DAG) and enough of the ancestor chain to compute `heads()`,
`next_logical()`, and its own `next_seq()`. That's the **DAG skeleton** — a set
of `MessageCore`s linked by `parents` back to the genesis.

Crucially, replying does **not** require *reading* old messages. A `MessageCore`
carries `body: ciphertext` and per-recipient key-wraps live in the envelope, but
a late joiner was not a recipient of messages sent before it joined, so it holds
no key-wrap for them and cannot decrypt them — and doesn't need to, to thread a
reply. This cleanly splits D0:

- **Serve the skeleton (this slice).** Hand over `MessageCore`s (or the full
  envelopes — ciphertext, safe to pass) so the requester can reconstruct the DAG
  and participate going forward. No new crypto.
- **Re-wrap to *read* history (deferred, D2).** Letting a new device/member
  *decrypt* old bodies needs a holder of each message's content-key to re-wrap
  it to the requester's key (SPEC §5.2 — "no cryptographic difference between
  your new device and a new member"). Out of scope here; noted so we don't
  conflate the two.

This matches SPEC §5.2's framing: the *mechanism* (`get`/`get-successors`) is one
thing; *willingness to re-wrap* is a separate, later gate.

---

## 2. The two primitives (SPEC §5.2)

- `get(id)` → the `MessageEnvelope` for `id`, if the peer holds it and chooses
  to serve it; else "not held".
- `get-successors(id)` → the ids of messages the peer holds whose `parents`
  include `id` (known children). Lets a requester pull *forward* (catch up on
  newer messages) as well as backward. **Kept in this slice even though our own
  backfill (§5) only uses `get` (resolved 2026-07-12):** it's half of SPEC
  §5.2's named primitive pair, cheap to serve, and defining + serving it now
  spares a later wire addition — other client implementations may drive forward
  sync before we do. Served + round-trip tested here; our client starts driving
  it in D0b. (Serving it scans conversations — fine at friend/family scale.)

**Serving is discretionary** (SPEC §5.2): a peer serves what it has and what it
*chooses* to. Backlog privacy is "don't serve the parents." **MVP policy —
resolved (2026-07-12): permissive, serve-what-you-hold.** Answer any
authenticated caller for any message you hold. It's the simplest correct start,
and restrictions are easy to add later: a contacts-only or per-conversation gate
is pure client policy, layered on without any protocol change. A policy knob,
never baked into the wire.

---

## 3. Wire protocol (additive, new ALPN)

A new ALPN `zink-sync/1` (constant `SYNC_ALPN` in `zink-protocol`), independent
of the mailbox ALPN. Same shape as the mailbox wire (versioned BORSH, one
FIN-framed request per client-opened bi-stream, `try_from_bytes` that never
panics on hostile input):

```
SyncRequest  { version, op: SyncOp }
SyncOp       = Get { id } | GetSuccessors { id }
SyncResponse { version, result: SyncResult }
SyncResult   = Envelope { envelope }    // Get hit
             | NotHeld                   // Get miss / declined
             | Successors { ids }        // GetSuccessors (possibly empty)
             | Error { code }            // malformed, unsupported version
```

`Get` returns the full `MessageEnvelope`, not a bare core. **Why the envelope,
resolved:** the sender's signature is over `core.id()` and lives in the
envelope, so serving the envelope lets the requester verify *authorship*
(`envelope.verify()`) as well as content-addressing — and it reuses the exact
store path incoming messages already take (`Client::remember`). The per-recipient
key-wraps it carries are useless to a non-recipient (can't unseal) but are pure
ciphertext and leak nothing beyond `core.recipients`, which already lists the
recipient set in the clear. This supersedes the earlier "bare core" idea and
closes the §6 authorship question. The requester still re-checks that a returned
envelope's id equals the id it asked for.

No version bump to any existing object: this is a brand-new ALPN and message
family. Old peers simply don't speak it (dialing the ALPN fails → the requester
falls back to "can't backfill yet", same as an offline peer).

---

## 4. The client becomes a server (the architectural change)

Today the client endpoint is **dial-only** (`net.rs` builds an `Endpoint` and
only `connect`s; no `Router`, no `accept`). Serving `zink-sync/1` requires the
client to run an **accepting router** on its own endpoint for the first time —
it is now also a server.

- Mirror the relay's `spawn_relay_router`: build a `Router` on the client
  endpoint that `accept`s `SYNC_ALPN` with a `SyncHandler` backed by the client's
  store. The `Client` owns the router for its lifetime.
- **Lifecycle:** desktop serves while the app runs; Android serves while the
  foreground service holds the process (the same FGS that already keeps the C4
  subscription loop alive — no new mechanism). CLI serves for the life of a
  `listen` command. When the process is down, the peer is simply unreachable and
  the requester falls back to the relay/other peers — best-effort, tenet 6.
- **Reachability = presence.** A reachable peer *is* an online, serving peer;
  an unreachable one falls back to the relay/other peers. No presence protocol.

**Addressing the peer — resolved (2026-07-12), robustness principle.** `backfill`
accepts *both* forms of peer address and picks by shape:

- **`<endpoint-id>@<ip:port>` — the primary, shipped path.** Deterministic,
  needs no discovery infrastructure, and is what the CLI and tests use. This is
  what we expect users to paste/exchange for now.
- **A bare `<endpoint-id>` — resolved via iroh discovery** (SPEC §3.6: discovery
  resolves key→address for online nodes). Liberal input for other clients and
  for later UX, *but not free*: key-only resolution needs discovery **enabled on
  the client endpoint**, which `presets::Minimal` does not do today. So this is a
  real endpoint capability, not just a parse branch — it lights up when we wire
  discovery (alongside D0b), and until then a bare key simply fails to resolve
  (surfaced, never a silent hang). Accepting the form now keeps the API stable;
  we don't half-implement the resolution and pretend it works.

**Discovery feasibility (investigated 2026-07-12).** iroh 1.0 renamed discovery
to `address_lookup`, added à la carte via `Builder::address_lookup(...)`.
Enabling it is ~2 lines — and crucially `presets::N0DisableRelay` gives n0's
DNS/pkarr key→address discovery *with n0's relay fleet turned off*, so discovery
is separable from the relays we deliberately don't want (the original reason we
picked `Minimal`). In-crate mechanisms (`DnsAddressLookup`, `PkarrPublisher`/
`PkarrResolver`) need no new dep but lean on n0's public DNS/pkarr server; mDNS
(`iroh-mdns-address-lookup`, LAN-only) and mainline-DHT
(`iroh-mainline-address-lookup`, decentralized) are separate crates. **The catch
is not discovery but reachability:** finding a peer's address ≠ being able to
connect to it. A phone on cellular/CGNAT usually isn't dialable at a direct
`ip:port` even once resolved — that needs holepunching via an iroh *transport*
relay, which we don't run (our mailbox works cross-network only because both
clients dial its stable public address *outbound*). So key-only discovery is a
cheap add for *reachable* peers (same-LAN, or one side publicly reachable); full
cross-NAT peer sync is a larger, later piece (an iroh relay, or routing sync
through the existing rendezvous). This is why D0a ships the dial string and the
ContactRecord `relays` stay the rendezvous anchor.

---

## 5. The backfill loop (fixing the hole)

Requester side, `Client::backfill(conversation, from)` where `from` is a peer
address (§4 — a dial string now, a bare key once discovery is wired):

```
dial `from` on SYNC_ALPN (short timeout; on failure, give up — caller falls back)
frontier = missing ancestors we need (referenced parents we don't yet hold,
           computed straight from stored envelopes — no valid DAG required)
while frontier not empty and not yet at genesis:
    for id in frontier:
        Get(id) → envelope?  → verify + id-match, store, its parents we still lack
                               rejoin the frontier next round
    (bounded: cap total fetched per call; a peer that never yields the genesis
     is treated as a declining/hostile peer and the loop ends — honesty over a
     fabricated root)
```

Backward-fill via `Get(parent)` reaches the genesis; `get-successors` is the
forward complement (catch up on newer messages) and is served + round-trip
tested in this slice but not yet driven by an auto-sync loop.

**Who to ask:** for the reply hole, the natural target is the `sender` of the
message that arrived (they were in the conversation and likely hold its
history). First slice: an explicit `from` peer (CLI-testable). Auto-triggering
backfill on receipt of an orphan message — and choosing the peer from the
message's `sender`/`recipients` — is a small follow-up once serve+fetch works.

---

## 6. Safety / non-panic (tenet: never trust, verify)

- Every received envelope is `verify()`d (sender signature over the recomputed
  id) *and* checked that its id equals the id requested; either failure drops it.
  A served peer is trusted no more than a relay.
- **Signature coverage — resolved by serving the envelope (§3).** The `sender`
  signature is over `core.id()` and travels in the envelope, so a served history
  message carries full authorship proof; a lying peer cannot forge a core the
  real sender never signed. (The earlier open question assumed we'd serve a bare,
  unsigned core; serving the envelope removes it.)
- `try_from_bytes` returns errors (never panics) on malformed/oversized input,
  same discipline as the mailbox wire. A `MAX_SYNC_*` byte cap bounds responses.

---

## 7. Slicing

- **D0a · Serve + backward-fill (this slice).** `SYNC_ALPN` + wire types in
  `zink-protocol`; client accepting router + `SyncHandler` serving envelopes;
  `Client::backfill(conversation, from)`; a CLI hook and a headless e2e test:
  A builds a conversation of N messages, B is handed only the latest, B
  backfills from A to the genesis, B `load_dag` succeeds and B can thread a
  reply. Non-goals: re-wrap-to-read (D2), auto-backfill-on-orphan wiring,
  key-only dialing (needs discovery enabled — dial string only for now),
  forward auto-sync.
- **D0b · Auto-sync wiring.** Trigger backfill on an orphan receipt; pick the
  peer from `sender`; forward catch-up via `get-successors`; wire iroh discovery
  so a bare `<endpoint-id>` resolves (the key-only address form of §4). Small,
  once D0a is proven.

## 8. Doc touchpoints when this lands

- SPEC §5.2: the shipped wire shape (serve envelopes; re-wrap still deferred).
- mailbox-rendezvous-push.md §3 already notes "forward now"; add the peer-ALPN
  sync path as the pull complement.
- client-core.md: the client now runs an accepting router (serve side) + the
  `backfill` API.
- [direct-delivery.md](./direct-delivery.md): D5 adds a `Deliver` op to *this*
  ALPN — cross-reference once D0a lands.
