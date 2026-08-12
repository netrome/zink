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
| 3 | Ports & adapters — time + transport behind traits (client-side) | 📝 scoping (2026-08-12) | [3-ports-and-adapters/tracker.md](./3-ports-and-adapters/tracker.md) |

*Projects 1–2 closed the same day; project 2 ran parallel to the tail of
project 1. Project 3 is the first post-MVP engineering-excellence iteration; a
module split follows it as `4-…` (sequenced after, so it lands on 3's clean
seams).*
