//! zink phone/desktop app (Tauri): thin command layer over `zink-client`.
//! C1 scope: send + receive text with a persistent device key. UI, contacts
//! and live delivery come in C2–C4.

use tauri::Manager;
use zink_client::{Client, Contact, hex};

/// This device's public key (creating it on first run), so it can be shared
/// with a contact. QR exchange replaces this in C2.
#[tauri::command]
async fn my_key(app: tauri::AppHandle) -> Result<String, String> {
    let client = open_client(&app).await?;
    Ok(hex::encode(&client.public_key().0))
}

/// Send a text to one contact (`<pubkey-hex>`) via a relay dial string.
#[tauri::command]
async fn send_text(
    app: tauri::AppHandle,
    relay: String,
    to: String,
    text: String,
) -> Result<String, String> {
    let client = open_client(&app).await?;
    let contact = Contact::parse(&format!("{to}@{relay}"))?;
    let receipt = client.send(&[contact], text.into_bytes(), vec![]).await?;
    Ok(format!(
        "sent (conv {}, seq {})",
        &hex::encode(&receipt.conversation.0)[..8],
        receipt.seq
    ))
}

/// Drain the relay's mailbox; returns displayable lines.
#[tauri::command]
async fn recv_texts(app: tauri::AppHandle, relay: String) -> Result<Vec<String>, String> {
    let client = open_client(&app).await?;
    let received = client.recv(&[relay]).await?;
    Ok(received
        .iter()
        .map(|message| match &message.body {
            Ok(plaintext) => format!(
                "from {}: {}",
                &hex::encode(&message.envelope.core.sender.0)[..8],
                String::from_utf8_lossy(plaintext)
            ),
            Err(e) => format!("undecryptable message ({e})"),
        })
        .collect())
}

/// One client per call for now: binds a fresh endpoint each time. Fine at
/// C1 scope; a managed long-lived client arrives with the UI slice.
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
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![my_key, send_text, recv_texts])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
