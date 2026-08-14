# Transport ports

Status: **designed 2026-08-13 (P3); iroh adapter 2026-08-13 (P4); double kit
2026-08-14 (P5); loopback + e2e migrations 2026-08-14 (P6).** Project
[3-ports-and-adapters](../projects/3-ports-and-adapters/tracker.md).
Companion to the client clock ports (`zink-client/src/ports/clock.rs`).

## 1. Why

`zink-client`'s domain logic — fan-out, reachability-by-trying, delivery,
drains, who-is — reaches iroh directly, so every failure-path test needs a real
endpoint, a deliberately dead network, and real time; a production env-var
back-channel (`ZINK_*_MS`) papers over the missing seam. These ports put the
frame/blob transport behind traits so domain logic is tested in-process,
deterministically, with doubles that produce on command what a real network
produces only by accident: a peer going offline mid-send, a relay flapping, a
dial that outlives its deadline and *then* completes.

The stable artifact is each trait plus its **contract** (§3) — what a caller
may and may not assume. Doubles are small, per-scenario, and composable (§7);
a full-behavior fake of iroh would drift and lie, and is rejected outright
(tracker §4).

## 2. The ports

Three decisions shape the surface:

- **Frame-level, not stream-level.** Every mailbox/sync exchange the client
  makes is one framed BORSH request and one length-capped response per
  bi-stream; the one long-lived connection (`subscribe`) additionally receives
  one-way nudge frames. The ports expose exactly that. Raw byte streams would
  expose mechanics no call site uses and invite doubles to fake QUIC.
- **Blob transfer at intent level.** `push_blobs`/`fetch_encrypted` hand the
  connection to the iroh-blobs crate; re-modelling its bitfield protocol in
  doubles would be the emulator trap by the back door. The port states the
  domain's intents — "durably at the remote", "bytes hash to `hash`" — and the
  mechanics stay in the adapter.
- **Pull-style inbound.** `accept()` yields `Inbound { peer, frame, reply }`
  into a domain-owned serve loop; today's `Router`/`ProtocolHandler` plumbing
  becomes adapter internals feeding it. The double is a channel a test feeds,
  and the serve loop becomes visible domain code. If P4 finds the `Router`
  fighting the pull surface, the fallback is push-style registration — a
  contract change, revisited here first.

Ports in `zink-client/src/ports/transport.rs`; the iroh adapter in
`adapters/iroh.rs` (P4); doubles in `ports/transport/test_transport.rs`
behind `#[cfg(test)]` (P5) — doubles are each port's contract kit and live
with it, not with the real-world adapters. The `ports`/`adapters` split
carries the dependency rule in the module tree: **nothing outside `adapters/`
names an iroh type** (a one-glob audit), and no iroh type appears in any port
signature. Async methods are RPITIT (`impl Future + Send`), as in
`ports/clock.rs`.

