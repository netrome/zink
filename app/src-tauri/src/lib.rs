//! zink phone/desktop app (Tauri): thin command layer over `zink-client`.
//! C3b scope: one managed long-lived client; structured DTO commands
//! (`zink-app-dto`) rendered from the *stored DAG*; the webview owns only
//! presentation. Images render in C3c; live delivery is C4.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use data_encoding::BASE64;
use tauri::{AppHandle, Emitter, Manager, State};
use zink_app_dto::{
    AppState, BlobInfo, ContactRow, Conversation, DeviceRow, Message, OutgoingImage, QrPayload,
    RecordPreview, UnknownMember, WhoIsCandidate, WhoIsReport,
};
use zink_client::{Client, ResolvedName, hex};
use zink_protocol::{BlobDraft, BlobHash, BlobKind, ContactRecord, MessageId, PublicKey};

/// The one `Client` for the app's lifetime, created on first use. A single
/// instance means a single endpoint and no two commands racing first-run
/// key creation or the state dir. `subscribed` tracks which home relays
/// already have a live-delivery task; `notified` dedups notifications by
/// message id (with several home relays, more than one loop can deliver
/// the same message).
struct ManagedClient {
    client: tokio::sync::OnceCell<Arc<Client>>,
    subscribed: Mutex<HashSet<String>>,
    notified: Arc<Mutex<HashSet<[u8; 32]>>>,
}

async fn client(
    app: &AppHandle,
    managed: &State<'_, ManagedClient>,
) -> Result<Arc<Client>, String> {
    let client = managed
        .client
        .get_or_try_init(|| async {
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("app data dir: {e}"))?;
            std::fs::create_dir_all(&data_dir).map_err(|e| format!("create data dir: {e}"))?;
            let key_path = data_dir.join("device.key");
            // Rendered via the De1 edge shim, keeping the closure's error
            // type `String` like the rest of this layer.
            let client = Client::open_or_create(&key_path.to_string_lossy())
                .await
                .map(Arc::new)
                .map_err(String::from)?;
            // Re-push the avatar ciphertext once per app run (D1d): relay
            // caches expire (30-day TTL) and the publisher is the only
            // source. Best-effort, off the first command's path.
            let push = client.clone();
            tauri::async_runtime::spawn(async move {
                push.push_avatar().await;
            });
            Ok::<_, String>(client)
        })
        .await?
        .clone();
    spawn_subscriptions(app, managed, &client);
    Ok(client)
}

/// One live-delivery task per home relay (C4b): the subscription loop
/// drains on nudges and reconnects forever; each non-empty drain raises a
/// `new-messages` event (webview re-renders from the store) and posts
/// local notifications (C4c). Called on every command — the `subscribed`
/// set makes it spawn-once, and relays added later (set_profile) get
/// picked up on the next call.
fn spawn_subscriptions(app: &AppHandle, managed: &State<'_, ManagedClient>, client: &Arc<Client>) {
    for relay in client.home_relays() {
        let mut subscribed = managed.subscribed.lock().expect("subscribed lock");
        if !subscribed.insert(relay.clone()) {
            continue;
        }
        drop(subscribed);
        let (app, client) = (app.clone(), client.clone());
        let notified = managed.notified.clone();
        tauri::async_runtime::spawn(async move {
            let on_new_client = client.clone();
            client
                .subscribe(&relay, move |messages| {
                    let _ = app.emit("new-messages", messages.len());
                    notify_arrivals(&app, &on_new_client, &notified, &messages);
                })
                .await;
        });
    }
}

