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
| Person entry (local) | `{ local id, person label, avatar override, member contact entries }`. The id is an opaque local row id — never derived from keys, since clusters merge, split, and rename. Per-device petnames stay underneath. Never on the wire. |
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
- **S2 · Person entries.** Local store, overlap resolution, label
  collision, send-by-name over clusters, `participant_labels` reads
  person entries. Migration: one person per existing contact entry;
  mutual-link evidence produces clustering offers, never silent merges.
  *Done when:* merge / split / rename dangle nothing; adversarial overlap
  still surfaces; a send to a two-device person reaches both keys.
- **S3 · The page.** Header (avatar, label, message / vouch — or add /
  ignore / who-is for a stranger), the lens switcher, device rows (label,
  self-claim, link tier with direction, disavowal warnings, that device's
  relays + freshness, fingerprint at trust moments, pair-back); page-open
  subject-refresh for contacts; the per-friend lens ask. *Done when:*
  live — a stranger's request previews and add completes from the page
  with no query fired on open; a two-device contact shows distinct labels
  and relays; asking one friend dials only that friend.
- **S4 · Tappable surfaces.** Sender lines, member rows, wild-key rows
  navigate to the page; the wild-key panel shrinks to a link; the R3
  stuck-cue tap target resolves from membership. *Done when:* every
  rendered identifier navigates.
- **S5 · Lens avatars.** Third-party `Avatar` claims in endorsements +
  per-friend rendering ("as Bob tells you"). *Done when:* a friend's
  vouched avatar renders under *through friends*, never replacing my
  override.
- **S6 · Own-device lens sync.** Op vocabulary (one carrier format,
  designed here, reused by project 8), the self conversation, adoption +
  offers, conflict surfacing, chat-surface suppression. Short design doc
  just-in-time. *Done when:* paired devices converge on a rename made
  while one was offline; a contact add is offer-then-accept; a repudiated
  sibling's offers are voided.

## Open questions

- The carrier format is shared with project 8 (shared conversation
  names) — design it once, sequence the two proposals.
- How the lens conversation stays out of the chat surface (list and
  notification rendering — local policy; pushes carry no content anyway).
- Do contact-add offers batch ("your phone added 3 contacts")?

## Non-goals

- No person object on the wire — a person stays a local lens over keys.
- No enforced or assumed cross-device consistency — divergence is
  legitimate; convergence is best-effort.
- No auto-adopted contact-store writes, from any source.
- No gossip plane — the conversation is the carrier.

## Doc touchpoints as slices land

- SPEC §3.2 + §11: `DeviceLabel`, multi-key relay resolution (S1).
- who-is-this.md §5: the page-open subject-refresh joins the carve-out
  list; the per-friend scoped ask noted beside the manual button (S3).
- multi-device.md §7: the display-vs-addressing separation cashes in (S2).
- ui-design-system.md §1/§3: person entries in the view-model; the page
  IA (S3/S4).
- web-of-trust.md §6: the avatar lens lands (S5).
- New lens-sync design doc (S6), cross-referenced from project 8.
