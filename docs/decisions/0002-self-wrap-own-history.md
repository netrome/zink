# 0002 · Self-wrap convention — sealing to your own key for readable history

- **Status:** Accepted
- **Date:** 2026-07-12
- **Tenets:** 3 (enforcement is impossible — replace it with discretion), 4 (protocol = primitives; clients own policy)
- **Where it landed:** project 1-mvp · C3 / C3a (recorded in SPEC §6)

## Context

A sender encrypts each message body once under a random content-key, then seals
that content-key to each *recipient's* key. The sender is not in
`core.recipients`, so a device that sent a message could not later reopen its own
stored envelope — its own chat history rendered as `<unopenable>`. Storing
plaintext-at-rest to work around this would violate ciphertext-at-rest.

## Decision

`seal` **always adds a key-wrap for the sender's own key** — *without* listing
self in `core.recipients` and *without* depositing to self. The sender can then
reopen its own stored envelopes, so history renders from the stored DAG with
everything ciphertext-at-rest. This is a **client convention, not protocol**.

## Consequences

- **Content-addressing is untouched.** Wraps live *outside* the hashed core, so
  message ids are unchanged, no `version` bump is needed, and the recipient set is
  unaffected. Determinism holds.
- A client that *skips* the convention is fully interoperable — it simply loses the
  ability to read its own sent history. Nothing breaks for anyone else.
- This is the read-your-own-messages case only. Full **send-to-self** (depositing
  to your own mailbox so *another of your devices* receives it) is the D3
  multi-device extension of the same idea, not this decision.

## Ties to the philosophy

This is tenet 4 in miniature: the protocol offers the primitive (seal a
content-key to a key) and the *client* decides to also seal to itself — a policy
choice that never enters the wire format or `zink-protocol`. And it honours tenet
3: rather than the protocol *mandating* self-inclusion (unenforceable — other
clients would ignore it), self-wrap is pure local discretion whose only stake is
your own history, so a client that declines it costs no one but itself.
