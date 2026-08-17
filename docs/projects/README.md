# Projects

Numbered, **time-bound** efforts — each has a goal and an end. A project's folder
holds its tracker (the slice checklist + the narrative record of what was done and
learned). Durable knowledge a project produces does **not** stay here: it
graduates to [`docs/design/`](../design/) (subsystem rationale),
[`docs/decisions/`](../decisions/) (ADRs), or SPEC §11 (protocol decisions). When a
project closes, its tracker is marked complete and kept as the record.

Start a new effort as the next number: `docs/projects/N-name/`.

| # | Project | Status | Tracker |
|---|---|---|---|
| 1 | MVP — protocol + relay + native client to a usable product | ✅ complete (2026-07-26) | [1-mvp/build-plan.md](./1-mvp/build-plan.md) |
| 2 | UI facelift — first coherent UX/visual pass over the app | ✅ complete (2026-07-26) | [2-ui-facelift/tracker.md](./2-ui-facelift/tracker.md) |
| 3 | Ports & adapters — time + transport behind traits (client-side) | ✅ complete (2026-08-14) | [3-ports-and-adapters/tracker.md](./3-ports-and-adapters/tracker.md) |
| 4 | Module split — carve `client.rs` (and `app/ui`), keep `Client` | ✅ complete (2026-08-15) | [4-module-split/tracker.md](./4-module-split/tracker.md) |
| 5 | Relay lifecycle — scan, heal, and honest delivery state | ✅ complete (2026-08-16) | [5-relay-lifecycle/tracker.md](./5-relay-lifecycle/tracker.md) |

*Projects 1–2 closed the same day; project 2 ran parallel to the tail of
project 1. Projects 3–4 are the post-MVP engineering-excellence iterations:
3 moved time and transport behind ports; 4 is the module split sequenced
after it, so the split lands on 3's clean seams.*
