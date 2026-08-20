# Storage port: state I/O joins time and transport

> **Status: 💡 proposed (2026-08-20), to scope.** Trigger: project 7 S2
> grew the file-per-fact store again (`persons/`), and reading `state.rs`
> means reading layout, encoding, and fs mechanics at once. ADR 0004's
> port discipline stops at time / transport / timing-entropy today.

## TL;DR

Put a **narrow byte-level storage port** under `ClientState`: get /
atomic-put / delete / list under path-like keys, fs adapter now,
IndexedDB/OPFS later, one memory double with fault controls.
`ClientState` survives as the pure typed layer above it — layout,
encoding, invariants — with its API (and every `Client` call site)
unchanged, and the fs adapter byte-identical on disk (no data migration).

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
  temp-dir plumbing leaves the test kit.
- **The PWA client**: `std::fs` compiles on wasm32 and fails at runtime —
  the browser client cannot exist without this port. Its backends are
  async, which is the design question below.
- The audit line grows: "no `std::fs` outside `adapters/`".

## The load-bearing open question: sync or async

The browser forces async storage (IndexedDB is async; OPFS sync handles
live only in workers). Options: **(a)** an async port, asyncifying
today's sync reads (`contacts()`, `persons()`, `participant_labels` —
wide but mechanical; the edges are already async); **(b)** a sync port
now, revisited at the PWA (the migration paid twice); **(c)** a sync
facade over a worker (browser-only machinery). Settle in scoping, with
the PWA client's architecture in view — this question is why the project
is not a mid-project-7 shoehorn.

## Non-goals

- No schema, no database, no caching layer — the file-per-fact layout
  and formats stay exactly as they are.
- No behavior change anywhere; `ClientState`'s API holds.
- Not the PWA client itself — this unblocks it, nothing more.