/// Petname + text-preview local notifications, posted after local decrypt
/// (live-delivery.md §5 — resolved: the content never leaves the device;
/// there is no third party anywhere in this path). Skipped while the
/// window is focused (the live view is already updating); deduped by
/// message id.
fn notify_arrivals(
    app: &AppHandle,
    client: &Client,
    notified: &Mutex<HashSet<[u8; 32]>>,
    messages: &[zink_client::Received],
) {
    use tauri_plugin_notification::NotificationExt;
    let focused = app
        .get_webview_window("main")
        .and_then(|window| window.is_focused().ok())
        .unwrap_or(false);
    if focused {
        return;
    }
    let contacts = client.contacts().unwrap_or_default();
    for message in messages {
        let Ok(body) = &message.body else {
            continue; // nothing readable to preview
        };
        if !notified
            .lock()
            .expect("notified lock")
            .insert(message.envelope.id().0)
        {
            continue;
        }
        let text = String::from_utf8_lossy(body);
        let preview: String = if text.trim().is_empty() {
            format!("📎 {} attachment(s)", message.envelope.core.blob_refs.len())
        } else {
            text.chars().take(120).collect()
        };
        let _ = app
            .notification()
            .builder()
            .title(label(&contacts, &message.envelope.core.sender))
            .body(preview)
            .show();
    }
}

#[tauri::command]
async fn app_state(app: AppHandle, managed: State<'_, ManagedClient>) -> Result<AppState, String> {
    let client = client(&app, &managed).await?;
    let record = match client.my_record() {
        Ok(record) => Some(qr_payload(&record)?),
        Err(_) => None, // no profile yet — the UI shows the setup form
    };
    Ok(AppState {
        my_key: hex::encode(&client.public_key().0),
        name: client.profile_name(),
        // The full spec (`dial[#relay-url]`): this value round-trips through
        // the profile form back into set_profile — the bare dial string
        // would silently drop the relay URL on a re-save (D0b).
        relay: client.home_relay_specs().into_iter().next(),
        contacts: client
            .contacts()?
            .into_iter()
            .map(|(petname, record)| ContactRow {
                petname,
                key: record
                    .keys
                    .first()
                    .map(|key| hex::encode(&key.0))
                    .unwrap_or_default(),
            })
            .collect(),
        record,
        devices: client
            .recognized_devices()
            .into_iter()
            .map(|(key, record)| DeviceRow {
                name: record
                    .self_claimed_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| hex::encode(&key.0)[..8].to_string()),
                key: hex::encode(&key.0),
            })
            .collect(),
    })
}

/// Decode a `ZINK:` payload for the pair-mode confirm (D3e) — nothing is
/// stored or signed here; this is what the fingerprint check renders.
#[tauri::command]
async fn inspect_record(payload: String) -> Result<RecordPreview, String> {
    let record = ContactRecord::from_qr_string(&payload).map_err(|e| format!("record: {e}"))?;
    let key = record
        .keys
        .first()
        .ok_or("record has no keys".to_string())?;
    Ok(RecordPreview {
        name: record.self_claimed_name().map(str::to_string),
        key: hex::encode(&key.0),
    })
}

/// The one-way "recognize this device as me" act (D3e, multi-device.md §3)
/// — called only after the UI's explicit fingerprint confirm. Fires an
/// opportunistic re-wrap pull afterward (D3d): if the sibling has already
/// recognized this device back, pre-pairing history becomes readable now;
/// if not, it declines harmlessly.
#[tauri::command]
async fn recognize_device(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    payload: String,
) -> Result<String, String> {
    let record = ContactRecord::from_qr_string(&payload).map_err(|e| format!("record: {e}"))?;
    let client = client(&app, &managed).await?;
    client.recognize_device(&record)?;
    let name = record
        .self_claimed_name()
        .unwrap_or("your device")
        .to_string();
    let rewrapper = client.clone();
    tauri::async_runtime::spawn(async move {
        let healed = rewrapper.rewrap_backlog().await;
        if healed > 0 {
            let _ = app.emit("new-messages", healed);
        }
    });
    Ok(name)
}

