//! The production transport: iroh behind the ports (`docs/design/transport.md`
//! §5–6). Everything iroh-shaped lives here — endpoint binding, addressing,
//! stream mechanics, the router feeding `Accept`'s pull surface, and the
//! iroh-blobs push/fetch/observe machinery. The dial-spec and relay-URL
//! parsers are here too (their string formats embed iroh's id/url encodings),
//! callable from domain code as plain functions.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use iroh::endpoint::{Connection, SendStream, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::tls::CaTlsConfig;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey,
};
use iroh_blobs::Hash;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesSeq, ObserveRequest, PushRequest};
use iroh_blobs::store::mem::MemStore;
use n0_future::StreamExt;
use zink_protocol::{DeviceKey, EncryptedBlob, PublicKey, SYNC_ALPN};

use super::{
    Accept, AcceptUni, Close, ConnError, Dial, DialBlobs, DialError, FetchBlob, Home, Inbound,
    InsertRelay, InvalidRelayUrl, Peer, PushBlob, RemoveRelay, Request, Respond,
};
use crate::error::Error;

/// One iroh endpoint wearing every port: dialing, blobs, inbound sync
/// requests, homing, shutdown. Cheap to clone — clones share the endpoint,
/// the router, and the inbound queue.
#[derive(Clone)]
pub struct IrohTransport {
    endpoint: Endpoint,
    /// Serves `SYNC_ALPN`, forwarding each request into `inbound`. Held for
    /// its lifetime; shut down in `close`.
    router: Arc<Router>,
    inbound: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Inbound<IrohReply>>>>,
}

impl IrohTransport {
    /// The endpoint key IS the device key: mailbox auth is the connection.
    ///
    /// `home_relays` are this device's own relay-service URLs (D0b): with any
    /// set, the endpoint homes to them and stays reachable by key across NATs
    /// — iroh holepunches to a direct path when it can and falls back to
    /// relaying the (encrypted) QUIC when it can't. The relay transport is
    /// ALWAYS bound — with an empty map when no profile exists yet (De5) —
    /// so peers' relay URLs dial immediately and `insert_relay` homes the
    /// *running* endpoint.
    ///
    /// `max_inbound` caps how many bytes of one inbound request the router
    /// reads — the domain's `MAX_SYNC_REQUEST_BYTES`.
    pub(crate) async fn bind(
        device: &DeviceKey,
        home_relays: &[String],
        max_inbound: usize,
    ) -> Result<Self, Error> {
        let map: RelayMap = home_relays
            .iter()
            .map(|url| parse_relay_url(url).map(relay_config))
            .collect::<Result<_, _>>()?;
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&device.seed()))
            .relay_mode(RelayMode::Custom(map))
            // The relay serves QAD with a self-signed cert (De2) — webpki
            // roots would put a CA in the trust path, which zink relays
            // deliberately don't have. Nothing security-relevant rides on
            // this TLS: iroh connections authenticate by endpoint key, and
            // a QAD man-in-the-middle can at most misreport our observed
            // address (degraded holepunching — today's baseline anyway).
            .ca_tls_config(CaTlsConfig::insecure_skip_verify())
            .bind()
            .await
            .map_err(|e| Error::Transport(format!("bind endpoint: {e}")))?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let router = Router::builder(endpoint.clone())
            .accept(SYNC_ALPN, ForwardHandler { tx, max_inbound })
            .spawn();
        Ok(Self {
            endpoint,
            router: Arc::new(router),
            inbound: Arc::new(tokio::sync::Mutex::new(rx)),
        })
    }

    /// This device's peer dial string `<endpoint-id>@<ip:port>` — the format
    /// `parse_dial` reads back. Errors until the endpoint has a bound socket.
    pub(crate) fn sync_address(&self) -> Result<String, Error> {
        let addr = self.endpoint.addr();
        let sock = addr
            .ip_addrs()
            .next()
            .ok_or_else(|| Error::Transport("no bound address yet".into()))?;
        Ok(format!("{}@{}", self.endpoint.id(), sock))
    }

    /// This endpoint as a `Peer` — every bound socket plus any homed relay,
    /// the full multi-address another local endpoint can dial (a bare public
    /// socket isn't reliably self-reachable on one host).
    #[cfg(test)]
    pub(crate) fn peer(&self) -> Peer {
        let addr = self.endpoint.addr();
        Peer {
            key: PublicKey(*self.endpoint.id().as_bytes()),
            relays: addr.relay_urls().map(|url| url.to_string()).collect(),
            sockets: addr.ip_addrs().copied().collect(),
        }
    }
}

impl Dial for IrohTransport {
    type Conn = IrohConn;

    fn dial(
        &self,
        to: &Peer,
        alpn: &[u8],
    ) -> impl std::future::Future<Output = Result<IrohConn, DialError>> + Send {
        let addr = endpoint_addr(to);
        async move {
            let connection = self
                .endpoint
                .connect(addr.map_err(DialError)?, alpn)
                .await
                .map_err(|e| DialError(e.to_string()))?;
            Ok(IrohConn { connection })
        }
    }
}

