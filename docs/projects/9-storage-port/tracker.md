# Storage port: state I/O joins time and transport

> **Status: 🚧 in progress — scoped 2026-11-05.** Trigger: project 7 S2
> grew the file-per-fact store again (`persons/`), and reading `state.rs`
> means reading layout, encoding, and fs mechanics at once. ADR 0004's
> port discipline stops at time / transport / timing-entropy today.

## TL;DR

Put a **narrow, async, byte-level storage port** under `ClientState`:
get / atomic-put / delete / list under typed path-like keys, an fs
adapter now, IndexedDB/OPFS later, one memory double with fault
controls. `ClientState` survives as the typed layer above it — layout,
encoding, invariants — with the same methods, now `async`, and the fs
adapter byte-identical on disk (no data migration).

## Why project 3 didn't do this

ADR 0004's driver was determinism of *failure paths* — time and transport
were where untestable flakiness lived. Local-disk fs was already
deterministic under temp dirs and already concentrated in one seam
(`state.rs`). This project is the readability / testability /
portability half that project 3 deliberately didn't buy.

## What it buys

- **Readability**: `state.rs` methods become layout + encoding only; the
  I/O mechanics (`write_atomic`, `create_parent`, warn-and-skip) live
  once, in the port contract and adapter.
- **Testability**: the crash-ordering claims code comments currently
  assert untested ("a crash between these writes leaves a duplicate,
  never a lost contact") become tests against a fault-injecting double;
  temp-dir plumbing leaves the test kit. Project 7's eager person rows
  added more of these: the contact-add's paired writes (entry + person
  row), merge/split write ordering, and the `persons()` crash-gap
  self-heal are all crash-ordering contracts waiting for that double.
- **The PWA client**: `std::fs` compiles on wasm32 and fails at runtime —
  the browser client cannot exist without this port.
- The audit line grows: "no `std::fs` outside `adapters/`".

## Decisions (scoped 2026-11-05)

| Decision | Resolution |
|---|---|
| Sync or async | **Async.** The browser's backends are async (IndexedDB; OPFS sync handles live only in workers), and a sync port now would mean paying the migration twice at the PWA. The edges are already async, so asyncifying today's sync reads (`contacts()`, `persons()`, …) is wide but mechanical. |
| Port shape | Verb traits composed under one umbrella, the transport.rs pattern: `Get`, `Put`, `Delete`, `List` → `Store`. Injected by a defaulted generic (`ClientState<S = FsStore>`, the `S: Store` bound on the impl blocks, not the type), never `dyn`. Futures are `+ Send`, like every transport port. |
| Keys | A typed `Key`: a non-empty sequence of `Segment`s (1–255 bytes of `[A-Za-z0-9_.-]`, never `.`/`..`), so a key is a well-formed relative path by construction — no traversal, no separators. Leading dots stay legal: the legacy `persons/.next` counter must stay deletable through the port. The fs adapter maps it 1:1 onto today's paths, which is what keeps the layout byte-identical. |
| Contract | `get` → `Ok(None)` when absent, `Err` only on a real failure. `put` replaces atomically (a reader sees the old bytes or the new, never a torn write) and creates parents implicitly. `delete` is idempotent. `list(dir)` returns the immediate children's names, sorted; a missing dir lists empty; the adapter's own scratch files are invisible (they carry a `~`, outside the segment alphabet, so they can neither list nor collide with a key). A key names a value or a directory, never both: a put under a value or onto a directory fails, in the double too. **No multi-key atomicity and no durability promise** (no fsync today, none promised) — ordering across keys is the typed layer's job, and exactly what the fault double tests. |
| Failure typing | The port returns its own `StorageError`; `ClientState` maps it to `Error::Storage` where it surfaces today, and keeps today's collapse-to-absent on the advisory reads. No behavior change. |
| What stays out | The keystore (`keystore.rs`): private-key storage has its own at-rest-encryption concerns and is not a file-per-fact store — its own follow-up. Wasm `Send` bounds: shared with the transport ports, settled by the PWA project, not here. |

## Constraints fixed by canon

- No schema, no database, no caching layer — the file-per-fact layout
  and formats stay exactly as they are.
- No behavior change anywhere; `ClientState`'s methods keep their
  names, arguments, and results (only `async` is added).
- Not the PWA client itself — this unblocks it, nothing more.

## Slices

Ordered around project 10 (direct blob delivery), in flight on `main`:
S1 touches nothing it touches; S2 is the one slice that edits the same
call sites (`send.rs` and friends) and lands after phase B, or rebases
across it — the edits are mechanical.

