//! zink phone/desktop app (Tauri): thin command layer over `zink-client`.
//! C3b scope: one managed long-lived client; structured DTO commands
//! (`zink-app-dto`) rendered from the *stored DAG*; the webview owns only
//! presentation. Images render in C3c; live delivery is C4.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use data_encoding::BASE64;
use tauri::{AppHandle, Emitter, Manager, State};
use zink_app_dto::{
    AddPreview, AppState, BlobInfo, ContactRow, Conversation, ConversationMembers, DeviceCard,
    DeviceRow, FriendLens, Inbox, Message, OutgoingImage, PersonInfo, PersonPage, PersonRef,
    QrPayload, RecordPreview, RelayRow, StrangerInfo, SubjectAsk, UnknownMember, WhoIsCandidate,
    WhoIsReport,
};
use zink_client::{Client, RecordMatch, RecordUpdate, RelaySource, ResolvedName, hex};
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
            // Direct delivery (D5): a peer hands the message straight to
            // this device, so there is no mailbox and no nudge — this sink
            // is the only live signal. Treated exactly like a nudge drain:
            // render + notify at once, then heal (auto-sync/who-is/re-wrap)
            // and render again, since healing may pull in the ancestors that
            // make the conversation whole.
            let sink_app = app.clone();
            let sink_notified = managed.notified.clone();
            let sink_client = client.clone();
            client.on_direct_delivery(move |messages| {
                let _ = sink_app.emit("new-messages", messages.len());
                notify_arrivals(&sink_app, &sink_client, &sink_notified, &messages);
                let (app, client) = (sink_app.clone(), sink_client.clone());
                tauri::async_runtime::spawn(async move {
                    client.after_direct(&messages).await;
                    let _ = app.emit("new-messages", messages.len());
                });
            });
            // Re-push the avatar ciphertext once per app run (D1d): relay
            // caches expire (30-day TTL) and the publisher is the only
            // source. Best-effort, off the first command's path.
            let push = client.clone();
            tauri::async_runtime::spawn(async move {
                push.push_avatar().await;
            });
            // C4c-i heartbeat: a Dozed/frozen process can't beat, so gaps
            // between these lines — and `late_ms` spikes — are the freeze
            // detector the overnight diagnosis reads.
            tauri::async_runtime::spawn(async {
                let mut beat = 0u64;
                let mut last = std::time::Instant::now();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    beat += 1;
                    let late_ms = last.elapsed().as_millis().saturating_sub(60_000) as u64;
                    last = std::time::Instant::now();
                    tracing::info!(beat, late_ms, "heartbeat");
                }
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
    // Own devices don't notify (De8 sweep): send-to-self (D3c) deposits every
    // message to your other devices, so typing on the laptop used to buzz the
    // phone — labelled as raw hex, since `label` doesn't read the devices
    // store. You know what you just sent; the chat view still updates.
    //
    // **Receiver-side on purpose.** This device already holds
    // `recognized_devices`, so it can tell a sibling from a contact without
    // being told: a sender-set "don't notify" flag would put UX policy on the
    // wire to carry a fact the receiver already has — and would enforce
    // nothing anyway (tenet 3). A *silent-send* feature, where the sender
    // genuinely knows something we don't, is a different question and belongs
    // in the encrypted body as an advisory convention.
    let own = client.own_keys();
    for message in messages {
        let Ok(body) = &message.body else {
            continue; // nothing readable to preview
        };
        if own.contains(&message.envelope.core.sender) {
            continue;
        }
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
        // Title precedence (S6): a locally named conversation names the
        // notification too — still local data only, nothing from a push.
        let sender = label(&contacts, &message.envelope.core.sender);
        let conversation = message
            .envelope
            .core
            .conversation
            .unwrap_or_else(|| message.envelope.id());
        let title = match client.conversation_name(conversation) {
            Some(name) => format!("{name} — {sender}"),
            None => sender,
        };
        let _ = app
            .notification()
            .builder()
            .title(title)
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
        device_label: client.device_label(),
        // The full specs (`dial[#relay-url]`): these round-trip through the
        // profile form back into set_profile — a bare dial string would
        // silently drop the relay URL on a re-save (D0b). All of them now
        // (U5 multi-relay), not just the first.
        relays: client.home_relay_specs(),
        // One row per PERSON (S2/S3, cluster-first): the people list and
        // the picker render the lens, never the per-device entries.
        contacts: {
            let mut rows = Vec::new();
            for person in client.persons()? {
                let keys: Vec<PublicKey> = person.keys().into_iter().collect();
                let first = person
                    .members
                    .first()
                    .and_then(|(_, record)| record.keys.first().copied());
                let mut disavowals = Vec::new();
                for &key in &keys {
                    disavowals.extend(disavowal_lines(&client, key)?);
                }
                rows.push(ContactRow {
                    id: person.id.to_string(),
                    petname: person.label.clone(),
                    self_name: person
                        .members
                        .iter()
                        .find_map(|(_, record)| record.self_claimed_name())
                        .map(str::to_string),
                    key: first.map(|key| hex::encode(&key.0)).unwrap_or_default(),
                    keys: keys.iter().map(|key| hex::encode(&key.0)).collect(),
                    members: person.members.len(),
                    vouched: keys.iter().any(|key| client.vouches(key)),
                    disavowals,
                });
            }
            rows
        },
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

/// Render-ready disavowal warnings for a key (D4c): every valid negative
/// says WHO; only same-person ones exclude from replies.
fn disavowal_lines(client: &Client, key: PublicKey) -> Result<Vec<String>, String> {
    Ok(client
        .disavowals(key)?
        .into_iter()
        .map(|disavowal| {
            if disavowal.excludes {
                format!(
                    "⚠ disavowed by {} — excluded from your replies",
                    disavowal.attester_label
                )
            } else {
                format!(
                    "⚠ {} disavows this key (third-party claim — a warning, not an exclusion)",
                    disavowal.attester_label
                )
            }
        })
        .collect())
}

/// Vouch for a contact (D4c): share your petname for them with anyone who
/// asks you about them. Explicit — nothing vouches on add.
#[tauri::command]
async fn vouch(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    petname: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.vouch(&petname)?;
    Ok(())
}

/// Withdraw a vouch: it stops being served; fresh answers replace it away.
#[tauri::command]
async fn unvouch(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    petname: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.unvouch(&petname)?;
    Ok(())
}

/// Repudiate a key (D4c): published in your record and served with your
/// answers — the friend-assisted recovery's second act, and the
/// lost-device act on your own devices. Advisory: observers decide.
#[tauri::command]
async fn repudiate_key(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    key: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.repudiate(PublicKey(hex::parse32(&key)?))?;
    Ok(())
}

/// Un-recognize a device, locally only (D4c): losing interest is not the
/// same as declaring it compromised — that is `repudiate_key`.
#[tauri::command]
async fn unrecognize_device(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    key: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.unrecognize_device(&PublicKey(hex::parse32(&key)?));
    Ok(())
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

/// Save name + home relays (U5 multi-relay: the full set replaces the old),
/// register the mailboxes there, return the QR. `device_label` is the
/// optional qualifier beside the name ("phone", "laptop" — SPEC §3.2,
/// S1's two calm questions); empty keeps the current one.
/// (`rename_all`: tauri v2 expects camelCase invoke keys by default, and
/// the webview sends this arg as `device_label` — without the attribute
/// the Option silently arrives `None`.)
#[tauri::command(rename_all = "snake_case")]
async fn set_profile(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    name: String,
    relays: Vec<String>,
    device_label: Option<String>,
) -> Result<QrPayload, String> {
    let client = client(&app, &managed).await?;
    client.set_profile(&name, &relays).await?;
    if let Some(label) = device_label.filter(|label| !label.trim().is_empty()) {
        client.set_device_label(&label)?;
    }
    // Best-effort (R4, 5-relay-lifecycle §8): the profile is saved and the
    // QR must render even when the relay is unreachable — that QR is the
    // recovery artifact. An unreachable own relay surfaces through the
    // send/recv paths (R3), not by walling off the save.
    if let Err(error) = client.register_at_home_relays().await {
        tracing::warn!(%error, "relay registration failed; profile saved anyway");
    }
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

/// Triage a scanned/pasted payload before storing (R1): the UI routes on
/// `updates` — `Some(petname)` opens the update-confirm card, `None` flows
/// to the plain add. An ambiguous record (spans two contacts) errors here
/// with the same message storing it would produce.
#[tauri::command]
async fn preview_contact(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    payload: String,
) -> Result<AddPreview, String> {
    let record = ContactRecord::from_qr_string(&payload).map_err(|e| format!("record: {e}"))?;
    let client = client(&app, &managed).await?;
    match client.preview_contact(&record)? {
        RecordMatch::New { suggested_petname } => Ok(AddPreview {
            updates: None,
            changes: vec![],
            name: suggested_petname,
        }),
        RecordMatch::Update(update) => Ok(AddPreview {
            name: update.new_name.clone(),
            changes: change_lines(&update),
            updates: Some(update.petname),
        }),
        RecordMatch::Ambiguous { petnames } => Err(format!(
            "record shares keys with multiple contacts ({}) — not stored",
            petnames.join(", ")
        )),
    }
}

/// The update card's change list, render-ready (petnames stay untouched —
/// the card only shows what *their* record changes).
fn change_lines(update: &RecordUpdate) -> Vec<String> {
    let claim = |name: &Option<String>| name.clone().unwrap_or_else(|| "(none)".to_string());
    let mut lines = Vec::new();
    if update.old_name != update.new_name {
        lines.push(format!(
            "name: {} → {}",
            claim(&update.old_name),
            claim(&update.new_name)
        ));
    }
    for relay in &update.relays_added {
        lines.push(format!("+ relay {relay}"));
    }
    for relay in &update.relays_removed {
        lines.push(format!("− relay {relay}"));
    }
    if update.keys_added > 0 {
        lines.push(format!("+ {} device key(s)", update.keys_added));
    }
    if update.keys_removed > 0 {
        lines.push(format!("− {} device key(s)", update.keys_removed));
    }
    lines
}

/// The confirmed update act (R1): replace the overlapped contact's stored
/// record with the scanned one. The petname is untouched — renaming lives
/// on the person page.
#[tauri::command]
async fn update_contact(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    payload: String,
) -> Result<String, String> {
    let record = ContactRecord::from_qr_string(&payload).map_err(|e| format!("record: {e}"))?;
    let client = client(&app, &managed).await?;
    Ok(client.update_contact(&record)?)
}

/// Rename a contact — set my petname for them (my lens, U4). Local only;
/// sharing that name with friends is the separate, explicit `vouch`.
#[tauri::command]
async fn rename_contact(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    current: String,
    new: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.rename(&current, &new)?;
    Ok(())
}

/// Set a local photo for a contact (U6, my lens): a webview-downscaled
/// image, stored plaintext on this device only — never published, never a
/// claim. It overrides the resolved self-claim everywhere `avatar` is shown.
#[tauri::command]
async fn set_local_avatar(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    key: String,
    image: String,
) -> Result<(), String> {
    let image = BASE64
        .decode(image.as_bytes())
        .map_err(|e| format!("decode avatar: {e}"))?;
    if !looks_like_image(&image) {
        return Err("that file does not look like an image".into());
    }
    let client = client(&app, &managed).await?;
    client.set_local_avatar(PublicKey(hex::parse32(&key)?), image)?;
    Ok(())
}

/// Drop the local photo for a contact — their self-claimed avatar shows again.
#[tauri::command]
async fn clear_local_avatar(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    key: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.clear_local_avatar(PublicKey(hex::parse32(&key)?));
    Ok(())
}

/// The person page for a contact person, by opaque person id (project 7
/// S3; ids identify, labels display). Renders **local stores only** —
/// opening a page never queries anyone; the page's network acts
/// (`page_refresh`, `ask_friend`, `who_is`) are separate, explicit
/// commands.
#[tauri::command]
async fn person_page(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    id: String,
) -> Result<PersonPage, String> {
    let client = client(&app, &managed).await?;
    let person = client.person_by_id(id.parse()?)?;
    person_page_dto(&client, person)
}

/// The page for a bare key (S3): a key belonging to a person lands on that
/// person's page; anything else renders the stranger variant — everything
/// this device already believes, plus the acts. Own keys live in Me.
#[tauri::command]
async fn key_page(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    key: String,
) -> Result<PersonPage, String> {
    let client = client(&app, &managed).await?;
    let subject = PublicKey(hex::parse32(&key)?);
    if client.own_keys().contains(&subject) {
        return Err("this key is you — it lives in the Me view".into());
    }
    if let Some(person) = client.persons()?.into_iter().find(|person| {
        person
            .members
            .iter()
            .any(|(_, record)| record.keys.contains(&subject))
    }) {
        return person_page_dto(&client, person);
    }
    stranger_page_dto(&client, subject)
}

fn person_page_dto(
    client: &Client,
    person: zink_client::PersonEntry,
) -> Result<PersonPage, String> {
    let can_split = person.members.len() > 1;
    let mut devices = Vec::new();
    for (petname, record) in &person.members {
        devices.push(device_card(client, petname, record, can_split)?);
    }
    let first = person
        .members
        .first()
        .and_then(|(_, record)| record.keys.first().copied());
    // The through-friends lens, merged across member keys: one row per
    // friend, whatever they've told us about any of the person's devices.
    let mut friends = BTreeMap::new();
    for key in person.keys() {
        merge_friend_views(client, key, &mut friends)?;
    }
    let merge_candidates = client
        .persons()?
        .into_iter()
        .filter(|other| other.id != person.id)
        .map(|other| PersonRef {
            id: other.id.to_string(),
            label: other.label,
        })
        .collect();
    Ok(PersonPage {
        label: person.label.clone(),
        person: Some(PersonInfo {
            id: person.id.to_string(),
            merge_candidates,
        }),
        stranger: None,
        avatar_key: first.map(|key| hex::encode(&key.0)).unwrap_or_default(),
        has_local_avatar: first
            .map(|key| client.has_local_avatar(&key))
            .unwrap_or(false),
        devices,
        friends: friends.into_values().collect(),
    })
}

/// The stranger variant: a one-card degenerate cluster from the learned
/// store — candidates, evidence, claims, route, and the pair-back offer.
fn stranger_page_dto(client: &Client, subject: PublicKey) -> Result<PersonPage, String> {
    let learned = client.learned_candidates(subject)?;
    let best_record = learned.first().map(|(_, record)| record.clone());
    let label = learned
        .first()
        .map(|(name, _)| name.name.clone())
        .unwrap_or_else(|| hex::encode(&subject.0)[..8].to_string());
    let candidates = learned
        .into_iter()
        .map(|(name, record)| candidate_dto(name, Some(record.to_qr_string())))
        .collect();
    // Route + provenance, per the same read-time resolution sends use;
    // strangers get no debt lines (nothing is owed to an unadded key).
    let (relay_source, relays, relay_override) = match client.relay_resolution(subject) {
        Some(resolution) => (
            relay_source_line(&resolution.source),
            resolution
                .relays
                .iter()
                .map(|entry| RelayRow {
                    spec: entry.to_spec(),
                    owed: None,
                })
                .collect(),
            resolution.source == RelaySource::Override,
        ),
        None => (
            "no route known — ask a friend, or scan their code".to_string(),
            vec![],
            false,
        ),
    };
    let mut friends = BTreeMap::new();
    merge_friend_views(client, subject, &mut friends)?;
    Ok(PersonPage {
        label,
        person: None,
        stranger: Some(StrangerInfo {
            key: hex::encode(&subject.0),
            candidates,
            dismissed: client.dismissed().contains(&subject),
            pair_back: client
                .claims_to_be_my_device(subject)
                .map(|record| record.to_qr_string()),
        }),
        avatar_key: hex::encode(&subject.0),
        has_local_avatar: client.has_local_avatar(&subject),
        devices: vec![DeviceCard {
            petname: hex::encode(&subject.0)[..8].to_string(),
            device_label: best_record
                .as_ref()
                .and_then(|record| record.self_device_label())
                .map(str::to_string),
            self_name: best_record
                .as_ref()
                .and_then(|record| record.self_claimed_name())
                .map(str::to_string),
            key: hex::encode(&subject.0),
            link: evidence_lines(client, subject)?,
            disavowals: disavowal_lines(client, subject)?,
            relay_source,
            relays,
            relay_override,
            vouched: false,
            can_split: false,
        }],
        friends: friends.into_values().collect(),
    })
}

/// One member device, my belief per key: labels, claims, evidence,
/// warnings, and **that device's** relays (they bind to the publishing
/// device — SPEC §3.6).
fn device_card(
    client: &Client,
    petname: &str,
    record: &ContactRecord,
    can_split: bool,
) -> Result<DeviceCard, String> {
    let key = record
        .keys
        .first()
        .copied()
        .ok_or("stored record has no keys")?;
    let status = client.relay_status(petname)?;
    Ok(DeviceCard {
        petname: petname.to_string(),
        device_label: record.self_device_label().map(str::to_string),
        self_name: record.self_claimed_name().map(str::to_string),
        key: hex::encode(&key.0),
        link: evidence_lines(client, key)?,
        disavowals: disavowal_lines(client, key)?,
        relay_source: relay_source_line(&status.source),
        relay_override: status.source == RelaySource::Override,
        relays: relay_rows(status.relays),
        vouched: client.vouches(&key),
        can_split,
    })
}

/// Verified link evidence, render-ready (D3c): WHO claims, tiered — the
/// one-way tier is a claim, the mutual one is consent-proof.
fn evidence_lines(client: &Client, key: PublicKey) -> Result<Vec<String>, String> {
    Ok(client
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
        .collect())
}

/// Fold one subject's friend views into the page's per-friend lens rows.
fn merge_friend_views(
    client: &Client,
    subject: PublicKey,
    into: &mut BTreeMap<String, FriendLens>,
) -> Result<(), String> {
    for view in client.friend_views(subject)? {
        let lens = into
            .entry(view.petname.clone())
            .or_insert_with(|| FriendLens {
                petname: view.petname.clone(),
                vouched_name: None,
                held: Vec::new(),
            });
        if lens.vouched_name.is_none() {
            lens.vouched_name = view.vouched_name.clone();
        }
        let claimed = match (
            view.record.self_claimed_name(),
            view.record.self_device_label(),
        ) {
            (Some(name), Some(label)) => format!("\u{201c}{name} · {label}\u{201d}"),
            (Some(name), None) => format!("\u{201c}{name}\u{201d}"),
            (None, Some(label)) => format!("a \u{201c}{label}\u{201d}"),
            (None, None) => "no valid self-claim".to_string(),
        };
        lens.held.push(format!(
            "holds their record — {claimed}, {}",
            ago(view.received_ms)
        ));
    }
    Ok(())
}

/// Render-ready provenance for a relay resolution (R5).
fn relay_source_line(source: &RelaySource) -> String {
    match source {
        RelaySource::Override => "you set these by hand — their record is not in use".to_string(),
        RelaySource::SubjectServed { received_ms } => {
            format!("served by them · {}", ago(*received_ms))
        }
        RelaySource::Scanned => "from the record you scanned".to_string(),
        RelaySource::Hearsay { received_ms } => {
            format!("heard from a contact · {}", ago(*received_ms))
        }
    }
}

/// Relay rows with per-relay debt lines (R5).
fn relay_rows(relays: Vec<zink_client::RelayHealth>) -> Vec<RelayRow> {
    relays
        .into_iter()
        .map(|relay| RelayRow {
            spec: relay.spec,
            owed: (relay.owed > 0).then(|| {
                let since = relay
                    .owed_since_ms
                    .map(|ms| format!(" · oldest {}", ago(ms)))
                    .unwrap_or_default();
                format!("⚠ {} message(s) queued for this relay{since}", relay.owed)
            }),
        })
        .collect()
}

/// The page-open subject-refresh (S3): rate-limited, and it only ever asks
/// the subject about themself — no third party learns the page was opened;
/// a stranger's keys are a no-op. Returns whether fresh data landed (the
/// UI re-fetches the page when true).
#[tauri::command]
async fn page_refresh(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    keys: Vec<String>,
) -> Result<bool, String> {
    let client = client(&app, &managed).await?;
    let mut healed = false;
    for key in keys {
        healed |= client.refresh_contact(PublicKey(hex::parse32(&key)?)).await;
    }
    Ok(healed)
}

/// The per-friend lens ask (S3): `who_is_among` scoped to exactly one
/// friend, about each of the person's keys. The friend learns you asked —
/// the UI copy says so plainly; nobody else is dialed.
#[tauri::command]
async fn ask_friend(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    keys: Vec<String>,
    friend: String,
) -> Result<WhoIsReport, String> {
    let client = client(&app, &managed).await?;
    let responder = client
        .contacts()?
        .into_iter()
        .find(|(petname, _)| *petname == friend)
        .and_then(|(_, record)| record.keys.first().copied())
        .ok_or_else(|| format!("no contact named {friend:?}"))?;
    let (mut answers, mut asked, mut unreachable) = (0, 0, 0);
    for key in keys {
        let subject = PublicKey(hex::parse32(&key)?);
        let outcome = client.who_is_among(subject, &[responder]).await?;
        answers += outcome.answers.len();
        asked = asked.max(outcome.asked);
        unreachable = unreachable.max(outcome.unreachable);
    }
    Ok(WhoIsReport {
        answers,
        asked,
        unreachable,
        contact: None,
        candidates: vec![],
        disavowals: vec![],
    })
}

/// Rename a person — the addressing label, my lens (S2). Local only.
#[tauri::command]
async fn rename_person(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    id: String,
    new: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    Ok(client.rename_person(id.parse()?, &new)?)
}

/// Merge one person into another — the explicit clustering act (S2):
/// evidence offers, this act decides. `into` / `from` are opaque person
/// ids (ids identify; labels display).
#[tauri::command]
async fn merge_persons(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    into: String,
    from: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    client.merge_persons(into.parse()?, from.parse()?)?;
    Ok(())
}

/// Split a member device back out to its own person (S2) — the undo of a
/// merge. Returns the split-off person's label.
#[tauri::command]
async fn split_person(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    member: String,
) -> Result<String, String> {
    let client = client(&app, &managed).await?;
    Ok(client.split_person(&member)?.label)
}

/// Relative wall time for provenance lines ("2 h ago") — presentation at
/// the edge, deliberately coarse.
fn ago(then_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);
    let elapsed = now_ms.saturating_sub(then_ms);
    match elapsed {
        ms if ms < 60_000 => "just now".to_string(),
        ms if ms < 60 * 60_000 => format!("{} min ago", ms / 60_000),
        ms if ms < 24 * 60 * 60_000 => format!("{} h ago", ms / (60 * 60_000)),
        ms => format!("{} d ago", ms / (24 * 60 * 60_000)),
    }
}

/// Set (or clear, with an empty list) the manual relay override for a
/// contact (R5) — the person page's escape hatch when their record is
/// stale and a rescan isn't at hand.
#[tauri::command]
async fn set_relay_override(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    petname: String,
    relays: Vec<String>,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    Ok(client.set_relay_override(&petname, &relays)?)
}

/// The conversation list, rendered from the stored DAG (not from a recv).
#[tauri::command]
async fn conversations(app: AppHandle, managed: State<'_, ManagedClient>) -> Result<Inbox, String> {
    let client = client(&app, &managed).await?;
    // The whole own cluster is "me" (D3c): a conversation is never
    // "with mårten laptop".
    let own = client.own_keys();
    // Unknown senders are quarantined into a bounded requests queue by the
    // client core (groups.md §6), so CLI and app cannot drift on the rule.
    let inbox = zink_client::triage(client.conversations()?);
    let dropped = inbox.dropped;
    let mut conversations = Vec::new();
    let mut requests = Vec::new();
    for summary in inbox.conversations.iter().chain(inbox.requests.iter()) {
        let row = conversation_row(&client, &own, summary)?;
        if row.request {
            requests.push(row);
        } else {
            conversations.push(row);
        }
    }
    Ok(Inbox {
        conversations,
        requests,
        dropped,
    })
}

/// The conversation-label rule — "only me" when nobody else is there.
/// Shared by list rows and the members panel, so the two can't drift.
fn conversation_label(others: &[String]) -> String {
    if others.is_empty() {
        "only me".to_string()
    } else {
        others.join(", ")
    }
}

/// Display labels for the participants outside the own cluster, deduped per
/// person (multi-device.md §7): a two-device contact labels once.
fn other_labels(
    client: &Client,
    own: &std::collections::BTreeSet<PublicKey>,
    participants: &[PublicKey],
) -> Result<Vec<String>, String> {
    let other_keys: Vec<_> = participants
        .iter()
        .copied()
        .filter(|key| !own.contains(key))
        .collect();
    Ok(client.participant_labels(&other_keys)?)
}

/// One dto row from a summary: participants labelled minus the own cluster,
/// "only me" when alone.
fn conversation_row(
    client: &Client,
    own: &std::collections::BTreeSet<PublicKey>,
    summary: &zink_client::ConversationSummary,
) -> Result<Conversation, String> {
    let others = other_labels(client, own, &summary.participants)?;
    // Label precedence (S6): my local name, else the participant default —
    // one conversation with a name of its own is what tells three same-set
    // chats apart.
    let label = summary
        .local_name
        .clone()
        .unwrap_or_else(|| conversation_label(&others));
    // The row preview (S5): "who: what" — client-side policy over the
    // local plaintext store; nothing new transits a relay.
    let snippet = match &summary.last {
        None => String::new(),
        Some(last) => {
            let what = match &last.body {
                None => "🔒 can't read this yet".to_string(),
                Some(bytes) => {
                    let text = String::from_utf8_lossy(bytes);
                    let text = text.trim();
                    if !text.is_empty() {
                        text.chars().take(120).collect()
                    } else if last.has_blobs {
                        "📎 image".to_string()
                    } else {
                        String::new() // a bare membership change — no preview
                    }
                }
            };
            if what.is_empty() {
                String::new()
            } else if own.contains(&last.sender) {
                format!("you: {what}")
            } else if others.len() > 1 {
                // A group names the speaker; a 1:1's label already does.
                let who = client
                    .participant_labels(&[last.sender])?
                    .pop()
                    .unwrap_or_default();
                format!("{who}: {what}")
            } else {
                what
            }
        }
    };
    // A request row's preview handle (S3): the unknown sender's key —
    // the newest message's sender when unknown, else the first unknown
    // participant. Opens the person page without opening the chat.
    let stranger_key = (!summary.known)
        .then(|| {
            let contacts = client.contacts().unwrap_or_default();
            let unknown = |key: &PublicKey| {
                !own.contains(key) && !contacts.iter().any(|(_, record)| record.keys.contains(key))
            };
            summary
                .last
                .as_ref()
                .map(|last| last.sender)
                .filter(unknown)
                .or_else(|| {
                    summary
                        .participants
                        .iter()
                        .copied()
                        .find(|key| unknown(key))
                })
                .map(|key| hex::encode(&key.0))
        })
        .flatten();
    Ok(Conversation {
        id: hex::encode(&summary.id.0),
        label,
        message_count: summary.message_count,
        last_timestamp_ms: summary.last_timestamp_ms,
        snippet,
        unread: summary.unread,
        request: !summary.known,
        stranger_key,
    })
}

/// The members panel (project 6 S2): every current member labelled, the
/// header label re-derived, and the contact petnames among the members —
/// what the add-picker excludes. Membership is heads-based (groups.md §2);
/// unknown keys label as short hex (the wild-key panel owns their flow).
#[tauri::command]
async fn conversation_members(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
) -> Result<ConversationMembers, String> {
    let client = client(&app, &managed).await?;
    let id = parse_id(&conversation)?;
    let membership: Vec<PublicKey> = client.membership(id)?.into_iter().collect();
    let own = client.own_keys();
    let others = other_labels(&client, &own, &membership)?;
    let mut members = vec!["you".to_string()];
    members.extend(others.iter().cloned());
    // Person labels (S2/S3): the add-picker rows are persons, so the
    // exclusion list must speak the same names.
    let petnames = client
        .persons()?
        .into_iter()
        .filter(|person| person.keys().iter().any(|key| membership.contains(key)))
        .map(|person| person.label)
        .collect();
    // Label precedence (S6): my local name outranks the participant
    // default, here exactly as in the rows — one rule, or headers and
    // lists would disagree.
    let local_name = client.conversation_name(id);
    Ok(ConversationMembers {
        label: local_name
            .clone()
            .unwrap_or_else(|| conversation_label(&others)),
        local_name,
        members,
        petnames,
    })
}

/// Set or clear my local name for a conversation (project 6 S6) — my
/// lens, this device only, never transmitted. Blank clears.
#[tauri::command]
async fn name_conversation(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
    name: String,
) -> Result<(), String> {
    let client = client(&app, &managed).await?;
    let id = parse_id(&conversation)?;
    let name = name.trim();
    client.set_conversation_name(id, (!name.is_empty()).then_some(name))?;
    Ok(())
}

/// The conversations a person is in (project 6 S2): membership intersects
/// their key cluster. Newest-first, like `conversations`.
#[tauri::command]
async fn person_conversations(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    id: String,
) -> Result<Vec<Conversation>, String> {
    let client = client(&app, &managed).await?;
    let own = client.own_keys();
    // The whole person (S2): any member device's key counts.
    let keys: Vec<PublicKey> = client.person_by_id(id.parse()?)?.keys().into_iter().collect();
    let mut rows = Vec::new();
    for summary in client.conversations()? {
        if summary.participants.iter().any(|key| keys.contains(key)) {
            rows.push(conversation_row(&client, &own, &summary)?);
        }
    }
    Ok(rows)
}

/// The stored conversations whose people-set is exactly the given contacts —
/// the draft view's discovery list ("you already have N with these people").
/// People, not keys: the own cluster counts as "me", a contact matches
/// through any key of their cluster, and a conversation holding a key that
/// is neither is not a match. Newest-first, like `conversations`.
#[tauri::command]
async fn conversations_with(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    to: Vec<String>,
) -> Result<Vec<Conversation>, String> {
    let client = client(&app, &managed).await?;
    let picked: std::collections::BTreeSet<String> = to.into_iter().collect();
    if picked.is_empty() {
        return Ok(vec![]);
    }
    let own = client.own_keys();
    let mut allowed = own.clone();
    let mut clusters = Vec::new();
    for (petname, record) in client.contacts()? {
        if picked.contains(&petname) {
            allowed.extend(record.keys.iter().copied());
            clusters.push(record.keys.clone());
        }
    }
    if clusters.len() != picked.len() {
        return Ok(vec![]); // a name that resolves to no contact matches nothing
    }
    let mut rows = Vec::new();
    for summary in client.conversations()? {
        let every_member_picked = summary.participants.iter().all(|key| allowed.contains(key));
        let every_pick_member = clusters
            .iter()
            .all(|keys| keys.iter().any(|key| summary.participants.contains(key)));
        if every_member_picked && every_pick_member {
            rows.push(conversation_row(&client, &own, &summary)?);
        }
    }
    Ok(rows)
}

/// One conversation's messages, linearized, petname-labelled.
#[tauri::command]
async fn messages(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    conversation: String,
) -> Result<Vec<Message>, String> {
    // A message owed longer than this is rendered "can't reach their
    // relay" instead of "sending…" (R3) — long enough that a relay restart
    // or a slow flush never alarms, short enough to be actionable. Edge
    // policy, deliberately not in the client.
    const STUCK_AFTER_MS: u64 = 10 * 60 * 1000;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);
    let client = client(&app, &managed).await?;
    let conversation = parse_id(&conversation)?;
    // Rendering is reading (S7): this command runs on open and on every
    // arrival while the chat is showing, which is exactly the unread
    // marker's truth. The side effect is this layer's policy — the client
    // API stays an explicit `mark_conversation_read`. Best-effort: a
    // failed marker never blocks the render.
    let _ = client.mark_conversation_read(conversation);
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
            // De7, positive-only: labelled off the already-loaded contacts
            // and devices rather than `participant_labels`, which re-reads
            // both stores per call — this runs inside a per-message map.
            confirmed: message
                .confirmed
                .iter()
                .map(|key| device_name(key).unwrap_or_else(|| label(&contacts, key)))
                .collect(),
            mine: message.sender == me,
            text: message
                .body
                .ok()
                .map(|body| String::from_utf8_lossy(&body).into_owned()),
            timestamp_ms: message.timestamp_ms,
            pending: message.owed_since_ms.is_some(),
            stuck: message
                .owed_since_ms
                .is_some_and(|since| now_ms.saturating_sub(since) > STUCK_AFTER_MS),
            undelivered: message
                .owed_since_ms
                .is_some_and(|since| now_ms.saturating_sub(since) > zink_client::OUTBOX_GIVE_UP_MS),
            crossed: message.crossed,
            merged: message.merged,
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
    // Stage only: seal, store, ledger — no network. The message is in the
    // store when this returns, so the view can render it immediately (flagged
    // "sending…" by its outbox entries) instead of waiting out delivery.
    // Delivery is honestly slow sometimes — an unreachable relay costs its
    // whole deadline — and none of that needs to be in front of the user.
    let staged = match (conversation, to) {
        (Some(conversation), _) => {
            let conversation = parse_id(&conversation)?;
            let resolved = client.reply_contacts(conversation)?;
            // --add grows the recipient set (groups.md §2): the signed
            // recipients list is the membership announcement.
            let mut contacts = resolved.contacts;
            // A person label adds the whole cluster, one Contact per member
            // device (S2/S3); an entry petname adds that device alone.
            for name in &adding {
                contacts.extend(client.resolve_person(name)?);
            }
            // Unroutable members stay recipients (groups.md §2 — membership
            // is not deliverability); only an all-unroutable set is an error.
            if contacts.iter().all(|contact| contact.relays.is_empty()) {
                return Err("no routable participants — add their contacts first".into());
            }
            client.stage_send_in(conversation, &contacts, text.into_bytes(), blobs)?
        }
        (None, Some(names)) if !names.is_empty() => {
            // Person labels: "message Alice", never "message Alice's phone"
            // — every member device rides its own entry's relays (S2/S3).
            let mut contacts: Vec<zink_client::Contact> = Vec::new();
            for name in &names {
                contacts.extend(client.resolve_person(name)?);
            }
            // The app's "new chat" is always a fresh genesis (project 6 §7):
            // conversations are genesis-identified, several per participant
            // set is a feature — the draft view already offered the existing
            // ones. Replies go through the `conversation` arm above.
            client.stage_send_new(&contacts, text.into_bytes(), blobs)?
        }
        _ => return Err("no conversation or contact given".into()),
    };
    let conversation = hex::encode(&staged.conversation.0);
    // Deliver off the command's path. Losing this task loses nothing: the
    // outbox entry is already written, and every flush trigger (recv,
    // reconnect, the next send) pays it. `new-messages` on completion is what
    // clears the "sending…" flag — or leaves it honestly in place.
    let deliver_client = client.clone();
    let deliver_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let receipt = deliver_client.deliver(&staged).await;
        let _ = deliver_app.emit("new-messages", 1);
        match receipt {
            // A fully delivered send proves the network is up: retry the
            // backlog too.
            Ok(receipt) if receipt.pending_relays == 0 => {
                let _ = deliver_client.flush_outbox().await;
            }
            Ok(_) => {}
            // Not lost — queued, and the message renders as such.
            Err(error) => tracing::warn!(%error, "send delivery queued for retry"),
        }
    });
    Ok(conversation)
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
        let device_evidence = evidence_lines(&client, key)?;
        members.push(UnknownMember {
            key: hex::encode(&key.0),
            candidates,
            dismissed: dismissed.contains(&key),
            device_evidence,
            disavowals: disavowal_lines(&client, key)?,
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
        disavowals: disavowal_lines(&client, key)?,
    })
}

