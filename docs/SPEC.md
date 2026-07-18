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
| **Petname** | A *client-level* label you assign a person (the term of art for a locally-assigned, observer-specific name). **Not a protocol object** — becomes a `name` attestation only if you choose to broadcast it. |
| **Attestation** | A signed, *advisory* claim linking keys or naming them; shared **by request**, never broadcast. |
| **Conversation** | A DAG of messages rooted at a genesis message; identified by the genesis hash. |
| **Message** | A content-addressed (BLAKE3) signed object; its hash is its id. |
| **Content-key** | Random per-message symmetric key; encrypts the body once. Sealed per recipient. |
| **Recipients** | The keys a sender chose to fan a message out to. Recorded, signed, advisory. |
| **Mailbox** | Relay-hosted store of E2E-encrypted messages/blobs for an offline key. |

---

## 3. Identity

### 3.1 Model

- A person = a set of keys. **No** mandatory long-lived "identity key."
- Keys are linked by **attestations** you and your contacts sign and share on request.
- "Who is Alice" is **your local belief**, not a global fact. Manual override always
  wins over any received attestation.

### 3.2 Attestations (one primitive, several uses)

A signed statement, shared with contacts on request, treated as *advisory input*:

```
Attestation {                          // id = BLAKE3(borsh(...)); sig is Ed25519 over the id
  version:   format tag                // §10 protocol/format version (like every hashed object)
  attester:  key
  subject:   key
  claim:     name <label> | avatar <blob-ref> | same-person-as <key> | negative
  revision:  u64                        // supersession counter — see below
  sig
}
```

Uses, all the same primitive:

- **Add my own device** — sign `same-person-as` linking a new key to an existing key
  of mine ("this key is also me"). A link is **strongest when mutual** (both keys sign
  each other); the pairing handshake (§3.6) produces exactly that, and clients should
  weight mutual links above unilateral ones — a lone key asserting "your key is also
  me" is structurally just a claim, trusted only as much as its attester.
- **Vouch for a contact** — sign that some key is the person I call Alice.
- **Repudiate** — a `negative` claim: an *active* disavowal ("I do not / no longer
  recognise this key") that propagates so others can act on it (§3.4 relies on this).
- **Profiles / third-party profiles** — `name` / `avatar` claims, self-asserted or
  about others. This is what makes "everyone can set profile pictures for other
  people" work: clients aggregate contacts' claims, weighted by trust, and show
  *"your friends call them …"*.

**Supersession — one mechanism.** The highest `revision` wins, scoped per
`(attester, subject, claim-kind, + the linked key for same-person-as)`. So bumping
your avatar never disturbs your name, and linking a second device never unlinks the
first. There is **no separate `revoke`**: to withdraw a claim you supersede it with a
higher-`revision` `negative` — active disavowal that travels, rather than silence that
doesn't.

On the wire, attestations link **key → key**. A human label enters the protocol
*only* as a broadcast `name` attestation. The label you assign but keep to yourself —
your **petname** for a person — is a pure client convention the core protocol never
sees; broadcasting it is exactly what turns it into a `name` attestation.

### 3.3 Name resolution = the addressing layer

Sending "to Alice" means: resolve the petname "Alice" → the set of keys you
currently believe are Alice (your attestations + trusted contacts' attestations +
manual overrides), then fan out to those keys. A repudiated key drops out of the set.
The petname→cluster binding is your local convention; the resolution to keys uses
trusted attestations. **Identity resolution and message addressing are the same step.**

### 3.4 Recovery is social, not cryptographic

Losing a key forks your identity. You and an attacker can both claim to be you; the
protocol does not arbitrate — and deliberately offers **no cryptographic recovery
anchor** (it would add complexity and contradict the philosophy). You call a friend
out-of-band; they re-attest which key is really you, mark the other `negative`, and
share it — their clients stop routing your messages to the bad key.

### 3.5 Identity discovery: the "who is this?" query

There is a **single identity primitive**: a `who-is-this(key)` request, answerable by
**yourself** (return your self-attestations) or by **anyone else** (return their
attestations about that key). You send it to your contacts; each answers at
discretion. Default reach is **1 hop** (your direct contacts); a contact may forward
to theirs within a small hop limit, as their own choice.

Anyone can claim anything about anybody, so **your social graph is your trust
boundary** — and *ranking* those claims is client policy: weight a close friend over
an acquaintance, require agreement from N contacts, prefer self-asserted over
third-party, automatically or manually. This is where clients differentiate while the
protocol stays tiny.

Because there is no broadcast channel, **default privacy is structural**: an outsider
has no path to your attestations unless someone in your circle chooses to relay one.

### 3.6 Onboarding: the contact / pairing record (QR)

Adding a contact and pairing your own next device use the **same artifact**: a client
renders a QR / link encoding your **rendezvous record** —

```
ContactRecord {
  keys:          [key]              // current device keys
  attestations:  [Attestation]      // self-attestations (name, avatar, same-person-as links)
  relays:        [RelayEntry]       // my relay services, one entry each:
                                    //   mailbox    — dial string (deposits/drains)
                                    //   relay_url  — the same service's iroh relay
                                    //                URL (optional; peer dial-by-key)
}
```

Scanning it gives the other side everything needed to reach and render you: whom to
fan out to, how to display you, and — crucially — **which relay(s) hold your mailbox**
when you're offline. This is the rendezvous answer for *both* states: a device homes
to its relays' iroh relay URLs, so a peer holding your record reaches you **by key**
while you're online (relay-coordinated — signaling via your relay, holepunched to a
direct path, relayed as encrypted fallback; no discovery service involved), and
`mailbox` is how a sender finds your inbox when you're not. An entry without a
`relay_url` is mailbox-only knowledge: deposits work, dial-by-key doesn't. Pairing is
the same exchange between two of your own devices and yields a **mutual**
`same-person-as` link. It's also the natural place to later hand over an initial
capability grant (§8) so a new contact can message you from the start.

