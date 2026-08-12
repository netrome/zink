# UI facelift: coherent design & "don't make me think" UX

> **Status: ✅ complete & archived (2026-07-26).** Project **2-ui-facelift**. This
> is the narrative record of the first UX/visual pass over the app. The durable
> half — identity-render model, design tokens, IA, vocabulary — graduated to
> [`docs/design/ui-design-system.md`](../../design/ui-design-system.md); later UI
> work edits it there. What remains below is the goal, the baseline audit, the
> decisions log, and the slice tracker.

A dedicated design + task tracker for the first real UX/visual pass over the
Tauri/Leptos app. Ran **parallel to** [1-mvp](../1-mvp/build-plan.md)
(the MVP was functionally near-complete; this is polish, not a new capability),
governed by the same slice discipline: small vertical slices, one per turn,
each runnable and reviewed before the next.

**This pass is UI-only.** No protocol changes, no `zink-protocol` touch, no new
Rust dependencies. Everything below is layout, CSS, view structure, and the
words on screen. The one thing that *could* graduate to the protocol layer —
renaming a couple of precise-but-jargon terms — is called out in §7 as a
**separate proposal**, never encoded silently here.

Guiding voice: Steve Krug — *make the user think as little as possible.* The
constraint that makes this interesting: **we do not hide the p2p model, we
translate it.** zink's model maps cleanly onto social intuitions people already
have (a phone's contact list is *your* names for people; an unknown number is
unknown until someone vouches). Dress the model in social language and it stops
being scary; keep the cryptographic truth visible exactly where it's
load-bearing (confirming a device, adding someone).

## 1. Goal & non-goals

**Goal.** The app should *feel* like a real, coherent, minimal product: one
consistent palette anchored on the logo, screens that each answer a single
question, a first run that welcomes rather than interrogates, and buttons that
sit where a thumb expects them. No user should have to think about "which of
the four things is this screen for" or hunt for a button hidden behind the
phone's navigation bar.

**Non-goals.** No flashy motion or decorative animation (minimal by intent — a
few functional transitions at most). No hard-coded default-relay list yet
(§Decisions) — relays stay user-provided. No PWA/web-client styling work
(native-first; the browser client is post-MVP). No protocol or wire changes.
No new features — a facelift reorganizes and re-skins what already exists.

## 2. Where the app makes people think today

Baseline audit of `app/ui/src/lib.rs` + `app/dist/index.html` (2026-07-22):

- **Brand incoherence.** The logo is violet (`#7c3aed`, `#a78bfa` in dark) — a
  two-node-and-a-path "Z", a tidy p2p metaphor — but the app is teal
  (`#0e6b64`). The mark's palette is unused.
- **The "contacts" tab is four screens in one.** `ContactsView` is
  simultaneously *your profile* (name/avatar/QR/device key), *device pairing*,
  *add-a-contact*, and *the contact list*, in one long scroll. Different
  questions ("who am I?" vs "who do I know?") sharing one surface.
- **First run interrogates.** A new user with no profile is routed straight to
  that mega-screen; the second field is `endpoint-id@ip:port`, a raw relay dial
  string — the single scariest thing in the app, shown first.
- **The chat list isn't a list.** It permanently carries a multi-select "new
  chat" composer glued to the bottom, plus a manual **refresh** button (a
  worry-generator: "is this broken?"). Browsing and composing are tangled.
- **Protocol vocabulary leaks.** "repudiate", "vouch", "recognize device",
  "unopenable", "ZINK: payload", and a run-on of ` · ⏳ · ⇄ · ⋈` metadata ask
  the user to learn our words instead of using words they already own.
- **The bottom-bar occlusion bug.** `dist/index.html`'s viewport meta lacks
  `viewport-fit=cover`, the layout uses `min-height: 100vh`, and `.compose` is
  pinned via `margin-top:auto` with no safe-area padding. On Android WebView
  `100vh` spans the strip under the system gesture/nav bar, so the send button
  lands under it. (This is what forces the phone-rotation workaround.)

## 3. Decisions (resolved 2026-07-22)