/// An established connection: framed round-trips plus inbound one-way
/// frames, per the one-request-per-bi-stream wire convention.
pub struct IrohConn {
    connection: Connection,
}

impl Request for IrohConn {
    async fn request(&self, frame: &[u8], max_response: usize) -> Result<Vec<u8>, ConnError> {
        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| ConnError(format!("open stream: {e}")))?;
        send.write_all(frame)
            .await
            .map_err(|e| ConnError(format!("send request: {e}")))?;
        send.finish()
            .map_err(|e| ConnError(format!("finish stream: {e}")))?;
        recv.read_to_end(max_response)
            .await
            .map_err(|e| ConnError(format!("read response: {e}")))
    }
}

impl AcceptUni for IrohConn {
    async fn accept_uni(&self, max: usize) -> Result<Vec<u8>, ConnError> {
        let mut recv = self
            .connection
            .accept_uni()
            .await
            .map_err(|e| ConnError(e.to_string()))?;
        recv.read_to_end(max)
            .await
            .map_err(|e| ConnError(e.to_string()))
    }
}

impl DialBlobs for IrohTransport {
    type Conn = IrohBlobConn;

    fn dial_blobs(
        &self,
        to: &Peer,
    ) -> impl std::future::Future<Output = Result<IrohBlobConn, DialError>> + Send {
        let addr = endpoint_addr(to);
        async move {
            let connection = self
                .endpoint
                .connect(addr.map_err(DialError)?, iroh_blobs::ALPN)
                .await
                .map_err(|e| DialError(e.to_string()))?;
            Ok(IrohBlobConn { connection })
        }
    }
}

/// A connection on the iroh-blobs ALPN. Push/fetch stage through a
/// throwaway in-memory store per operation — the port speaks blobs, not
/// stores.
pub struct IrohBlobConn {
    connection: Connection,
}

impl PushBlob for IrohBlobConn {
    async fn push(&self, blob: &EncryptedBlob) -> Result<(), ConnError> {
        let staging = MemStore::new();
        staging
            .add_bytes(blob.bytes.clone())
            .await
            .map_err(|e| ConnError(format!("stage blob: {e}")))?;
        let hash = Hash::from_bytes(blob.hash.0);
        let push = PushRequest::new(hash, ChunkRangesSeq::from_ranges([ChunkRanges::all()]));
        staging
            .remote()
            .execute_push(self.connection.clone(), push)
            .await
            .map_err(|e| ConnError(format!("push: {e}")))?;
        await_blob_complete(&staging, &self.connection, hash).await
    }
}

impl FetchBlob for IrohBlobConn {
    fn fetch(
        &self,
        hash: &zink_protocol::BlobHash,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, ConnError>> + Send {
        let blob_hash = Hash::from_bytes(hash.0);
        async move {
            let store = MemStore::new();
            store
                .remote()
                .fetch(self.connection.clone(), blob_hash)
                .await
                .map_err(|e| ConnError(format!("fetch blob: {e}")))?;
            store
                .blobs()
                .get_bytes(blob_hash)
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|e| ConnError(format!("read fetched blob: {e}")))
        }
    }
}

/// Push completion is not acknowledged in-band (iroh-blobs 0.103), so
/// confirm via an Observe request: wait until the relay reports the blob
/// complete. Returning right after the push would race the transfer.
///
/// The observe stream sends one initial bitfield and then *diffs*, so the
/// items must be accumulated — no single diff ever looks complete.
async fn await_blob_complete(
    store: &MemStore,
    connection: &Connection,
    hash: Hash,
) -> Result<(), ConnError> {
    let mut bitfields = std::pin::pin!(
        store
            .remote()
            .observe(connection.clone(), ObserveRequest::new(hash))
    );
    let mut current = iroh_blobs::api::proto::Bitfield::empty();
    while let Some(item) = bitfields.next().await {
        let item = item.map_err(|e| ConnError(format!("observe blob: {e}")))?;
        current.update(&item);
        if current.is_complete() {
            return Ok(());
        }
    }
    Err(ConnError(
        "relay never confirmed the blob upload".to_string(),
    ))
}

impl Accept for IrohTransport {
    type Reply = IrohReply;

    async fn accept(&self) -> Option<Inbound<IrohReply>> {
        self.inbound.lock().await.recv().await
    }
}

/// The response half of one inbound bi-stream.
pub struct IrohReply {
    send: SendStream,
}

impl Respond for IrohReply {
    async fn respond(mut self, frame: &[u8]) -> Result<(), ConnError> {
        self.send
            .write_all(frame)
            .await
            .map_err(|e| ConnError(format!("send response: {e}")))?;
        self.send
            .finish()
            .map_err(|e| ConnError(format!("finish stream: {e}")))?;
        Ok(())
    }
}

/// The router-side half of `Accept`: reads one request per bi-stream and
/// forwards `(peer, frame, reply)` into the queue the port pulls from. The
/// router runs one of these per live connection, so requests from different
/// peers interleave — the serve loop keeps that concurrency by spawning per
/// request.
#[derive(Debug, Clone)]
struct ForwardHandler {
    tx: tokio::sync::mpsc::Sender<Inbound<IrohReply>>,
    max_inbound: usize,
}

