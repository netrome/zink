# zink — MVP Specification (draft)

A small, p2p-first chat protocol and app built on [iroh 1.0](https://www.iroh.computer/blog/v1),
for me and my close friends. Specifies the **MVP feature set**, the **protocol
building blocks**, and the **high-level system components**. It is a protocol first,
with clients and relays as independent implementations.

Read [DESIGN-PHILOSOPHY.md](./DESIGN-PHILOSOPHY.md) first — this document is
downstream of it. Sections marked ⚠️ are open decisions.

Status: **draft, converging.**

---

## 1. The core model in one paragraph

A person is a **fluid set of keys**, linked by signed **attestations** ("this key is
also me" / "this key is Alice"). There is no permanent identity key and no global
account. Each client maintains its own belief about which keys belong to whom, like a
contact list, and can revise it manually at any time. A **conversation** is a
content-addressed **DAG of messages** rooted at a genesis message; there is no
separate notion of a "group" — a message is simply **fanned out**, encrypted
per-recipient, to the set of keys the sender chose. Membership, names, and grouping
are local interpretations layered on top of keys and hashes. Offline delivery and
notifications go through untrusted **relays/mailboxes** that only ever see ciphertext.

Everything below is an elaboration of that paragraph.

---

## 2. Terminology

| Term | Meaning |
|---|---|
| **Key** | An iroh keypair = one device / one `EndpointId`. The only cryptographic identifier. |
| **Person** | A *local* clustering of keys, via attestations you trust. Not a protocol object. |
| **Petname** | Your local, private name for a person. Never sent on the wire. |
| **Attestation** | A signed, gossiped, *advisory* claim linking keys or naming them. |
| **Conversation** | A DAG of messages rooted at a genesis message; identified by the genesis hash. |
| **Message** | A content-addressed (BLAKE3) signed object; its hash is its id. |
| **Recipient-set** | The keys a sender chose to fan a message out to. Recorded, advisory. |
| **Device-family key** | A symmetric key shared among one person's own devices, for history sync. |
| **Mailbox** | Relay-hosted store of E2E-encrypted messages/blobs for an offline key. |

---

## 3. Identity

### 3.1 Model

- A person = a set of keys. No mandatory long-lived "identity key."
- Keys are linked by **attestations** you and your contacts sign and gossip.
- "Who is Alice" is **your local belief**, not a global fact. Manual override always
  wins over any received attestation.

### 3.2 Attestations (one primitive, several uses)

A signed statement, gossiped to contacts and treated as *advisory input*:

```
Attestation {
  subject:  key (or key it links to)
  claim:    same-person-as <key> | name <label> | avatar <blob-ref> | negative | revoke <attestation-hash>
  attester: key
  version:  monotonic (per attester+subject)
  sig
}
```

Uses, all the same primitive:

- **Add my own device** — I sign `same-person-as` linking my new key to an existing key of mine ("this key is also me").
- **Vouch for a contact** — I sign that some key is the same person I call Alice.
- **Repudiate** — a `negative` claim ("I don't recognize this key") or a `revoke`.
- **Profiles / third-party profiles** — `name` / `avatar` claims, self-asserted or
  about others. This is what makes "everyone can set profile pictures for other
  people" work: clients aggregate contacts' claims, weighted by trust, and show
  *"your friends call them …"*.

On the wire attestations link **key → key**; the human `label` is your local petname
overlay and is never required to be shared.

### 3.3 Name resolution = the addressing layer

Sending "to Alice" means: resolve petname → the current set of keys you believe are
Alice (your attestations + trusted contacts' attestations + manual overrides), then
fan out to those keys. A repudiated key simply drops out of the set. **Identity
resolution and message addressing are the same step.**

### 3.4 Recovery is social (tenet 8)

Losing a key forks your identity. You and an attacker can both claim to be you; the
protocol does not arbitrate. You call a friend out-of-band; they re-attest which key
is really you, mark the other `negative`, and gossip it — their clients stop routing
your messages to the bad key.

> ⚠️ **Open decision:** offer an *opt-in* cryptographic recovery anchor (a
> cold-storage key that can authorize/repudiate device keys) for users who want
> stronger recovery? Default off; never mandatory.

---

## 4. Conversations & messages

### 4.1 Conversation = genesis-rooted message DAG

- A conversation is identified by the hash of its **genesis message**. Distinct
  geneses are distinct conversations automatically (no separate group id / nonce).
- A **message** is content-addressed; its BLAKE3 hash is its id and how others
  reference it.
- Every message points to its **parents** — the sender's current known *heads*
  (messages with no known successor). This forms the causal DAG.

```
Message {
  conversation:  genesis hash
  parents:       [message hash]     // current heads at send time
  recipient-set: [key]              // who the sender fanned out to (advisory record)
  timestamp:     wall-clock hint    // display only, never trusted for ordering
  ciphertext:    bytes              // E2E, per-recipient (§6)
  blob-refs:     [ {hash, wrapped-key, kind: thumb|full} ]  // §7
  sender:        key
  sig
}
```

### 4.2 There is no "group" — only fan-out

A 1:1 chat and a group chat are the same thing: a message fanned out to a set of
keys. "Membership" is not a protocol object — it is each client's local
reconstruction from `recipient-set`s it has seen, plus its own key→person clustering.
The `recipient-set` is a durable, honest record of who the sender actually sent to
(and thus who could see it), but it enforces nothing.

### 4.3 Ordering (tenet 7)

- The DAG **is** the causal history; it subsumes vector clocks.
- **Multiple heads = concurrency** — "these messages crossed in flight," first-class
  data rather than a hidden accident.
- Clients render a **deterministic linear default** (topological sort + stable
  tiebreak, e.g. by message hash). Wall-clock `timestamp` is a display hint only.
- Advanced clients *may* expose the concurrency structure. This is a client choice.

### 4.4 Membership is local (tenet 3)

Anyone can send to anyone; you cannot prevent Bob from including Charlie. So there is
no membership consensus and no enforced removal. "Add/remove," "who's in this chat,"
and "should the newcomer see the backlog" are all resolved by **local discretion**:
who you send to, and — crucially — **what history you choose to serve** (§5.2).

---

## 5. Delivery & sync

### 5.1 Two transport planes

- **Direct plane** (reliable, dedicated ALPN): fan-out message delivery, blob
  transfer, device pairing, history sync. Direct iroh connections + acks;
  relay-routed for browsers.
- **Gossip plane** (best-effort, `iroh-gossip`): attestations, presence, and the
  "who is this?" discovery query (contacts respond with their attestations).

Gossip is **not** reliable delivery. Anything that must arrive uses the direct plane
+ mailbox.

### 5.2 History sync = one primitive, best-effort (tenet 6)

There is a single content-addressed sync mechanism, used identically for **a new
member requesting backlog** and **your own new device catching up**:

- `get(hash)` — fetch a message/blob by hash.
- `get-successors(hash)` — fetch known children of a message.
- Missing parent pointers are the **gap signal**: a referenced parent you don't have
  tells you exactly what to ask for.
- Serving is **discretionary** — a peer serves what it has and what it *chooses* to.
  This is also how backlog privacy works (don't want the newcomer to see old
  messages? don't serve the parents).

Backfill is never guaranteed complete, and the DAG makes the incompleteness visible.
The only difference between "new person" and "new device of mine" is
**decryptability**, not mechanism (§6.2).

### 5.3 Offline delivery & notifications (foundational, not a late feature)

- **Mailbox:** when a recipient key is offline, its ciphertext is parked in a
  relay-hosted mailbox until the device reconnects. Untrusted for content.
- **Web Push gateway:** on deposit, the relay sends a content-free push ("you have
  messages"); the device wakes, authenticates, pulls, decrypts. Requires VAPID +
  browser push services — an unavoidable non-p2p dependency for a PWA. Acknowledged.
- **Retention bounds recoverability:** "you can always backfill" is true only within
  mailbox/blob retention.

---

## 6. Encryption

### 6.1 Two layers

- **Fan-out layer (between people):** each message/blob-key is encrypted
  *per-recipient-key*. Pairwise channels (X25519 / Noise-style; ratcheting gives
  forward secrecy per channel). Relays never see plaintext.
- **Device-family layer (within one person):** a symmetric key shared among your own
  devices (established at pairing) encrypts your local history so any of your devices
  can serve/backfill any other. This is the *only* shared-key construction, and it is
  scoped to a single person's devices — it avoids all group-key complexity.

`parents`, `conversation`, `recipient-set`, and `sender` are identical across all
recipients (they must be, to reassemble the DAG) — visible to members, and their
*sizes* visible to the relay.

### 6.2 Why device sync needs the device-family key

Fetching old messages by hash doesn't help a new device that wasn't an original
recipient — they were encrypted to other keys. The device-family key is what lets the
identical sync mechanism actually decrypt same-person backfill.

---

## 7. Images & blobs

- **iroh-blobs** (BLAKE3, content-addressed).
- Encrypt each image **once** with a random symmetric key; address by the ciphertext
  hash. Fan out only the small **wrapped key** per recipient — media stays O(1) in
  storage, O(recipients) in a few hundred bytes.
- Send **two blobs**: a small encrypted **thumbnail** (instant preview) + **full-res**.
  The message references both.
- Relays may **cache encrypted blobs** (TTL / size cap) so a recipient can fetch even
  after the sender goes offline.

---

## 8. System components

```
┌─────────────────┐    gossip + direct (relay-routed for browsers)    ┌─────────────────┐
│  PWA client      │◀─────────────────────────────────────────────────▶│  PWA client      │
│  (WASM + iroh)   │                                                    │  (WASM + iroh)   │
└───────┬─────────┘                                                    └─────────────────┘
        │  mailbox sync · push registration · blob fetch
        ▼
┌─────────────────────────────────────────────────┐
│  Relay + Mailbox + Push gateway (small binary)     │  ← untrusted; ciphertext + metadata only
│   · iroh relay (connectivity / NAT traversal)      │
│   · encrypted mailbox (offline delivery)           │
│   · encrypted blob cache (TTL)                     │
│   · Web Push (VAPID) sender                        │
└─────────────────────────────────────────────────┘
```

- **PWA client (WASM):** iroh built with `default-features = false`; **always
  relay-routed** (browsers can't hole-punch). Holds keys, attestations, contacts,
  crypto, DAG store, UI.
- **Relay / mailbox / push server:** small Rust binary; untrusted for content. One
  instance to start; protocol allows adding more (⚠️ federation later).
- **(Optional ⚠️) native client:** can achieve true direct p2p (hole-punching).

---

## 9. Protocol building blocks (summary)

1. **Message object** — content-addressed, signed; `{conversation, parents,
   recipient-set, timestamp, ciphertext, blob-refs, sender, sig}`.
2. **Attestation object** — signed, gossiped, advisory; links/names/repudiates keys.
3. **Local name resolution** — petname → trusted key-set; the fan-out addressing
   function; manual override wins.
4. **Delivery + sync** — per-key fan-out send (direct or mailbox); content-addressed
   `get` / `get-successors`, served at each peer's discretion.

Everything else — grouping, naming, ordering-for-display, membership presentation,
custom conversation views — is **client policy/UX**.

---

## 10. Open decisions ⚠️

1. **Recovery anchor:** offer opt-in cryptographic anchor, or social-only? (lean: opt-in)
2. **Native client:** in MVP scope, or PWA-only first?
3. **Wire format:** CBOR vs protobuf.
4. **Pairwise channel:** static sealed-box vs ratcheting (forward secrecy).
5. **Multiple relays / federation:** how soon.

---

## 11. Suggested phasing

| Phase | Deliverable | Proves |
|---|---|---|
| **0** | Keys, one device, 1:1 fan-out messages, images, mailbox + push | The hard delivery/offline plumbing |
| **1** | Attestations + local name resolution + multi-device (device-family key, history sync) | Fluid identity & "this key is also me" |
| **2** | Multi-recipient fan-out + the message DAG (parents, heads, linearization) | Group chat with no group crypto |
| **3** | Profile attestations + "who is this?" discovery, concurrency-aware views | The social identity layer |

---

## References

- [Iroh 1.0 — Dial Keys, not IPs](https://www.iroh.computer/blog/v1)
- [iroh WebAssembly & browser support](https://docs.iroh.computer/deployment/wasm-browser-support)
- [Iroh & the Web](https://www.iroh.computer/blog/iroh-and-the-web)
- iroh-gossip, iroh-blobs — separate protocol crates on top of iroh
