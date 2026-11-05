//! Transport doubles: per-capability controls, composed per scenario
//! (`docs/design/transport.md` §7). Controls, not simulation — a test holds
//! dials, scripts exact frames, or wires two clients together; nothing here
//! models a network. Scripted replies are real BORSH frames built with
//! `zink-protocol`.
//!
//! Honest defaults, never silent success: remote-initiated capabilities
//! default to *silence* (`accept`/`accept_uni`/`online` pend until the test
//! acts — the serve loop parks exactly as it would on a quiet network),
//! while an **unscripted domain-initiated action panics** (a dial or request
//! the test didn't script is either a test bug or a code bug, and a returned
//! error would vanish into the domain's best-effort handling). Caveat: the
//! panic fails loudly only while transport calls run in the test's own task
//! tree — see §7 before migrating anything that dials from a spawned task.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use zink_protocol::{BlobHash, EncryptedBlob, PublicKey};

use super::{
    Accept, AcceptBlobs, AcceptUni, Admit, BlobEvent, Close, ConnError, Dial, DialBlobs, DialError,
    FetchBlob, Home, Inbound, InsertRelay, InvalidRelayUrl, Peer, PushBlob, RemoveRelay, Request,
    Respond,
};
use crate::hex;

/// The composition root: one double per capability, all sharing state across
/// clones (the test keeps a clone as its control handle).
#[derive(Clone)]
pub(crate) struct TestTransport {
    pub dial: ScriptedDial,
    pub blobs: TestBlobs,
    pub accept: ChannelAccept,
    /// Inbound blob events — what a wired peer's blob dial lands in, or
    /// what a test injects directly.
    pub blob_accept: ChannelBlobs,
    pub home: TestHome,
    /// Every domain-initiated wire act, in the order it happened, across
    /// scripted dials, wired peers and blob caches alike — the one place
    /// an *ordering* claim ("the blobs landed before the deposit") can be
    /// asserted.
    pub log: WireLog,
    /// The relay set as `insert_relay`/`remove_relay` left it — keyed by
    /// URL like the real endpoint's map, so re-inserting is a no-op.
    relays: Arc<Mutex<Vec<String>>>,
    /// Set by `Loopback::transport`: this transport's own key, and the
    /// registry that resolves dials to other wired clients.
    wiring: Option<(PublicKey, Loopback)>,
}

impl TestTransport {
    pub(crate) fn new() -> Self {
        let log = WireLog::default();
        Self {
            dial: ScriptedDial::default(),
            blobs: TestBlobs::new(log.clone()),
            accept: ChannelAccept::new(),
            blob_accept: ChannelBlobs::new(),
            home: TestHome,
            log,
            relays: Arc::new(Mutex::new(Vec::new())),
            wiring: None,
        }
    }
}

impl Dial for TestTransport {
    type Conn = TestConn;
    /// Scripts take precedence over wiring — enqueueing a `hold` for a wired
    /// key is how a loopback peer goes offline for the next dial.
    async fn dial(&self, to: &Peer, _alpn: &[u8]) -> Result<TestConn, DialError> {
        let logged = |link| TestConn {
            to: to.key,
            log: self.log.clone(),
            link,
        };
        if let Some(script) = self.dial.take(&to.key) {
            return ScriptedDial::resolve(script)
                .await
                .map(|conn| logged(Link::Scripted(conn)));
        }
        if let Some((me, loopback)) = &self.wiring
            && let Some((target, _)) = loopback.target(&to.key)
        {
            return Ok(logged(Link::Routed(LoopConn {
                caller: *me,
                target,
            })));
        }
        panic!("unscripted dial to {}", hex::encode(&to.key.0));
    }
}

