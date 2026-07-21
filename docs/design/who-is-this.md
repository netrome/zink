# who-is-this: identity discovery & name resolution (D1)

Design for the D1 sub-slices. Downstream of [SPEC](../SPEC.md) §3.2 (attestations),
§3.5 (the `who-is-this` query), §3.6 (record freshness); sibling of
[sync-primitives.md](./sync-primitives.md) — the same peer ALPN and the same
D0b dial-by-key substrate. SPEC §11 lists "the `who-is-this` query format and
default hop limit" as still-to-pin-down; this doc pins them.

## 1. Goal & non-goals

**Goal.** Resolve an unknown key to a believable name, and keep known contacts'
records fresh — with the social graph as the only trust boundary. The concrete
hole it closes: a QR add is one-directional. If C scans A's QR and messages A,
A sees a hex key and cannot reply (no record → no relays). After D1, A asks
their contacts about C's key, learns C's record from whoever holds it, and
replies. Secondary win: record staleness (the D0b field trap — relay changes
invalidate stored records) heals by re-asking.

**Non-goals (all deferred, with homes):** query forwarding / hops > 1 (D4);
issuing third-party `name` claims — "vouching" — and negative claims (D4;
recovery flows use them); aggregation UX beyond an agreement count (D4);
`same-person-as` handling (D3); automatic querying (see §5 — a privacy
decision, not a missing feature).

## 2. Decisions (resolved 2026-07-19)

| Decision | Resolution |
|---|---|
| Wire shape | `SyncOp::WhoIs { key }` → the responder's stored `ContactRecord` for that key, or `NotHeld`. |
| Response payload | The **full record** (attestations *and* relays) — `who-is-this` is one of the three record-freshness channels SPEC §3.6 names, so rendezvous info must ride along. |
| Serving policy | **Contacts-only, uniformly** — same gate as `get`/`get-successors`; strangers get `NotHeld` for every subject, including the responder's own keys. (The D0c note anticipated a *looser* per-op policy here; resolved the other way for maximum privacy. The gate stays per-op structurally — the policies just coincide.) |
| Hop limit | 1, structurally: learned records are never re-served (§4), and there is no forwarding. No `hops` field on the wire — forwarding is a responder-side choice added at D4 via a version bump. |
| Auto-query | None against the contact list — manual trigger only (§5). *Revised at D2b (2026-07-19):* scoped auto-query of a conversation's own participants for its unknown members, gated + rate-limited (§5 carve-out; groups.md §4). |
| Learned records | Stored **outside** the contact store, with provenance (§5) — they must never widen the D0c serving gate. Multiple records per subject is the data model, one per `(subject, responder)`. |
| Record refresh | **Nothing is ever overwritten** (revised 2026-07-19 — an earlier draft auto-refreshed the stored record in place; rejected as needless mutation of a trust anchor). The contact store is never touched by network input; subject-served answers append to the learned store like any other, with provenance "the subject"; freshness is a **read-time** resolution (§7). |
| Avatars | Content key travels **inside `Claim::Avatar`**, next to the blob hash; ciphertext on relay caches, key only ever on E2E channels (§8). |
| Profile revision | `my_record`'s hardcoded `revision: 0` is a bug — persist a counter, bump on profile change (§7). |

## 3. Wire

One new op pair on the existing `zink-sync/1` ALPN (added in-place at version 1,
pre-external-deployment norm — same category as D0b's `RelayEntry`):

```
SyncOp::WhoIs      { key: PublicKey }
SyncResult::Known  { record: Box<ContactRecord>,
                     endorsements: Vec<SignedAttestation> }   // D4a
// negative answer: the existing SyncResult::NotHeld — declining, not-knowing,
// and not-serving-you are indistinguishable on the wire (SPEC §5.2)
```

The record is served **as stored** — the responder relays the subject's signed
self-attestations verbatim. Its own voice rides *beside* the record (D4a,
web-of-trust.md §3): `endorsements` are the responder's issued claims about
the subject — its vouch, later its disavowal — accepted only when the
attester IS the answering connection key, so nothing second-hand ever
travels. The requester verifies everything (§5): the responder is trusted no
more than a relay.

## 4. Serving policy

Resolved per connection like D0c (the caller's key IS the authenticated
connection key), answered per op:

- Caller **not a contact** (nor a recognized own device — the D3b gate
  extension, multi-device.md §6) → `NotHeld`, for every subject.
- Caller is served, subject **∈ my keys** → my fresh `my_record()`.
- Caller is served, subject ∈ my **recognized own devices** → that device's
  stored record (the D3b **mirror rule**, multi-device.md §6): recognizing
  a device is a willingness to advertise it — and nobody else *can* serve a
  new device's record (its own contact store is empty; siblings hold it
  only in the own-devices store).
