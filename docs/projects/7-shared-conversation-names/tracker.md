# Shared conversation names: my lens travels, theirs renders

> **Status: 💡 proposed (2026-08-20), to scope.** TL;DR to iterate on in a
> scoping session — not yet a tracker. Seam prepared by project 6 (S6 +
> tracker §8); the design conversation is summarized there and here.

## TL;DR

Name a chat "besties" and let the other participants *see* that name —
while a friend calls the same chat "me and my boiz" and everyone stays
sovereign over what they render. This is the **person-naming model
transposed to conversations** (the subject is a genesis id instead of a
key): my lens > peer suggestions rendered with provenance, nothing
auto-adopted without attribution — `Claim::Name`'s primitive-vs-policy
line, walked again.

## Fixed by prior decisions (project 6)

- **Carrier: inside the conversation.** A name claim sealed to the
  participants, riding the DAG — the membership-announcement precedent.
  Participants only; late joiners get it via backfill; relays see
  ciphertext. **Never in served records** — that would leak a
  conversation's existence to whoever pulls the record. Not a gossip
  plane (the deferred-list stance stands); the conversation is the carrier.
- **Local lens already shipped and stays sovereign** (S6): the `my-name`
  anchor-class sidecar; peer suggestions land in a separate learned-class
  store — no migration, precedence local > suggestion > joined-petnames.
- **Sharing default-on, per-rename optional**: "besties" you'd share; the
  "annoying coworkers" label stays local. The S6 members-panel name field
  grows the share choice.

## Where scoping starts

A **SPEC §11 proposal first**: at minimum a body-encoding agreement (the
claim may fit inside the sealed body — possibly no relay-visible wire
change at all), plus the called-out renegotiation of the "naming never
enters the protocol" invariant (the protocol gains a *primitive*, clients
keep the *policy* — argue it, never assume it). Implementation slices
follow the proposal.

## Open questions for the scoping session

- Body-format extension vs envelope change — how small can the version
  bump be?
- Rendering several suggestions: provenance rule, ordering, and the
  adoption default for a chat *I* haven't named (render the suggestion
  with attribution, or only in the members panel?).
- Abuse posture: a member who renames constantly (rate/last-writer
  presentation policy — client-side, but decide it).
- Does the CLI get any of this, or app-only at first?
- The body encoding is shared with project 8's own-device lens sync
  (advisory claims riding a conversation — there pointed at siblings, here
  at friends): design the carrier format once, sequence the two proposals.