/// Introduce this device's siblings to a conversation now (D3c sugar): an
/// empty-body message — send-to-self appends the devices, and the signed
/// recipients list is the announcement. Purely optional; the next organic
/// message would do the same.
#[tauri::command]
async fn introduce_devices(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    if client.recognized_devices().is_empty() {
        return Err("no recognized devices — pair one first".into());
    }
    let conversation = parse_id(&conversation)?;
    let resolved = client.reply_contacts(conversation)?;
    if resolved
        .contacts
        .iter()
        .all(|contact| contact.relays.is_empty())
    {
        return Err("no routable participants".into());
    }
    client
        .send_in(conversation, &resolved.contacts, Vec::new(), vec![])
        .await?;
    Ok(())
}

/// Save name + home relay, register the mailbox there, return the QR.
#[tauri::command]
async fn set_profile(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    name: String,
    relay: String,
) -> Result<QrPayload, String> {
    let client = client(&app, &managed).await?;
    client.set_profile(&name, &[relay]).await?;
    client.register_at_home_relays().await?;
    // The relay may be new — give it its live-delivery task right away.
    spawn_subscriptions(&app, &managed, &client);
    qr_payload(&client.my_record()?)
}

/// Add a contact from a scanned or pasted `ZINK:` payload.
#[tauri::command]
async fn add_contact(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    payload: String,
    petname: Option<String>,
) -> Result<String, String> {
    let record = ContactRecord::from_qr_string(&payload).map_err(|e| format!("record: {e}"))?;
    let client = client(&app, &managed).await?;
    Ok(client.add_contact(&record, petname.filter(|name| !name.trim().is_empty()))?)
}

/// The conversation list, rendered from the stored DAG (not from a recv).
#[tauri::command]
async fn conversations(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
) -> Result<Vec<Conversation>, String> {
    let client = client(&app, &managed).await?;
    // The whole own cluster is "me" (D3c): a conversation is never
    // "with mårten laptop".
    let own = client.own_keys();
    let mut conversations = Vec::new();
    for summary in client.conversations()? {
        let other_keys: Vec<_> = summary
            .participants
            .iter()
            .copied()
            .filter(|key| !own.contains(key))
            .collect();
        // Deduped per person (multi-device.md §7): a two-device contact
        // labels once.
        let others = client.participant_labels(&other_keys)?;
        conversations.push(Conversation {
            id: hex::encode(&summary.id.0),
            label: if others.is_empty() {
                "only me".to_string()
            } else {
                others.join(", ")
            },
            message_count: summary.message_count,
            last_timestamp_ms: summary.last_timestamp_ms,
        });
    }
    Ok(conversations)
}

/// One conversation's messages, linearized, petname-labelled.
#[tauri::command]
async fn messages(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
) -> Result<Vec<Message>, String> {
    let client = client(&app, &managed).await?;
    let conversation = parse_id(&conversation)?;
    let contacts = client.contacts()?;
    let me = client.public_key();
    // Own sibling devices (D3c): label by their self-claimed name, and
    // never flag them as unknown senders — they are this person.
    let devices = client.recognized_devices();
    let device_name = |key: &PublicKey| {
        devices.iter().find(|(k, _)| k == key).map(|(_, record)| {
            record
                .self_claimed_name()
                .map(str::to_string)
                .unwrap_or_else(|| hex::encode(&key.0)[..8].to_string())
        })
    };
    Ok(client
        .history(conversation)?
        .into_iter()
        .map(|message| Message {
            id: hex::encode(&message.id.0),
            conversation: hex::encode(&conversation.0),
            sender: if message.sender == me {
                "me".to_string()
            } else if let Some(name) = device_name(&message.sender) {
                format!("me ({name})")
            } else {
                label(&contacts, &message.sender)
            },
            unknown_sender: (message.sender != me
                && device_name(&message.sender).is_none()
                && !contacts
                    .iter()
                    .any(|(_, record)| record.keys.contains(&message.sender)))
            .then(|| hex::encode(&message.sender.0)),
            sender_key: hex::encode(&message.sender.0),
            joined: message
                .joined
                .iter()
                .map(|key| label(&contacts, key))
                .collect(),
            left: message
                .left
                .iter()
                .map(|key| label(&contacts, key))
                .collect(),
            mine: message.sender == me,
            text: message
                .body
                .ok()
                .map(|body| String::from_utf8_lossy(&body).into_owned()),
            timestamp_ms: message.timestamp_ms,
            pending: message.pending,
            blobs: message
                .blob_refs
                .iter()
                .map(|blob_ref| BlobInfo {
                    hash: hex::encode(&blob_ref.hash.0),
                    kind: match blob_ref.kind {
                        BlobKind::Thumbnail => "thumbnail".to_string(),
                        BlobKind::Full => "full".to_string(),
                    },
                })
                .collect(),
        })
        .collect())
}

