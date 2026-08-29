# Design: UI design system & the identity model it renders

The durable UI reference for the Tauri/Leptos app: the identity model the screens
render, the design tokens, the information architecture, and the UI vocabulary.
Downstream of [DESIGN-PHILOSOPHY.md](../DESIGN-PHILOSOPHY.md) (a person is local
belief over keys) and [SPEC.md](../SPEC.md); siblings
[who-is-this.md](./who-is-this.md) and [web-of-trust.md](./web-of-trust.md) own the
lookup/provenance machinery these screens surface.

Status: **resolved for MVP** — extracted from the U1–U8 facelift
([../projects/2-ui-facelift/tracker.md](../projects/2-ui-facelift/tracker.md)),
which is the narrative record of how it was built. This doc is the *what and why*
that later UI work edits in place.

Guiding voice: Steve Krug — *make the user think as little as possible.* The
constraint that makes this interesting: **we do not hide the p2p model, we
translate it.** zink's model maps cleanly onto social intuitions people already
have (a phone's contact list is *your* names for people; an unknown number is
unknown until someone vouches). Dress the model in social language and it stops
being scary; keep the cryptographic truth visible exactly where it's load-bearing
(confirming a device, adding someone).

## 1. The identity model the screens render

zink's model — and, it turns out, `zink-client` already — treats a **person as a
local lens over a set of keys**, never a shared object. `Contact` is
`keys: Vec<PublicKey>` (a cluster whose label sits at its first key);
`SamePersonAs` links are **self-attested only**, so recognition is *directional*
and need never be symmetric (your phone can recognize your laptop while the
laptop doesn't reciprocate); vouching (D4a) is the **per-attester lens** —
a friend's label reaches you only if they *chose to publish* it (who-is-this.md
§6, web-of-trust.md §6). "Seeing people through your friends' eyes" is this lens,
made first-class in the UI.

**The corner to avoid — and it's the only one.** The core is cluster-first, but
the DTO/UI layer flattened it: `ContactRow` carries a single `key`,
`Message.sender_key` is singular. Build the People/Me screens on that and we
silently re-bake "one key = one person," fighting the model. **Guardrail:** the
People/Me screens render a `Person` view-model = `{ key-set, my petname, my avatar
(override-or-resolved), per-key trust/recognition state }`, and the DTOs widen to
carry it. Cluster-first, never key-first.

**Three layers of belief, always visually separated** (the heart of the person
detail screen):

1. **My lens** — authoritative to me: my petname, my avatar for them (a photo I
   chose, else the resolved one), and this person's device keys.
2. **Their self-claim** — verified self-name / self-avatar, plus any self-attested
   `SamePersonAs` links ("this key says it's also …").
3. **Friends' lens** — the vouched names mutual contacts published: *"Alice calls
   them 'Bobby' · vouched by B, D."* **Privacy invariant, printed in the copy:**
   you only ever see what a friend *chose to share* — never their private petname.

**How a person's keys cluster, rendered honestly:**

- **Self-attested link** (crypto-backed): "recognized as the same person — they
  say so," shown with **directionality** ("this device recognizes it; scan back to
  confirm both ways" — matches the existing pair copy). Never implies symmetry.
  A contact's cluster is exactly its signed `ContactRecord`'s keys (`contact_from`
  sets `Contact.keys = record.keys.clone()`); own devices cluster via
  `recognize_device`.
- **Never a third-party device link** — a friend can vouch a *name*, never link
  someone else's devices (web-of-trust.md §6). Structurally impossible; we don't
  offer it.

> *Arbitrary local grouping* — bagging keys under one label you choose —
> **landed with project 7 S2**: the person store (`persons/<id>`, ids minted
> locally, labels collision-checked) is exactly the "new local same-person
> store" this note once deferred. Merge / split / rename are explicit acts,
> evidence only ever *offers* a merge, and send-by-name resolves the person
> label to every member key. Nothing groups on its own — clustering stays a
> deliberate act of yours.

**Friends' avatars landed too** (project 7 S5, web-of-trust.md §6): a friend's
photo of someone renders under the *through friends* lens — "as Bob tells
you" — from third-party `Avatar` endorsements, never replacing your override
or the subject's self-claim as the page face. A **local avatar override** (a
photo *you* assign) remains client-side and shipped (U6).

## 2. Design system (the tokens)

