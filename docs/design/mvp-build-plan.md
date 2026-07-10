# MVP Build Plan

The slice checklist and shared task tracker for reaching a working MVP. Downstream of
[SPEC.md](../SPEC.md); governed by [../../AGENTS.md](../../AGENTS.md).

**How to use this doc.** We build in small vertical slices, native-first, walking-
skeleton first. Each slice ends in something *runnable* with focused tests. Check items
off as they land; add follow-ups inline. Write a short `docs/design/<name>.md` only for
a slice with genuine unresolved design (flagged 🎯 below).

**Definition of done (every slice):** runnable / tests pass · `cargo fmt` + `clippy`
clean · `mvp-build-plan.md` updated · docs updated if behavior changed.

**Workspace shape (target):**
```
crates/zink-protocol   # pure core: types, BORSH, hashing, DAG, crypto. No I/O.
crates/zink-relay      # bin: iroh relay + mailbox ALPN + push + blob cache. Ports+adapters.
crates/zink-cli        # bin: native dev/test client (not shipped) — drives the relay.
crates/zink-client     # (Stage C) lib compiled to WASM for the PWA
web/                   # (Stage C) PWA assets + service worker
```

---

## Stage A — Foundation & walking skeleton (native)

- [x] **A1 · Workspace scaffold.** Cargo workspace with `zink-protocol`, `zink-relay`,
  `zink-cli` (empty-ish). *Done when:* `cargo build`, `cargo test`, `clippy` all pass.
- [x] **A2 · Protocol core: keys, types, hashing.** Ed25519 keypair; `MessageCore` /
  `MessageEnvelope` + `Attestation` types; canonical BORSH encode/decode; message id =
  `BLAKE3(borsh(core))`; sign/verify. *Done when:* round-trip, **determinism** (same
  value → same bytes → same id), and signature-verify tests pass. Pure, no I/O.
- [x] **A3 · Envelope encryption.** Random per-message content-key (AEAD) encrypts the
  body once; seal the content-key per recipient (X25519 via a **vetted** Ed25519→X25519
  conversion); open. *Done when:* encrypt→seal→open→decrypt round-trips for N recipients;
  **`key-commit` verified before trusting** (commitment mismatch rejects); wrong key
  fails; malformed input returns an error (never panics).