```rust
/// The full network capability, as one bound for `Client`'s type parameter.
/// Blanket-implemented — nothing implements it by name; helpers take the
/// narrowest verb they exercise.
pub trait Transport:
    Dial + DialBlobs + Accept + Home + InsertRelay + RemoveRelay + Close + Clone {}
impl<T: Dial + DialBlobs + Accept + Home + InsertRelay + RemoveRelay + Close + Clone>
    Transport for T {}

pub trait Dial: Send + Sync + 'static {
    type Conn: Request + AcceptUni;
    /// May stay pending arbitrarily long — race it against a Clock deadline.
    /// A returned connection is to `to.key`, authenticated by the handshake.
    fn dial(&self, to: &Peer, alpn: &[u8])
        -> impl Future<Output = Result<Self::Conn, DialError>> + Send;
}

/// One framed request, one length-capped framed response.
pub trait Request: Send + Sync + 'static {
    fn request(&self, frame: &[u8], max_response: usize)
        -> impl Future<Output = Result<Vec<u8>, ConnError>> + Send;
}

/// Unsolicited one-way frames from the remote (the nudge path).
pub trait AcceptUni: Send + Sync + 'static {
    fn accept_uni(&self, max: usize)
        -> impl Future<Output = Result<Vec<u8>, ConnError>> + Send;
}

pub trait DialBlobs: Send + Sync + 'static {
    type Conn: PushBlob + FetchBlob;
    fn dial_blobs(&self, to: &Peer)
        -> impl Future<Output = Result<Self::Conn, DialError>> + Send;
}

pub trait PushBlob: Send + Sync + 'static {
    /// Resolves only once the remote durably holds the blob.
    fn push(&self, blob: &EncryptedBlob)
        -> impl Future<Output = Result<(), ConnError>> + Send;
}

pub trait FetchBlob: Send + Sync + 'static {
    /// Success means the returned bytes hash to `hash`.
    fn fetch(&self, hash: &BlobHash)
        -> impl Future<Output = Result<Vec<u8>, ConnError>> + Send;
}

pub trait Accept: Send + Sync + 'static {
    type Reply: Respond;
    /// Next inbound request from any peer; None once the endpoint closes.
    fn accept(&self) -> impl Future<Output = Option<Inbound<Self::Reply>>> + Send;
}

pub trait Respond: Send + 'static {
    fn respond(self, frame: &[u8])
        -> impl Future<Output = Result<(), ConnError>> + Send;
}

/// To home — attach to a home relay, the transition that makes this endpoint
/// reachable by key.
pub trait Home: Send + Sync + 'static {
    /// Resolves when a home relay connection is up. May NEVER resolve.
    fn online(&self) -> impl Future<Output = ()> + Send;
}

pub trait InsertRelay: Send + Sync + 'static {
    fn insert_relay(&self, url: &str)
        -> impl Future<Output = Result<(), InvalidRelayUrl>> + Send;
}

pub trait RemoveRelay: Send + Sync + 'static {
    fn remove_relay(&self, url: &str) -> impl Future<Output = ()> + Send;
}

pub trait Close: Send + Sync + 'static {
    /// Graceful drain; the caller races it against a deadline.
    fn close(&self) -> impl Future<Output = ()> + Send;
}

/// A peer: the key that *is* its identity, plus fallible route hints for
/// reaching it, filled from contact/device records. Plain data; the adapter
/// maps it to iroh addressing. The handshake pins the key — a stale hint can
/// slow or fail a dial, never redirect it.
pub struct Peer {
    pub key: PublicKey,           // zink-protocol type — the only identifier
    pub relays: Vec<String>,      // relay URLs as recorded
    pub sockets: Vec<SocketAddr>, // explicit ip:port hints (dial strings)
}

/// An inbound request. `peer` is authenticated by the transport handshake.
pub struct Inbound<R> {
    pub peer: PublicKey,
    pub frame: Vec<u8>,
    pub reply: R,
}
```

`Clone` is on the aggregate because background tasks (the serve loop,
`subscribe`) need owned handles; the adapter wraps `Arc`s, so it is cheap.
`DialError`/`ConnError` carry an opaque message for logs and deliberately have
**no variants**: the domain maps failures structurally — dial errors to
`Error::Unreachable`, connection-op errors to `Error::Transport`, by which
port call failed — and never branches on taxonomy. A variant is added only
when domain logic branches on it, never speculatively.

The sketch binds the **shape and the contract**. P4 may refine mechanics —
error plumbing, `&str` vs a validated relay-URL newtype — freely; anything
touching a contract line in §3 comes back here first.

## 3. The contract, per capability

The load-bearing half of each entry is what a caller may **not** assume — the
port promises only what iroh actually promises (tracker §4).

**`Dial::dial`** — may assume: a returned `Conn` is to `to.key` (the handshake
proves key possession — the outbound mirror of `Inbound.peer`) and was
established at that moment; concurrent dials proceed independently. May *not*
assume: bounded time
(no internal timeout — §4); that an offline peer errors rather than hangs
(reality does both); that a successful dial implies requests will succeed;
anything about the path taken (direct vs relay-mediated). An error means "no
connection", nothing more.

**`Request::request`** — may assume: a response is the remote's reply to
*this* request (stream isolation); responses over `max_response` fail. May
*not* assume: bounded time; **anything, from an error, about whether the
remote received or processed the request** — the asymmetry that deposit
idempotency and `deposit_with_retry` exist for; ordering across concurrent
requests; that the connection is usable after an error.

**`AcceptUni::accept_uni`** — may assume: frames can arrive at any moment,
including between calls (buffered within transport limits). May *not* assume:
that every frame the remote sent is observed — one-way frames are coalescible
best-effort signals, and anything needing reliability uses `request`. Nor that
frames are empty (today's nudges are; the port doesn't promise it).

