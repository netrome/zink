# Own-device lens sync (project 7 S6)

> **Status: designed 2026-08-23, landed with project 7 S6.** The op
> carrier defined here is the shared format project 8 (shared
> conversation names) extends — design once, sequence the proposals;
> project 8's scoping session owns the SPEC §11 body-encoding proposal,
> with this document as its input.

## 1. Goal & non-goals

My devices converge on my lens — person labels, contact adds — **by
default, never by assumption** (project 7 decision table). Divergence is
legitimate; convergence is best-effort; nothing arbitrates.

Non-goals: no shared object (each device applies acts in its own world);
no protocol change (relays see ciphertext, the encoding lives in sealed
bodies); no gossip plane (the conversation is the carrier); no lens data
in served records (it would leak the social graph); merge / split /
petname ops (the vocabulary has room; deferred until wanted).

## 2. The carrier: op frames

An **op frame** is a sealed message body of the form

```
b"zop\0" ++ borsh(OpFrame { version: u16, op: LensOp })

LensOp (client vocabulary, never zink-protocol's):
  0  Hello                                        — channel genesis marker
  1  LabelPerson { members: Vec<[u8;32]>, label: String }
  2  OfferContact { record: Vec<u8>, petname: String }
```

- **Versioned and appendable** like every wire object: new variants are
  appended; an unknown variant or version parses as "an op this build
  doesn't speak" and is ignored — an honest no-op, never an error.
  Project 8 appends its variants (a conversation-name claim) to the same
  frame; the frame is the shared carrier, the variants are per-proposal.
- **Parsing is never trust.** Anyone can send bytes that look like a
  frame; `try`-parse never panics (hostile-input rule), and *effects* are
  gated per op kind by the **envelope's verified author** — for every S6
  op: recognized own devices only. Author gating is client policy; the
  protocol carries opaque bytes, as always.
- **Rendering rule:** a body that parses as a frame renders as *nothing*
  in chat surfaces (like a bare membership change), whatever the author —
  hiding attacker-chosen bytes harms no one; the gates are on effects.
- The op vocabulary lives in `zink-client` (policy), encoded with the
  same borsh the protocol crate already uses — no new tree dependency.

## 3. The channel

The lens channel is an **ordinary conversation** whose genesis an own
device authored with a `Hello` body and no human recipients. The
send-to-self machinery (D3c) does the rest: recognized devices are
appended to every send, deposits give offline convergence, and
sync + backfill + re-wrap carry the whole history to a newly paired
device — no new transport, no new trust.

- **Classification is local:** genesis authored by an own key *and* its
  body opens to a `Hello` frame. Classified channels are recorded
  (`lens/channels`) as replay encounters them.
- **Emission target:** the stored channel (`lens/conversation`), created
  on the first act that emits; adopted on sight when a sibling created
  one first. If several classify (two devices created channels while
  apart), emit into the **smallest conversation id** — a deterministic
  tiebreak every sibling computes alike — while ops from *every*
  classified channel still apply. No shared object, so nothing has to be
  the one true channel.
- Emission is **stage-only** (synchronous): sealed, stored, outboxed;
  delivery rides the normal outbox flush. Acts stay fast and offline-safe.
- The audit trail is the DAG itself — which device did what, when. A
  viewer UI stays deferred (hidden-by-default affordance).

## 4. Ops and their effects

| Act (local) | Emits | Sibling effect |
|---|---|---|
| `rename_person` | `LabelPerson { person's keys, new label }` | resolve by key overlap; auto-adopt or conflict (§5) |
| `add_contact` (new entry) | `OfferContact { record bytes, petname }` | an **offer**, never a write (§6) |

`LabelPerson` speaks keys, never person ids — "label the cluster
containing K" (the cross-device-vocabulary decision): devices that
cluster differently each apply it correctly in their own world.

## 5. Adoption: replay, manual-wins, conflicts

Adoption is a **store-driven, idempotent replay**, not a
received-batch hook: after every drain and after re-wraps land, replay
classified channels in DAG-linearized order, skipping ops already in the
applied ledger (`lens/applied`) and ops this device authored (they took
effect at the act). Store-driven because the new-device bootstrap lands
envelopes via backfill whose bodies only open after re-wrap — a batch
hook would miss them.

For a sibling's `LabelPerson`:

- No key overlap with any person → ignore (someone this device doesn't
  hold; contact adds travel as offers, not labels).
- Label already current → converged, ledger and move on.
- **Auto-adopt** iff every lens op this device authored about the same
  person is an ancestor of the incoming op — the sibling demonstrably
  saw my state, so its edit is later, not competing. Renames made while
  a device was offline converge exactly here.
- Otherwise the edits are **concurrent: keep mine, surface theirs** —
  a conflict entry `{person, their label, which device}` renders with
  provenance ("your phone calls them X"); nothing arbitrates. A conflict
  clears when I rename the person (my act supersedes and emits) or take
  theirs (which *is* a rename). An adopted label that would collide with
  an existing label (`ensure_label_free`) also surfaces as a conflict —
  never forced.

## 6. Contact adds are offers

A sibling's `OfferContact` is stored (`lens/offers/<subject>`), keyed by
the record's first key — latest per subject wins; a record overlapping
an **existing contact is dropped** (already held — this also breaks the
re-offer loop, since accepting emits an offer of its own to the other
siblings). The offer renders with provenance ("your phone added X — add
them here too?"); **only the explicit accept writes the contact store**
— it is the ordinary `add_contact` on the carried record, so "the
contact store is never modified by network input" holds verbatim.
Decline drops the offer. **Repudiating a sibling voids its pending
offers** (dropped by author) — a compromised sibling could already read
everything; the offer gates the write surface.

## 7. Suppression

Classified channels leave the inbox at the client (`conversations()`),
so the CLI and app inherit the filter; op frames render as nothing in
chat surfaces; notifications were already silent for own-device senders
(the De8 sweep), which covers every S6 op. Nothing is deleted — the
channel stays stored, inspectable history.

## 8. Edges, noted and accepted

- The participant-set index maps the own-device set → the lens channel;
  no UI path today creates an own-set chat, so the collision is latent.
  If a note-to-self surface ever ships, it must skip that index entry.
- Offer batching ("your phone added 3 contacts") is presentation-only —
  deferred with the viewer UI.
- A hostile op frame in a normal chat occupies an unread count it never
  renders for — cosmetic, accepted.
