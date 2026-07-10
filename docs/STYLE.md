# zink — Code Style Guide

Conventions for the zink codebase (protocol core, relay, and client). Two things
matter above all: **simplicity** and **testability** — everything below serves them.
There's little code yet, so treat these as the conventions we adopt from the start;
follow them for all new code and refactors.

## Guiding principles

- The code should be as easy to read as possible. Prefer the obvious construction over
  the clever one.
- Business logic is **pure and testable**; I/O lives at the **edges**.
- Keep functions and modules small and single-purpose.
- Comments explain *why* when it isn't obvious — never restate *what* the code says.

## Architecture: hexagonal-lite

Ports & adapters, without the ceremony — we don't go all-in.

- A pure **protocol core** (`zink-protocol`): message/attestation types, BORSH
  encoding, hashing, the DAG, crypto primitives. No network, no async runtime, no
  framework types — just data in, data out. Most of the logic and most of the tests
  live here. Both the relay and the (WASM) client depend on it.
- **Ports** are traits describing what the domain needs from the outside — e.g.
  `Mailbox`, `PushSender`, `BlobStore`, `Transport`. Domain code depends on the trait,
  never a concrete implementation.
- **Adapters** implement ports against the real world (iroh connections, a Web Push
  sender, on-disk storage) and are injected at the call site, so the domain can be
  driven by fakes in tests.
- **Edges** are thin: the relay's protocol handlers and the client's WASM entry points
  / service worker extract input, call a domain function, and map the result out. Push
  logic down into the core where it can be unit-tested without a network.

**Pragmatic exception:** don't abstract a boundary before it pays. A small on-disk
store can take a `&Path` directly; reach for a trait once there's a second
implementation or a test fake to justify it. New *external* boundaries (network, push,
remote services) are trait-based from the start.

## Module organization

### Public before private

All `pub` / `pub(crate)` functions come before private `fn`s. A module's external
interface is more significant than its helpers, and should be what the reader meets
first.

### Callers before callees (order by occurrence)

Within a section, if A calls B, define A before B — high-level API before
implementation detail. A public type appears immediately above the function that
returns or takes it:

```rust
pub struct ResolvedIdentity { … }
pub fn resolve_identity(…) -> ResolvedIdentity { … }
fn walk_attestations(…) { … }   // private helper, below its caller
```

### Separation of concerns

Domain code (the protocol core) never depends on transport or framework types — iroh
connection types, HTTP status codes, WASM bindings. Those belong in adapters and edges.
A domain function takes and returns plain data:

```rust
// core: pure, unit-testable
pub fn order_for_display(messages: &[Message]) -> Vec<MessageId> { … }

// edge (relay handler): thin wrapper
let envelope = decode_envelope(bytes)?;
mailbox.deposit(envelope).await?;   // `mailbox` is a port
```

## Naming

- Descriptive names that convey intent (`resolve_identity`, not `resolve` or `process`).
- No redundant prefixes that repeat the module name.
- Struct fields are named for what they hold, not where they came from.

## Testing

### Naming

Double underscores separate subject from expectation; every test module carries
`#[allow(non_snake_case)]`:

```rust
fn subject__should_describe_expected_behavior() { … }
```

### Structure

Use `// Given`, `// When`, `// Then` sections. Omit only for trivial one-liner
assertions; prefer them for anything with setup.

```rust
#[test]
fn order_for_display__should_sort_concurrent_messages_by_id() {
    // Given
    let msgs = two_concurrent_messages();

    // When
    let order = order_for_display(&msgs);

    // Then
    assert_eq!(order, sorted_by_id(&msgs));
}
```

### Focus

One behavior per test. The name should tell you what broke without reading the body.

### Determinism (zink-specific)

Content-addressing depends on canonical encoding. Test the invariants directly: the
same logical value encodes to identical BORSH bytes and hashes to the same id, and
`decode(encode(x)) == x`. These are load-bearing for the DAG and must never regress.

### Hostile input (zink-specific)

The core parses bytes from the network. Parsing and verification functions must **never
panic** on malformed, truncated, oversized, or hostile input — they return errors. Test
with such inputs explicitly.

### Isolation

Tests that touch the filesystem use a shared temp-dir helper for a unique directory,
and clean up (`remove_dir_all`) at the end.

### JavaScript / service worker tests

Pure browser-asset logic (no DOM, no I/O) is unit-tested with the Node.js built-in test
runner — no third-party packages. Test files sit beside the module as `<name>.test.mjs`
and import it directly:

```js
import { test } from "node:test";
import assert from "node:assert/strict";
import { drainMailbox } from "./mailbox.js";
```

Run with `node --test`. Use the same `// Given / // When / // Then` structure. Keep
DOM-dependent code in a separate module so the testable logic stays pure and importable
under Node.

## Comments

Only when necessary. If a comment restates the code, delete it or rename the code so the
comment isn't needed. Good names and small functions beat comments.
