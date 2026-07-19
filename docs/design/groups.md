# Groups: membership presentation & the unknown-key pipeline (D2)

Design for the D2 sub-slices. Downstream of [SPEC](../SPEC.md) §4 (the
conversation DAG — already multi-recipient since B1/B2) and
[who-is-this.md](./who-is-this.md) (the lookup + provenance machinery this
pipeline drives); feeds [D3 · Multi-device](./mvp-build-plan.md), which rides
this pipeline ("your new device" and "a new member" are the same event under
the hood, SPEC §5.2).

**This doc's job is defensive.** Everything hard already exists: fan-out
delivery (B2), group-capable ordering (B1), late-joiner healing (D0),
identity lookup with provenance (D1). **No new mechanisms**: the signed
`recipients` list *is* the membership announcement, `who-is-this` *is* the
lookup, and re-wrap (D3) is the only future wire op. If an implementation
step seems to need a membership object, a group id, an invite message type,
or a broadcast channel — stop; it doesn't.

## 1. Goal & non-goals

**Goal.** Conversations with N people feel first-class: create them, see who
they're with, add someone mid-conversation, and have an unknown key that
appears in one resolve to a name-with-provenance and an add-or-ignore choice
— without the user ever manually querying.

**Non-goals (deferred, with homes):** re-wrap so late joiners *read* the
backlog (D3 — until then a new member reads from their join onward, which is
honest); `same-person-as` clustering — "Alice added a device" vs "a wild
Charlie" (D3); third-party vouching in the popup ranking (D4); group *names*
(a purely local label is trivially client-side; a *shared* name is a
conversation-scoped convention someone must propose — neither blocks MVP);
any removal/leave *mechanism* (see §2 — stop-including is the model; there
is nothing to build, only honest rendering).

## 2. Membership = the heads' participant set

There is no membership object, by design — membership is a **lens on the
DAG**. The rule:

> **The conversation's current membership is the union, over its DAG heads,
> of each head's `recipients` ∪ `sender`.**

- **Adding** someone = sending a message with them in `recipients`. The next
  heads include them; membership grows. The signed core is the announcement
  — every participant learns the key from a sender they already trust.
