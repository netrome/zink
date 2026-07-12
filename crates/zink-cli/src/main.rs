//! Native dev/test client (not shipped): a thin argument/printing layer
//! over `zink-client`, which owns all the actual client logic.
//!
//! ```text
//! zink-cli keygen <key-file>                 # new key, prints public key
//! zink-cli pubkey <key-file>                 # prints public key
//! zink-cli my-record --key <file> [--name <name>] [--relay <relay> …]
//! zink-cli contact-add --key <file> [--name <petname>] <ZINK:…>
//! zink-cli contacts --key <file>
//! zink-cli send --key <file> --to <petname | pubkey@relay[,relay…]> [--to …]
//!               [--image <file> [--thumb <file>]] <text>
//! zink-cli recv --key <file> [--relay <relay> …] [--blobs-dir <dir>]
//! zink-cli conversations --key <file>
//! zink-cli history --key <file> [--blobs-dir <dir>] <conversation-id | prefix>
//! zink-cli reply --key <file> <conversation-id | prefix> <text>
//! zink-cli listen --key <file> [--relay <relay> …]
//! ```
//!
//! `<relay>` is a dial string `<endpoint-id>@<ip:port>` as printed by
//! `zink-relay`.

use std::path::Path;
use std::process::ExitCode;

use zink_client::{Client, ClientConfig, Contact, Received, hex, keystore};
use zink_protocol::{BlobDraft, BlobKind, BlobRef, ContactRecord, MessageId, PublicKey};

#[tokio::main]
async fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("pubkey") => pubkey(&args[1..]),
        Some("my-record") => my_record(&args[1..]).await,
        Some("contact-add") => contact_add(&args[1..]).await,
        Some("contacts") => contacts(&args[1..]).await,
        Some("send") => send(&args[1..]).await,
        Some("recv") => recv(&args[1..]).await,
        Some("conversations") => conversations(&args[1..]).await,
        Some("history") => history(&args[1..]).await,
        Some("reply") => reply(&args[1..]).await,
        Some("listen") => listen(&args[1..]).await,
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
  zink-cli my-record --key <file> [--name <name>] [--relay <relay> ...] [--qr]
  zink-cli contact-add --key <file> [--name <petname>] <ZINK:...>
  zink-cli contacts --key <file>
  zink-cli send --key <file> --to <petname | pubkey@relay[,relay...]> [--to ...]
                [--image <file> [--thumb <file>]] <text>
  zink-cli recv --key <file> [--relay <relay> ...] [--blobs-dir <dir>]
  zink-cli conversations --key <file>
  zink-cli history --key <file> [--blobs-dir <dir>] <conversation-id | prefix>
  zink-cli reply --key <file> <conversation-id | prefix> <text>
  zink-cli listen --key <file> [--relay <relay> ...]
(<relay> = <endpoint-id>@<ip:port>, as printed by zink-relay; recv and listen
 default to the home relays set via my-record)";

fn keygen(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or(USAGE)?;
    let device = keystore::create(path)?;
    println!("{}", hex::encode(&device.public().0));
    Ok(())
}

fn pubkey(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or(USAGE)?;
    println!("{}", hex::encode(&keystore::load(path)?.public().0));
    Ok(())
}

/// Set (or reuse) the profile, print the shareable record payload.
async fn my_record(args: &[String]) -> Result<(), String> {
    // `--qr` is a boolean flag: give it a dummy value before pair-parsing.
    let args: Vec<String> = args
        .iter()
        .flat_map(|arg| {
            if arg == "--qr" {
                vec![arg.clone(), "yes".to_string()]
            } else {
                vec![arg.clone()]
            }
        })
        .collect();
    let (flags, _) = parse_flags(&args)?;
    let client = open_client(&flags).await?;
    let name = optional(&flags, "--name")?.or_else(|| client.profile_name());
    let relays = {
        let given = values(&flags, "--relay");
        if given.is_empty() {
            client.home_relays()
        } else {
            given
        }
    };
    let name = name.ok_or(format!("no profile name yet — pass --name\n{USAGE}"))?;
    if relays.is_empty() {
        return Err(format!("no home relay yet — pass --relay\n{USAGE}"));
    }
    client.set_profile(&name, &relays)?;
    client.register_at_home_relays().await?;
    let payload = client.my_record()?.to_qr_string();
    println!("{payload}");
    if flags.iter().any(|(flag, _)| flag == "--qr") {
        let code = qrcode::QrCode::new(payload.as_bytes()).map_err(|e| format!("qr: {e}"))?;
        println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>().build()
        );
    }
    Ok(())
}

