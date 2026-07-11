//! Client-side persistence (slice B5): conversations on disk, so the CLI
//! can thread messages instead of sending standalone geneses.
//!
//! Layout under `<key-file>.state/`:
//! - `conversations/<conv-id-hex>/<message-id-hex>.env` — one file per
//!   envelope, content = canonical wire bytes. The DAG is rebuilt from
//!   these on demand (out-of-order insert is the store's normal mode).
//! - `participants/<fingerprint-hex>` — maps a participant set to its
//!   conversation id. "One conversation per participant set" is *client
//!   policy* (SPEC tenet 4), not protocol: sender and recipients land in
//!   the same conversation because both sides fingerprint the same set.
//! - `profile.name` / `profile.relays` — this device's display name and
//!   home relays (what goes into its ContactRecord).
//! - `contacts/<key-hex>.record` (wire bytes) + `.name` (the local petname
//!   — client policy, defaulting to the contact's self-claimed name).

use std::collections::BTreeSet;
use std::path::PathBuf;

use zink_protocol::{ContactRecord, ConversationDag, MessageEnvelope, MessageId, PublicKey};

pub struct ClientState {
    root: PathBuf,
}

impl ClientState {
    /// State lives next to the key file: `<key-file>.state/`.
    pub fn open(key_path: &str) -> Self {
        Self {
            root: PathBuf::from(format!("{key_path}.state")),
        }
    }

    /// The conversation this participant set maps to, if any.
    pub fn conversation_for(&self, participants: &BTreeSet<PublicKey>) -> Option<MessageId> {
        let bytes = std::fs::read(self.participants_file(participants)).ok()?;
        Some(MessageId(bytes.try_into().ok()?))
    }

    pub fn record_conversation(
        &self,
        participants: &BTreeSet<PublicKey>,
        conversation: MessageId,
    ) -> Result<(), String> {
        let path = self.participants_file(participants);
        create_parent(&path)?;
        write_atomic(&path, &conversation.0).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// Persist an envelope under its conversation. Idempotent (the file
    /// name is the message id).
    pub fn store_envelope(
        &self,
        conversation: MessageId,
        envelope: &MessageEnvelope,
    ) -> Result<(), String> {
        let path = self
            .conversation_dir(conversation)
            .join(format!("{}.env", hex(&envelope.id().0)));
        create_parent(&path)?;
        write_atomic(&path, &envelope.to_bytes()).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// Rebuild the DAG from the stored envelopes. Order on disk is
    /// irrelevant — the store accepts children before parents. A damaged
    /// file is skipped with a warning, never fatal: the DAG then honestly
    /// reports the hole as a missing parent / seq gap. Only a missing
    /// genesis is unrecoverable (there is no root to build on).
    pub fn load_dag(&self, conversation: MessageId) -> Result<ConversationDag, String> {
        let mut cores = Vec::new();
        let dir = self.conversation_dir(conversation);
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {dir:?}: {e}"))?;
        for entry in entries.flatten() {
            let path = entry.path();
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    MessageEnvelope::try_from_bytes(&bytes).map_err(|e| e.to_string())
                }) {
                Ok(envelope) => cores.push(envelope.core),
                Err(e) => eprintln!("warning: skipping damaged {path:?}: {e}"),
            }
        }
        let genesis_at = cores
            .iter()
            .position(|core| core.conversation.is_none())
            .ok_or_else(|| {
                format!(
                    "conversation {} has no genesis on disk",
                    hex(&conversation.0)
                )
            })?;
        let mut dag = ConversationDag::new(cores.swap_remove(genesis_at))
            .map_err(|e| format!("stored genesis invalid: {e}"))?;
        for core in cores {
            if let Err(e) = dag.insert(core) {
                eprintln!("warning: skipping invalid stored message: {e}");
            }
        }
        Ok(dag)
    }

    pub fn save_profile(&self, name: &str, relays: &[String]) -> Result<(), String> {
        let name_path = self.root.join("profile.name");
        create_parent(&name_path)?;
        write_atomic(&name_path, name.as_bytes()).map_err(|e| format!("write profile: {e}"))?;
        write_atomic(
            &self.root.join("profile.relays"),
            relays.join("\n").as_bytes(),
        )
        .map_err(|e| format!("write relays: {e}"))
    }

    pub fn profile_name(&self) -> Option<String> {
        let name = std::fs::read_to_string(self.root.join("profile.name")).ok()?;
        (!name.trim().is_empty()).then(|| name.trim().to_string())
    }

    pub fn home_relays(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("profile.relays"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .collect()
    }

    /// Store a contact under a petname. The record is kept in wire form;
    /// the petname is a sibling file (local convention, never protocol).
    pub fn save_contact(&self, petname: &str, record: &ContactRecord) -> Result<(), String> {
        let stem = self.contact_stem(record.keys.first().ok_or("record has no keys")?);
        create_parent(&stem.with_extension("record"))?;
        write_atomic(&stem.with_extension("record"), &record.to_bytes())
            .map_err(|e| format!("write contact: {e}"))?;
        write_atomic(&stem.with_extension("name"), petname.as_bytes())
            .map_err(|e| format!("write petname: {e}"))
    }

    /// All stored contacts as `(petname, record)`, petname-sorted.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, String> {
        let dir = self.root.join("contacts");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut contacts = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "record") {
                continue;
            }
            let record = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).map_err(|e| e.to_string()));
            let petname = std::fs::read_to_string(path.with_extension("name"));
            match (record, petname) {
                (Ok(record), Ok(petname)) => contacts.push((petname.trim().to_string(), record)),
                (record, petname) => eprintln!(
                    "warning: skipping damaged contact {path:?}: {:?} {:?}",
                    record.err(),
                    petname.err()
                ),
            }
        }
        contacts.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(contacts)
    }

    fn contact_stem(&self, key: &PublicKey) -> PathBuf {
        self.root.join("contacts").join(hex(&key.0))
    }

    fn conversation_dir(&self, conversation: MessageId) -> PathBuf {
        self.root.join("conversations").join(hex(&conversation.0))
    }

    fn participants_file(&self, participants: &BTreeSet<PublicKey>) -> PathBuf {
        // Fingerprint = BLAKE3 over the sorted keys (BTreeSet iterates
        // sorted), so any member computes the same name.
        let mut hasher = blake3::Hasher::new();
        for key in participants {
            hasher.update(&key.0);
        }
        self.root
            .join("participants")
            .join(hasher.finalize().to_hex().as_str())
    }
}

fn create_parent(path: &std::path::Path) -> Result<(), String> {
    let parent = path.parent().expect("state paths always have a parent");
    std::fs::create_dir_all(parent).map_err(|e| format!("create {parent:?}: {e}"))
}

/// Temp file + rename: a crash mid-write never leaves a truncated file.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.to_path_buf();
    tmp.set_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