/// The stranger bootstrap's direct rung (project 7): ask the subject about
/// themself — the only query that works when no mutual contact exists.
/// Manual trigger only; the dial is a liveness receipt to the subject, and
/// the button copy owns that. An answer lands in the learned store; the
/// page re-reads and its candidate rows offer the add.
#[tauri::command]
async fn ask_subject(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
    subject: String,
) -> Result<SubjectAsk, String> {
    let client = client(&app, &managed).await?;
    Ok(
        match client
            .ask_subject(PublicKey(hex::parse32(&subject)?))
            .await
        {
            zink_client::SubjectAsk::Answered => SubjectAsk::Answered,
            zink_client::SubjectAsk::Nothing => SubjectAsk::Nothing,
            zink_client::SubjectAsk::Unreachable => SubjectAsk::Unreachable,
        },
    )
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
    let report = client.recv(&relays).await?;
    // A relay that didn't answer keeps its mail; the drain no longer fails
    // over it (De6a). Logged rather than surfaced: only a multi-relay profile
    // can see a *partial* drain at all (with one relay, `recv` still errors),
    // and giving the UI a "your view may be incomplete" line is its own
    // slice. Until then the diag log is where this shows up.
    for failure in &report.failed {
        tracing::warn!(relay = %failure.relay, error = %failure.error, "relay not drained");
    }
    Ok(report.received.len())
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

/// Tracing to stderr (the `cargo tauri dev` terminal) AND a size-capped
/// file in the app data dir (C4c-i): on Android stderr goes nowhere, and
/// the background-delivery diagnosis reads this file the morning after —
/// the subscription loop's lifecycle lines are the whole point.
fn init_diagnostics(app: &AppHandle) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // The reconnect-backoff lines are debug-level in zink-client.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,zink_client=debug"));
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file = app.path().app_data_dir().ok().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("diag.log");
        // Dumb rotation: one predecessor kept, nothing unbounded.
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 5 * 1024 * 1024) {
            let _ = std::fs::rename(&path, dir.join("diag.log.1"));
        }
        let file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file)),
        )
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
        .try_init()
        .ok();
    // A fresh line per process: a "process start" with no heartbeats and
    // no "subscription live" after it is the revived-but-idle signature
    // (START_STICKY restarts the service; nothing restarts the loops).
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "process start");
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // iroh's reqwest is built in "no provider" rustls mode: the process must
    // install a default crypto provider before any client is built or it
    // panics (aborts on mobile). Ring to match what iroh already links.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls crypto provider");

    let builder = tauri::Builder::default()
        .manage(ManagedClient {
            client: tokio::sync::OnceCell::new(),
            subscribed: Mutex::default(),
            notified: Arc::default(),
        })
        .plugin(tauri_plugin_notification::init())
        .setup(|_app| {
            init_diagnostics(_app.handle());
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
            preview_contact,
            update_contact,
            rename_contact,
            set_relay_override,
            set_local_avatar,
            clear_local_avatar,
            person_page,
            key_page,
            page_refresh,
            ask_friend,
            ask_subject,
            rename_person,
            merge_persons,
            split_person,
            conversations,
            conversations_with,
            conversation_members,
            person_conversations,
            name_conversation,
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
            vouch,
            unvouch,
            repudiate_key,
            unrecognize_device,
            dismiss
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application")
}
