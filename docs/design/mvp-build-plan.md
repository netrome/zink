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
crates/zink-relay      # bin: iroh relay + mailbox ALPN + blob cache. Ports+adapters.
crates/zink-cli        # bin: native dev/test client (not shipped) — drives the relay.
crates/zink-client     # client core lib shared by CLI + app (C1); also builds to WASM
                       # (A6 spike — groundwork for the post-MVP PWA client)
app/                   # Tauri v2 phone/desktop app (excluded from workspace: desktop
                       # builds need system webkit2gtk; Android goes via `cargo tauri`)
web/                   # browser spike page (A6) — post-MVP PWA groundwork
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
- [x] **A6 · 🎯🚩 Browser→relay spike.** A near-empty WASM client that
  opens a browser→relay connection on the mailbox transport and round-trips one frame;
  also confirm **iroh-blobs compiles for WASM**. *Done when:* a browser round-trips a
  frame against the relay. **Converts the plan's biggest unknown into a known before
  Stage B commits to the transport/blob shape.** If iroh-in-WASM/ALPN doesn't hold, fall
  back to WebSocket + signed-challenge auth (already spec'd, SPEC §5.3) and serve blobs
  over the mailbox ALPN. *(Risk spike: iroh-in-WASM.)*
  ✅ *(2026-07-10: browser registered a mailbox through the iroh-relay ws path — no
  fallback needed. iroh + iroh-blobs both compile for wasm32 with
  `default-features = false`. Caveats: iroh-blobs is at 0.103 (pre-1.0, API may move);
  browser needs the relay co-located iroh-relay server (`examples/browser_spike.rs` is
  the dev-mode preview; production shape + TLS lands in C0/C1).)*

## Stage B — Phase 0 completeness (native, via CLI)

- [x] **B1 · Message DAG & ordering.** 🎯 Genesis rules; parents/heads; conversation id;
  a client-side DAG store; `logical`/`seq`; linearization. *Done when:* ordering tests
  pass — concurrent → deterministic order, partial-view linearization, `seq` gap detection.
  *(Design: [dag-store.md](./dag-store.md). CLI threading waits for persistence, B5.)*
- [x] **B2 · Fan-out & multi-relay.** Resolve recipients → distinct relays → deposit the
  envelope once per relay; relay indexes per recipient device-key; receiver dedups by id.
  *Done when:* 1→N delivery test and cross-relay dedup test pass.
- [x] **B3 · Blobs / images.** iroh-blobs, or blobs over the mailbox ALPN per A6's
  outcome; encrypt-once blob + sealed content-key + `key-commit` in
  the envelope; thumbnail + full-res; relay blob cache (TTL/size). *Done when:* CLI sends
  an image, recipient fetches + decrypts both blobs (commitment checked); refetch deduped
  by hash. *(Went with iroh-blobs (push enabled via event mask). Blob-cache TTL/size
  eviction deferred to B4 retention. iroh-blobs 0.103 caveats: push completion has no
  in-band ack — confirmed via an Observe round-trip, whose stream sends diffs that must
  be accumulated; the provider's push/observe gating reads `mask.get` upstream.)*
- [x] **B4 · Reliability.** Deposit ack + idempotent retry (by id); fetch cursor; ack/
  delete + TTL retention backstop. *Done when:* retry-idempotency and retention/expiry
  tests pass. *(Also the blob-cache TTL eviction deferred from B3: pushed blobs are
  tracked and protected for a TTL; iroh-blobs GC collects the rest. Defaults: 30-day
  mailbox retention and blob TTL, hourly GC.)*
- [x] **B5 · Persistence.** Relay mailbox + blob cache on-disk (behind a port); client
  DAG + keystore persisted. *Done when:* messages/keys survive a restart.
  *(Retention carry-over from B4: persisted timestamps must be wall-clock — `Instant`
  doesn't serialize. Blob retention should move off the in-memory push-time registry
  onto iroh-blobs' persisted **tags** (timestamped tag per push; evict = delete old
  tags, GC collects) — else a restart leaves persisted blobs unprotected and the first
  GC run wipes the cache.)*
  ✅ *(FsMailboxStore + FsStore blob cache with tag-based retention, both under
  `zink-relay [data-dir]`; the relay's own endpoint key persists too (`relay.key`) so
  dial strings stay valid across restarts. Client: `<key-file>.state/` holds envelopes
  per conversation + a participants→conversation index; `send` threads drafts from the
  stored DAG — one conversation per participant set is CLI policy, not protocol.)*

## Stage C — Phone client (native, Tauri v2)

> **Client-stance pivot (2026-07-11, resolved — SPEC §11 updated):** MVP client =
> **native Android + Linux desktop (Tauri v2, Leptos UI)** instead of PWA/WASM,
> verified by the C-spike below. The browser platform carried the MVP's hardest costs
> (Web Push, evictable IndexedDB keystore, TLS/VAPID ops) and denies true p2p; native
> replaces them with persistent-connection delivery ("forward-now"), a filesystem
> keystore reusing the B5 client work, and direct `id@ip:port` dialing. **The PWA
> becomes the post-MVP second client** — the cross-implementation proof; its
> groundwork (A6, `crates/zink-client` WASM spike, `web/spike`) stays in-tree.

- [x] **C-spike · 🎯🚩 Native client spike (Android).** The native sibling of A6:
  Tauri v2 scaffold; cross-compile `zink-protocol` + iroh for `aarch64-linux-android`;
  a hello-world app on a real phone registers a mailbox against the deployed relay.
  *Done when:* the phone shows a successful register round-trip.
  ✅ *(2026-07-11: APK built on the first attempt; phone registered over native QUIC.
  iroh + ring cross-compile cleanly. Scaffold lives in `app/`; build gotchas —
  debuginfo-bloated debug APKs, Gradle in-place repackaging — documented in
  DEV-SETUP.md.)*
- [x] **C0 · Relay deployment & caps.** Run the relay as an unattended service on the
  public server (stable port, persistent data dir, restarts on boot). **Minimal abuse
  caps**: max blob push size and a per-mailbox item cap — SPEC §8 claims "relay
  rate/size caps" as the MVP anti-spam. No TLS/domain/VAPID needed (native clients
  dial `id@ip:port` directly). *Done when:* the relay survives a server reboot
  unattended and an oversized blob push is bounded.
  ✅ *(2026-07-11: systemd user unit (`deploy/zink-relay.service`, DEV-SETUP §5) on
  stable port 4400; restart-verified with the same dial string. Caps: 1024
  items/mailbox (full = best-effort skip), 64 MiB max blob — oversized pushes are
  **evicted on the next sweep** rather than rejected, since iroh-blobs 0.103 has no
  mid-stream rejection hook; exposure is bounded by the GC interval. Reboot autostart
  needs `loginctl enable-linger` (one sudo).)*
