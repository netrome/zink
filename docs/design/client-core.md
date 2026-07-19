# Design: Client Core (`zink-client`)

The shared client library, pinned just-in-time for slice C1. Downstream of
[SPEC.md](../SPEC.md) §8 and the Stage C pivot (native app first).

Status: **resolved for MVP.**

## Goal & shape

One library implements "being a zink client"; every frontend is a thin edge over it:

```
zink-cli (args/printing)  app/src-tauri (Tauri commands)   [post-MVP: PWA via WASM]
        └───────────────┬───────────────┘
                  crates/zink-client        ← keystore, contacts-of-sorts, conversation
                        │                     state, send/recv flows, blob push/fetch
                  crates/zink-protocol      ← pure core (unchanged)
```

- **Moves from `zink-cli`:** keystore load/create, `ClientState` (participants →
  conversation, envelope storage, DAG rebuild), dial-string parsing, endpoint/
  connection handling, mailbox round-trips, deposit-with-retry, blob push (observe-
  confirmed) and fetch+decrypt, the send/recv flows.
- **Stays in `zink-cli`:** arg parsing, output formatting, writing fetched blobs to
  files. The CLI keeps its exact observable behavior — the existing e2e tests are the
  regression net for the whole lift.
- **Stays out entirely:** protocol logic (already in `zink-protocol`), UI, policy
  that C2+ will add (contact naming, trust).

## API sketch

A `Client` owns the device key, one iroh endpoint, and the on-disk state:

```rust
Client::open(key_path)                        // load key (CLI keygen creates it)
Client::open_with(key_path, ClientConfig)     // …with edge-injected tuning (e.g. the
                                              // relay connect deadline; the CLI maps
                                              // ZINK_CONNECT_TIMEOUT_MS onto it so the
                                              // e2e suite's down-relay tests run fast)
Client::open_or_create(key_path)              // app: silent first-run key creation
client.send(&[Contact], Vec<u8>, Vec<BlobDraft>) -> SendReceipt  // seal → deposit per
                                              // distinct relay (retry) → push blobs
client.recv(&[relay]) -> Vec<Received>        // register → page-fetch → dedup by id →
                                              // open → remember → ack each page
client.fetch_blob(&Received, &BlobHash) -> Vec<u8>              // cache, else the relay it
                                              // arrived through; verify + decrypt
// profile + contacts (C2): set_profile, my_record, add_contact, contacts,
// resolve_contact, register_at_home_relays
// contact identity = key overlap (D3a, multi-device.md §4): add_contact
// updates the one overlapping entry, only under its own petname (the
// explicit confirm) — ContactOverlap / AmbiguousOverlap otherwise;
// participant_labels(&[PublicKey]) dedups display labels per contact entry
// stored history (C3a):
client.conversations() -> Vec<ConversationSummary>   // id, participant keys, count,
                                              // last timestamp — naming is the edge's
client.history(conversation) -> Vec<HistoryMessage>  // linearized; bodies opened per
                                              // message (self-wrap covers own sends)
client.fetch_stored_blob(conversation, message, &BlobHash) -> Vec<u8>
                                              // cache, else own home relays (that's
                                              // where senders push blobs for us)
// replying (C3b; membership semantics from D2a, groups.md §2):
client.membership(conversation) -> BTreeSet<PublicKey> // heads-based — the current
                                              // participant set, a lens on the DAG
client.reply_contacts(conversation) -> ReplyContacts // membership minus me; routes
                                              // via contact OR learned records
                                              // (address, don't trust). Routeless
                                              // members stay recipients (sealed,
                                              // undelivered) + listed in `unknown`
client.send_in(conversation, &[Contact], Vec<u8>, Vec<BlobDraft>) -> SendReceipt
                                              // thread into a *given* conversation
                                              // (send-by-contacts uses the participant
                                              // index; this bypasses it)
// outbox (C4a, live-delivery.md §2): sends ledger per (message, relay) before
// any network work; one relay failing never aborts the others
// (SendReceipt.pending_relays; error only if NO relay took it — "queued");
client.flush_outbox() -> FlushReport          // idempotent re-deposit + re-push;
                                              // runs before sends and after recv;
                                              // HistoryMessage.pending flags the rest
// live delivery (C4b): one loop per relay, spawned by the edge (no runtime
// in the lib); connect → register → flush → drain → drain-per-nudge,
// reconnecting forever with jittered backoff; `on_new` per non-empty drain
client.subscribe(relay, on_new: FnMut(Vec<Received>)) -> never returns
// peer sync (D0a/D0b, sync-primitives.md): the client also *serves* — an
// accepting router on SYNC_ALPN answers get/get-successors from local
// storage for the client's lifetime. The relay transport is ALWAYS bound
// (empty map pre-profile — De5), so peers' relay URLs dial immediately on
// a fresh install; set_profile (async since De5) homes the RUNNING
// endpoint via insert_relay/remove_relay — profile changes apply live,
// no restart. Homing keeps this device reachable by key.
// Serving gate (D0c): contacts-only — non-contacts get NotHeld / empty
// successors, indistinguishable from not-holding; own key always served.
// Auto-sync (D0d): every drain (recv / catch-up / nudge) heals orphaned
// conversations before the edge renders — missing ancestors trigger a
// by-key sync from the message's sender; the walk also pulls forward via
// get-successors. Best-effort; a drain never fails on an unreachable peer.
client.backfill(conversation, "<id>@<ip:port>") -> usize   // explicit peer addr
client.backfill_by_key(conversation, PublicKey) -> usize   // via the relay_url in
                                              // the peer's stored ContactRecord:
                                              // rendezvous at their relay, holepunch
                                              // to direct, relayed as fallback
client.home_relay_specs() -> Vec<String>      // full `dial[#relay-url]` specs — the
                                              // round-trip form for profile forms;
                                              // home_relays() stays mailbox-only
// identity discovery (D1a/D1b, who-is-this.md): the serve side answers
// WhoIs with the fresh self-record (own key) or a user-added contact's
// stored record — learned records are never re-served (hop 1, structural).
client.who_is(PublicKey) -> WhoIsOutcome      // dial every dialable contact AT ONCE
                                              // (De3; deadline min(connect_timeout, 5s)),
                                              // validate like a scanned QR, append to
                                              // the learned store with provenance;
                                              // answers + asked/unreachable counts.
                                              // MANUAL trigger only (privacy §5)
client.resolve_name(PublicKey) -> ResolvedName // petname > learned self-claims
                                              // (revision-ranked, provenance +
                                              // agreement surfaced) > Unknown
client.learned_candidates(PublicKey) -> Vec<(LearnedName, ContactRecord)>
                                              // resolve_name's groups, each with the
                                              // freshest record claiming the name —
                                              // the popup's promotable payload (D2c)
client.dismiss(PublicKey) / client.dismissed() // ignore an unknown key (persisted
                                              // presentation policy, groups.md §5)
client.who_is_among(PublicKey, &[PublicKey]) -> WhoIsOutcome
                                              // responder-scoped who_is (D2b) — the
                                              // auto-query's shape; also auto-run
                                              // per drain for unknown members of
                                              // legitimate conversations (gated by
                                              // has_contributing_contact, rate-
                                              // limited per (subject, conversation))
// avatars (D1d, who-is-this.md §8): encrypt-once; the key rides inside the
// signed Avatar claim (E2E channels only) — relays cache ciphertext.
client.set_avatar(Vec<u8>) -> AvatarReceipt   // cache + claim at next revision +
                                              // push to home relays
client.push_avatar() -> usize                 // re-push (startup / publish) — the
                                              // publisher outlives the cache TTL
client.avatar(PublicKey) -> Option<Vec<u8>>   // highest-revision claim across
                                              // stored + learned records; fetch
                                              // from the claim-carrier's relays,
                                              // verify (hash + AEAD), cache
```

`Received` carries the envelope (sender, conversation id, blob refs) and the opened
body as a `Result` — the *edge* decides presentation (print, webview, notification).
The actual signatures live in `crates/zink-client/src/client.rs`; this sketch is a map,
not a contract.

## Decisions

- **No tokio dependency.** All flows are plain `async fn`s awaiting iroh futures; the
  binaries own the runtime. Keeps the crate portable (single-threaded WASM later).
- **Storage stays `std::fs`**, gated `#[cfg(not(target_family = "wasm"))]` along with
  the rest of the native flows. The PWA client will need a storage port + IndexedDB
  adapter — abstracting that boundary now would be a speculative port with one
  implementation (STYLE: don't abstract before it pays).
- **WASM deps are target-scoped** (`wasm-bindgen` only for `wasm32`); the A6 spike
  moves to a wasm-gated module so `web/spike/build.sh` keeps working.
- **iroh with `default-features = false, features = ["tls-ring"]`** (the WASM-proven
  configuration) — also sufficient native, so all consumers share one iroh config.
- Contacts remain `(key, relay entries)` — the ContactRecord wire format and QR
  exchange are C2. Since D0b an entry pairs the mailbox dial string with the same
  service's iroh relay URL (`RelayEntry`); the resolved `Contact` used by sends
  carries the mailbox strings, the relay URL feeds dial-by-key.
- **Learned records are not contacts (D1b, who-is-this.md §5/§7).**
  `<key-file>.state/learned/<subject>/<responder>.record` (+ receipt-time sibling)
  holds `who-is` answers with provenance — multiple records per key, latest per
  responder. Network input never mutates the contact store; freshness is read-time
  relay resolution (subject-served > user-added > contact-served, latest within a
  class), sealing keys come only from the user-added record until D3, and the
  profile name-attestation revision persists in `profile.revision` (bumped per
  rename — SPEC §3.2 supersession).
- **Errors are one crate-wide enum (`zink_client::Error`, De1).** Precise variants
  where an edge or test branches (`NoRelayUrl`, `NotAContact`, `ProfileIncomplete`,
  …), kind-grouped variants with a human payload elsewhere (`Storage`, `Transport`,
  …), `#[from]` pass-through for the protocol's typed errors. Edges that speak
  `Result<_, String>` convert at the boundary via `From<Error> for String`
  (Display) — presentation stays at the edge.
- **Blob cache (C3a): ciphertext at rest.** `<key-file>.state/blobs/<hash-hex>` holds
  encrypted blobs exactly as relays do; every read re-verifies against the referencing
  envelope (`open_blob`), so the cache is trusted no more than a relay. Own blobs are
  cached at send time — they get pushed to the *recipients'* relays, so the local copy
  is the only one we can render our own history from.

## Non-goals (C1)

ContactRecord/QR, UI, IndexedDB adapter, live delivery (C4), any protocol change.