| Decision | Resolution |
|---|---|
| Palette | Anchor on the **logo violet** (`#7c3aed` light / `#a78bfa` dark), replacing teal. Neutrals = the **zinc** gray scale (on-brand by name). CSS custom properties only; no framework, no new deps. |
| Dark mode | Supported via `prefers-color-scheme` (the logo already ships a dark variant). |
| Navigation | A **bottom tab bar** with three homes — **Chats / People / Me** — replacing the two-button top header. Thumb-reachable, and the bar owns the safe-area padding (which also fixes the occlusion bug structurally). |
| Screen split | The 4-in-1 `ContactsView` splits along its natural seams: *Me* (your identity + devices + relays), *People* (others), with add/pair as focused sub-flows. |
| Relays | **First-class and user-visible** (multi-relay supported out of the box), but surfaced to the *minimum* extent: a friendly list under **Me**, framed socially ("where your messages wait when you're offline"). **No hard-coded default relay** now — the user provides at least one (their own, or one a friend shares). A hard-coded default list is explicitly deferred. |
| First-run relay | Because there's no default, onboarding *must* include a relay step — but a gentle, well-explained one (paste/scan-friendly), not a raw field shown cold. |
| Person = lens over keys | A person is a **local lens over a key-set** (`Contact.keys` is already `Vec<PublicKey>`), never a shared object. Screens render three separated belief layers — mine / their self-claim / friends' vouches (§4) — and are built **cluster-first**; the single-key DTOs widen to carry the set. |
| Friends' lens | Render **vouched names** now (built, D4a): "Alice calls them 'Bobby' · vouched by B, D". A friend's **avatar** (per-attester avatar lens) is **deferred in web-of-trust.md §6** — rendered once the claim exists, tracked there, not a facelift slice. |
| Local avatar override | **In scope** (U6). Your lens may carry a photo *you* chose for a contact (client-side only, like a phone contact card), overriding the resolved avatar. |
| Petnames are local; sharing is a client convention | A petname is a local label (my lens). Broadcasting one is a *client choice*, not a protocol privacy rule — and this client's convention (MVP) is to broadcast it **when you vouch** for that person (`vouch` publishes `Claim::Name(<your petname>)`). So the friends' lens shows a friend's name for someone **only if they vouched**; the button says so plainly ("share the name you call them"). No auto-broadcast (keeps it simple + explicit); auto-share would be a separate, opt-in convention. |
| Setting your name for a person | Was missing (scan added a contact under its self-claimed name, no edit). Landed in U4: a petname field at add time, and rename on the person detail (`Client::rename` — client policy, no protocol). A *local* picture for a person is the U6 avatar override. |
| Vocabulary | **Translate** protocol terms to social ones in the UI (§7). Terms that read better may be floated as **protocol back-port proposals** — separately, never silently. |
| Concurrency markers | `crossed` / `merged` (tenet 7 honesty data) are **hidden by default**, available behind an optional "show concurrency" affordance — they're for advanced users, not everyday noise. |
| Manual refresh | Removed. Live delivery + the 60 s backstop poll already cover it; a visible refresh button only sows doubt. (Pull-to-refresh is an acceptable later nicety.) |
| Scope tracking | Tracked here, parallel to the MVP plan; same one-slice-per-turn cadence. |

## 4–7 · Identity model, design system, IA, vocabulary — graduated

These four sections were the *durable* half of this pass — how the app renders the
person-lens, the design tokens, the information architecture, and the UI vocabulary.
They now live as a maintained reference at
[../../design/ui-design-system.md](../../design/ui-design-system.md); later UI work
edits them there, not in this archived tracker. The §3 decisions above are the record
of *how* they were chosen; the slices below are the record of *when* they landed.

## 8. Slices (the tracker)

Same format as the MVP plan. **Definition of done (every slice):** runnable /
WASM UI builds · `cargo fmt` + `clippy` clean (Rust touched) · the app runs and
the change is visible on device where relevant · this doc updated.

- [x] **U1 · Design tokens + safe-area fix.** The CSS custom properties of §5
  in `dist/index.html`; recolor the existing UI from teal → violet + zinc with
  no structural change yet; type scale; button roles; `viewport-fit=cover` +
  `100dvh` + `env(safe-area-inset-bottom)`. *Done when:* the app is visibly
  violet/coherent and the composer's send button clears the phone's nav bar
  without rotating. **Highest visible-improvement-to-risk ratio; fixes the
  reported bug; lays the tokens every later slice uses. Recommended first.**