impl DialBlobs for TestTransport {
    type Conn = TestBlobConn;
    /// Scripted caches first (a relay the test stood up), then wiring (a
    /// peer client whose real gate answers), then the loud default.
    async fn dial_blobs(&self, to: &Peer) -> Result<TestBlobConn, DialError> {
        if self.blobs.scripted(&to.key) {
            return self.blobs.dial_blobs(to).await;
        }
        if let Some((me, loopback)) = &self.wiring
            && let Some((_, target)) = loopback.target(&to.key)
        {
            let admitted = target.inject_connect(*me).await.unwrap_or(false);
            return Ok(TestBlobConn::Routed(LoopBlobConn {
                caller: *me,
                to: to.key,
                target,
                admitted,
                log: self.log.clone(),
            }));
        }
        panic!("unscripted blob dial to {}", hex::encode(&to.key.0));
    }
}

impl AcceptBlobs for TestTransport {
    type Admit = TestAdmit;
    async fn accept_blob(&self) -> Option<BlobEvent<TestAdmit>> {
        self.blob_accept.accept_blob().await
    }
}

impl Accept for TestTransport {
    type Reply = TestReply;
    async fn accept(&self) -> Option<Inbound<TestReply>> {
        self.accept.accept().await
    }
}

impl Home for TestTransport {
    fn online(&self) -> impl std::future::Future<Output = ()> + Send {
        self.home.online()
    }
}

impl InsertRelay for TestTransport {
    async fn insert_relay(&self, url: &str) -> Result<(), InvalidRelayUrl> {
        let mut relays = self.relays.lock().expect("relays lock");
        if !relays.iter().any(|held| held == url) {
            relays.push(url.to_string());
        }
        Ok(())
    }
}

impl RemoveRelay for TestTransport {
    async fn remove_relay(&self, url: &str) {
        self.relays
            .lock()
            .expect("relays lock")
            .retain(|held| held != url);
    }
}

impl Close for TestTransport {
    /// Closing doubles drains nothing — trivially graceful.
    async fn close(&self) {}
}

/// Dial outcomes scripted per peer key, consumed in order per attempt.
/// "Held then connected" is two entries: the first attempt hangs until the
/// caller's deadline drops it, the next attempt gets the connection — which
/// is exactly how a peer that was down and came back looks.
#[derive(Clone, Default)]
pub(crate) struct ScriptedDial {
    scripts: Arc<Mutex<BTreeMap<[u8; 32], VecDeque<DialScript>>>>,
    attempts: Arc<Mutex<BTreeMap<[u8; 32], usize>>>,
}

enum DialScript {
    Connect(ScriptedConn),
    Hold,
}

impl ScriptedDial {
    /// The next dial to `key` succeeds; script the returned conn's replies.
    pub(crate) fn connect(&self, key: &PublicKey) -> ScriptedConn {
        let conn = ScriptedConn::new();
        self.enqueue(key, DialScript::Connect(conn.clone()));
        conn
    }

    /// The next dial to `key` hangs until the caller's deadline drops it.
    pub(crate) fn hold(&self, key: &PublicKey) {
        self.enqueue(key, DialScript::Hold);
    }

    /// How many times `key` was dialed — 0 is the assertion that evidence
    /// suppressed the dial entirely.
    pub(crate) fn dialed(&self, key: &PublicKey) -> usize {
        self.attempts
            .lock()
            .expect("attempts lock")
            .get(&key.0)
            .copied()
            .unwrap_or(0)
    }

    fn enqueue(&self, key: &PublicKey, script: DialScript) {
        self.scripts
            .lock()
            .expect("scripts lock")
            .entry(key.0)
            .or_default()
            .push_back(script);
    }

    /// Record an attempt and pop the next script for `key`, if any.
    fn take(&self, key: &PublicKey) -> Option<DialScript> {
        *self
            .attempts
            .lock()
            .expect("attempts lock")
            .entry(key.0)
            .or_default() += 1;
        self.scripts
            .lock()
            .expect("scripts lock")
            .get_mut(&key.0)
            .and_then(VecDeque::pop_front)
    }

    /// Play one script out — `Hold` pends until the caller's deadline
    /// drops the dial.
    async fn resolve(script: DialScript) -> Result<ScriptedConn, DialError> {
        match script {
            DialScript::Connect(conn) => Ok(conn),
            DialScript::Hold => std::future::pending().await,
        }
    }
}

impl Dial for ScriptedDial {
    type Conn = ScriptedConn;

