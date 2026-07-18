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
  it in D0d. (Serving it scans conversations — fine at friend/family scale.)

**Serving is discretionary** (SPEC §5.2): a peer serves what it has and what it
*chooses* to. Backlog privacy is "don't serve the parents." **MVP policy —
resolved (2026-07-12, tightened 2026-07-18): permissive serve-what-you-hold for
D0a only; a contacts-only gate lands as its own slice (D0c) right after D0b.**
Permissive was the simplest correct start while reaching a peer required knowing
its explicit `ip:port`; D0b's dial-by-key widens reachability to anyone holding
the key + relay URL, so that's the moment the default flips: `SyncHandler`
answers `NotHeld` to callers not in the contact store (indistinguishable from
not-holding — declining and not-having look the same). Pure client policy,
layered on without any protocol change — a policy knob, never baked into the
wire.

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

### 4.1 Addressing & reachability — resolved (2026-07-12)

Two shapes, and the settled model for reaching peers is **relay-coordinated, not
a discovery service**:

- **D0a (shipped): explicit dial string `<endpoint-id>@<ip:port>`.** No extra
  infrastructure, works when the peer has a directly reachable address (same-LAN,
  or one side public). What the CLI and tests use; the zero-infra path.
- **D0b (foundation): dial a peer by key, relay-coordinated.** The cross-NAT
  answer — and it needs **no DNS/pkarr/mDNS discovery service**:
  - The **`zink-relay` binary also runs the iroh relay server** — one service,
    iroh relaying + mailbox/blobs (logically separate, operationally one binary).
    iroh relay TLS is *optional*: `tls: None` runs it over plain HTTP, so no
    domain/cert for native clients (a browser client would later want HTTPS) —
    consistent with the "no TLS/domain" stance from C0.
  - **Clients home to their own relay(s)** (`RelayMode::Custom`) — still
    **multi-relay**: a device advertises its relays in its `ContactRecord` just
    as the mailbox already does. **Never a single shared relay** — spinning up
    more relays must work seamlessly (SPEC §3.6; `fanout::distinct_relays` and
    the per-relay `recv` loop already assume many).
  - To reach peer B, dial `EndpointAddr::new(B_key).with_relay_url(B_relay)`,
    where `B_relay` comes from **B's ContactRecord** (which A already holds as a
    contact). iroh routes initial signaling via B's relay, then **holepunches to
    a direct P2P path**, **falling back to relaying** the (encrypted) QUIC
    through the relay if the punch fails. Two peers on *different* relays connect
    fine — the callee's relay is the rendezvous.
  - **The one record change:** a relay entry in `ContactRecord` must carry the
    iroh `RelayUrl` alongside the mailbox dial string — **as one structured
    relay entry, not parallel vecs** (both address the same relay service; an
    index-paired association would drift). **No version bump** (resolved
    2026-07-18): nothing is deployed, so the field is added in-place at
    version 1 — existing dev-stage contacts/QRs stop parsing and are simply
    re-exchanged. (The record's earlier "relay URLs are a version bump" flag
    predates this; per-type versioning proper is parked in the build plan
    under *before first external deployment*.) Shared through the same
    QR/record flow (relays need not be invisible — users/clients may know
    and exchange them).

**Why relay-coordinated beats a discovery service.** A DNS/pkarr/mDNS lookup only
resolves key→address; it does *not* make a NATed peer connectable — that still
needs a relay to coordinate the holepunch. Since we already run a relay and
already ship each peer's relay set in its record, the relay is *both* the
rendezvous and the punch coordinator, and key→address falls out of the record for
free. (iroh's `address_lookup` / `N0DisableRelay` remain available if we ever
want key dialing without the relay-in-record, but they're not the plan.)

**Metadata** is no worse than today — the mailbox already sees who-talks-to-whom —
and a successful punch means the relay sees *less* (handshake only, then direct).

---

## 5. The backfill loop (fixing the hole)

Requester side, `Client::backfill(conversation, from)` where `from` is a peer
address (§4.1 — an explicit dial string now; dialed by key via the peer's
`RelayUrl` once D0b's relay-coordinated connectivity lands):

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

- **D0a · Serve + backward-fill (done).** `SYNC_ALPN` + wire types in
  `zink-protocol`; client accepting router + `SyncHandler` serving envelopes;
  `Client::backfill(conversation, from)`; a CLI hook and a headless e2e test:
  A builds a conversation of N messages, B is handed only the latest, B
  backfills from A to the genesis, B `load_dag` succeeds and B can thread a
  reply. Non-goals: re-wrap-to-read (D2), auto-backfill-on-orphan wiring,
  dial-by-key (D0b), forward auto-sync.
- **D0b · Relay-coordinated peer connectivity (§4.1).** iroh relay server in the
  `zink-relay` binary (`tls: None`); clients home to their own relays
  (`RelayMode::Custom`, multi-relay); `RelayUrl` added to `ContactRecord`
  (in-place at version 1, paired with the mailbox dial string — §4.1); dial a
  peer by key via their record's relay, holepunching to direct with relay
  fallback. The foundation for D0c/D0d, D1's `who-is-this`, and D5.
  *Done when:* two NAT'd clients on different relays connect and one backfills
  from the other by key alone — headless e2e for by-key dial via relay
  rendezvous; the cross-NAT holepunch itself is a documented manual run.
- **D0c · Serving gate (contacts-only, §2).** Right after D0b (independent code,
  so parallel is fine): `SyncHandler` answers `NotHeld` to callers not in the
  contact store. Client policy only; no wire change.
- **D0d · Auto-sync wiring.** Trigger backfill on an orphan receipt; pick the
  peer from `sender` (dialed by key via D0b); forward catch-up via
  `get-successors`. Small, once D0a + D0b are proven.

## 8. Doc touchpoints when this lands

- SPEC §5.2: the shipped wire shape (serve envelopes; re-wrap still deferred).
- mailbox-rendezvous-push.md §3 already notes "forward now"; add the peer-ALPN
  sync path as the pull complement.
- client-core.md: the client now runs an accepting router (serve side) + the
  `backfill` API.
- [direct-delivery.md](./direct-delivery.md): D5 adds a `Deliver` op to *this*
  ALPN — cross-reference once D0a lands.
