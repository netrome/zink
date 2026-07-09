# zink — Design Philosophy

These tenets are the *why* behind the protocol. When a design question comes up,
resolve it against these, not against how existing chat apps do it. The spec
([SPEC.md](./SPEC.md)) is downstream of this document.

---

### 1. Keys are the only real identifiers. People are a local interpretation.

The protocol addresses **keys** and **content hashes** — both cryptographically
crisp. "Alice," "the family group," "membership" are *human interpretations* that
each client reconstructs locally from keys it has chosen to trust. The protocol
never needs a global notion of a person or a group, and never has one.

### 2. The source of truth is what your peers believe, not what you claim.

There is no global registry and no authority. Identity is **local belief**, curated
per contact like a phone's contact list. You assign your own names to keys; you
decide which keys you believe belong to whom. Your friends do the same, and they may
disagree. That is not a bug to be fixed — it is the model.

### 3. Enforcement is impossible, so replace it with discretion.

Anyone can write a different client, so nothing the protocol "forbids" can actually
be prevented — you cannot stop Bob from forwarding a message, or from including
Charlie in a conversation Alice wanted private. We therefore **never design around
enforcement.** Every point of apparent control is really a *local choice*:

- who you choose to **send** to,
- whose attestations you choose to **trust**,
- what history you choose to **serve**.

This dissolves whole categories of "hard" distributed problems (membership
consensus, backlog privacy, revocation) into simple local policy.

### 4. The protocol provides building blocks; clients make policy and UX.

The wire protocol is a small set of primitives (§ SPEC building blocks). How
conversations are grouped, named, ordered for display, and how membership is
presented are **client decisions**. Multiple independent clients should interoperate
while making different choices.

### 5. Relays are untrusted infrastructure.

Everything is end-to-end encrypted, so relays and mailboxes handle **ciphertext
only**. A relay learns metadata (sizes, timing, which mailboxes) but never content.
This is what lets us lean on relays for connectivity and offline delivery without
compromising the p2p, trust-nobody spirit.

### 6. Best-effort over guarantees; embrace partial, eventual consistency.

Backfill of history is **never guaranteed complete** — peers serve what they have
and what they choose to. The message DAG makes incompleteness *detectable* (a
referenced parent you don't have = a known gap), which is the honest alternative to
pretending you have the whole story.

### 7. Honesty over false order.

There is no true global message order in a decentralized system, and we don't fake
one. Messages point to their causal parents, forming a DAG. Clients render a
sensible linear default (deterministic tiebreak), but concurrency — "these messages
crossed in flight" — is real data that advanced clients may choose to *show* rather
than hide.

### 8. Recovery is social, not cryptographic.

Losing a key forks your identity: you and an attacker can both claim to be you. We
do **not** try to solve this cryptographically. You call a friend, they re-attest
which key is really you and repudiate the other, and their clients stop routing your
messages to the bad key. A stronger, opt-in cryptographic anchor may be offered for
users who want it, but it is never mandatory — no one is forced to hold a key
forever.

### 9. P2P when possible, relayed when necessary.

True direct connections where the transport allows (native clients). Browsers can't
hole-punch, so PWA traffic is relay-routed — and that's acceptable precisely because
of tenet 5.

---

**The through-line:** identify by keys, believe locally, enforce nothing, guarantee
nothing, encrypt everything, and be honest about it.