    async fn dial(&self, to: &Peer, _alpn: &[u8]) -> Result<ScriptedConn, DialError> {
        match self.take(&to.key) {
            Some(script) => Self::resolve(script).await,
            None => panic!("unscripted dial to {}", hex::encode(&to.key.0)),
        }
    }
}

/// A connection a `TestTransport` produced — scripted (the test answers) or
/// routed (another wired client's real handler answers) — that logs each
/// request to the transport's `WireLog` before it goes out.
pub(crate) struct TestConn {
    to: PublicKey,
    log: WireLog,
    link: Link,
}

enum Link {
    Scripted(ScriptedConn),
    Routed(LoopConn),
}

impl Request for TestConn {
    async fn request(&self, frame: &[u8], max_response: usize) -> Result<Vec<u8>, ConnError> {
        self.log.record(WireEvent::Request {
            to: self.to,
            frame: frame.to_vec(),
        });
        match &self.link {
            Link::Scripted(conn) => conn.request(frame, max_response).await,
            Link::Routed(conn) => conn.request(frame, max_response).await,
        }
    }
}

impl AcceptUni for TestConn {
    async fn accept_uni(&self, max: usize) -> Result<Vec<u8>, ConnError> {
        match &self.link {
            Link::Scripted(conn) => conn.accept_uni(max).await,
            Link::Routed(conn) => conn.accept_uni(max).await,
        }
    }
}

/// The ordered record of what the domain put on the wire. Frames are kept
/// raw — the log never inspects them; a test decodes what it asserts on.
#[derive(Clone, Default)]
pub(crate) struct WireLog(Arc<Mutex<Vec<WireEvent>>>);

#[derive(Clone)]
pub(crate) enum WireEvent {
    /// A framed request sent to `to` (mailbox or sync ALPN alike).
    Request { to: PublicKey, frame: Vec<u8> },
    /// A blob push to `to` *confirmed* — the point `PushBlob::push` resolves.
    Pushed { to: PublicKey, hash: BlobHash },
}

/// Short form for assertion messages: a frame is its length, a key its
/// first bytes — a failing ordering claim should read as a timeline.
impl std::fmt::Debug for WireEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireEvent::Request { to, frame } => {
                write!(
                    f,
                    "Request(to {}…, {} bytes)",
                    &hex::encode(&to.0)[..8],
                    frame.len()
                )
            }
            WireEvent::Pushed { to, hash } => write!(
                f,
                "Pushed(to {}…, blob {}…)",
                &hex::encode(&to.0)[..8],
                &hex::encode(&hash.0)[..8]
            ),
        }
    }
}

impl WireLog {
    fn record(&self, event: WireEvent) {
        self.0.lock().expect("wire log lock").push(event);
    }

    /// Everything so far, oldest first.
    pub(crate) fn events(&self) -> Vec<WireEvent> {
        self.0.lock().expect("wire log lock").clone()
    }
}

/// Two in-process clients talking is *wiring*, not simulation
/// (transport.md §7): a dial to a registered key yields a connection whose
/// requests land in that client's accept queue — both ends run their real
/// domain logic. Unregistered keys still need scripts.
#[derive(Clone, Default)]
pub(crate) struct Loopback(Arc<Mutex<BTreeMap<[u8; 32], Wired>>>);

/// One wired client's two inbound queues: sync requests and blob events.
type Wired = (ChannelAccept, ChannelBlobs);

impl Loopback {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A transport wired into this loopback as `key` — what the client
    /// built on it can be dialed as, and dials others through.
    pub(crate) fn transport(&self, key: PublicKey) -> TestTransport {
        let transport = TestTransport::new();
        self.0.lock().expect("loopback lock").insert(
            key.0,
            (transport.accept.clone(), transport.blob_accept.clone()),
        );
        TestTransport {
            wiring: Some((key, self.clone())),
            ..transport
        }
    }

    fn target(&self, key: &PublicKey) -> Option<Wired> {
        self.0.lock().expect("loopback lock").get(&key.0).cloned()
    }
}

