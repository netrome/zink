//! Native dev/test client (not shipped): a thin argument/printing layer
//! over `zink-client`, which owns all the actual client logic.
//!
//! ```text
//! zink-cli keygen <key-file>                 # new key, prints public key
//! zink-cli pubkey <key-file>                 # prints public key
//! zink-cli send --key <file> --to <pubkey>@<relay>[,<relay>…] [--to …]
//!               [--image <file> [--thumb <file>]] <text>
//! zink-cli recv --key <file> --relay <relay> [--relay …] [--blobs-dir <dir>]
//! ```
//!
//! `<relay>` is a dial string `<endpoint-id>@<ip:port>` as printed by
//! `zink-relay`.

use std::path::Path;
use std::process::ExitCode;

use zink_client::{Client, Contact, Received, hex, keystore};
use zink_protocol::{BlobDraft, BlobKind};

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
  zink-cli send --key <file> --to <pubkey>@<relay>[,<relay>...] [--to ...]
                [--image <file> [--thumb <file>]] <text>
  zink-cli recv --key <file> --relay <relay> [--relay ...] [--blobs-dir <dir>]
(<relay> = <endpoint-id>@<ip:port>, as printed by zink-relay)";

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

async fn send(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let contacts: Vec<Contact> = values(&flags, "--to")
        .iter()
        .map(|spec| Contact::parse(spec))
        .collect::<Result<_, _>>()?;
    if contacts.is_empty() {
        return Err(format!("at least one --to required\n{USAGE}"));
    }
    let [text] = positionals.as_slice() else {
        return Err(format!("exactly one message text expected\n{USAGE}"));
    };
    let blobs = blob_drafts(&flags)?;

    let client = Client::open(&single(&flags, "--key")?).await?;
    let receipt = client
        .send(&contacts, text.clone().into_bytes(), blobs)
        .await?;
    println!(
        "deposited {} (conv {}, seq {}) ({} blob(s)) to {} relay(s)",
        hex::encode(&receipt.id.0),
        &hex::encode(&receipt.conversation.0)[..8],
        receipt.seq,
        receipt.blob_count,
        receipt.relay_count
    );
    Ok(())
}

async fn recv(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    if !positionals.is_empty() {
        return Err(USAGE.to_string());
    }
    let relays = values(&flags, "--relay");
    if relays.is_empty() {
        return Err(format!("at least one --relay required\n{USAGE}"));
    }
    let blobs_dir = optional(&flags, "--blobs-dir")?;

    let client = Client::open(&single(&flags, "--key")?).await?;
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
        let kind = match blob_ref.kind {
            BlobKind::Thumbnail => "thumbnail",
            BlobKind::Full => "full",
        };
        let path = Path::new(dir).join(format!(
            "{}-{kind}.bin",
            &hex::encode(&blob_ref.hash.0)[..8]
        ));
        std::fs::write(&path, &plaintext).map_err(|e| format!("write {}: {e}", path.display()))?;
        println!("  saved {kind} blob to {}", path.display());
    }
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
