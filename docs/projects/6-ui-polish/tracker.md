# UI polish: people-picking, membership, and everyday ergonomics

> **Status: 📋 scoped (2026-08-15), not started.** Project **6-ui-polish**,
> branch `better-selections`. Trigger: daily use — selecting people for a new
> chat is a wall of native checkboxes, adding someone to a running chat is a
> permanently visible dropdown with a stuck value, and a dozen smaller edges
> (no scroll pinning, armed-forever danger buttons, a status line that never
> clears) add friction everywhere. This is the second UX pass; the first
> ([2-ui-facelift](../2-ui-facelift/tracker.md)) set the design system and IA,
> this one makes the flows *work well*, within that system.
>
> **Runs concurrently with [5-relay-lifecycle](../5-relay-lifecycle/tracker.md)**
> (separate branches). The slice content is disjoint, but two surfaces are
> shared — `app/ui/src/chat.rs` (5's R3 renders send states there) and the
> Person view (5's R5 adds a relay panel) — so minor **rebase conflicts are
> expected and fine**; they should be textual, not semantic. Division of
> labor: send-state wording and relay UI belong to project 5; this project
> does not touch them (§4, §8).

Governed by the standard slice discipline (AGENTS.md): small vertical slices,
each runnable before the next. This project is **app-layer plus two narrow
`zink-client` policy seams**: an explicit fresh-genesis send entry (S1, §7)
and local read-marker / conversation-label storage if it lands client-side
(S6/S7, decided in-slice). No wire or `zink-protocol` change either way. No
new dependencies.

## 1. Why — the audit

A read-through of all nine `app/ui/src` modules against real use
(2026-08-15). The theme: the facelift got the *architecture* of the screens
right (three tabs, three belief layers, "+" sub-flows) and stopped at the
first working control for each flow. The controls are now the friction.

**Selecting people — the named pain.**

- The new-chat composer (`chats.rs`) renders contacts as a flat run of
  native checkboxes: no avatars (the People list has them — recognition is
  slower exactly where it matters), no search, no guaranteed order, no empty
  state (zero contacts renders `with:` followed by nothing), and tap targets
  well under the design system's own `--tap: 44px`.
- Adding to a running conversation (`chat.rs`) is a permanent `<select>` in
  every chat's composer. It lists all contacts *including current members*,
  commits one empty membership message per add — and the `<select>` has no
  `prop:value` binding, so after a successful add it still *displays* the
  picked name while the state is cleared: the next "add" tap silently no-ops.
- There is no membership surface at all — nothing shows who is in a
  conversation — and the chat header label is frozen at open
  (`lib.rs::View::Chat` carries it), so it goes stale the moment someone is
  added.
- The Person view has no path from a person to their conversations: the
  natural gesture (I'm looking at Bob → our chats) routes Chats → + → find
  Bob again in the checkbox wall, and nothing anywhere lists the
  conversations a person is in.
- A model conflict, surfaced by this audit: conversations are
  **genesis-identified** — several with the same people is a *feature* (the
  Slack-channel shape), and the receive path already supports it (a peer's
  two genesis messages are two conversations in the store). But `send_draft`
  (`send.rs`) threads any send-by-recipients into the participant-set
  index's conversation, so the app cannot locally start a second
  conversation with people you already chat with — and the one-slot index
  resolves a set to whichever conversation it happens to hold. The code
  comment already calls the index "`send`'s policy"; it is policy, and the
  wrong policy for the app's new-chat act.

**Everyday ergonomics, in descending pain:**

- The message list has no scroll management: opening a long chat lands at
  the *oldest* message; new arrivals don't scroll into view.
- A chat has no back affordance — the only exit is the Chats tab (the Person
  view has "‹ people"; the chat deserves the same).
- Timestamps are `hh:mm` only — last Tuesday reads like today.
- The armed danger buttons ("this key isn't them anymore", "mark
  compromised") never disarm: no timeout, no tap-away. An accidental tap
  minutes later publishes a repudiation.
- The status line persists until the next action overwrites it; a cancelled
  QR scan flashes a red ❌ (both scan sites carry a comment acknowledging
  it); errors render at the top while the thumb is at the bottom.
- Tab switches remount views and re-run `load_state`, and the Me prefill
  `Effect` re-fires on every state change — half-typed edits and half-picked
  selections are silently destroyed by a stray tab tap.
- No in-flight guard on either send path: a double-tap sends twice. Opening
  chat B renders chat A's messages under B's header until the fetch lands.
- Chats rows say `{n} message(s)` — noise ("1 message(s)") where a
  last-message time (already in the DTO) and snippet would serve. Nothing
  anywhere indicates unread.
- Advanced, rare affordances occupy prime space in every chat: the
  crossed-messages toggle is the first element, "introduce my devices" sits
  permanently in the composer row, and the attach control is a raw
  `<input type="file">`.

## 2. Goal & non-goals

**Goal.** Picking people — for a new chat, for a running chat, from a person
— becomes one recognizable, thumb-sized interaction; membership and a
person's conversations become visible; conversations with the same people
stay **distinct** (by genesis) and **findable** (by person); and the
everyday chat loop (open → read → reply) stops fighting the user. Concretely: a shared picker component, a members panel, scroll
pinning, honest timestamps, and the paper-cut batch — inside the existing
design system (`docs/design/ui-design-system.md`), not a new one.

**Non-goals.**
- No protocol, wire, or `zink-protocol` change; no new hashed fields, no new
  dependencies.
- No send-state / delivery wording changes and no relay UI — project 5 owns
  those surfaces (R3, R4, R5). The raw relay dial-string field in
  Me/onboarding is *that* project's fix (R4 scan path); parked here (§8).
- No new trust semantics: the picker selects existing contacts; adds remain
  grown recipient sets (groups.md §2 — the signed recipients list *is* the
  membership announcement). No membership consensus, no enforcement.
- No visual redesign, motion, or theming work — the facelift's tokens and IA
  stand.

## 3. Baseline (2026-08-15)

Shared with project 5: branched from the project-4 close — suite **239/239,
~1.1 s wall**, clippy clean, `wasm32` + `app/ui/build.sh` clean. That floor
holds for every slice here too.

Behavior baseline, reproducible today:

| # | Fact | Where |
|---|---|---|
| U1 | New-chat picker: native checkboxes, no search/avatars/order/empty-state, sub-`--tap` targets | `chats.rs` compose branch |
| U2 | Add-to-chat `<select>` lists current members, one message per add, displayed value never resets (no `prop:value`) → next "add" silently no-ops | `chat.rs` picks row |
| U3 | No membership view; header label frozen at open | `lib.rs::View::Chat`, `chat.rs` |
| U4 | Person view has no path to their conversations — no list, no new-conversation act | `person.rs` |
| U5 | `send_message` already accepts `add: Vec<String>` (batch is one signed message); the UI sends singletons. No `membership` command exists (client has `history.rs::membership`) | `src-tauri/lib.rs::send_message` |
| U6 | Armed repudiate/compromise buttons stay armed forever | `person.rs`, `me.rs` |
| U7 | Status flash persists indefinitely; scan-cancel flashes an error | `lib.rs::flash`, both scan sites |
| U8 | Remount-on-tab + prefill `Effect` destroy in-progress edits | `lib.rs` view match, `me.rs` |
| U9 | No in-flight send guard; stale message flash on chat switch | `chats.rs`, `chat.rs`, `lib.rs::open_chat` |
| U10 | No scroll pinning, no back affordance, `hh:mm`-only timestamps | `chat.rs` |
| U11 | Chats rows: `{n} message(s)`, no time/preview/unread | `chats.rs`, `dto::Conversation` |
| U12 | Send-by-recipients auto-threads into the participant-set index's conversation: a second conversation with the same set cannot be started from the app, and which conversation a repeated set resolves to is an accident of index writes | `send.rs::send_draft` |

Kept and built on (not rebuilt): the design tokens and three-tab IA, the
"+"-gated sub-flows, the wild-key / who-is surfaces, positive-only delivery
cues, the belief-layer Person view.

## 4. Guardrails — the traps to avoid

- **Policy stays client-side** (invariant: clients own policy/UX). Sorting,
  previews, read markers, membership *presentation* — all app-layer, never
  protocol, never `zink-protocol`. A last-message snippet reads the local
  plaintext store; nothing new transits a relay or a push.
- **Membership is presentation of signed recipient sets** (groups.md §2).
  The members panel renders what the DAG heads say — including unknown keys,
  honestly (the wild-key surface stays). Batch add = one message with the
  grown set; no add/remove semantics beyond what recipients-lists express.
- **Conversations never merge.** Identity is the genesis id; matching
  participant sets are never a reason to auto-route, collapse, or hide a
  conversation. Same-set listings (draft header, Person view) are discovery
  affordances — links, never silent redirection.
- **Positive-only delivery cues stand** (tenet 7). Nothing in this project
  adds ticks, read receipts sent to peers, or "undelivered" states. Local
  *unread* (S7) is my own device's read position — never transmitted.
- **Don't hide the model, translate it** (ui-design-system.md). The picker
  shows petnames + avatars (my lens); trust moments keep their fingerprints;
  disavowal warnings stay at the moment of decision.
- **Project-5 coexistence.** Do not touch send-state wording, `pending` /
  `confirmed` rendering, or relay fields — R3/R4/R5 own them. Rebase onto
  main after each project-5 merge rather than batching; conflicts in
  `chat.rs` / `person.rs` are expected to be adjacent-line noise. If a
  conflict turns *semantic* (both projects reshaping the same element), stop
  and re-sequence rather than merging blind.
- **No drive-by refactors.** The remount-on-navigation structure (U8) gets
  the smallest fix that preserves edits (state up, or keep-mounted — decide
  in-slice), not a router rewrite.

## 5. Graduation plan

- **`docs/design/ui-design-system.md`** — edited in place (it is the durable
  UI doc): the picker pattern joins §2/§3; §3's new-chat sentence is updated
  to the decided picker-first + draft flow (§7).
- **`docs/design/groups.md`** — one paragraph naming the stance §7 decides:
  conversation identity is the genesis id; several conversations per
  participant set is a feature; the participant-set index is send-by-name
  policy only, never conversation identity. Written at S1 close, cited from
  the fresh-genesis entry's doc comment.
- **No ADR expected** — nothing here is cross-cutting architecture; revisit
  at close if S7's read-marker storage lands in `zink-client`.
- **Projects README** row updated at close.

## 6. Slices

**DoD (every slice):** builds · `cargo fmt` + `clippy --all-targets` clean ·
suite green, floor held · `app/ui/build.sh` clean (wasm32 additionally if
`zink-client` is touched) · tracker updated · graduations per §5.

**Tier 1 — the named pain**

- [ ] **S1 · The people-picker + draft chat.** One shared picker component:
  full-width rows (avatar + petname), tap-anywhere toggle, ≥ `--tap`
  targets, selected people as chips above the list, filter box (appears
  past ~8 contacts), alphabetical order, real empty state pointing at
  People. New-chat flow: pick people → ChatView in a **draft** state — no
  id, empty history, a dim "your first message starts this conversation
  with alice, bob" line — whose first send *is* the genesis. A draft is
  **always a new conversation** (§7): `zink-client` gains an explicit
  fresh-genesis send entry (`stage_send`-shaped, skipping the
  participant-set lookup; name and shape in-slice, wasm32 stays clean) and
  the app's new-chat path uses it. Discovery, not coercion: when the picked
  set matches existing conversations, the draft lists them as links ("you
  already have 2 with these people — or send below to start another"),
  filtered app-side from summaries' membership. *Done when:* U1 and U12 are
  dead — including: a second conversation with the same people can be
  started from the app.
- [ ] **S2 · The members panel + a person's conversations.** New
  `membership` command (labels via the existing `history.rs::membership` +
  `participant_labels`), serving two surfaces. Chat header becomes tappable
  → panel: current members (unknown keys rendered honestly), add via the S1
  picker filtered to non-members, one batched membership message (U5's
  `add` vec), header label re-derived after changes. Person view gains
  **"conversations with them"** — the conversations whose membership
  intersects their keys, tappable — plus **"start a new conversation"**
  (opens an S1 draft pre-selected). The permanent `<select>` is deleted (U2
  dies by deletion); "introduce my devices" and the crossed-messages toggle
  relocate into the members panel — advanced affordances, one tap away
  instead of always-on. *Done when:* U2, U3, U4, and the composer-row
  clutter are dead.

**Tier 2 — the everyday loop**

- [ ] **S3 · Chat ergonomics.** Scroll pinning (bottom on open; follow
  arrivals when already at bottom — never yank a reader who scrolled up);
  "‹ chats" back affordance; day-aware timestamps (date separators or
  "tue 14:32" — decide in-slice); attach as a compact 📎 button replacing
  the raw file input; Enter-to-send on desktop (Shift+Enter newline),
  untouched on mobile. *Done when:* U10 is dead.
- [ ] **S4 · The paper-cut batch.** Independently small, one slice because
  each is a few lines: armed danger buttons disarm (timeout ~4 s or any
  other interaction); scan-cancel is silent (distinguish cancel from error
  at the two scan sites); status flashes auto-clear (ok after ~4 s; errors
  stay until replaced or dismissed); in-flight guards disable both send
  buttons while the invoke runs; `open_chat` clears the stale message list
  before switching; in-progress edits survive a tab bounce (U8 — smallest
  fix per §4). *Done when:* U6–U9 are dead.

**Tier 3 — the lists + close**

- [ ] **S5 · Rows that inform.** Chats rows: relative last-message time
  (DTO already carries `last_timestamp_ms`) + a one-line snippet (new DTO
  field off the local store — client-side policy, §4) replacing
  `{n} message(s)`. People rows: dim second line (self-claimed name when it
  differs from the petname), guaranteed alphabetical order. *Done when:*
  U11 is dead except unread.
- [ ] **S6 · Conversation names.** A local-only label per conversation — my
  lens for a conversation, the conversation-shaped sibling of a petname.
  Set/edit from the S2 members panel; display precedence local name >
  joined-petnames default everywhere a label renders (rows, chat header,
  notification titles via the command layer). Storage decided in-slice
  (same local-never-transmitted constraint class as S7's read markers), with
  one constraint fixed now: **my label is stored distinct from any future
  peer suggestion** — the anchor-vs-learned split the contact store already
  uses — so the shared-names follow-on (§8) slots underneath without
  migration. Why in scope: with several conversations per set first-class
  (S1), three rows all labelled "alice, bob" are indistinguishable — naming
  is the organizing tool. *Done when:* two same-set conversations are
  tellable apart at a glance in every list.
- [ ] **S7 · Unread + close.** A local read marker per conversation (storage
  decided in-slice: app-layer store vs `zink-client` state — local-only
  either way, never transmitted, §4) → unread badge on Chats rows and the
  Chats tab. Re-measure the floor, graduate per §5, README row, archive.
  *Done when:* an arrival marks its row until opened, and the tracker is
  closed out.

## 7. Decisions log

| Decision | Resolution |
|---|---|
| New-chat flow shape | **Decided (2026-08-15): picker-first + draft chat.** Pick people → ChatView in a draft state; the first send *is* the genesis, and a dim line says so ("your first message starts this conversation…") — the genesis stays a visibly special message without a second composer to maintain. One composer for everything (a first message can carry an image, which today's form cannot); an abandoned draft leaves no residue, because no genesis means no conversation. §5 updates ui-design-system.md §3 to match. |
| "New chat" means a new conversation | **Decided (2026-08-15), reversing the scoping lean.** Conversations are genesis-identified; several with the same people is a *feature* (the Slack-channel shape), and the receive path already supports it — only local initiation collapsed (U12). The client gains an explicit fresh-genesis entry which the app's draft flow uses; `send`'s participant-set auto-threading survives as send-by-name (CLI) policy, revisited only on need (§8). Discovery replaces coercion: existing same-set conversations are *listed* (draft header, Person view), never silently reused. |
| Person → conversations | A person's page shows the conversations they're in plus "start a new conversation" — a list, not a single "message" button, because plurality is the model (row above). (S2) |
| Picker identity handle | Petnames at the app boundary, as today — the command layer resolves names (dto's stated contract); the picker is my-lens presentation. Keys stay out of the webview except at trust moments. |
| Conversation names | Local-only label, never transmitted — client policy, the conversation-shaped sibling of a petname. Display precedence: local name > joined-petnames default. Storage decided in S6 under the same constraints as read markers. |
| Batch add | One `send_message` with the full `add` vec — one signed membership message. Per-person messages (today's shape) die with the `<select>`. (S2) |
| Advanced-affordance home | The S2 members panel. Constraint: one tap away, never buried two levels — crossed-cues and introduce-devices are legitimate features, just not always-on. |
| Snippet in rows | Client-side policy over the local plaintext store; fine by the invariants (nothing new through relays/pushes). Truncation/emptiness shape decided in S5. |
| Read-marker storage | **Open — decide in S7.** App-layer store vs `zink-client` state. Constraints fixed now: local-only, never transmitted, never a protocol concept; if it lands in `zink-client`, wasm32 must stay clean and an ADR is considered at close (§5). |
| Edit-survival fix (U8) | **Open — decide in S4.** Lift sub-state to parent signals vs keep views mounted and toggle visibility. Constraint: smallest change that preserves edits; no router rewrite (§4). |

## 8. Follow-ups / parked

- **Raw relay dial-string field** in Me/onboarding (the "shown cold" field
  ui-design-system.md §3 warns about) — fixed properly by project 5's R4
  (relay QR + prefix-routed scan). Not touched here to avoid two projects
  reshaping one field; if R4 closes first, a rebase picks it up.
- **Send-state rendering** ("sending…" → sent/delivered/stuck) — project 5's
  R3, on the same `chat.rs` surface as S3. Rebase order per §4.
- **Send-by-name auto-threading (CLI).** With several conversations per set
  first-class, the one-slot participant-set index is ambiguous by
  construction. The CLI's send-by-name keeps today's policy (thread into
  whatever the index holds) until real usage says otherwise; any change is
  client policy, not protocol. The fresh-genesis entry (S1) is the escape
  hatch either way.
- **Shared conversation names — a future project, not a slice here** (scoped
  in conversation, 2026-08-15). Letting participants see what you call a
  chat ("besties" vs "me and my boiz") is the person-naming model transposed
  to a new subject type (a genesis id instead of a key): my lens sovereign >
  peer suggestions rendered with provenance, sharing default-on but
  per-rename optional (a "annoying coworkers" label stays local), nothing
  auto-adopted without attribution — `Claim::Name`'s primitive-vs-policy
  line, walked again. Carrier: a name claim sealed *inside* the conversation
  (the membership precedent — participants only, late joiners via backfill,
  relays see ciphertext); **never in served records**, which would leak a
  conversation's existence to whoever pulls the record. Needs a SPEC §11
  proposal (body-encoding agreement at minimum) and a called-out
  renegotiation of the "naming never enters the protocol" invariant —
  exactly what this project's charter excludes. Prerequisite: S6's local
  lens, whose storage constraint keeps this seam open. Not a gossip plane
  (deferred list stands): the conversation DAG is the carrier.
- **Arbitrary local grouping of conversations** (folders/tags beyond a name)
  — S6 does names only; grouping waits for a concrete need, alongside
  ui-design-system.md §1's parked arbitrary-grouping note for people.
- **Message search** — parked; needs a local index design, nothing protocol.
- **Notification → deep-link into the chat** (tap a notification, land in
  that conversation) — worth doing once S7's read markers exist; parked to
  keep S7 closeable.
