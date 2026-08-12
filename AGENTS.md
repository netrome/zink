# Agent Instructions (zink)

zink is a **small, p2p-first chat protocol and its apps**, built on iroh 1.0. It is
**protocol-first**: clients and relays are independent implementations of an open
protocol. Optimize for **simplicity, testability, minimal dependencies**, and staying
true to the design philosophy.

The **MVP is complete** (2026-07-26): protocol + relay + native Android/desktop client,
text + images, online and offline, with notifications. Work now proceeds as **post-MVP
iterations**, each a numbered project. This file is deliberately lean and build-focused.

## Read these first
- `docs/README.md` — the **doc map**: how canon / design / decisions / projects fit together.
- `docs/DESIGN-PHILOSOPHY.md` — the *why*. The nine tenets are binding constraints.
- `docs/SPEC.md` — the protocol (§11 resolved decisions, §12 phasing).
- `docs/STYLE.md` — code conventions.
- `docs/design/*.md` — durable subsystem design rationale ("why the code is shaped this way").
- `docs/decisions/*.md` — ADRs: cross-cutting architecture decisions, each tied to a tenet.
- `docs/projects/*/` — time-bound trackers; the current effort is the highest-numbered one.
- `docs/DEV-SETUP.md` — toolchain setup (core, WASM, Android) for building on a fresh machine.
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

## Workflow: post-MVP iterations

We build in **small vertical slices**, each ending in something runnable with focused
tests (`// Given / // When / // Then`, per STYLE.md). Each iteration is a numbered project
under `docs/projects/N-name/`; its tracker is the live slice checklist (highest number is
current — keep it current: tick finished slices, add follow-ups).

- **Separate the log from the knowledge.** Trackers record *what we did and when*;
  durable *how-it-works-and-why* graduates out — subsystem rationale to `docs/design/`,
  cross-cutting architecture decisions to `docs/decisions/` (an ADR), protocol decisions
  to SPEC §11. Don't leave load-bearing decisions buried in a tracker's slice notes.
- No creep beyond the current slice; the invariants above always hold.
- Write a short `docs/design/<name>.md` only for a slice with genuine unresolved design
  — just-in-time, not upfront. Dev tooling (e.g. the CLI test-client) is welcome when it
  speeds the loop or de-risks integration — dev tools, not shipped clients.

For each slice:
1. Briefly state it: what it adds, files touched, non-goals.
2. Implement it.
3. Run: `cargo fmt`; `cargo clippy --all-targets --all-features`; `cargo test`;
   `node --test` (browser/SW modules only); build the WASM target when the client is touched.
4. Show it running / tests passing.
5. Update the current tracker, graduate any durable decision to design/decisions/SPEC,
   and update any docs whose behavior changed.

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
