//! Native dev/test client (not shipped): drives a relay end-to-end.
//!
//! ```text
//! zink-cli keygen <key-file>                                  # new key, prints public key
//! zink-cli pubkey <key-file>                                  # prints public key
//! zink-cli send --key <file> --relay <id@ip:port> --to <pubkey-hex> <text>
//! zink-cli recv --key <file> --relay <id@ip:port>             # register + drain + ack
//! ```

use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use rand_core::OsRng;
use zink_protocol::{
    DeviceKey, MAILBOX_ALPN, MAX_RESPONSE_BYTES, MailboxOp, MailboxRequest, MailboxResponse,
    MailboxResult, MessageDraft, MessageEnvelope, PublicKey,
};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("pubkey") => pubkey(&args[1..]),
        Some("send") => send(&args[1..]).await,
        Some("recv") => recv(&args[1..]).await,
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage:
  zink-cli keygen <key-file>
  zink-cli pubkey <key-file>
  zink-cli send --key <file> --relay <id@ip:port> --to <pubkey-hex> <text>
  zink-cli recv --key <file> --relay <id@ip:port>";

fn keygen(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or(USAGE)?;
    let mut seed = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut OsRng, &mut seed);
    std::fs::write(path, hex_encode(&seed)).map_err(|e| format!("write {path}: {e}"))?;
    println!("{}", hex_encode(&load_key(path)?.public().0));
    Ok(())
}

fn pubkey(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or(USAGE)?;
    println!("{}", hex_encode(&load_key(path)?.public().0));
    Ok(())
}

async fn send(args: &[String]) -> Result<(), String> {
    let device = load_key(&flag(args, "--key")?)?;
    let relay = parse_relay(&flag(args, "--relay")?)?;
    let recipient = PublicKey(parse_hex32(&flag(args, "--to")?)?);
    let text = args.last().filter(|_| args.len() % 2 == 1).ok_or(USAGE)?;

    // Until the DAG lands (slice B1) every message is a standalone genesis.
    let draft = MessageDraft {
        conversation: None,
        parents: vec![],
        recipients: vec![recipient],
        seq: 0,
        logical: 0,
        timestamp_ms: now_ms(),
        plaintext: text.clone().into_bytes(),
    };
    let envelope = MessageEnvelope::seal(draft, &device, &mut OsRng)
        .map_err(|e| format!("seal message: {e}"))?;
    let id = envelope.id();

    let (_endpoint, connection) = connect(&device, relay).await?;
    let result = request(
        &connection,
        MailboxOp::Deposit {
            envelope: Box::new(envelope),
        },
    )
    .await?;
    match result {
        MailboxResult::Deposited { .. } => {
            println!("deposited {}", hex_encode(&id.0));
            Ok(())
        }
        other => Err(format!("unexpected relay response: {other:?}")),
    }
}

async fn recv(args: &[String]) -> Result<(), String> {
    let device = load_key(&flag(args, "--key")?)?;
    let relay = parse_relay(&flag(args, "--relay")?)?;
    let (_endpoint, connection) = connect(&device, relay).await?;

    request(&connection, MailboxOp::Register).await?;
    let items = match request(&connection, MailboxOp::Fetch { after: 0 }).await? {
        MailboxResult::Envelopes { items } => items,
        other => return Err(format!("unexpected relay response: {other:?}")),
    };

    if items.is_empty() {
        println!("no new messages");
        return Ok(());
    }
    let mut last_cursor = 0;
    for item in &items {
        match item.envelope.open(&device) {
            Ok(plaintext) => println!(
                "from {}: {}",
                &hex_encode(&item.envelope.core.sender.0)[..8],
                String::from_utf8_lossy(&plaintext)
            ),
            Err(e) => println!("undecryptable message ({e})"),
        }
        last_cursor = last_cursor.max(item.cursor);
    }
    request(&connection, MailboxOp::Ack { up_to: last_cursor }).await?;
    Ok(())
}

async fn connect(
    device: &DeviceKey,
    relay: EndpointAddr,
) -> Result<(Endpoint, Connection), String> {
    // The endpoint key IS the device key: mailbox auth is the connection.
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&device_seed(device)))
        .bind()
        .await
        .map_err(|e| format!("bind endpoint: {e}"))?;
    let connection = endpoint
        .connect(relay, MAILBOX_ALPN)
        .await
        .map_err(|e| format!("connect to relay: {e}"))?;
    Ok((endpoint, connection))
}

async fn request(connection: &Connection, op: MailboxOp) -> Result<MailboxResult, String> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("open stream: {e}"))?;
    send.write_all(&MailboxRequest::new(op).to_bytes())
        .await
        .map_err(|e| format!("send request: {e}"))?;
    send.finish().map_err(|e| format!("finish stream: {e}"))?;
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| format!("read response: {e}"))?;
    Ok(MailboxResponse::try_from_bytes(&bytes)
        .map_err(|e| format!("decode response: {e}"))?
        .result)
}

fn load_key(path: &str) -> Result<DeviceKey, String> {
    let hex = std::fs::read_to_string(Path::new(path))
        .map_err(|e| format!("read key file {path}: {e}"))?;
    Ok(DeviceKey::from_seed(parse_hex32(hex.trim())?))
}

/// The CLI stores the raw seed, so the device key can double as the iroh
/// endpoint key (same Ed25519 seed derivation on both sides).
fn device_seed(device: &DeviceKey) -> [u8; 32] {
    device.seed()
}

fn parse_relay(spec: &str) -> Result<EndpointAddr, String> {
    let (id, sock) = spec
        .split_once('@')
        .ok_or("relay must be <endpoint-id>@<ip:port>")?;
    let id = EndpointId::from_str(id).map_err(|e| format!("relay endpoint id: {e}"))?;
    let sock = SocketAddr::from_str(sock).map_err(|e| format!("relay socket addr: {e}"))?;
    Ok(EndpointAddr::new(id).with_ip_addr(sock))
}

fn flag(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .ok_or_else(|| format!("missing {name}\n{USAGE}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hex32(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(format!("expected 64 hex chars, got {}", hex.len()));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("invalid hex: {e}"))?;
    }
    Ok(out)
}
