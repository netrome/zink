# Profile pages: one person page over a key cluster

> **Status: 🚧 in progress — scoped 2026-08-20.** Branch: `profile-page`.
> Trigger: a message request from an unknown key offers no way to preview
> who sent it, and contacts have no browsable profile page.

## TL;DR

One **person page** for contacts and strangers alike, reachable by tapping
any identifier anywhere. It renders the three layers of belief
(ui-design-system.md §1) — my lens, their claims, friends' vouches — with
per-device detail. Around it, the model work that keeps it honest:

- **Person vs device names**: self-claims stop overloading one string —
  "mårten laptop" becomes name "Mårten" + device label "laptop".
- **Person entries**: a local label over a cluster of keys — what "write
  a message to Alice" resolves to.
- **Own-device lens sync**: my phone and laptop converge on the same
  clusters *by default, never by assumption*.

## Decisions (scoped 2026-08-20)

| Decision | Resolution |
|---|---|
| One page, cluster-keyed | Generalize the Person view. A stranger is a one-key cluster with no petname, rendered from the learned store, with add / ignore / manual who-is. |
| Person entry (local) | `{ local id, person label, avatar override, member contact entries }`. The id is an opaque local token (`PersonId`, 128 random bits drawn at creation) — never derived from keys or content, since clusters merge, split, and rename. **Ids identify; labels display and address** (re-scoped 2026-08-21): every act and page fetch keys on the id, and a label resolves exactly once, at the human boundary (send-by-name, CLI args) — duplicate labels error there instead of first-match-wins. Every contact entry belongs to a person from the moment it's added (label initialized from the petname; independent facts after). Per-device petnames stay underneath. Never on the wire. |
| Cross-device vocabulary = keys | Person entries never travel. A lens statement between my devices is an act about keys ("label the cluster containing K 'Alice'"), resolved locally by key overlap. Devices that cluster differently each apply acts correctly in their own world — no shared object exists, so nothing has to be consistent. |
| Addressing | Send-by-name resolves person label → all member keys; the name-collision check moves to person labels. This is the display-vs-addressing split parked in multi-device.md §7. |
| Person/device names on the wire | New claim kind `Claim::DeviceLabel(String)`; `Name` becomes the person name. Each supersedes independently per claim kind. Drift across devices renders honestly (revision + agreement). SPEC §3.2 proposal, appended-variant norm. |
| Relays render per device | Shown per device row, from that device's own record — never as a person-level list. S1 also settles whether a multi-key record's relays should apply to non-publishing keys at all (`send.rs` currently smears one relay set across all listed keys). |
| Lens toggle | *My view / what they claim / through friends* — the per-attester lens (web-of-trust.md §6): shows what a friend chose to publish, never their private petnames; display-only, addressing stays mine. Friends' avatars = evaluating third-party `Avatar` claims in endorsements (no version bump). Lenses render from held data; each friend's lens offers an explicit scoped ask — `who_is_among(subject, [friend])` — with copy saying plainly that the friend learns you asked. |
| Queries from the page | A query reveals interest to whoever is asked (who-is-this.md §5) — that, not data integrity, is its cost (nothing is ever overwritten; stores append, resolution is read-time). So: a contact's page auto-runs the existing rate-limited **subject-refresh** on open (asking the subject about themselves reveals nothing new); third parties are asked only per explicit, per-friend act (the lens ask); a stranger's page queries no one on open — asking contacts broadcasts "X contacted me", asking the sender is a liveness receipt to a possible spammer. The ask-everyone who-is button keeps its one job: the stranger bootstrap. |
| Own-device lens sync | Lens edits are sealed ops in a conversation whose participants are my own devices. Existing machinery does the rest: deposits give offline convergence; send-to-self + sync + re-wrap bootstrap a new device. No protocol change — the encoding lives in the sealed body; relays see ciphertext. |
| Adoption policy | Sibling lens ops auto-adopt by default; manual edits always win; concurrent conflicts surface with provenance ("your phone said X, your laptop said Y"). Nothing arbitrates. |
| Contact adds are offers | A sibling's contact-add renders as an offer ("your phone added X — add them here too?"); only the explicit accept writes the contact store, so "the contact store is never modified by network input" holds verbatim. A compromised sibling can already read everything; the offer gates the write surface — the sealing/relay trust anchor. Repudiating a sibling voids its pending offers. |
| Inspectable | The lens conversation is ordinary DAG history — a built-in audit trail (which device did what, when). A viewer UI is deferred: hidden-by-default affordance, like the concurrency markers. |
| CLI | Person entries live in `zink-client` (addressing and lens ops need them), so the CLI gets them nearly free — but it grows only the thin commands e2e slices need. No cluster-management UX; addressing by key stays. |

