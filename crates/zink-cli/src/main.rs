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
//! `<relay>` is the spec printed by `zink-relay`:
//! `<endpoint-id>@<ip:port>[#<relay-url>]` — the mailbox dial string, plus
//! (D0b) the same service's iroh relay URL for peer dial-by-key.

use std::path::Path;
use std::process::ExitCode;

use zink_client::{Client, ClientConfig, Contact, Received, ResolvedName, hex, keystore};
use zink_protocol::{
    BlobDraft, BlobKind, BlobRef, ContactRecord, MessageId, PublicKey, RelayEntry,
};

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
        Some("recognize") => recognize(&args[1..]).await,
        Some("devices") => devices(&args[1..]).await,
        Some("rewrap") => rewrap(&args[1..]).await,
        Some("vouch") => vouch(&args[1..]).await,
        Some("unvouch") => unvouch(&args[1..]).await,
        Some("repudiate") => repudiate(&args[1..]).await,
        Some("send") => send(&args[1..]).await,
        Some("recv") => recv(&args[1..]).await,
        Some("conversations") => conversations(&args[1..]).await,
        Some("history") => history(&args[1..]).await,
        Some("reply") => reply(&args[1..]).await,
        Some("listen") => listen(&args[1..]).await,
        Some("backfill") => backfill(&args[1..]).await,
        Some("who-is") => who_is(&args[1..]).await,
        Some("set-avatar") => set_avatar(&args[1..]).await,
        Some("avatar") => avatar(&args[1..]).await,
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
  zink-cli recognize --key <file> <ZINK:...>
  zink-cli devices --key <file>
  zink-cli rewrap --key <file>
  zink-cli vouch --key <file> <petname>
  zink-cli unvouch --key <file> <petname>
  zink-cli repudiate --key <file> <petname | pubkey-hex>
  zink-cli send --key <file> --to <petname | pubkey@relay[,relay...]> [--to ...]
                [--image <file> [--thumb <file>]] <text>
  zink-cli recv --key <file> [--relay <relay> ...] [--blobs-dir <dir>]
  zink-cli conversations --key <file>
  zink-cli history --key <file> [--blobs-dir <dir>] <conversation-id | prefix>
  zink-cli reply --key <file> [--add <petname> ...] <conversation-id | prefix> <text>
  zink-cli listen --key <file> [--relay <relay> ...]
  zink-cli backfill --key <file> <conversation-id | prefix>
                    <peer-addr | petname | pubkey-hex>
  zink-cli who-is --key <file> <petname | pubkey-hex>
  zink-cli set-avatar --key <file> <image-file>
  zink-cli avatar --key <file> [--out <file>] <petname | pubkey-hex>
(<relay> = <endpoint-id>@<ip:port>[#<relay-url>] as printed by zink-relay;
 <peer-addr> = <endpoint-id>@<ip:port> as printed by `listen`. A petname or
 key backfills by key via the relay url in the stored contact record. recv
 and listen default to the home relays set via my-record)";

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
            // Full specs, not bare dial strings — re-saving the profile must
            // not silently drop the relay URLs (D0b).
            client.home_relay_specs()
        } else {
            given
        }
    };
    let name = name.ok_or(format!("no profile name yet — pass --name\n{USAGE}"))?;
    if relays.is_empty() {
        return Err(format!("no home relay yet — pass --relay\n{USAGE}"));
    }
    client.set_profile(&name, &relays).await?;
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
    client.close().await;
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
    client.close().await;
    Ok(())
}

/// The one-way "recognize this device as me" act (D3b, multi-device.md
/// §3). The dev-tool stand-in for the app's scan + fingerprint confirm:
/// the payload the user pastes IS what they confirm.
async fn recognize(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [payload] = positionals.as_slice() else {
        return Err(format!("exactly one ZINK:... payload expected\n{USAGE}"));
    };
    let record = ContactRecord::from_qr_string(payload).map_err(|e| format!("record: {e}"))?;
    let client = open_client(&flags).await?;
    let key = client.recognize_device(&record)?;
    println!(
        "recognized {} ({}) as this person's device",
        record.self_claimed_name().unwrap_or("<unnamed>"),
        &hex::encode(&key.0)[..8],
    );
    client.close().await;
    Ok(())
}

/// Vouch for a contact (D4a): share your petname for them with anyone who
/// asks you about them. Explicit — nothing vouches on add.
async fn vouch(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [petname] = positionals.as_slice() else {
        return Err(format!("exactly one petname expected\n{USAGE}"));
    };
    let client = open_client(&flags).await?;
    client.vouch(petname)?;
    println!("vouching for {petname:?} — served to anyone who asks you about them");
    client.close().await;
    Ok(())
}

/// Repudiate a key (D4b): "I no longer recognize this key" — published in
/// your record, served with answers, and a repudiated sibling is
/// un-recognized on the spot. Advisory: observers decide what it means.
async fn repudiate(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [target] = positionals.as_slice() else {
        return Err(format!("exactly one petname or key hex expected\n{USAGE}"));
    };
    let client = open_client(&flags).await?;
    let key = resolve_peer_key(&client, target)?;
    client.repudiate(key)?;
    println!(
        "repudiated {} — published in your record; contacts learn it from \
         their next pull",
        &hex::encode(&key.0)[..8],
    );
    client.close().await;
    Ok(())
}