**Freshness.** `relays` is the rendezvous anchor for offline delivery, so it must stay
reasonably current. It propagates lazily — via the QR at add-time, `who-is-this`, and a
version hint piggybacked on messages — and a device keeps an abandoned relay alive for a
grace period. Brief mis-delivery windows are tolerated (best-effort, tenet 6).

---

## 4. Conversations & messages

### 4.1 Conversation = genesis-rooted message DAG

- Identified by the hash of its **genesis message**. The genesis's own content is its
  de-facto identifier — no separate group id or nonce field. Byte-identical geneses
  (same sender, recipients, body, tick) are *by definition the same conversation*: a
  harmless idempotent merge, not a collision to defend against. A client wanting two
  identically-framed but distinct conversations just varies the body (e.g. a title) or
  drops in its own nonce there — at the client layer.
- A **message** is content-addressed; its BLAKE3 hash is its id.
- Every message points to its **parents** — the sender's current known *heads*
  (messages with no known successor). This forms the causal DAG.

The signed, hashed **core** (identical bytes for every recipient → one shared id):

```
MessageCore {                          // id = BLAKE3(borsh(MessageCore)); sender signs those 32 bytes
  version:       format tag            // §10 protocol/format version
  conversation:  genesis id | null     // null in the genesis itself; else the genesis's id
  parents:       [message hash]        // current heads at send time ([] in the genesis)
  recipients:    [key]                 // who this was fanned out to (advisory, but signed)
  sender:        key
  seq:           u64                   // per (sender, conversation), 0-based (sender's first msg = 0)
  logical:       u64                   // Lamport = 1 + max(parents.logical); 0 in the genesis
  timestamp:     wall-clock hint       // display only, never trusted for ordering
  body:          ciphertext            // encrypted ONCE with a random content-key
  key-commit:    BLAKE3(content-key)   // binds the id to the key → "same id ⇒ same content" (§6)
  blob-refs:     [ {hash, kind: thumb|full, key-commit: BLAKE3(blob content-key)} ]   // §7
}
```

**Genesis message:** `conversation = null`, `parents = []`, `seq = 0`, `logical = 0`;
its own id becomes the conversation id every later message carries. This breaks the
circularity — the genesis cannot contain its own hash.

Transport envelope (**not** part of the id; per-recipient parts live here):