## Constraints fixed by canon

- Recognize-as-device is never one tap: pair-back goes through the
  fingerprint confirm (multi-device.md §3).
- Cluster-first (ui-design-system.md §1) — nothing re-bakes
  one-key-one-person.
- A friend vouches a name, never someone else's device links
  (web-of-trust.md §6).
- Lens data never rides served records — it would leak the social graph;
  the own-devices conversation is the only carrier.

## Slices

- **S1 · SPEC + protocol — done (2026-08-20).** `Claim::DeviceLabel`; the
  multi-key-record relay question; resolution helper beside
  `self_claimed_name`; profile set/edit. *Done when:* SPEC §3.2/§11
  updated; unit tests — independent supersession, hostile claims drop;
  CLI set/show.
  *(As built: variant appended at v1, tags stable;
  `self_device_label_claim` mirrors the name helper; the label persists
  as `profile.device` with its own revision, set-if-changed like the
  name; CLI `my-record --device <label>`, label rendered by `who-is` and
  `devices`. The relay question resolved as "relays bind to the
  publishing device" (SPEC §3.6 + §11); structural enforcement rides S2's
  addressing rework — every record built today is single-key, so no live
  path smears. SPEC's header now states its living-document status.
  Prefill composition ("Mårten · laptop") is rendering — moved to S3 with
  the onboarding split.)*
- **S2 · Person entries — done (2026-08-20).** Local store, overlap
  resolution, label collision, send-by-name over clusters,
  `participant_labels` reads person entries. Migration: one person per
  existing contact entry; mutual-link evidence produces clustering
  offers, never silent merges. *Done when:* merge / split / rename dangle
  nothing; adversarial overlap still surfaces; a send to a two-device
  person reaches both keys.
  *(As built — id model reworked 2026-08-21 after review: `persons/<id>`
  files named by `PersonId` — a 128-bit uniqueness draw through the rng
  port's new `Mint` capability (`SystemRng` implements `Draw + Mint`;
  uniqueness tokens are scriptable in tests — distinctness is their whole
  contract — unlike crypto randomness, which stays `OsRng`-at-call-site
  per the port's footgun rule). The boundary form is typed —
  `person:<32 hex>`, so an id never reads as just another hex blob and
  stray hex refuses to parse; storage filenames stay bare `{:032x}`
  (fs-safe, no migration).
  **Eager one-person-per-entry** replaced the first-landed lazy /
  virtual-singleton read: two id kinds proved a bug attractor (an act
  could invalidate a held handle), so `add_contact` creates the person
  row (label = petname at add; independent facts after — an
  entry-petname rename no longer moves the label), `persons()`
  self-heals an unclaimed entry on sight (crash gap / pre-eager store),
  and open re-mints counter-era rows. Acts key on ids; `person_by_label`
  is the one human boundary and errors on duplicate labels.
  `split_person` refuses a split that would twin the source's label (the
  merge-then-split-the-namesake hole — regression-tested).
  `resolve_person` resolves label-then-petname, one `Contact` per member
  entry; `resolve_relays` gained the publishing-device guard and the
  send path binds a Contact's relays to its first key only — S1's
  deferred enforcement, landed (SPEC §11 row updated). Collision checks
  are one `ensure_label_free` across both namespaces, own-member
  shadowing exempt; `replace_contact` re-points person members on a stem
  move (the no-dangle rule). CLI: `persons` (prints ids), `person-merge`,
  `person-split`, `person-rename` — label or id; `send --to` / `reply
  --add` resolve persons. Clustering offers needed no new seam —
  `device_evidence` (D3c) stays the evidence, `merge_persons` is the
  accept. The person-level avatar override is rendering and rides S3.
  multi-device.md §7 updated — the display-vs-addressing bullet cashed
  in; the paired-write / crash-gap contracts are noted as project 9
  fault-double material.)*
