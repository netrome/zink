# Profile pages: one person page over a key cluster

> **Status: 🚧 in progress — scoped 2026-08-20.** Supersedes the "identity
> preview" proposal (same folder, renamed): its page + tappable-surface work
> is absorbed as S3/S4, and its trigger stands — a phone paired one-way to a
> laptop; the laptop receives fine but offers no navigable way to inspect
> the sender or act on it (the wild-key panel is inline-in-chat and
> transient). Branch: `profile-page`.

## TL;DR

A **person page** for contacts and strangers alike: one page keyed by the
observer's local cluster lens, rendering the three layers of belief
(ui-design-system.md §1) with per-device detail, reachable by tapping any
identifier anywhere. Around it, the model work that makes it honest:
a **person/device naming split** (self-claims stop overloading "mårten
laptop" into one string), **person entries** (the local label over a
cluster that "write a message to Alice" resolves), a **per-friend lens
toggle**, and — as the tail — **own-device lens sync**, so the clusters on
my phone are the clusters on my laptop *by default, never by assumption*.

## Decisions (scoped 2026-08-20)

| Decision | Resolution |
|---|---|
| One page, cluster-keyed | Generalize the Person view; a stranger is a degenerate one-key cluster (no petname; rendered from the learned store, with add-as-contact / ignore / manual who-is). Resolves the original proposal's KeyView-vs-PersonView question: they are one page. |
| Person entry (local) | `{ local id, person label, avatar override, member contact entries }`. The id is an **opaque local row id**, never derived from member keys — clusters merge, split, and rename, and a content-derived id re-homes silently (the `keys.first()` trap). Entries reference the existing per-device contact entries; per-device petnames stay underneath. Many-to-many latitude (multi-device.md §7) stays open. Never on the wire. |
| Cross-device vocabulary = keys | Person entries never travel. Any cross-device lens statement is an **act about keys** ("set label 'Alice' on the cluster containing K"), resolved onto local entries by key overlap — the multi-device.md §4 rule again. Devices with divergent clusterings each apply acts correctly in their own world: no shared cluster object exists, so there is nothing to keep consistent — only advisory acts each device interprets locally. |
| Addressing | Send-by-name resolves **person label → union of member entries' keys**; the petname-collision check moves to person labels. This cashes in multi-device.md §7's parked display-vs-addressing separation. Delivery completeness still rests on the owner's send-to-self, as ever — addressing any member key suffices. |
| Person/device names on the wire | SPEC §3.2 proposal: new appended claim kind **`Claim::DeviceLabel(String)`** — self-claimed `Name` becomes the person name ("Mårten"), `DeviceLabel` the qualifier ("laptop"). Supersession already scopes per claim kind, so they bump independently. Person-name drift across devices (profiles deliberately don't sync) renders honestly by revision + agreement. Pre-deployment appended-variant norm; onboarding asks two calm questions (your name / this device); pairing prefills the person name from the sibling's claim, the device label stays fresh. |
| Relays render per device | The page shows relays **per device row**, sourced from that device's own record — never a person-level relay list. SPEC scoping question rides S1: whether a multi-key record's relay set should apply to non-publishing keys at all (`send.rs` currently maps `(key, record.relays)` for every listed key — advisory robustness by design, but it is the one spot that smears one relay set across a key set). |
| Lens toggle | *My view / what they claim / through friends* — the per-attester lens web-of-trust.md §6 parked, with its boundaries intact: a lens shows what a friend **tells** you (their published vouches), never their private petnames; lenses are display-only — addressing always resolves through my store. Friends' **avatars** = evaluating third-party `Avatar` claims in endorsements — an evaluation change, not a version bump (web-of-trust.md §1). |
| Own-device lens sync | Lens edits are **sealed ops riding a conversation whose participants are my own devices** — project 7's carrier decision turned inward. Deposits/drain/DAG-heal give offline convergence (phone edits at noon, laptop converges at night); send-to-self + skeleton sync + `GetKeys` re-wrap give new-device bootstrap with zero new mechanism. No protocol change: the op encoding lives inside the sealed body; relays see ciphertext. |
| Adoption policy | Lens ops (labels, clusters, avatar overrides) **auto-adopt from siblings by default**; manual local edits always win; concurrent conflicts **surface with provenance** ("your phone renamed this to X while your laptop said Y" — causal supersession per attesting device, latest-received as the default face). Cross-device label collisions surface as a rename offer; nothing arbitrates. Convergence is a default, never an assumption. |
| Contact adds never auto-adopt | A sibling's contact-add travels as **evidence with provenance** and renders as an offer ("your phone added X — add them here too?"); the explicit accept is the one act that writes the contact store. **"The contact store is never modified by network input" survives verbatim** — the D3 offer pattern pointed at a new evidence source. Threat model: a compromised sibling already reads everything (the honest meaning of pairing, multi-device.md §8), so lens sync adds no read surface; the offer-gate protects the *write* surface — the sealing-key/relay trust anchor. Repudiating a sibling voids its pending offers. |
| Inspectability | The lens conversation is ordinary DAG history — the audit trail exists from day one (every act renders with which device signed it, when). The viewing affordance is deferred: advanced-affordance pattern, hidden by default, one tap away (like the concurrency markers). |

## Constraints fixed by canon (carried over, extended)

- **Who-is stays a manual button** (who-is-this.md §5): opening the page
  never auto-queries anyone — rendering is local; asking broadcasts interest.
- **Recognize-as-device is never one tap**: pair-back routes through the
  existing pair-preview fingerprint confirm (multi-device.md §3).
- **Cluster-first** (ui-design-system.md §1): the page renders a key-set
  lens; nothing re-bakes one-key-one-person.
- **No third-party device links** — a friend vouches a name, never someone
  else's cluster (web-of-trust.md §6). Structurally impossible; not offered.
- **Lens data never enters served records** — cluster beliefs and labels
  would leak the social graph to whoever pulls a record; the own-devices
  conversation is the only carrier.

## Slices

- **S1 · SPEC proposal + protocol.** `Claim::DeviceLabel` (appended
  variant); resolve the multi-key-record relay question in SPEC §3.6/§11;
  protocol resolution helper beside `self_claimed_name`; profile gains the
  device label (set/edit, revision per claim kind); pairing/onboarding
  prefill split. *Done when:* SPEC §3.2/§11 updated; unit tests — label and
  name supersede independently, hostile claims verify-or-drop as ever; CLI
  set/show.
- **S2 · Person entries.** The local store + overlap resolution; label
  collision moves to person labels; send-by-name resolves clusters;
  `participant_labels` reads person entries (one label per person, device
  qualifier available). Migration default: one person per existing contact
  entry; mutual-link evidence produces clustering **offers**, never silent
  merges (the D3a tiers, reused). *Done when:* unit tests — merge / split /
  rename dangle nothing, adversarial overlap still surfaces instead of
  merging; a send to a two-device person fans out to the member keys.
- **S3 · The person page.** Header (avatar override-or-resolved, person
  label, message / vouch; stranger: add / ignore / who-is), the lens
  switcher, the devices section — per-device label, self-claim, link tier
  with directionality, disavowal warnings, **that device's relays** and
  record freshness, fingerprint at trust moments, pair-back via Me's
  confirm. *Done when:* live — a stranger's message request previews to the
  page and add-as-contact completes from it; a two-device contact renders
  distinct labels and relay sets.
- **S4 · Tappable-surface sweep.** Sender lines, member-panel rows,
  wild-key rows navigate to the page; the wild-key panel shrinks to a link;
  the R3 stuck-cue tap target resolves from membership instead of the
  frozen label (the project 5/6 rebase note). *Done when:* every rendered
  identifier navigates; no dead identifiers remain.
- **S5 · Lens avatars.** Third-party `Avatar` claim evaluation in
  endorsements + the per-friend lens rendering ("as Bob tells you", with
  the according-to-Bob marker). *Done when:* a friend's vouched avatar
  renders under *through friends*, never replacing my override.
- **S6 · Own-device lens sync.** Op vocabulary (coordinated with project
  7's body-encoding proposal — one "advisory claims riding a conversation"
  format, pointed at friends there and at siblings here), the self
  conversation, adoption policy + contact-add offers, conflict surfacing,
  chat-surface suppression for op-only conversations. Genuine unresolved
  design → short `docs/design/` doc just-in-time. *Done when:* two paired
  devices converge on a rename/cluster performed while one was offline; a
  contact add renders as an offer and only the accept writes the store; a
  repudiated sibling's pending offers are voided.

## Open questions

- The op body encoding shared with project 7 — sequence the two proposals
  so the carrier format is designed once.
- Chat-surface policy for the lens conversation: hidden from the Chats
  list; notification suppression for op-only deposits (no content in
  pushes already — this is about the local "new message" rendering).
- Do contact-add offers batch ("your phone added 3 contacts")?
- Does the CLI grow person entries, or app-only at first?

## Non-goals

- **No person record or person id on the wire** — a person stays a local
  lens over keys; the page renders a cluster, it never reifies one.
- **No enforced or assumed cross-device consistency** — divergent lenses
  are two legitimate beliefs; convergence is best-effort and honest.
- **No auto-adopted contact-store writes**, from any source.
- **No gossip plane** — the conversation is the carrier (the project 7
  stance); lens data never rides served records or `WhoIs` answers.
- No group-crypto, capability, or recovery scope creep (deferred list
  unchanged).

## Doc touchpoints as slices land

- SPEC §3.2 + §11: `DeviceLabel`, the multi-key relay resolution (S1).
- multi-device.md §7: the parked display-vs-addressing separation cashes
  in — point it here (S2).
- ui-design-system.md §1/§3: the Person view-model gains person entries;
  People/detail IA becomes the person page (S3/S4).
- web-of-trust.md §6: the per-attester avatar lens lands (S5).
- New design doc for lens sync, just-in-time (S6); cross-reference from
  project 7's carrier proposal.
- projects/README.md: row updated at scoping (done).