- Caller is served, subject in my **contact store** (user-added) → that
  stored record.
- Subject known only via my own *learned* store → `NotHeld`. Serving
  second-hand records would create implicit gossip chains — hop limit 1 is
  structural, not advisory.

Serving a record still reveals to the *asking contact* that you know the
subject. That is the SPEC's intended trust model ("your social graph is your
trust boundary") — you already chose to trust this contact; the stranger case
is what the gate closes.

## 5. Pull flow & learned records

`Client::who_is(subject)` — **manual trigger only** (a UI action / CLI
command). Auto-querying on unknown-sender receipt would broadcast "I just got
a message from X" to every contact asked; that's a live privacy leak for a
marginal UX gain, so it's a deliberate non-goal, not a follow-up.

> **Scoped carve-out (resolved 2026-07-19; shipped with D2b — groups.md
> §4).** The blanket rule above is about *unsolicited senders*, and it
> stands for them. Inside a conversation, a key's presence in the signed
> `recipients` is already mutual knowledge among participants — so
> `who_is_among`, the responder-scoped variant, is auto-triggered after a
> drain for unknown members of conversations passing the
> contributing-contact rule (groups.md §6), asking **only that
> conversation's participants**, rate-limited per (subject, conversation)
> per run. The whole contact list is never auto-queried. Manual control
> over the contact store stays absolute (add-or-ignore), and the
> responder-side gate is unchanged.

The flow, best-effort like every peer op:

1. Candidate responders = every dialable stored contact, **plus the subject
   itself** if a stored record makes it dialable (the freshness case, §7),
   **plus recognized own devices** (D3c, multi-device.md §5): siblings
   serve this caller like self, and on a fresh device they are the only
   responders there are.
2. Dial each by key (D0b) — **all at once, deadline capped at
   `min(connect_timeout, 5 s)`** (De3: an offline contact costs one bounded
   dial, never a serial sum) — send `WhoIs { subject }`, collect answers
   plus honest asked/unreachable counts.
3. Validate each answer like a scanned QR: record decodes, `subject ∈
   record.keys`, self-attestations verify (`self_claimed_name` already
   enforces attester = subject ∈ keys). Unverifiable answers are dropped with
   a warning, never fatal.
4. Store survivors in the **learned store**, keyed `(subject, responder)`,
   with the receipt time — including answers from the subject itself
   (provenance "the subject", proven by the authenticated connection key).
   Distinct from the contact store — learned records don't get petnames,
   aren't served onward (§4), and don't open the D0c gate. A re-answer from
   the same responder replaces that responder's entry (latest per responder);
   nothing else is ever overwritten, and **the contact store is never
   modified by network input** — the scanned (or explicitly promoted) record
   is immutable evidence until another explicit act replaces it.

Promotion to a real contact is exactly the existing `add_contact` — one
explicit act, petname prefilled from the winning self-claim.

## 6. Name resolution (ranking)

MVP precedence, deterministic and honest about provenance:

1. **Petname** — the subject is a contact; manual assignment always wins
   (SPEC §3.1). (Every contact has one — `add_contact` requires it — so
   ranking below only ever applies to non-contacts.)
2. **Learned self-claim** — the verified self-claimed name from learned
   records, displayed with provenance and agreement: *"calls themself Carol —
   records held by B, D"*. Conflicting names across answers (a rename caught
   mid-propagation) resolve by attestation `revision` (SPEC §3.2 supersession:
   highest wins); a genuine tie surfaces both, honestly.
