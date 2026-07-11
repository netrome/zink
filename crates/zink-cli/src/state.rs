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

use std::collections::BTreeSet;
use std::path::PathBuf;

use zink_protocol::{ConversationDag, MessageEnvelope, MessageId, PublicKey};

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
        std::fs::write(&path, conversation.0).map_err(|e| format!("write {path:?}: {e}"))
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
        std::fs::write(&path, envelope.to_bytes()).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// Rebuild the DAG from the stored envelopes. Order on disk is
    /// irrelevant — the store accepts children before parents.
    pub fn load_dag(&self, conversation: MessageId) -> Result<ConversationDag, String> {
        let mut cores = Vec::new();
        let dir = self.conversation_dir(conversation);
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {dir:?}: {e}"))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("read {dir:?}: {e}"))?.path();
            let bytes = std::fs::read(&path).map_err(|e| format!("read {path:?}: {e}"))?;
            let envelope = MessageEnvelope::try_from_bytes(&bytes)
                .map_err(|e| format!("decode {path:?}: {e}"))?;
            cores.push(envelope.core);
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
            dag.insert(core)
                .map_err(|e| format!("stored message invalid: {e}"))?;
        }
        Ok(dag)
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
