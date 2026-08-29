# Own-device lens sync (project 7 S6)

> **Status: designed 2026-08-23, landed with project 7 S6.** The op
> carrier defined in §3 is the shared format project 8 (shared
> conversation names) extends — designed once, as both trackers agreed;
> project 8's scoping session owns the SPEC §11 body-encoding proposal,
> with this document as its input.

## 1. The problem

In zink, "who someone is" lives entirely on your device: your contact
list, the names you call people, and which keys you group together as
one person. This local view is called the **lens** (it's yours — nothing
about it ever travels to other people or relays).

That's fine with one device. With two, the lenses drift apart: add Carol
on your phone and your laptop has never heard of her; rename "Kalle" to
"Carol" on the laptop and the phone keeps saying "Kalle". Before this
slice there was no mechanism at all — every add and rename had to be
repeated by hand on every device.

The goal: **your devices converge on the same lens by default, never by
assumption.** "By default" — an add or rename on one device should just
show up on the others. "Never by assumption" — devices can disagree
(one was offline, or you renamed differently on both), and when they do,
nothing may silently overwrite what you did by hand or invent an
agreement that doesn't exist.

Three standing rules shape any solution:

- **The contact store is never modified by network input.** A message
  arriving — even from your own phone — must not write your contact
  list. (A compromised sibling device can already read everything; this
  rule is what keeps it from *writing*.)
- **Relays are untrusted.** Whatever devices tell each other must look
  like any other ciphertext to a relay.
- **No shared object.** There is no "the" contact list living somewhere
  both devices reference. Each device applies changes in its own world;
  convergence is best-effort, not enforced.

## 2. The shape of the solution

Devices tell each other *what they did*, as small machine-readable
messages — **ops** — sent through an ordinary zink conversation whose
only participants are your own devices. "I labelled this person Carol",
"I added a contact, here's their record".

That's the whole trick: a conversation between your devices already has
everything sync needs, built and tested — end-to-end sealing (relays
see ciphertext), offline delivery via relay deposits, and history
hand-off to a newly paired device via backfill and re-wrap. No new
transport, no protocol change, no gossip plane.

The rest of this document is the four design questions that follow:
how ops are encoded (§3), which conversation carries them (§4), what
happens when an op arrives (§5–§6), and how all of this stays out of
your chat list (§7).

## 3. The carrier: op frames

An op rides in a sealed message body, marked so clients can tell it
apart from chat text:

```
b"zop\0" ++ borsh(OpFrame { version: u16, op: LensOp })

LensOp (client vocabulary, never zink-protocol's):
  0  Hello                                        — channel genesis marker (§4)
  1  LabelPerson { members: Vec<[u8;32]>, label: String }
  2  OfferContact { record: Vec<u8>, petname: String }
```

- **Versioned and appendable** like every wire object: new op kinds are
  appended to the enum; a frame this build can't decode is "an op we
  don't speak" and is ignored — an honest no-op, never an error. This is
  the carrier project 8 reuses: it appends its conversation-name claim
  as a new variant; the frame is shared, the variants are per-proposal.
- **Parsing is never trust.** Anyone can send bytes that start with the
  magic; parsing `try`s and never panics (the hostile-input rule), and
  an op only has an *effect* if its envelope's verified author passes
  that op's policy — for every op in this document: recognized own
  devices only. Author gating is client policy; the protocol carries
  opaque bytes, as always.
- **Rendering rule:** a body that starts with the magic renders as
  *nothing* in chat surfaces — whoever sent it, decodable or not. This
  is safe: hiding attacker-chosen bytes harms no one, because effects
  (not rendering) are where the gates are.
- The vocabulary lives in `zink-client` — ops are policy, and policy
  never enters the protocol crate. The encoding is the same borsh the
  protocol crate already uses, so no new dependency enters the tree.

## 4. The channel

The ops travel in the **lens channel**: an ordinary conversation whose
first message (its genesis) an own device authored with a `Hello` body
and no human recipients. Existing machinery does everything else — the
send-to-self convention (D3c) appends your other devices to every send,
relay deposits deliver to devices that were offline, and a newly paired
device receives the whole channel through the normal backfill + re-wrap
bootstrap.

- **Recognizing the channel is local:** a conversation counts as a lens
  channel when its genesis was authored by an own key *and* the genesis
  body is a `Hello` frame. Recognized channels are recorded
  (`lens/channels`) as the replay (§5) encounters them.
