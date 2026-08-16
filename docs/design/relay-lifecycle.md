# Relay lifecycle: delivery debt follows the record

How relay knowledge stays current, and what happens to deliveries already
owed when it changes. Downstream of [SPEC](../SPEC.md) §3.6 (record
freshness) and [mailbox-rendezvous-push.md](./mailbox-rendezvous-push.md)
§4 (rendezvous); sibling of [who-is-this.md](./who-is-this.md) §7
(read-time relay resolution) and [live-delivery.md](./live-delivery.md) §2
(the outbox ledger this doc extends). Built by project
[5-relay-lifecycle](../projects/5-relay-lifecycle/tracker.md); grows as its
slices land (§5 is R6's).

## 1. The problem: staged snapshots vs. moving records

A send stages one outbox entry per distinct recipient-relay — a snapshot of
`effective_relays` at that moment. Records move (a server reinstall, a new
relay), and before R2 nothing revisited the snapshot: every flush re-dialed
the staged dial string for 30 days, then went quiet with the entry still
pending. The failure compounds — a stale record kills the mailbox path
*and* dial-by-key (both route through the dead relay) — so the message
often reached the recipient anyway (they dialed us and pulled it via sync)
while the sender's UI claimed "sending…" forever. Best-effort delivery is
tenet 6; a marker that contradicts reality is a tenet 7 violation.

## 2. The rule: the ledger owes recipients, not dial strings

An outbox entry means "these recipients aren't served yet". The relay in
its key is *how* we last knew to serve them, never *what* is owed. Every
`flush_outbox` therefore **reconciles before retrying**, per pending
message:

1. **Derive** — the sealed recipients re-resolve through
   `effective_relays` exactly as a fresh send would (same provenance
   classes, same read-time stance as who-is-this.md §7).
2. **Release** — entries for relays no longer in any unserved recipient's
   resolution are cleared, counted as `released` in the `FlushReport` —
   settled debt, never laundered into `delivered`.
3. **Re-target** — newly-owed relays get entries **inheriting the
   message's original age**: a moved record must not reset the 30-day
   give-up clock, or a dead message could be revived forever.

This is the outbox's own read-time resolution: the contact store is still
never mutated by network input (who-is-this.md §5) — the ledger is *our*
bookkeeping about *our* debt, reshaped from local knowledge only. On-disk
entries kept their `(message, relay)` shape, so pre-R2 ledgers migrate by
simply being flushed once.

## 3. Settlement by proof of possession

A durable `Stored` ack from a recipient's device (D5, persisted per De7)
settles that recipient's share of the debt: a relay whose every hosted
recipient has acked is released without a deposit — the flush-time twin of
`deliver`'s per-relay skip (direct-delivery.md §3), extended to acks
learned *after* the entries were staged.

**Blob messages are exempt from ack-settlement.** An ack proves the
envelope is stored; blob bytes are fetched from the recipient's *relay
cache* (C3a), so the deposit-and-push is still owed. Same guard, same
reason, as the send-time skip.

**A sync pull is deliberately not possession.** Serving `get`/
`get-successors` to a recipient proves they *requested* the message, not
that they durably stored it — counting it would launder best-effort
serving into a delivery claim. If this gap bites (it closes the last
"they have it but we still owe a relay" window), the honest fix is an
explicit ack op on the peer ALPN — a SPEC §11 decision, not a client-side
inference.

## 4. What reconciliation never does

- **Never drops debt on no evidence.** A recipient resolving to *no*
  relays (a raw-spec send, a lost record) keeps every staged entry alive —
  staged knowledge is the honest fallback, and best-effort means retrying
  it, not forgetting it.
- **Never re-litigates membership.** Recipients were sealed at send time;
  a later disavowal changes future addressing, not deliveries already
  owed.
- **Never adds retry backoff.** Reconciliation changes what entries
  *target*; the no-per-entry-backoff position (live-delivery.md §2
  known-remaining) is unchanged.

## 5. Manual overrides: the user's patch, ranked like a petname

R5 adds a per-contact **relay override**: specs stored *beside* the stored
record (`contacts/<stem>.relays`), never inside it — the scan stays
immutable evidence — and a top-ranked class in read-time resolution
(who-is-this.md §7). The rank was the contested call:

- **Manual wins while it is kept.** The petname precedent: an explicit
  local act is the user's lens, and no network input outranks it. The
  cost — an override can go stale and recreate the very disease this
  project treats — is accepted because the honesty valve already exists:
  R3's stuck cues say *"can't reach their relay"* on the person's own
  page, right next to the clear button.
- **An explicit record update clears it.** A confirmed rescan
  (`update_contact`) is the user adopting fresher truth; keeping the patch
  would silently shadow it. Symmetric with what created it: overrides are
  set by one explicit act and removed by another (clear, or rescan).
- **Overrides are an escape hatch, not a path.** The normal repair is a
  rescan (R1) or the subject-refresh (§6); the override exists for the
  case where neither is at hand — a relay died and the contact told you
  the new spec over any other channel.

The person view renders the *effective* relays with their provenance
class named and the outbox's per-relay debt beside them — the panel where
"why aren't my messages arriving" gets an answer and every answer has an
action.

## 6. Keeping records current (R6, unwritten)

Reconciliation is only as good as the records it reads. The healing loop —
opportunistic `who_is` against a contact over any already-authenticated
connection — lands at R6 and will be documented here.