```
MessageEnvelope {
  version:        format tag           // transport framing evolves independently of the core
  core:           MessageCore
  sig:            Ed25519 by `sender` over the id (= BLAKE3(borsh(core)))
  key-wraps:      [ {recipient: key,
                     sealed: [ {ref: "body" | blob-hash, sealed-key} ]} ]   // one wrap per encrypted object
}
```

Because the per-recipient `key-wraps` sit outside the hashed core, **everyone derives
the same message id**, so `parents` and the DAG hold across all recipients. `sealed` carries one wrapped content-key per encrypted object — the
body plus each blob (a thumb+full image → three). The **envelope is the unit of
delivery**: a sender deposits it once per distinct recipient-relay, and a relay indexes
it under each recipient device-key it hosts (see `docs/design/mailbox-rendezvous-push.md`).

### 4.2 There is no "group" — only fan-out

A 1:1 chat and a group chat are the same thing: a message fanned out to a set of
keys. "Membership" is not a protocol object — it is each client's local
reconstruction from the `recipients` lists it has seen, plus its own key→person
clustering. `recipients` is a signed, durable record of who the sender actually sent
to (hence who could see it), but it **enforces nothing**.

Anyone can fan out to anyone, so unsolicited contact is possible by design: **the
social graph is the spam boundary.** Delivery from non-contacts is surfaced or
filtered by client policy (and by relay policy, §5.3); capability-based gating (§8)
can be added later — introduced then as a versioned envelope field, not reserved now.

### 4.3 Ordering (tenet 7)

Two small integers, different jobs, plus the DAG:

- **`logical` (Lamport):** lets a client linearize *any subset* of messages
  consistently with causality by sorting on `(logical, tiebreak-by-id)` — works even
  with a partial view, without walking the whole DAG. This is the linear **default view**.
- **`seq` (per `(sender, conversation)`):** completeness / gap detection. Scoped to
  the conversation so contiguity is meaningful — a global per-sender counter would
  show spurious "gaps" that are just the sender's messages in *other* chats, and
  would leak cross-chat volume. `seq` gaps — plus a peer advertising its latest `seq`
  per conversation at connect/sync time — reveal when you're missing a sender's
  *newest* messages, which dangling parent pointers alone cannot.
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

### 5.1 Two interaction patterns (no gossip plane)

Everything runs over iroh connections — direct when a path exists or holepunches,
relayed (still end-to-end encrypted QUIC) through a party's relay when it doesn't,
and always relay-routed for browsers — in two shapes:

- **Fan-out (push):** deliver a message/blob to the recipient keys you chose —
  reliable, acked, parked in the recipient's mailbox if offline.
- **Request/response (pull):** content-addressed `get` / `get-successors` for history
  and blobs, and the `who-is-this` identity query (§3.5) — each served at the
  answering peer's discretion.

At friend/family scale, epidemic **gossip buys nothing**: delivery is already
per-recipient fan-out (§4.2), and identity/profile discovery is pull-based (§3.5).
There is **no gossip plane in the core**; `iroh-gossip` stays an optional future
optimization for swarms large enough to want it.

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
serving discretion as everything else — no shared "family key" exists.

**Why this is safe:** re-wrapping is not a new leak — a recipient could always just
re-send the plaintext; re-wrap is merely the efficient form of that same inherent
capability. Backfill is never guaranteed complete, and the DAG makes incompleteness
visible.

### 5.3 Offline delivery & notifications (foundational, not a late feature)

- **Mailbox:** when a recipient key is offline, its envelope is parked in a
  relay-hosted mailbox until the device reconnects. Untrusted for content.
- **Mailbox auth (protocol) vs. retention (policy).** Mailbox ops run over an
  **authenticated iroh connection**, so the relay already knows the connected peer's
  key: reading/deleting your mailbox just requires the connection key to match the
  mailbox key — no separate challenge (a signed challenge is only the stateless/HTTP
  fallback). This stops anyone draining or deleting another key's mailbox. *Who may
  deposit*, retention windows, rate/size caps, and whether to keep messages from
  non-contacts are **relay-operator policy**, not protocol.
- **Wake-on-message:** the *native* MVP client holds a live connection to its
  relay(s) and receives deliveries forwarded over it ("forward-now"), kept alive by an
  OS foreground service. The **Web Push gateway** (content-free push → device wakes,
  authenticates, pulls, decrypts; VAPID + browser push services — an unavoidable
  non-p2p dependency) is what the future PWA client needs instead.