impl ProtocolHandler for ForwardHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = PublicKey(*connection.remote_id().as_bytes());
        loop {
            let Ok((send, mut recv)) = connection.accept_bi().await else {
                break;
            };
            // A failed read forwards an empty frame: undecodable and
            // unreadable answer the same way (a malformed-request error).
            let frame = recv.read_to_end(self.max_inbound).await.unwrap_or_default();
            let inbound = Inbound {
                peer,
                frame,
                reply: IrohReply { send },
            };
            if self.tx.send(inbound).await.is_err() {
                break; // the client is gone; stop serving
            }
        }
        Ok(())
    }
}

impl Home for IrohTransport {
    fn online(&self) -> impl std::future::Future<Output = ()> + Send {
        self.endpoint.online()
    }
}

impl InsertRelay for IrohTransport {
    async fn insert_relay(&self, url: &str) -> Result<(), InvalidRelayUrl> {
        let url = RelayUrl::from_str(url)
            .map_err(|e| InvalidRelayUrl(format!("relay url {url}: {e}")))?;
        self.endpoint
            .insert_relay(url.clone(), Arc::new(relay_config(url)))
            .await;
        Ok(())
    }
}

impl RemoveRelay for IrohTransport {
    async fn remove_relay(&self, url: &str) {
        // A URL that never parsed was never inserted — nothing to remove.
        if let Ok(url) = RelayUrl::from_str(url) {
            self.endpoint.remove_relay(&url).await;
        }
    }
}

impl Close for IrohTransport {
    async fn close(&self) {
        let _ = self.router.shutdown().await;
        self.endpoint.close().await;
    }
}

/// One home relay's client-side config. Same-port convention (De2): the
/// relay serves QUIC address discovery on UDP at the relay URL's own port
/// number (TCP for HTTP relaying and UDP for QAD coexist at one number, and
/// distinct URLs get distinct QAD ports — multi-relay on one host stays
/// collision-free). A URL with no explicit port keeps iroh's default QAD
/// port (7842), which is exactly the convention standard iroh relays use.
fn relay_config(url: RelayUrl) -> RelayConfig {
    let port = url.port();
    let mut config = RelayConfig::from(url);
    if let (Some(port), Some(quic)) = (port, config.quic.as_mut()) {
        quic.port = port;
    }
    config
}

/// Parse an iroh relay URL from a `RelayEntry.relay_url` value — validation
/// for values that later reach `Home::insert_relay` or a `Peer`'s hints.
pub(crate) fn parse_relay_url(url: &str) -> Result<RelayUrl, Error> {
    RelayUrl::from_str(url).map_err(|e| Error::InvalidInput(format!("relay url {url}: {e}")))
}

/// A dialable `Peer` from a key + its relay URLs (from its ContactRecord),
/// both validated here — the same early rejection the pre-port code did at
/// address-build time, so an undialable record is an input error, not a
/// spent dial.
pub(crate) fn validated_peer(key: PublicKey, relay_urls: Vec<String>) -> Result<Peer, Error> {
    EndpointId::from_bytes(&key.0)
        .map_err(|e| Error::InvalidInput(format!("peer endpoint id: {e}")))?;
    for url in &relay_urls {
        parse_relay_url(url)?;
    }
    Ok(Peer {
        key,
        relays: relay_urls,
        sockets: Vec::new(),
    })
}

/// `<endpoint-id>@<ip:port>`, as printed by `zink-relay` and `sync_address`.
/// Tolerates the full relay spec `<endpoint-id>@<ip:port>#<relay-url>` —
/// dialing only needs the part before the `#`.
pub(crate) fn parse_dial(spec: &str) -> Result<Peer, Error> {
    let spec = spec.split_once('#').map_or(spec, |(dial, _)| dial);
    let (id, sock) = spec
        .split_once('@')
        .ok_or_else(|| Error::InvalidInput("relay must be <endpoint-id>@<ip:port>".into()))?;
    let id = EndpointId::from_str(id)
        .map_err(|e| Error::InvalidInput(format!("relay endpoint id: {e}")))?;
    let sock = SocketAddr::from_str(sock)
        .map_err(|e| Error::InvalidInput(format!("relay socket addr: {e}")))?;
    Ok(Peer {
        key: PublicKey(*id.as_bytes()),
        relays: Vec::new(),
        sockets: vec![sock],
    })
}

/// A `Peer`'s iroh addressing: the key as the endpoint id, every relay URL
/// and socket as hints. iroh routes initial signaling via the relay, then
/// holepunches to a direct path or falls back to relaying.
fn endpoint_addr(to: &Peer) -> Result<EndpointAddr, String> {
    let id = EndpointId::from_bytes(&to.key.0).map_err(|e| format!("peer endpoint id: {e}"))?;
    let mut addr = EndpointAddr::new(id);
    for url in &to.relays {
        let url = RelayUrl::from_str(url).map_err(|e| format!("relay url {url}: {e}"))?;
        addr = addr.with_relay_url(url);
    }
    for sock in &to.sockets {
        addr = addr.with_ip_addr(*sock);
    }
    Ok(addr)
}
