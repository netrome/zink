# Agent Instructions (zink)

zink is a **small, p2p-first chat protocol and its apps**, built on iroh 1.0. It is
**protocol-first**: clients and relays are independent implementations of an open
protocol. Optimize for: **simplicity, testability, minimal dependencies**, and staying
true to the design philosophy.

## Read these first
- `docs/DESIGN-PHILOSOPHY.md` — the *why*. The nine tenets are binding constraints.
- `docs/SPEC.md` — the protocol. Note §11 (resolved decisions) and §12 (phasing).
- `docs/STYLE.md` — code conventions.
- `docs/design/*.md` — detailed component/flow designs.
- `README.md` — project overview (if present).

## Core rules

- **NO FEATURE CREEP.** Implement only the requested slice. The docs define
  *constraints*, not a backlog. In particular these are **explicitly deferred** — do
  not build them unless scheduled: send-capabilities, personal tokens / economics,
  native clients, group crypto beyond fan-out (MLS/sender-keys), a gossip plane,
  service-worker message decryption, a cryptographic recovery anchor.

- **STAY TRUE TO THE PHILOSOPHY (it is the invariant set).** Do not violate a tenet in
  code. The load-bearing ones:
  - *Keys are the only identifiers; identity is local belief.* No central account,
    registry, or global namespace. "People" are a client-side clustering of keys.
  - *Enforce nothing; provide building blocks + discretion.* Never add enforcement,
    membership consensus, or global agreement mechanisms.
  - *Protocol = minimal primitives; clients own policy/UX.* Grouping, naming,
    display-ordering, membership presentation, trust-ranking, and petnames are
    **client-side** and must never enter the protocol or the `zink-protocol` core.
  - *Relays are untrusted.* Ciphertext + minimal metadata only. Never route plaintext
    through a relay; never put message content in a push notification.
  - *Best-effort over guarantees.* Assume partial views; never assume complete history
    or a global message order.
  - *Honesty over false order.* The causal DAG is the truth; don't fabricate a total order.

  If you believe a tenet or a resolved decision (SPEC §11) must change, **propose the
  doc change and call it out explicitly** — never encode the change silently in code.

- **CONTENT-ADDRESSING IS SACRED.** Hashed objects use canonical **BORSH**; a message
  id is `BLAKE3` of its canonical core. Changing a hashed struct is a wire/format
  change — bump the explicit `version` tag. Preserve determinism (same value → same
  bytes → same id); it's covered by tests and must not regress.

- **CRYPTO & SECURITY ARE NOT OPTIONAL.** End-to-end encryption everywhere; verify
  signatures before trusting anything; **never panic on malformed or hostile input**
  (return errors); private keys never leave a device unencrypted. Don't weaken the
  crypto model without a deliberate spec change.

- **I/O AT THE EDGES.** The `zink-protocol` core is pure — no network, async runtime,
  framework, or WASM types. External boundaries go behind **ports** (traits) with
  adapters injected at the edges. See `docs/STYLE.md`.

- **DEPENDENCY DISCIPLINE.** Avoid adding dependencies. If one is needed, justify it
  (and why the stdlib or an existing dep won't do). Prefer small, well-maintained crates.

- **VERSION EXPLICITLY.** Every hashed/wire object begins with a `version` tag. Add
  fields via a version bump; do not reserve unused fields "for later."

## Documentation
- `docs/SPEC.md`, `docs/DESIGN-PHILOSOPHY.md`, `docs/STYLE.md` are stable constraints —
  change rarely, deliberately, and announce the change.
- `docs/design/*.md` holds detailed designs, one file per component/flow.
- Record significant decisions in SPEC §11 or the relevant design doc.
- Keep docs in sync with code whenever behavior or the protocol changes.

## Work modes

### Conversation mode
Open-ended discussion of the system. Provide helpful responses; no code changes.

### Design mode
For exploratory/architectural work or anything too large for one PR.
- Default output is a single doc: `docs/design/<name>.md`. Docs-only unless code is
  explicitly requested.
- Consider at most 2–3 options; recommend one.
- End with a task breakdown of small, PR-sized items, each with acceptance criteria.
- If it touches the protocol, philosophy, crypto, or data model, or adds a significant
  dependency: update SPEC/PHILOSOPHY and call out the change (and any tenet impact).

### Feature mode
- The smallest change that satisfies the acceptance criteria.
- Avoid refactors; propose them as follow-ups instead of bundling them in.
- Write easy-to-test code and add focused tests (`// Given / // When / // Then`).

### Refactor / engineering-excellence mode
Must include: a clear motivation (what pain/risk it reduces), a safety net
(tests/golden files), and a bounded scope (what is explicitly *not* being touched).

### Review mode
Assume it compiles and clippy/tests pass. Correctness first:
- Logic errors? Does it meet the acceptance criteria? New bugs?
- Tests covering new/changed logic? Docs in sync?
- zink invariants held: no policy/UX in the protocol; signatures verified; no panics on
  hostile input; content-addressing determinism preserved; relay sees no plaintext.

Then readability: lean, locally-reasoned functions; accurate names; public-before-
private and callers-before-callees; clean separation of concerns / correct module homes.

## Development workflow (features & refactors)
1. **Respond with a plan first**: approach, files to touch, non-goals, risks.
2. Wait for confirmation; adjust the plan.
3. Implement exactly the plan.
4. Run:
   - `cargo fmt`
   - `cargo clippy --all-targets --all-features`
   - `cargo test` (or `cargo nextest run` if configured)
   - `node --test` for browser JS / service-worker modules (only when changed)
   - build the WASM client target when the client is touched
5. Update docs if behavior/protocol changed (SPEC / design / README).
6. Provide: summary of changes, tests added/updated, risks/limitations.

## What NOT to do
- No feature creep or future-proofing — none of the deferred features above.
- No policy or UX in the protocol layer or the `zink-protocol` core.
- No drive-by refactors.
- No new dependencies without justification.
- No trusting the relay: no plaintext through a relay, no content in pushes.
- No central registry, global identity, or enforced membership.
- No breaking content-addressing determinism without a version bump and spec change.
- No protocol or philosophy change encoded silently in code — update the docs and say so.