- **Retention bounds recoverability.** Peer-to-peer backfill is the *primary* path;
  relay retention is a reluctant, TTL-bounded fallback for offline delivery, kept as
  minimal as possible.

---

## 6. Encryption (envelope / hybrid)

- **Body, encrypted once:** a random **content-key** (symmetric AEAD) encrypts the
  message body. The body ciphertext is identical for all recipients → stable content
  hash → working DAG. Same scheme for blobs (§7).
- **Content-key, sealed per recipient:** static **sealed-box** to each recipient key.
  iroh keys are Ed25519; sealing uses the standard **Ed25519→X25519** conversion (use a
  vetted implementation — clamping / low-order-point footguns; never hand-rolled), and
  the message id is signed with the Ed25519 key. One key per device, two uses.
- **Self-wrap convention (2026-07-12):** senders also seal each content-key **to their
  own key**, *without* listing themselves in `recipients` — key-wraps live outside the
  hashed core, so ids are unaffected and recipients see no difference. This is what
  lets a device reopen its own stored copy when rendering history (everything at rest
  stays ciphertext). A client convention, not protocol: a client that skips it only
  loses its own history. Depositing to your *own mailbox* is the multi-device
  extension of the same idea (phase 2).
- **Key commitment (non-committing-AEAD fix):** common AEADs (XChaCha20-Poly1305,
  AES-GCM) are **not** key-committing — a malicious sender could craft one ciphertext
  that decrypts to *different* valid plaintexts under two content-keys, seal a different
  key to each recipient, and yield conflicting messages **sharing one id**, silently
  breaking "same id ⇒ same content" (the invariant the DAG rests on). We commit the
  content-key in the hashed core (`key-commit`, and per-blob in `blob-refs`). A
  recipient unseals its key and **checks it against the commitment before trusting the
  message**; since the commitment is inside the id, only one content-key is valid, so
  all recipients decrypt identical content or reject.
- **Conscious tradeoff:** static sealing means **no forward secrecy** (harvest-now/
  decrypt-later exposure, as with any EC scheme). Accepted for a friends app that
  retains history anyway; still categorically better than a third-party service that
  may hold your plaintext or a backdoor. Ratcheting can replace the sealing layer
  later without touching the DAG or the envelope shape.
- **Relays see ciphertext + metadata only** — the plaintext core (`sender`,
  `recipients`), envelope sizes, and timing; never the body. `recipients` must be
  visible to route; `sender` could later move into the body to reduce this.

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
┌─────────────────┐    direct QUIC (hole-punched; relay-assisted when NATed —          ┌─────────────────┐
│  App client      │    browsers, as a future second client, are always relay-routed)  │  App client      │
│  (native + iroh) │◀───────────────────────────────────────────────────────────────▶│  (native + iroh) │
└───────┬─────────┘                                                                 └─────────────────┘
        │  mailbox sync · blob fetch · live-delivery connection
        ▼
