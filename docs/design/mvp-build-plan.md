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
- [x] **C3 · Messaging UI (Leptos).** Split into three runnable sub-slices below.
  *Done when:* usable text + image chat between two phones.
  ✅ *(2026-07-12: verified live phone ↔ laptop — text + images both ways, both
  MVP platforms covered.)*
  *(Decision, 2026-07-12 — **self-wrap convention**: `seal` always adds a key-wrap
  for the sender's own key, *without* listing self in `core.recipients` or
  depositing to self. Senders can then reopen their own stored envelopes, so
  history renders from the stored DAG with everything ciphertext-at-rest. Wraps
  live outside the hashed core: ids unchanged, no version bump, recipients
  unaffected. A client convention, not protocol — a client that skips it only
  loses its own history; full send-to-self (deposit to own mailbox) is the D2
  multi-device extension of the same idea. Record in SPEC §6 when it lands.)*
- [x] **C3a · Client-core groundwork (no UI).** Self-wrap in `seal`; conversation
  enumeration + history API on `Client` (linearized, opened bodies); encrypted
  blob cache in `ClientState` (`blobs/<hash>`, ciphertext at rest — without it the
  relay's 30-day TTL silently eats images, and every view costs a round-trip);
  blob fetch for stored messages via *own home relays* (that's where senders push
  them); CLI `conversations` / `history` subcommands so it's all e2e-testable
  without phones. *Done when:* the CLI shows a threaded, decrypted two-sided
  history — including the device's own sent messages.
  ✅ *(2026-07-12: self-wrap recorded in SPEC §6; `conversations`/`history`/
  `fetch_stored_blob` on `Client` (client-core.md updated); own blobs cached at
  send time — the sender's local copy is the only one reachable for own history,
  since blobs are pushed to the recipients' relays. e2e: two-sided threaded
  history on both devices, and blobs still render *after the relay is gone* (cache
  proof). Envelopes stored before the self-wrap show as `<unopenable>` — honest,
  dev-stage data only.)*
- [x] **C3b · Managed client + structured commands + Leptos scaffold.** One
  long-lived `Client` in Tauri managed state (closes the concurrent-state-dir and
  double-first-run-key races found in the A1–C2 review); commands return
  structured DTOs rendered from the **stored DAG**, not `recv`'s return value
  (dissolves the per-call dedup re-surfacing; replaces `recv_texts`'s formatted
  strings); `app/ui/` Leptos CSR crate with a hand-rolled `invoke` shim (no
  `tauri-sys` dependency), built by `app/ui/build.sh` wired into
  `beforeDevCommand`/`beforeBuildCommand` (wasm-bindgen CLI, the proven §A6 flow
  — no trunk, one less tool); conversation list + message view + send text;
  refresh = on-load + button + coarse foreground poll (C4 replaces this with
  forward-now). Reply resolves participants → contact records for relays;
  unknown participant keys are skipped with a warning (client policy,
  best-effort). *Done when:* two Linux desktops chat through the deployed relay.
  *(2026-07-12: code complete — `reply_contacts`/`send_in` in `zink-client` with
  a CLI `reply` command e2e-testing the unknown-participant skip; `app/dto`
  crate = one set of command wire types both sides compile against; C2 flows
  (QR/scan/paste/petnames) ported into the Leptos contacts view. Verified with
  two desktop instances chatting through the deployed relay — note: two
  instances on *one machine* need distinct app identifiers (separate data
  dirs) and `--no-watch` on the first, else they fight over the state dir.)*
- [x] **C3c · Images + mobile polish.** Image pick → thumbnail via webview canvas
  (keeps the `image` crate off the Rust side); send full + thumb as the existing
  `BlobDraft` pair; render thumbnails, tap to fetch/decrypt full-res through the
  blob cache; scanner cancel/back affordance (C2 footgun); Android build + the
  two-phone acceptance run. *Done when:* C3's overall criterion.
  ✅ *(verified live phone ↔ laptop, 2026-07-12.)*
  *(2026-07-12: code complete — canvas downscale in `app/ui/src/image.rs`
  (thumb ≤320px, full ≤1600px, JPEG re-encode: bounded size whatever was
  picked); images ride the JSON IPC as base64 (`data-encoding`, already in the
  tree); thumbnails fetch lazily through the client blob cache, tap opens
  full-res in an overlay; scan now runs `windowed: true` with a cancel overlay
  (page transparent behind, `barcode-scanner:allow-cancel` was already
  granted). Known nit for later: a thumbnail whose fetch fails sticks on
  "loading…" — tap-to-retry is a cheap C4-adjacent polish.)*
- [ ] **C4 · 🎯🚩 Live delivery & notifications.** Split into three runnable
  sub-slices below. *Done when:* a backgrounded app on a real phone shows a
  notification for an incoming message. *(Design:
  [live-delivery.md](./live-delivery.md) — nudge-and-fetch, outbox, foreground
  service; decisions resolved 2026-07-12. Risk spike: background delivery vs
  Android Doze/battery optimization — the successor to the retired Web Push
  spike, isolated in C4c.)*
- [x] **C4a · Outbox.** The per-relay delivery ledger fixing the store-first
  send hole from the A1–C2 review (a failed deposit left a phantom message in
  the local DAG and a permanent seq gap for recipients): entry per
  (message, relay) persisted before any network work, cleared per relay on
  success (blob pushes owed tracked too); flush pass (idempotent re-deposit +
  re-push) before send / after recv / on reconnect (C4b); entries past the
  retention window stop retrying but stay surfaced as undelivered; `pending`
  flag on history messages, rendered in the UI. *Done when:* e2e — send with
  the relay down shows pending, relay back up + any flush trigger delivers,
  recipient gets it, pending clears.
  ✅ *(2026-07-12: `outbox/` ledger in the client state dir; one relay
  failing no longer aborts the rest of the fan-out
  (`SendReceipt.pending_relays`; send errors only when *zero* relays took
  it — "queued", not "lost"); blob re-push re-stages from the C3a cache.
  Flush-on-open dropped (network before first render) — recv-on-open covers
  it. Also: client `connect` now has a 10 s timeout, and an unreachable
  relay is no longer retried in-send at all (that's the outbox's job) — a
  down relay costs a send seconds, not minutes. e2e: queue→flush→deliver
  with blobs across a relay restart at the same dial string, plus the
  give-up window (aged entries skip retry, stay `[pending]`).)*
- [x] **C4b · Nudge + subscription loop.** Relay keeps a live-connection map
  per registered mailbox and, on deposit, opens a zero-length uni stream to
  each hosted recipient's connection (the nudge — additive to
  `zink-mailbox/1`, old clients unaffected); client subscription loop in
  `zink-client` (connect → register → flush outbox → drain → await nudge;
  jittered-backoff reconnect), spawned by the edges; the desktop app delivers
  live, and the foreground poll stretches to a backstop. *Done when:* e2e —
  a deposit from A drains at B's subscription without B polling.
  ✅ *(2026-07-12: live map is session-numbered so a stale connection's
  cleanup never evicts its replacement (tested); nudges are spawned +
  timeout-bounded so a peer that never accepts uni streams can't park the
  depositor's loop on exhausted stream credit. `Client::subscribe` per relay,
  spawned by the edge; CLI grew `listen` (the dev-tool sibling of the app's
  subscription tasks). App: `new-messages` Tauri event → webview re-renders
  from the store; poll stretched 7 s → 60 s backstop. e2e: a listener
  receives a pre-existing message via the catch-up drain, then a second
  message with *zero* client-side action — deposit → nudge → drain. Wire doc
  + rendezvous doc + client-core.md updated. Note: with multiple home
  relays, `on_new` can repeat a message another loop already delivered —
  storage dedups; C4c's notification path dedups by id.)*
- [ ] **C4c · 🚩 Foreground service + notifications.** The Doze risk spike,
  then the plumbing: minimal Kotlin FGS shell (`specialUse` type +
  battery-optimization exemption) whose only job is keeping the process — and
  the Rust subscription loop in it — alive while backgrounded; petname + text
  preview local notifications posted after local decrypt
  (tauri-plugin-notification). *Done when:* overnight on a real phone, screen
  off and unplugged, an incoming message notifies within minutes at
  single-digit battery drain — C4's overall criterion.
  *(2026-07-12: code complete — `DeliveryService.kt` (~45 lines, pure
  process-keeper, IMPORTANCE_MIN persistent notification) + manifest
  (`specialUse` + subtype property + FGS/notification/battery permissions);
  even simpler than designed: started from `MainActivity.onCreate`, so no
  Rust↔Kotlin bridge exists at all. Battery-exemption prompt on first
  launch; notification permission requested at startup (Android 13+).
  Message notifications: petname + 120-char preview after local decrypt,
  deduped by id, skipped while the window is focused; works on desktop too.
  APK builds. **Awaiting the overnight measurement** — screen off,
  unplugged, message at hour N notifies within minutes, single-digit drain;
  that run ticks this box, C4, and the MVP milestone. 🎉)*

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

- [ ] **D0 · Sync primitives.** 🎯 `get` / `get-successors` (SPEC §5.2) over a peer
  ALPN, served at each peer's discretion. Fixes the known late-joiner hole (a client
  without a conversation's genesis cannot reply — noted in B5); prerequisite for D2
  backfill and D4's backlog serving. *(The peer ALPN it stands up is also the substrate
  for D5 direct delivery.)* Design: [sync-primitives.md](./sync-primitives.md).
  - [x] **D0a · Serve + backward-fill.** `SYNC_ALPN` + sync wire types in
    `zink-protocol`; the client runs an *accepting* router (first time — it's been
    dial-only) serving envelopes at discretion; `Client::backfill(conversation,
    from)` walks `parents` back to the genesis. *Done when:* headless e2e — A builds an
    N-message conversation, B holds only the latest, B backfills from A to the genesis,
    B's `load_dag` succeeds and B threads a reply. Non-goals: re-wrap-to-*read* old
    bodies (D2), auto-backfill-on-orphan, forward auto-sync.
    ✅ *(2026-07-12: serve full envelopes — not bare cores — so the requester
    verifies authorship for free and reuses `remember`; permissive serve-what-you-hold;
    peer addressed by dial string now (bare-key discovery deferred to D0b — see
    sync-primitives.md §4 on the reachability caveat). `get-successors` served +
    round-trip tested but not yet driven. Two headless tests: backfill walks a 3-message
    chain to genesis so `load_dag`/`heads`/`next_logical` are reply-ready; and a
    peer-serves-nothing case stops rather than looping. CLI hook: `zink-cli backfill`,
    with `listen` printing its peer sync address. WASM build unaffected — sync gated
    `cfg(not(wasm))`.)*
  - [ ] **D0b · Auto-sync wiring.** Trigger backfill on an orphan receipt (peer chosen
    from the message `sender`); forward catch-up via `get-successors`.
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
- [ ] **D5 · Direct delivery (both-online fast/private path).** 🎯 When a recipient
  device is online and dialable (iroh discovery), deliver the envelope peer-to-peer
  over the D0 peer ALPN (a `Deliver` op + durable-store ack) instead of the relay
  mailbox; fall back to the mailbox on any failure, discharge the C4 outbox entry
  either way, dedup by id (free). Closes the SPEC §5.1/§5.3 intent-vs-implementation
  gap: the relay sees no metadata for online conversations, and two reachable peers
  don't need a working relay. **Depends on D0's peer ALPN; off the social-features
  critical path** (schedule when p2p/metadata-minimization is prioritized). Design:
  [direct-delivery.md](./direct-delivery.md) (⚠️ skip-mailbox-on-direct-ack vs
  always-deposit — resolve after first on-device test). *Done when:* two CLI clients
  online with the relay unreachable exchange a message directly; killing the receiver
  falls back to a mailbox deposit fetched on its return.

