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
`same-person-as` handling (D2); automatic querying (see §5 — a privacy
decision, not a missing feature).

## 2. Decisions (resolved 2026-07-19)

| Decision | Resolution |
|---|---|
| Wire shape | `SyncOp::WhoIs { key }` → the responder's stored `ContactRecord` for that key, or `NotHeld`. |
| Response payload | The **full record** (attestations *and* relays) — `who-is-this` is one of the three record-freshness channels SPEC §3.6 names, so rendezvous info must ride along. |
| Serving policy | **Contacts-only, uniformly** — same gate as `get`/`get-successors`; strangers get `NotHeld` for every subject, including the responder's own keys. (The D0c note anticipated a *looser* per-op policy here; resolved the other way for maximum privacy. The gate stays per-op structurally — the policies just coincide.) |
| Hop limit | 1, structurally: learned records are never re-served (§4), and there is no forwarding. No `hops` field on the wire — forwarding is a responder-side choice added at D4 via a version bump. |
| Auto-query | None. Manual trigger only (§5). |
| Learned records | Stored **outside** the contact store, with provenance (§5) — they must never widen the D0c serving gate. |
| Record refresh | A record served *by its own subject* auto-refreshes a stored contact iff the key set is unchanged (§7). |
| Avatars | Content key travels **inside `Claim::Avatar`**, next to the blob hash; ciphertext on relay caches, key only ever on E2E channels (§8). |
| Profile revision | `my_record`'s hardcoded `revision: 0` is a bug — persist a counter, bump on profile change (§7). |

## 3. Wire

One new op pair on the existing `zink-sync/1` ALPN (added in-place at version 1,
pre-external-deployment norm — same category as D0b's `RelayEntry`):

```
SyncOp::WhoIs      { key: PublicKey }
SyncResult::Known  { record: Box<ContactRecord> }
// negative answer: the existing SyncResult::NotHeld — declining, not-knowing,
// and not-serving-you are indistinguishable on the wire (SPEC §5.2)
```

The record is served **as stored** — the responder relays the subject's signed
self-attestations verbatim; it adds nothing of its own (issuing third-party
claims is D4). The requester verifies everything (§5): the responder is trusted
no more than a relay.

## 4. Serving policy

Resolved per connection like D0c (the caller's key IS the authenticated
connection key), answered per op:

- Caller **not a contact** → `NotHeld`, for every subject.
- Caller is a contact, subject **∈ my keys** → my fresh `my_record()`.
- Caller is a contact, subject in my **contact store** (user-added) → that
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

The flow, best-effort like every peer op:

1. Candidate responders = every dialable stored contact, **plus the subject
   itself** if a stored record makes it dialable (the refresh case, §7).
2. Dial each by key (D0b), send `WhoIs { subject }`, collect answers.
3. Validate each answer like a scanned QR: record decodes, `subject ∈
   record.keys`, self-attestations verify (`self_claimed_name` already
   enforces attester = subject ∈ keys). Unverifiable answers are dropped with
   a warning, never fatal.
4. Store survivors in the **learned store**, keyed by subject, with
   provenance: which contact served it, and when. Distinct from the contact
   store — learned records don't get petnames, aren't served onward (§4),
   and don't open the D0c gate. Re-answers from the same responder replace
   (latest wins per responder).

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
3. **Key** — abbreviated hex, as today.

Weighting *which contact* served an answer (close friend vs. acquaintance) is
D4 differentiation; MVP treats all contacts equally and shows the list.

## 7. Record refresh & the revision fix

**Refresh.** When a `WhoIs` answer comes from the *subject's own connection*
(responder key ∈ stored record's keys) and the served record's key set equals
the stored one, the stored contact record is replaced in place — fresh relays
and attestations, petname untouched. A changed key set is **not** auto-merged:
a single compromised device could smuggle attacker keys into the set; key-set
changes wait for D2's mutual `same-person-as` links (and the related review
note — contact identity keyed on `keys.first()` — lands there too). Until D2,
a key-set change means re-scan the QR.

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

```
Claim::Avatar { hash: BlobHash, key: [u8; 32] }   // in-place at v1; dev-stage
                                                  // records re-exchanged (D0b norm)
```

- **Crypto:** encrypt-once with a fresh random content key, reusing the A3/B3
  `EncryptedBlob` construction unchanged — including `key-commit`, verified
  before trusting, exactly like message blobs. `hash` addresses the
  ciphertext, as everywhere.
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

- **D1a · Protocol op + serve side.** Wire types + `SyncHandler` policy (§3,
  §4). *Done when:* headless e2e — a contact's `WhoIs` returns the stored
  record; a stranger's returns `NotHeld`; a learned-only subject returns
  `NotHeld` even to a contact.
- **D1b · Pull, learned store, resolution, refresh.** §5–§7, CLI `who-is` for
  e2e. *Done when:* the one-way-add flow headless — A learns C's record via
  contact B, adds C, replies to C; plus a subject-served refresh updates
  relays and a key-set change doesn't.
- **D1c · UI.** Unknown participant → "who is this?" action; candidates with
  provenance; add-as-contact prefilled; refresh surfaced on contact view.
  *Done when:* the acceptance flow runs live on two devices.
- **D1d · Avatars.** §8. *Done when:* two devices see each other's avatars in
  contacts + conversation views, and the blobs on the relay are ciphertext.

## 10. Doc touchpoints when this lands

- SPEC §11: strike "`who-is-this` query format and default hop limit" from
  still-to-pin-down; record the resolutions (D1a).
- SPEC §3.2: `Claim::Avatar` shape (hash + key) (D1d).
- `client-core.md`: `who_is`, the learned store, resolution precedence (D1b).
- `mvp-build-plan.md`: tick sub-slices as they land.
