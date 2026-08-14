# Module split: carve `client.rs`, keep `Client`

> **Status: 📝 scoping (2026-08-14).** Project **4-module-split**, sequenced
> after [3-ports-and-adapters](../3-ports-and-adapters/tracker.md) so the
> split lands on the port seams that project carved (its §8 parked this).
> Behavior-preserving throughout: the suite is the net, the diff is moves.

Governed by the standard slice discipline (AGENTS.md): small vertical slices,
one per turn, each runnable and measured before the next. This project touches
**no wire format, no `zink-protocol` core, no public API surface** (the
`lib.rs` re-exports stay byte-identical), and adds **no dependency**.

## 1. Why — and what this is *not*

`crates/zink-client/src/client.rs` is 7,696 lines. The diagnosis matters,
because it decides the treatment: **`Client` is not a god object — the file
is.** The struct has ten fields, and nearly all are shared plumbing every
flow needs (`state`, `transport`, `device`, `config`, the two clocks); the
method clusters differ in *behavior*, not in *state*. The two exceptions —
`reach` (three methods + one write from `sync.rs`) and `queried` (one call
site) — are exactly the two raw mutexes, and they get named types (§7).

So this is **not** a decomposition of `Client` into sub-structs. That was
considered and rejected (§7): the state doesn't partition, every sub-struct
would carry the same three generic parameters (`C, W, N`), the clusters call
each other (deliver → contact resolution; recv → the auto-hooks), and the
facade *is* the design — `lib.rs`: "everything 'being a zink client' means."
What's wrong is navigability and three concrete violations of our own rules:

- **STYLE.md's ordering rule is violated**: "a public type appears
  immediately above the function that returns or takes it" — today all 20
  result types sit in a block *after* the 2,770-line impl (`client.rs:3059`).
- **The test kit is buried**: ~4,240 of the 7,696 lines are one `mod tests`
  (69 tests + ~25 shared helpers). The helpers (`loop_client`, `chain`,
  the frame builders, `spawn_test_relay`…) are invisible from outside it.
- **A dependency inversion around reach**: `sync.rs` defines
  `ReachMap`/`Reach` while `client.rs` owns the policy
  (`reach_of`/`note_reach`/`persist_unreachable`/`load_unreachable`/
  `direct_budget`), so the serve edge imports `crate::client::now_ms` upward
  to write one field.

**This is not a redesign.** No logic changes ride along. The two extractions
that look like design (`ReachLedger`, `AskedOnce`) are re-homing existing
behavior behind named interfaces — same semantics, pinned by moved tests plus
new unit tests the old shape couldn't express.

## 2. Goal & non-goals

**Goal.** `client.rs` becomes a root module (struct, config, constructors,
lifecycle) over a `client/` directory of flow modules, each one
`impl<C, W, N> Client<C, W, N>` block + its result types + its tests, with
the shared test helpers in a `#[cfg(test)]` kit module. The two anonymous
mutexes become named handles owning their locks. `app/ui/src/lib.rs`
(2,362 lines) gets the same by-screen treatment. Every file tells the reader
what it is; no file needs a scroll map.

**Non-goals.**
- No behavior change, no API change, no new deps, no protocol/wire touch.
- **No sub-structs / parallel facades** (`SendPipeline`, `ContactBook` — §7).
- No `mod.rs` files — root-file-plus-directory, as `ports.rs`/`ports/` and
  `adapters.rs`/`adapters/` already do.
- Not resolving the mid-run wall-rewind question (project 3 §8): the
  `ReachLedger` gives it a *home*, but deciding monotonic-vs-wall evidence
  ages changes behavior — separate decision, parked in §8.
- `state.rs` (1,016 lines) stays: one cohesive store, no cluster tension.
- No UX/design change in the `app/ui` carve (ui-design-system.md governs).

## 3. Baseline (measured 2026-08-14)

| File | Lines | Shape |
|---|---|---|
| `client.rs` | 7,696 | `impl Client` 168–2936 (~2,770) · free fns + 20 pub types ~3059–3456 · one `mod tests` 3457–7696 (69 tests, ~25 shared helpers) |
| `state.rs` | 1,016 | cohesive; out of scope |
| `sync.rs` | 312 | defines `Reach`/`ReachMap` whose policy lives in `client.rs` |
| `app/ui/src/lib.rs` | 2,362 | every screen in one file (workspace-excluded, WASM) |

Suite (project 3 P8): **235 tests, ~1.1 s wall** (up to ~5.7 s when the
`keep_delivering` smoke waits out a slow holepunch); clippy clean; `wasm32`
compiles. Those numbers are the **regression floor** — re-measure each slice;
a split that slows the suite or breaks determinism has moved something it
shouldn't have.