- **S1 · The port, alone — done (2026-11-05).** `ports/store.rs` (the
  four verbs + `Store`, `Key`/`Segment`, `StorageError`),
  `adapters/fs_store.rs` (`write_atomic` mechanics, scratch names now
  `<name>~<pid>.<seq>`), the `TestStore` double, and
  `ports/store/contract.rs`: one suite of 12 contract clauses, expanded
  by `store_contract!` against both implementations. Nothing calls the
  port yet — the module declarations carry an
  `expect(dead_code)` that S2 must remove.
- **S2a · `ClientState` over the port — done (2026-11-05).**
  `ClientState<S = FsStore>` holds the store (bound on the impls); every
  method is async and builds keys from layout strings
  (`Key::parse("contacts/<hex>.record")`); `dag_of` became a free fn. The
  async ripple reached every `Client` read path and both edges: the CLI,
  and the Tauri app, whose arrival notifications (now store reads) run
  on a spawned task from the sync drain / direct-delivery callbacks.
  `Client`, `BlobGate`, and the sync server stay on `FsStore` — the store
  parameter arrives with S2b, which needs it. A golden test runs every
  writer once and pins all 35 resulting paths and their BLAKE3s, generated
  from the pre-port code: the layout is byte-identical, no migration.
  Deliberate deltas: `load_envelopes` on a conversation with nothing
  stored errors (was: errors only when the directory is missing);
  `save_relay_override` writes atomically and creates its parent (was a
  bare `std::fs::write`); storage error messages now name op + key.
- **S2b · The test kit onto the double — done (2026-11-05).** `Client`
  gained its fifth parameter (`S = FsStore`, bound on the impls), as did
  `SyncHandler` and `BlobGate`. The loopback kit (`loop_client`,
  `with_transport`) runs on `TestStore` with an in-memory device key — no
  temp dir, no key file — and so do the blob-gate tests; reopen tests
  share the store handle and the device seed explicitly
  (`ClientState::store()` is a test-only accessor; `TestStore::remove_under`
  stands in for a hand-deleted directory). First-seen is time-as-data:
  `store_envelope` takes `now_ms` from the caller's `WallClock`, and the
  sync server holds a clone of the client's, which also stamps its reach
  evidence — `system_clock::now_ms()`, the last ambient wall-clock read in
  the core, is gone.
- **S3 · Crash-ordering tests — done (2026-11-05).** `TestStore`
  journals every completed write; `crashed_after(n)` is the store a crash
  right after the n-th leaves (the port promises no multi-key atomicity,
  so a prefix is all a crash can assume). The kit's `crash_states` reopens
  a client on *every* prefix an act spans — exhaustive, not the one point
  a comment names — and refuses an act that wrote nothing (vacuous). No
  fail-the-n-th control: no claim needed one. Pinned: contact add (each
  entry renders once), merge and split (no entry lost, none twice),
  re-key (never lost; stays clustered), the lens replay (a crash anywhere,
  then the next replay converges — ledger-after-effect holds), and
  staging (a message in history is always owed). Two claims failed and
  were fixed by reordering writes, no new mechanism:
  - **Staging stored the envelope before its outbox entries**, so a crash
    between left a message in history that nothing would ever deliver,
    rendered as sent. Now blobs → ledger → envelope → index; an entry
    whose envelope never landed is dropped by the flush as unfulfillable
    (live-delivery.md §2 updated).
  - **A re-key dropped the old entry before re-pointing its person
    membership**, so a crash between stranded the new entry outside its
    cluster (the S2 no-dangle rule). Now new entry → re-point → drop old;
    the in-between state is the duplicate the comment always accepted.
- **S3b · Local-logic tests onto the doubles.** Of the tests on the
  production constructors (`Client::open_or_create` / `open_homed` — real
  iroh endpoint, real key file), a rough classification (2026-11-05) puts
  ~57 as local logic only (contact overlap, petname collisions, persons,
  vouches) and ~16 as genuinely networked. The local ones move to
  `loop_client`; their `dir_bytes` probes become store assertions. The
  networked ones — including the labelled real-network smoke tier
  (transport.md §8) — stay on the production constructors: they are the
  integration check that real iroh and `FsStore` work together. Self-
  checking: a misclassified test makes an unscripted dial, which
  `TestTransport` turns into a loud panic.
- **S4 · Close.** The audit line ("no `std::fs` outside `adapters/`") —
  the CLI/app edges' own fs use is theirs; this is about the client core.
  ADR 0004 addendum (storage joins the ports), client-core.md, docs.