**`PushBlob::push`** — `Ok` means the remote durably holds the blob. May *not*
assume bounded time, or any knowledge of the remote's state after an error
(possibly a partial blob).

**`FetchBlob::fetch`** — success means the bytes hash to `hash` —
content-addressing is a promise the real stack keeps (iroh-blobs verifies), so
the port states it and doubles must honor it. No availability, no bounded
time.

**`Accept::accept`** — may assume: `peer` is authenticated (the handshake
proves key possession — load-bearing, because Deliver/GetKeys gate on caller
identity). May *not* assume: fairness or ordering across peers; that the peer
is still there; bounded time between requests. `None` means the endpoint
closed, nothing else.

**`Respond::respond`** — the same asymmetry as `request`: an error says
nothing about what the peer saw; success means sent, not processed.

**`Home::online`** — resolution means a home relay connection was up *at that
moment* — a fact, not a lease. It may never resolve (no relay configured,
relay down — the reason `Reachable::NotYet` exists), so callers always race a
Clock deadline. `NoHomeRelay` is decided by the domain from the profile and
never asks the port.

**`InsertRelay`/`RemoveRelay`** — affect future dials and homing; no promise
about existing connections. `insert_relay` fails only on an unparseable URL;
`remove_relay` cannot fail (a URL that never parsed was never inserted).

**`Close::close`** — best-effort drain, arbitrarily slow; race it against a
deadline. After close, port operations fail and `accept` yields `None`.

## 4. Time is never inside the port

No port method takes a `Duration` (`max_response` is bytes, not time) and no
adapter starts a timer: every deadline is a domain-side `clock.timeout(…)`
race. This is the rule that composes the two ports into deterministic tests —
a double holds an operation pending, `TestClock::advance` fires the domain's
deadline, the fallback is asserted, the double releases, recovery is asserted.
A timeout inside the adapter would be invisible to `TestClock`. It also keeps
timeout policy (`connect_timeout`, `direct_budget` caps) in unit-tested domain
code, and preserves today's semantics: deadlines bound the *dial*, not a
slow-but-progressing blob transfer — which is why `DialBlobs` is a separate
dial rather than a timeout-wrapped whole-transfer call.

## 5. Seam placement

Below fan-out, reachability, and delivery decisions; nothing enters
`zink-protocol`.

**Domain (in-process tested):** `direct_budget`; the `Reach` map and
`unreachable.keys` persistence; fan-out orchestration and relay-discharge;
`deposit_with_retry`'s retry policy (each attempt a fresh `dial` — retry is
policy, not transport); drain loops and cross-relay dedup; `subscribe`'s
backoff loop; all BORSH encode/decode including hostile input (size *caps* are
enforced by the adapter via `max_response`, using the domain's `MAX_*_BYTES`
constants — the inbound-request cap is fixed at adapter construction;
*parsing* stays domain and keeps its never-panic tests); every
deadline race; the serve loop over `Accept`, spawning per `Inbound` to keep
cross-connection concurrency.

**Adapter (`IrohTransport` wrapping `Endpoint` + `Router`):** `bind_endpoint`
and endpoint config; `EndpointAddr` construction and relay-URL parsing;
`open_bi`/`write_all`/`finish`/`read_to_end` mechanics; the `Router` plumbing
feeding `accept()`; iroh-blobs staging, push, fetch, observe-until-complete.

Local addressing (`endpoint.addr()`, used only to print CLI dial strings) is
not a port — no domain logic branches on it, no double needs it. It becomes an
inherent method on `IrohTransport`, surfaced through a production-only
`impl<C, W> Client<C, W, IrohTransport>` block, like the production
constructors. `net.rs` splits along the seam: `request`, `sync_request`,
`deposit_with_retry` become domain helpers over `&impl Request`/`&impl Dial`;
the connect mechanics become adapter internals.

## 6. Injection

```rust
pub struct Client<C: Clock = SystemClock, W: WallClock = SystemClock,
                  N: Transport = IrohTransport> { … }
```

One transport parameter, added last so existing `Client<TestClock, …>`
spellings don't shift; edges keep writing bare `Client`. One parameter where
the clocks got two: dial/blobs/accept/homing are facets of one endpoint, and
their doubles usually share state (what one side dials into, the other
accepts). Narrowness lives in helper signatures instead — each takes exactly
the capability it exercises (`sync_request(conn: &impl Request, …)`,
`deposit_with_retry(net: &impl Dial, …)`,
`await_reachable(home: &impl Home, clock: &impl Clock, …)`).

