# Design: Conversation DAG Store

The client-side store for one conversation's message DAG, pinned just-in-time for
slice B1. Downstream of [SPEC.md](../SPEC.md) §4 (conversations, ordering).

Status: **resolved for MVP.**

## Scope & placement

- One `ConversationDag` per conversation, living in `zink-protocol` (pure: no I/O, no
  async). Persistence (B5) and the multi-conversation container (C2) wrap it later.
- It stores **`MessageCore`s that the caller has already authenticated** (envelope
  signature verified, commitment checked). The store orders; it does not vet crypto.
- Cycles need no handling: a parent reference is a BLAKE3 hash of the parent's bytes,
  so a cycle would require a hash preimage. Out-of-order *arrival* is the normal case
  and fully supported (a child may arrive before its parents).

## Validation on insert — structural only

| Rule | Genesis | Non-genesis |
|---|---|---|
| `conversation` | `None` | `Some(genesis id)` — must match the store's |
| `parents` | `[]` | non-empty |
| `seq` | `0` | any (`0` is valid for another sender's first message) |
| `logical` | `0` | `≥ 1` (Lamport is `1 + max(parents)`; `0` would sort before the genesis) |

Nothing deeper is enforced. A sender lying about `logical` or `seq` only distorts
where *their* message appears and what gaps are reported about *them* — ordering is
display policy, not integrity (tenet 7). Structural rejects exist solely to keep the
store's own invariants sound.

Duplicate inserts are idempotent (`AlreadyKnown`), mirroring dedup-by-id everywhere
else in the system.

## What the store tracks

- **messages**: id → core.
- **children** index (includes children of *not-yet-arrived* parents, so heads are
  correct the moment a late parent lands).
- **heads**: known messages with no known children — the `parents` of the next send.
- **missing parents**: referenced ids we don't hold. This *is* tenet 6's honesty: a
  known gap, surfaced, never papered over.

## Ordering & gaps

- **Linearize** = sort known messages by `(logical, id)`. Deterministic, total, and
  causality-consistent for honest messages (a parent's `logical` is strictly smaller).
  Because the sort key is intrinsic to each message, any two clients — and any two
  *partial views* — agree on the relative order of the messages they share.
- **`seq` gaps** = per sender within the conversation, the missing values below that
  sender's highest seen `seq`. Detects holes; cannot detect missing *newest* messages
  (that needs the sync-time head/`seq` advert, SPEC §11 — later slice).

## Drafting the next message

The store supplies what `MessageDraft` needs: `heads()` (the new message's parents),
`next_logical()` (`1 + max(heads.logical)`), and `next_seq(sender)`. Wiring this into
the CLI waits for client persistence (B5) — a stateless per-invocation CLI can't hold
a conversation.

## Non-goals (B1)

Persistence, multi-conversation container, peer sync / backfill (`get` /
`get-successors`), concurrency-aware *views* (advanced-client UX, Stage D).