- [x] **C1 · Client core (`zink-client`).** 🎯 Lift the client logic from `zink-cli`
  into `zink-client` as a native lib shared by CLI and app: keystore, conversation
  state + DAG threading, send/recv/fan-out, blob fetch. The app gets a persistent
  device key in its data dir. *Done when:* the CLI runs on `zink-client` with all
  existing e2e tests green, and the app sends + receives a text via Tauri commands.
  *(Design: [client-core.md](./client-core.md). ✅ 2026-07-11: phone ↔ CLI chat worked
  live — two client implementations threading one conversation. No tokio in the lib;
  the A6 WASM spike moved to a wasm-gated module and still builds.)*
- [x] **C2 · Contacts & QR.** ContactRecord (SPEC §3.6): generate + display your QR
  (keys, self-attestations, relays); scan a contact's (tauri barcode-scanner plugin);
  contact store; send-by-name. *Done when:* two phones exchange QRs and message each
  other by contact name.
  ✅ *(2026-07-11: verified live — QR displayed on one device, camera-scanned by
  another, plus paste fallback and rename. Record payload = `ZINK:<base32(borsh)>`
  (QR alphanumeric mode); only verified *self-issued* name attestations are trusted;
  petnames are client policy with collision rejection; publishing a record registers
  its mailboxes (kills the register-before-first-deposit footgun); CLI renders
  terminal QRs via `my-record --qr`. **C3 follow-up:** the scanner view has no
  cancel/back affordance if no QR is in sight.)*
- [ ] **C3 · Messaging UI (Leptos).** Conversation list; message view (linearized
  DAG); send text; send image (client-side thumbnail + full-res); scanner
  cancel/back affordance (C2 footgun). *Done when:* usable text + image chat between
  two phones.
  *(Fold in from the A1–C2 review: (a) render conversation views from the **stored
  DAG**, not from `recv`'s return value — dissolves the per-call dedup re-surfacing;
  (b) a **process-wide/managed `Client`** instead of one-per-Tauri-command, closing
  the concurrent-state-dir and double-first-run-key races [review MEDIUM-4].)*