async fn contact_add(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [payload] = positionals.as_slice() else {
        return Err(format!("exactly one ZINK:... payload expected\n{USAGE}"));
    };
    let record = ContactRecord::from_qr_string(payload).map_err(|e| format!("record: {e}"))?;
    let client = open_client(&flags).await?;
    let petname = client.add_contact(&record, optional(&flags, "--name")?)?;
    println!("added contact {petname:?}");
    Ok(())
}

async fn contacts(args: &[String]) -> Result<(), String> {
    let (flags, _) = parse_flags(args)?;
    let client = open_client(&flags).await?;
    let contacts = client.contacts()?;
    if contacts.is_empty() {
        println!("no contacts");
    }
    for (petname, record) in contacts {
        let keys: Vec<String> = record
            .keys
            .iter()
            .map(|k| hex::encode(&k.0)[..8].to_string())
            .collect();
        println!(
            "{petname}  ({}, {} relay(s))",
            keys.join(","),
            record.relays.len()
        );
    }
    Ok(())
}

async fn send(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let client = open_client(&flags).await?;
    let contacts: Vec<Contact> = values(&flags, "--to")
        .iter()
        .map(|spec| {
            // '@' means the raw pubkey@relay escape hatch; else a petname.
            if spec.contains('@') {
                Contact::parse(spec)
            } else {
                client.resolve_contact(spec)
            }
        })
        .collect::<Result<_, _>>()?;
    if contacts.is_empty() {
        return Err(format!("at least one --to required\n{USAGE}"));
    }
    let [text] = positionals.as_slice() else {
        return Err(format!("exactly one message text expected\n{USAGE}"));
    };
    let blobs = blob_drafts(&flags)?;

    // Note: unlike the app, the CLI does not flush the outbox after a send —
    // a one-shot dev command shouldn't eat a 10s-per-dead-entry backlog
    // retry on every invocation. The backlog is retried by `recv`/`listen`.
    let receipt = client
        .send(&contacts, text.clone().into_bytes(), blobs)
        .await?;
    println!(
        "deposited {} (conv {}, seq {}) ({} blob(s)) to {} relay(s){}",
        hex::encode(&receipt.id.0),
        &hex::encode(&receipt.conversation.0)[..8],
        receipt.seq,
        receipt.blob_count,
        receipt.relay_count,
        pending_note(receipt.pending_relays)
    );
    Ok(())
}

/// Suffix for a partially-delivered send (the outbox will retry).
fn pending_note(pending_relays: usize) -> String {
    if pending_relays == 0 {
        String::new()
    } else {
        format!(" — {pending_relays} queued for retry")
    }
}

async fn recv(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    if !positionals.is_empty() {
        return Err(USAGE.to_string());
    }
    let blobs_dir = optional(&flags, "--blobs-dir")?;

    let client = open_client(&flags).await?;
    let relays = {
        let given = values(&flags, "--relay");
        if given.is_empty() {
            client.home_relays()
        } else {
            given
        }
    };
    if relays.is_empty() {
        return Err(format!(
            "no relays: pass --relay or set home relays via my-record\n{USAGE}"
        ));
    }
    let received = client.recv(&relays).await?;
    if received.is_empty() {
        println!("no new messages");
        return Ok(());
    }
    for message in &received {
        match &message.body {
            Ok(plaintext) => println!(
                "from {}: {}",
                &hex::encode(&message.envelope.core.sender.0)[..8],
                String::from_utf8_lossy(plaintext)
            ),
            Err(e) => println!("undecryptable message ({e})"),
        }
        if !message.envelope.core.blob_refs.is_empty() {
            match &blobs_dir {
                Some(dir) => save_blobs(&client, message, dir).await?,
                None => println!(
                    "  ({} blob(s) attached; pass --blobs-dir to fetch)",
                    message.envelope.core.blob_refs.len()
                ),
            }
        }
    }
    Ok(())
}

/// Fetch every referenced blob and write it to `dir`.
async fn save_blobs(client: &Client, message: &Received, dir: &str) -> Result<(), String> {
    for blob_ref in &message.envelope.core.blob_refs {
        let plaintext = client.fetch_blob(message, &blob_ref.hash).await?;
        write_blob(dir, blob_ref, &plaintext)?;
    }
    Ok(())
}

