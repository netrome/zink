# Agent Instructions (zink)

zink is a **small, p2p-first chat protocol and its apps**, built on iroh 1.0. It is
**protocol-first**: clients and relays are independent implementations of an open
protocol. Optimize for **simplicity, testability, minimal dependencies**, and staying
true to the design philosophy.

We are **pre-MVP**: the goal right now is a working product. This file is deliberately
lean and build-focused; richer process (formal feature/refactor/review modes) comes
*after* the MVP works.

## Read these first
- `docs/DESIGN-PHILOSOPHY.md` — the *why*. The nine tenets are binding constraints.
- `docs/SPEC.md` — the protocol (§11 resolved decisions, §12 phasing).
- `docs/STYLE.md` — code conventions.
- `docs/design/*.md` — detailed designs, including `mvp-build-plan.md` (the current task list).
- `README.md` — project overview (if present).

## Invariants (never violate in code)

- **Keys are the only identifiers; identity is local belief.** No central account,
  registry, or global namespace; "people" are a client-side clustering of keys.
- **Enforce nothing; provide building blocks + discretion.** No enforcement, membership
  consensus, or global-agreement mechanisms.
- **Protocol = minimal primitives; clients own policy/UX.** Grouping, naming,
  display-ordering, membership presentation, trust-ranking, and petnames are
  client-side and never enter the protocol or the `zink-protocol` core.
- **Relays are untrusted.** Ciphertext + minimal metadata only; never route plaintext
  through a relay, never put message content in a push.
- **Best-effort over guarantees; honesty over false order.** Assume partial views; the
  causal DAG is the truth — don't fabricate a total order.
- **Content-addressing is sacred.** Canonical BORSH; a message id is `BLAKE3` of its
  core. Changing a hashed struct bumps the `version` tag; determinism (same value →
  same bytes → same id) is tested and must not regress.
- **Crypto & security.** E2E everywhere; verify before trusting; **never panic on
  malformed/hostile input** (return errors); private keys never leave a device unencrypted.
- **I/O at the edges.** `zink-protocol` core is pure (no network/async/framework/WASM
  types); external boundaries behind ports (traits), adapters at the edges. See STYLE.md.
- **Dependency discipline.** Avoid new dependencies; justify any addition.
- **Version explicitly.** Every hashed/wire object starts with a `version` tag; add
  fields via a version bump — don't reserve unused fields.

If an invariant or a resolved decision (SPEC §11) must change, **propose the doc change
and call it out** — never encode it silently in code.

## Current workflow: building the MVP

We build in **small vertical slices toward a runnable product**, walking-skeleton first.

- Each slice is the smallest step that ends in **something runnable**, with **focused
  tests** (`// Given / // When / // Then`, per STYLE.md).
- `docs/design/mvp-build-plan.md` is the slice checklist and shared task tracker. Keep
  it current: check off finished slices, add follow-ups.
- Scaffolding and dev tooling (e.g. a native CLI test-client) are welcome when they
  speed the loop or de-risk integration — these are dev tools, not shipped clients.
- No creep beyond the current slice; the invariants above always hold.
- Write a short `docs/design/<name>.md` only for a slice with genuine unresolved design
  — just-in-time, not upfront.

For each slice:
1. Briefly state it: what it adds, files touched, non-goals.
2. Implement it.
3. Run: `cargo fmt`; `cargo clippy --all-targets --all-features`; `cargo test`;
   `node --test` (browser/SW modules only); build the WASM target when the client is touched.
4. Show it running / tests passing.
5. Update `mvp-build-plan.md` and any docs whose behavior changed.

## What NOT to do
- No feature creep or future-proofing. **Explicitly deferred until scheduled:**
  send-capabilities, personal tokens / economics, native *shipped* clients, group crypto
  beyond fan-out (MLS/sender-keys), a gossip plane, service-worker decryption, a
  cryptographic recovery anchor.
- No policy or UX in the protocol layer or the `zink-protocol` core.
- No drive-by refactors; no new dependencies without justification.
- No trusting the relay: no plaintext through it, no content in pushes.
- No central registry, global identity, or enforced membership.
- No breaking content-addressing determinism without a version bump and spec change.
- No protocol or philosophy change encoded silently in code — update the docs and say so.

*(Formal feature / refactor / review work modes will be added post-MVP.)*
