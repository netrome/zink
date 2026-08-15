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
each runnable before the next. This project is **app-layer only** —
`app/ui` + `app/dto` + `app/src-tauri` command code. No wire or
`zink-protocol` change; `zink-client` is touched at most by S6 (read markers
— storage decided in-slice, local-only either way). No new dependencies.

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
- The Person view has no "message them": the natural gesture (I'm looking at
  Bob → chat with Bob) routes Chats → + → find Bob again in the checkbox
  wall. Meanwhile `send_draft` (`send.rs`) already threads a repeated
  participant set into its existing conversation — re-picking the same
  people is safe, and nothing in the UI says so.

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
— becomes one recognizable, thumb-sized interaction; membership becomes
visible; and the everyday chat loop (open → read → reply) stops fighting the
user. Concretely: a shared picker component, a members panel, scroll
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
| U4 | Person view has no path to a conversation | `person.rs` |
| U5 | `send_message` already accepts `add: Vec<String>` (batch is one signed message); the UI sends singletons. No `membership` command exists (client has `history.rs::membership`) | `src-tauri/lib.rs::send_message` |
| U6 | Armed repudiate/compromise buttons stay armed forever | `person.rs`, `me.rs` |
| U7 | Status flash persists indefinitely; scan-cancel flashes an error | `lib.rs::flash`, both scan sites |
| U8 | Remount-on-tab + prefill `Effect` destroy in-progress edits | `lib.rs` view match, `me.rs` |
| U9 | No in-flight send guard; stale message flash on chat switch | `chats.rs`, `chat.rs`, `lib.rs::open_chat` |
| U10 | No scroll pinning, no back affordance, `hh:mm`-only timestamps | `chat.rs` |
| U11 | Chats rows: `{n} message(s)`, no time/preview/unread | `chats.rs`, `dto::Conversation` |

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
- **Positive-only delivery cues stand** (tenet 7). Nothing in this project
  adds ticks, read receipts sent to peers, or "undelivered" states. Local
  *unread* (S6) is my own device's read position — never transmitted.
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
  UI doc): the picker pattern joins §2/§3; §3's new-chat sentence ("pick
  people → chat opens with an empty composer") is reconciled with whatever
  S1 decides (§7) so doc and app stop disagreeing.
- **No ADR expected** — nothing here is cross-cutting architecture; revisit
  at close if S6's read-marker storage lands in `zink-client`.
- **Projects README** row updated at close.

## 6. Slices

**DoD (every slice):** builds · `cargo fmt` + `clippy --all-targets` clean ·
suite green, floor held · `app/ui/build.sh` clean (wasm32 additionally if
`zink-client` is touched) · tracker updated · graduations per §5.

**Tier 1 — the named pain**

- [ ] **S1 · The people-picker.** One shared component: full-width rows
  (avatar + petname), tap-anywhere toggle, ≥ `--tap` targets, selected
  people as chips above the list, filter box (appears past ~8 contacts),
  alphabetical order, real empty state pointing at People. New-chat composer
  adopts it. Person view gains **"message"** — jumps to the existing
  conversation for that participant set, else opens the composer
  pre-selected (`send_draft` reuse makes both the same act; say so in the
  copy: "continues your existing chat"). Decide the new-chat flow shape
  (§7) in this slice. *Done when:* U1 and U4 are dead.
- [ ] **S2 · The members panel.** New `membership` command (labels via the
  existing `history.rs::membership` + `participant_labels`). Chat header
  becomes tappable → panel: current members (unknown keys rendered
  honestly), add via the S1 picker filtered to non-members, one batched
  membership message (U5's `add` vec), header label re-derived after
  changes. The permanent `<select>` is deleted (U2 dies by deletion);
  "introduce my devices" and the crossed-messages toggle relocate into this
  panel — advanced affordances, one tap away instead of always-on. *Done
  when:* U2, U3, and the composer-row clutter are dead.

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
- [ ] **S6 · Unread + close.** A local read marker per conversation (storage
  decided in-slice: app-layer store vs `zink-client` state — local-only
  either way, never transmitted, §4) → unread badge on Chats rows and the
  Chats tab. Re-measure the floor, graduate per §5, README row, archive.
  *Done when:* an arrival marks its row until opened, and the tracker is
  closed out.

## 7. Decisions log

| Decision | Resolution |
|---|---|
| New-chat flow shape | **Open — decide in S1.** ui-design-system.md §3 says "pick people → chat opens with an empty composer"; the implementation requires the first message in the composer (a conversation exists only at genesis). Lean: adopt the doc — picker first, then ChatView in a *draft* state whose first send is the genesis (`send_message` with `to:` already does this); it reuses the chat screen and matches messenger intuition. Whichever way S1 lands, §5 reconciles the doc. |
| Picker identity handle | Petnames at the app boundary, as today — the command layer resolves names (dto's stated contract); the picker is my-lens presentation. Keys stay out of the webview except at trust moments. |
| Existing-set reuse | Surfaced, not changed: `send_draft` already threads a repeated participant set into its conversation. S1 adds the copy; no client change. |
| Batch add | One `send_message` with the full `add` vec — one signed membership message. Per-person messages (today's shape) die with the `<select>`. (S2) |
| Advanced-affordance home | The S2 members panel. Constraint: one tap away, never buried two levels — crossed-cues and introduce-devices are legitimate features, just not always-on. |
| Snippet in rows | Client-side policy over the local plaintext store; fine by the invariants (nothing new through relays/pushes). Truncation/emptiness shape decided in S5. |
| Read-marker storage | **Open — decide in S6.** App-layer store vs `zink-client` state. Constraints fixed now: local-only, never transmitted, never a protocol concept; if it lands in `zink-client`, wasm32 must stay clean and an ADR is considered at close (§5). |
| Edit-survival fix (U8) | **Open — decide in S4.** Lift sub-state to parent signals vs keep views mounted and toggle visibility. Constraint: smallest change that preserves edits; no router rewrite (§4). |

## 8. Follow-ups / parked

- **Raw relay dial-string field** in Me/onboarding (the "shown cold" field
  ui-design-system.md §3 warns about) — fixed properly by project 5's R4
  (relay QR + prefix-routed scan). Not touched here to avoid two projects
  reshaping one field; if R4 closes first, a rebase picks it up.
- **Send-state rendering** ("sending…" → sent/delivered/stuck) — project 5's
  R3, on the same `chat.rs` surface as S3. Rebase order per §4.
- **Local conversation names** (user-set labels replacing the joined-petname
  label) — legitimate client-side policy, out of scope; revisit on a
  concrete need, alongside ui-design-system.md §1's parked arbitrary-grouping
  note.
- **Message search** — parked; needs a local index design, nothing protocol.
- **Notification → deep-link into the chat** (tap a notification, land in
  that conversation) — worth doing once S6's read markers exist; parked to
  keep S6 closeable.
