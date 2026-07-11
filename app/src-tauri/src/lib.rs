//! zink phone/desktop app (Tauri): thin command layer over `zink-client`.
//! C2 scope: profile + QR contact exchange + messaging by name. Real UI
//! (conversation views) is C3; live delivery is C4.

use serde::Serialize;
use tauri::Manager;
use zink_client::{Client, hex};
use zink_protocol::ContactRecord;

/// Everything the UI needs on load, in one call.
#[derive(Serialize)]
struct AppState {
    my_key: String,
    name: Option<String>,
    relay: Option<String>,
    contacts: Vec<String>,
    record: Option<QrPayload>,
}

#[derive(Serialize)]
struct QrPayload {
    svg: String,
    text: String,
}

#[tauri::command]
async fn app_state(app: tauri::AppHandle) -> Result<AppState, String> {
    let client = open_client(&app).await?;
    let record = match client.my_record() {
        Ok(record) => Some(qr_payload(&record)?),
        Err(_) => None, // no profile yet — the UI shows the setup form
    };
    Ok(AppState {
        my_key: hex::encode(&client.public_key().0),
        name: client.profile_name(),
        relay: client.home_relays().into_iter().next(),
        contacts: contact_names(&client)?,
        record,
    })
}

/// Save name + home relay, register the mailbox there, return the QR.
#[tauri::command]
async fn set_profile(
    app: tauri::AppHandle,
    name: String,
    relay: String,
) -> Result<QrPayload, String> {
    let client = open_client(&app).await?;
    client.set_profile(&name, &[relay])?;
    client.register_at_home_relays().await?;
    qr_payload(&client.my_record()?)
}

/// Add a contact from a scanned or pasted `ZINK:` payload.
#[tauri::command]
async fn add_contact(
    app: tauri::AppHandle,
    payload: String,
    petname: Option<String>,
) -> Result<String, String> {
    let record = ContactRecord::from_qr_string(&payload).map_err(|e| format!("record: {e}"))?;
    let client = open_client(&app).await?;
    client.add_contact(&record, petname.filter(|name| !name.trim().is_empty()))
}

/// Send a text to a contact by petname.
#[tauri::command]
async fn send_text(app: tauri::AppHandle, to: String, text: String) -> Result<String, String> {
    let client = open_client(&app).await?;
    let contact = client.resolve_contact(&to)?;
    let receipt = client.send(&[contact], text.into_bytes(), vec![]).await?;
    Ok(format!(
        "sent to {to} (conv {}, seq {})",
        &hex::encode(&receipt.conversation.0)[..8],
        receipt.seq
    ))
}

/// Drain the home relays; returns displayable lines (petname-resolved).
#[tauri::command]
async fn recv_texts(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let client = open_client(&app).await?;
    let relays = client.home_relays();
    if relays.is_empty() {
        return Err("set up your profile first".into());
    }
    let contacts = client.contacts()?;
    let received = client.recv(&relays).await?;
    Ok(received
        .iter()
        .map(|message| {
            let sender = message.envelope.core.sender;
            let from = contacts
                .iter()
                .find(|(_, record)| record.keys.contains(&sender))
                .map(|(petname, _)| petname.clone())
                .unwrap_or_else(|| hex::encode(&sender.0)[..8].to_string());
            match &message.body {
                Ok(plaintext) => {
                    format!("{from}: {}", String::from_utf8_lossy(plaintext))
                }
                Err(e) => format!("{from}: <undecryptable: {e}>"),
            }
        })
        .collect())
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

fn contact_names(client: &Client) -> Result<Vec<String>, String> {
    Ok(client
        .contacts()?
        .into_iter()
        .map(|(petname, _)| petname)
        .collect())
}

/// One client per call for now: binds a fresh endpoint each time. Fine at
/// this scope; a managed long-lived client arrives with the UI slice.
async fn open_client(app: &tauri::AppHandle) -> Result<Client, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app data dir: {e}"))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create data dir: {e}"))?;
    let key_path = data_dir.join("device.key");
    Client::open_or_create(&key_path.to_string_lossy()).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    builder
        .invoke_handler(tauri::generate_handler![
            app_state,
            set_profile,
            add_contact,
            send_text,
            recv_texts
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
