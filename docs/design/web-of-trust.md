# Web of trust: endorsements, repudiation, recovery (D4)

Design for the D4 sub-slices. Downstream of [SPEC](../SPEC.md) §3.2
(attestations — the one primitive this whole layer reuses), §3.4 (recovery
is social), §3.5 ("who is this?" — answerable "by anyone else: return
*their* attestations about that key", the line this doc finally cashes in);
rides [who-is-this.md](./who-is-this.md) (the answer channel, the learned
store, read-time ranking) and closes what
[multi-device.md](./multi-device.md) parked: the repudiation-lag note and
`Negative`-claim handling deferred from D3a.

**The governing principle: evidence accumulates; nobody arbitrates.** A
third-party claim — "I call this key Carol", "I no longer recognize this
key" — is one more signed input to each observer's *local* belief, weighed
by their trust in the attester and always beneath their own manual state
(tenets 1–3). Nothing in this layer auto-applies, auto-revokes, or reaches
consensus: the protocol carries claims; every client decides alone what
they mean. Conflicts (two keys disavowing each other) are *surfaced*, never
resolved by the system — SPEC §3.4's "the protocol does not arbitrate" is a
feature this doc must not erode.

**Defensive framing.** One wire *field* (endorsements on the existing
`WhoIs` answer), zero new claim kinds (`Negative` has existed since A2 —
D4 makes it mean something), zero new stores (endorsements land in the
learned store; issued vouches live in client state like the profile). If a
step seems to need a reputation score, a revocation list, a trust registry,
or any claim that applies without the observer's policy consenting — stop;
it doesn't.

## 1. Goal & non-goals

**Goal.** Names get social depth ("your friends call them Carol"), and keys
get a social exit: a contact can vouch for a key, and a person can disavow
a lost one — with contacts' clients *learning* the disavowal and dropping
the key from their addressing, each by their own policy. The lost-device
story that multi-device opened ("a lost device keeps receiving until
contacts learn otherwise") closes here.

**Non-goals (deferred, with homes):** query forwarding / hops > 1 (a
version bump with a transitive-privacy story to design — post-MVP; the
endorsement rule in §3 keeps hop limit 1 structural in the meantime);
numeric per-contact trust weights and any automated weighing beyond
provenance classes + agreement counts (client differentiation, post-MVP);
avatar endorsements (deferred with the per-attester lens, §6 — "Bob's
picture of her" is a real future use; the wire field already carries any
claim kind, so adding them later is an evaluation change, not a version
bump); third-party `same-person-as` evaluation
(`link_tier` stays self-attested-only — a friend vouches a *name* for a
key, never someone else's device link); automatic vouching (broadcasting a
petname is an explicit act, SPEC §3.2 — privacy default); any enforcement
(a `Negative` is advisory evidence, never a kill switch).

## 2. Decisions

| Decision | Resolution |
|---|---|
| How third-party claims travel | The `WhoIs` answer gains them: `SyncResult::Known { record, endorsements: Vec<SignedAttestation> }` (field added in-place at v1 — pre-deployment norm). The record stays the subject's artifact, **served verbatim as today**; endorsements are the responder's *own* signed claims about the subject, riding beside it. SPEC §3.5 already frames answers exactly this way. |
| Hop limit stays structural | An endorsement is accepted only if its `attester` **is the answering connection key**. Relaying *other people's* endorsements would be second-hand gossip — precisely the hops>1 forwarding deferred out of D4. One rule keeps the whole layer 1-hop. |
| Endorsed claim kinds (MVP) | `Name` (the vouch: "I call this key Carol") and `Negative` (the disavowal). Nothing else is served or evaluated at D4. |
| Issuing a vouch | **Explicit, per contact** (`Client::vouch(petname)` — a "share my name for them" act): it broadcasts your petname, which SPEC §3.2 keeps private by default. Withdrawn by a higher-revision `Negative` (§4), like everything. No auto-vouch on add. |
| `Negative` semantics | `Attestation { attester: K, subject: L, claim: Negative }` = "I, K, disavow key L." **The voiding rule (§4):** any claim by K that *binds* L — a `Name` about L, a `SamePersonAs` link whose linked key is L, a vouch — is void while K's `Negative` about L stands at a strictly higher revision. Cross-kind by design; SPEC §3.2's supersession paragraph gets sharpened (touchpoint, §8). |
| What a repudiation does | Nothing, by itself — it's evidence. Each observer's client: removes L from *that attester's* contribution to a person's key cluster, drops L from reply/fan-out sets when the disavowal comes from a key the observer trusts for that person (their own policy), renders the honest line ("K disavowed this key"). **Manual override always wins** (SPEC §3.1). |
| Repudiation vs membership | Dropping a repudiated key from a reply IS the deliberate stop-include (groups.md §2) — unlike routelessness (the D2a hazard), exclusion here is the *intended* meaning. The key stays in history; it stops being addressed. |
| Mutual disavowal (theft race) | A stolen device can disavow back — two keys of one person each claiming the other is bad. Not arbitrated (SPEC §3.4): both negatives render, addressed keys shrink to what the observer still trusts, and the tie is broken out-of-band (you call your friend). The UI says exactly that. |
| Recovery flow | Social, two acts, both existing primitives: the person's **new** key is added the normal way (QR / who-is + explicit add), and the *friend* vouches it (`Name`) + repudiates the old one (`Negative`) — each an explicit act on their own device, propagated as endorsements. No recovery object, no ceremony. |
| Ranking (MVP) | Endorsed names join `resolve_name` as a provenance class: petname (manual) > subject's verified self-claim > **endorsed names** ("2 friends call them Carol" — agreement count + who) > hex. Within a class: revision, then count. No numeric weights. Ranking is only the **default lens** — the label printed where the UI must pick one (chat labels, lists, notifications, prefills) with no user input; the underlying data stays per-attester, never merged (§6). |
| Fork views | A presentation-only indicator where the linearization hides real concurrency: consecutive linearized messages that are causally incomparable render a "crossed in flight" marker; a message with multiple parents renders as the merge it is. Derived from the DAG at history build; the deterministic linear default is untouched (tenet 7). |

## 3. Endorsements on the wire

```
SyncResult::Known {
  record:        Box<ContactRecord>,        // the subject's artifact, verbatim (unchanged)
  endorsements:  Vec<SignedAttestation>,    // the RESPONDER's own claims about the subject
}
```

- **Serve side**: alongside the stored record, attach this device's own
  issued claims about the subject — its vouch (`Name`) and/or disavowal
  (`Negative`), from client state. Only claims signed by *this device's
  key*: a responder never relays what others told it (hop 1, structural).
- **Requester side**: verify each endorsement like everything else —
  signature valid, `attester == the connection key that answered`,
  `subject == the queried key`. Anything else is dropped with a warning,
  never fatal. Survivors land in the learned store next to the answer,
  keyed by responder — the store's `(subject, responder)` shape already
  fits, and re-answers replace per responder like records do.
- **Nothing is pushed — and nothing polls.** Endorsements travel only
  inside `WhoIs` answers — the same lazy, pull-based, gated channel as
  records. A **freshness pull** (the term of art since D1c) is simply the
  manual "who is?" action re-run against a known key: a *user-triggered*
  re-ask whose answers sharpen read-time resolution by themselves. The
  only automatic query in the system is D2b's scoped one — first
  encounter of an unknown member, once per (subject, conversation) per
  run — and known contacts are never re-queried on any schedule: a query
  broadcasts your interest to whoever you ask, so queries stay scarce and
  intentional (the D1 privacy stance). A vouch therefore reaches whoever
  asks the voucher; a repudiation spreads exactly as fast as pulls and
  first encounters do. Best-effort and honest about it (tenet 6) — the
  repudiation-lag note from multi-device stays true, it just finally has
  a decreasing tail. *(If the lag ever bites, the designed lever is SPEC
  §3.6's piggybacked record-version hint — event-driven, riding traffic
  you already receive, still zero polling. Not built; not needed yet.)*

## 4. The voiding rule (`Negative` evaluation)

> **A claim by K that binds key L is void while K's `Negative` about L
> stands at a strictly higher revision.**

"Binds L" means: a `Name`/`Avatar` whose subject is L; a `SamePersonAs`
link whose *linked key* is L (the link lives at `(attester: K, subject: K,
SamePersonAs(L))` — the negative that kills it is `(K, L)`, so the rule is
deliberately cross-shape); a vouch about L. Re-vouching after a disavowal
is a yet-higher-revision positive claim — supersession stays one mechanism
with no special revocation state, exactly SPEC §3.2's intent, now written
down precisely.

Where it's evaluated — the same read-time seams that already exist, never
storage:

- `link_tier` (D3a): a voided link contributes nothing; a mutual pair with
  one side voided degrades to the surviving direction's tier.
- `resolve_name` / `learned_candidates`: voided names drop out of their
  attester's contribution; a disavowal from the *subject's own* sibling
  key or from contacts renders as its own line, not silence.
- Reply/fan-out resolution (SPEC §3.3 "a repudiated key drops out of the
  set"): when the observer's policy accepts the disavowal — MVP default
  (**sharpened at implementation, 2026-07-21**): it comes from the
  observer's own keys, or from *the same person* — a shared contact
  entry, **or any held `SamePersonAs` between attester and key, where the
  same-person test deliberately ignores voiding**: a voided link no
  longer clusters, but it remains proof the keys were once one person,
  which is exactly what makes the disavowal self-referential ("their own
  key disavowed it") rather than third-party. Per-device contact entries
  made the original same-entry wording insufficient — the phone and the
  lost laptop are separate entries, and the surviving same-person
  evidence is the laptop's own reverse link in its promoted record. The
  key is then excluded from addressed sets: a deliberate stop-include
  (§2 table). Third-party negatives render as warnings, never exclude
  (§7). Explicit acts — sending to the disavowed entry by name — always
  still work: that IS the manual override, with no extra state.
- The own-devices store: repudiating a sibling also un-recognizes it
  locally (serving, send-to-self, and re-wrap stop; the vouch in
  `my_record` is superseded by the published `Negative`).

**What a `Negative` never does:** delete history (messages stand — you
cannot unsend, and authorship stays honest), propagate by itself (it's
pulled, like everything), or bind any observer who chooses otherwise.

## 5. The recovery flow (social, per SPEC §3.4)

The drill, end to end — every step an existing primitive plus §3/§4:

1. **Lost a device, kept one**: from the survivor — repudiate the lost key
   (`Negative`, published in `my_record` + served as an endorsement) and
   un-recognize it. Contacts converge as their freshness pulls and
   auto-queries land; each stops sealing to the lost key by its own
   policy. *(This is the multi-device repudiation-lag closure.)*
2. **Lost everything**: new key, new record, and the out-of-band call —
   the friend scans the new QR (explicit add, as ever), **vouches** the
   new key, and **repudiates** the old one on the person's say-so. Other
   contacts asking that friend now learn all three facts at once: a
   record, a name they trust the friend about, and a disavowal. Their
   accept stays theirs.
3. **The attacker's counterplay** (stolen device disavows back): both
   negatives surface; nothing auto-picks a winner. Observers see "these
   keys disavow each other — X vouches for this one" and decide; the
   out-of-band call is the tiebreaker, by design.

## 6. Presentation

- **who-is panel / popup**: endorsed names rank per §2 ("Carol — you're
  told by Bob, Dana"); disavowals render as warnings on the key wherever
  it appears ("Bob disavowed this key", "their other device disavowed this
  key"), including on the promote/add offers — evidence at the moment of
  decision, never a block.
- **Contact view**: a per-contact **vouch** toggle (issue / withdraw), the
  honest framing spelled out ("shares the name you call them with anyone
  who asks you about them").
- **Device list**: un-recognize (local) and repudiate (local + published)
  as separate acts — losing interest in a sibling is not the same as
  declaring it compromised.
- **Per-attester lenses (future, parked — sharpened at review,
  2026-07-20)**: nothing in this layer ever merges views — the learned
  store and endorsements stay keyed by responder/attester — so "view this
  profile as Bob" (Bob's name and picture for a person, rendered with an
  according-to-Bob marker; "see this chat as Bob") is a pure presentation
  feature a later client builds from data it already holds. Two
  boundaries, both deliberate: a lens shows **what Bob tells you** (his
  broadcast vouches), never "what Bob sees" — unshared petnames stay
  private; and lenses are display-only — addressing always resolves
  through *your* store (the multi-device.md §7 display-vs-addressing
  separation, the same parked latitude). Nothing in D4 may preclude this.
- **Fork views** (D4d): the "crossed in flight" marker between causally
  incomparable neighbors and a merge marker on multi-parent messages.
  Advanced-view data made visible, nothing reordered. *(Orthogonal to the
  trust layer — D4 carries it only to clear SPEC §12 phase 3's bundled
  entry; it shares no design surface with §2–§5 and could land any time.)*

## 7. Security notes

- **Defamation / wrong-name vouching**: an endorsement always renders as
  *who* says it and counts only per-attester — a malicious contact can
  make their own voice lie, never someone else's, and never silently (the
  same tiered-not-gated stance as multi-device §8).
- **Negative griefing**: a contact disavowing a victim's real key only
  moves observers who weigh that contact's word for that person; the
  subject's own verified activity (authorship, self-claims) stays visible
  beside the warning, and manual state wins. No observer can be *made* to
  drop a key.
- **Revision races**: revisions are per-attester counters, so an attacker
  can outbid only their own past claims — a stolen device can escalate its
  own negatives (the §5.3 mutual-disavowal surface) but cannot void
  anyone else's vouches.
- **Privacy**: vouching reveals your petname for a person to anyone you'd
  answer about them (your contacts — the existing gate); hence the
  explicit act. Endorsements never widen the serving gate, never carry
  learned material, and never travel except inside answers you chose to
  give.

## 8. Slices

- **D4a · Endorsements + vouch — done (2026-07-21).** Wire field (in-place
  at v1); serve-side attach (own claims only); requester validation +
  learned-store landing; `Client::vouch(petname)` / withdraw; ranking
  class in `resolve_name` / `learned_candidates`; CLI `vouch`. *Done
  when:* headless e2e — B vouches Carol, A's who-is shows the endorsed
  name with provenance; an endorsement whose attester ≠ the answering key
  is dropped; no vouch issued → no endorsement served (nothing
  auto-broadcasts).
  *(As built: the learned store's per-responder entry replaces wholesale
  — endorsements included — so `unvouch` propagates by absence on the
  next pull; endorsement revisions never mix into self-claim ordering
  (different supersession scopes); endorsed-only name groups pair with
  the endorsing responder's served record as their promotable payload.)*
- **D4b · Negative claims + repudiation — done (2026-07-21).** The §4
  voiding rule in the protocol (`link_tier`, name claims) with the
  cross-kind supersession pinned in SPEC §3.2; `Client::repudiate(key)`
  (sign + publish + serve; un-recognize when it's a sibling); read-time
  exclusion from clusters and addressed sets; CLI `repudiate`. *Done
  when:* unit + headless e2e — a higher-revision negative voids the vouch
  and the device link (and a yet-higher re-vouch restores it); after the
  phone repudiates its lost laptop, a contact's next freshness pull stops
  sealing to the laptop and renders the disavowal; the observer's manual
  override survives everything.
  *(As built: the stance store holds one latest claim per subject, so
  re-vouch-after-repudiate is free; the §4 same-person scoping was
  sharpened at implementation (see the §4 note); the manual override is
  structural — exclusion applies only to automatic reply fan-out, never
  to explicit sends — and the record path carries a repudiated sibling's
  disavowal, since its own record stops being servable.)*
- **D4c · Recovery acceptance + UI.** The §5 drill live: vouch toggle,
  disavowal warnings, un-recognize vs repudiate in the device list, the
  friend-assisted flow. *Done when:* the lost-device drill runs across
  real devices — contacts converge on the new key and stop addressing the
  old one, each through their own explicit accepts.
- **D4d · Fork views.** The §6 concurrency indicator, derived at history
  build. *Done when:* two clients send concurrently and both render the
  crossed-in-flight marker on sync, with the linear default unchanged.

## 9. Doc touchpoints when this lands

- SPEC §3.2: sharpen supersession with the §4 voiding rule (cross-kind:
  a `Negative` voids the attester's binding claims about that key) (D4b).
- SPEC §11: pin the endorsement wire shape + attester-is-responder rule
  (D4a) and the `Negative` evaluation rule (D4b).
- who-is-this.md §3/§5/§6: the answer gains endorsements; ranking gains
  the endorsed-name class (D4a).
- multi-device.md §1/§8: the repudiation-lag and `Negative` deferrals
  resolve here (D4b).
- client-core.md: vouch / repudiate APIs, endorsement handling (D4a–b).
- mvp-build-plan.md: tick sub-slices as they land.