- [x] **U2 · Bottom-tab navigation + screen split.** Replace the top
  two-button header with a bottom tab bar (Chats / People / Me, safe-area
  padded). Relocate the existing `ContactsView` content into **Me** (profile +
  devices + relays) and **People** (list + add/pair), no redesign of the
  internals yet — just move them to the right homes. *Done when:* all existing
  functionality is reachable under the three tabs; nothing regressed.
- [x] **U3 · Chats list + compose flow.** The list becomes a clean list; a **+**
  opens "start a chat" (pick one or more people → chat opens). Remove the
  permanent multi-select form and the refresh button. *Done when:* starting a
  new chat is a deliberate + action and the list shows only conversations.
- [x] **U4 · People + person detail (lens-first).** Contact rows → tap-through
  detail screen built as the §4 **Person lens**: three separated belief layers —
  *my lens* (petname, avatar, their device keys) · *their self-claim*
  (self-name/avatar, self-attested `SamePersonAs` links shown directionally) ·
  *friends' lens* (vouched names — "Alice calls them 'Bobby' · vouched by B, D",
  with the "only what they shared" privacy note). Trust actions (vouch /
  repudiate) and disavowal warnings live in context; add/scan/paste/pair as
  focused sub-flows off a **+**. Widen `ContactRow`/`Message` DTOs to carry the
  key-set + layers. *Done when:* the detail screen shows all three belief layers
  cluster-first, every D1–D4 action lives there, and nothing assumes
  one-key-per-person.
- [x] **U5 · Me: profile, devices, relays.** The identity screen: name, avatar,
  QR/"your code", your **linked devices shown with directional recognition** (§4
  — "this device recognizes X; scan back to confirm both ways"), and the
  **multi-relay list** framed per §7 (add/remove, "where your messages wait").
  *Done when:* a user can manage name, avatar, devices, and ≥1 relay from one
  calm screen, with recognition directionality honest.
- [x] **U6 · Local avatar override.** Let your lens carry a photo *you* chose
  for a contact — a client-side override of the resolved self-claim, stored
  plaintext on-device only, never published (reuses the U5 canvas path). Wins in
  `Client::avatar` everywhere their avatar shows; a "use their photo instead"
  clears it. *Done when:* you can set a private photo for a contact from their
  detail screen and it never leaves the device. *(Code complete 2026-07-24:
  `state` local-avatar store + `Client::set_local_avatar`/`clear_local_avatar`/
  `has_local_avatar`, `avatar` prefers the override; `set_local_avatar`/
  `clear_local_avatar` commands; `PersonDetail.has_local_avatar`; picker +
  "use their photo instead" in the person detail.)*
  *(Local grouping — the "football-team" case — was descoped 2026-07-24: not
  thought through, and it needs a new local same-person store; see the §4 note.)*
- [x] **U7 · First-run onboarding.** The §6 sequence (name/avatar → relay →
  your code), replacing the "dumped into the mega-screen" first run. Reuses U5
  widgets. *Done when:* a fresh install walks a new user to a shareable code
  without ever showing a raw dial string cold.
  *(2026-07-24: **code complete** — commit `3ac8afb`, `app/ui/src/lib.rs`: an
  `OnboardingView` takeover shown while no profile exists (no tab bar),
  stepping Identity → Relay → Code with back navigation, then landing on
  Chats.)*
  ✅ *(2026-07-26: **fresh-install run done** by Mårten — a wiped data dir
  walked to a shareable code, which is the only thing the criterion asserts.)*
- [x] **U8 · Language + metadata legibility.** Apply the §7 vocabulary across
  the UI; make message metadata scannable (states as small pills/icons with
  meaning, not a symbol run-on); hide `crossed`/`merged` behind an optional
  "show concurrency" toggle. *Done when:* no protocol jargon is user-facing by
  default and the message row reads at a glance.

**🎉 Facelift complete** (2026-07-26) — U1–U8 all landed, U7's fresh-install run
being the last of them. Nothing in this tracker is outstanding; the follow-ups
below are deliberately out of scope, not leftovers.

Follow-ups / parked: **vouched avatars (friend-lens photos)** — the per-attester
avatar lens is deferred in web-of-trust.md §6; when third-party avatar claims
land there, U4's friends'-lens renders them with no facelift redesign. Also:
**reclaim vertical space in the chat view** (message rows are tall — noted at
U3, 2026-07-23); pull-to-refresh; a "show concurrency" advanced view beyond the
toggle; any accepted vocabulary back-port (separate doc); PWA styling (post-MVP,
when the browser client returns).
