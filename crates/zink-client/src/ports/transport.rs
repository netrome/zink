//! The transport ports: what the client needs from the network, as verb-named
//! capability traits. Contracts, seam placement, and the test-double
//! discipline live in `docs/design/transport.md` — in particular what a
//! caller may NOT assume (no bounded time, no ordering, no
//! error-implies-not-processed). No method takes a `Duration` and no adapter
//! runs a timer: every deadline is a caller-side `Clock::timeout` race.

use std::future::Future;
use std::net::SocketAddr;

use thiserror::Error;
use zink_protocol::{BlobHash, EncryptedBlob, PublicKey};

/// The full network capability, as one bound for `Client`'s type parameter.
/// Blanket-implemented — nothing implements it by name; helpers take the
/// narrowest verb they exercise.
pub trait Transport:
    Dial + DialBlobs + Accept + AcceptBlobs + Home + InsertRelay + RemoveRelay + Close + Clone
{
}
impl<T: Dial + DialBlobs + Accept + AcceptBlobs + Home + InsertRelay + RemoveRelay + Close + Clone>
    Transport for T
{
}

pub trait Dial: Send + Sync + 'static {
    type Conn: Request + AcceptUni;
    /// May stay pending arbitrarily long — race it against a Clock deadline.
    /// A returned connection is to `to.key`, authenticated by the handshake.
    fn dial(
        &self,
        to: &Peer,
        alpn: &[u8],
    ) -> impl Future<Output = Result<Self::Conn, DialError>> + Send;
}

/// One framed request, one length-capped framed response.
pub trait Request: Send + Sync + 'static {
    fn request(
        &self,
        frame: &[u8],
        max_response: usize,
    ) -> impl Future<Output = Result<Vec<u8>, ConnError>> + Send;
}

/// Unsolicited one-way frames from the remote (the nudge path).
pub trait AcceptUni: Send + Sync + 'static {
    fn accept_uni(&self, max: usize) -> impl Future<Output = Result<Vec<u8>, ConnError>> + Send;
}

pub trait DialBlobs: Send + Sync + 'static {
    type Conn: PushBlob + FetchBlob;
    fn dial_blobs(&self, to: &Peer) -> impl Future<Output = Result<Self::Conn, DialError>> + Send;
}

pub trait PushBlob: Send + Sync + 'static {
    /// Resolves only once the remote durably holds the blob.
    fn push(&self, blob: &EncryptedBlob) -> impl Future<Output = Result<(), ConnError>> + Send;
}

pub trait FetchBlob: Send + Sync + 'static {
    /// Success means the returned bytes hash to `hash`.
    fn fetch(&self, hash: &BlobHash) -> impl Future<Output = Result<Vec<u8>, ConnError>> + Send;
}

pub trait Accept: Send + Sync + 'static {
    type Reply: Respond;
    /// Next inbound request from any peer; None once the endpoint closes.
    fn accept(&self) -> impl Future<Output = Option<Inbound<Self::Reply>>> + Send;
}

/// Inbound blob pushes — the recipient half of peer blob delivery (project
/// 10 phase B). A pull surface like `Accept`: the adapter runs the blob
/// protocol and reports what happens; the domain answers the one question
/// that is policy — who may push — and takes what lands.
pub trait AcceptBlobs: Send + Sync + 'static {
    type Admit: Admit;
    /// Next inbound blob event from any peer; None once the endpoint closes.
    fn accept_blob(&self) -> impl Future<Output = Option<BlobEvent<Self::Admit>>> + Send;
}

/// The answer to a `BlobEvent::Connected`: let this peer push, or not.
pub trait Admit: Send + 'static {
    fn admit(self, admitted: bool) -> impl Future<Output = ()> + Send;
}

/// What the blob protocol reports about peers pushing to us. `peer` is the
/// key the transport handshake authenticated, in every variant.
pub enum BlobEvent<A> {
    /// `peer` opened a blob connection; nothing flows until `admit` answers.
    /// Declining closes the connection — on the wire indistinguishable from
    /// a cache that is full. A declined connection reports no `Closed`.
    Connected { peer: PublicKey, admit: A },
    /// A blob `peer` pushed has landed whole: the transport verified that
    /// `blob.bytes` hash to `blob.hash` before reporting it.
    Pushed {
        peer: PublicKey,
        blob: EncryptedBlob,
    },
    /// An admitted blob connection from `peer` closed.
    Closed { peer: PublicKey },
}

pub trait Respond: Send + 'static {
    fn respond(self, frame: &[u8]) -> impl Future<Output = Result<(), ConnError>> + Send;
}

/// To home — attach to a home relay, the transition that makes this endpoint
/// reachable by key (De6c: "homed").
pub trait Home: Send + Sync + 'static {
    /// Resolves when a home relay connection is up. May NEVER resolve (no
    /// relay configured, relay down) — always race it against a deadline.
    fn online(&self) -> impl Future<Output = ()> + Send;
}

pub trait InsertRelay: Send + Sync + 'static {
    /// Add a home relay; affects future dials and homing. Fails only on an
    /// unparseable URL.
    fn insert_relay(&self, url: &str) -> impl Future<Output = Result<(), InvalidRelayUrl>> + Send;
}

pub trait RemoveRelay: Send + Sync + 'static {
    /// Drop a home relay from future dials and homing; no promise about
    /// existing connections.
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
#[derive(Debug, Clone)]
pub struct Peer {
    pub key: PublicKey,
    /// Relay URLs as recorded.
    pub relays: Vec<String>,
    /// Explicit ip:port hints (dial strings).
    pub sockets: Vec<SocketAddr>,
}

/// An inbound request. `peer` is authenticated by the transport handshake.
pub struct Inbound<R> {
    pub peer: PublicKey,
    pub frame: Vec<u8>,
    pub reply: R,
}

/// A dial failed. Opaque on purpose: the domain never branches on failure
/// taxonomy — a variant is added only when domain logic branches on it.
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct DialError(pub String);

/// An operation on an established connection failed. Says nothing about
/// whether the remote received or processed anything (transport.md §3).
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct ConnError(pub String);

/// A relay URL the adapter cannot parse.
#[derive(Debug, Clone, Error)]
#[error("{0}")]
pub struct InvalidRelayUrl(pub String);

#[cfg(test)]
mod test_transport;
#[cfg(test)]
pub(crate) use test_transport::{Loopback, ScriptedConn, TestTransport, WireEvent};