A dozen CSS custom properties in `dist/index.html` are the entire "system".
Zinc neutrals + violet accent (anchored on the logo mark); semantic colors for
state. Concrete values:

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
/ 1.125 / 1.375rem` (caption / small / body / lead / title). No unreadable
`0.6rem` monospace record text — fingerprints get a legible mono size and only
appear at trust moments.

**Buttons — three roles, not one:** `primary` (filled violet), `secondary`
(ghost/outline), `danger` (red, e.g. repudiate). Inline row actions (who is? /
vouch / repudiate) are small ghost buttons, not stacked full-width blocks.
All interactive targets ≥ `--tap`.

**The people-picker** (project 6 S1): every pick-a-person surface is the same
component (`app/ui/src/picker.rs`) — full-width tappable rows (avatar +
petname, ≥ `--tap`), the selected set as removable chips above the list, a
filter box once the list passes ~8 contacts, alphabetical order, and an empty
state that points at People. Selection is by petname; what a pick *means*
stays with the calling screen.

**Safe area (the occlusion fix), specifically:**
- `viewport-fit=cover` on the viewport meta.
- `min-height: 100dvh` (dynamic viewport height), never `100vh`.
- Pad the bottom bar / composer with `env(safe-area-inset-bottom)`.

## 3. Information architecture

Three destinations, each answering one question. Bottom tab bar (thumb-reachable;
the bar owns the safe-area padding, which also fixes the occlusion bug
structurally).

- **Chats** — *"what's happening?"* The conversation list, nothing else glued
  to it. A single **+** starts a new chat: pick people (the §2 picker), then a
  **draft chat** — empty history, a line naming that the first send starts the
  conversation, and links to the existing conversations with exactly those
  people (discovery, never auto-routing: a new chat is always a *new*
  conversation; several per participant set is a feature, groups.md §3). The
  draft reuses the one composer, so a first message can do anything a reply
  can. Inside a chat, the header is tappable → a **members panel**: current
  membership (unknown keys as honest short hex), adding people via the picker
  (one batched membership message), **naming the conversation** — a local
  label, my lens like a petname, never transmitted; it outranks the
  joined-petnames default in every list, the header, and notification titles,
  which is what tells several same-set conversations apart — and the advanced
  affordances (introduce-devices, concurrency cues) — one tap away, never
  always-on. Rows carry a relative time, a one-line preview (local plaintext,
  client policy), and an **unread badge** off a local read marker — never a
  receipt to anyone; the positive-only delivery cues (tenet 7) are untouched.
  No permanent compose form, no refresh button (live delivery + the 60 s
  backstop poll cover it; a visible refresh button only sows doubt).
- **People** — *"who do I know?"* Just the contact list + a **+** (scan / paste
  / pair as focused sub-flows). Tapping a person opens a **detail screen** built
  as the §1 lens: my lens (petname, avatar, their device keys) · their self-claim ·
  **friends' lens** (vouched names). Trust actions (vouch / repudiate) and
  disavowal warnings in context.
- **Me** — *"who am I, and how do I reach the world?"* Name, avatar, my QR, my
  linked devices (with directional recognition, §1), and my relays. Relays are
  **first-class and user-visible** (multi-relay out of the box) but surfaced
  gently — a friendly list framed socially ("where your messages wait when you're
  offline"). **No hard-coded default relay:** the user provides at least one
  (their own, or one a friend shares); a default list is deferred.

**Every rendered identifier navigates to the person page** (project 7 S4):
chat sender lines, members-panel rows, and wild-key rows tap through to the
§1 lens (a member key lands on its person's page; a stranger key on the
stranger variant; the merged "you" row is an own cluster — Me is its page).
The page owns the identity acts and evidence; in-chat surfaces only surface
and link — the wild-key row is a link, not a popup with its own who-is /
ignore / add machinery.

First run is a calm sequence, reusing the Me widgets: **name (+ optional
avatar) → add a relay (explained, paste/scan-friendly) → here's your code**.
Because there's no default relay, onboarding *must* include the relay step — but
gently, never a raw `endpoint-id@ip:port` field shown cold.

## 4. Vocabulary: translate in the UI, propose upward separately

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

**Petnames are local; sharing is a client convention.** A petname is a local
label (my lens). Broadcasting one is a *client choice*, not a protocol privacy
rule — and this client's convention (MVP) is to broadcast it **when you vouch**
for that person (`vouch` publishes `Claim::Name(<your petname>)`). So the
friends' lens shows a friend's name for someone **only if they vouched**; the
button says so plainly ("share the name you call them"). No auto-broadcast.

**Concurrency markers.** `crossed` / `merged` (tenet 7 honesty data) are hidden
by default, available behind an optional "show concurrency" affordance — for
advanced users, not everyday noise.

**Back-port candidates (separate proposal, never silent):** a few protocol
terms may read better even at the spec level — e.g. whether "recognize device" /
"same-person-as" wants a clearer canonical name. If any is worth it, it gets its
own doc/spec change per AGENTS.md; nothing here changes the protocol.