Method clusters in the big impl (the module lines, verified against the
call graph): lifecycle · send/stage/deliver · reach+outbox · recv/subscribe ·
contacts+trust · who-is · profile/home-relays · avatars · conversation reads
(history/membership/triage) · backfill/auto-sync · rewrap. Cross-cluster
calls run one direction: flows → resolution helpers → state; recv → the
`auto_*` hooks.

## 4. Guardrails — the traps to avoid

- **Moves are moves.** Each carve is a pure relocation plus the comment
  prune, reviewable as such. A bug or improvement found mid-move lands as
  its own slice/commit, never silently inside one. If a moved test's
  assertion has to change, the move wasn't a move — stop and say why.
- **Prune ≠ delete knowledge.** Many `client.rs` docstrings carry
  why/mechanism/testing narration that belongs in design docs (project 3 §8).
  While each file is rewritten anyway: load-bearing narration graduates to
  the relevant `docs/design/*.md`, restatement is deleted, and what remains
  is what a caller needs (Kevlin-lean). Never drop a rationale that exists
  nowhere else.
- **Don't over-carve.** A cluster earns a file; a method doesn't. If a
  module would open with one function, it hasn't earned the file yet.
  Exact member assignment is decided at each slice by call-graph affinity —
  the §6 table is the sketch, not a contract.
- **The re-export surface is frozen.** `lib.rs`'s `pub use` list stays
  identical; `zink-cli` and the app compile with zero diff. Inside the
  crate, `client.rs` re-exports its submodules' types so paths stay short.
- **Named shared state, not naked collections.** The two extractions follow
  one pattern: the lock lives private in one small module, the interface is
  verbs that maintain the invariants, timestamps come in as data (`now`
  params — the transport rule "no time inside the port," applied to state),
  I/O stays with the caller. The mutexes *survive* — the sharing is real
  (the serve task and `deliver_direct`'s concurrent dial futures genuinely
  share the reach table) — they just stop being crate-visible type aliases.
- **The app carve is view-only.** By screen, no styling or copy changes;
  verify by building (`app/ui/build.sh`) and clicking through, since the
  crate is workspace-excluded and has no test net.

## 5. Graduation plan

- **STYLE.md** gains the two conventions this project establishes:
  *one facade type, impl-per-module* (when a type's API is the product,
  split files not structs) and *share handles, not collections* (named
  shared-state types owning their locks; `ClientState` was already the
  pattern). Small additions to §Module organization.
- **`reach.rs`'s `//!`** absorbs the positive-in-memory / negative-persisted
  rationale currently at `sync.rs:32–44`, plus the ledger's no-clock/no-I/O
  contract — the clock-port precedent (`ports/clock.rs`) suggests the `//!`
  suffices; write a `docs/design/` note only if it outgrows that.
- **`docs/design/client-core.md`** gets its module-map pointer refreshed
  (it names `client.rs` as where the signatures live — still true, but the
  map grows the `client/` directory).