---

## Notes

- **Risk spikes** (🎯 with *Risk spike*) are integration unknowns paper can't resolve —
  A4 (custom ALPN) ✅, A6 (iroh WASM) ✅, C-spike (iroh-on-Android + Tauri mobile) ✅,
  C4 (background delivery vs Android Doze — replaced the retired Web Push spike).
  Expect to learn by building; keep them small and isolated.
- **Just-in-time design docs** (🎯): A4 mailbox wire messages ✅, B1 DAG store ✅,
  C1 client-core split ✅, C4 live delivery / foreground service ✅
  ([live-delivery.md](./live-delivery.md)), D0 sync primitives 📝
  ([sync-primitives.md](./sync-primitives.md)), D5 direct delivery 📝
  ([direct-delivery.md](./direct-delivery.md), drafted ahead of D0). The app
  shell (C3) needed no design doc — it assembled resolved decisions; its
  as-built map lives in `app/README.md`.
- **Async ports, sync core.** Ports are async traits from A4 onward; the pure
  `zink-protocol` core stays synchronous (no async runtime, no threads) so it ports to
  single-threaded WASM cleanly. This keeps Stage C a re-plumbing, not a rewrite.
- Stage D maps to SPEC §12 phases 1–3 and is intentionally coarse; we'll slice it
  finer when Stage C lands.
