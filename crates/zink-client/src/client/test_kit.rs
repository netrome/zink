//! The shared test kit for the client's tests: temp-dir plumbing, envelope
//! builders, contact-record shapes, mailbox-frame scripting for the
//! transport doubles (transport.md §7), and client constructors — real-homed
//! and loopback-wired. Helpers only, no assertions; anything unexercised is
//! deleted, not kept warm (the standing kit rule, project 3 §7).

use super::*;
use crate::ports::clock::TestClock;
use crate::ports::transport::{Loopback, ScriptedConn, TestTransport};
use zink_protocol::{KeyCommitment, MessageCore};

/// A key path in a per-test temp dir (tests run in parallel, so the dir is
/// namespaced by `test` — a shared root would let one test's cleanup delete
/// another's files mid-run). The caller cleans up with `temp_root(test)`.
pub(crate) fn temp_key(test: &str, name: &str) -> String {
    let dir = temp_root(test);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(name).to_string_lossy().into_owned()
}

pub(crate) fn temp_root(test: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("zink-client-sync-{}-{test}", std::process::id()))
}

/// A signed linear chain: genesis (seq/logical 0), then children each
/// threading onto the previous — what a real send would produce, enough to
/// rebuild the DAG. Bodies are empty (the backfill test never decrypts).
pub(crate) fn chain(author: &DeviceKey, recipient: PublicKey, len: u64) -> Vec<MessageEnvelope> {
    let mut envelopes: Vec<MessageEnvelope> = Vec::new();
    for seq in 0..len {
        let (conversation, parents) = match envelopes.first() {
            None => (None, vec![]),
            Some(genesis) => (Some(genesis.id()), vec![envelopes.last().unwrap().id()]),
        };
        let core = MessageCore {
            version: MessageCore::CURRENT,
            conversation,
            parents,
            recipients: vec![recipient],
            sender: author.public(),
            seq,
            logical: seq,
            timestamp_ms: 0,
            body: vec![],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        };
        envelopes.push(MessageEnvelope::new(core, author));
    }
    envelopes
}

/// A conversation of *genuinely sealed* envelopes (real wraps, real
/// crypto) — what the re-wrap tests need; `chain` above is
/// skeleton-only.
pub(crate) fn sealed_chain(
    author: &DeviceKey,
    recipient: PublicKey,
    texts: &[&[u8]],
) -> Vec<MessageEnvelope> {
    let mut envelopes: Vec<MessageEnvelope> = Vec::new();
    for (seq, text) in texts.iter().enumerate() {
        let (conversation, parents) = match envelopes.first() {
            None => (None, vec![]),
            Some(genesis) => (Some(genesis.id()), vec![envelopes.last().unwrap().id()]),
        };
        let draft = MessageDraft {
            conversation,
            parents,
            recipients: vec![recipient],
            seq: seq as u64,
            logical: seq as u64,
            timestamp_ms: 0,
            plaintext: text.to_vec(),
            blobs: vec![],
        };
        envelopes.push(
            MessageEnvelope::seal(draft, author, &mut OsRng)
                .expect("seal")
                .envelope,
        );
    }
    envelopes
}

/// One signed message with an explicit shape — the group-membership
/// tests need varying recipient sets and forks (bodies empty, never
/// opened).
pub(crate) fn message(
    author: &DeviceKey,
    recipients: Vec<PublicKey>,
    conversation: Option<MessageId>,
    parents: Vec<MessageId>,
    seq: u64,
    logical: u64,
) -> MessageEnvelope {
    MessageEnvelope::new(
        MessageCore {
            version: MessageCore::CURRENT,
            conversation,
            parents,
            recipients,
            sender: author.public(),
            seq,
            logical,
            timestamp_ms: 0,
            body: vec![],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        },
        author,
    )
}

/// A genesis envelope really sealed to `recipient` — a push must be
/// openable to be stored (the handler mirrors the mailbox drain), so the
/// direct-delivery tests can't use bare unsealed cores.
pub(crate) fn sealed_for(sender: &DeviceKey, recipient: PublicKey, text: &[u8]) -> MessageEnvelope {
    MessageEnvelope::seal(
        MessageDraft {
            conversation: None,
            parents: vec![],
            recipients: vec![recipient],
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            plaintext: text.to_vec(),
            blobs: vec![],
        },
        sender,
        &mut OsRng,
    )
    .expect("seal")
    .envelope
}