- **S3 · The page.** Header (avatar, label, message / vouch — or add /
  ignore / who-is for a stranger), the lens switcher, device rows (label,
  self-claim, link tier with direction, disavowal warnings, that device's
  relays + freshness, fingerprint at trust moments, pair-back); page-open
  subject-refresh for contacts; the per-friend lens ask. *Done when:*
  live — a stranger's request previews and add completes from the page
  with no query fired on open; a two-device contact shows distinct labels
  and relays; asking one friend dials only that friend.
  *(2026-08-21: code complete — awaiting the live run. One `PersonView`
  for both variants (`View::Person{label}` / `View::Key{key}`; a member
  key lands on its person's page, own keys point at Me); `person_page` /
  `key_page` render local stores only. Client grew `refresh_contact`
  (page-open subject-refresh, sharing R6's ledger — stranger pages are a
  structural no-op), `friend_views` (per-responder lens data, voiding
  applied), `relay_resolution` (bare-key route + provenance), and
  `claims_to_be_my_device` (the pair-back offer: verified self-link to an
  own key, negatives void it; unit-tested with forgeries). The app's data
  layer went person-shaped: `AppState.contacts` rows come from
  `persons()` (the picker and People list render clusters), sends resolve
  via `resolve_person`, the members panel excludes person labels. The
  per-friend ask is `who_is_among` scoped to one friend per member key,
  copy stating they'll know. Pair-back routes through `inspect_record`'s
  fingerprint confirm — never one tap. Request rows carry
  `stranger_key` + a "who is this?" preview button. Me + onboarding
  gained the device field (S1's prefill split); vouch copy now names the
  exact string it shares. Follow-up noted: vouching publishes the entry
  petname — publishing the person label instead is a small client change,
  with S5. Verified: workspace tests green, clippy clean, UI wasm +
  src-tauri (desktop shell) compile, both bundles build.)*
  *(2026-08-21, with the id rework: the page and every act key on the
  person id — `View::Person { id }`, `ContactRow.id`, `PersonInfo.id`,
  merge picker options are `PersonRef { id, label }`; the R3 stuck-cue
  resolves its 1:1 label to the person row's id. Two pre-live-run fixes:
  the Friends-lens "they'll know you asked" disclosure is now reactive
  (it only rendered after a reload — unreadable before the first ask),
  and `set_profile` gained `rename_all = "snake_case"` (tauri v2
  camelCases invoke args by default, so the webview's `device_label`
  silently arrived `None`). Re-verified: workspace green, clippy clean,
  both wasm bundles + desktop shell build.)*
- **S3b · The subject ask — done (2026-08-21).** Live-run finding: in the
  two-fresh-devices bootstrap (Alice scans Bob's QR, messages him), Bob
  had *no* path to Alice's record — the scoped auto-query is gated on a
  contributing contact, the arrival refresh is contacts-only, the page
  fires nothing on open, and ask-everyone with zero contacts asks nobody;
  `add_contact` needs a self-signed record, so no affordance could render.
  The missing rung is an explicit act: `ask_subject` — `WhoIs(subject)`
  asked *of the subject*, bare dial-by-key first, learned route as
  fallback, answer saved subject-served via the shared `refresh_on`. No
  rate limit (a tap is never silently swallowed); the cost is the dial —
  a liveness receipt — so the stranger page's button copy owns it ("ask
  them who they are — they'll know you asked") and no automatic path ever
  calls it (who-is-this.md §5 records the stance). Three outcomes worded
  distinctly: answered / reached-but-nothing (declining and not-holding
  indistinguishable) / unreachable. The existing candidate rows + add
  button complete the flow unchanged. *Done when:* on a fresh device,
  request → page → ask → add completes; a responder that hasn't added the
  asker yields nothing learned; a hostile answer not naming the subject
  is dropped. *(As built: covered by unit tests for all four paths —
  answer/add, gate decline, unreachable, forged record; workspace green,
  clippy clean, UI wasm + desktop shell build.)*
- **S4 · Tappable surfaces — done (2026-08-21).** Sender lines, member rows,
  wild-key rows navigate to the page; the wild-key panel shrinks to a link;
  the R3 stuck-cue tap target resolves from membership. *Done when:* every
  rendered identifier navigates.
  *(As built: navigation is by key — `key_page` already resolves a member
  key to its person's page, so one `open_key` callback serves every
  surface; ChatView's `open_person` is gone. `participant_rows` joined the
  client (`participant_labels` delegates): the person-deduped label plus
  the cluster's first key as the row's handle — the members panel renders
  `MemberRow { label, key }`, the merged "you" row keyless (an own
  cluster, not one identifier; Me is its page). The wild-key row is now
  surface-and-link only — the in-chat who-is machinery (report panel,
  add-candidate, ignore) is deleted with `UnknownMember` trimmed to
  `{ key, dismissed }`; the page is a strict superset. The stuck cue
  resolves when every non-own member row shares one person label
  (collision-unique ⇒ one person) — this also fixes the cue silently
  breaking on locally-named chats, which the old label==petname match
  did. Non-goal kept: no nav stack — back from a page goes to Chats/People
  as before. ui-design-system.md §3 records the rule. Verified: workspace
  green, clippy clean, UI wasm + desktop shell build.)*
- **S5 · Lens avatars — done (2026-08-23).** Third-party `Avatar` claims in
  endorsements + per-friend rendering ("as Bob tells you"). *Done when:* a
  friend's vouched avatar renders under *through friends*, never replacing
  my override.
  *(As built: what a friend shares is **their own local-override photo of
  the subject** — never a relayed self-claim (hop 1 stays structural), no
  new wire shape (`Claim::Avatar` with attester = friend, endorsement
  validation was already kind-agnostic — no version bump, as scoped).
  `share_avatar` seals the override fresh-keyed (§8 key-in-claim), stores
  the attestation as a sibling slot beside the name vouch
  (`vouches/<subject>.avatar` — kinds supersede independently, like the
  SPEC §3.2 self-claims), pushes ciphertext to own home relays; the
  startup `push_avatar` re-push covers shared blobs too. Lifecycle: a new
  override re-shares at the next revision (the toggle must not lie);
  clearing the override withdraws the share; `repudiate` withdraws it and
  out-revisions it, so learned copies void under the §4 rule —
  `shared_avatar_claim` applies the same voiding the vouched name uses.
  Requester: `friend_views` marks sharing friends; `shared_avatar`
  fetches from the *friend's* resolved relays (the claim rode their
  answer), verifies hash + AEAD, caches. The page: a share toggle beside
  the photo controls (contacts with an override only; strangers never
  offer it), friend rows render the photo lazily — only inside the
  through-friends lens, never the page face (`avatar` resolution is
  untouched, pinned by test). Verified: 125 client tests green (share/
  serve/fetch e2e over loopback, lifecycle, voiding incl. the
  hostile stale-combo), clippy clean, UI wasm + desktop shell build.
  web-of-trust.md §3/§6 + who-is-this.md §8 updated.)*
- **S6 · Own-device lens sync — done (2026-08-23).** Op vocabulary (one
  carrier format, designed here, reused by project 8), the self
  conversation, adoption + offers, conflict surfacing, chat-surface
  suppression. Short design doc just-in-time. *Done when:* paired devices
  converge on a rename made while one was offline; a contact add is
  offer-then-accept; a repudiated sibling's offers are voided.
  *(As built — the design is `docs/design/lens-sync.md`; highlights: an
  **op frame** is a sealed body `zop\0` + borsh-encoded versioned enum
  (`Hello` / `LabelPerson{keys,label}` / `OfferContact{record,petname}`)
  — client vocabulary in `zink-client` (borsh added: already in-tree via
  the protocol crate), never zink-protocol's; project 8 appends its
  variants to the same frame. The **channel** is an ordinary conversation
  (`Hello` genesis, no human recipients) riding the existing
  send-to-self / deposits / backfill / re-wrap machinery — emission is
  stage-only (sync; delivery via the normal outbox flush), several
  channels tolerated with a smallest-id emission tiebreak. **Adoption**
  is a store-driven idempotent replay (applied-ops ledger; unopenable
  bodies retry after re-wrap — the new-device bootstrap) run after every
  drain and re-wrap: renames auto-adopt iff every op I authored about
  that person is a DAG ancestor of the sibling's (manual edits win);
  concurrent or colliding labels surface as conflicts with provenance
  ("your phone calls them X" + use-theirs on the person page), cleared by
  any rename. **Offers**: `add_contact` emits; siblings store latest per
  subject (already-held records drop — this also breaks the
  accept→re-offer loop); the People view renders "your phone added X —
  add them here too?"; accept IS `add_contact` — the contact store is
  never modified by network input; repudiation voids the sibling's
  offers, and the read path also filters un-recognized authors.
  **Suppression**: lens channels leave the inbox in
  `client.conversations()` (CLI + app inherit); op frames render as
  nothing in chat, previews, and notifications (own-device senders were
  already silent, covering every honest op). Effects are author-gated to
  recognized own devices — never parsed into effect from anyone else.
  Verified: 130 client tests green — offline-rename convergence,
  concurrent-rename conflict + take-theirs, offer-accept e2e with the
  loop-break roundtrip, repudiation voiding, hostile mimicry (non-sibling
  Hello + rename: no effect, no classification); clippy clean, UI wasm +
  desktop shell build.)*

## Open questions

*(All resolved with S6 — kept for the record.)*

- ~~The carrier format is shared with project 8~~ → designed once in
  lens-sync.md §2 (the op frame); project 8's scoping session owns the
  SPEC §11 body-encoding proposal, with that doc as its input.
- ~~How the lens conversation stays out of the chat surface~~ →
  client-side: channels filtered from `conversations()` (CLI + app
  inherit), op frames render as nothing, notifications were already
  silent for own-device senders (lens-sync.md §7).
- ~~Do contact-add offers batch?~~ → deferred, presentation-only
  (lens-sync.md §8), alongside the audit-trail viewer.

## Non-goals

- No person object on the wire — a person stays a local lens over keys.
- No enforced or assumed cross-device consistency — divergence is
  legitimate; convergence is best-effort.
- No auto-adopted contact-store writes, from any source.
- No gossip plane — the conversation is the carrier.

## Doc touchpoints as slices land

- SPEC §3.2 + §11: `DeviceLabel`, multi-key relay resolution (S1). ✅
- who-is-this.md §5: the page-open subject-refresh joins the carve-out
  list; the per-friend scoped ask noted beside the manual button (S3);
  the subject ask recorded as an explicit act (S3b). ✅ 2026-08-21
- multi-device.md §7: the display-vs-addressing separation cashes in
  (S2). ✅ 2026-08-21
- ui-design-system.md §1/§3: person entries in the view-model; the page
  IA (S3/S4). §3: every identifier navigates ✅ 2026-08-21 (S4).
- web-of-trust.md §6: the avatar lens lands (S5); §3 serve side names the
  share; who-is-this.md §8 notes third-party distribution. ✅ 2026-08-23
- New lens-sync design doc (S6), cross-referenced from project 8.
  ✅ 2026-08-23 — `docs/design/lens-sync.md`; project 8's tracker points
  at it for the shared carrier.