- [ ] **C4 · 🎯🚩 Live delivery & notifications.** Relay **forward-now** over the live
  connection (rendezvous doc §3 — specified, never implemented); the app holds a
  persistent connection via an Android foreground service; local notification on
  arrival (tauri-plugin-notification); fetch-on-foreground stays the backstop.
  *Done when:* a backgrounded app on a real phone shows a notification for an
  incoming message. *(Risk spike: background delivery vs Android Doze/battery
  optimization — the successor to the retired Web Push spike.)*
  *(Fold in from the A1–C2 review [MEDIUM-3]: **send is store-first with no
  re-deposit path** — a failed deposit leaves a phantom message in the local DAG and
  a permanent seq gap. Fix here where delivery state gets a home: store on first
  successful deposit, or track per-relay delivery so a later pass re-deposits.)*

**🎉 MVP-usable milestone: end of Stage C** — text + images between friends on Android
(+ Linux desktop), online and offline, with notifications.

### Hardening pass (2026-07-11, post-C2 independent review)

Two fresh-eyes reviews (one via subagent, one external) audited A1–C2. Core clean:
invariants held, crypto/commitment/signature paths tested against attacks,
content-addressing pinned, no panics on hostile input. Fixed in this pass:
- **fs mailbox cursor reset after a full drain** (data loss) — persistent per-mailbox
  high-water counter; regression test `append__cursor_should_not_reset_after_a_full_drain`.
- **unpaginated fetch** (a >16 MiB mailbox was undrainable) — relay pages responses
  (`MAX_FETCH_PAGE_BYTES`), client loops until empty; test + wire-doc update.
- swallowed tag-set after blob push (silent blob loss to GC) — now logged.
- key files written `0600`; zeroize on the crypto error path; recv skips
  unsupported-version envelopes (SPEC §10).
Deferred with homes above: MEDIUM-3 → C4, MEDIUM-4 + render-from-DAG → C3. Also noted:
`zink-client` has no unit tests of its own (only e2e coverage); `String` errors will
want structured variants once the UI branches on failure kind; contact identity keyed
on `keys.first()` needs revisiting at D2.

## Stage D — Identity & social layer (SPEC phases 1–3, post-Stage-C)

- [ ] **D0 · Sync primitives.** `get` / `get-successors` (SPEC §5.2) over a peer ALPN,
  served at each peer's discretion. Fixes the known late-joiner hole (a client without a
  conversation's genesis cannot reply — noted in B5); prerequisite for D2 backfill and
  D4's backlog serving.
- [ ] **D1 · Attestations & name resolution.** Self-profile (name/avatar); client-side
  petnames; `who-is-this` pull; client-side trust ranking.
- [ ] **D2 · Multi-device.** QR pairing (mutual `same-person-as`); device set in
  resolution; history backfill via content-key re-wrap. *(Review note: contact
  identity is currently keyed on `record.keys.first()` — revisit so a re-scanned
  record with reordered/added device keys isn't treated as a different contact.)*
- [ ] **D3 · Groups.** Multi-recipient conversations in the UI (delivery is already
  fan-out; this is mostly membership *presentation* — client UX).
- [ ] **D4 · Web-of-trust.** Third-party profile attestations; "who is this?" answers
  from contacts; concurrency-aware message views.

---

## Notes

- **Risk spikes** (🎯 with *Risk spike*) are integration unknowns paper can't resolve —
  A4 (custom ALPN) ✅, A6 (iroh WASM) ✅, C-spike (iroh-on-Android + Tauri mobile) ✅,
  C4 (background delivery vs Android Doze — replaced the retired Web Push spike).
  Expect to learn by building; keep them small and isolated.
- **Just-in-time design docs** (🎯): A4 mailbox wire messages ✅, B1 DAG store ✅,
  C1 client-core split, C4 live delivery / foreground service. Write these as short
  `docs/design/<name>.md` when we reach them.
- **Async ports, sync core.** Ports are async traits from A4 onward; the pure
  `zink-protocol` core stays synchronous (no async runtime, no threads) so it ports to
  single-threaded WASM cleanly. This keeps Stage C a re-plumbing, not a rewrite.
- Stage D maps to SPEC §12 phases 1–3 and is intentionally coarse; we'll slice it
  finer when Stage C lands.