- **Where ops are sent:** into the stored channel (`lens/conversation`),
  created on the first act that needs one — or adopted on sight when a
  sibling created one first. If two devices each created a channel while
  apart, every sibling emits into the one with the **smallest
  conversation id** (a deterministic tiebreak they all compute alike)
  while still applying ops from every recognized channel. No channel is
  "the real one"; there is no shared object to fight over.
- **Emitting is local-only** (stage-only, synchronous): the op is
  sealed, stored, and queued in the outbox; actual delivery rides the
  normal outbox flush. Acts stay fast and work offline. Emission is
  best-effort: if it fails, the local act (the rename, the add) still
  succeeds — devices just diverge until the next act, and a warning
  says so.
- The channel's history doubles as an **audit trail** — which device
  did what, when, in ordinary DAG form. A viewer UI is deferred.

## 5. Renames: adopt or surface, never overwrite

The acts that emit, and what a sibling does on receipt:

| Act (local) | Emits | Sibling effect |
|---|---|---|
| `rename_person` | `LabelPerson { the person's keys, new label }` | adopt or surface (below) |
| `add_contact` (new entry) | `OfferContact { record bytes, petname }` | an offer, never a write (§6) |

`LabelPerson` speaks **keys, never person ids** — it means "label the
cluster containing these keys". Person entries are local and never
travel; a device that groups keys differently still applies the op
correctly in its own world (the cross-device-vocabulary decision from
the project tracker).

Adoption runs as a **replay**: after every drain, and again after
re-wraps land, each recognized channel is walked in DAG order and every
not-yet-applied op is processed (an applied-ops ledger makes this cheap
and exactly-once; a body that can't be opened yet is skipped *without*
being ledgered, so the new-device bootstrap picks it up once its re-wrap
arrives). Replay reads the store rather than the just-received batch
precisely for that bootstrap case: backfilled history doesn't arrive as
a live batch.

For a sibling's `LabelPerson`:

- **No key overlap with anyone I hold** → ignore. (Contacts travel as
  offers, not labels; this op is about someone this device never added.)
- **Label already matches** → converged; nothing to do.
- **Auto-adopt** iff every label op *this device* authored about the
  same person is an ancestor of the incoming op in the DAG. Ancestry is
  proof the sibling *saw* my edit before making theirs — theirs is
  later, not competing. This is exactly the offline case: phone renames,
  laptop was off, laptop drains later and adopts.
- **Otherwise the edits are concurrent: keep mine, surface theirs.**
  The conflict renders with provenance — "your phone calls them X" —
  and a *use theirs* button. Nothing arbitrates; taking theirs is just
  a rename (which emits, so the resolution travels too). Any rename of
  that person clears the surfaced conflict. A label that would collide
  with an existing name is surfaced the same way, never forced.

"Manual edits always win" falls out of the ancestry rule: an op that
didn't see my edit can't replace it.

## 6. Contact adds are offers

A sibling's `OfferContact` is **stored, not applied**
(`lens/offers/<subject>`, latest per subject). The People view renders
it with provenance — "your phone added Carol — add them here too?" —
and only the explicit accept writes anything: the accept *is* the
ordinary `add_contact` on the carried record, so the
never-modified-by-network-input rule holds verbatim. Decline just drops
the offer.

Two details keep offers tidy:

- A record overlapping an **existing contact is dropped on receipt** —
  already held. This also breaks the loop where accepting would
  re-offer: the accept emits its own `OfferContact` (it is an
  `add_contact`, after all), which every sibling that already has the
  contact simply drops.
- **Repudiating a sibling voids its pending offers** — they are dropped
  by author, and the read path additionally filters out offers from any
  author no longer recognized. The offer is the write-surface gate a
  compromised sibling would otherwise reach through.

## 7. Staying out of the chat surface

The lens channel is infrastructure, not a chat:

- Recognized channels are filtered out of the inbox inside
  `client.conversations()`, so the CLI and the app inherit the filter.
- Op frames render as nothing in the message list and row previews
  (§3's rendering rule) — this covers hostile frames in normal chats
  and, later, project 8's ops.
- Notifications were already silent for own-device senders (the De8
  sweep), which covers every op an honest sibling sends.

Nothing is deleted — the channel stays stored, inspectable history.

## 8. Edges, noted and accepted

- The participant-set index maps the own-device set → the lens channel;
  no UI path today creates an own-set chat, so the collision is latent.
  If a note-to-self surface ever ships, it must skip that index entry.
- Offer batching ("your phone added 3 contacts") is presentation-only —
  deferred with the audit-trail viewer.
- A hostile op frame in a normal chat occupies an unread count it never
  renders for — cosmetic, accepted.
- Merge / split / petname ops are deferred until wanted; the vocabulary
  has room (appended variants, §3).
