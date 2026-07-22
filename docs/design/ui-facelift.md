# UI facelift: coherent design & "don't make me think" UX

A dedicated design + task tracker for the first real UX/visual pass over the
Tauri/Leptos app. Runs **parallel to** [mvp-build-plan.md](./mvp-build-plan.md)
(the MVP is functionally near-complete; this is polish, not a new capability),
governed by the same slice discipline: small vertical slices, one per turn,
each runnable and reviewed before the next.

**This pass is UI-only.** No protocol changes, no `zink-protocol` touch, no new
Rust dependencies. Everything below is layout, CSS, view structure, and the
words on screen. The one thing that *could* graduate to the protocol layer —
renaming a couple of precise-but-jargon terms — is called out in §6 as a
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
| Vocabulary | **Translate** protocol terms to social ones in the UI (§6). Terms that read better may be floated as **protocol back-port proposals** — separately, never silently. |
| Concurrency markers | `crossed` / `merged` (tenet 7 honesty data) are **hidden by default**, available behind an optional "show concurrency" affordance — they're for advanced users, not everyday noise. |
| Manual refresh | Removed. Live delivery + the 60 s backstop poll already cover it; a visible refresh button only sows doubt. (Pull-to-refresh is an acceptable later nicety.) |
| Scope tracking | Tracked here, parallel to the MVP plan; same one-slice-per-turn cadence. |

## 4. Design system (the tokens)

A dozen CSS custom properties in `dist/index.html` are the entire "system".
Zinc neutrals + violet accent; semantic colors for state. Concrete starting
values (tune in U1):

```css
:root {
  --accent:        #7c3aed;  /* violet-600 — the logo */
  --accent-strong: #6d28d9;  /* pressed / active */
  --accent-weak:   #ede9fe;  /* own-message bubble tint (replaces teal) */
  --bg:            #fafafa;
  --surface:       #ffffff;
  --surface-alt:   #f4f4f5;  /* incoming bubble, rows */
  --border:        #e4e4e7;
  --text:          #18181b;  /* zinc-900 */
  --muted:         #71717a;  /* zinc-500 — timestamps, hints */
  --danger:        #dc2626;  /* repudiate / destructive */
  --ok:            #16a34a;
  --radius:        10px;
  --radius-sm:     6px;
  --space:         8px;       /* scale: 4 / 8 / 12 / 16 / 24 */
  --tap:           44px;      /* min interactive height */
}
@media (prefers-color-scheme: dark) {
  :root {
    --accent:        #a78bfa;  /* violet-400 — the logo's dark variant */
    --accent-strong: #8b5cf6;
    --accent-weak:   #2e1065;
    --bg:            #0b0b0f;
    --surface:       #18181b;
    --surface-alt:   #27272a;
    --border:        #3f3f46;
    --text:          #fafafa;
    --muted:         #a1a1aa;
    --danger:        #f87171;
    --ok:            #4ade80;
  }
}
```

**Type scale:** keep `system-ui` (minimal, zero deps). Sizes ~ `.75 / .875 / 1
/ 1.125 / 1.375rem` (caption / small / body / lead / title). Kill the unreadable
`0.6rem` monospace record text — fingerprints get a legible mono size and only
appear at trust moments.

**Buttons — three roles, not one:** `primary` (filled violet), `secondary`
(ghost/outline), `danger` (red, e.g. repudiate). Inline row actions (who is? /
vouch / repudiate) become small ghost buttons, not stacked full-width blocks.
All interactive targets ≥ `--tap`.

**Safe area (the occlusion fix), specifically:**
- Add `viewport-fit=cover` to the viewport meta.
- `min-height: 100vh` → `100dvh` (dynamic viewport height).
- Pad the bottom bar / composer with `env(safe-area-inset-bottom)`.

## 5. Information architecture

Three destinations, each answering one question. Bottom tab bar.

- **Chats** — *"what's happening?"* The conversation list, nothing else glued
  to it. A single **+** starts a new chat (pick people → chat opens with an
  empty composer). No permanent compose form, no refresh button.
- **People** — *"who do I know?"* Just the contact list + a **+** (scan / paste
  / pair as focused sub-flows). Tapping a person opens a **detail screen**:
  avatar, what they call themselves, your petname, trust actions (vouch /
  repudiate), and their devices.