- [x] **A4 · Relay mailbox + ALPN (in-memory).** 🎯 iroh endpoint with a custom ALPN;
  `register` / `deposit` / `fetch` / `ack` over the authenticated connection (auth =
  connection key). In-memory store. Define the mailbox ops **transport-agnostically** (so
  a WebSocket fallback doesn't ripple into Stage B) and the ports as **async traits**.
  *Done when:* an integration test deposits from one endpoint and fetches from another.
  *(Risk spike: custom-ALPN handling in iroh 1.0.)*
- [x] **A5 · 🚩 WALKING SKELETON.** `zink-cli` send/recv through the relay: A encrypts +
  deposits an envelope for B's key; B fetches + opens + prints plaintext. *Done when:* a
  manual run works **and** an automated test spins up relay + two clients end-to-end.
  **This is the milestone — the spine works.** ✅ *(2026-07-10: manual run + automated
  `walking_skeleton` test both green.)*
- [ ] **A6 · 🎯🚩 Browser→relay spike.** A near-empty WASM client that
  opens a browser→relay connection on the mailbox transport and round-trips one frame;
  also confirm **iroh-blobs compiles for WASM**. *Done when:* a browser round-trips a
  frame against the relay. **Converts the plan's biggest unknown into a known before
  Stage B commits to the transport/blob shape.** If iroh-in-WASM/ALPN doesn't hold, fall
  back to WebSocket + signed-challenge auth (already spec'd, SPEC §5.3) and serve blobs
  over the mailbox ALPN. *(Risk spike: iroh-in-WASM.)*

## Stage B — Phase 0 completeness (native, via CLI)

- [ ] **B1 · Message DAG & ordering.** 🎯 Genesis rules; parents/heads; conversation id;
  a client-side DAG store; `logical`/`seq`; linearization. *Done when:* ordering tests
  pass — concurrent → deterministic order, partial-view linearization, `seq` gap detection.
- [ ] **B2 · Fan-out & multi-relay.** Resolve recipients → distinct relays → deposit the
  envelope once per relay; relay indexes per recipient device-key; receiver dedups by id.
  *Done when:* 1→N delivery test and cross-relay dedup test pass.
- [ ] **B3 · Blobs / images.** iroh-blobs, or blobs over the mailbox ALPN per A6's
  outcome; encrypt-once blob + sealed content-key + `key-commit` in
  the envelope; thumbnail + full-res; relay blob cache (TTL/size). *Done when:* CLI sends
  an image, recipient fetches + decrypts both blobs (commitment checked); refetch deduped
  by hash.
- [ ] **B4 · Reliability.** Deposit ack + idempotent retry (by id); fetch cursor; ack/
  delete + TTL retention backstop. *Done when:* retry-idempotency and retention/expiry
  tests pass.
- [ ] **B5 · Persistence.** Relay mailbox + blob cache on-disk (behind a port); client
  DAG + keystore persisted. *Done when:* messages/keys survive a restart.

## Stage C — PWA client (WASM)

- [ ] **C0 · Ops prerequisites.** Public relay with a domain + TLS; a VAPID keypair;
  relay outbound HTTPS to browser push services. *Done when:* the relay is reachable from
  a browser over TLS and can send a test Web Push. (Needed before C1/C4 can be tested at all.)
- [ ] **C1 · WASM + browser→relay.** 🎯 Build `zink-protocol`/`zink-client` to WASM
  (`iroh` `default-features = false`); connect browser→relay over WebSocket; fetch a
  message. *Done when:* a browser fetches a message **deposited by `zink-cli`** — proving
  cross-implementation interop. (The iroh-in-WASM unknown is retired in A6; this slice is
  integration work.)
- [ ] **C2 · Client core in-browser.** Keystore (IndexedDB); ContactRecord generate +
  QR scan; DAG store (IndexedDB); fan-out send + mailbox drain. *Done when:* two browser
  instances hold a 1:1 conversation via the relay.
- [ ] **C3 · Minimal UI.** Conversation list; message view; send text; send image
  (thumbnail preview → full). *Done when:* usable text + image chat between two browsers.
- [ ] **C4 · 🎯🚩 Push (isolated).** Service worker + Web Push subscription; relay VAPID
  sender; content-free push → SW wakes → fetch → generic notification. *Done when:* a
  backgrounded PWA on **Android** receives a push showing "New message"; opening shows
  content. *(Risk spike: PWA Web Push — quarantine this slice.)*

**🎉 MVP-usable milestone: end of Stage C** — text + images between friends on Android,
online and offline, with notifications.

## Stage D — Identity & social layer (SPEC phases 1–3, post-Stage-C)

- [ ] **D1 · Attestations & name resolution.** Self-profile (name/avatar); client-side
  petnames; `who-is-this` pull; client-side trust ranking.
- [ ] **D2 · Multi-device.** QR pairing (mutual `same-person-as`); device set in
  resolution; history backfill via content-key re-wrap.
- [ ] **D3 · Groups.** Multi-recipient conversations in the UI (delivery is already
  fan-out; this is mostly membership *presentation* — client UX).
- [ ] **D4 · Web-of-trust.** Third-party profile attestations; "who is this?" answers
  from contacts; concurrency-aware message views.

---

## Notes

- **Risk spikes** (🎯 with *Risk spike*) are integration unknowns paper can't resolve —
  A4 (custom ALPN), A6 (iroh WASM), C4 (push). Expect to learn by building; keep them
  small and isolated.
- **Just-in-time design docs** (🎯): A4 mailbox wire messages, B1 DAG store, C1 WASM
  integration, C4 push. Write these as short `docs/design/<name>.md` when we reach them.
- **Async ports, sync core.** Ports are async traits from A4 onward; the pure
  `zink-protocol` core stays synchronous (no async runtime, no threads) so it ports to
  single-threaded WASM cleanly. This keeps Stage C a re-plumbing, not a rewrite.
- Stage D maps to SPEC §12 phases 1–3 and is intentionally coarse; we'll slice it
  finer when Stage C lands.
