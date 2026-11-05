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
| Port shape | Verb traits composed under one umbrella, the transport.rs pattern: `Get`, `Put`, `Delete`, `List` → `Store`. Injected by generics with a default (`ClientState<S: Store = FsStore>`), never `dyn`. Futures are `+ Send`, like every transport port. |
| Keys | A typed `Key`: a non-empty sequence of validated segments (`[A-Za-z0-9_.-]`, non-empty, no leading `.`), so a key is a well-formed relative path by construction — no `..`, no separators, no hidden files. The fs adapter maps it 1:1 onto today's paths, which is what keeps the layout byte-identical. |
| Contract | `get` → `Ok(None)` when absent, `Err` only on a real failure. `put` replaces atomically (a reader sees the old bytes or the new, never a torn write) and creates parents implicitly. `delete` is idempotent. `list(dir)` returns the immediate children's names, sorted; a missing dir lists empty; the adapter's own temp files are invisible. **No multi-key atomicity and no durability promise** (no fsync today, none promised) — ordering across keys is the typed layer's job, and exactly what the fault double tests. |
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

- **S1 · The port, alone.** `ports/store.rs` (the traits, `Key`,
  `StorageError`), `adapters/fs_store.rs` (today's `write_atomic`
  mechanics, moved), the memory double, and **one contract suite run
  against both** — the trait plus its contract is the stable artifact,
  not any implementation (ADR 0004). Nothing calls the port yet.
- **S2 · `ClientState` over the port.** `ClientState<S: Store = FsStore>`,
  every method async, every `Client` call site `.await`s. A golden-layout
  test pins the fs adapter's on-disk paths and bytes against today's.
  The client test kit moves onto the memory double.
- **S3 · Crash-ordering tests.** Fault controls on the double (fail the
  n-th put, stop persisting after a point) and the tests the comments
  promise: `replace_contact`'s write-then-drop, contact-add + person row,
  merge/split ordering, the `persons()` self-heal, the lens applied-ops
  ledger's mark-after-effect.
- **S4 · Close.** The audit line ("no `std::fs` outside `adapters/`") —
  the CLI/app edges' own fs use is theirs; this is about the client core.
  ADR 0004 addendum (storage joins the ports), client-core.md, docs.

## Follow-ups

- `note_first_seen` reads the real clock inside `state.rs`
  (`system_clock::now_ms()`) — an ADR 0004 audit-line violation that S2
  surfaces; fix there by passing wall time in from `Client`'s
  `WallClock`.