## 7. The double kit

Small, per-capability, composable — never a network model. Landed in P5 as
`ports/transport/test_transport.rs`:

1. **One double per capability**, composed through `TestTransport
   { dial, blobs, accept, home }` (one `Clone`-shared handle scripts and
   inspects everything). The kit's reach stops at `Client`-level tests:
   helpers take the narrowest verb, so a helper test with odd needs
   hand-rolls a five-line double rather than growing the kit. Per-test
   variation must stay in the *scripts* (data); a variant that needs to
   inspect a request to choose its reply is the double reimplementing
   protocol logic — that test gets a bespoke double instead.
2. **Controls, not simulation**: hold (never resolves — the caller's deadline
   drops it), connect-after-hold (the next attempt succeeds: "down, then came
   back"), and the loopback (below). No latency or loss models — the test is
   the scenario. Refuse/fail/kill controls were built in P5 and **deleted at
   P6's close unexercised** — the standing rule: a control that no migrated
   test drives comes out, and returns with the first test that needs it.
3. **Doubles speak real frames**: scripted replies are exact
   `MailboxResponse`/`SyncResponse` BORSH bytes built with `zink-protocol`
   (pure, available to tests). Doubles script *which* frame comes back; they
   never inspect requests or reimplement protocol logic.
4. **Honest defaults, never silent success**: remote-initiated capabilities
   default to *silence* — `accept`, `accept_uni` and `online()` pend until
   the test acts, exactly as a quiet network would — while an **unscripted
   domain-initiated action panics** (a returned error would vanish into the
   domain's best-effort handling; silence would hang the test instead of
   failing it). Caveat: a panic fails loudly only while transport calls run
   in the test's own task tree — true today (fan-out is in-task `join_all`;
   the serve loop never dials). A migration that puts dials inside a
   *spawned* task (the subscribe loop, if it ever migrates) turns that panic
   into a silently aborted task and a hanging test: script everything such a
   task touches, or add an erroring control first.
5. **Two-client wiring is wiring** — landed in P6 as `Loopback`: a dial to a
   registered key yields a connection whose requests land in that client's
   accept queue as `Inbound { peer: caller, … }`, so both ends run their real
   handlers (the D5 gate, verification, storage, real acks). Scripts take
   precedence over wiring — holding a wired key is how a loopback peer goes
   offline. The registry resolves keys and moves frames, nothing more; the
   moment it grows behavior (ordering, timing, loss), it has become the
   forbidden simulator. Peers a client has no trust path to (a stranger's
   direct push is declined) receive via scripted mailbox conns instead, with
   the test shuttling deposited envelopes into drains — the test IS the
   relay's storage, visibly.

The kit: `ScriptedDial` (per-key connect/hold queues, plus a `dialed` counter
— `dialed == 0` is the assertion that evidence suppressed a dial entirely),
`ScriptedConn` (exact-frame replies, a sent-request recorder),
`Loopback`/`LoopConn` (the wiring), `ChannelAccept` (inject an inbound
request, await the served response), `TestHome` (pends — the online smoke
stays real-network), `ScriptedDialBlobs` (panics until the first blob
migration). The archetype (tracker §4), landed as
`delivery__should_recover_when_a_dead_relay_returns`, 7 ms:

```rust
// Given — the relay silent, a client on a TestClock
net.dial.hold(&relay);
// When — the send's deadline parks, one advance fires it
let (result, ()) = tokio::join!(a.send(&recipients, msg, vec![]), async {
    clock.wait_for_sleepers(1).await;
    clock.advance(DEADLINE);
});
// Then — fallback observed…
assert!(matches!(result, Err(Error::AllRelaysPending(_))));
// …and the relay returns: the next dial connects, the deposit is taken
net.dial.connect(&relay).reply(deposited_frame());
assert_eq!(a.flush_outbox().await.expect("flush").delivered, 1);
```

## 8. What stays real

A thin, explicitly-labelled smoke tier (P7) stays on real iroh for what only a
real network proves: handshake/ALPN against a real relay, holepunch and
relay-fallback, `Endpoint::online()` homing, iroh-blobs streaming, the
graceful-close drain. P4 proves the adapter first by passing the *existing*
suite through it before any test migrates.
