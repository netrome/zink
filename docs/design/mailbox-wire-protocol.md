# Design: Mailbox Wire Protocol

The concrete wire messages for the mailbox ops of
[mailbox-rendezvous-push.md](./mailbox-rendezvous-push.md) §5, pinned just-in-time for
slice A4. Downstream of [SPEC.md](../SPEC.md).

Status: **resolved for MVP.**

## Transport

- **ALPN `zink-mailbox/1`** on the relay's iroh endpoint. The protocol generation is
  in the ALPN, so incompatible speakers never exchange frames; individual objects
  still carry their own `version` tag (SPEC §10).
- **One request per bidirectional stream.** Client opens a bi-stream, writes one
  encoded `MailboxRequest`, finishes; relay replies with one `MailboxResponse` and
  finishes. QUIC stream FIN provides the message boundary — **no length-prefix
  framing**. Streams are cheap; a client opens as many as it has requests.
- **Size caps:** requests ≤ 1 MiB, responses ≤ 16 MiB (`read_to_end` limits).
  Operator policy may lower them; the constants live in `zink-protocol::mailbox`.
- **Auth is the connection** (design doc §5): the relay reads the peer's key from the
  iroh connection. `register` / `fetch` / `ack` operate on *the caller's own* mailbox —
  there is no way to name another key's mailbox. `deposit` is open to any connected
  peer (gating deferred; rendezvous doc §5).

## Messages (BORSH, versioned)

```
MailboxRequest  { version: u16, op }
  op: Register                      // create/refresh the caller's mailbox
    | Deposit { envelope }          // store a MessageEnvelope for its recipients
    | Fetch   { after: cursor }     // envelopes deposited after this cursor
    | Ack     { up_to: cursor }     // drop delivered envelopes ≤ cursor

MailboxResponse { version: u16, result }
  result: Registered
        | Deposited { id }                       // message id — idempotency receipt
        | Envelopes { items: [ {cursor, envelope} ] }
        | Acked
        | Error { code: malformed | internal }
```

- **Cursor** = per-mailbox monotonic deposit index (relay-local, meaningless across
  relays). Each fetched item carries its cursor so the client acks precisely.
- **Dedup / idempotency:** a deposit whose message id is already present in a mailbox
  is a no-op; the response is `Deposited { id }` either way, so sender retries are safe.
- **Recipient indexing:** the relay decodes the envelope and indexes it under each
  `core.recipients` key **that has a registered mailbox** here. Unregistered
  recipients are skipped (no storage for keys that never registered — spam surface);
  registration-on-first-connect makes this invisible in practice.
- **The relay does not verify signatures** — it is untrusted either way, verification
  is the recipient's job, and staying signature-blind keeps it dumb (it must decode
  the envelope only to read `recipients`).

## Decisions

- One-request-per-stream over length-prefixed multiplexing — simpler, QUIC-native.
- Cursor-based ack (not per-id) — matches the drain pattern; per-id ack can be added
  as a new op if a client ever needs selective ack.
- Deposit to zero registered recipients is a silent no-op (best-effort, tenet 6).
