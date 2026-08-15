# Relay lifecycle: scan, heal, and honest delivery state

> **Status: 🟡 in progress (started 2026-08-15) — R1 ✅ R2 ✅ R3 ✅.** Project **5-relay-lifecycle**,
> picking up the project-4 §8 parked item ("stale relay entries make 'sending…'
> permanent") and the SPEC §3.6 freshness ⚠️. Trigger: a real relay migration
> (server reinstall, 2026-08-15) hit three compounding walls — see §1.

Governed by the standard slice discipline (AGENTS.md): small vertical slices,
each runnable and tested before the next. This project is **client + app +
relay-binary presentation only**: no wire format change, no `zink-protocol`
core change (the one protocol idea it surfaces is deliberately parked as a
SPEC proposal, §8). The one dependency addition is `qrcode` into `zink-relay`
— already a workspace dependency (CLI + app use 0.14.1).

## 1. Why — and what this is *not*

A relay was reinstalled at a new address. The recovery flow failed three
independent ways, and together they made the migration feel broken rather
than routine:

- **Rescan rejected.** `add_contact`
  (`crates/zink-client/src/client/contacts.rs`) treats key overlap as an
  update of that contact, gated on a matching petname — the explicit confirm
  that stops a hostile record smuggling a contact's key from rewriting that
  contact's trust anchor. But when the scanner's name field is empty, the
  petname **defaults to the record's current self-claimed name**. The contact
  had renamed their profile, so the derived petname no longer matched the
  stored one → `ContactOverlap` error. The guard is right; the default turns
  "no opinion about the name" into "rename attempt", which trips it. (Typing
  the *old* petname would have worked — nobody will ever guess that.)
- **No manual repair path.** A contact's relays exist only inside their
  stored `ContactRecord` and the learned store. The Person view edits petname
  and local avatar; own relays are editable and live-apply
  (`client/profile.rs` insert/remove relay) — a *contact's* relays have no
  equivalent. With the rescan rejected, there was no way through the UI at
  all.
- **"sending…" forever.** `pending` is "an outbox entry exists"
  (`state.rs::pending_messages`); entries are keyed `(message,
  relay-dial-string)`, frozen at stage time. Every flush re-deposited to the
  dead relay for the full `OUTBOX_GIVE_UP_MS` (30 days) — after which entries
  stop retrying but **stay pending**. Direct delivery couldn't discharge them
  either: dial-by-key rendezvouses through the same dead `relay_url`
  (`contacts.rs::peer_addr_for`). Meanwhile the peer — who held *our* fresh
  record — dialed in and pulled the messages via sync, so the conversation
  worked while the marker claimed otherwise. Tenet 7 in spirit: the UI shows
  "sending…" for messages the peer already holds.

The architecture anticipated all of this: SPEC §3.6 flags relay freshness
with a ⚠️, `effective_relays` already resolves relays at read time by
provenance class (subject-served > user-added scan > hearsay), the learned
store exists precisely so records can accumulate without mutating trust
anchors, and multi-relay is plumbed end to end. **The primitives exist; the
healing loop and the UI seams don't.** That is the whole project.

**What this is not.**
- **Not a protocol project.** The message-borne record-revision hint SPEC
  §3.6 names is the one wire-touching idea here; it is parked (§8) until the
  client-only healing shows what gap remains.
- **Not a relay feature project.** The relay stays dumb and untrusted; it
  gains a QR *print* of what it already prints, nothing served, nothing
  stored.
- **Not the outbox-backoff work** (live-delivery.md known-remaining) and
  **not De6e presence** (declined on privacy grounds, SPEC §11) — both stand
  as parked.

## 2. Goal & non-goals

**Goal.** Relays stop being assumed static. The user-visible contract:
**plumbing, not homework** —

- a relay migration is *start the new relay, scan its QR, done*;
- contacts converge on your new relays automatically, over authenticated
  channels that already exist, with no action on their side;
- when delivery is genuinely stuck the app says so honestly and offers the
  repair, instead of an eternal "sending…";
- every automatic mechanism has an inspectable, overridable manual
  counterpart: own relays (exists), a contact's relays (new), send state
  (new).

**Non-goals.**
- No wire/protocol/`zink-protocol` change; no new hashed or wire fields.
- No trust-ranking of relays, no relay directory or registry (invariant: no
  central anything).
- No automatic `who_is` to **third parties** — the who-is-this.md §5 privacy
  stance stands untouched; §6's healing talks only to the subject.
- No per-entry outbox backoff, no relay presence query (both stay parked).
- No group-crypto or membership work; groups are exercised only as fan-out
  recipients of the outbox changes.

## 3. Baseline (measured 2026-08-15, project-4 close)

Suite: **239/239, ~1.1 s wall**, clippy clean, `wasm32` + `app/ui/build.sh`
clean — the regression floor.

Behavior baseline, reproducible today:

| # | Fact | Where |
|---|---|---|
| B1 | Rescan of a renamed contact errors `ContactOverlap` unless the old petname is typed | `contacts.rs::add_contact` petname default |
| B2 | No UI/client path edits a contact's relays; rescan (B1) and `who_is` answers are the only movers | `person.rs`, `contacts.rs::effective_relays` |
| B3 | Outbox entries pin the dial string staged at send time; a relay change strands them pending for 30 days, then silently forever | `state.rs::add_outbox/clear_outbox`, `outbox.rs::flush_outbox` |
| B4 | Direct delivery to a peer whose stored record names a dead relay fails at rendezvous — the stale record kills both paths | `contacts.rs::peer_addr_for`, `backfill.rs` |
| B5 | The app renders `pending` but not `confirmed` (CLI shows both) | project-4 §8, `app/dto`, `chat.rs` |
| B6 | `zink-relay` prints `relay spec:` lines; adding a relay to a phone means transcribing one | `zink-relay/src/main.rs` |

Kept and built on (not rebuilt): multi-relay everywhere, `effective_relays`
provenance classes, the learned store, `who_is`, live-applied own-relay
changes, the loopback/scripted-double test kit.

## 4. Guardrails — the traps to avoid

- **The contact store is never modified by network input** (who-is-this.md
  §5). Healing writes the *learned* store (subject-served class) only;
  rescan-update and manual override remain explicit user acts. If a slice
  finds itself mutating a stored record on network input, it's wrong — stop.
- **Sealing keys never move by healing** (who-is-this.md §7). Everything here
  moves *relays*; key-set changes still require an explicit re-add until D3.
- **Auto-queries go only to the subject, about themself, over an
  already-authenticated connection.** Asking a peer "who are you" on a
  channel they opened leaks nothing. Asking anyone else anything
  automatically is the §5 privacy line — do not cross it.
- **Honesty over reassurance (tenet 7).** Send-state wording reflects
  evidence we hold: deposited, direct-acked, confirmed, or can't-reach.
  Confirmations stay positive-only; "stuck" is a fact about *our deposits*,
  never a claim about *their receipt*.
- **The relay stays dumb.** The QR is a presentation of the existing spec
  string. No new relay op, no state, no client-list.
- **One scanner, payload decides.** Prefix-routed (`ZINK:` /
  `ZINK-RELAY:`); never two camera flows for the user to choose between.
- **No silent guard-loosening.** B1's overlap guard exists for a real attack
  (key smuggling). The fix moves the explicit confirm somewhere humans can
  operate it (a preview + confirm), it does not remove the confirm.

## 5. Graduation plan

- **`docs/design/relay-lifecycle.md`** — written when the genuine design
  lands (R2/R6): outbox re-resolution semantics, the override provenance
  class, the subject-refresh policy (triggers, rate limit, the privacy
  argument). Cited from `//!` in `outbox.rs` and wherever the refresh lives.
- **who-is-this.md §7** — the override class joins the read-time precedence
  list.
- **SPEC §3.6** freshness paragraph and **mailbox-rendezvous-push.md §4 ⚠️**
  — updated to name the shipped mechanisms; the revision-hint sentence
  becomes a pointer to the parked proposal (§8).
- **ADR** — only if the "records heal over authenticated channels" stance
  proves cross-cutting; decide at close.
- **Projects README** row updated at close.

## 6. Slices

**DoD (every slice):** builds · `cargo fmt` + `clippy --all-targets` clean ·
suite green, floor held (~1.1 s) · `wasm32` compiles when the client is
touched, `app/ui/build.sh` when the app is · tracker updated · graduations
recorded per §5.

**Tier 1 — stop the bleeding**

- [x] **R1 · Rescan is an update.** ✅ 2026-08-15. The seam is a read-only
  triage plus a separate explicit act, `add_contact` untouched:
  `preview_contact(record)` → `RecordMatch::{New{suggested_petname},
  Update(RecordUpdate), Ambiguous{petnames}}` — the `RecordUpdate` diff
  (stored → scanned: self-claims, relay specs ±, device-key counts ±) is
  what a confirm renders — and `update_contact(record)`, which takes **no
  petname at all**: an update never renames, so the parameter doesn't
  exist (sharper than the sketch's "empty petname means keep"). All three
  triage on one extracted `overlapping()` helper — the multi-device.md §4
  identity rule now has a single definition. App: every scan/paste routes
  through preview first; an overlap renders the confirm card ("this code
  belongs to **X**" + change lines + "your name for them stays …"), and
  the confirm invokes `update_contact` — the explicit act relocated from
  retype-the-petname to one tap. New `AddPreview` DTO (render-ready
  change lines composed at the command layer); ambiguous records error
  with the message storing them would produce. CLI: `contact-update
  --key <file> <ZINK:...>` — the command name is the confirm. Six new
  tests (preview new/diff/span; update keeps-petname — the B1
  regression — /no-overlap/span, store-untouched pinned by `dir_bytes`);
  the hostile-record drill unchanged — the guard didn't loosen, its
  confirm moved. **Proof:** 245/245 (was 239), clippy clean, `wasm32` +
  `app/ui/build.sh` + src-tauri check clean; live drill against two local
  relays: added under relay A as "Anna" → renamed to "Ann" + migrated to
  relay B → plain re-add still walls (`ContactOverlap`) →
  `contact-update` heals — petname kept, stored record on relay B.
  **B1 is dead.** (App card is build-verified; this box is headless —
  eyeball it on the next desktop/phone run.)
- [x] **R2 · The outbox follows the record.** ✅ 2026-08-15. Shipped as
  **reconcile-at-flush over the unchanged `(message, relay)` ledger** (the
  keying decision, §7): every `flush_outbox` first re-derives each pending
  message's owed relays — sealed recipients through `effective_relays`,
  exactly as a fresh send — releases entries no longer owed, and adds
  entries for newly-owed relays **inheriting the message's original age**
  (a moved record must not reset the give-up clock). Settlement by proof
  of possession: a relay whose every hosted recipient has a persisted De7
  ack is released without a deposit — except for blob messages (bytes come
  from the recipient's relay cache, so the deposit is still owed; the
  send-time skip's guard, mirrored). Recipients resolving to *no* relays
  (raw-spec sends, lost records) keep every staged entry — debt is never
  dropped on no evidence. `FlushReport` gained `released` (settled ≠
  `delivered` — honesty kept). Pre-R2 ledgers migrate by being flushed
  once; membership is never re-litigated. Five new tests: the B3
  migration regression (stage to silent A → R1 `update_contact` to B →
  flush deposits to B, A released un-redialed), ack-release without a
  dial, the blob guard, the raw-spec fallback, and age-inheritance under
  a jumped `TestWallClock` (expired re-target stays surfaced, undialed).
  Design graduated to **`docs/design/relay-lifecycle.md`** §1–§4 (the
  ledger owes recipients, not dial strings; sync pulls deliberately don't
  count as possession — §8); live-delivery.md §2 updated in place.
  **Proof:** 250/250 (~1.1 s; was 245), clippy clean, `wasm32` compiles,
  the real-network outbox smoke (relay restart at the same dial string)
  green. **B3 and the direct-ack half of B4 are dead.**
- [x] **R3 · Honest send states in the app.** ✅ 2026-08-16. The client
  now exposes *since when*, not just *whether*: `HistoryMessage.pending:
  bool` became `owed_since_ms: Option<u64>` (the oldest outbox entry's
  stamp — one field, nothing to drift; the CLI's `[pending]` rides the
  new field unchanged), sourced from `ClientState::pending_since`;
  `OUTBOX_GIVE_UP_MS` went public so edges render the same boundary the
  flush enforces. The `Message` DTO gained `stuck` and `undelivered`,
  computed at the command layer — stuck = owed > 10 min (edge policy,
  deliberately not client state), undelivered = past give-up. Chat cue
  chain, our-deposit facts only: *sending…* (young) → *⚠ can't reach
  their relay* (stuck — tappable in a 1:1 chat, opening the person page
  via the label-is-a-petname match; groups get plain text) →
  *undelivered — no relay took it in 30 days* (given up). `confirmed`
  ("✓ delivered to X") turned out to **already render** — project-4
  §8(a) was stale before this project started; only (b)'s wording landed
  here. **Proof:** 250/250, clippy clean, `wasm32` + `app/ui/build.sh` +
  src-tauri check clean. **B5 is dead** — a dead relay now produces a
  visible, actionable state instead of eternal "sending…". (Cues are
  build-verified; eyeball on the next desktop/phone run.)

**Tier 2 — the management surface**

- [ ] **R4 · Relay QR + one scanner.** `zink-relay` prints a terminal QR
  (Unicode half-block; `qrcode`, already in the workspace) of
  `ZINK-RELAY:<spec>` beside the existing `relay spec:` lines — which
  sockets get one decided in-slice (lean: each, they're two). App: the
  existing scanner routes by prefix — `ZINK:` → contact flow (unchanged),
  `ZINK-RELAY:` → "add to my relays?" confirm in Me; paste accepts the
  prefixed form anywhere a spec is accepted, CLI included. *Done when:*
  phone-adds-relay is scan → confirm, zero transcription.
- [ ] **R5 · Per-contact relay panel.** Person view shows the *effective*
  relays with provenance and freshness — "from your scan · Jul 26", "served
  by them · yesterday", "you set this · today" — plus last-deposit outcome
  per relay, the existing refresh (`who_is`) action, and add/remove of a
  **manual override**. Client: overrides live beside the stored record
  (never inside it — the scan stays immutable evidence, §4), a new class in
  `effective_relays`; rank decided in-slice (§7 has the lean and the
  tension). *Done when:* B2 is dead — the wall in §1 has a door.

**Tier 3 — self-healing + close**

- [ ] **R6 · Subject-refresh over live channels.** Whenever an authenticated
  connection to a contact exists — inbound serve, successful outbound dial,
  direct-delivery ack — run `who_is(them)` *against them*, rate-limited
  (order-of-daily per contact; eager when their deposits are currently
  failing — exact policy decided in-slice). Answers land as subject-served
  learned records, which already win read-time resolution — so with R2,
  sends heal with **no user action on either side**. Write
  `relay-lifecycle.md` here (§5). *Done when:* the §1 scenario heals with no
  rescan: two contacts who merely keep chatting converge on the new relay.
- [ ] **R7 · The migration drill + graduate.** One in-process scenario test
  on the loopback/doubles kit replaying 2026-08-15 end to end: two contacts
  chatting → one side's relay replaced *and* profile renamed → prove each
  layer independently (rescan-as-update path, outbox re-target, subject
  refresh) and that the send marker converges to the truth. Re-measure,
  graduate per §5, README row.

## 7. Decisions log

| Decision | Resolution |
|---|---|
| Rescan guard | **Kept, confirm relocated.** The petname-match confirm was the right rule with unusable ergonomics (retype a petname nobody remembers). The explicit act becomes a rendered preview + confirm; the client API distinguishes new-vs-update so no layer auto-rewrites an anchor. (R1) |
| Empty petname on update | Resolved sharper at R1: `update_contact` has **no petname parameter** — an update never renames, so the API can't express the mistake. The self-claim default survives only for genuinely new contacts, where it's the right prefill. |
| Where overrides live | Beside the record, as a new provenance class in `effective_relays` — never mutating the stored record (immutable evidence, who-is-this.md §5). Relays are unsigned in the record, so nothing cryptographic is at stake; provenance honesty is. (R5) |
| Override rank | **Open — decide in R5.** Tension: the petname precedent says manual wins; but a manual override outranking subject-served answers can go stale and recreate this project's disease. Lean: manual wins while it works, and R3's stuck-surfacing is the honesty valve. |
| Outbox keying | Resolved at R2: **keep `(message, relay)`, reconcile at flush.** The file format never changed, so pre-R2 ledgers migrate by being flushed once; per-recipient state lives nowhere because it's derivable (sealed recipients + acks + current records). Re-keying by recipient was rejected as a stored duplicate of what reconciliation computes. |
| Stuck threshold & wording | Resolved at R3: the client exposes the *fact* (`owed_since_ms`); thresholds are **edge policy** — stuck at 10 min (a relay restart never alarms), undelivered at the public `OUTBOX_GIVE_UP_MS`. Wording is our-deposit-only ("can't reach *their relay*", never "not delivered"); confirmation stays positive-only. |
| Relay QR payload | `ZINK-RELAY:` + the existing spec string, no new encoding — the spec format is already the versioned artifact. One scanner, prefix-routed. (R4) |
| Subject-refresh policy | **Open — decide in R6.** Triggers (which connection events), rate limit, and the failure-eager mode. Fixed now: subject-only, existing-connection-only, learned-store-only. |
| Revision hint on messages | **Parked** (§8) — a wire change through SPEC §11, proposed only after R6 shows the residual gap (receivers who never get a live channel to the migrated peer). |

## 8. Follow-ups / parked

- **Message-borne record-revision hint** (SPEC §3.6 already names it): sender
  stamps its profile revision; a receiver seeing a newer one pulls the record
  from the subject. Covers the passive-receiver gap R6 can't. Envelope
  version bump → a SPEC §11 proposal, after R6 evidence. Never encode it
  silently (AGENTS.md).
- **Per-entry outbox backoff** (live-delivery.md known-remaining) — R2
  changes what entries *target*, not retry cadence; unchanged, parked.
- **Old-relay grace period** is a deployment practice (run old + new during
  migration; SPEC §3.6 tolerates the window) — add a line to the deploy notes
  when R4 touches relay docs; no code.
- **Partial direct-ack in groups** — R2 shipped this per-relay (a relay is
  released when every recipient *it hosts* has acked), so the residue is
  only "some of one relay's recipients acked, the rest unreachable" — that
  stays pending and surfaces in R3's stuck state honestly. Revisit only if
  the drill (R7) shows worse.
- **Sync pulls are not possession** (resolved at R2, relay-lifecycle.md §3):
  serving a message to its recipient over `get`/`get-successors` doesn't
  settle the outbox — a request isn't a durable-store ack. Closing that
  last window honestly means an explicit ack op on the peer ALPN — a SPEC
  §11 proposal, only if it bites in practice.
- **De6e presence query** — declined stance stands (SPEC §11).
- **`my-record` can't print while a home relay is down** (found by the R1
  drill): the CLI hard-fails on `register_at_home_relays()?` *before*
  printing the payload — so a user whose relay just died can't produce the
  very record/QR that would re-home them. Registration should be
  best-effort for the print path. Natural home: R4 (it touches this
  command anyway).