/// List every stored conversation, labelled with the other participants.
async fn conversations(args: &[String]) -> Result<(), String> {
    let (flags, _) = parse_flags(args)?;
    let client = open_client(&flags).await?;
    let summaries = client.conversations()?;
    if summaries.is_empty() {
        println!("no conversations");
    }
    let contacts = client.contacts()?;
    let me = client.public_key();
    for summary in summaries {
        let others: Vec<String> = summary
            .participants
            .iter()
            .filter(|key| **key != me)
            .map(|key| label(&contacts, key))
            .collect();
        println!(
            "{}  {} message(s)  with {}",
            hex::encode(&summary.id.0),
            summary.message_count,
            if others.is_empty() {
                "only me".to_string()
            } else {
                others.join(", ")
            }
        );
    }
    Ok(())
}

/// Print one conversation's stored history in linearized order.
async fn history(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [wanted] = positionals.as_slice() else {
        return Err(format!(
            "exactly one conversation id (or unique prefix) expected\n{USAGE}"
        ));
    };
    let blobs_dir = optional(&flags, "--blobs-dir")?;
    let client = open_client(&flags).await?;
    let conversation = resolve_conversation(&client, wanted)?;
    let contacts = client.contacts()?;
    let me = client.public_key();
    for message in client.history(conversation)? {
        let from = if message.sender == me {
            "me".to_string()
        } else {
            label(&contacts, &message.sender)
        };
        let pending = if message.pending { " [pending]" } else { "" };
        match &message.body {
            Ok(plaintext) => println!("{from}: {}{pending}", String::from_utf8_lossy(plaintext)),
            Err(e) => println!("{from}: <unopenable: {e}>{pending}"),
        }
        match &blobs_dir {
            Some(dir) => {
                for blob_ref in &message.blob_refs {
                    let plaintext = client
                        .fetch_stored_blob(conversation, message.id, &blob_ref.hash)
                        .await?;
                    write_blob(dir, blob_ref, &plaintext)?;
                }
            }
            None if !message.blob_refs.is_empty() => println!(
                "  ({} blob(s) attached; pass --blobs-dir to fetch)",
                message.blob_refs.len()
            ),
            None => {}
        }
    }
    Ok(())
}

/// Reply into a stored conversation: participants resolve back to contact
/// records; unreachable keys are called out, not silently dropped.
async fn reply(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [wanted, text] = positionals.as_slice() else {
        return Err(format!(
            "exactly one conversation id (or prefix) and one text expected\n{USAGE}"
        ));
    };
    let client = open_client(&flags).await?;
    let conversation = resolve_conversation(&client, wanted)?;
    let resolved = client.reply_contacts(conversation)?;
    for key in &resolved.unknown {
        eprintln!(
            "warning: no contact record for participant {} — they will not receive this reply",
            &hex::encode(&key.0)[..8]
        );
    }
    if resolved.contacts.is_empty() {
        return Err("no reachable participants — add their contact records first".into());
    }
    let receipt = client
        .send_in(
            conversation,
            &resolved.contacts,
            text.clone().into_bytes(),
            vec![],
        )
        .await?;
    println!(
        "replied in {} (seq {}) to {} relay(s){}",
        &hex::encode(&receipt.conversation.0)[..8],
        receipt.seq,
        receipt.relay_count,
        pending_note(receipt.pending_relays)
    );
    Ok(())
}