- **Me** — *"who am I, and how do I reach the world?"* Name, avatar, my QR, my
  linked devices, and my relays (the multi-relay list, framed gently).

First run is a calm sequence, reusing the Me widgets: **name (+ optional
avatar) → add a relay (explained, paste/scan-friendly) → here's your code**.

## 6. Vocabulary: translate in the UI, propose upward separately

UI-facing words (protocol names stay as-is in code/spec unless a back-port is
separately accepted):

| Protocol / current UI | UI word | Notes |
|---|---|---|
| `endpoint-id@ip:port` (relay) | "your relay — where messages wait for you" | one friendly field; multi-entry list |
| repudiate | "this isn't them anymore" / mark compromised | destructive styling |
| vouch | "vouch for" / "help friends recognize them" | |
| recognize device / same-person-as | "this is also me" / "link a device" | |
| unopenable | 🔒 "can't read this yet" | |
| the raw key | "fingerprint" | shown only at trust decisions, as something to compare |
| "a wild key appeared" | *(keep — it's good)* | soften surrounding copy only |
| ZINK: payload | "your code" / "their code" | |

**Back-port candidates (separate proposal, not this pass):** a few protocol
terms may read better even at the spec level — e.g. whether "recognize
device" / "same-person-as" wants a clearer canonical name. If any is worth it,
it gets its own doc/spec change per AGENTS.md; nothing here changes the
protocol.

## 7. Slices (the tracker)

Same format as the MVP plan. **Definition of done (every slice):** runnable /
WASM UI builds · `cargo fmt` + `clippy` clean (Rust touched) · the app runs and
the change is visible on device where relevant · this doc updated.

- [ ] **U1 · Design tokens + safe-area fix.** The CSS custom properties of §4
  in `dist/index.html`; recolor the existing UI from teal → violet + zinc with
  no structural change yet; type scale; button roles; `viewport-fit=cover` +
  `100dvh` + `env(safe-area-inset-bottom)`. *Done when:* the app is visibly
  violet/coherent and the composer's send button clears the phone's nav bar
  without rotating. **Highest visible-improvement-to-risk ratio; fixes the
  reported bug; lays the tokens every later slice uses. Recommended first.**
- [ ] **U2 · Bottom-tab navigation + screen split.** Replace the top
  two-button header with a bottom tab bar (Chats / People / Me, safe-area
  padded). Relocate the existing `ContactsView` content into **Me** (profile +
  devices + relays) and **People** (list + add/pair), no redesign of the
  internals yet — just move them to the right homes. *Done when:* all existing
  functionality is reachable under the three tabs; nothing regressed.
- [ ] **U3 · Chats list + compose flow.** The list becomes a clean list; a **+**
  opens "start a chat" (pick one or more people → chat opens). Remove the
  permanent multi-select form and the refresh button. *Done when:* starting a
  new chat is a deliberate + action and the list shows only conversations.
- [ ] **U4 · People + person detail.** Contact rows → tap-through detail screen
  (avatar, self-name, petname, vouch/repudiate, their devices, disavowal
  warnings). Add/scan/paste/pair as focused sub-flows off a **+**. *Done when:*
  every D1–D4 contact action lives on a coherent detail screen, not a flat row
  of stacked buttons.
- [ ] **U5 · Me: profile, devices, relays.** The identity screen: name, avatar,
  QR/"your code", linked devices, and the **multi-relay list** framed per §6
  (add/remove, "where your messages wait"). *Done when:* a user can manage
  name, avatar, devices, and ≥1 relay from one calm screen.
- [ ] **U6 · First-run onboarding.** The §5 sequence (name/avatar → relay →
  your code), replacing the "dumped into the mega-screen" first run. Reuses U5
  widgets. *Done when:* a fresh install walks a new user to a shareable code
  without ever showing a raw dial string cold.
- [ ] **U7 · Language + metadata legibility.** Apply the §6 vocabulary across
  the UI; make message metadata scannable (states as small pills/icons with
  meaning, not a symbol run-on); hide `crossed`/`merged` behind an optional
  "show concurrency" toggle. *Done when:* no protocol jargon is user-facing by
  default and the message row reads at a glance.

Follow-ups / parked: pull-to-refresh; a "show concurrency" advanced view beyond
the toggle; any accepted vocabulary back-port (separate doc); PWA styling
(post-MVP, when the browser client returns).