/// Fetch + verify + decrypt one blob of a stored message (local cache
/// first, then the home relays); returned base64 for the JSON IPC.
#[tauri::command]
async fn fetch_blob(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
    message: String,
    hash: String,
) -> Result<String, String> {
    let client = client(&app, &managed).await?;
    let bytes = client
        .fetch_stored_blob(
            parse_id(&conversation)?,
            parse_id(&message)?,
            &BlobHash(hex::parse32(&hash)?),
        )
        .await?;
    Ok(BASE64.encode(&bytes))
}

/// Send text — into an existing conversation (reply: participants resolve
/// back to contact records, unreachable keys skipped best-effort), or to a
/// contact by petname (threads via the participant-set index). Returns the
/// conversation id to show.
#[tauri::command]
async fn send_message(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: Option<String>,
    to: Option<Vec<String>>,
    add: Option<Vec<String>>,
    text: String,
    image: Option<OutgoingImage>,
) -> Result<String, String> {
    let adding = add.unwrap_or_default();
    if text.trim().is_empty() && image.is_none() && adding.is_empty() {
        return Err("nothing to send".into());
    }
    let blobs = match image {
        Some(image) => blob_drafts(&image)?,
        None => vec![],
    };
    let client = client(&app, &managed).await?;
    let receipt = match (conversation, to) {
        (Some(conversation), _) => {
            let conversation = parse_id(&conversation)?;
            let resolved = client.reply_contacts(conversation)?;
            // --add grows the recipient set (groups.md §2): the signed
            // recipients list is the membership announcement.
            let mut contacts = resolved.contacts;
            for petname in &adding {
                contacts.push(client.resolve_contact(petname)?);
            }
            // Unroutable members stay recipients (groups.md §2 — membership
            // is not deliverability); only an all-unroutable set is an error.
            if contacts.iter().all(|contact| contact.relays.is_empty()) {
                return Err("no routable participants — add their contacts first".into());
            }
            client
                .send_in(conversation, &contacts, text.into_bytes(), blobs)
                .await?
        }
        (None, Some(petnames)) if !petnames.is_empty() => {
            let contacts: Vec<zink_client::Contact> = petnames
                .iter()
                .map(|petname| client.resolve_contact(petname))
                .collect::<Result<_, _>>()?;
            client.send(&contacts, text.into_bytes(), blobs).await?
        }
        _ => return Err("no conversation or contact given".into()),
    };
    // A successful send proves the network is up: retry any backlog now, but
    // off the command's path so this send's latency doesn't wait on it.
    // (`zink-client` stays runtime-free; the edge owns the spawn.)
    if receipt.pending_relays == 0 {
        let client = client.clone();
        tauri::async_runtime::spawn(async move {
            let _ = client.flush_outbox().await;
        });
    }
    Ok(hex::encode(&receipt.conversation.0))
}

/// Decode a webview-prepared image into the thumbnail + full-res blob pair.
fn blob_drafts(image: &OutgoingImage) -> Result<Vec<BlobDraft>, String> {
    let decode = |b64: &str, what: &str| {
        BASE64
            .decode(b64.as_bytes())
            .map_err(|e| format!("decode {what}: {e}"))
    };
    Ok(vec![
        BlobDraft {
            kind: BlobKind::Thumbnail,
            plaintext: decode(&image.thumb_b64, "thumbnail")?,
        },
        BlobDraft {
            kind: BlobKind::Full,
            plaintext: decode(&image.full_b64, "full image")?,
        },
    ])
}

