//! zink phone/desktop app (Tauri): thin command layer over `zink-client`.
//! C3b scope: one managed long-lived client; structured DTO commands
//! (`zink-app-dto`) rendered from the *stored DAG*; the webview owns only
//! presentation. Images render in C3c; live delivery is C4.

use tauri::{AppHandle, Manager, State};
use zink_app_dto::{AppState, Conversation, Message, QrPayload};
use zink_client::{Client, hex};
use zink_protocol::{ContactRecord, MessageId, PublicKey};

/// The one `Client` for the app's lifetime, created on first use. A single
/// instance means a single endpoint and no two commands racing first-run
/// key creation or the state dir.
struct ManagedClient(tokio::sync::OnceCell<Client>);

async fn client<'a>(
    app: &AppHandle,
    managed: &'a State<'_, ManagedClient>,
) -> Result<&'a Client, String> {
    managed
        .0
        .get_or_try_init(|| async {
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("app data dir: {e}"))?;
            std::fs::create_dir_all(&data_dir).map_err(|e| format!("create data dir: {e}"))?;
            let key_path = data_dir.join("device.key");
            Client::open_or_create(&key_path.to_string_lossy()).await
        })
        .await
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
        relay: client.home_relays().into_iter().next(),
        contacts: client
            .contacts()?
            .into_iter()
            .map(|(petname, _)| petname)
            .collect(),
        record,
    })
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
    client.set_profile(&name, &[relay])?;
    client.register_at_home_relays().await?;
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
    client.add_contact(&record, petname.filter(|name| !name.trim().is_empty()))
}

/// The conversation list, rendered from the stored DAG (not from a recv).
#[tauri::command]
async fn conversations(
    app: AppHandle,
    managed: State<'_, ManagedClient>,
) -> Result<Vec<Conversation>, String> {
    let client = client(&app, &managed).await?;
    let contacts = client.contacts()?;
    let me = client.public_key();
    Ok(client
        .conversations()?
        .into_iter()
        .map(|summary| {
            let others: Vec<String> = summary
                .participants
                .iter()
                .filter(|key| **key != me)
                .map(|key| label(&contacts, key))
                .collect();
            Conversation {
                id: hex::encode(&summary.id.0),
                label: if others.is_empty() {
                    "only me".to_string()
                } else {
                    others.join(", ")
                },
                message_count: summary.message_count,
                last_timestamp_ms: summary.last_timestamp_ms,
            }
        })
        .collect())
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
    Ok(client
        .history(conversation)?
        .into_iter()
        .map(|message| Message {
            id: hex::encode(&message.id.0),
            sender: if message.sender == me {
                "me".to_string()
            } else {
                label(&contacts, &message.sender)
            },
            mine: message.sender == me,
            text: message
                .body
                .ok()
                .map(|body| String::from_utf8_lossy(&body).into_owned()),
            timestamp_ms: message.timestamp_ms,
            blob_count: message.blob_refs.len(),
        })
        .collect())
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
    to: Option<String>,
    text: String,
) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("nothing to send".into());
    }
    let client = client(&app, &managed).await?;
    let receipt = match (conversation, to) {
        (Some(conversation), _) => {
            let conversation = parse_id(&conversation)?;
            let resolved = client.reply_contacts(conversation)?;
            if resolved.contacts.is_empty() {
                return Err("no reachable participants — add their contacts first".into());
            }
            client
                .send_in(conversation, &resolved.contacts, text.into_bytes(), vec![])
                .await?
        }
        (None, Some(petname)) => {
            let contact = client.resolve_contact(&petname)?;
            client.send(&[contact], text.into_bytes(), vec![]).await?
        }
        (None, None) => return Err("no conversation or contact given".into()),
    };
    Ok(hex::encode(&receipt.conversation.0))
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
    let builder = tauri::Builder::default().manage(ManagedClient(tokio::sync::OnceCell::new()));
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
            refresh
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application")
}
