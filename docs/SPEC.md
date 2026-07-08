# zink — MVP Specification (draft)

A small, p2p-first chat protocol and app built on [iroh 1.0](https://www.iroh.computer/blog/v1),
for me and my close friends. This document specifies the **MVP feature set** and
**high-level system components**. It is a protocol first, with clients and relays
as separate implementations.

Status: **draft for discussion**. Sections marked ⚠️ are open decisions.

---

## 1. Principles

1. **P2P when possible, relayed when necessary.** End-to-end encryption everywhere,
   so relays are always *untrusted* infrastructure — they route and store ciphertext,
   never plaintext.
2. **Identity is local, not global.** No central account registry. Each user curates
   a contact list (a petname system) and their own view of who's who.
3. **Small and simple.** A WASM PWA client and a tiny relay/mailbox server binary.
4. **Open protocol.** Versioned wire formats; many implementations possible.

---

## 2. Terminology

| Term | Meaning |
|---|---|
| **Endpoint / device key** | An iroh keypair = one device. iroh's `EndpointId`. Does the actual networking. |
| **Identity key** | A long-lived keypair representing a *person*. Signs device certs & revocations. Backed up as a recovery phrase; rarely online. |
| **Device cert** | A statement signed by an identity key: "device key D belongs to me, valid from time T." |
| **Contact** | A local record: an identity key + a user-assigned **petname** + cached profile. |
| **Petname** | A memorable, *locally-assigned* name for a contact. Not global. |
| **Self-profile** | Name/avatar a user asserts about themselves, signed by their identity key. |
| **Attestation** | A signed claim by user A about user B: "I call this key <name>, they look like <avatar>." |
| **Mailbox** | Relay-hosted store of E2E-encrypted messages for an offline device. |

> **Key point:** an iroh `EndpointId` is a *device* identity, not a *person* identity.
> A person = one identity key that certifies a set of device keys.

---

## 3. Identity & key management

### 3.1 Model

- A person has **one identity key** and **N device keys** (one per device).
- The identity key signs a **device cert** for each device key it authorizes.
- Networking, messaging, and gossip all use **device keys** (iroh endpoints).
- The identity key is the root of trust for a person and should be backed up
  (BIP39-style recovery phrase) and kept offline as much as possible.

### 3.2 Adding a device (pairing)

1. New device generates a device key.
2. Local pairing between two of *your own* devices (QR code / short code over a
   local or relay channel), authenticated by a short-authentication-string to
   resist MITM.
3. An existing device (which can act for the identity key) issues a **signed
   device cert** for the new device key.
4. The updated device set is **gossiped** to contacts so they update their records.
   This is the authenticated version of "hey — this key is also me."

> ⚠️ The identity key may be held directly by the primary device, or split so a
> device only holds a delegation. MVP: primary device holds the identity key;
> pairing produces a device cert signed by it.

### 3.3 Removing / revoking a device

- A signed **revocation** (device key + monotonic version/timestamp) is gossiped.
- Contacts apply last-writer-wins by version. Lost/compromised identity key is the
  hard case (recovery-phrase rotation) — **out of scope for MVP**, documented as a
  known limitation.

---

## 4. Trust, contacts & profiles

### 4.1 Petname contact list

- Adding a contact = storing their identity key + assigning a petname.
- Bootstrapped by an out-of-band exchange (QR / link) or via an attestation from an
  existing contact.

### 4.2 Profiles

- **Self-profile:** name + avatar signed by the subject's identity key. Shown by default.
- **Third-party attestations:** any contact can gossip "I call key X <name>, avatar <blob>."
  Aggregated and shown as *"your friends call them …"*. This is what makes
  "everyone can set profile pictures for other people" work.

### 4.3 "Who is this?" web-of-trust query

- Client gossips a query: "who is `identity-key`?"
- Contacts respond with their attestation (self-asserted if it's them, or their
  petname/avatar for that key).
- Client aggregates responses, ranks by trust (direct contact > friend-of-friend).

> ⚠️ This is the most research-y feature. **Deferred to the last MVP phase.**

---

## 5. Messaging

### 5.1 Two transport planes

- **Direct plane** (reliable): 1:1 DMs, blob transfer, pairing, mailbox sync — over
  direct iroh connections with acks. Use a dedicated ALPN.
- **Gossip plane** (best-effort): group messages, presence, identity/profile
  attestations, "who is this?" queries — over `iroh-gossip` topics.

> Gossip is **not** reliable delivery. Anything that must arrive uses the direct
> plane + mailbox.

### 5.2 Direct messages

- Sender opens a direct connection to a recipient device (relay-routed for browsers).
- If recipient is offline → deposit ciphertext in their **mailbox** (§7) + trigger push.

### 5.3 Group messages

- A group = a gossip topic + a membership list (signed set of member identity keys).
- Messages are E2E-encrypted to the current member set (§6).
- Membership changes are signed group-management events.

### 5.4 Message envelope

- Versioned (CBOR or protobuf), signed by the sending device key, containing:
  `{ version, group/thread id, sender device key, timestamp, ciphertext, blob refs, sig }`.

---

## 6. Encryption ⚠️

Transit is already encrypted by iroh (QUIC/TLS with device keys), but gossip fans
out through relays and peers, so **confidentiality requires an application layer.**

Two candidates:

| | Shared group key | MLS (OpenMLS) |
|---|---|---|
| Complexity | Low | High |
| Forward secrecy | No | Yes |
| Member removal | Re-key O(members) | Efficient |
| Fit | MVP shortcut | Long-term correct |

**Recommendation:** design the envelope so the crypto layer is swappable. Decide
early whether to adopt MLS now (retrofitting group crypto later is painful) or ship
a shared-key scheme first. Pairwise DMs can use a simple X25519/Noise-style channel
regardless.

---

## 7. Images & blobs

- Use **iroh-blobs** (BLAKE3, content-addressed).
- **Encrypt the image**, address by hash of the ciphertext, put the symmetric key in
  the (E2E-encrypted) message. Content-addressing gives integrity + dedup for free.
- Send **two blobs**: a small encrypted thumbnail (preview) + full-res. The message
  references both.
- Offline delivery: relay caches encrypted blobs with a TTL / size cap so the
  recipient can fetch even if the sender has gone offline.

---

## 8. Notifications & offline delivery (foundational, not optional)

- **Mailbox:** relay stores E2E-encrypted messages/blobs for offline device keys.
  Untrusted for content; learns only metadata (which mailbox, sizes, timing).
- **Web Push gateway:** on deposit for an offline user, relay sends a content-free
  Web Push ("you have messages"). Device wakes, authenticates, pulls & decrypts.
- Requires VAPID + browser push services (FCM/Apple/Mozilla) — an unavoidable
  non-p2p dependency for a PWA. Acknowledged.

---

## 9. System components

```
┌─────────────────┐        gossip / direct (relay-routed for browser)        ┌─────────────────┐
│  PWA client      │◀───────────────────────────────────────────────────────▶│  PWA client      │
│  (WASM + iroh)   │                                                          │  (WASM + iroh)   │
└───────┬─────────┘                                                          └─────────────────┘
        │  mailbox sync, push registration, blob fetch
        ▼
┌──────────────────────────────────────────────┐
│  Relay + Mailbox + Push gateway (small binary) │
│  - iroh relay (connectivity / NAT traversal)   │
│  - encrypted mailbox store (offline delivery)  │
│  - encrypted blob cache (TTL)                  │
│  - Web Push (VAPID) sender                     │
└──────────────────────────────────────────────┘
```

- **PWA client (WASM):** iroh compiled with `default-features = false`; always
  relay-routed (no browser hole-punching). Handles keys, contacts, crypto, UI.
- **Relay/mailbox server:** small Rust binary. iroh relay + mailbox + blob cache +
  push. Untrusted for content. Hosted initially on one server; protocol allows more.
- **(Optional) native client:** can achieve true direct p2p (hole-punching). Decide
  if in scope.

---

## 10. Open decisions ⚠️

1. **Group crypto:** MLS now vs shared-key first.
2. **Native clients:** in MVP scope, or PWA-only?
3. **Identity key custody:** on primary device vs. split/delegated.
4. **Wire format:** CBOR vs protobuf.
5. **Multiple relays / federation:** how soon.

---

## 11. Suggested phasing

| Phase | Deliverable | Proves |
|---|---|---|
| **0** | Single-device identity, 1:1 DMs, images, mailbox + push | The hard delivery plumbing |
| **1** | Multi-device pairing (identity key + device certs) | Authenticated "this key is also me" |
| **2** | Group messages + group crypto decision | Confidential fan-out |
| **3** | Profile attestations + "who is this?" web-of-trust | The fun identity layer |

---

## References

- [Iroh 1.0 — Dial Keys, not IPs](https://www.iroh.computer/blog/v1)
- [iroh WebAssembly & browser support](https://docs.iroh.computer/deployment/wasm-browser-support)
- [Iroh & the Web](https://www.iroh.computer/blog/iroh-and-the-web)
- [iroh-gossip], [iroh-blobs] — separate protocol crates on top of iroh