3. **Endorsed name** (D4a, web-of-trust.md §2) — a name contacts *vouch*
   with their own signature: *"vouched by B, D"*. Ranks below any verified
   self-claim; endorsement revisions are the voucher's own counter and
   never mix into the self-claim ordering.
4. **Key** — abbreviated hex, as today.

Weighting *which contact* served an answer (close friend vs. acquaintance) is
D4 differentiation; MVP treats all contacts equally and shows the list.

**What friends' answers do and don't contain.** A `WhoIs` answer is the
subject's record as the responder stores it — the subject's *self*-claims,
relayed verbatim. Different friends may therefore show you different
*revisions* of the same person's self-record (a stale name, an old avatar) —
useful drift-spotting — but never their own labels: a responder's petname for
the subject is exactly what SPEC §3.2 keeps private unless they deliberately
broadcast it as a third-party `name` attestation. That — "your friends call
them …" — is D4 vouching, purely additive on this same primitive.

## 7. Record freshness = read-time resolution

Nothing refreshes in place — freshness is resolved when a record is *used*
(revised 2026-07-19; the write-time auto-refresh in an earlier draft was
rejected: mutating a trust anchor on network input is dangerous and
unnecessary when the inputs can just accumulate and be ranked, the same
stance the DAG takes).

At dial / fan-out time, the relays for a subject resolve by provenance
class, latest-received within a class:

1. **Subject-served** learned record — authenticated by the connection key
   (only the key-holder can produce one).
2. **The contact-store record** — authenticated by the out-of-band scan (or
   the explicit promotion).
3. **Contact-served** learned records — third-hand; used only when nothing
   better exists, which is precisely the one-way-add bootstrap where it's
   the whole point.

**Sealing keys never come from learned records** — only from the user-added
contact-store record, until D3's mutual `same-person-as` links can evaluate
key-set changes (the review note about contact identity keyed on
`keys.first()` lands there too). A learned record with a different key set
just sits as evidence; until D3, adopting new keys means an explicit re-add.

**Revision.** `my_record` currently hardcodes `revision: 0`, so a renamed
profile issues a *second* rev-0 claim and supersession has no winner — ranking
(§6) and display of conflicting answers both need one. Fix in D1b: persist a
per-profile revision counter in client state, bump on every `set_profile`
change, stamp it into the self-attestations. (Per `(claim-kind)` scope is
already respected: name and avatar bump independently.)

**Revision.** `my_record` currently hardcodes `revision: 0`, so a renamed
profile issues a *second* rev-0 claim and supersession has no winner — ranking
(§6) and refresh both need one. Fix in D1b: persist a per-profile revision
counter in client state, bump on every `set_profile` change, stamp it into the
self-attestations. (Per `(claim-kind)` scope is already respected: name and
avatar bump independently.)

## 8. Avatars (D1d)

The gap: `Claim::Avatar(BlobHash)` names ciphertext but carries no key, and an
avatar has no message envelope to wrap keys in. Plaintext blobs on relay
caches would break the ciphertext-only invariant. Resolution — put the key in
the claim:

**Why not just an image message?** (Considered 2026-07-19.) The image
*plumbing* is fully reused — same `EncryptedBlob` construction, key-commit,
relay caches, canvas downscale; D1d adds no new crypto. What can't be reused
is the envelope: a message's content key is sealed to a recipient set **fixed
at seal time**, while an avatar's audience is **open-ended and
forward-growing** — every future contact, plus friends-of-friends via `WhoIs`
(the subject can't have key-wrapped for people it doesn't know exist, and a
relaying contact can't re-wrap without fabricating content the requester
couldn't verify against the subject's signature). Nostr-style "profile = a
note with extra fields" works there because notes are public plaintext; in an
E2E system the one-primitive-with-variants layer is the *attestation*, and
the key-in-claim is what makes the audience open-ended while relays still
hold only ciphertext.

```
Claim::Avatar { hash: BlobHash, key: [u8; 32] }   // in-place at v1; dev-stage
                                                  // records re-exchanged (D0b norm)
```

