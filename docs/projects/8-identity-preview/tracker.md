# Identity preview: tap any identifier, see who this is

> **Status: 💡 proposed (2026-08-20), to scope.** TL;DR to iterate on in a
> scoping session — not yet a tracker. Trigger: real use, 2026-08-20 — a
> phone paired one-way to a laptop; the laptop receives fine but offers no
> navigable way to inspect the sender or act on it (the wild-key panel is
> inline-in-chat and transient).

## TL;DR

An identity page keyed by **key** (not petname), reachable by tapping any
identifier anywhere — message sender lines, member-panel rows, wild-key
rows. One page that renders everything this device already believes about
a key, and offers the acts: add as contact, ask who-is (manual), ignore,
and — for the one-way pairing case — "pair back" routed through Me's
fingerprint confirm.

## What the page renders (all read-time, local stores only)

The §1 belief layers, generalized to a non-contact: their self-claim
(learned store), friends' lens (existing who-is candidates + provenance),
device evidence ("mårten says this is their device — confirm from Me to
pair back"), disavowal warnings, avatar, full-key fingerprint at the trust
moments. If the key belongs to a contact, land on (or link to) the
existing Person view — decide in scoping whether PersonView generalizes or
gets a key-keyed sibling.

## Constraints fixed by canon

- **Who-is stays a manual button** (who-is-this.md §5): opening the page
  must not auto-query anyone — rendering is local; asking broadcasts
  interest.
- **Recognize-as-device is never one tap**: it routes through the existing
  pair-preview fingerprint confirm (multi-device.md §3), not a page action.
- **Cluster-first** (ui-design-system §1): the page renders a key-set lens,
  never re-bakes one-key-one-person.

## Shape

Mostly app-layer + one command (`key_detail(subject)` over existing
stores); no protocol or `zink-client` policy work expected. Likely 2–3
slices: the page + command; the tappable-surface sweep (senders, member
rows, wild-key rows — the wild-key panel may shrink to a link); follow-ups
in context. Candidate slice: the R3 stuck-cue tap target resolves from
membership instead of the frozen label (currently degrades to plain text
on a locally named 1:1 — noted at the project 5/6 rebase).

## Open questions for the scoping session

- One `KeyView` vs generalizing `PersonView` — and whether they merge at
  close.
- The tappable-surface inventory (People rows too? avatars?).
- What the members panel shows once rows are tappable (dedupe with the
  wild-key panel).
