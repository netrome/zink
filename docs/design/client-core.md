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
Client::open_or_create(key_path)              // app: silent first-run key creation
client.send(&[Contact], Vec<u8>, Vec<BlobDraft>) -> SendReceipt  // seal → deposit per
                                              // distinct relay (retry) → push blobs
client.recv(&[relay]) -> Vec<Received>        // register → page-fetch → dedup by id →
                                              // open → remember → ack each page
client.fetch_blob(&Received, &BlobHash) -> Vec<u8>              // cache, else the relay it
                                              // arrived through; verify + decrypt
// profile + contacts (C2): set_profile, my_record, add_contact, contacts,
// resolve_contact, register_at_home_relays
// stored history (C3a):
client.conversations() -> Vec<ConversationSummary>   // id, participant keys, count,
                                              // last timestamp — naming is the edge's
client.history(conversation) -> Vec<HistoryMessage>  // linearized; bodies opened per
                                              // message (self-wrap covers own sends)
client.fetch_stored_blob(conversation, message, &BlobHash) -> Vec<u8>
                                              // cache, else own home relays (that's
                                              // where senders push blobs for us)
// replying (C3b):
client.reply_contacts(conversation) -> ReplyContacts // participants → contact records;
                                              // keys without a record come back as
                                              // `unknown` (unreachable, surfaced)
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
- Contacts remain `(key, relay dial strings)` — the ContactRecord wire format and QR
  exchange are C2.
- **Blob cache (C3a): ciphertext at rest.** `<key-file>.state/blobs/<hash-hex>` holds
  encrypted blobs exactly as relays do; every read re-verifies against the referencing
  envelope (`open_blob`), so the cache is trusted no more than a relay. Own blobs are
  cached at send time — they get pushed to the *recipients'* relays, so the local copy
  is the only one we can render our own history from.

## Non-goals (C1)

ContactRecord/QR, UI, IndexedDB adapter, live delivery (C4), any protocol change.
