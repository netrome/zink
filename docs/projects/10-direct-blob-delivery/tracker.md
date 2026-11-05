# Direct blob delivery: referent before reference, then blobs peer-to-peer

> **Status: 🚧 in progress — scoped 2026-10-17; phase A (S1–S3) landed
> 2026-10-17, phase B to schedule.** Trigger: the project 7
> live run (2026-08-23) saw an image message arrive before the sender's
> blob push finished, so the receiver's first fetch missed the relay
> cache and the thumbnail stayed blank for the session. Phase A (S1–S3)
> fixes that race; phase B (S4–S7) closes the gap direct-delivery.md §3
> left open — blobs still ride the relay even when the message itself
> went peer-to-peer.

## TL;DR

Two halves of one rule: **a message is never deliverable anywhere before
its blobs are fetchable there.**

- **Phase A — the race.** The sender publishes the reference before the
  referent: `deliver_to_relay` deposits the envelope (which nudges the
  recipient) and pushes blobs *after*, and the direct `Deliver` runs
  before any relay work at all. Flip it: push blobs, then deposit; for a
  blob message, push to every relay before the direct handoff. The
  receiver independently stops treating a missing blob as a terminal
  error — relays evict, senders crash mid-fan-out, outbox flushes push
  later — so the fix is never load-bearing for honesty.
- **Phase B — direct blobs.** The sender pushes blobs to each recipient
  device over the iroh-blobs ALPN *before* `Deliver`, so the ack means
  "has everything" and an image message acked by every recipient never
  touches a relay. The one new component is the client **accepting**
  inbound blob pushes: a `BlobsProtocol` handler on the client endpoint,
  gated in the core like `Deliver` (contacts, recognized own devices,
  self), draining into the existing ciphertext cache.

## Decisions (scoped 2026-10-17)

| Decision | Resolution |
|---|---|
| Ordering is the sender's job | Referent before reference, per destination: blobs are content-addressed and self-contained, so a blob arriving before its envelope is harmless (an orphan is TTL-swept), while an envelope arriving before its blobs is a broken image. Applies to relays (push, then deposit) and, in phase B, to peers (push, then `Deliver`). The outbox flush inherits the order through `deliver_to_relay`. |
| Direct handoff waits for the push phase | For a blob message the direct `Deliver` runs after the blob pushes settle. Image sends were already push-dominated (direct-delivery.md §4 measured ~10 s); an instant envelope with a blank image is not a win. Text is untouched. |
| A missing blob is a state, not an error | Client policy, in the app layer: a `BlobUnavailable` thumbnail drops out of the in-flight map (so the next messages reload refetches it), renders a quiet "not available yet, tap to retry" placeholder, and gets a short bounded backoff through the app's clock. Never an error toast for a transient. |
| Push, not pull, for direct blobs | Pull-after (recipient fetches from the sender after `Deliver`) has no spam surface but costs a second dial per image, loses the instant render, and cannot discharge the relay because full-res is fetched lazily. Push-first mirrors the relay flow, reuses `push_blobs` (`DialBlobs::dial_blobs` already takes a `Peer`), and lets `discharged` drop its blobs-empty guard. |
| Accept side lives in the core | The adapter mounts iroh-blobs' `BlobsProtocol` and hands the core an inbound push with the authenticated peer key; the core decides. Admission mirrors `Deliver` (D0c): contacts, recognized own devices, self. No policy in the adapter. |
| Store meeting point | The client blob cache stays plain ciphertext files (`state/blobs/<hash>`, re-verified on read). Pushes land in a memory store and drain into the cache on completion — the on-disk layout does not change, and project 9's storage port is not pre-empted. ⚠️ Confirm iroh-blobs exposes a completion hook that makes the drain clean; else a small custom push handler over the blobs ALPN. |
| Unreferenced-blob bound | A contact can push bytes no envelope references. Phase B ships the cheap half — per-blob size cap, per-sender in-flight cap — and a sweep of cache entries no stored envelope references is a follow-up unless scoping finds it trivial. Contacts can already fill your mailbox; this is the same trust, not a new one. |
| Not a protocol change | SPEC §5.1 already says fan-out delivers "a message/blob"; only the as-built sentence ("Blob bytes still go to the recipient's relay cache") and direct-delivery.md §3 change. Same wire (iroh-blobs push), no new frames, no new dependency. |

## Constraints fixed by canon

- Relays are untrusted and dumb: no relay-side "hold the nudge until the
  blobs land" — the relay cannot know which blobs an envelope references
  and must not learn.