/// One wired connection: each request lands in the target's accept queue as
/// `Inbound { peer: caller, … }` — the identity a real handshake would have
/// authenticated — and the handler's response comes back as the reply.
pub(crate) struct LoopConn {
    caller: PublicKey,
    target: ChannelAccept,
}

impl Request for LoopConn {
    async fn request(&self, frame: &[u8], max_response: usize) -> Result<Vec<u8>, ConnError> {
        let response = self
            .target
            .inject(self.caller, frame.to_vec())
            .await
            .map_err(|_| ConnError("peer dropped the request".to_string()))?;
        if response.len() > max_response {
            return Err(ConnError("response over max_response".to_string()));
        }
        Ok(response)
    }
}

impl AcceptUni for LoopConn {
    /// Peers don't nudge; silence is the honest default.
    async fn accept_uni(&self, _max: usize) -> Result<Vec<u8>, ConnError> {
        std::future::pending().await
    }
}

/// A scripted connection: framed replies consumed in order per request.
/// Frames are exact bytes — the double never inspects what it answers.
#[derive(Clone)]
pub(crate) struct ScriptedConn {
    replies: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Every request frame sent, for asserting what went on the wire.
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl ScriptedConn {
    pub(crate) fn new() -> Self {
        Self {
            replies: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Answer the next request with these exact frame bytes.
    pub(crate) fn reply(&self, frame: Vec<u8>) -> &Self {
        self.replies.lock().expect("replies lock").push_back(frame);
        self
    }

    /// The request frames sent so far.
    pub(crate) fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Request for ScriptedConn {
    async fn request(&self, frame: &[u8], max_response: usize) -> Result<Vec<u8>, ConnError> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(frame.to_vec());
        let reply = self.replies.lock().expect("replies lock").pop_front();
        match reply {
            Some(bytes) if bytes.len() <= max_response => Ok(bytes),
            Some(_) => Err(ConnError("response over max_response".to_string())),
            None => panic!("unscripted request (frame {} bytes)", frame.len()),
        }
    }
}

impl AcceptUni for ScriptedConn {
    /// No migrated test feeds nudges yet — a scripted connection is silent.
    async fn accept_uni(&self, _max: usize) -> Result<Vec<u8>, ConnError> {
        std::future::pending().await
    }
}

/// Blob caches, one per scripted key: `cache(&key)` says a cache answers
/// at `key` from now on — pushes are held (and logged) and fetches serve
/// what is held, exactly the relay contract; `refuse(&key)` one that
/// connects but takes nothing. A blob dial to a key no test scripted and
/// no loopback wires is unscripted — loudly, like a dial.
#[derive(Clone)]
pub(crate) struct TestBlobs {
    caches: Arc<Mutex<BTreeMap<[u8; 32], Held>>>,
    /// Caches that connect but take nothing — full, or declining us; on
    /// the wire the two look the same.
    refusing: Arc<Mutex<std::collections::BTreeSet<[u8; 32]>>>,
    log: WireLog,
}

type Held = Arc<Mutex<BTreeMap<[u8; 32], Vec<u8>>>>;

impl TestBlobs {
    fn new(log: WireLog) -> Self {
        Self {
            caches: Arc::new(Mutex::new(BTreeMap::new())),
            refusing: Arc::new(Mutex::new(Default::default())),
            log,
        }
    }

    /// A reachable blob cache at `key` that refuses every push — a full
    /// cache, with no deadline in the way.
    pub(crate) fn refuse(&self, key: &PublicKey) -> &Self {
        self.cache(key);
        self.refusing.lock().expect("refusing lock").insert(key.0);
        self
    }

    /// A reachable blob cache at `key`, empty until something is pushed.
    pub(crate) fn cache(&self, key: &PublicKey) -> &Self {
        self.caches
            .lock()
            .expect("caches lock")
            .entry(key.0)
            .or_default();
        self
    }

    /// Whether a test scripted a cache at `key`.
    fn scripted(&self, key: &PublicKey) -> bool {
        self.caches
            .lock()
            .expect("caches lock")
            .contains_key(&key.0)
    }

    /// The hashes the cache at `key` holds, in hash order.
    pub(crate) fn held(&self, key: &PublicKey) -> Vec<BlobHash> {
        self.caches
            .lock()
            .expect("caches lock")
            .get(&key.0)
            .map(|held| {
                held.lock()
                    .expect("held lock")
                    .keys()
                    .map(|hash| BlobHash(*hash))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl DialBlobs for TestBlobs {
    type Conn = TestBlobConn;
    async fn dial_blobs(&self, to: &Peer) -> Result<TestBlobConn, DialError> {
        let held = self
            .caches
            .lock()
            .expect("caches lock")
            .get(&to.key.0)
            .cloned();
        let refusing = self
            .refusing
            .lock()
            .expect("refusing lock")
            .contains(&to.key.0);
        match held {
            Some(held) => Ok(TestBlobConn::Cache(CacheConn {
                to: to.key,
                held,
                refusing,
                log: self.log.clone(),
            })),
            None => panic!("unscripted blob dial to {}", hex::encode(&to.key.0)),
        }
    }
}

/// A blob connection a `TestTransport` produced: to a scripted cache, or
/// routed to a wired peer whose real gate admitted (or declined) us.
pub(crate) enum TestBlobConn {
    Cache(CacheConn),
    Routed(LoopBlobConn),
}

impl PushBlob for TestBlobConn {
    async fn push(&self, blob: &EncryptedBlob) -> Result<(), ConnError> {
        match self {
            TestBlobConn::Cache(conn) => conn.push(blob).await,
            TestBlobConn::Routed(conn) => conn.push(blob).await,
        }
    }
}

impl FetchBlob for TestBlobConn {
    async fn fetch(&self, hash: &BlobHash) -> Result<Vec<u8>, ConnError> {
        match self {
            TestBlobConn::Cache(conn) => conn.fetch(hash).await,
            TestBlobConn::Routed(_) => Err(ConnError("peers serve no blobs".to_string())),
        }
    }
}

/// A connection to one scripted blob cache.
pub(crate) struct CacheConn {
    to: PublicKey,
    held: Held,
    refusing: bool,
    log: WireLog,
}

impl PushBlob for CacheConn {
    async fn push(&self, blob: &EncryptedBlob) -> Result<(), ConnError> {
        if self.refusing {
            return Err(ConnError("cache refused the push".to_string()));
        }
        self.held
            .lock()
            .expect("held lock")
            .insert(blob.hash.0, blob.bytes.clone());
        self.log.record(WireEvent::Pushed {
            to: self.to,
            hash: blob.hash,
        });
        Ok(())
    }
}

impl FetchBlob for CacheConn {
    async fn fetch(&self, hash: &BlobHash) -> Result<Vec<u8>, ConnError> {
        self.held
            .lock()
            .expect("held lock")
            .get(&hash.0)
            .cloned()
            .ok_or_else(|| ConnError("blob not held".to_string()))
    }
}

/// A wired blob connection to another client: its gate answered the
/// admission at dial time; each push lands in its blob queue as `Pushed`
/// (the real adapter reports a push the same way, after the transfer), and
/// dropping the connection reports `Closed` — exactly as the provider does
/// for an admitted connection, and not at all for a declined one.
pub(crate) struct LoopBlobConn {
    caller: PublicKey,
    to: PublicKey,
    target: ChannelBlobs,
    admitted: bool,
    log: WireLog,
}

impl PushBlob for LoopBlobConn {
    async fn push(&self, blob: &EncryptedBlob) -> Result<(), ConnError> {
        if !self.admitted {
            return Err(ConnError("peer declined the blob connection".to_string()));
        }
        self.target.inject_push(self.caller, blob.clone());
        self.log.record(WireEvent::Pushed {
            to: self.to,
            hash: blob.hash,
        });
        Ok(())
    }
}

impl Drop for LoopBlobConn {
    fn drop(&mut self) {
        if self.admitted {
            self.target.inject_close(self.caller);
        }
    }
}

/// Inbound blob events as a queue the test (or a `LoopBlobConn`) feeds;
/// the blob serve loop pulls exactly as it does from the router.
#[derive(Clone)]
pub(crate) struct ChannelBlobs {
    tx: tokio::sync::mpsc::UnboundedSender<BlobEvent<TestAdmit>>,
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<BlobEvent<TestAdmit>>>>,
}

impl ChannelBlobs {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }

    /// `peer` knocks; the returned receiver carries the gate's answer.
    pub(crate) fn inject_connect(&self, peer: PublicKey) -> tokio::sync::oneshot::Receiver<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(BlobEvent::Connected {
            peer,
            admit: TestAdmit { tx },
        });
        rx
    }

    /// A blob from `peer` landed whole.
    pub(crate) fn inject_push(&self, peer: PublicKey, blob: EncryptedBlob) {
        let _ = self.tx.send(BlobEvent::Pushed { peer, blob });
    }

    /// `peer`'s admitted connection closed.
    pub(crate) fn inject_close(&self, peer: PublicKey) {
        let _ = self.tx.send(BlobEvent::Closed { peer });
    }
}

impl AcceptBlobs for ChannelBlobs {
    type Admit = TestAdmit;

    async fn accept_blob(&self) -> Option<BlobEvent<TestAdmit>> {
        // The sender half lives in this struct, so an unfed queue pends —
        // a quiet network, not a closed endpoint.
        self.rx.lock().await.recv().await
    }
}

/// The gate's answer to one injected `Connected`.
pub(crate) struct TestAdmit {
    tx: tokio::sync::oneshot::Sender<bool>,
}

impl Admit for TestAdmit {
    async fn admit(self, admitted: bool) {
        let _ = self.tx.send(admitted);
    }
}

/// Inbound requests as a queue the test (or a `LoopConn`) feeds; the serve
/// loop pulls exactly as it does from the router.
#[derive(Clone)]
pub(crate) struct ChannelAccept {
    tx: tokio::sync::mpsc::UnboundedSender<Inbound<TestReply>>,
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Inbound<TestReply>>>>,
}

impl ChannelAccept {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }

    /// One inbound request from `peer`; await the returned receiver for the
    /// response frame the handler serves.
    pub(crate) fn inject(
        &self,
        peer: PublicKey,
        frame: Vec<u8>,
    ) -> tokio::sync::oneshot::Receiver<Vec<u8>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(Inbound {
            peer,
            frame,
            reply: TestReply { tx: reply_tx },
        });
        reply_rx
    }
}

impl Accept for ChannelAccept {
    type Reply = TestReply;

    async fn accept(&self) -> Option<Inbound<TestReply>> {
        // The sender half lives in this struct, so an unfed queue pends —
        // a quiet network, not a closed endpoint.
        self.rx.lock().await.recv().await
    }
}

/// The response half of one injected request.
pub(crate) struct TestReply {
    tx: tokio::sync::oneshot::Sender<Vec<u8>>,
}

impl Respond for TestReply {
    async fn respond(self, frame: &[u8]) -> Result<(), ConnError> {
        self.tx
            .send(frame.to_vec())
            .map_err(|_| ConnError("inject receiver dropped".to_string()))
    }
}

/// Homing: `online()` pends — the truthful default for an endpoint with no
/// relay connection. A settable control arrives with the first test that
/// drives readiness (the online smoke stays real-network, P7).
#[derive(Clone, Default)]
pub(crate) struct TestHome;

impl Home for TestHome {
    fn online(&self) -> impl std::future::Future<Output = ()> + Send {
        std::future::pending()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[tokio::test]
    #[should_panic(expected = "unscripted dial")]
    async fn scripted_dial__should_panic_on_an_unscripted_dial() {
        let dial = ScriptedDial::default();
        let to = Peer {
            key: PublicKey([1; 32]),
            relays: vec![],
            sockets: vec![],
        };
        let _ = dial.dial(&to, b"alpn").await;
    }

    #[tokio::test]
    async fn scripted_conn__should_fail_replies_over_the_response_cap() {
        // Given
        let conn = ScriptedConn::new();
        conn.reply(vec![0; 32]);

        // When
        let result = conn.request(b"req", 16).await;

        // Then: the cap is the contract, doubles included
        assert!(result.is_err());
    }
}
