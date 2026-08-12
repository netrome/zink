# zink docs — map

zink is a small, p2p-first chat protocol and its apps. The docs separate **durable
knowledge** (how the system works and why) from the **time-bound log** (what we did,
when). Start with the canon, reach for design/decisions when you ask "why is it
like this," and open a project only for the narrative of a specific effort.

## Canon — the constitution (read first, changes rarely)

| Doc | What it is |
|---|---|
| [DESIGN-PHILOSOPHY.md](./DESIGN-PHILOSOPHY.md) | The *why*. Nine tenets — binding constraints every decision answers to. |
| [SPEC.md](./SPEC.md) | The protocol: primitives, wire formats, §11 resolved decisions, §12 phasing. |
| [STYLE.md](./STYLE.md) | Code conventions (ports/adapters, testing, error handling). |
| [DEV-SETUP.md](./DEV-SETUP.md) | Toolchain setup — core, WASM, Android — and the relay deploy. |

Governance for how we work lives in the repo-root [AGENTS.md](../AGENTS.md)
(symlinked as `CLAUDE.md`).

## [design/](./design/) — subsystem rationale (Resources)

Durable "why the code is shaped this way," keyed by **subsystem**, independent of
which project built it. Edited in place as subsystems evolve. Many are cited from
code `//!` comments.

Sync & identity: [sync-primitives.md](./design/sync-primitives.md) ·
[who-is-this.md](./design/who-is-this.md) · [groups.md](./design/groups.md) ·
[multi-device.md](./design/multi-device.md) · [web-of-trust.md](./design/web-of-trust.md).
Delivery: [mailbox-rendezvous-push.md](./design/mailbox-rendezvous-push.md) ·
[mailbox-wire-protocol.md](./design/mailbox-wire-protocol.md) ·
[live-delivery.md](./design/live-delivery.md) ·
[direct-delivery.md](./design/direct-delivery.md) ·
[fast-failure.md](./design/fast-failure.md).
Client & data: [client-core.md](./design/client-core.md) ·
[dag-store.md](./design/dag-store.md) · [ui-design-system.md](./design/ui-design-system.md).

## [decisions/](./decisions/) — ADRs

One cross-cutting architecture/implementation decision per file, each tied back to
a tenet. The fast answer to "why is it this way, and can I change it?" See the
[index](./decisions/README.md). (Protocol/wire decisions live in SPEC §11 instead.)

## [projects/](./projects/) — time-bound trackers (Projects + Archive)

Numbered efforts, each with a start and an end. Trackers are the narrative record;
durable knowledge graduates out to the three sections above. See the
[index](./projects/README.md).

---

**The through-line:** trackers say *what we did*; canon, design, decisions and SPEC
say *how it works and why*. When a project closes, the knowledge graduates and the
tracker is archived.
