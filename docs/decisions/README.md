# Architecture decision records (ADRs)

One decision per file, numbered, append-only. An ADR captures a **cross-cutting
architecture/implementation decision** and — crucially — *why* it holds and which
[design tenet](../DESIGN-PHILOSOPHY.md) it serves. It's the fast answer to "why is
the code shaped this way, and can I change it?"

## When to write one

Write an ADR when a decision is **load-bearing across the codebase** and not
obvious from the code: a client-wide convention, a transport/connectivity choice,
a stance that trades one property for another. If a future contributor might
undo it without knowing the cost, it deserves an ADR.

Do **not** write one for:
- **Protocol / wire decisions** — those belong in **SPEC §11** (the resolved-decisions
  ledger). Link to the ADR from there only if the rationale is broader than the wire.
- **Subsystem design** — that's a [`docs/design/`](../design/) doc.
- **Per-slice choices** — those live inline in the relevant project tracker.

Keep them short. A decision that needs ten pages is a design doc with an ADR
pointing at it.

## Format

Copy [`0000-template.md`](./0000-template.md). Next number, kebab-case title,
`git`-friendly. Never renumber or delete; supersede instead (mark the old one
`Superseded by NNNN` and link forward).

## Index

| # | Decision | Status | Tenets |
|---|---|---|---|
| [0001](./0001-native-first-client.md) | Native-first client (Tauri v2) over PWA/WASM for the MVP | Accepted | 5, 9 |
| [0002](./0002-self-wrap-own-history.md) | Self-wrap convention — sealing to your own key for readable history | Accepted | 3, 4 |
| [0003](./0003-per-user-homed-multi-relay.md) | Per-user homed, multi-relay connectivity (never a shared relay) | Accepted | 5, 9 |
| [0004](./0004-time-and-transport-behind-ports.md) | Time and transport behind ports in the client | Accepted | 6, 7 |
