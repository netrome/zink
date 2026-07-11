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
Client::open(key_path)                 // load key (CLI keygen creates it)
Client::open_or_create(key_path)       // app: silent first-run key creation
client.send(&[Contact], text, Vec<BlobDraft>) -> SendReceipt   // seal → deposit per
                                       // distinct relay (retry) → push blobs (observed)
client.recv(&[relay]) -> Vec<Received> // register → fetch → dedup by id → open →
                                       // remember → ack per relay
client.fetch_blob(relay, &Received, hash) -> Vec<u8>            // fetch + verify + decrypt
```

`Received` carries plaintext bytes, sender, conversation id, and blob refs — the
*edge* decides presentation (print, JSON to a webview, notification text).

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

## Non-goals (C1)

ContactRecord/QR, UI, IndexedDB adapter, live delivery (C4), any protocol change.