- Honesty over false order (tenet 7): the receiver renders "not yet
  available", never an error it cannot substantiate.
- The serving gate (D0c) is the only gate: a stranger's push is declined,
  and falls back to the mailbox path exactly as a stranger's `Deliver`
  does today.
- No peer blob transfer beyond push: no gossip, no blob discovery.

## Slices

### Phase A — the race

- **S1 · Referent before reference (sender) — done (2026-10-17).**
  `deliver_to_relay` pushes blobs, then deposits (split into
  `push_blobs_to` / `deposit_to`; the outbox flush inherits the order).
  In `send`, a blob message runs its relay pushes concurrently *before*
  `deliver_direct`; the deposits follow the handoff, and a relay whose
  push failed gets **no deposit** — the entry stays owed and the flush
  retries push-then-deposit, so the envelope is never deliverable
  anywhere its blobs are not fetchable. Text is untouched. Docs:
  live-delivery.md §2, direct-delivery.md §3, client-core.md flow line,
  SPEC §7 (one SHOULD + a recipient MUST-cope — called out at review).
- **S2 · A missing blob is a state (receiver) — done (2026-10-17).**
  `chat.rs`: `thumbs` holds a `Thumb` state (loading / ready /
  unavailable) instead of a URL. A miss re-asks on a 2 s / 6 s backoff,
  then settles as unavailable — rendered as "image not available yet ·
  tap to retry", never a toast — and every messages reload re-asks the
  unavailable ones (an arrival is when a late blob is likeliest there).
  The full-res viewer's tap failure says "not available yet" rather than
  surfacing a raw error. *Policy note:* reload-retry costs one fetch per
  unavailable blob per arrival; fine at chat scale, revisit if a relay
  that evicts wholesale makes it chatty.
- **S3 · Pin it — done (2026-10-17).** `TestTransport` gained a `WireLog`
  (every request and confirmed push, in order, across scripted dials,
  wired peers and blob caches) and `TestBlobs` replaced the panicking
  blob double: `cache(&key)` scripts a reachable cache that holds pushes
  and serves fetches, `hold(&key)` an unreachable one; unscripted blob
  dials still panic. Two regressions in `send.rs`: an image send pushes
  both blobs before the first `Deliver` *and* the deposit (verified to
  fail on the pre-S1 order), and a relay whose cache is down gets no
  deposit while the direct handoff still lands. The CLI image e2e stays
  the smoke.

### Phase B — direct blobs

- **S4 · Inbound blob port + adapter.** A verb-named accept port for
  pushed blobs (`Inbound`-shaped: peer key, hash, bytes), the iroh
  adapter mounting `BlobsProtocol` beside `SYNC_ALPN`, the memory-store
  staging and drain. Test double alongside.
- **S5 · Admission + caps (core).** Accept from contacts / recognized own
  devices / self; per-blob size cap; per-sender in-flight cap. Declined
  pushes are indistinguishable on the wire from a full cache.
- **S6 · Send orchestration + discharge.** Per recipient: push blobs,
  then `Deliver`; `discharged` requires every hosted recipient to have
  acked *and* taken the blobs; undischarged relays get S1's push → deposit.
- **S7 · Docs + live run.** direct-delivery.md §3 as-built replaced; SPEC
  §5.1 as-built sentence; client-core.md flow lines; two devices + relay
  live run of an image both ways, relay logs confirming no blob touched
  it when both peers were online.

## Open questions

- Should S1 order the thumbnail push before the deposit and let the
  full-res push follow it? It would shorten time-to-first-pixel on slow
  links at the cost of re-opening the race for the full-res tap. Default:
  no — all blobs first; revisit with numbers from S7's live run.
- The unreferenced-blob sweep (phase B): a `pushed_at` tag on cache
  entries plus the relay's sweep shape, or nothing until it bites?

## Non-goals

- No blob discovery or pull from peers; the relay cache stays the
  offline path.
- No change to the blob cache layout (project 9 owns storage).
- No receipts, no progress protocol; a push ack is `PushBlob::push`
  resolving, nothing more.

## Doc touchpoints as slices land

- live-delivery.md §2 ("deposit + blob push per relay") → push, then
  deposit, with the invariant named (S1).
- direct-delivery.md §3 "blobs still ride the relay": the direct handoff
  waits for the push phase (S1); replaced outright by S7.
- client-core.md `send` flow line: seal → push blobs → direct → deposit (S1).
- SPEC §7: one SHOULD — a sender makes a blob fetchable at a destination
  before the referencing envelope is deliverable there (S1, called out).
- SPEC §5.1 as-built sentence (S7).