/// Set this device's avatar from a webview-downscaled image (D1d):
/// encrypt-once, cache, claim at the next revision, push to the home
/// relays. Returns how many relays took the push.
#[tauri::command]
async fn set_avatar(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    image: String,
) -> Result<usize, String> {
    let image = BASE64
        .decode(image.as_bytes())
        .map_err(|e| format!("decode avatar: {e}"))?;
    if !looks_like_image(&image) {
        return Err("that file does not look like an image".into());
    }
    let client = client(&app, &managed).await?;
    let receipt = client.set_avatar(image).await?;
    Ok(receipt.pushed_relays)
}

/// The best-believed avatar for a key, base64 (D1d) — `None` when no
/// avatar is claimed or its blob is currently unfetchable. Decrypted bytes
/// are sniffed before they reach the webview: a claim can name any bytes,
/// but only an image gets rendered.
#[tauri::command]
async fn avatar(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    subject: String,
) -> Result<Option<String>, String> {
    let client = client(&app, &managed).await?;
    let key = PublicKey(hex::parse32(&subject)?);
    Ok(client
        .avatar(key)
        .await?
        .filter(|bytes| looks_like_image(bytes))
        .map(|bytes| BASE64.encode(&bytes)))
}

/// One render-ready candidate row (provenance preformatted — the webview
/// never re-implements naming policy).
fn candidate_dto(learned: zink_client::LearnedName, payload: Option<String>) -> WhoIsCandidate {
    let mut provenance = Vec::new();
    if learned.confirmed_by_subject {
        provenance.push("confirmed by themself".to_string());
    }
    if !learned.held_by.is_empty() {
        provenance.push(format!("records held by {}", learned.held_by.join(", ")));
    }
    // "your friends call them…" (D4a) — the voucher's own claim, named.
    if !learned.endorsed_by.is_empty() {
        provenance.push(format!("vouched by {}", learned.endorsed_by.join(", ")));
    }
    WhoIsCandidate {
        name: learned.name,
        provenance: provenance.join("; "),
        payload,
    }
}

/// The unknown members of a conversation — the "a wild key appeared"
/// surface (D2c, groups.md §5). Candidates render from the learned store
/// (the scoped auto-query fills it at drain time); payloads come from the
/// freshest learned record, so add-as-contact works offline.
#[tauri::command]
async fn unknown_members(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
) -> Result<Vec<UnknownMember>, String> {
    let client = client(&app, &managed).await?;
    let conversation = parse_id(&conversation)?;
    // Own sibling devices are never "unknown members" (D3c).
    let own = client.own_keys();
    let contacts = client.contacts()?;
    let dismissed = client.dismissed();
    let mut members = Vec::new();
    for key in client.membership(conversation)? {
        if own.contains(&key)
            || contacts
                .iter()
                .any(|(_, record)| record.keys.contains(&key))
        {
            continue;
        }
        let candidates = client
            .learned_candidates(key)?
            .into_iter()
            .map(|(learned, record)| candidate_dto(learned, Some(record.to_qr_string())))
            .collect();
        // The popup upgrade (D3c, multi-device.md §7): "P says this is
        // their device", tiered — evidence for the one-tap offer.
        let device_evidence = client
            .device_evidence(key)?
            .into_iter()
            .map(|evidence| match evidence.tier {
                zink_protocol::LinkTier::MutuallyConfirmed => format!(
                    "{} and this key vouch each other (mutually confirmed)",
                    evidence.petname
                ),
                _ => format!(
                    "{} says this is their device (unconfirmed by the key)",
                    evidence.petname
                ),
            })
            .collect();
        members.push(UnknownMember {
            key: hex::encode(&key.0),
            candidates,
            dismissed: dismissed.contains(&key),
            device_evidence,
        });
    }
    Ok(members)
}

/// Ignore an unknown key (D2c): collapses its popup; the key keeps
/// rendering as hex, and manual who-is stays available.
#[tauri::command]
async fn dismiss(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    subject: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    Ok(client.dismiss(PublicKey(hex::parse32(&subject)?))?)
}

