# Design: Live Delivery, Outbox & Notifications (C4)

The just-in-time design for slice C4: relay **forward-now** (rendezvous doc §3 —
specified, never implemented), the **outbox** (the store-first send hole found in
the A1–C2 review), the app's **persistent connection** (Android foreground
service), and **local notifications**. Downstream of
[mailbox-rendezvous-push.md](./mailbox-rendezvous-push.md) and
[mailbox-wire-protocol.md](./mailbox-wire-protocol.md).

Status: **draft for discussion.** ⚠️ marks open decisions.

---

## 1. Goals & non-goals

**Goals:** a backgrounded phone shows a notification for an incoming message
within seconds; a send that fails mid-fan-out is eventually delivered without
user action; the UI never presents an undelivered message as delivered.

**Non-goals:** delivery/read *receipts* (app-level messages, rendezvous doc §8);
peer-to-peer sync (D0); relay-initiated anything toward parties without a live
connection (that's the mailbox's job); any push service (retired with the PWA
pivot — content-free-push constraints stay honored trivially, since no third
party is involved at all).

---

## 2. The outbox (review MEDIUM-3: store-first send has no re-deposit path)

**Problem.** `finish_send` stores the envelope locally, then deposits per relay
and pushes blobs. Any failure after the store leaves a *phantom*: the sender's
DAG (and UI) shows the message as sent, later sends thread on top of it, and
recipients get a permanent missing-parent/seq gap — nothing ever retries.
Partial variants: deposited to relay 1 but not relay 2; deposit ok but blob push
failed. D0 sync does not fix this (it needs both peers online at once; the
mailbox exists because that can't be assumed).

**Fix: keep store-first, add a delivery ledger.**

- On send, *before* any network work, persist an outbox entry per distinct
  relay: `outbox/<relay-fingerprint>/<msg-id>` under the client state dir,
  holding the relay dial string + whether blobs are still owed. (The envelope
  itself is already stored under its conversation; the entry is a pointer,
  not a copy. Blob ciphertext is already in the local blob cache — C3a — so
  a later blob push re-stages from there.)
- Deposit + blob push per relay exactly as today; on success, delete that
  relay's entry. A send that fully succeeds touches the outbox only twice
  (create, delete) — the common case stays cheap.
- **Flush pass** (idempotent — deposits dedup by id, blob pushes by hash):
  walk the outbox, retry each entry. Triggered on: client open, before every
  send, after every `recv`, and on every reconnect of the live connection
  (§4). No timer of its own — those hooks fire often enough at MVP scale.
- **Surfacing:** `history()`/`Message` gain a `pending` flag (outbox entry
  exists for the message on ≥1 relay). The UI renders it (clock/`…` cue) —
  client policy. `send` still returns an error when *zero* relays took the
  deposit, but the message stays stored + outboxed either way: the send is
  honest ("queued, not delivered"), not rolled back.
- **Give-up policy — resolved (2026-07-12):** entries older than the relay
  retention window (30 days) are dead — the recipients' cursors moved on and
  the social moment passed. Keep the entry, stop retrying, surface as
  "undelivered" in history; deleting a message the user wrote is not the
  client's call (tenet: discretion over enforcement).

Testable headless end-to-end: send while the relay is down (CLI: deposit fails,
outbox entry persists, `history` shows pending) → start the relay → any flush
trigger delivers → recipient fetches, sender's pending flag clears.

---

## 3. Forward-now: nudge-and-fetch, not envelope push

The rendezvous doc's diagram says "forward now over live conn". Two shapes:

**(a) Envelope push** — relay writes the full envelope down a relay→client
stream on deposit. Needs new wire framing (today's protocol is strictly
one FIN-framed request per client-opened bi-stream), plus bookkeeping so the
pushed copy and a concurrent `fetch` don't double-deliver, plus its own
ack semantics.

**(b) Nudge-and-fetch (resolved, 2026-07-12)** — on deposit to a mailbox with a live
connection, the relay opens a **uni stream** to that connection and closes it
(zero payload — the stream *is* the signal). The client reacts by running its
normal `fetch`/`ack` drain.

*Why (b):* it reuses everything that already works — pagination, cursor acks,
cross-relay dedup, the hostile-relay guards — and adds no new framing, no new
message type, and no second delivery path to reconcile. Cost: one extra RTT
per delivery (~tens of ms) — irrelevant at human scale. A malicious relay can
at worst nudge spuriously (= a fetch finding nothing, same as the existing
poll). If a payload-carrying push is ever wanted, it's an additive upgrade on
the same subscription connection. *(Nothing is deployed yet, so breaking wire
compatibility was on the table — but the nudge is naturally additive, so the
freedom buys nothing here.)*

**Wire/compat:** additive to `zink-mailbox/1` — documented in
mailbox-wire-protocol.md as: *"the relay MAY open zero-length uni streams on a
connection whose peer holds a registered mailbox; clients SHOULD treat one as a
fetch hint; clients that don't accept uni streams are unaffected."* No version
bump: old clients never call `accept_uni`, and unread streams on a QUIC
connection cost nothing. `Register` stays the subscription act — a connection
that registered and stays open *is* "live" (no new subscribe op).

**Relay side:** an in-memory map `mailbox key → live connection` maintained by
the accept loop (insert on `register`, drop on connection close; a newer
connection for the same key replaces the older). On `deposit`, nudge each
hosted recipient's live connection, best-effort — a failed nudge is fine, the
mailbox still holds the envelope and fetch-on-foreground remains the backstop.
No persistence, no retry, no queue.

---

## 4. The client connection lifecycle

One long-lived task per home relay (the "subscription loop"):

```
connect → register → flush outbox → drain (fetch/ack) → loop {
    await (uni-stream nudge | keepalive-tick | connection-close)
    on nudge:  drain; hand new messages to the edge (notify + re-render)
    on close:  reconnect with jittered exponential backoff (cap ~1 min), re-register
}
```

- Lives in `zink-client` as an async fn (`Client::subscription_loop(relay, on_new)`
  or similar) — **no runtime dependency**, per client-core.md: the edge spawns
  it (Tauri: `tauri::async_runtime`; CLI: tokio in the bin; the future
  foreground service: its own runtime handle).
- Reconnect covers network switches (wifi→cellular) — QUIC connection dies,
  loop redials. Every reconnect re-registers (refreshing liveness on the
  relay) and flushes the outbox (§2) — the two failure recoveries share one
  hook.
- Keepalive ⚠️: the QUIC idle timeout must be outlived or NAT bindings decay.
  iroh has endpoint keepalive config; the interval is a battery/traffic
  tradeoff measured in the C4-spike (below), not chosen on paper.
- The existing foreground poll (C3b, 7 s) stays as the backstop but stretches
  (~60 s ⚠️) once a subscription is healthy — belt & suspenders, per the
  rendezvous doc §8.

---

## 5. Android: foreground service + Doze (the risk spike)

The reason C4 is flagged 🚩. Unknowns paper can't resolve:

- **FGS mechanics.** Tauri v2 has no first-party foreground-service plugin; we
  write a minimal one. **The Kotlin is a shell, not a participant:** Android
  instantiates a `Service` class from the manifest via the JVM runtime — no
  Rust path to *being* that component — but the service's only job is to
  exist, because its existence keeps our **process** (where the Rust
  subscription loop already runs) alive when the Activity/webview is
  backgrounded. `DeliveryService.kt` ≈ 50–60 lines (notification channel +
  "zink is connected" notification + `startForeground` + `START_STICKY`), a
  manifest entry, and a two-command plugin bridge (start/stop) invoked from
  Rust. No message, key, or socket ever crosses the Kotlin boundary; message
  notifications go through the existing `tauri-plugin-notification`. No new
  toolchain — Gradle already compiles ~10 Kotlin files (Tauri's shell, the
  barcode-scanner plugin) on every APK build. API 34+ constrains FGS types ⚠️:
  `dataSync` now carries runtime limits; `specialUse` needs a manifest
  declaration; `connectedDevice` doesn't fit. *Lean:* `specialUse` +
  requesting the battery-optimization exemption (the Signal/Molly pattern for
  connection-based delivery without FCM).
  **Known limit (accepted for MVP):** under memory pressure Android can kill
  even an FGS process; `START_STICKY` revives the service but not Tauri's
  Rust init, so delivery pauses until the app is next opened (the
  fetch-on-open backstop covers it). The overnight spike measures how rare
  this is with the exemption granted.
- **Doze behavior.** Even with an FGS, Doze windows may freeze the socket.
  Acceptable if delivery resumes on maintenance windows / screen-on;
  unacceptable if the connection silently zombifies (keepalive claims alive,
  nudges never arrive). Measure, don't guess.
- **Spike (C4c gate):** minimal FGS holding one iroh connection overnight on a
  real phone; log every nudge→drain latency and battery drain. *Done when:* a
  message sent at hour N (screen off, unplugged) notifies within minutes, and
  overnight drain is single-digit %.

Desktop needs none of this: the subscription loop runs while the app runs.

**Notifications:** on drained new messages while backgrounded, the edge posts a
local notification via `tauri-plugin-notification` — after local decrypt, so
sender petname + preview are available. Content policy is client policy —
**resolved (2026-07-12): petname + text preview**, since the notification
never leaves the device (no third party anywhere in this path).

---

## 6. Slicing

*(Tracked as C4a/C4b/C4c in [mvp-build-plan.md](./mvp-build-plan.md) — this
section records the rationale for the cut, the plan tracks progress.)*

- **C4a · Outbox.** §2 complete, CLI-testable e2e (relay down → pending →
  relay up → flush → delivered; blob-owed variant). `Message.pending` in the
  app UI.
- **C4b · Nudge + subscription loop.** §3 + §4; relay nudges on deposit;
  desktop app delivers live (no poll wait); e2e test with an in-process relay:
  deposit from A → B's subscription drains without polling.
- **C4c · Foreground service + notifications.** §5 spike, then the plugin +
  notification wiring. *Done when* the plan's C4 criterion passes on a real
  phone.

## 7. Doc touchpoints when this lands

- mailbox-wire-protocol.md: the nudge stream (additive, §3 wording above).
- mailbox-rendezvous-push.md §3: "forward now" → "nudge-and-fetch" note.
- client-core.md: subscription loop + outbox in the API sketch.
- SPEC §5.3 already says wake-on-message via persistent connection — no change.
