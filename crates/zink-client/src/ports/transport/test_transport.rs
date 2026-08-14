//! Transport doubles: per-capability controls, composed per scenario
//! (`docs/design/transport.md` §7). Controls, not simulation — a test holds,
//! releases, fails or scripts exact frames; nothing here models a network.
//! Scripted replies are real BORSH frames built with `zink-protocol`.
//!
//! Honest defaults, never silent success: remote-initiated capabilities
//! default to *silence* (`accept`/`accept_uni`/`online` pend until the test
//! acts — the serve loop and subscribe park exactly as they would on a quiet
//! network), while an **unscripted domain-initiated action panics** (a dial
//! or request the test didn't script is either a test bug or a code bug, and
//! a returned error would vanish into the domain's best-effort handling).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use zink_protocol::{BlobHash, EncryptedBlob, PublicKey};

use super::{
    Accept, AcceptUni, Close, ConnError, Dial, DialBlobs, DialError, FetchBlob, Home, Inbound,
    InsertRelay, InvalidRelayUrl, Peer, PushBlob, RemoveRelay, Request, Respond,
};
use crate::hex;

/// The composition root: one double per capability, all sharing state across
/// clones (the test keeps a clone as its control handle).
#[derive(Clone)]
pub(crate) struct TestTransport {
    pub dial: ScriptedDial,
    pub blobs: ScriptedDialBlobs,
    pub accept: ChannelAccept,
    pub home: TestHome,
    /// Relay-set changes, recorded (`insert_relay` pushes, `remove_relay`
    /// retains out) — mirrors what the real endpoint's map would hold.
    relays: Arc<Mutex<Vec<String>>>,
}