┌──────────────────────────────────────────────────┐
│  Relay + Mailbox (small binary)                     │  ← untrusted; ciphertext + metadata only
│   · mailbox ALPN (offline delivery, deposit/fetch)  │     interchangeable; anyone can run one
│   · encrypted blob cache (TTL)                      │
│   · live-forward to connected recipients (C4)       │
└──────────────────────────────────────────────────┘
```

*(Shipped relay = mailbox ALPN + blob cache; native clients dial its `id@ip:port`
directly, so it hosts neither an iroh-relay server nor a Web Push sender. Both belong
to the future PWA client's path: a co-located iroh-relay for browser WebSocket
transport, and Web Push for browser wake — see §5.3 and the A6 spike.)*

- **App client (native, Tauri v2):** the MVP client — Android + Linux desktop from
  one codebase. Native iroh: direct QUIC with hole-punching, relays only for
  introduction/fallback/offline. Holds keys, attestations, contacts, crypto, the DAG
  store, and UI. *(A PWA/WASM client is the planned second implementation, post-MVP:
  iroh-in-WASM is proven, but it is always relay-routed and needs the Web Push path.)*
- **Relay / mailbox / push server:** small Rust binary; untrusted for content;
  minimal role (help peers connect + retain messages). **Anyone can run one**, and
  they are interchangeable — a user configures which relay(s) they use. I'll run one
  or two to start.

**Anti-spam (deferred).** The social graph is the boundary, and at friends-scale it
mostly self-enforces: only your contacts hold your device key (shared via QR). MVP =
relay rate/size caps + client-side filtering; **no relay-side gating, no economics**.
When spam becomes real, add **capability-based gating** (the sender presents an
unforgeable token you issued; the relay checks it without maintaining an allowlist) —
introduced then as a versioned envelope field, *not* reserved now. That is the first
rung toward optional **fungible personal tokens** (each person the sole clearing
authority for their own token — Chaumian-style, no global consensus). None built now.

> Capability-gating does **not** by itself buy graph privacy: the relay already reads
> `sender` and `recipients` from the plaintext message core (it needs `recipients` to
> route). Minimising that core — e.g. moving `sender` into the encrypted body — is a
> separate, later metadata-minimisation track.

---

## 9. Protocol building blocks (summary)

1. **Message** — content-addressed, signed core + per-recipient envelope; carries
   `conversation, parents, recipients, sender, seq, logical, timestamp, body, blob-refs`.
2. **Attestation** — signed, advisory, shared by request; links / names / repudiates keys.
3. **Local name resolution** — petname → trusted key-set; the fan-out addressing
   function; manual override wins.
4. **Delivery + sync** — per-key fan-out send (push, direct or mailbox); pull via
   `get` / `get-successors` / `who-is-this`, served at each peer's discretion;
   content-key re-wrap for backfill.

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
| Client scope | **Native app first** (Tauri v2: Android + Linux desktop; revised 2026-07-11, was "PWA only") | The browser platform carried the MVP's hardest costs (Web Push, evictable IndexedDB keystore, TLS/VAPID ops) and denies true p2p; native reuses the proven Rust client stack and hole-punches. The PWA becomes the post-MVP second client (iroh-in-WASM already proven). |
| Wire format | **BORSH** | Deterministic encoding required for content-addressing. |
| Pairwise channel | **Static sealed-box** | Simple; FS has low value for a history-retaining friends app. |
| Device history sync | **Peer sync + content-key re-wrap** | No shared family key; device sync = peer sync. |
| Relays | **Anyone can run one; interchangeable** | Minimal, replaceable infrastructure. |
| Ordering | **Lamport (`logical`) + per-sender `seq` + DAG** | Partial-view ordering, gap detection, honest concurrency. |
| Message integrity | **Commit the content-key in the core** (`key-commit`) | AEADs aren't key-committing; without it "same id ⇒ same content" breaks. |
| ContactRecord wire form | **Relay endpoints as dial strings** (`id@ip:port`); QR payload = `ZINK:<base32(borsh)>` | Compact, human-inspectable, QR-alphanumeric-mode friendly; richer relay addressing (browser relay URLs) is a version bump. |
| AEAD + commitment | **XChaCha20-Poly1305** (fresh random nonce, `nonce ‖ ct`); commitment = BLAKE3 `derive_key("zink v1 key-commit", content-key)` | Random-nonce-safe AEAD; domain-separated commitment, not a bare hash. |
| Key sealing | **libsodium-style sealed box** (X25519 via the standard Ed25519 conversions) | Anonymous, per-recipient, vetted construction — nothing hand-rolled. |
| `seq` origin | **0-based per (sender, conversation)** | Sender's first message = 0 (genesis included); a cross-impl interop point. |

**Still to pin down (implementation-level):** the `who-is-this` query format and
default hop limit, sync-time head/`seq` exchange, relay discovery/config UX, and the
deferred capability/token gating mechanism (added as a versioned field when needed).
*(The mailbox auth/handshake is resolved and shipped — connection-key auth over the
`zink-mailbox/1` ALPN, [mailbox-wire-protocol.md](../docs/design/mailbox-wire-protocol.md).
Web Push specifics move to the future PWA client's scope.)*

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
- iroh-blobs — content-addressed blob transfer on top of iroh
- iroh-gossip — optional/future; not used in the core protocol