/// Withdraw a vouch: it stops being served; fresh answers replace it away.
async fn unvouch(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [petname] = positionals.as_slice() else {
        return Err(format!("exactly one petname expected\n{USAGE}"));
    };
    let client = open_client(&flags).await?;
    client.unvouch(petname)?;
    println!("no longer vouching for {petname:?}");
    client.close().await;
    Ok(())
}

/// Pull re-wraps for unopenable history from paired devices (D3d) — the
/// dev-tool trigger for the opportunistic run the drain path does itself.
async fn rewrap(args: &[String]) -> Result<(), String> {
    let (flags, _) = parse_flags(args)?;
    let client = open_client(&flags).await?;
    let healed = client.rewrap_backlog().await;
    println!("{healed} message(s) became readable");
    client.close().await;
    Ok(())
}

/// List the own-devices store — this device's recognition set.
async fn devices(args: &[String]) -> Result<(), String> {
    let (flags, _) = parse_flags(args)?;
    let client = open_client(&flags).await?;
    let devices = client.recognized_devices();
    if devices.is_empty() {
        println!("no recognized devices");
    }
    for (key, record) in devices {
        println!(
            "{}  ({})",
            record.self_claimed_name().unwrap_or("<unnamed>"),
            &hex::encode(&key.0)[..8],
        );
    }
    client.close().await;
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
        // Full specs, so "does this record enable dial-by-key?" is visible:
        // an entry without `#<relay-url>` is mailbox-only (D0b).
        let relays: Vec<String> = record.relays.iter().map(RelayEntry::to_spec).collect();
        println!("{petname}  ({})", keys.join(","));
        for spec in relays {
            println!("  relay: {spec}");
        }
    }
    client.close().await;
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
    client.close().await;
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
        client.close().await;
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
    client.close().await;
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
    // The whole own cluster is "me" (D3c): a conversation is never
    // "with mårten laptop".
    let own = client.own_keys();
    for summary in summaries {
        let other_keys: Vec<_> = summary
            .participants
            .iter()
            .copied()
            .filter(|key| !own.contains(key))
            .collect();
        // Deduped per person (multi-device.md §7): a two-device contact
        // labels once.
        let others = client.participant_labels(&other_keys)?;
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
    client.close().await;
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
        // Membership deltas (groups.md §2) — derived, rendered inline.
        for key in &message.joined {
            println!("  [+ {}]", label(&contacts, key));
        }
        for key in &message.left {
            println!("  [- {}]", label(&contacts, key));
        }
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
    client.close().await;
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
            "warning: no route for participant {} — they stay a recipient \
             (membership holds) but receive nothing until a route is learned",
            &hex::encode(&key.0)[..8]
        );
    }
    // --add grows the recipient set (groups.md §2): the signed recipients
    // list is the membership announcement — no other mechanism exists.
    let mut contacts = resolved.contacts;
    for petname in values(&flags, "--add") {
        contacts.push(client.resolve_contact(&petname)?);
    }
    if contacts.iter().all(|contact| contact.relays.is_empty()) {
        return Err("no routable participants — add or learn their records first".into());
    }
    let receipt = client
        .send_in(conversation, &contacts, text.clone().into_bytes(), vec![])
        .await?;
    println!(
        "replied in {} (seq {}) to {} relay(s){}",
        &hex::encode(&receipt.conversation.0)[..8],
        receipt.seq,
        receipt.relay_count,
        pending_note(receipt.pending_relays)
    );
    client.close().await;
    Ok(())
}

/// Pull a conversation's missing ancestors from a peer (D0 sync), walking back
/// to the genesis so a mid-conversation joiner can build the DAG and reply. The
/// peer must be running to serve (e.g. `listen`, which prints its address).
/// The peer is a dial string `<endpoint-id>@<ip:port>`, or — D0b — a contact
/// petname / device-key hex, reached by key via the relay URL in their stored
/// record (holepunched direct, relayed as fallback).
async fn backfill(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [wanted, peer] = positionals.as_slice() else {
        return Err(format!(
            "exactly one conversation id (or prefix) and one peer expected\n{USAGE}"
        ));
    };
    let client = open_client(&flags).await?;
    let conversation = resolve_conversation(&client, wanted)?;
    let fetched = if peer.contains('@') {
        client.backfill(conversation, peer).await?
    } else {
        let key = resolve_peer_key(&client, peer)?;
        client.backfill_by_key(conversation, key).await?
    };
    println!("backfilled {fetched} message(s) from {peer}");
    client.close().await;
    Ok(())
}