impl TestTransport {
    pub(crate) fn new() -> Self {
        Self {
            dial: ScriptedDial::default(),
            blobs: ScriptedDialBlobs::default(),
            accept: ChannelAccept::new(),
            home: TestHome::default(),
            relays: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The relay set as insert/remove left it.
    #[allow(dead_code)] // part of the kit; first asserted on in P6
    pub(crate) fn home_relays(&self) -> Vec<String> {
        self.relays.lock().expect("relays lock").clone()
    }
}

impl Dial for TestTransport {
    type Conn = TestConn;
    async fn dial(&self, to: &Peer, alpn: &[u8]) -> Result<TestConn, DialError> {
        self.dial.dial(to, alpn).await
    }
}

impl DialBlobs for TestTransport {
    type Conn = TestBlobConn;
    async fn dial_blobs(&self, to: &Peer) -> Result<TestBlobConn, DialError> {
        self.blobs.dial_blobs(to).await
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
        self.relays
            .lock()
            .expect("relays lock")
            .push(url.to_string());
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
    Connect(TestConn),
    Refuse(String),
    Hold,
}

impl ScriptedDial {
    /// The next dial to `key` succeeds; script the returned conn's replies.
    pub(crate) fn connect(&self, key: &PublicKey) -> TestConn {
        let conn = TestConn::new();
        self.enqueue(key, DialScript::Connect(conn.clone()));
        conn
    }

    /// The next dial to `key` hangs until the caller's deadline drops it.
    pub(crate) fn hold(&self, key: &PublicKey) {
        self.enqueue(key, DialScript::Hold);
    }

    /// The next dial to `key` fails promptly.
    #[allow(dead_code)] // part of the kit; first exercised in P6
    pub(crate) fn refuse(&self, key: &PublicKey, reason: &str) {
        self.enqueue(key, DialScript::Refuse(reason.to_string()));
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
}

impl Dial for ScriptedDial {
    type Conn = TestConn;

    async fn dial(&self, to: &Peer, _alpn: &[u8]) -> Result<TestConn, DialError> {
        *self
            .attempts
            .lock()
            .expect("attempts lock")
            .entry(to.key.0)
            .or_default() += 1;
        let script = self
            .scripts
            .lock()
            .expect("scripts lock")
            .get_mut(&to.key.0)
            .and_then(VecDeque::pop_front);
        match script {
            Some(DialScript::Connect(conn)) => Ok(conn),
            Some(DialScript::Refuse(reason)) => Err(DialError(reason)),
            Some(DialScript::Hold) => std::future::pending().await,
            None => panic!("unscripted dial to {}", hex::encode(&to.key.0)),
        }
    }
}

/// A scripted connection: framed replies consumed in order per request, an
/// inbound uni-frame queue the test feeds. Frames are exact bytes — the
/// double never inspects what it answers.
#[derive(Clone)]
pub(crate) struct TestConn {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    /// Every request frame sent, for asserting what went on the wire.
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    uni_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    uni_rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>>,
}

enum Reply {
    Frame(Vec<u8>),
    Fail(String),
    Hold,
}

impl TestConn {
    pub(crate) fn new() -> Self {
        let (uni_tx, uni_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            replies: Arc::new(Mutex::new(VecDeque::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            uni_tx,
            uni_rx: Arc::new(tokio::sync::Mutex::new(uni_rx)),
        }
    }

    /// Answer the next request with these exact frame bytes.
    pub(crate) fn reply(&self, frame: Vec<u8>) -> &Self {
        self.replies
            .lock()
            .expect("replies lock")
            .push_back(Reply::Frame(frame));
        self
    }

    /// Fail the next request — established, then broken mid-operation.
    #[allow(dead_code)] // part of the kit; first exercised in P6
    pub(crate) fn reply_fail(&self, reason: &str) -> &Self {
        self.replies
            .lock()
            .expect("replies lock")
            .push_back(Reply::Fail(reason.to_string()));
        self
    }

    /// The next request hangs until the caller's deadline drops it.
    #[allow(dead_code)] // part of the kit; first exercised in P6
    pub(crate) fn reply_hold(&self) -> &Self {
        self.replies
            .lock()
            .expect("replies lock")
            .push_back(Reply::Hold);
        self
    }

    /// The request frames sent so far.
    #[allow(dead_code)] // part of the kit; first asserted on in P6
    pub(crate) fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().expect("requests lock").clone()
    }

    /// Deliver an inbound one-way frame (a nudge) to a parked `accept_uni`.
    #[allow(dead_code)] // part of the kit; first exercised in P6
    pub(crate) fn send_uni(&self, frame: Vec<u8>) {
        let _ = self.uni_tx.send(frame);
    }
}

impl Request for TestConn {
    async fn request(&self, frame: &[u8], max_response: usize) -> Result<Vec<u8>, ConnError> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(frame.to_vec());
        let reply = self.replies.lock().expect("replies lock").pop_front();
        match reply {
            Some(Reply::Frame(bytes)) if bytes.len() <= max_response => Ok(bytes),
            Some(Reply::Frame(_)) => Err(ConnError("response over max_response".to_string())),
            Some(Reply::Fail(reason)) => Err(ConnError(reason)),
            Some(Reply::Hold) => std::future::pending().await,
            None => panic!("unscripted request (frame {} bytes)", frame.len()),
        }
    }
}

impl AcceptUni for TestConn {
    async fn accept_uni(&self, max: usize) -> Result<Vec<u8>, ConnError> {
        // The sender half lives in this struct, so recv never yields None —
        // an unfed queue pends, which is what a quiet connection does.
        match self.uni_rx.lock().await.recv().await {
            Some(frame) if frame.len() <= max => Ok(frame),
            Some(_) => Err(ConnError("uni frame over max".to_string())),
            None => std::future::pending().await,
        }
    }
}

/// Blob-connection outcomes scripted per peer key, like `ScriptedDial`.
#[derive(Clone, Default)]
pub(crate) struct ScriptedDialBlobs {
    scripts: Arc<Mutex<BTreeMap<[u8; 32], VecDeque<BlobScript>>>>,
}

enum BlobScript {
    Connect(TestBlobConn),
    Hold,
}

impl ScriptedDialBlobs {
    #[allow(dead_code)] // part of the kit; first exercised when blobs migrate
    pub(crate) fn connect(&self, key: &PublicKey) -> TestBlobConn {
        let conn = TestBlobConn::default();
        self.enqueue(key, BlobScript::Connect(conn.clone()));
        conn
    }

    #[allow(dead_code)] // part of the kit; first exercised when blobs migrate
    pub(crate) fn hold(&self, key: &PublicKey) {
        self.enqueue(key, BlobScript::Hold);
    }

    fn enqueue(&self, key: &PublicKey, script: BlobScript) {
        self.scripts
            .lock()
            .expect("scripts lock")
            .entry(key.0)
            .or_default()
            .push_back(script);
    }
}

impl DialBlobs for ScriptedDialBlobs {
    type Conn = TestBlobConn;

    async fn dial_blobs(&self, to: &Peer) -> Result<TestBlobConn, DialError> {
        let script = self
            .scripts
            .lock()
            .expect("scripts lock")
            .get_mut(&to.key.0)
            .and_then(VecDeque::pop_front);
        match script {
            Some(BlobScript::Connect(conn)) => Ok(conn),
            Some(BlobScript::Hold) => std::future::pending().await,
            None => panic!("unscripted blob dial to {}", hex::encode(&to.key.0)),
        }
    }
}

/// A blob connection: `push` records and confirms, `fetch` serves what the
/// test staged with `serve`.
#[derive(Clone, Default)]
pub(crate) struct TestBlobConn {
    pushed: Arc<Mutex<Vec<EncryptedBlob>>>,
    held: Arc<Mutex<BTreeMap<[u8; 32], Vec<u8>>>>,
}

impl TestBlobConn {
    /// What `push` delivered, in order.
    #[allow(dead_code)] // part of the kit; first exercised when blobs migrate
    pub(crate) fn pushed(&self) -> Vec<EncryptedBlob> {
        self.pushed.lock().expect("pushed lock").clone()
    }

    /// Stage ciphertext for `fetch` to serve.
    #[allow(dead_code)] // part of the kit; first exercised when blobs migrate
    pub(crate) fn serve(&self, hash: BlobHash, bytes: Vec<u8>) {
        self.held.lock().expect("held lock").insert(hash.0, bytes);
    }
}

impl PushBlob for TestBlobConn {
    async fn push(&self, blob: &EncryptedBlob) -> Result<(), ConnError> {
        self.pushed.lock().expect("pushed lock").push(blob.clone());
        Ok(())
    }
}

impl FetchBlob for TestBlobConn {
    async fn fetch(&self, hash: &BlobHash) -> Result<Vec<u8>, ConnError> {
        self.held
            .lock()
            .expect("held lock")
            .get(&hash.0)
            .cloned()
            .ok_or_else(|| ConnError("blob not held".to_string()))
    }
}

/// Inbound requests as a queue the test feeds; the serve loop pulls exactly
/// as it does from the router. `inject` returns the response the handler
/// eventually sends.
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
    #[allow(dead_code)] // part of the kit; first exercised in P6
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

/// Homing as a settable fact: `online()` pends until `set_online` — the
/// truthful default for an endpoint with no relay connection.
#[derive(Clone, Default)]
pub(crate) struct TestHome(Arc<Mutex<HomeState>>);

#[derive(Default)]
struct HomeState {
    online: bool,
    parked: Vec<Waker>,
}

impl TestHome {
    #[allow(dead_code)] // part of the kit; first exercised in P6
    pub(crate) fn set_online(&self) {
        let mut state = self.0.lock().expect("home lock");
        state.online = true;
        for waker in state.parked.drain(..) {
            waker.wake();
        }
    }
}

impl Home for TestHome {
    fn online(&self) -> impl std::future::Future<Output = ()> + Send {
        let state = self.0.clone();
        std::future::poll_fn(move |cx| {
            let mut state = state.lock().expect("home lock");
            if state.online {
                return Poll::Ready(());
            }
            state.parked.push(cx.waker().clone());
            Poll::Pending
        })
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
    async fn test_conn__should_fail_replies_over_the_response_cap() {
        // Given
        let conn = TestConn::new();
        conn.reply(vec![0; 32]);

        // When
        let result = conn.request(b"req", 16).await;

        // Then: the cap is the contract, doubles included
        assert!(result.is_err());
    }
}