/// JPEG / PNG / WebP magic bytes — the formats the webview canvas emits.
fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || (bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

/// Ask contacts "who is this key?" (D1c, who-is-this.md §5) and return a
/// render-ready report: name candidates with provenance for an unknown
/// key, or just the answer count for a contact (the refresh flow — fresh
/// answers sharpen relay resolution by themselves). Manual trigger only —
/// asking reveals the interest to everyone asked.
#[tauri::command]
async fn who_is(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    subject: String,
) -> Result<WhoIsReport, String> {
    let client = client(&app, &managed).await?;
    let key = PublicKey(hex::parse32(&subject)?);
    let outcome = client.who_is(key).await?;
    let answers = outcome.answers;
    let (contact, candidates) = match client.resolve_name(key)? {
        ResolvedName::Petname(petname) => (Some(petname), vec![]),
        ResolvedName::Learned(names) => {
            let candidates = names
                .into_iter()
                .map(|learned| {
                    // The freshest served record claiming this name — what
                    // add_contact promotes. Answers from earlier queries
                    // whose responder is now offline have no payload.
                    let payload = answers
                        .iter()
                        .find(|answer| {
                            answer.record.self_claimed_name() == Some(learned.name.as_str())
                        })
                        .map(|answer| answer.record.to_qr_string());
                    candidate_dto(learned, payload)
                })
                .collect();
            (None, candidates)
        }
        ResolvedName::Unknown => (None, vec![]),
    };
    Ok(WhoIsReport {
        answers: answers.len(),
        asked: outcome.asked,
        unreachable: outcome.unreachable,
        contact,
        candidates,
    })
}

/// Drain the home relays into the store; the UI re-renders from the stored
/// DAG afterwards. Returns how many messages arrived.
#[tauri::command]
async fn refresh(app: AppHandle, managed: State<'_, ManagedClient>) -> Result<usize, String> {
    let client = client(&app, &managed).await?;
    let relays = client.home_relays();
    if relays.is_empty() {
        return Err("set up your profile first".into());
    }
    Ok(client.recv(&relays).await?.len())
}

fn parse_id(id_hex: &str) -> Result<MessageId, String> {
    Ok(MessageId(hex::parse32(id_hex)?))
}

/// Petname if the key belongs to a stored contact, else short hex.
fn label(contacts: &[(String, ContactRecord)], key: &PublicKey) -> String {
    contacts
        .iter()
        .find(|(_, record)| record.keys.contains(key))
        .map(|(petname, _)| petname.clone())
        .unwrap_or_else(|| hex::encode(&key.0)[..8].to_string())
}

fn qr_payload(record: &ContactRecord) -> Result<QrPayload, String> {
    let text = record.to_qr_string();
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| format!("qr: {e}"))?;
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build();
    Ok(QrPayload { svg, text })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Logs to stderr — visible in the `cargo tauri dev` terminal on desktop.
    // (On Android stderr goes nowhere; a logcat layer is a later add.)
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .ok();
    let builder = tauri::Builder::default()
        .manage(ManagedClient {
            client: tokio::sync::OnceCell::new(),
            subscribed: Mutex::default(),
            notified: Arc::default(),
        })
        .plugin(tauri_plugin_notification::init())
        .setup(|_app| {
            // Android 13+ gates notifications behind a runtime permission;
            // ask once at startup, off the main thread (it shows a dialog).
            #[cfg(mobile)]
            {
                use tauri_plugin_notification::NotificationExt;
                let handle = _app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let _ = handle.notification().request_permission();
                });
            }
            Ok(())
        });
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    builder
        .invoke_handler(tauri::generate_handler![
            app_state,
            set_profile,
            add_contact,
            conversations,
            messages,
            send_message,
            fetch_blob,
            refresh,
            who_is,
            set_avatar,
            avatar,
            unknown_members,
            inspect_record,
            recognize_device,
            introduce_devices,
            dismiss
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application")
}
