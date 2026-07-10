# Design: Mailbox, Rendezvous & Push

Detailed design for the operational core of Phase 0 — how a message actually reaches
an offline recipient and wakes their device. Downstream of [SPEC.md](../SPEC.md) and
[DESIGN-PHILOSOPHY.md](../DESIGN-PHILOSOPHY.md).

Status: **draft for discussion.** ⚠️ marks open decisions.

---

## 1. Goals & non-goals

**Goals:** reliable delivery to offline devices; wake-on-message for a PWA; keep the
relay dumb and untrusted (ciphertext + minimal metadata); one delivery path for online
and offline; multi-relay and multi-device without special-casing.

**Non-goals:** the relay never sees plaintext, never orders messages (the DAG does),
never arbitrates identity, and stores nothing it isn't asked to. Read receipts and
delivery receipts are *app-level messages*, not relay features.

---

## 2. Relay: two co-located functions

One small binary, two distinct roles:

1. **iroh relay** — connectivity / NAT traversal for peers (standard iroh; used when
   two clients connect, and always for browser clients which can't hole-punch).
2. **Mailbox + push service** — an application protocol (custom iroh ALPN) spoken by a
   client *to the relay's own endpoint*: register, deposit, fetch, ack, plus a Web
   Push (VAPID) sender and an encrypted blob cache.

Clients reach the mailbox protocol over an authenticated iroh connection to the relay.

---

## 3. The unified delivery path

```
Sender (Bob)                 Alice's relay R              Alice's device A (closed → woken)
   │ resolve Alice → device keys + relays
   │  (from her ContactRecord, §rendezvous)
   │ deposit(envelope) ───────────▶│ store under mailbox[A] (+ any other
   │ ◀──────────── ack(msg-id) ────│  recipients on R that appear in envelope)
   │                               │
   │                               │ A live-connected? ── yes ─▶ forward now over live conn
   │                               │        │ no
   │                               │        ▼
   │                               │ Web Push (content-free) ─▶ push service ─▶ SW 'push' event
   │                               │                                              │ connect to R
   │                               │ ◀──────── fetch(since cursor) ───────────────│
   │                               │ ────────── [envelopes] ──────────────────────▶ decrypt locally,
   │                               │                                                upgrade notification
   │                               │ ◀──────── ack([msg-id…]) ────────────────────│
   │                               │ drop acked (else TTL backstop)               │
```

"Online" = a live connection draining the mailbox in real time. "Offline" = woken by
push, then drains. Same path; the only difference is forward-now vs. push-then-fetch.

---

## 4. Rendezvous — finding where to deliver

- A recipient's **ContactRecord** (SPEC §3.6) lists her `relays`. Her devices each
  **register** a mailbox on each of those relays.
- To send, the sender resolves recipient device keys (identity resolution), collects
  the **distinct relays** across all recipients, and **deposits the envelope to each**.
  The envelope carries key-wraps for every recipient; each relay indexes it under the
  recipient device-keys it hosts. → deposit count = number of distinct relays, not
  devices.
- **Receiver side:** a device polls/subscribes to **all** its own relays and **dedups
  by message id** (content-addressed → free dedup). So a sender may deposit to one
  relay (cheap) or several (redundant) with no dup problem.
- **Online delivery** for two live peers still uses a direct iroh connection
  (relay-routed for browsers); the mailbox is the store for when the target isn't live.
- **Freshness ⚠️:** relay lists change. The ContactRecord propagates lazily — handed
  over at QR add-time, re-fetched via `who-is-this`, and piggybacked as a version hint
  on messages (a peer seeing a newer version pulls the update). A device should keep an
  abandoned relay alive for a grace period. Brief mis-delivery windows are tolerated
  (best-effort, tenet 6).

---

## 5. Mailbox protocol

All ops run over an authenticated iroh connection to the relay. **Auth is implicit:**
the connection proves the peer's key, so the relay knows who it is talking to — no
separate signed challenge needed. (This refines SPEC §5.3's "signed challenge," which
is the stateless/HTTP fallback.)

| Op | Who | Auth | Effect |
|---|---|---|---|
| `register(push_sub?)` | a device, for its own mailbox | connection key = mailbox key | create/refresh mailbox; store push subscription |
| `deposit(envelope)` | any sender | connection key = sender key | store envelope, index under hosted recipients; forward-now or push |
| `fetch(since cursor)` | a device, own mailbox | connection key = mailbox key | return envelopes deposited after cursor |
| `ack(cursor \| [ids])` | a device, own mailbox | connection key = mailbox key | drop delivered envelopes; advance cursor |

- **Cursor:** a relay-local monotonic deposit index per mailbox. Purely a "what have I
  drained" marker; real ordering is the DAG, client-side.
- **Idempotency:** deposits dedup by message id, so sender retries are safe.
- **Deposit gating ⚠️:** this is *not* a privacy fork. The relay already reads
  `sender` and `recipients` from the **plaintext core** (it needs `recipients` to
  route), so relay-side gating leaks ~nothing extra — "authenticate the sender" and
  "know the graph" are the same coin, already spent. So the choice is just: **no
  gating** (simplest; fine at friends-scale — only contacts hold your key) vs.
  **relay-side gating** (rejects non-contacts before storage/push — worth it mainly to
  stop *push/storage* spam). *Lean: no gating for MVP;* add **capability-gating** (a
  token you issued, checked without a relay-held allowlist) when spam is real.

---

## 6. Push flow (Web Push / VAPID)

The fiddly part. Standard Web Push, used privacy-preservingly:

1. **Subscribe:** the PWA's service worker subscribes via the Push API → a
   `PushSubscription` (endpoint URL at the browser vendor's push service + `p256dh` +
   `auth`). The client `register`s this with **each** of its relays.
2. **Wake:** on a deposit with no live connection, the relay POSTs to the endpoint a
   payload encrypted per **RFC 8291** (so the push service can't read it) and signed
   with the relay's **VAPID** key (RFC 8292).
3. **Content-free:** the relay only has ciphertext, so the push says at most "you have
   N new messages" — never content. The SW wakes, connects, `fetch`es, decrypts
   **locally**, then upgrades the notification with the real sender/preview (the Signal
   pattern).
4. **Lifecycle:** subscriptions rotate; the SW handles `pushsubscriptionchange` and
   re-`register`s the new sub with all relays.

**Decryption fidelity ⚠️:**
- *Simple (MVP default):* push shows a generic "New message"; fetch/decrypt happens
  when the app is opened. No private keys in the service worker.
- *Rich:* the SW decrypts to show sender + preview immediately — requires the device
  private key be reachable from the SW (IndexedDB keystore). Better UX, more surface.
- **Lean: generic first, rich later.**

**iOS caveat ⚠️ (biggest practical risk):** iOS Web Push works only for a PWA
**installed to the home screen** (iOS 16.4+), not in a Safari tab, with some
reliability quirks. Viable for a daily-use installed app, but may justify a thin native
wrapper (Capacitor/Tauri) for iOS push specifically. Relevant since the primary users
are likely on iPhones.

---

## 7. Blobs over the mailbox

- Body is content-addressed and encrypted once; only the sealed content-key is
  per-recipient (SPEC §7). The **blob itself** must sit where each recipient can fetch
  it while the sender is offline.
- The uploader pushes each blob to the **blob cache of each distinct recipient-relay**
  (once per relay, not per device). Recipients fetch by hash; the relay serves from
  cache (TTL / size cap / LRU). Fetch of an already-held blob is deduped by hash.
- Thumbnails travel the same way; the small thumb makes previews instant before the
  full-res pull.

---

## 8. Retention, acks, receipts

- Per-device mailbox drops an envelope once **that device acks**, or on **TTL backstop**
  (e.g. 30 days) if it never returns. Each device's copy is independent.
- Blob cache: TTL + size cap, LRU eviction.
- **Delivery/read receipts are app-level messages** (a tiny fan-out message back), not
  relay features — keeps receipts E2E and the relay dumb.
- On foreground/startup a client always `fetch`es to cover dropped pushes (belt & suspenders).

---

## 9. What the relay learns (metadata)

- Which device keys have mailboxes (`register`).
- Per deposit: `sender` and `recipients` (both plaintext in the message core), sizes,
  timing — independent of how the deposit is authenticated.
- Via the push service: that a device was woken, and when.

Never: message content, identity clustering, or ordering. Running your own relay is the
mitigation; the protocol assumes the relay is untrusted regardless.

---

## 10. Open decisions

1. **Deposit gating** — none (caps + client filter) vs. relay-side gating (blocks
   push/storage spam). Not a privacy fork — the relay reads sender/recipients from the
   plaintext core regardless. *Lean: none for MVP; capability-gating when spam is real.*
2. **Push fidelity** — generic wake vs. SW-decrypts-rich-notification. *Lean: generic first.*
3. **iOS strategy** — accept installed-PWA push limits, or plan a thin native wrapper
   for iOS. *Needs your call.*
4. **Relay redundancy** — deposit to one relay (cheap) or all listed (robust)? *Lean:
   sender's choice; receiver dedups either way.*
5. **Mailbox transport** — mailbox ops as an iroh ALPN (auth for free) vs. HTTP
   (needs explicit signed requests). *Lean: iroh ALPN.*

---

## 11. Spec refinements this implies

- SPEC §5.3: mailbox auth is **implicit in the iroh connection identity**; the
  "signed challenge" is only the stateless fallback.
- SPEC §4.1: note the envelope is the deposit unit and relays index it per recipient.
- SPEC §3.6: ContactRecord `relays` is the rendezvous anchor; add the freshness/lazy-
  propagation note.