- **Projects README** row added (and project 3's stale "scoping" row fixed).

## 6. Slices

**DoD (every slice):** builds · `cargo fmt` + `clippy --all-targets` clean ·
full suite green with **no assertion changes** · wall time not regressed ·
`wasm32` compiles when the client is touched, `app/ui/build.sh` when the app
is · tracker updated · prune-graduations recorded.

Target tree (the sketch; §4 governs deviations):

```
crates/zink-client/src/
  client.rs        Client, ClientConfig, constructors, assemble/close/
                   await_reachable/on_direct_delivery, Reachable; re-exports
  client/
    send.rs        stage/send/send_in/deliver/deliver_direct/deliver_to_relay
                   + StagedSend, SendReceipt, ReplyContacts
    outbox.rs      flush_outbox/reload_owed + FlushReport
    recv.rs        recv/drain_relay/subscribe/drain_connection/after_direct
                   + RecvReport, Received, RelayFailure
    contacts.rs    add/rename/resolve/vouch/repudiate/disavowals/
                   trusted_record_for/effective_relays/peer_addr_for/
                   resolve_name/learned_candidates/dismiss
                   + Contact, ResolvedName, LearnedName, DeviceEvidence, Disavowal
    who_is.rs      who_is/who_is_among/auto_who_is + WhoIsOutcome, WhoIsAnswer,
                   AskedOnce
    profile.rs     set_profile/home relays/my_record/register_at_home_relays/
                   build_own_record + avatars + AvatarReceipt
    history.rs     conversations/membership/history/participant_labels/triage
                   + Inbox, ConversationSummary, HistoryMessage
    backfill.rs    backfill*/fill_*/fetch_one/auto_sync + rewrap* + remember
    test_kit.rs    #[cfg(test)] shared helpers (was the top of mod tests)
  reach.rs         ReachLedger + Reach (the one lock site)
```

**Tier 1 — the shared-state extractions (de-tangle before moving)**

- [x] **M1 · `reach.rs`: the `ReachLedger`.** ✅ 2026-08-14. New top-level
  leaf module (319 lines incl. its tests) absorbing `Reach` + the `ReachMap`
  alias from `sync.rs` and `reach_of`/`note_reach`/`persist_unreachable`'s
  prune half/`load_unreachable`/`direct_budget`/`FAIL_COOLDOWN_MS` from
  `client.rs`. A cheap-clone handle owning the lock; the notes landed as
  **`note_delivered` / `note_seen` / `note_failed`** (sharper than the
  sketch's `noted_*`): `note_delivered` (a `Stored` ack) also clears a
  pending cooldown — so a concurrent dial's failure can't suppress the next
  send to a peer that just took a message — while `note_seen` (a decline,
  or an inbound connection) deliberately doesn't; both patterns existed as
  closures at the call sites, now named and unit-pinned. `restore` and
  `unreachable_snapshot` are data-in/data-out (no clock, no I/O inside —
  every method takes `now`); `dial_budget` wraps the still-pure
  `direct_budget`, whose policy test moved verbatim. **Poisoned-lock policy
  decided once**, as promised: `PoisonError::into_inner` at the single lock
  site (was three different stances) — no invariant spans entries, evidence
  is advisory, and the drain path's `seen` set already used the same stance.
  `SyncHandler` takes a `ReachLedger`; the free `now_ms()` moved to
  `adapters/system_clock.rs` (state/sync now import the P1 wall-clock
  shortcut from the adapter it names, strengthening `adapters.rs`'s "no
  real clock outside" audit line). The two `load_unreachable` tests were
  re-expressed against the public surface (dial suppressed / not, instead
  of map internals) with fabricated timestamps — no fs, no cleanup; three
  new tests pin delivered-clears / seen-doesn't / snapshot-prunes.
  **Proof:** 238/238 (was 235: −3 moved, +6 in `reach.rs`), clippy clean,
  `wasm32` compiles; `client.rs` 7,696 → 7,490, `sync.rs` 312 → 285; zero
  reach `.lock()` outside `reach.rs`, `ReachMap` gone.
- [x] **M2 · `client/test_kit.rs`.** ✅ 2026-08-14. The 24 shared helpers
  graduated out of `mod tests` into a `#[cfg(test)] mod test_kit` (393
  lines) — bodies verbatim, `pub(crate)`, grouped by concern: temp-dir
  plumbing · envelope builders (`chain`/`sealed_chain`/`message`/
  `sealed_for`) · record shapes (`routed_record`/`signed_record`/…) ·
  mailbox-frame scripting for the doubles (`script_drain`/
  `deposited_envelopes`/…) · client constructors (`spawn_test_relay`/
  `open_homed*`/`loop_client`) · probes (`summary`/`dir_bytes`). They had
  drifted into six bands interleaved with tests; the kit is one discoverable
  module, and this plants the `client/` directory the Tier-2 carves grow
  into (root-file + dir, no `mod.rs`). One comment repair: `record_with_
  dead_mailbox`'s doc was stranded above `sealed_for`'s — reunited. Kit
  sweep: all 24 exercised (zero dead-code warnings), nothing kept warm.
  **Proof:** 238/238, clippy clean, `wasm32` compiles; `client.rs`
  7,490 → 7,111; `mod tests` now opens with its first test.

**Tier 2 — carve the impl, cluster by cluster** (each slice: move the impl
block + its types + its tests, prune comments per §4; standing rule: a moved
test that binds a real endpoint without asserting a real-network property —
the project 3 §7 residue, ~0.1–0.3 s each — converts to loopback/doubles
opportunistically, as its own commit)

- [ ] **M3 · `client/send.rs` + `client/outbox.rs`.** The send/stage/deliver
  paths and the flush/reload machinery. `deliver_direct` now reads as
  ledger calls. `OUTBOX_GIVE_UP_MS`/`FLUSH_CONCURRENCY` ride along.
- [ ] **M4 · `client/recv.rs`.** recv/drain/subscribe/after_direct +
  `Received` (re-exported where `sync.rs` needs it) + `MAX_NUDGE_BYTES`.
- [ ] **M5 · `client/contacts.rs` + `client/who_is.rs`.** The trust/identity
  cluster and the query cluster. `queried` becomes **`AskedOnce`**,
  private to `who_is.rs`: one method (`first(subject, conversation) ->
  bool`), test-and-set atomic by construction, the D2b rationale on the
  type instead of a field comment. `WHO_IS_DIAL_CAP` rides along.
- [ ] **M6 · `client/profile.rs` + `client/history.rs`.** Profile/home-relay
  management + `build_own_record` (still shared with `sync.rs` — now a
  downward import from a small module instead of into the monolith), the
  avatar flows, and the read-side conversation views + `triage`.
- [ ] **M7 · `client/backfill.rs`.** backfill/fill/fetch_one/auto_sync +
  the rewrap trio + `remember` (shared with `sync.rs`; decide at the slice
  whether it belongs here or beside `state.rs`). Named `backfill`, not
  `sync` — the crate already has a `sync.rs` serving the *other* direction.

**Tier 3 — the app + close-out**

- [ ] **M8 · `app/ui/src/lib.rs` by screen.** Same treatment, different
  toolchain: root `lib.rs` (app shell, routing) over per-screen view
  modules. View-only (§4); verified by build + click-through.
- [ ] **M9 · Re-measure + graduate.** Final line-count and suite-time table
  here; STYLE.md conventions, `reach.rs` `//!`, client-core.md pointer,
  projects README rows per §5.

## 7. Decisions log

| Decision | Resolution |
|---|---|
| Split the type or the module? | **Module.** `Client` stays the one facade; one file per flow cluster, each an `impl<C, W, N> Client<C, W, N>` block. The state doesn't partition (ten fields, all-but-two shared by every cluster), sub-structs would each carry the same three generic parameters plus delegation for ~60 pub methods, and the clusters interlock (deliver → resolution; recv → auto-hooks). Rejected: parallel structs (`SendPipeline`, `ContactBook`). |
| Directory style | **No `mod.rs`**: `client.rs` stays the root module and gains a `client/` directory — the `ports.rs`/`ports/`, `adapters.rs`/`adapters/` shape the crate already uses. |
| Types | Move next to their methods, per STYLE.md's existing rule ("a public type appears immediately above the function that returns or takes it") — the current after-the-impl block violates it. |
| Tests | Move with their subjects; each file carries its own `mod tests`; shared helpers live in `#[cfg(test)] client/test_kit.rs`. |
| The reach mutex | **Kept, demoted**: the sharing is real (serve task + concurrent dial futures write one fact table, by design — an inbound connection is reach evidence). Rejected: single-writer/channels — the send side isn't a task (`join_all` fan-out), `dial_budget` needs synchronous read-your-writes at send start, and a channel adds an owner task + lag to express the same map. The fix is **naming**: `ReachLedger`, lock private to one ~100-line module, verb interface, poisoned-lock policy decided once. |
| Time & I/O in the ledger | Neither: every method takes `now` as data (the transport-port rule applied to state — keeps it unit-testable with fabricated timestamps, no doubles), and persistence stays with the caller (`restore` takes rows, `unreachable_snapshot` returns rows). |
| The queried mutex | Same medicine, smaller dose: `AskedOnce`, private to `who_is.rs`, one atomic test-and-set method. It exists because `Client` sits behind `Arc` at the edges and drains can overlap — the mutex is right, the raw `Mutex<BTreeSet<([u8;32],[u8;32])>>` field wasn't. Stays in-memory on purpose (the manual trigger re-asks). |
| Principle to graduate | **Share handles, not collections**: shared mutability isn't the smell — anonymous shared mutability is. A crate-visible `Arc<Mutex<Collection>>` alias hands every holder the invariants; a named cheap-clone handle (as `ClientState` already is) owns them. → STYLE.md at close. |
| Comment pruning | Rides along per project 3 §8 — the split rewrites every file, so prune then; graduation rules in §4. |
| `app/ui` | In scope as the final carve (the project 3 parking note named both files), view-only, by screen. |
| Sequencing | Ledger first (M1): it shrinks both `client.rs` and `sync.rs`, kills the upward import and the alias, and the send/outbox carve (M3) then moves clean call sites instead of raw lock code. |

## 8. Follow-ups / parked

- **Mid-run wall-rewind exposure** (inherited from project 3 §8): in-run
  reach ages still `saturating_sub` against live wall time; whether in-run
  evidence should be monotonic (an instant or both stamps in `Reach`) is a
  behavior decision. After M1 it's a one-module change — decide it there,
  as its own slice, when scheduled.
- **`now_ms` free-function home**: the P1 deferral (un-asserted wall reads
  in `state.rs` first-seen and `sync.rs` reach-seen) survives this project;
  after M1 the reach half takes `now` as a parameter, but the handler still
  produces the reading. Thread `W` into `SyncHandler` only when a test
  first needs to drive it.
- **Lazy endpoint bind** (fast-failure.md F5) — still unscheduled, CLI-only.