/// A record naming a peer's live relay URL for rendezvous and a
/// deliberately **dead** mailbox: dial-by-key works, a deposit cannot.
/// That's the D5 acceptance shape — the mailbox is unreachable, so
/// anything that arrives arrived directly.
pub(crate) fn record_with_dead_mailbox(key: PublicKey, relay_url: &str) -> ContactRecord {
    ContactRecord::new(
        vec![key],
        vec![],
        vec![RelayEntry {
            mailbox: format!("{}@203.0.113.9:1", hex::encode(&key.0)),
            relay_url: Some(relay_url.to_string()),
        }],
    )
}

/// A record naming `key`, mailboxed at `relay` and dialable by key.
pub(crate) fn routed_record(key: PublicKey, relay: &PublicKey) -> ContactRecord {
    ContactRecord::new(
        vec![key],
        vec![],
        vec![RelayEntry {
            mailbox: mailbox_spec(relay),
            relay_url: Some("http://203.0.113.1:1".to_string()),
        }],
    )
}

/// A one-key record with a verified self-claimed name at `revision`.
pub(crate) fn signed_record(
    device: &DeviceKey,
    name: &str,
    revision: u64,
    relays: Vec<RelayEntry>,
) -> ContactRecord {
    let attestation = SignedAttestation::new(
        Attestation {
            version: Attestation::CURRENT,
            attester: device.public(),
            subject: device.public(),
            claim: Claim::Name(name.to_string()),
            revision,
        },
        device,
    );
    ContactRecord::new(vec![device.public()], vec![attestation], relays)
}

/// A mailbox spec whose endpoint id is `relay_key` — parseable (real id,
/// TEST-NET socket), never really dialed: the doubles key on the id.
pub(crate) fn mailbox_spec(relay_key: &PublicKey) -> String {
    format!("{}@203.0.113.1:1", hex::encode(&relay_key.0))
}

pub(crate) fn mailbox_only(mailbox: &str) -> Vec<RelayEntry> {
    vec![RelayEntry {
        mailbox: mailbox.to_string(),
        relay_url: None,
    }]
}

/// A relay's `Deposited` ack as exact frame bytes for a scripted conn
/// (the id is an idempotency receipt; `deliver` matches any).
pub(crate) fn deposited_frame() -> Vec<u8> {
    zink_protocol::MailboxResponse::new(MailboxResult::Deposited {
        id: MessageId([0; 32]),
    })
    .to_bytes()
}

pub(crate) fn registered_frame() -> Vec<u8> {
    zink_protocol::MailboxResponse::new(MailboxResult::Registered).to_bytes()
}

pub(crate) fn acked_frame() -> Vec<u8> {
    zink_protocol::MailboxResponse::new(MailboxResult::Acked).to_bytes()
}

pub(crate) fn envelopes_frame(envelopes: Vec<MessageEnvelope>) -> Vec<u8> {
    let items = envelopes
        .into_iter()
        .zip(1u64..)
        .map(|(envelope, cursor)| zink_protocol::MailboxItem { cursor, envelope })
        .collect();
    zink_protocol::MailboxResponse::new(MailboxResult::Envelopes { items }).to_bytes()
}

/// The envelopes a scripted mailbox conn took as deposits — the test
/// shuttles them into the recipient's next drain. The test IS the
/// relay's storage, visibly (transport.md §7).
pub(crate) fn deposited_envelopes(conn: &ScriptedConn) -> Vec<MessageEnvelope> {
    conn.requests()
        .iter()
        .filter_map(|frame| {
            match zink_protocol::MailboxRequest::try_from_bytes(frame)
                .ok()?
                .op
            {
                MailboxOp::Deposit { envelope } => Some(*envelope),
                _ => None,
            }
        })
        .collect()
}

/// Script one full drain on `conn`: register, one page of `envelopes`,
/// ack, empty page (just the empty page when there is nothing waiting).
pub(crate) fn script_drain(conn: &ScriptedConn, envelopes: Vec<MessageEnvelope>) {
    conn.reply(registered_frame());
    if envelopes.is_empty() {
        conn.reply(envelopes_frame(vec![]));
        return;
    }
    conn.reply(envelopes_frame(envelopes));
    conn.reply(acked_frame());
    conn.reply(envelopes_frame(vec![]));
}