/// A peer named on the command line: a contact petname, or a device-key hex.
/// (Contact identity is keyed on the record's first key — the C2 convention,
/// revisited at D3.)
fn resolve_peer_key(client: &Client, peer: &str) -> Result<zink_protocol::PublicKey, String> {
    for (petname, record) in client.contacts()? {
        if petname == *peer {
            return record
                .keys
                .first()
                .copied()
                .ok_or_else(|| "contact record has no keys".to_string());
        }
    }
    Ok(zink_protocol::PublicKey(zink_client::hex::parse32(peer)?))
}

/// Ask every dialable contact "who is this key?" (D1b, who-is-this.md §5).
/// Prints each answer with its provenance and shareable payload (feed it
/// to `contact-add` to promote), then the ranked resolution over
/// everything learned so far. Manual by design — asking reveals your
/// interest in the key to everyone asked.
async fn who_is(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [subject] = positionals.as_slice() else {
        return Err(format!(
            "exactly one subject (petname or key hex) expected\n{USAGE}"
        ));
    };
    let client = open_client(&flags).await?;
    let key = resolve_peer_key(&client, subject)?;
    let outcome = client.who_is(key).await?;
    println!(
        "asked {} contact(s), {} unreachable",
        outcome.asked, outcome.unreachable
    );
    let answers = outcome.answers;
    if answers.is_empty() {
        println!("no answers");
    }
    for answer in &answers {
        let name = answer
            .record
            .self_claimed_name()
            .unwrap_or("(no valid self-claim)");
        println!(
            "{} holds a record: calls themself {name:?} — {}",
            answer.responder_petname,
            answer.record.to_qr_string()
        );
    }
    // Disavowals (D4b): every valid negative renders with WHO says it;
    // only same-person ones exclude from addressing.
    for disavowal in client.disavowals(key)? {
        println!(
            "disavowed by {}{}",
            disavowal.attester_label,
            if disavowal.excludes {
                " — excluded from your replies (their own key disavowed it)"
            } else {
                " (third-party claim — a warning, never an exclusion)"
            }
        );
    }
    // Link evidence (D3c, multi-device.md §7): says WHO claims, tiered —
    // the one-way tier is a claim, the mutual one is consent-proof.
    for evidence in client.device_evidence(key)? {
        match evidence.tier {
            zink_protocol::LinkTier::MutuallyConfirmed => println!(
                "device evidence: {} and this key vouch each other (mutually confirmed)",
                evidence.petname
            ),
            _ => println!(
                "device evidence: {} says this is their device (unconfirmed by the key)",
                evidence.petname
            ),
        }
    }
    match client.resolve_name(key)? {
        ResolvedName::Petname(petname) => println!("resolved: contact {petname:?}"),
        ResolvedName::Learned(names) => {
            for name in names {
                let confirmed = if name.confirmed_by_subject {
                    ", confirmed by themself"
                } else {
                    ""
                };
                let held = if name.held_by.is_empty() {
                    String::new()
                } else {
                    format!(", records held by {}", name.held_by.join(", "))
                };
                let endorsed = if name.endorsed_by.is_empty() {
                    String::new()
                } else {
                    format!(", vouched by {}", name.endorsed_by.join(", "))
                };
                println!(
                    "resolved: {:?} (revision {}){confirmed}{held}{endorsed}",
                    name.name, name.revision
                );
            }
        }
        ResolvedName::Unknown => println!("resolved: unknown key"),
    }
    client.close().await;
    Ok(())
}

/// Set this device's avatar (D1d): encrypt-once, cache, claim, push to the
/// home relays. Re-run `my-record` afterwards — the printed record now
/// carries the avatar claim, and contacts need a record with the claim to
/// fetch (existing contacts pick it up via `who-is` freshness).
async fn set_avatar(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [path] = positionals.as_slice() else {
        return Err(format!("exactly one image file expected\n{USAGE}"));
    };
    let image = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let client = open_client(&flags).await?;
    let receipt = client.set_avatar(image).await?;
    println!(
        "avatar set: hash {} revision {} — pushed to {} relay(s)",
        hex::encode(&receipt.hash.0),
        receipt.revision,
        receipt.pushed_relays
    );
    client.close().await;
    Ok(())
}

/// Fetch + decrypt the best-believed avatar for a contact or key (D1d).
async fn avatar(args: &[String]) -> Result<(), String> {
    let (flags, positionals) = parse_flags(args)?;
    let [subject] = positionals.as_slice() else {
        return Err(format!(
            "exactly one subject (petname or key hex) expected\n{USAGE}"
        ));
    };
    let client = open_client(&flags).await?;
    let key = resolve_peer_key(&client, subject)?;
    match client.avatar(key).await? {
        Some(bytes) => {
            match optional(&flags, "--out")? {
                Some(out) => {
                    std::fs::write(&out, &bytes).map_err(|e| format!("write {out}: {e}"))?;
                    println!("avatar: {} bytes -> {out}", bytes.len());
                }
                None => println!("avatar: {} bytes", bytes.len()),
            };
        }
        None => println!("no avatar"),
    }
    client.close().await;
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
    if let Ok(addr) = client.sync_address() {
        // What a peer dials to backfill history from this device (D0 sync).
        println!("peer sync address: {addr}");
    }
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
    Ok(Client::open_with(&single(flags, "--key")?, config).await?)
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
