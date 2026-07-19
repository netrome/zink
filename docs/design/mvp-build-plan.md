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
  loses its own history; full send-to-self (deposit to own mailbox) is the D3
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
  *(Status 2026-07-18 — **preliminary field observation, box stays open**:
  foreground delivery instant with loud notifications, but a backgrounded
  send typically notifies only when the app is next opened — the
  stale-connection signature (the subscription loop parks in `accept_uni`
  and only learns the connection died when that call errors; a NAT-expired
  or Doze-frozen path can look alive indefinitely). Diagnostic deferred
  until after D0b **deliberately**: D0b moves clients onto a persistent
  iroh home-relay connection (`RelayMode::Custom`) with its own keepalive
  machinery, which changes — and plausibly fixes — the very transport this
  fails on; debugging the current substrate first would be partly wasted.
  Revisit on the new substrate: if backgrounded delivery still fails then,
  the suspect list shifts to Doze/process-death on the Kotlin side.)*

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
want structured variants once the UI branches on failure kind (✅ resolved — De1,
2026-07-19); contact identity keyed on `keys.first()` needs revisiting at D3.

## Stage D — Identity & social layer (SPEC phases 1–3, post-Stage-C)

- [x] **D0 · Sync primitives & peer connectivity.** 🎯 `get` / `get-successors` (SPEC
  §5.2) over a peer ALPN, served at each peer's discretion, plus the relay-coordinated
  connectivity that lets a client actually *reach* a peer. Fixes the known late-joiner
  hole (a client without a conversation's genesis cannot reply — noted in B5);
  prerequisite for D3 backfill and D4's backlog serving. *(The peer ALPN + connectivity
  it stands up are the substrate for D1's `who-is-this` and D5 direct delivery too.)*
  Design: [sync-primitives.md](./sync-primitives.md).
  - [x] **D0a · Serve + backward-fill.** `SYNC_ALPN` + sync wire types in
    `zink-protocol`; the client runs an *accepting* router (first time — it's been
    dial-only) serving envelopes at discretion; `Client::backfill(conversation,
    from)` walks `parents` back to the genesis. *Done when:* headless e2e — A builds an
    N-message conversation, B holds only the latest, B backfills from A to the genesis,
    B's `load_dag` succeeds and B threads a reply. Non-goals: re-wrap-to-*read* old
    bodies (D3), auto-backfill-on-orphan, forward auto-sync.
    ✅ *(2026-07-12: serve full envelopes — not bare cores — so the requester
    verifies authorship for free and reuses `remember`; permissive serve-what-you-hold;
    peer addressed by dial string now (dial-by-key deferred to D0b's
    relay-coordinated connectivity — see sync-primitives.md §4.1). `get-successors` served +
    round-trip tested but not yet driven. Two headless tests: backfill walks a 3-message
    chain to genesis so `load_dag`/`heads`/`next_logical` are reply-ready; and a
    peer-serves-nothing case stops rather than looping. CLI hook: `zink-cli backfill`,
    with `listen` printing its peer sync address. WASM build unaffected — sync gated
    `cfg(not(wasm))`.)*
  - [x] **D0b · Relay-coordinated peer connectivity.** 🎯 The reachability layer under
    the peer ALPN. Today the client is dial-only and reaches a peer only at an explicit
    `ip:port`, so cross-NAT peer dialing — and therefore auto-sync, `who-is-this`, and
    direct delivery — can't work. Run the **iroh relay server inside the `zink-relay`**
    binary (one service = iroh relaying + mailbox/blobs; `tls: None`, so no domain/cert,
    native clients for now), have clients **home to their own relays** (`RelayMode::
    Custom` — still **multi-relay**, never a single shared relay), and dial a peer by key
    via their `RelayUrl`. iroh then **holepunches** to a direct P2P path when it can and
    **falls back to relaying** the (encrypted) QUIC through the relay when it can't — a
    peer stays reachable across NATs without routing plaintext or assuming direct
    connectivity. Needs a `RelayUrl` in the `ContactRecord`, **paired with its mailbox
    dial string in one structured relay entry** (they describe the same relay service —
    parallel vecs would drift). *No version bump:* nothing is deployed yet, so the field
    is added **in-place at version 1** — existing dev-stage contacts/QRs stop parsing and
    are re-exchanged (same category as C3a's pre-self-wrap envelopes); shared via the
    existing QR/record flow, since relays need not be invisible. *Done when:* two NAT'd
    clients on **different** relays establish a peer connection (direct if holepunched,
    relayed otherwise) and one backfills from the other **by key alone** — headless e2e
    covers by-key dial via relay rendezvous; the actual cross-NAT holepunch is a
    documented manual run (like C-spike/C4c). Design:
    [sync-primitives.md](./sync-primitives.md) §4. **Foundation for D0c/D0d, D1's
    `who-is-this`, and D5.**
    ✅ *(2026-07-18: code complete + headless e2e green — two in-process iroh relay
    servers, A homed to one / B to the other, B backfills by key alone (record carries
    only key + relay URL, no ip:port anywhere). `RelayEntry { mailbox, relay_url }` in
    the record; the user-facing form is the spec `<id>@<ip:port>#http://<ip>:<port>`
    the relay now prints — `parse_relay` tolerates the full spec everywhere a dial
    string is accepted. Endpoint homes at open (relay transport is fixed at bind), so
    profile relay changes apply on the next start. Edges that round-trip the profile
    use `home_relay_specs()` — feeding `home_relays()` (mailbox-only) back into
    `set_profile` would silently drop the URL. Deploy: unit now passes
    `--relay-port 4401` (tcp) next to `--port 4400` (udp), DEV-SETUP §5 updated.
    **Manual run verified live (2026-07-18):** laptop (app identity via CLI)
    backfilled a 27-message conversation from the phone **by petname alone** —
    conversation stashed, one fresh message received, `backfill` walked back to
    the genesis, full history restored and readable. Field lessons: a stale
    installed relay binary (the DEV-SETUP §5 `~/.local/bin` footgun) and pre-D0b
    profiles (bare dial strings → records without relay URLs) are the two traps;
    the phone had to restart after its profile change before it was dialable
    (homing applies at bind — expected, but a "restart to apply" hint in the app
    is cheap polish). Run with the phone on cellular (Wi-Fi off), so the
    cross-NAT criterion is met — **D0b fully done.**)*
  - [x] **D0c · Serving gate (contacts-only).** Immediately after D0b — dial-by-key
    widens who can reach the sync ALPN from "whoever knows my `ip:port`" to "anyone
    holding my key + relay". Client policy, not protocol: `SyncHandler` checks the
    connection's remote endpoint id against the contact store and answers `NotHeld`
    for unknowns (indistinguishable from not-holding — declining and not-having look
    the same, SPEC §5.2). Independent of D0b's code, so it can also run in parallel.
    *Done when:* a non-contact peer's `get` for a held id returns `NotHeld`; a
    contact's succeeds.
    ✅ *(2026-07-18: gate resolved once per connection (the caller's key IS the
    authenticated connection key); non-contacts get `NotHeld` / empty successors;
    own key always served (self-dial, and D3 own-device sync rides the same
    allowance). e2e: a stranger backfills 0 and sees no successors of a held
    genesis, the same requester succeeds after their record is stored. Note for
    D1: `who-is-this` will want a *different* policy for its own op — identity
    queries from strangers are the point — so the gate stays per-op, not
    per-connection-refusal.)*
  - [x] **D0d · Auto-sync wiring.** Trigger backfill on an orphan receipt (peer chosen
    from the message `sender`, dialed by key via D0b); forward catch-up via
    `get-successors`.
    ✅ *(2026-07-19: the sync walk is now backward (parents → genesis) **then
    forward** (`get-successors`; first round queries every stored id since forks
    can hang off interior messages, later rounds only newly-fetched ids — converges,
    chatty-but-fine at friend scale). Shared validation gained a
    conversation-membership check: forward ids are the *peer's claim*, not parents
    read from verified envelopes, so a served envelope must also belong to the
    conversation being synced. `auto_sync` runs after every drain (recv, catch-up,
    nudge), *before* the edge renders — a healed conversation appears whole; one
    scan when nothing is orphaned, dials the sender by key when something is.
    Best-effort: unreachable sender / mailbox-only record logs and moves on.
    e2e: middle-message holder syncs to both ends (2 back + 2 forward); an
    orphaned receipt heals via `auto_sync` across two relays with zero explicit
    action. Explicit `backfill` (CLI included) now also pulls forward.
    **Verified live 2026-07-19:** laptop app with its conversation store wiped —
    one message from the phone and the drain auto-healed the full history, no
    explicit action. Incidentally also the recovery story for a lost/corrupted
    store: any peer holding the history restores it by messaging you.)*
- [x] **De1 · Structured errors in `zink-client` (pre-D1).** Replace the crate's
  `Result<_, String>` with error enums (per flow or one crate enum with variants —
  decide at implementation). Flagged in the post-C2 hardening review ("`String`
  errors will want structured variants once the UI branches on failure kind");
  the cost is now visible: tests assert on prose (`err.contains("no relay url")`),
  and D1's UI genuinely branches ("re-exchange records" is a different UX than
  "peer offline" or "not a contact"). Do it *before* D1 — a cross-cutting
  conversion gets more expensive with every new caller. *Done when:* no
  `Result<_, String>` remains in `zink-client`'s public API; error-case tests
  match variants, not substrings; CLI/app render messages via `Display`.
  ✅ *(2026-07-19: one crate-wide `zink_client::Error` (thiserror v2 — new dep,
  user-approved: derive-only, wasm-clean). Shape: precise variants for what
  edges/tests branch on (`NoRelayUrl`, `NotAContact`, `PetnameCollision`,
  `ProfileIncomplete`, `NoRecipients`, `AllRelaysPending`, `InvalidRecord`),
  kind-grouped variants with human payloads for the rest (`Keystore`, `Storage`,
  `Conversation`, `InvalidInput`, `Unreachable`, `Transport`,
  `UnexpectedResponse`, `BlobUnavailable`), `#[from]` pass-through for the
  protocol's typed errors (`CryptoError`/`OpenError`/`DecodeError`). Edge shim:
  `From<Error> for String` renders via `Display`, so the CLI's and app's
  `Result<_, String>` command boundaries kept working with `?` — only two
  tail-position returns needed `Ok(…?)`. The NoRelayUrl test now matches the
  variant. Note: `Received.body` already carried typed `OpenError` — unchanged.)*
- [x] **De2 · QAD endpoint in `zink-relay`.** The embedded iroh relay serves only
  HTTP relaying (`quic: None`), so clients' first net-report waits out iroh's full
  3 s `PROBES_TIMEOUT` before the endpoint reports online (measured: ~3.03 s of
  the ~3.15 s relay-based e2e tests), and address discovery for holepunching is
  degraded (disco-only — direct paths may silently fall back to relaying).
  Enable `ServerConfig.quic` (QUIC address discovery, the STUN replacement):
  needs a TLS 1.3 rustls config — investigate whether a self-signed cert
  generated into the relay data dir satisfies iroh's QAD client (no domain/ACME,
  consistent with the no-TLS-ops stance). Side effects: faster client startup,
  better holepunch rates, and the two relay-based e2e tests drop to sub-second.
  *Done when:* a client homing to the relay reports online well under 1 s and
  the QAD probe succeeds in the net-report.
  ✅ *(2026-07-19: **same-port convention** — the relay serves QAD on UDP at the
  relay-URL port number (TCP http + UDP QAD coexist at one number; distinct
  URLs → distinct QAD ports, so multi-relay e2e on one machine stays
  collision-free; a URL with no explicit port keeps iroh's default 7842, which
  is standard iroh relays' own convention). Not a hard assumption anywhere —
  the port is per-relay in the client's `RelayMap`. Self-signed cert
  regenerated per start (nothing pins it, so no data-dir persistence); the
  client homes with `CaTlsConfig::insecure_skip_verify` — a webpki CA in the
  trust path is against the philosophy, iroh connections authenticate by
  endpoint key, and a QAD MITM can at most misreport the observed address
  (degrades holepunching to today's baseline). That constructor sits behind
  iroh-relay's *empty* `test-utils` feature — flipped via a direct
  `iroh-relay` dep (zero new compiled code; `rcgen`/`rustls` in `zink-relay`
  likewise already in the graph via iroh-relay `server`). Ephemeral relay
  ports are now picked by the relay (a `:0` pair would land on two numbers).
  Suite: zink-client e2e **3.18 s → 0.37 s**; timing test pins the regression
  (`online` < 2 s vs the 3 s probe timeout). Deploy: unit unchanged, but
  **4401/udp must be open** next to 4400/udp + 4401/tcp — DEV-SETUP §5
  updated; QAD failing is soft (falls back to the pre-De2 stall).)*
- [x] **D1 · Attestations & name resolution.** 🎯 Self-profile (name/avatar);
  client-side petnames; `who-is-this` pull *(a peer request/response — rides D0b
  connectivity)*; client-side trust ranking. *(Profile + petnames are largely already
  built from Stage C: `set_profile`/`my_record`, `add_contact` petnames with collision
  checks, the app ContactsView + QR add. Unbuilt: the networked `who-is-this`,
  ranking, avatars.)* **Overall acceptance — the one-way-add reply hole:** C scanned
  A's QR and messaged A; A resolves C's key through a shared contact, adds C, and
  replies. Design: [who-is-this.md](./who-is-this.md) — query format, hop limit,
  serving policy, learned-record storage, avatar crypto; decisions resolved
  2026-07-19. Notably: serving stays **contacts-only uniformly** (the D0c note
  anticipated a looser per-op policy for this op; resolved the other way — the
  per-op gate structure stays, the policies coincide), **no auto-query** (asking
  contacts about a key reveals you just heard from it — manual trigger only, a
  privacy decision), and **network input never mutates stored records** — answers
  accumulate in a learned store (multiple records per key) and freshness is a
  read-time resolution, the same keep-evidence-rank-at-use stance as the DAG.
  - [x] **D1a · Protocol op + serve side.** `SyncOp::WhoIs { key }` →
    `SyncResult::Known { record }` / `NotHeld` on the existing sync ALPN (in-place
    at v1); `SyncHandler` serves a contact caller the fresh self-record (own key)
    or a *user-added* contact's stored record — learned records are never
    re-served (hop limit 1 is structural), strangers always get `NotHeld`.
    *Done when:* headless e2e — a contact's `WhoIs` returns the stored record; a
    stranger's returns `NotHeld`; a learned-only subject is `NotHeld` even to a
    contact.
    ✅ *(2026-07-19: variants **appended** so existing BORSH tags stay stable;
    round-trip + hostile-input tests extended. Self-record construction extracted
    to `build_own_record`, shared by `my_record` and the handler so the two can't
    drift (revision stays hardcoded 0 until D1b's supersession fix); the handler
    now holds its own `DeviceKey` (rebuilt from the seed — deliberately not
    `Clone`) with a hand-written redacting `Debug`, since serving a *fresh*
    self-record needs signing at request time — a profile change is served
    immediately, no restart (unlike endpoint homing). Contact-store read errors
    fail closed, like the gate. e2e: stranger `NotHeld` → befriend → stored
    record verbatim; unknown subject `NotHeld` even to a contact; own-key query
    `NotHeld` until the profile completes, then a verified self-record. SPEC §11:
    who-is-this format/hops moved from still-to-pin-down to a resolved row.)*
  - [x] **D1b · Pull, learned store, resolution.** `Client::who_is(key)` dialing
    the subject (if held) + all dialable contacts; answers validated like scanned
    QRs and stored in a **learned store** with provenance, keyed
    (subject, responder), outside the contact store (the D0c gate must not
    widen); **nothing is ever overwritten** — the contact store is never touched
    by network input; freshness is read-time relay resolution by provenance
    class (subject-served > scanned > contact-served, latest within class), and
    sealing keys come only from the user-added record until D3; name precedence
    petname > learned self-claim (with provenance + agreement count, `revision`
    breaks conflicts) > hex; fix `my_record`'s hardcoded `revision: 0` (persist +
    bump per profile change — supersession needs a winner); CLI `who-is`.
    *Done when:* headless e2e — the one-way-add flow (A learns C via contact B,
    adds C, replies); a subject-served answer wins relay resolution with the
    contact store byte-identical after any sequence of `who_is` calls; sealing
    keys ignore learned records.
    ✅ *(2026-07-19: learned store at `state/learned/<subject>/<responder>.record`
    + receipt-time sibling; resolution went in as one read-time seam —
    `effective_relays` (provenance classes, latest within class) now feeds
    `resolve_contact`, `reply_contacts`, `backfill_by_key` **and `who_is`'s own
    dialing**, so every by-key path benefits from freshness. `resolve_name`
    groups by name, ranks by attestation revision (protocol gained
    `self_name_claim` — and `self_claimed_name` now picks the *highest-revision*
    valid claim rather than the first, the SPEC §3.2 rule; forged
    higher-revision claims still lose, tested). Revision fix: `profile.revision`
    persists, bumped on rename only (per claim-kind scope; relay changes order
    by receipt time instead). e2e: client-crate — learn-via-contact with the
    contact store byte-compared, subject-served beats newer hearsay, smuggled
    keys inert, rename supersession; CLI — full one-way-add acceptance
    (`tests/who_is.rs`): Carol messages Alice one-way, Alice `who-is`es her key,
    Bob (listening) serves Carol's record, Alice promotes the printed payload
    via `contact-add` and her reply reaches Carol. `who-is` prints answers with
    provenance + shareable payload, then the ranked resolution.)*
  - [x] **D1c · Messaging-UI hook.** Unknown participant → "who is this?" action;
    candidate names with provenance ("records held by B, D"); add-as-contact with
    the petname prefilled; refresh from the contact view. *Done when:* the
    acceptance flow runs live on two devices.
    ✅ **Verified live 2026-07-19** — phone + two laptop instances (second
    instance: `cargo tauri dev --no-watch` with a distinct app identifier for
    its own data dir, the C3b technique): one-way add from the phone, banner on
    the unknown key, who-is resolved via the third identity, add-as-contact
    prefilled, reply delivered.
    *(2026-07-19: code complete — `Message.unknown_sender` (hex key iff the
    sender is no stored contact) drives a per-conversation banner; "who is
    this?" opens a panel: candidates with name + provenance ("confirmed by
    themself" / "records held by …") and an add-as-contact button per
    candidate a responder is currently serving (petname prefilled from the
    self-claim; sender labels flip on reload). Contacts view rows gained a
    "who is?" freshness pull ("N answer(s) — fresh records apply
    automatically" — read-time resolution needs no apply step). One command,
    `who_is`, returns the render-ready `WhoIsReport`; `AppState.contacts`
    became structured rows (petname + key) so the contact view can ask by
    key. No auto-query anywhere, per the D1 privacy decision. Also fixed: a
    **latent De1 breakage** in the managed-client init (the app is outside
    the workspace, so nothing had compiled it since the error-enum landed).
    UI wasm + aarch64 APK build. Groups aren't needed to exercise this —
    one-way adds are the no-groups unknown-sender case, and the same hook
    covers a re-keyed friend showing up as unknown.)*
  - [x] **D1d · Avatars.** `Claim::Avatar` gains the content key next to the blob
    hash (in-place at v1, dev-stage records re-exchanged — D0b norm): the key
    travels only in records / `WhoIs` answers (QR + E2E peer channels), never
    through a relay; encrypt-once (A3 AEAD + content address), pushed to the
    publisher's *own* home-relay caches on publish and re-pushed on open
    (30-day TTL); fetchers verify + cache client-side (C3a); pick/downscale
    reuses the C3c canvas path. *Done when:* two devices see each other's
    avatars in contacts + conversation views, and the relay-cached bytes are
    ciphertext.
    *(2026-07-19: code complete. **Design correction at implementation** —
    the planned `key-commit` was dropped as tautological: hash and key ride
    in ONE signed attestation, so the signature already binds them and a
    commitment would be checked against the very claim that supplied the
    key; integrity = signature + content address + AEAD tag (who-is-this.md
    §8 updated). Protocol: `seal_avatar`/`open_avatar` (hostile-input
    tested), `self_avatar_claim` with supersession + forgery tests. Client:
    `set_avatar` (512 KiB backstop cap, bumps its own revision — per
    claim-kind scope, independent of the name counter), `push_avatar`
    (publish + app-startup re-push; relays dedup by hash), `avatar(key)`
    resolving the highest-revision claim across stored + learned records,
    fetching from the claim-carrying record's relays, verify-then-cache
    (ciphertext at rest, like every blob). CLI `set-avatar`/`avatar
    [--out]`; e2e `avatar.rs`: cross-client render through a relay with
    plaintext nowhere at rest. App: picker (canvas ≤256 px JPEG) + own
    preview, contact-row + chat-sender avatars (lazy, best-effort),
    decrypted bytes magic-sniffed before the webview renders them. Note:
    contacts learn a *new* avatar only with a fresh record — re-scan QR or
    a `who-is` freshness pull; nothing announces it (by design, no
    broadcast channel).)*
    ✅ **Verified live 2026-07-19** (QR re-scan propagation path) — **D1
    complete.** 🎉 Field note: the who-is UX can stall or look like a
    failure — diagnosed as `who_is`'s serial dials × the 10 s production
    connect timeout per unreachable contact, plus view-local avatar caches
    that never retry a miss; tracked as De3 below.
- [x] **De3 · who-is responsiveness (UX polish).** Field observation at D1's close:
  the who-is panel can stall for tens of seconds or read as a silent failure.
  Causes, diagnosed 2026-07-19: (a) `Client::who_is` dials contacts **serially**
  with the production 10 s `connect_timeout` each — one offline contact stalls
  the whole query, N offline contacts stack to N×10 s; (b) `WhoIsReport` doesn't
  say how many contacts were asked vs unreachable, so "0 answers after 20 s"
  and "nobody knows this key" render identically; (c) the app's per-view avatar
  caches mark a miss as done and never retry until the view remounts, and the
  contacts-view "who is?" doesn't reload avatars/state after answers land.
  Fix shape: dial concurrently (`n0_future` join — no runtime in the lib) with
  a tighter per-peer bound, add asked/unreachable counts to the report, retry
  avatar misses on who-is completion / view events. *Done when:* a who-is with
  one offline contact answers in ~one connect-timeout, and the panel says
  "asked N, M unreachable" honestly.
  ✅ *(2026-07-19: `Client::who_is` now dials all targets at once
  (`n0_future::join_all` — runtime-free, wasm-clean) with the deadline capped
  at `min(connect_timeout, 5 s)` — no new config knob, so the CLI e2e's
  `ZINK_CONNECT_TIMEOUT_MS` tightening still applies; returns `WhoIsOutcome
  { answers, asked, unreachable }` (mailbox-only records aren't "asked").
  Regression test: 3 TEST-NET-offline contacts + 1 live responder with a 1 s
  deadline completes in ~1.3 s with `(asked, unreachable) = (4, 3)` — serial
  would be ≥ 3 s. Edges: CLI prints the counts line; the app panel now
  distinguishes "no dialable contacts", "none reachable — try again later",
  and "the reachable ones don't know this key"; who-is completion and
  add-as-contact re-fetch the subject's avatar past any cached miss, and the
  contacts-view pull refreshes that row's avatar. The ~5 s `who_is.rs` e2e
  is inherent subprocess cost (~15 CLI invocations, each a full client
  open), left as is.)*
- [ ] **De5 · who-is from a freshly-set-up client.** Field observation at the
  D2c run (2026-07-19, three instances on one laptop): the *new* participant's
  who-is never completed — not even after adding a contact — while the
  long-running clients' queries worked. Prime suspect: **homing applies at
  bind** (the D0b field lesson) — a client whose profile was set *this run*
  has an endpoint with no relay transport, so it can register mailboxes and
  send (direct dials) but cannot dial *anyone by key* until the next open;
  who-is is pure dial-by-key. Two fixes to make: (a) the "restart to apply"
  hint from D0b graduates from polish to necessary — better, rebind or prompt
  on profile save; (b) verify the panel's behavior under a zero-relay-transport
  endpoint — dials should fail *bounded* and report "N unreachable", never
  hang; if it truly never completes, the connect path has a second bug.
  Not critical (one-time-per-install window); revisit before D3's pairing
  flow, which will hit the same fresh-client moment by construction.
- [ ] **De4 · e2e suite latency.** The full-stack CLI tests are slow (groups
  ~13 s, who-is ~5 s) for reasons that are harness-shaped, not protocol-shaped:
  each drives the CLI *binary*, so every step is a subprocess doing a full
  client open (bind, home, net-report, register) and close — ~15–30 spawns per
  test — plus fixed-sleep readiness polls around background `listen` processes.
  The flows themselves are hundreds-of-ms fast (the in-process client-crate
  e2es prove it: multi-relay sync suites finish in ~0.4 s). Fix shape: an
  in-process multi-client harness — zink-relay's mailbox service as a
  zink-client dev-dependency (no cycle; the blobs/nudge router already spawns
  in-process in zink-relay's own tests) — so whole flows run as library calls;
  the CLI keeps one or two thin smoke tests for arg-parsing/output. *(Noted
  2026-07-19 at D2b review.)*
- [x] **D2 · Groups: membership & the unknown-key pipeline.** 🎯 *(Was D3 —
  reorg resolved 2026-07-19: the machinery that makes multi-device "seamless"
  on the contacts' side IS the membership-presentation pipeline, so it ships
  first and multi-device becomes a thin layer on top. SPEC §12 phases
  multi-device before groups, but that phasing assumed the multi-recipient DAG
  was unbuilt — it shipped in B2, so the inversion is consistent with the
  spec's intent.)* Multi-recipient conversation creation in the UI (delivery
  is fan-out since B2 — this is mostly membership *presentation*), plus the
  unknown-key pipeline: a key appearing in a conversation's **signed
  `recipients` IS the announcement** — the fan-out itself carries the fact,
  no new mechanism; unknown key → **auto `who-is-this` scoped to that
  conversation's participants** (a deliberate, recorded revision of D1's
  no-auto-query: the blanket rule was about unsolicited senders — within a
  conversation the key's presence is already mutual knowledge, so the scoped
  query reveals nothing; revise who-is-this.md §5 when this lands) →
  provenance-ranked candidates → the "a wild Charlie appeared" surface →
  add-as-contact or ignore, the user always in control of their own store.
  *Done when:* headless e2e — B adds C to a conversation with A; A's client
  auto-queries, surfaces C with provenance, A adds C and replies to all.
  Design: [groups.md](./groups.md) (drafted 2026-07-19 — defensive by
  construction: membership = the **heads'** participant set, a lens on the
  DAG, never an object; the `send_in` participant-index artifact-fork fix;
  the scoped auto-query with introducing-sender-first responders + a
  per-(subject, conversation) rate limit; **non-contact members stay
  addressable** — trust is naming + the serving gate, never delivery, else
  one skeptical participant's head shrinks the group for everyone;
  **conversation legitimacy = a contact has *contributed*** (authored, not
  merely listed — authorship is unforgeable), triaged at presentation and
  gating the auto-query, never storage; the responder-side-gate limit
  documented; SPEC untouched end to end).
  - [x] **D2a · Membership core + index fix.** Heads-based membership feeding
    `reply_contacts` + summaries (union-fallback when the DAG can't build);
    membership-delta info on history; `send_in` updates the participant-set
    index like `remember` does. *Done when:* headless e2e — add-member with
    everyone (including the adder, by name) threading one conversation; a
    stop-including reply shrinks the reply set; delta lines derived.
    ✅ *(2026-07-19: `Client::membership` + heads-based `reply_contacts` /
    `ConversationSummary.participants`; `HistoryMessage.{joined,left}`
    derived per message vs held parents (empty for genesis / partial view);
    the index fix moved into `finish_send` — every send now records its
    sealed core's participant set, mirroring `remember`, so the old
    only-on-create param is gone. CLI: `reply --add <petname>` (the grow
    gesture) + `[+ x]`/`[- x]` delta lines in `history`. **Design sharpened
    by the e2e** (groups.md §2 updated): a member with no route must STAY in
    the sealed `recipients` — dropping them shrank membership for everyone
    through that reply's head, the §2 hazard arriving via routelessness
    instead of distrust; membership ≠ deliverability, so they're sealed to,
    surfaced as `unknown`, delivered nothing until a route is learned (their
    copy heals via peer sync), and only an all-unroutable set refuses the
    send. Also observed: a *shared* relay masks routelessness — deposits fan
    out to every registered recipient in the envelope, so the no-route case
    only manifests across distinct relays (the e2e gives Carol her own).
    e2e `groups.rs`: 1:1 grows via `reply --add`; the adder's send-by-name
    threads (fork regression, both ends); Bob reaches never-promoted Carol
    through a who-is-learned route; stop-include shrinks the reply set with
    no fork. Client-crate: heads-vs-union membership incl. concurrent-head
    union, delta derivation, the grown-set index mapping recorded even when
    every relay is down.)*
  - [x] **D2b · Scoped auto-query.** Responder-scoped `who_is` variant; the
    post-drain trigger (auto_sync's seam) with the rate limit;
    who-is-this.md §5 revised. *Done when:* the acceptance flow headless —
    A auto-learns C with zero manual action, adds C, replies to all.
    ✅ *(2026-07-19: `who_is_among(subject, responders)` — `who_is` is now
    a thin wrapper passing the contact list; responders resolve to routes
    like reply targets (contact or learned records; a non-contact responder
    labels as short hex). `auto_who_is` runs after `auto_sync` on every
    drain path: touched conversations → contributing-contact gate
    (`has_contributing_contact`, pub — it's also the parked quarantine's
    predicate) → unknown members with no learned record → ask that
    conversation's participants only, once per (subject, conversation) per
    run (in-memory; the manual trigger re-asks). Answers land in the
    learned store; edges render via `resolve_name` at the next paint — no
    new event plumbing, as designed. Responder *ordering* became moot: De3
    made the dials concurrent, so all participants are asked at once. e2e
    (`groups.rs`): bob grows the 1:1 with carol and goes online; alice
    only drains — with bob then killed, carol still resolves ("records
    held by Bob") and alice's reply-to-all reaches her via the
    auto-learned route; client-crate tests pin provenance, the rate limit
    (wiped learned store not re-asked), and the gate short-circuiting
    before the rate limit for a stranger-authored conversation.)*
  - [x] **D2c · Group UI.** Multi-select compose; add-to-conversation;
    heads-based labels + delta lines; the wild-Charlie popup with persisted
    dismissal. *Done when:* a live group chat across devices — create, add,
    popup → add, reply-all reaches everyone.
    ✅ **Verified live 2026-07-19** — three participants on one laptop:
    create, add, popup → add, reply-all delivered. One field observation
    tracked as De5: who-is from the freshly-set-up participant never
    completed (prime suspect: unhomed endpoint — profile set this run,
    homing applies at bind). **D2 complete.** 🎉
    *(2026-07-19: code complete — compose is a contact multi-select
    (`send_message` takes `to: Vec<String>` + `add: Vec<String>`; an add
    with empty text is allowed — the membership change IS the message);
    chat view gained an add-to-conversation picker, derived delta lines
    (`+ name · − name`, from `Message.{joined,left}` labels), and the
    wild-key popup driven by a new `unknown_members` command — unknown
    *members* (from heads-based membership, so added-but-silent members
    surface, which sender-based detection would miss) with learned
    candidates (name + provenance + promotable payload from
    `Client::learned_candidates`, the freshest record per name group —
    `resolve_name` now delegates to it) and a `dismissed` flag; ignore →
    `dismiss` command → `dismissed.keys` in client state (persisted;
    dismissed keys collapse to the compact who-is row — the un-dismiss
    path; honest hex rendering stays). Labels everywhere are heads-based
    for free (D2a changed the summary semantics). UI wasm + aarch64 APK
    build; 155 tests incl. freshest-record pairing + dismissal
    persistence. **Awaiting the live group-chat run** — phone + two
    desktop instances works: create a group from the multi-select, add the
    third member from the picker, their device pops "a wild key appeared"
    with the auto-learned candidate, add → label flips, reply-all reaches
    everyone.)*
- [ ] **D3 · Multi-device.** 🎯 *(Was D2 — now genuinely thin: D2's pipeline
  does the propagation, since "your new device" and "a new member" are the
  same event under the hood (SPEC §5.2).)* QR pairing producing the mutual
  `same-person-as`; the clustering rule that upgrades the popup from "wild
  Charlie" to "Alice added a device" — cluster iff a mutual link involving an
  already-trusted key verifies; otherwise it stays an unknown key added by
  Alice (the one real anti-spoofing rule, and the only place `same-person-as`
  evaluation enters); the re-wrap op + the D0c gate extension (keys mutually
  attested as mine count as self); send-to-self deposits (the SPEC §6
  extension of self-wrap); the parked `keys.first()` contact-identity fix
  (a re-scanned record with reordered/added keys must not read as a new
  contact). Design doc should state the completeness hierarchy: own sibling
  devices are the *primary* history/freshness channel (send-to-self +
  own-device sync); contacts' fan-out is robustness, never load-bearing —
  and park the honest repudiation-lag note (quiet conversations keep
  addressing a dead key until someone speaks; lazy by design). *Done when:*
  pair a second device, it sends one message per active conversation,
  contacts' clients cluster it under the person, and it reads old history
  via re-wrap.
- [ ] **D4 · Web-of-trust.** Third-party profile attestations; "who is this?"
  answers from contacts; concurrency-aware message views. *(Position confirmed
  at the 2026-07-19 reorg: D2's pipeline runs entirely on D1's
  self-claims-with-provenance; D4's vouching — "your friends call them…" —
  hop>1 forwarding, and weighted aggregation layer onto the existing popup
  ranking additively. Concurrency-aware views likewise: groups make forks
  more common, but the Lamport linearization already renders them honestly.)*
- [ ] **D5 · Direct delivery (both-online fast/private path).** 🎯 When a recipient
  device is online and reachable (via D0b connectivity — holepunched direct, or
  relay-routed as fallback), deliver the envelope peer-to-peer over the D0a peer ALPN
  (a `Deliver` op + durable-store ack) instead of the relay mailbox; fall back to the
  mailbox on any failure, discharge the C4 outbox entry either way, dedup by id (free).
  Closes the SPEC §5.1/§5.3 intent-vs-implementation gap: the relay sees no metadata for
  online conversations, and two reachable peers don't need the mailbox. **Depends on
  D0a's peer ALPN + D0b connectivity; off the social-features critical path** (schedule
  when p2p/metadata-minimization is prioritized). Design:
  [direct-delivery.md](./direct-delivery.md) (⚠️ skip-mailbox-on-direct-ack vs
  always-deposit — resolve after first on-device test). *Done when:* two CLI clients
  online with the relay unreachable exchange a message directly; killing the receiver
  falls back to a mailbox deposit fetched on its return.

---

## Parked — before first external deployment

Not scheduled into a stage; must land before any build leaves our hands.

- [ ] **Unknown-sender quarantine.** Anyone holding a record can deposit
  (mutuality is not required — that's what makes one-way adds work), so an
  unknown key can fill a mailbox and the conversation list with noise. Client
  policy, no protocol change: conversations failing the **contributing-contact
  rule** (groups.md §6 — no contact has *authored* a held message; presence in
  `recipients` is attacker-controlled, authorship isn't) land in a bounded
  "message requests" view instead of the main list (oldest evicted at the
  cap); D1c's who-is hook is the promote-out path. D2b computes the predicate
  (it gates the scoped auto-query); this parked item is just the view.
  Complements the relay-side caps (C0) until send-capabilities (SPEC §8,
  deferred) provide the real gate. *(Noted 2026-07-19 while wiring D1c;
  criterion sharpened at the D2 design.)*
- [ ] **Per-type format versions.** One global `FORMAT_VERSION` is stamped into every
  versioned object and `decode_versioned` accepts only the exact current value — so any
  single object's version bump forks the whole protocol at once (v1 clients silently
  skip v2 messages per SPEC §10, and a bumped client can't decode its own v1 on-disk
  state). Fine while every install is ours (we change structs in-place at v1 and wipe
  dev data, as in D0b); untenable the moment two builds coexist in the wild. Fix:
  per-type version constants and a per-type accepted-version set in `decode_versioned`,
  so e.g. `ContactRecord` can accept `{1, 2}` while `MessageCore` stays at 1 and ids
  don't move.

---

## Notes

- **Risk spikes** (🎯 with *Risk spike*) are integration unknowns paper can't resolve —
  A4 (custom ALPN) ✅, A6 (iroh WASM) ✅, C-spike (iroh-on-Android + Tauri mobile) ✅,
  C4 (background delivery vs Android Doze — replaced the retired Web Push spike).
  Expect to learn by building; keep them small and isolated.
- **Just-in-time design docs** (🎯): A4 mailbox wire messages ✅, B1 DAG store ✅,
  C1 client-core split ✅, C4 live delivery / foreground service ✅
  ([live-delivery.md](./live-delivery.md)), D0 sync primitives 📝
  ([sync-primitives.md](./sync-primitives.md)), D1 identity discovery 📝
  ([who-is-this.md](./who-is-this.md)), D2 groups 📝 ([groups.md](./groups.md)),
  D5 direct delivery 📝
  ([direct-delivery.md](./direct-delivery.md), drafted ahead of D0). The app
  shell (C3) needed no design doc — it assembled resolved decisions; its
  as-built map lives in `app/README.md`.
- **Async ports, sync core.** Ports are async traits from A4 onward; the pure
  `zink-protocol` core stays synchronous (no async runtime, no threads) so it ports to
  single-threaded WASM cleanly. This keeps Stage C a re-plumbing, not a rewrite.
- Stage D maps to SPEC §12 phases 1–3 and is intentionally coarse; we'll slice it
  finer when Stage C lands.