/// An in-process iroh relay server (plain HTTP, `tls: None` — the same
/// shape the `zink-relay` binary embeds). Returns the handle (kept alive
/// by the caller) and its relay URL.
pub(crate) async fn spawn_test_relay() -> (iroh_relay::server::Server, String) {
    use iroh_relay::server::{QuicConfig, RelayConfig, Server, ServerConfig};
    use std::net::Ipv4Addr;
    // Same-port convention (De2): QAD rides UDP at the relay URL's port
    // number, so the port is picked up front — two `:0` binds would land
    // on different numbers. Distinct URLs get distinct QAD ports, which
    // is what lets multi-relay tests share one machine. Retried in case
    // the picked pair races a parallel test.
    for _ in 0..3 {
        let port = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("pick a port")
            .local_addr()
            .expect("local addr")
            .port();
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, port)));
        let mut quic = QuicConfig::new((Ipv4Addr::LOCALHOST, port));
        let (_certs, tls) = iroh_relay::server::testing::self_signed_tls_certs_and_config();
        quic.server_config = Some(tls);
        config.quic = Some(quic);
        if let Ok(server) = Server::spawn(config).await {
            let url = format!("http://{}", server.http_addr().expect("relay http addr"));
            return (server, url);
        }
    }
    panic!("no free port pair for a test relay in 3 attempts");
}

/// Store `requester`'s key as a contact of `server`, so the D0c
/// contacts-only serving gate lets the requester's sync calls through
/// (the minimal record — key only — via the store, skipping
/// `add_contact`'s reachability validation, which serving doesn't need).
pub(crate) fn befriend(server: &ClientState, requester: PublicKey) {
    let record = ContactRecord::new(vec![requester], vec![], vec![]);
    server
        .save_contact("requester", &record)
        .expect("save contact");
}

/// A profile whose relay entry carries `relay_url` — written straight to
/// state so the endpoint homes to it at the *next* open (the D0b
/// restart-to-apply semantics; the mailbox dial string is never used
/// here). Returns the homed client.
pub(crate) async fn open_homed(test: &str, name: &str, relay_url: &str) -> Client {
    open_homed_with(
        test,
        name,
        relay_url,
        ClientConfig::default().connect_timeout,
    )
    .await
}

/// `open_homed` with a tightened relay deadline — for tests that make a
/// mailbox deliberately unreachable and shouldn't wait out production
/// patience for it.
pub(crate) async fn open_homed_with(
    test: &str,
    name: &str,
    relay_url: &str,
    connect_timeout: Duration,
) -> Client {
    let key_path = temp_key(test, name);
    ClientState::open(&key_path)
        .save_profile(
            name,
            &[RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: Some(relay_url.to_string()),
            }],
        )
        .expect("save profile");
    keystore::load_or_create(&key_path).expect("device key");
    Client::open_with(
        &key_path,
        ClientConfig {
            connect_timeout,
            ..Default::default()
        },
    )
    .await
    .expect("open client")
}

/// A loopback-wired client: dialable by its device key, dialing other
/// wired clients — both ends run their real handlers (transport.md §7).
pub(crate) fn loop_client(
    test: &str,
    name: &str,
    wire: &Loopback,
) -> (
    Client<TestClock, SystemClock, TestTransport>,
    TestTransport,
    TestClock,
) {
    let key_path = temp_key(test, name);
    keystore::create(&key_path).expect("key");
    let device = keystore::load(&key_path).expect("load key");
    let net = wire.transport(device.public());
    let clock = TestClock::new();
    let client = Client::with_transport(
        device,
        &key_path,
        ClientConfig::default(),
        clock.clone(),
        SystemClock,
        net.clone(),
    );
    (client, net, clock)
}

pub(crate) fn summary(id: u8, known: bool, first_seen_ms: u64) -> ConversationSummary {
    ConversationSummary {
        id: MessageId([id; 32]),
        participants: vec![],
        message_count: 1,
        last_timestamp_ms: 0,
        known,
        first_seen_ms,
    }
}

/// Every file under `dir` with its bytes — the store-was-not-touched
/// probe for the D1b "network input never mutates stored records" rule.
pub(crate) fn dir_bytes(dir: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        out.insert(
            entry.file_name().to_string_lossy().into_owned(),
            std::fs::read(entry.path()).unwrap_or_default(),
        );
    }
    out
}
