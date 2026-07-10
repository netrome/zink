//! Native dev/test client (not shipped): drives relays end-to-end.
//!
//! ```text
//! zink-cli keygen <key-file>                 # new key, prints public key
//! zink-cli pubkey <key-file>                 # prints public key
//! zink-cli send --key <file> --to <pubkey>@<relay>[,<relay>…] [--to …] <text>
//! zink-cli recv --key <file> --relay <relay> [--relay …]
//! ```
//!
//! `<relay>` is a dial string `<endpoint-id>@<ip:port>` as printed by
//! `zink-relay`. Send seals one envelope for all recipients and deposits it
//! once per distinct relay; recv drains every given relay and dedups by
//! message id.

use std::collections::BTreeSet;
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
    MailboxResult, MessageDraft, MessageEnvelope, PublicKey, distinct_relays,
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
  zink-cli send --key <file> --to <pubkey>@<relay>[,<relay>...] [--to ...] <text>
  zink-cli recv --key <file> --relay <relay> [--relay ...]
(<relay> = <endpoint-id>@<ip:port>, as printed by zink-relay)";

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
    let (flags, positionals) = parse_flags(args)?;
    let device = load_key(&single(&flags, "--key")?)?;
    let contacts: Vec<Contact> = values(&flags, "--to")
        .iter()
        .map(|spec| parse_contact(spec))
        .collect::<Result<_, _>>()?;
    if contacts.is_empty() {
        return Err(format!("at least one --to required\n{USAGE}"));
    }
    let [text] = positionals.as_slice() else {
        return Err(format!("exactly one message text expected\n{USAGE}"));
    };

    // Until the DAG lands in the client (slice B5) every message is a
    // standalone genesis.
    let draft = MessageDraft {
        conversation: None,
        parents: vec![],
        recipients: contacts.iter().map(|c| c.key).collect(),
        seq: 0,
        logical: 0,
        timestamp_ms: now_ms(),
        plaintext: text.clone().into_bytes(),
    };
    let envelope = MessageEnvelope::seal(draft, &device, &mut OsRng)
        .map_err(|e| format!("seal message: {e}"))?;
    let id = envelope.id();

    // One envelope, one deposit per distinct relay across all recipients.
    let relays = distinct_relays(contacts.into_iter().map(|c| c.relays));
    let endpoint = bind_endpoint(&device).await?;
    for relay in &relays {
        let connection = connect(&endpoint, relay).await?;
        match request(
            &connection,
            MailboxOp::Deposit {
                envelope: Box::new(envelope.clone()),
            },
        )
        .await?
        {
            MailboxResult::Deposited { .. } => {}
            other => return Err(format!("unexpected response from {relay}: {other:?}")),
        }
    }
    println!(
        "deposited {} to {} relay(s)",
        hex_encode(&id.0),
        relays.len()
    );
    Ok(())
}

async fn recv(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    if !positionals.is_empty() {
        return Err(USAGE.to_string());
    }
    let device = load_key(&single(&flags, "--key")?)?;
    let relays = values(&flags, "--relay");
    if relays.is_empty() {
        return Err(format!("at least one --relay required\n{USAGE}"));
    }

    let endpoint = bind_endpoint(&device).await?;
    let mut seen: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut printed_any = false;
    for relay in &relays {
        let connection = connect(&endpoint, relay).await?;
        request(&connection, MailboxOp::Register).await?;
        let items = match request(&connection, MailboxOp::Fetch { after: 0 }).await? {
            MailboxResult::Envelopes { items } => items,
            other => return Err(format!("unexpected response from {relay}: {other:?}")),
        };
        // Cursors are relay-local: ack each relay at its own high-water mark.
        let mut last_cursor = None;
        for item in &items {
            last_cursor = last_cursor.max(Some(item.cursor));
            if !seen.insert(item.envelope.id().0) {
                continue; // already drained via another relay
            }
            printed_any = true;
            match item.envelope.open(&device) {
                Ok(plaintext) => println!(
                    "from {}: {}",
                    &hex_encode(&item.envelope.core.sender.0)[..8],
                    String::from_utf8_lossy(&plaintext)
                ),
                Err(e) => println!("undecryptable message ({e})"),
            }
        }
        if let Some(up_to) = last_cursor {
            request(&connection, MailboxOp::Ack { up_to }).await?;
        }
    }
    if !printed_any {
        println!("no new messages");
    }
    Ok(())
}

/// A recipient and the relays hosting their mailbox — the CLI stand-in for
/// the ContactRecord (SPEC §3.6) until Stage C.
struct Contact {
    key: PublicKey,
    relays: Vec<String>,
}

/// `<pubkey-hex>@<relay>[,<relay>…]` — hex contains no `@`, so the first
/// `@` splits key from relay list.
fn parse_contact(spec: &str) -> Result<Contact, String> {
    let (key_hex, relay_list) = spec
        .split_once('@')
        .ok_or("--to must be <pubkey>@<relay>[,<relay>...]")?;
    let relays: Vec<String> = relay_list.split(',').map(str::to_string).collect();
    for relay in &relays {
        parse_relay(relay)?; // validate early, before any network work
    }
    Ok(Contact {
        key: PublicKey(parse_hex32(key_hex)?),
        relays,
    })
}

async fn bind_endpoint(device: &DeviceKey) -> Result<Endpoint, String> {
    // The endpoint key IS the device key: mailbox auth is the connection.
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&device.seed()))
        .bind()
        .await
        .map_err(|e| format!("bind endpoint: {e}"))
}

async fn connect(endpoint: &Endpoint, relay: &str) -> Result<Connection, String> {
    endpoint
        .connect(parse_relay(relay)?, MAILBOX_ALPN)
        .await
        .map_err(|e| format!("connect to relay {relay}: {e}"))
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

fn parse_relay(spec: &str) -> Result<EndpointAddr, String> {
    let (id, sock) = spec
        .split_once('@')
        .ok_or("relay must be <endpoint-id>@<ip:port>")?;
    let id = EndpointId::from_str(id).map_err(|e| format!("relay endpoint id: {e}"))?;
    let sock = SocketAddr::from_str(sock).map_err(|e| format!("relay socket addr: {e}"))?;
    Ok(EndpointAddr::new(id).with_ip_addr(sock))
}

/// `--flag value` pairs in order, plus positional args.
type ParsedArgs = (Vec<(String, String)>, Vec<String>);

/// Split args into `--flag value` pairs (repeatable) and positionals.
fn parse_flags(args: &[String]) -> Result<ParsedArgs, String> {
    let mut flags = Vec::new();
    let mut positionals = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            let value = iter.next().ok_or(format!("missing value for --{name}"))?;
            flags.push((format!("--{name}"), value.clone()));
        } else {
            positionals.push(arg.clone());
        }
    }
    Ok((flags, positionals))
}

fn values(flags: &[(String, String)], name: &str) -> Vec<String> {
    flags
        .iter()
        .filter(|(flag, _)| flag == name)
        .map(|(_, value)| value.clone())
        .collect()
}

fn single(flags: &[(String, String)], name: &str) -> Result<String, String> {
    match values(flags, name).as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("missing {name}\n{USAGE}")),
        _ => Err(format!("{name} given more than once\n{USAGE}")),
    }
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