/// Live delivery: subscribe to every relay and print messages as they are
/// nudged in. Runs until killed — the dev-tool sibling of the app's
/// subscription tasks.
async fn listen(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    if !positionals.is_empty() {
        return Err(USAGE.to_string());
    }
    let client = std::sync::Arc::new(open_client(&flags).await?);
    let relays = {
        let given = values(&flags, "--relay");
        if given.is_empty() {
            client.home_relays()
        } else {
            given
        }
    };
    if relays.is_empty() {
        return Err(format!(
            "no relays: pass --relay or set home relays via my-record\n{USAGE}"
        ));
    }
    let contacts = std::sync::Arc::new(client.contacts()?);
    println!("listening on {} relay(s)…", relays.len());
    let mut loops = Vec::new();
    for relay in relays {
        let (client, contacts) = (client.clone(), contacts.clone());
        loops.push(tokio::spawn(async move {
            client
                .subscribe(&relay, |messages| {
                    for message in &messages {
                        let from = label(&contacts, &message.envelope.core.sender);
                        match &message.body {
                            Ok(plaintext) => {
                                println!("{from}: {}", String::from_utf8_lossy(plaintext))
                            }
                            Err(e) => println!("{from}: <unopenable: {e}>"),
                        }
                        if !message.envelope.core.blob_refs.is_empty() {
                            println!(
                                "  ({} blob(s) attached; fetch via history --blobs-dir)",
                                message.envelope.core.blob_refs.len()
                            );
                        }
                    }
                    use std::io::Write;
                    let _ = std::io::stdout().flush(); // piped stdout buffers
                })
                .await;
        }));
    }
    for task in loops {
        let _ = task.await;
    }
    Ok(())
}

/// A full conversation id, or any unambiguous hex prefix of one.
fn resolve_conversation(client: &Client, prefix: &str) -> Result<MessageId, String> {
    let matching: Vec<MessageId> = client
        .conversations()?
        .into_iter()
        .map(|summary| summary.id)
        .filter(|id| hex::encode(&id.0).starts_with(prefix))
        .collect();
    match matching.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("no conversation matches {prefix:?}")),
        _ => Err(format!("{prefix:?} is ambiguous — give more of the id")),
    }
}

/// Petname if the key belongs to a stored contact, else short hex.
fn label(contacts: &[(String, ContactRecord)], key: &PublicKey) -> String {
    contacts
        .iter()
        .find(|(_, record)| record.keys.contains(key))
        .map(|(petname, _)| petname.clone())
        .unwrap_or_else(|| hex::encode(&key.0)[..8].to_string())
}

/// Write one decrypted blob to `dir`, named by hash prefix and kind.
fn write_blob(dir: &str, blob_ref: &BlobRef, plaintext: &[u8]) -> Result<(), String> {
    let kind = match blob_ref.kind {
        BlobKind::Thumbnail => "thumbnail",
        BlobKind::Full => "full",
    };
    let path = Path::new(dir).join(format!(
        "{}-{kind}.bin",
        &hex::encode(&blob_ref.hash.0)[..8]
    ));
    std::fs::write(&path, plaintext).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("  saved {kind} blob to {}", path.display());
    Ok(())
}

/// `--image <file> [--thumb <file>]` → blob drafts (thumbnail first).
fn blob_drafts(flags: &[(String, String)]) -> Result<Vec<BlobDraft>, String> {
    let image = optional(flags, "--image")?;
    let thumb = optional(flags, "--thumb")?;
    let read = |path: &str, kind: BlobKind| -> Result<BlobDraft, String> {
        Ok(BlobDraft {
            kind,
            plaintext: std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?,
        })
    };
    match (image, thumb) {
        (None, None) => Ok(vec![]),
        (None, Some(_)) => Err(format!("--thumb requires --image\n{USAGE}")),
        (Some(image), thumb) => {
            let mut blobs = Vec::new();
            if let Some(thumb) = thumb {
                blobs.push(read(&thumb, BlobKind::Thumbnail)?);
            }
            blobs.push(read(&image, BlobKind::Full)?);
            Ok(blobs)
        }
    }
}

/// Open the client at `--key`, honoring dev/test knobs from the
/// environment — the config edge the lib deliberately doesn't have:
/// `ZINK_CONNECT_TIMEOUT_MS` shrinks the relay-connect deadline (the e2e
/// suite sets it so down-relay tests fail in milliseconds, not the
/// production 10 s).
async fn open_client(flags: &[(String, String)]) -> Result<Client, String> {
    let mut config = ClientConfig::default();
    if let Ok(ms) = std::env::var("ZINK_CONNECT_TIMEOUT_MS") {
        let ms: u64 = ms
            .parse()
            .map_err(|e| format!("ZINK_CONNECT_TIMEOUT_MS: {e}"))?;
        config.connect_timeout = std::time::Duration::from_millis(ms);
    }
    Client::open_with(&single(flags, "--key")?, config).await
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

fn optional(flags: &[(String, String)], name: &str) -> Result<Option<String>, String> {
    match values(flags, name).as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => Err(format!("{name} given more than once\n{USAGE}")),
    }
}