- **Stop-including** someone = the soft removal this model supports: when no
  head addresses them anymore, membership shrinks. They keep the history
  they hold (you can't unsend — honest), and any participant can re-include
  them (tenet: no enforced membership; discretion, not control).
- **Concurrent heads disagree** → union: honest over-inclusion that
  converges when the fork merges. Forks are data, not corruption.
- **Partial views**: heads need a buildable DAG; when the genesis is missing
  (`load_dag` fails — pre-D0-heal window), fall back to the union over all
  stored messages. Best-effort, converges with sync.

**Why not the union over all messages** (today's `reply_contacts` /
`ConversationSummary` behavior): a union can only grow, which makes
stop-including impossible — one reply re-addresses everyone ever seen. The
heads rule subsumes it for stable membership (1:1s are unaffected) and
matches what a reply *means*: you address the conversation as it currently
stands. D2a moves `reply_contacts` and summaries onto it.

**Rendering the delta**: a message whose participant set differs from its
parents' renders a membership line ("+ Charlie", "− Dana") — *derived* from
the signed cores, not a message type. Bodies stay opaque; there is nothing
to parse.

**Non-contact members: address, don't trust** (resolved 2026-07-19).
Continuing a conversation never requires promoting its members to contacts.
Nothing about sending needs trust: the key to seal to comes from the
**signed cores** (the announcement — D1b's "sealing keys never come from
learned records" is unbent, since a learned record only ever supplies the
mailbox route), and the relay route comes from a contact record *or a
learned record* — exactly what the scoped auto-query (§4) produces.
Declining to add someone costs them only your naming and your serving gate:
they render as hex or a learned name with provenance, your D0c gate never
serves them, but they keep receiving your replies. The rejected
alternative — dropping non-contacts from the reply set — makes membership
viewer-dependent and interacts badly with the heads rule: one skeptical
participant's reply, as the momentary sole head, would shrink the group for
*everyone*. Removal stays a deliberate act (stop-including), never a side
effect of not trusting. A member with **no route at all** (auto-query
failed — the gate limit, §4) **stays in the sealed `recipients`
regardless** — sealing needs only the key; only *delivery* is skipped.
Dropping them from the signed list would shrink membership for everyone
through this reply's head — the same accidental-removal hazard, arriving
via routelessness instead of distrust (caught by the D2a e2e, 2026-07-19).
Membership is not deliverability: their copy stays fetchable via peer sync
once they have a route; the edge surfaces them as `unknown`, and only an
*all*-unroutable set refuses the send.

## 3. The participant-set index fix

Found at review (2026-07-19): `send`-by-contacts keys conversations on the
exact participant set; `send_in` threads into a conversation **without
updating that index**, while receivers' `remember()` *does* map the grown
set. After Alice adds Charlie via `send_in`, everyone's index maps
`{A,B,C}` → the conversation *except Alice's own* — her next send-by-name
to those people silently forks a fresh conversation while everyone else
threads the old one. An artifact fork, not an honest concurrency fork.

**Fix:** `send_in` saves the participants→conversation mapping for its
message's set, exactly like `remember` does on receipt — sender and
receivers then agree by construction. Mappings are latest-writer-wins per
set; older sets keep pointing at the conversation (already today's behavior
— sending to a subset threads rather than forks, and the compose flow's
"new chat" is explicitly conversation-creating, so no UX ambiguity is
added).

## 4. The scoped auto-query (the who-is-this.md §5 revision)

D1 resolved "no auto-query" because asking your contacts about a key
broadcasts *that you just heard from it*. The carve-out, agreed 2026-07-19:
**inside a conversation, the key's presence in the signed `recipients` is
already mutual knowledge among participants** — querying *those
participants* about it reveals nothing they don't know. This section is the
deliberate revision who-is-this.md §5 points to; D2b edits that doc when it
lands.

- **Trigger:** after a drain stores messages (the same seam as D0d's
  `auto_sync`, before the edge renders), any conversation member that
  resolves to neither a contact nor a fresh learned record — **in
  legitimate conversations only** (§6): a fabricated group must not make
  your client dial your contacts asking about a spammer's strangers.
- **Responders:** *only* that conversation's participants that are dialable
  (contact or learned records) — **introducing sender first** (the sender
  of the message that brought the key in), then the rest. Never the whole
  contact list: that's the privacy boundary the carve-out preserves.
- **Mechanics:** `who_is` grows a responder-scoped variant (trivial on
  De3's concurrent, capped shape). Results land in the learned store like
  any answer; no new event plumbing — edges already render names via
  `resolve_name`, so the popup data is simply *there* at render time.
- **Rate limit:** at most one auto-query per `(subject, conversation)` per
  client run (in-memory set), so a drain loop can't re-broadcast interest;
  the manual trigger stays for re-asking.
- **The responder-side-gate limit, documented:** the scoped query is
  answered only by participants who hold *you* as a contact (D0c gate,
  unchanged). In a group where you and the introducer aren't mutual
  contacts, the query comes back all-`NotHeld` and the key stays hex with
  a manual-add path — graceful degradation, not a blocker. If it ever
  bites at real scale, the fix is a per-op serving policy ("answer `WhoIs`
  about participants of conversations we share") which the gate structure
  already supports without wire changes. Decide then, not now.

## 5. The "a wild Charlie appeared" surface

The D1c banner/panel, generalized and made proactive:

- An unknown participant in a rendered conversation surfaces a popup/banner:
  *"a wild key appeared — added by Alice"*, with whatever the auto-query
  learned: candidates as in D1c (name, provenance, agreement), best first.
- Actions: **add as contact** (petname prefilled — the one explicit act,
  unchanged) or **ignore**. Ignoring persists a client-side dismissed set
  (state, not protocol) so the popup doesn't nag every open; a dismissed key
  still renders as hex in messages (honest), and the manual "who is this?"
  row remains as the un-dismiss path.
- At D3 the same popup gains the clustering upgrade ("Alice added a
  device") — nothing here needs redesign for it.

## 6. Conversation acceptance: the contributing-contact rule

When is an incoming conversation *legitimate*? (Resolved 2026-07-19.)

> **A conversation is legitimate iff at least one of your contacts has
> _contributed_ — authored a message you hold in it.**

The distinction that makes this spam-resistant: *presence in `recipients`
is attacker-controlled* — a spammer can list your friends for free —
*authorship is not* (signatures verify against keys you already trust).

- **Applied at presentation, never at storage or delivery.** Messages
  arrive in any order: the contact's first contribution may land *after*
  the stranger's message that introduced the group, so rejecting at
  delivery would destroy data a later arrival retroactively legitimizes.
  Triaged at render time, a conversation upgrades to the main list the
  moment a contact's message arrives.
- **This rule is the triage criterion for the parked unknown-sender
  quarantine** ("message requests", see the plan's parked section): a 1:1
  from a stranger is just its degenerate case — a two-party group with no
  contributing contact. One rule covers both; the bounded-quarantine *view*
  itself stays parked (pre-external-deployment), but D2 computes the
  predicate since the auto-query gates on it (§4).
- Pure client policy, revisable per client without coordination — like
  everything in this doc.

## 7. Compose & group UX

- New chat: contact **multi-select** (the compose `<select>` becomes
  checkboxes/chips); send creates the genesis with all selected.
- In-chat: an **"add to conversation"** action (contact picker) — sends the
  next message with the grown recipient set (a normal message; the user's
  text, or a bare add if empty bodies are permitted by the edge — no
  special type either way).
- Labels stay derived (petnames joined, unknowns as short hex), now from
  the heads-based membership. Group naming: deferred (§1).
- Reply-all is already the only reply there is (`reply_contacts`), now
  heads-based; unknown/unreachable participants stay surfaced, best-effort.

## 8. Slices

- **D2a · Membership core + index fix — done (2026-07-19).** Heads-based
  membership (one helper, DAG-first with union fallback) feeding
  `reply_contacts` + `ConversationSummary`; `HistoryMessage.{joined,left}`;
  the index fix landed in `finish_send` (every send records its sealed
  core's set — strictly more general than patching `send_in`); CLI
  `reply --add` + delta lines. The e2e sharpened §2: unroutable members
  stay in the sealed `recipients` (dropping them shrank membership through
  that head), and a shared relay masks routelessness (deposits fan out to
  every registered recipient), so the e2e runs Carol on her own relay.
  All done-when criteria met, incl. the never-promoted member reached
  through a who-is-learned route.
- **D2b · Scoped auto-query.** The `who_is` responder-scoped variant; the
  post-drain trigger with the rate limit, gated on the contributing-contact
  rule (§4, §6); who-is-this.md §5 revised. *Done when:* the plan's
  acceptance headless — B adds C to a conversation with A; A's client
  auto-learns C's record with zero manual action (`resolve_name` returns
  the candidate), A adds C and replies to all; a conversation with no
  contributing contact triggers no query.
- **D2c · Group UI.** Multi-select compose; add-to-conversation action;
  membership deltas + heads-based labels; the wild-Charlie popup with
  persisted dismissal (§5). *Done when:* a three-device (or
  two-devices-plus-CLI) group chat runs live: create, add, popup → add
  Charlie, reply-all reaches everyone.

## 9. Doc touchpoints when this lands

- who-is-this.md §5: replace the pending-revision blockquote with the
  resolved carve-out (D2b).
- mvp-build-plan.md: tick sub-slices as they land.
- SPEC: **nothing** — the protocol is untouched end to end, which is the
  strongest sign the model was right. (§4.4 already states membership =
  per-message recipients; §5.2 already states the device/member
  unification.)
