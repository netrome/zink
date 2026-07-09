# zink — MVP Specification (draft)

A small, p2p-first chat protocol and app built on [iroh 1.0](https://www.iroh.computer/blog/v1),
for me and my close friends. Specifies the **MVP feature set**, the **protocol
building blocks**, and the **high-level system components**. It is a protocol first,
with clients and relays as independent implementations.

Read [DESIGN-PHILOSOPHY.md](./DESIGN-PHILOSOPHY.md) first — this document is
downstream of it.

Status: **draft, converged on core model.**

---

## 1. The core model in one paragraph

A person is a **fluid set of keys**, linked by signed **attestations** ("this key is
also me" / "this key is Alice"). There is no permanent identity key and no global
account. Each client maintains its own belief about which keys belong to whom, like a
contact list, and can revise it manually at any time. A **conversation** is a
content-addressed **DAG of messages** rooted at a genesis message; there is no
separate notion of a "group" — a message is simply **fanned out** to the set of keys
the sender chose, its body encrypted once and the body-key sealed per recipient.
Membership, names, and grouping are local interpretations layered on top of keys and
hashes. Offline delivery and notifications go through untrusted, interchangeable
**relays/mailboxes** that only ever see ciphertext.

Everything below elaborates that paragraph.

---

## 2. Terminology

| Term | Meaning |
|---|---|
| **Key** | An iroh keypair = one device / one `EndpointId`. The only cryptographic identifier. Ed25519. |
| **Person** | A *local* clustering of keys, via attestations you trust. Not a protocol object. |
| **Petname** | Your local, private name for a person. Never sent on the wire. |
| **Attestation** | A signed, gossiped, *advisory* claim linking keys or naming them. |
| **Conversation** | A DAG of messages rooted at a genesis message; identified by the genesis hash. |
| **Message** | A content-addressed (BLAKE3) signed object; its hash is its id. |
| **Content-key** | Random per-message symmetric key; encrypts the body once. Sealed per recipient. |
| **Recipients** | The keys a sender chose to fan a message out to. Recorded, signed, advisory. |
| **Mailbox** | Relay-hosted store of E2E-encrypted messages/blobs for an offline key. |

---

## 3. Identity

### 3.1 Model

- A person = a set of keys. **No** mandatory long-lived "identity key."
- Keys are linked by **attestations** you and your contacts sign and gossip.
- "Who is Alice" is **your local belief**, not a global fact. Manual override always
  wins over any received attestation.

### 3.2 Attestations (one primitive, several uses)

A signed statement, gossiped to contacts, treated as *advisory input*:

```
Attestation {
  subject:  key
  claim:    same-person-as <key> | name <label> | avatar <blob-ref> | negative | revoke <attestation-hash>
  attester: key
  version:  monotonic (per attester+subject)
  sig
}
```

Uses, all the same primitive:

- **Add my own device** — sign `same-person-as` linking a new key to an existing key of mine ("this key is also me").
- **Vouch for a contact** — sign that some key is the person I call Alice.
- **Repudiate** — a `negative` claim ("I don't recognize this key") or a `revoke`.
- **Profiles / third-party profiles** — `name` / `avatar` claims, self-asserted or
  about others. This is what makes "everyone can set profile pictures for other
  people" work: clients aggregate contacts' claims, weighted by trust, and show
  *"your friends call them …"*.

On the wire, attestations link **key → key**; the human `label` is a local petname
overlay, never required to be shared.

### 3.3 Name resolution = the addressing layer

Sending "to Alice" means: resolve petname → the set of keys you currently believe are
Alice (your attestations + trusted contacts' attestations + manual overrides), then
fan out to those keys. A repudiated key drops out of the set. **Identity resolution
and message addressing are the same step.**

### 3.4 Recovery is social, not cryptographic

Losing a key forks your identity. You and an attacker can both claim to be you; the
protocol does not arbitrate — and deliberately offers **no cryptographic recovery
anchor** (it would add complexity and contradict the philosophy). You call a friend
out-of-band; they re-attest which key is really you, mark the other `negative`, and
gossip it — their clients stop routing your messages to the bad key.

---

## 4. Conversations & messages

### 4.1 Conversation = genesis-rooted message DAG

- Identified by the hash of its **genesis message**. Distinct geneses are distinct
  conversations automatically (no separate group id / nonce needed).
- A **message** is content-addressed; its BLAKE3 hash is its id.
- Every message points to its **parents** — the sender's current known *heads*
  (messages with no known successor). This forms the causal DAG.

The signed, hashed **core** (identical bytes for every recipient → one shared id):

```
MessageCore {                          // id = BLAKE3(borsh(MessageCore))
  version
  conversation:  genesis hash
  parents:       [message hash]        // current heads at send time
  recipients:    [key]                 // who this was fanned out to (advisory, but signed)
  sender:        key
  seq:           u64                   // per-sender sequence number (completeness / gap detection)
  logical:       u64                   // Lamport clock = 1 + max(parents.logical); ordering
  timestamp:     wall-clock hint       // display only, never trusted for ordering
  body:          ciphertext            // encrypted ONCE with a random content-key
  blob-refs:     [ {hash, kind: thumb|full} ]   // §7
}
```

Transport envelope (**not** part of the id; per-recipient parts live here):

```
MessageEnvelope {
  core:      MessageCore
  sig:       sender's signature over the id
  key-wraps: [ {recipient: key, sealed: content-key sealed to recipient} ]   // + blob content-keys
}
```

Because the per-recipient `key-wraps` are outside the hashed core, **everyone derives
the same message id**, so `parents` and the DAG hold across all recipients.

### 4.2 There is no "group" — only fan-out

A 1:1 chat and a group chat are the same thing: a message fanned out to a set of
keys. "Membership" is not a protocol object — it is each client's local
reconstruction from the `recipients` lists it has seen, plus its own key→person
clustering. `recipients` is a signed, durable record of who the sender actually sent
to (hence who could see it), but it **enforces nothing**.

### 4.3 Ordering (tenet 7)

Two small integers, different jobs, plus the DAG:

- **`logical` (Lamport):** lets a client linearize *any subset* of messages
  consistently with causality by sorting on `(logical, tiebreak-by-id)` — works even
  with a partial view, without walking the whole DAG. This is the linear **default view**.
- **`seq` (per-sender):** completeness / gap detection. `seq` gaps, and a presence
  beacon advertising a sender's latest `seq`, tell you when you're missing a sender's
  *newest* messages — which dangling parent pointers alone cannot reveal.
- **`parents`:** source of truth for **concurrency**. Multiple heads = "these
  messages crossed in flight," real data that advanced clients *may* choose to show.

Wall-clock `timestamp` is a display hint only.

### 4.4 Membership is local (tenet 3)

Anyone can send to anyone; you cannot prevent Bob from including Charlie. There is no
membership consensus and no enforced removal. "Add/remove," "who's in this chat," and
"should the newcomer see the backlog" are all resolved by **local discretion**: who
you send to, and what history you choose to serve (§5.2).

---

## 5. Delivery & sync

### 5.1 Two transport planes

- **Direct plane** (reliable, dedicated ALPN): message delivery, blob transfer,
  device pairing, history sync. Direct iroh connections + acks; relay-routed for
  browsers.
- **Gossip plane** (best-effort, `iroh-gossip`): attestations, presence beacons, and
  the "who is this?" discovery query.

Gossip is **not** reliable delivery. Anything that must arrive uses the direct plane
+ mailbox.

### 5.2 History sync = one primitive, best-effort (tenet 6)

A single content-addressed mechanism, used identically for **a new member requesting
backlog** and **your own new device catching up** — device sync *is* peer sync:

- `get(hash)` — fetch a message/blob by hash (bodies are identical for all recipients).
- `get-successors(hash)` — fetch known children of a message.
- **Gap signals:** a referenced parent you don't have, or a `seq` gap.
- **Serving is discretionary** — a peer serves what it has and what it *chooses* to.
  This is also how backlog privacy works (don't want the newcomer to see old
  messages? don't serve the parents).

**There is no cryptographic difference between your new device and a new member** —
both need *someone holding a message's content-key* to **re-wrap** it to their key
(cheap — no re-encryption of the body). The content-key is symmetric and shared, so
*any* recipient of a message can re-wrap it for anyone; nothing privileges your own
devices. In practice your own devices re-wrap for each other (trust + they hold your
full history), but a friend who was in the conversation could re-wrap for your new
device just as well. The only gate is **willingness to re-wrap**, i.e. the same
serving discretion as everything else — no shared "family key" exists. (This is not a
new leak: a recipient could always re-send plaintext; re-wrapping is just the
efficient form of the same inherent capability.) Backfill is never guaranteed
complete, and the DAG makes incompleteness visible.

### 5.3 Offline delivery & notifications (foundational, not a late feature)

- **Mailbox:** when a recipient key is offline, its envelope is parked in a
  relay-hosted mailbox until the device reconnects. Untrusted for content.
- **Web Push gateway:** on deposit, the relay sends a content-free push ("you have
  messages"); the device wakes, authenticates, pulls, decrypts. Requires VAPID +
  browser push services — an unavoidable non-p2p dependency for a PWA. Acknowledged.
- **Retention bounds recoverability.** Peer-to-peer backfill is the *primary* path;
  relay retention is a reluctant, TTL-bounded fallback for offline delivery, kept as
  minimal as possible.

---

## 6. Encryption (envelope / hybrid)

- **Body, encrypted once:** a random **content-key** (symmetric AEAD) encrypts the
  message body. The body ciphertext is identical for all recipients → stable content
  hash → working DAG. Same scheme for blobs (§7).
- **Content-key, sealed per recipient:** static **sealed-box** to each recipient key.
  iroh keys are Ed25519; sealing uses the standard **Ed25519→X25519** conversion, and
  the message id is signed with the Ed25519 key. One key per device, two uses.
- **Conscious tradeoff:** static sealing means **no forward secrecy** (harvest-now/
  decrypt-later exposure, as with any EC scheme). Accepted for a friends app that
  retains history anyway; still categorically better than a third-party service that
  may hold your plaintext or a backdoor. Ratcheting can replace the sealing layer
  later without touching the DAG or the envelope shape.
- **Relays see ciphertext + metadata only** (envelope sizes, recipient keys, timing).

---

## 7. Images & blobs

- **iroh-blobs** (BLAKE3, content-addressed).
- Encrypt each image **once** with a random content-key; address by ciphertext hash.
  Fan out only the small **sealed content-key** per recipient (via the envelope's
  `key-wraps`) — media stays O(1) in storage, O(recipients) in a few hundred bytes.
- Send **two blobs**: a small encrypted **thumbnail** (instant preview) + **full-res**.
- Relays may **cache encrypted blobs** (TTL / size cap) so a recipient can fetch even
  after the sender goes offline.

---

## 8. System components

```
┌─────────────────┐    gossip + direct (relay-routed — browsers can't hole-punch)   ┌─────────────────┐
│  PWA client      │◀────────────────────────────────────────────────────────────────▶│  PWA client      │
│  (WASM + iroh)   │                                                                 │  (WASM + iroh)   │
└───────┬─────────┘                                                                 └─────────────────┘
        │  mailbox sync · push registration · blob fetch
        ▼
┌──────────────────────────────────────────────────┐
│  Relay + Mailbox + Push gateway (small binary)      │  ← untrusted; ciphertext + metadata only
│   · iroh relay (connectivity / NAT traversal)       │     interchangeable; anyone can run one
│   · encrypted mailbox (offline delivery)            │
│   · encrypted blob cache (TTL)                      │
│   · Web Push (VAPID) sender                         │
└──────────────────────────────────────────────────┘
```

- **PWA client (WASM):** the only client for the MVP. iroh built with
  `default-features = false`; **always relay-routed** (browsers can't hole-punch).
  Holds keys, attestations, contacts, crypto, the DAG store, and UI.
- **Relay / mailbox / push server:** small Rust binary; untrusted for content;
  minimal role (help peers connect + retain messages). **Anyone can run one**, and
  they are interchangeable — a user configures which relay(s) they use. I'll run one
  or two to start.

---

## 9. Protocol building blocks (summary)

1. **Message** — content-addressed, signed core + per-recipient envelope; carries
   `conversation, parents, recipients, sender, seq, logical, timestamp, body, blob-refs`.
2. **Attestation** — signed, gossiped, advisory; links / names / repudiates keys.
3. **Local name resolution** — petname → trusted key-set; the fan-out addressing
   function; manual override wins.
4. **Delivery + sync** — per-key fan-out send (direct or mailbox); content-addressed
   `get` / `get-successors`, served at each peer's discretion; content-key re-wrap for
   backfill.

Everything else — grouping, naming, ordering-for-display, membership presentation,
custom conversation views — is **client policy/UX**.

---

## 10. Encoding & versioning

- **BORSH** for all hashed/signed objects. Content-addressing requires a
  **canonical, deterministic** encoding (identical bytes on every implementation →
  identical hashes); BORSH is designed for deterministic serialization-for-hashing.
  (Protobuf and bincode are unsuitable for the hashed objects.)
- BORSH is not self-describing, so **every object begins with an explicit `version`**
  tag; unknown future versions are ignored or surfaced, never misparsed.

---

## 11. Decisions (resolved)

| Decision | Choice | Why |
|---|---|---|
| Recovery anchor | **None** | Social recovery only; an anchor adds complexity and fights the philosophy. |
| Client scope | **PWA only** | Simplicity; native (true p2p) can come later. |
| Wire format | **BORSH** | Deterministic encoding required for content-addressing. |
| Pairwise channel | **Static sealed-box** | Simple; FS has low value for a history-retaining friends app. |
| Device history sync | **Peer sync + content-key re-wrap** | No shared family key; device sync = peer sync. |
| Relays | **Anyone can run one; interchangeable** | Minimal, replaceable infrastructure. |
| Ordering | **Lamport (`logical`) + per-sender `seq` + DAG** | Partial-view ordering, gap detection, honest concurrency. |

**Still to pin down (implementation-level):** exact AEAD choice (e.g.
XChaCha20-Poly1305), Web Push payload/encryption specifics, presence-beacon format,
relay discovery/configuration UX.

---

## 12. Suggested phasing

| Phase | Deliverable | Proves |
|---|---|---|
| **0** | Keys, one device, 1:1 fan-out messages, images, mailbox + push | The hard delivery/offline plumbing |
| **1** | Attestations + local name resolution + multi-device (peer-sync + content-key re-wrap) | Fluid identity & "this key is also me" |
| **2** | Multi-recipient fan-out + the message DAG (parents, heads, `logical`/`seq`, linearization) | Group chat with no group crypto |
| **3** | Profile attestations + "who is this?" discovery, concurrency-aware views | The social identity layer |

---

## References

- [Iroh 1.0 — Dial Keys, not IPs](https://www.iroh.computer/blog/v1)
- [iroh WebAssembly & browser support](https://docs.iroh.computer/deployment/wasm-browser-support)
- [Iroh & the Web](https://www.iroh.computer/blog/iroh-and-the-web)
- iroh-gossip, iroh-blobs — separate protocol crates on top of iroh