- **Crypto:** encrypt-once with a fresh random content key, reusing the A3
  AEAD + content-addressing (`hash` addresses the ciphertext, as
  everywhere). **No `key-commit`** (corrected 2026-07-19 at implementation —
  the draft said "commit kept"): a commitment guards a key that arrives on a
  *different* channel than its binding (envelope key-wraps vs the hashed
  core); here hash and key ride in one signed attestation, so the signature
  already binds them and a commitment would be derived from and checked
  against the same claim — verifying nothing. Integrity = signature over
  (hash, key) + content address + the AEAD tag.
- **Why the key may live in the claim:** attestations travel only inside
  records and `WhoIs` answers — QR (out-of-band) and the E2E-encrypted peer
  ALPN. No relay ever sees a claim, so the invariant holds: relays store
  ciphertext they cannot open; whoever you voluntarily hand the record to can
  render you. Rotation = new key + hash at a higher `revision` — supersession
  handles it with no extra mechanism.
- **Distribution:** publisher pushes the encrypted avatar to its **own** home
  relays' blob caches (the C3a own-blob pattern) on publish, and re-pushes on
  open — the cache TTL (30 days) would otherwise silently expire it. Fetchers
  read the relays from the same record the claim arrived in, decrypt, verify,
  and keep it in the client blob cache (fetch once, render forever).
- **Image handling:** pick + downscale reuses the C3c canvas path (thumb-sized;
  bounded bytes); decrypted bytes are validated as an image before rendering —
  hostile-input hygiene, same stance as everywhere.

## 9. Slices

- **D1a · Protocol op + serve side — done (2026-07-19).** Wire types +
  `SyncHandler` policy (§3, §4). Variants appended (BORSH tags stable);
  self-record construction shared with `my_record` via `build_own_record`;
  a fresh profile is served immediately (signed per request, no restart).
  e2e: stranger `NotHeld`, contact gets the stored record verbatim, unknown
  subject `NotHeld` even to a contact, own-key `NotHeld` until the profile
  completes.
- **D1b · Pull, learned store, resolution — done (2026-07-19).** §5–§7, CLI
  `who-is` for e2e. Read-time resolution shipped as one seam
  (`effective_relays`) feeding every by-key path; `resolve_name` ranks by
  attestation revision (the protocol's `self_claimed_name` now applies
  supersession, new `self_name_claim` exposes the revision); the profile
  revision persists and bumps per rename. e2e: the one-way-add flow through
  the CLI (learn from a mutual contact → promote → reply delivered), the
  contact store byte-compared across `who_is` calls, subject-served beating
  newer hearsay, smuggled learned keys inert.
- **D1c · UI — done (2026-07-19, verified live).** Unknown-sender banner +
  who-is panel in the chat view, per-contact freshness pull in the contacts
  view, one render-ready `who_is` command. Exercised without groups —
  one-way adds are the no-groups unknown-sender case (§1): phone added the
  laptop one-way and messaged it; the banner appeared, a third identity
  served the record, add-as-contact prefilled the petname, the reply
  delivered.
- **D1d · Avatars.** §8. *Done when:* two devices see each other's avatars in
  contacts + conversation views, and the blobs on the relay are ciphertext.
  *(2026-07-19: code complete — `Claim::Avatar { hash, key }`,
  `seal_avatar`/`open_avatar` in the protocol (no key-commit, see §8);
  client `set_avatar`/`push_avatar`/`avatar` with read-time claim
  resolution across stored + learned records; CLI `set-avatar`/`avatar`;
  app picker + chat/contact-row rendering with magic-byte validation.
  Headless e2e proves cross-client render with ciphertext at rest.
  Awaiting the two-device run.)*

## 10. Doc touchpoints when this lands

- SPEC §11: strike "`who-is-this` query format and default hop limit" from
  still-to-pin-down; record the resolutions (D1a).
- SPEC §3.2: `Claim::Avatar` shape (hash + key) (D1d).
- `client-core.md`: `who_is`, the learned store, resolution precedence (D1b).
- `mvp-build-plan.md`: tick sub-slices as they land.
