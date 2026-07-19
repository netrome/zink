# Multi-device: pairing, clustering, re-wrap (D3)

Design for the D3 sub-slices. Downstream of [SPEC](../SPEC.md) §3.2
(attestations / `same-person-as`), §3.6 (pairing uses the contact artifact),
§5.2 (re-wrap; "no cryptographic difference between your new device and a new
member"), §6 (send-to-self); rides [groups.md](./groups.md) end to end — a
new device IS a new member, propagated by the signed `recipients` list and
resolved by the D2 pipeline. D3 adds the three things groups deliberately
deferred: the **mutual link** (who counts as the same person), the **re-wrap
op** (reading history from before the key existed), and **send-to-self**
(your own devices as implicit recipients).

**The governing principle (sharpened at review, 2026-07-19): clustering is
the observer's choice.** Identity is local belief (tenet 1): Alice may treat
Bob's phone and laptop as two contacts, Carol may merge them — both are
correct. Links are *advisory evidence*, never instructions; labels↔keys is
many-to-many (one may even label several *people* "someone from the soccer
team" — future latitude, §7). And **completeness is the owner's
responsibility**: MY clients include MY devices in MY messages
(send-to-self) and sync each other (own-device sync + re-wrap). Contacts'
clients owe my devices nothing — their fan-out to my record's device set is
robustness, never load-bearing. This dissolves what would otherwise be the
hardest machinery: there is no key-adoption rule, because nobody else ever
needs one — my devices join conversations through my own signed
`recipients`, and the D2 pipeline does the rest.

**Defensive framing, continued.** One new wire op (`GetKeys`), one new claim
*use* (no new claim kinds), one new client store (own devices). If a step
seems to need a device registry, a family key, a transfer protocol, a
pairing server — or *any behavior another client must adopt for my devices
to work* — stop; it doesn't.

## 1. Goal & non-goals

**Goal.** Pair a second device by QR; from then on *my clients* make it
behave as me: everything I send from either device lands on both
(send-to-self), it joins my conversations through my own signed
`recipients`, it can *read* history from before it existed (re-wrap), and
contacts' clients get honest *evidence* to cluster both keys as one person
— if and how they render that is theirs.

**Non-goals (deferred, with homes):** repudiation / lost-device recovery
(`Negative` claims + the social flow — D4/post-MVP; the repudiation-lag note
from groups planning parks here too); friend-re-wraps-for-you (SPEC allows
it; the *willingness* gate for non-own-devices is a later policy — D3 serves
re-wraps to own devices only); >2 devices (nothing below assumes 2, but the
UX and tests target the pair case); any auto-pairing or device discovery
(pairing is two deliberate scans, like contacts — SPEC §3.6: same artifact).

## 2. Decisions

| Decision | Resolution |
|---|---|
| Link shape | `Attestation { attester: K, subject: K, claim: SamePersonAs(L) }` — "I, K, am the same person as L". **Mutual** = both directions exist and verify (K→L and L→K); unilateral links are rendered but never trusted for clustering (SPEC §3.2 weights mutual above unilateral — an attacker can always *claim* your key). |
| Where links live | In the **record's** `attestations`, like name/avatar claims (SPEC §3.6 says exactly this). The record becomes the *person record*: `keys` lists the device set, attestations carry both link directions once held. |
| Pairing flow | Two scans, reusing the C2 exchange in an explicit **pair mode** (§3) — no pairing server, no new wire op. The link exchange completes over the existing who-is freshness machinery. |
| Contact identity | Fixed from `keys.first()` to **key-overlap** (§4) — the parked review note lands here. A contact entry is the observer's local grouping of keys under a petname; the record inside it is evidence, not authority. |
| Key adoption by contacts | **None — automatic adoption is rejected** (the review's simplification). Sealing rules are unchanged from D1b/D2 (user-added records + signed cores); my devices reach contacts' sealing sets through *my* recipients lists, not through their evaluation of my claims. Updating a contact entry stays the one explicit act, now popup-assisted with link evidence (§7). D1b's "key-set changes wait for D3" resolves as: *they stay explicit forever; D3 adds the evidence.* |
| Send-to-self | **The core mechanism.** Own other devices are appended to every send's `recipients` and deposited like any recipient (§5) — devices are honest conversation members, joined by their owner's signature. The SPEC §6 note gets recorded when this lands. |
| New-device bootstrap | Lazy by default: send-to-self makes the *next organic message* per conversation carry the new key — no enumeration op, no new mechanism. An optional "introduce now" button (the D2c add-member gesture per conversation) is pure UX sugar for the impatient (§5). |
| Re-wrap | `SyncOp::GetKeys { ids }` → per-id `KeyWrap`s re-sealed to the caller (§6); served to **own devices only** at D3. Wraps append outside the hashed core — ids never move. |
| Gate extension | The D0c `serves` self-allowance widens from `caller == me` to `caller ∈ my verified device cluster` (§6) — evaluated against the local own-device store, never against claims in the request. |

## 3. Pairing (two scans, one mode)

The C2 mutual-scan exchange with an "this is my device" flag:

1. **Old device O**: "pair a new device" → shows its QR (the normal record).
2. **New device N** (fresh install — dial-by-key works pre-profile since
   De5): "pair with existing device" → scans O → stores O's record in the
   **own-devices store** (not contacts, no petname) → signs `N: same-person-as
   O` → **adopts O's profile** (name; avatar claim rides in O's record and
   re-publishes with N's record) → shows its own QR, which now lists
   `keys = [N, O]` and carries N's link.
3. **O** scans N's QR → verifies N's link names O → stores N in its
   own-devices store → signs `O: same-person-as N` → O's record now lists
   both keys and O's link.
4. **Completion**: N still lacks O's link (signed after N's scan). Either
   device dialing the other with the existing record-freshness query
   (`who_is_among(partner, [partner])`) picks up the missing link — the
   pairing UI does this automatically after step 3. From here both devices
   publish the identical person record.

**Confirm before signing** (the one real risk): pair mode signs a link after
a scan, so scanning a *wrong* QR in pair mode must not silently link an
attacker. Each device shows the scanned key's short fingerprint + claimed
name and requires an explicit confirm before signing. Mutuality bounds the
damage — an attacker also needs *your* device to sign them — but the confirm
keeps the deliberate act deliberate.

## 4. The person record & contact identity

- **`my_record`** gains: the own-device keys in `keys`, my `SamePersonAs`
  links, and (once held) the partners' links. This is an *offer* of how to
  address me — a contact holding the fresh record fans out to the whole set
  (B2 machinery; `send` already seals to every key of a `Contact`), which
  is welcome robustness but never required: my devices' completeness is my
  clients' job (§5, §6).
- **Contact identity = key overlap** (the parked `keys.first()` fix):
  `add_contact` and the petname-collision check identify an existing contact
  by *any shared key*; a re-scan with reordered or added keys updates that
  contact instead of forking a duplicate. The store stem is re-derived from
  the updated record; the petname is untouched.
- **No adoption rule.** Updating a contact's entry with a fresh record
  remains the one explicit act it has always been (`add_contact` — re-scan,
  paste, or the popup's one-tap offer). What D3 adds is *evidence quality*:
  the offer can say "these keys are mutually linked, verified" vs "this
  record merely lists an extra key". The observer decides; nothing updates
  itself. (D1b's "key-set changes wait for D3" resolves as: they stay
  explicit forever — D3 supplies the evidence, not the automation.)

## 5. Send-to-self & the introduction

- **Send-to-self**: `finish_send` appends the own-device keys to every
  draft's `recipients` (they're honest members — Mårten's framing: "it's
  really a conversation between Alice's phone, Alice's laptop, my phone and
  my tablet") and the fan-out deposits to the own home relays like for any
  recipient. Each device keeps its own `seq` per conversation (already
  per-sender-key). The C3 self-wrap stays — it's what lets the *sending*
  device reopen its copy; deposits are for the *other* devices.
- **The new device joins lazily and automatically**: because send-to-self
  appends own devices to *every* send, the next organic message in each
  conversation carries the new key into the signed `recipients` — contacts'
  clients then run the ordinary D2 pipeline (scoped auto-query → evidence →
  their choice). No introduction mechanism exists; an optional "introduce
  now" button (one empty-body add-member message per conversation, the D2c
  gesture) is UX sugar for the impatient. Either way the new device
  receives from its first inclusion onward; D0 auto-sync heals the
  skeleton; §6 re-wraps make the backlog readable — all owner-side.

## 6. Re-wrap: `GetKeys` and the gate extension

The one new wire op (in-place at v1, appended variants — tags stable):

```
SyncOp::GetKeys    { ids: Vec<MessageId> }        // bounded batch
SyncResult::Wraps  { wraps: Vec<(MessageId, KeyWrap)> }
// misses are simply absent; NotHeld stays the op-level decline
```

- **Serve side**: for each id (batch capped), if we hold the envelope *and*
  hold a wrap we can open, unseal the body + blob content-keys and re-seal
  them to the **caller's connection key** (sealed-box, the A3 primitive) as
  a fresh `KeyWrap { recipient: caller, sealed }`. Cheap — no body
  re-encryption (SPEC §5.2).
- **Authorization**: own devices only at D3 — the caller's key must be in
  the local own-devices store. This is also the **gate extension**: D0c's
  `serves` self-allowance becomes "caller == me **or** caller ∈ my verified
  device cluster", so own-device sync (get / get-successors / who-is /
  GetKeys) rides one rule. Verified against local state only; nothing in
  the request is trusted.
- **Requester side**: after the D0 skeleton sync leaves messages stored but
  unopenable, the new device batches their ids to a paired device, verifies
  each returned wrap (`recipient` = own key, keys unseal, body opens — a
  wrap that doesn't open is dropped with a warning), and **appends** the
  wrap to the stored envelope. Wraps live outside the hashed core: the id
  is unchanged, storage just rewrites the envelope file. Runs
  opportunistically (after pairing, after auto-sync heals) — best-effort,
  like every peer op.

## 7. Presentation: clustering

- **Labels**: a key resolves through the contact store by membership in a
  record (already true) — both device keys hit the same record → same
  petname. Edges dedup labels per person (a two-device contact renders
  once, not twice, in conversation labels and reply lists).
- **The popup upgrade** (groups.md §5 hand-off): before rendering "a wild
  key appeared", check the D2b-learned records: if one contains the unknown
  key AND an already-trusted key of contact P, with **mutual links
  verifying**, render *"P added a device (mutually verified)"* with a
  one-tap **offer** to update P's contact entry — the same explicit
  `add_contact` act, popup-assisted. Declining is fine: the key stays a
  separate (hex or wild) entry, and messages still flow — nothing depends
  on the observer merging. `same-person-as` evaluation enters here and in
  the §6 gate — nowhere else.
- **Membership deltas**: "+ Alice's new device" renders via the same
  clustering (a joined key that clusters to a known person labels as that
  person).
- **Many-to-many latitude (future, parked)**: labels↔keys is the observer's
  many-to-many relation — merging several *people* under one label
  ("someone from the soccer team") is a legitimate client policy this
  model must not preclude. The one current hardening against it is the
  petname-collision check (one petname → one record, so send-by-name stays
  unambiguous); relaxing it means separating *display labels* from
  *addressing names* — deliberate future work, nothing in D3 makes it
  harder.

## 8. Security notes

- **Mutual-or-nothing**: unilateral links never rank as verified evidence
  and never widen the gate. A stolen record can't be upgraded into a
  device claim without both signatures. And since nothing auto-adopts,
  even a verified link only ever produces an *offer* on other people's
  screens.
- **Local trust anchors**: the own-devices store is written only by the
  pairing flow (explicit scans + confirms on both sides); serving decisions
  read it, never the wire.
- **Re-wrap scope**: own-device-only serving keeps "willingness to re-wrap"
  (SPEC §5.2) at its narrowest until the recovery flows (D4+) need more.
  A compromised paired device can read everything — that is the honest
  meaning of pairing, and revocation is exactly the deferred `Negative`
  flow.
- **Repudiation lag** (parked, recorded): a lost device keeps receiving
  until contacts learn otherwise; the lazy model's cost concentrates here.
  Sibling devices are the *primary* completeness channel; contacts'
  fan-out is robustness — never load-bearing (the over-complication guard
  from the reorg discussion).

## 9. Slices

- **D3a · Identity core.** `ContactRecord::device_cluster()` (keys connected
  to a given key by verified mutual links) in the protocol crate; the
  key-overlap contact-identity fix + petname-collision by overlap; label
  dedup per cluster in edges' data (membership/summary labels). *Done
  when:* unit tests — mutual links cluster, unilateral/forged don't; a
  re-scanned record with reordered/added keys updates the same contact.
- **D3b · Pairing + gate.** Own-devices store; pair flow in `Client`
  (store partner, sign link, adopt profile, `my_record` gains keys+links,
  completion query); the D0c gate extension; CLI `pair-show` / `pair-scan`
  (dev shape TBD at implementation). *Done when:* headless e2e — two
  clients pair; both `my_record`s list both keys with mutual links; the
  partner is served by the gate like self.
- **D3c · Send-to-self + clustering offers.** Recipients gain own devices
  (the core mechanism); §7 popup upgrade (evidence-ranked offer, explicit
  accept); optional introduce-now sugar. *Done when:* headless e2e — after
  pairing, P's next message carries both keys; the contact's client
  renders "P added a device" evidence and, on the explicit accept, both of
  P's devices receive the contact's reply.
- **D3d · Re-wrap.** `GetKeys` op + serve/request sides + wrap-append
  storage; opportunistic run after pairing/sync. *Done when:* headless e2e
  — the paired device reads bodies from before it existed (the D2a-style
  full flow), and a non-own-device caller gets `NotHeld`.
- **D3e · App UI + acceptance.** Pair mode screens (show/scan + confirm),
  device list in the me-view, introduction button, popup upgrade wired.
  *Done when:* the plan's acceptance live — pair a second device, introduce,
  contacts cluster it, it reads old history via re-wrap.

## 10. Doc touchpoints when this lands

- SPEC §6: record the full send-to-self (the C3 note's pending line) (D3c).
- SPEC §11: pin `GetKeys` shape + own-device-only serving (D3d).
- client-core.md: pairing APIs, own-devices store, gate rule (D3b–d).
- who-is-this.md §7: resolve "key-set changes wait for D3" as
  explicit-forever, evidence-assisted (D3c).
- groups.md §5: popup upgrade cross-reference (D3c).
- mvp-build-plan.md: tick sub-slices as they land.
