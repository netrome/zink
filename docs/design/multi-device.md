# Multi-device: pairing, clustering, re-wrap (D3)

Design for the D3 sub-slices. Downstream of [SPEC](../SPEC.md) §3.2
(attestations / `same-person-as`), §3.6 (pairing uses the contact artifact),
§5.2 (re-wrap; "no cryptographic difference between your new device and a new
member"), §6 (send-to-self); rides [groups.md](./groups.md) end to end — a
new device IS a new member, propagated by the signed `recipients` list and
resolved by the D2 pipeline. D3 adds the three things groups deliberately
deferred: the **recognition link** (which keys this device treats — and
vouches — as itself), the **re-wrap op** (reading history from before the
key existed), and **send-to-self** (your own devices as implicit
recipients).

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
| Link shape | `Attestation { attester: K, subject: K, claim: SamePersonAs(L) }` — "I, K, am the same person as L". **Links are asymmetric, like everything else** (resolved at review): each direction is an independent act, and evidence is *tiered*, never gated on mutuality (SPEC §3.2 weights, not requires). For observers, the load-bearing direction is **trusted-key → new-key** (a key you already trust vouches the new one — unforgeable); the reverse alone (a stranger claiming a trusted key) is the spoof direction and never clusters; both together upgrade the label to "mutually confirmed" — consent-proof against vouching an unrelated victim's key into your identity (§8). |
| Where links live | In the **record's** `attestations`, like name/avatar claims (SPEC §3.6 says exactly this). **Records stay per-device** (resolved at review): `keys` remains the device's own key; the "pair" exists only as two verified links held across two ordinary records — there is no person record and no merged key list. (Listing sibling keys stays *permitted* as an advisory addressing offer; nothing reads it as authority.) |
| Profiles are per-device | Each device self-claims its **own** name and avatar — "mårten phone" and "mårten laptop" are both correct, and a profile edit on one device syncs nowhere. Pairing may *prefill* the new device's name from the old one's claim; it never adopts or synchronizes profiles. |
| Pairing flow | **A one-way "recognize this device as me" act** (§3): scan the other device's ordinary QR, confirm the fingerprint, sign the link, store the key in the own-devices set. The shown side is passive — no pair mode, no handshake, no completion choreography. The common two-way case is the same act run once in each direction; "pairing" is the colloquial name for that composite. |
| Contact identity | Fixed from `keys.first()` to **key-overlap** (§4) — the parked review note lands here. A contact entry is the observer's local grouping of keys under a petname; the record inside it is evidence, not authority. An overlap-driven update is **explicitly confirmed, never silent** (§4; gap review, 2026-07-19). |
| Key adoption by contacts | **None — automatic adoption is rejected** (the review's simplification). Sealing rules are unchanged from D1b/D2 (user-added records + signed cores); my devices reach contacts' sealing sets through *my* recipients lists, not through their evaluation of my claims. Updating a contact entry stays the one explicit act, now popup-assisted with link evidence (§7). D1b's "key-set changes wait for D3" resolves as: *they stay explicit forever; D3 adds the evidence.* |
| Send-to-self | **The core mechanism.** Own other devices are appended to every send's `recipients` and deposited like any recipient (§5) — devices are honest conversation members, joined by their owner's signature. The SPEC §6 note gets recorded when this lands. |
| New-device bootstrap | Lazy by default: send-to-self makes the *next organic message* per conversation carry the new key — no enumeration op, no new mechanism. An optional "introduce now" button (the D2c add-member gesture per conversation) is pure UX sugar for the impatient (§5). The device's own *sending* bootstraps the same way — own-device authorship legitimizes, siblings answer its scoped auto-queries (§5; gap review, 2026-07-19). |
| Re-wrap | `SyncOp::GetKeys { ids }` → per-id `KeyWrap`s re-sealed to the caller (§6); served to **own devices only** at D3. Wraps append outside the hashed core — ids never move. |
| Gate extension | The D0c `serves` self-allowance widens from `caller == me` to `caller ∈ my verified device cluster` (§6) — evaluated against the local own-device store, never against claims in the request. |
| Device-record serving | The mirror of the gate extension (gap review, 2026-07-19): a `WhoIs` whose **subject** ∈ my verified device cluster is answered with that device's stored own-devices record, like a contact-store subject (§6). Recognizing a device is a willingness to advertise it — and without this rule *nobody* can serve a new device's record (its own contact store is empty; siblings hold the record only in the own-devices store), so the §7 accept would have no record to store and contacts no robustness route. |

## 3. Recognizing a device (one-way; run twice for the usual case)

One act, one direction: **"recognize this device as me."**

1. The device to be recognized shows its **ordinary QR** (its record —
   nothing special; a fresh install sets its own profile first, name
   prefilled-by-hand from whatever its owner likes: "mårten laptop").
2. The recognizing device scans it, shows the fingerprint + claimed name,
   and on an explicit **confirm** signs its link (`me: same-person-as
   scanned-key`) and stores the key + record in the **own-devices store**
   (not contacts, no petname).

From that moment, *this* device includes the recognized key in everything
it sends (§5), serves it like itself (§6), and its record carries the vouch
(§4). That's the whole transaction — the shown side did nothing but be
scanned.

The usual "pair both ways" is the same act run from the other device (scan
back, confirm, sign); the UI suggests it, nothing requires it. Asymmetric
setups are legitimate, but **one-way means receive-only** (resolved at the
gap review, 2026-07-19): a phone can recognize a read-mostly car that never
recognizes anything back, and the car receives everything the phone sends
from its first inclusion onward. What it can't do is *pull* — dialing a
sibling by key (backfill, `GetKeys`) requires holding that sibling's
record, and the record is held exactly where the recognize act stored it.
So a device that wants the backlog recognizes back — vouching your own
device is always a true statement, and the usual two-way pairing gives it
for free. No store-without-vouch act exists (it would fork the own-devices
store's semantics for a marginal case); each device's recognition set
stays its own social-graph decision, like everything else in the app.

**Confirm before signing** (the one real risk): the recognize act signs
after a scan, so scanning a wrong QR must not silently vouch an attacker —
hence the fingerprint + explicit confirm. What a mistaken vouch *can't* do
is silently move the other direction: the scanned key gains serving and
inclusion from *this* device only.

## 4. Records & contact identity

- **`my_record`** gains exactly one thing: my outgoing link attestations
  (the vouches this device signed). `keys` stays my own key; my name stays
  my own claim. Observers gather link evidence across the records they
  hold; what they render is theirs.
- **Contact identity = key overlap** (the parked `keys.first()` fix):
  `add_contact` and the petname-collision check identify an existing contact
  by *any shared key*; a re-scan with reordered or added keys updates that
  contact instead of forking a duplicate. The store stem is re-derived from
  the updated record; the petname is untouched.
- **Overlap is surfaced, never silently resolved** (gap review, 2026-07-19).
  A record's `keys` list is unauthenticated per-key (listing extra keys is
  an advisory offer, §2) — so overlap-updates-the-entry, applied silently,
  is a hijack: a hostile record that smuggles one of Bob's keys into its
  own list would, on an innocent "add Mallory" (paste, or the popup's
  one-tap promotion), *replace Bob's stored record* — the trust anchor that
  supplies sealing keys and relays (D1b) — while keeping his petname. The
  rule: an add whose record overlaps an existing contact **asks** ("this
  record shares a key with Bob — update Bob's entry?") whenever the user's
  act said *new contact* (a different petname typed, or a promotion);
  overlap spanning **two or more** existing contacts never merges — it
  surfaces both and stops. The benign re-scan (same person, fresh record)
  sails through the same confirm with the matched petname shown. Updating
  stays one explicit act; the overlap check only decides *which* entry the
  act is about — and when in doubt, it asks the observer.
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
- **The new device bootstraps its own sending the same way** (gap review,
  2026-07-19). Its contact store starts empty (per-device, never synced —
  deliberate), which unfixed would mute two D2 mechanisms: the
  contributing-contact rule fails everywhere (no conversation legitimate →
  the scoped auto-query never fires) and no contact is dialable, so its
  replies would seal correctly (the D2a unroutable-members rule) but
  deliver only to siblings, reaching contacts on the DAG's next heal. Two
  own-device allowances close the loop, both philosophically free:
  **own-device authorship counts as a contributing contact** (no key I
  trust more than my own), and **own devices are auto-query responders**
  (dialable via the own-devices store). The new device's first drain then
  auto-queries its siblings about the conversation's members; the sibling
  serves its contact-store records (the §6 gate extension already covers
  the caller side), routes land in the learned store, and replies from the
  new device deliver directly — no new mechanism, the D2 pipeline again.

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
- **The `WhoIs` serve set gains the mirror rule** (gap review, 2026-07-19):
  the gate extension widens who may *call*; this widens which *subjects*
  are answered for — a subject ∈ my verified device cluster is served its
  stored own-devices record, to any caller I serve. Recognizing a device is
  precisely a willingness to advertise it as mine; without this, a new
  device's record is unservable by anyone (§2, the device-record-serving
  row) and the §7 accept has nothing to store. Freshness rides the same
  machinery as everywhere: a sibling that changes home relays is re-learned
  by a subject-served `who-is` to it (own devices are dialable), read-time
  resolution unchanged.
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
  key appeared", evaluate the held evidence (stored + learned records),
  tiered: a verified link **from an already-trusted key of contact P**
  vouching the unknown key → *"P says this is their device"*; both
  directions verified → *"…mutually confirmed"*. Either tier is a one-tap
  **offer** — purely a naming act: store the device's record as its own
  contact entry, petname prefilled from its self-claim ("mårten laptop"),
  rendered clustered with P. The reverse direction alone (the unknown key
  claiming P) never clusters — that's the spoof direction; it stays a wild
  key whose claim renders as exactly that. Delivery never depended on any
  of it — replies reach both of P's devices through membership (P's own
  send-to-self put both keys there). Declining costs a hex label, nothing
  more. `same-person-as` evaluation enters here and in the §6 gate —
  nowhere else.
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

- **Tiered, never gated**: the spoof direction (a stranger's key claiming
  a trusted one) never clusters — vouching must come *from* trust. The
  one-way-vouch tier admits a subtler misuse: a malicious contact can
  vouch an *unrelated victim's* key as "their device", misattributing the
  victim's messages to themselves on accepting observers' screens — which
  is why the tier labels say who claims what ("P *says* this is their
  device") and mutual confirmation upgrades, not gates. And since nothing
  auto-adopts, any tier only ever produces an *offer*.
- **Local trust anchors**: the own-devices store is written only by this
  device's own recognize acts (explicit scan + confirm); serving decisions
  read it, never the wire. Recognition is per-device and per-direction —
  there is no global device set anywhere.
- **Re-wrap scope**: own-device-only serving keeps "willingness to re-wrap"
  (SPEC §5.2) at its narrowest until the recovery flows (D4+) need more.
  A compromised paired device can read everything — that is the honest
  meaning of pairing, and revocation is exactly the deferred `Negative`
  flow.
- **The cluster is visible metadata**: send-to-self puts my device keys in
  every plaintext `recipients`, so relays and all recipients see the
  cluster and its growth. Inherent (recipients must be visible to route,
  SPEC §6) and deliberate — the signed list IS the announcement (§5); one
  more reason metadata minimisation stays a later track.
- **Repudiation lag** (parked, recorded): a lost device keeps receiving
  until contacts learn otherwise; the lazy model's cost concentrates here.
  Sibling devices are the *primary* completeness channel; contacts'
  fan-out is robustness — never load-bearing (the over-complication guard
  from the reorg discussion).

## 9. Slices

- **D3a · Identity core — done (2026-07-19).** Tiered link-evidence
  evaluation over a *set* of held records (protocol helper over
  attestations; the client aggregates stored + learned records):
  vouched-from-trust vs mutually-confirmed vs spoof-direction-only; the
  key-overlap contact-identity fix + petname-collision by overlap, with
  the overlap confirm (§4); clustered label dedup in edges' data.
  *Done when:* unit tests — a trusted key's vouch tiers as offerable, both
  directions upgrade, the reverse direction alone and forged signatures
  tier as nothing; a re-scanned record with reordered/added keys updates
  the same contact through the confirm; adversarial overlap — a record
  smuggling a *different* contact's key, and one overlapping two contacts
  — surfaces instead of silently merging (§4).
  *(As built: `zink_protocol::link_tier` over `SignedAttestation`s — only
  verified, self-attested links count, so a forged reverse link can't fake
  the mutual upgrade; `add_contact`'s same-petname add is the confirm,
  `ContactOverlap`/`AmbiguousOverlap` are the refusals;
  `Client::participant_labels` is the dedup seam both edges render from.)*
- **D3b · Recognize + gate — done (2026-07-19).** Own-devices store; the
  one-way recognize act in `Client` (store key + record, sign link,
  `my_record` gains the vouch); the D0c gate extension (recognized keys
  served like self) + the `WhoIs` mirror rule (recognized-device subjects
  served, §6); CLI `recognize` (dev shape TBD at implementation). *Done
  when:* headless e2e — A recognizes B: A serves B like self while the
  reverse direction stays closed until B recognizes A back; a contact's
  `WhoIs` for B's key at A returns B's stored record; each record carries
  only its own vouches.
  *(As built: `recognize_device` signs at revision 0 — supersession scopes
  per linked key, so re-recognize is idempotent; the gate trusts only the
  *vouched* key of each stored device, never the rest of a record's `keys`
  list (§4's lesson); the mirror rule answers ahead of contact lookups;
  CLI `recognize` + `devices`, where pasting the payload is the confirm.
  SPEC §3.2/§3.6 + who-is-this.md §4 + client-core.md updated per §10.)*
- **D3c · Send-to-self + clustering offers — done (2026-07-19).**
  Recipients gain own devices (the core mechanism); the fresh-device
  bootstrap (own-device authorship legitimizes, own devices as auto-query
  responders — §5); §7 popup upgrade (evidence-ranked offer, explicit
  accept); optional introduce-now sugar. *Done when:* headless e2e — after
  pairing, P's next message carries both keys; the contact's client
  renders "P added a device" evidence and, on the explicit accept, both of
  P's devices receive the contact's reply; and a reply sent *from* the new
  device reaches the contact directly, not only via siblings.
  *(As built: appended in `finish_send`, sender unlisted (self-wrap);
  found at implementation — the send-by-name conversation lookup must try
  the device-extended participant set first, else post-pairing sends fork;
  `device_evidence` tiers per contact over attestations aggregated from
  stored + learned records, rendered in the popup and CLI `who-is`;
  introduce-now needs no API — it is an empty-body `send_in`, the button
  lands with D3e. The acceptance e2e proves directness with the sibling
  offline from before the contact's reply onward.)*
- **D3d · Re-wrap — done (2026-07-19).** `GetKeys` op + serve/request
  sides + wrap-append storage; opportunistic run after pairing/sync. *Done
  when:* headless e2e — the paired device reads bodies from before it
  existed (the D2a-style full flow), and a non-own-device caller gets
  `NotHeld`.
  *(As built: `MessageEnvelope::rewrap` checks every unsealed key against
  its commitment in the signed core before re-sealing — a tampered stored
  wrap can't be laundered onward; batch cap `MAX_GET_KEYS_IDS = 24`
  enforced on both sides; the serve predicate is the recognized-devices
  set alone, so a contact's `GetKeys` declines as a miss while their
  history access is untouched; `auto_rewrap` rides the drain seams after
  `auto_sync`, and `backfill_by_key` resolves siblings through the devices
  store so the whole skeleton-then-keys flow works by key alone.)*
- **D3e · App UI + acceptance.** Pair mode screens (show/scan + confirm),
  device list in the me-view, introduction button, popup upgrade wired.
  *Done when:* the plan's acceptance live — pair a second device, introduce,
  contacts cluster it, it reads old history via re-wrap.

## 10. Doc touchpoints when this lands

- SPEC §6: record the full send-to-self (the C3 note's pending line) (D3c).
- SPEC §3.2 + §3.6: the one-way recognize revises the "pairing handshake
  yields a **mutual** link" language — mutual is the composite of two
  independent acts, not the product of one exchange (D3b).
- who-is-this.md §4: the serve set gains own-device subjects — the §6
  mirror rule (D3b).
- SPEC §11: pin `GetKeys` shape + own-device-only serving (D3d).
- client-core.md: pairing APIs, own-devices store, gate rule (D3b–d).
- who-is-this.md §7: resolve "key-set changes wait for D3" as
  explicit-forever, evidence-assisted (D3c).
- groups.md §5: popup upgrade cross-reference (D3c).
- mvp-build-plan.md: tick sub-slices as they land.
