//! The outbox: the per-(message, relay) ledger of deliveries still owed.
//! Staging writes entries before any network work, so a crash loses no
//! message; a flush retries them idempotently (deposits dedup by id, blob
//! pushes by hash) with bounded concurrency. Entries past the give-up
//! window stay surfaced as pending, no longer retried — deleting them is
//! not our call.

use zink_protocol::MessageEnvelope;

use crate::error::Error;
use crate::hex;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::transport::Transport;

use super::Client;

/// Outbox entries older than this stop being retried (but stay surfaced):
/// mirrors the relay's default mailbox retention — past it, recipients'
/// cursors have moved on and the message is socially dead.
const OUTBOX_GIVE_UP_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// How many owed deliveries a flush has in flight at once (De6d). Concurrency
/// is what stops n dead relays costing n deadlines; the *bound* is because
/// each in-flight entry holds its message's blob bytes twice (loaded from the
/// cache, then staged), and a long backlog of images fanned out without limit
/// would spike memory where the serial version never did.
const FLUSH_CONCURRENCY: usize = 8;

/// What one outbox flush accomplished.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlushReport {
    pub delivered: usize,
    pub pending: usize,
    /// Entries past the give-up window: left in place, no longer retried.
    pub expired: usize,
}

impl<C: Clock, W: WallClock, N: Transport> Client<C, W, N> {
    /// Retry every outstanding delivery (idempotent: deposits dedup by id,
    /// blob pushes by hash). Entries older than the give-up window are left
    /// in place unretried — the relay's retention has expired, the message
    /// stays surfaced as pending/undelivered (deleting it is not our call).
    pub async fn flush_outbox(&self) -> Result<FlushReport, Error> {
        let mut report = FlushReport::default();
        let now = self.wall_clock.now_ms();
        // Cheap triage first: an aged-out entry never touches the network.
        let owed: Vec<crate::state::OutboxEntry> = self
            .state
            .outbox()
            .into_iter()
            .filter(|entry| {
                let expired = now.saturating_sub(entry.created_ms) > OUTBOX_GIVE_UP_MS;
                report.expired += usize::from(expired);
                !expired
            })
            .collect();
        // Concurrent per entry (De6d) — serially, one unreachable relay made
        // every later entry wait out its deadline first, so a backlog across
        // n dead relays cost n × `connect_timeout`.
        //
        // **Chunked**, though: an entry in flight holds its message's blob
        // bytes twice (loaded from the cache, then staged), so an unbounded
        // fan-out over a long backlog of images would be a memory spike where
        // the serial version had none. This bounds that at a few entries
        // while still turning n deadlines into ceil(n / N).
        for chunk in owed.chunks(FLUSH_CONCURRENCY) {
            let mut ready = Vec::with_capacity(chunk.len());
            for entry in chunk {
                match self.reload_owed(entry)? {
                    Some(loaded) => ready.push((entry, loaded)),
                    // No stored envelope — nothing a retry could ever send.
                    None => continue,
                }
            }
            let outcomes = n0_future::join_all(ready.iter().map(
                |(entry, (envelope, encrypted))| async move {
                    match self
                        .deliver_to_relay(&entry.relay, envelope, encrypted)
                        .await
                    {
                        Ok(()) => {
                            self.state.clear_outbox(entry.message, &entry.relay);
                            true
                        }
                        Err(error) => {
                            tracing::warn!(relay = %entry.relay, %error, "outbox retry failed");
                            false
                        }
                    }
                },
            ))
            .await;
            for delivered in outcomes {
                if delivered {
                    report.delivered += 1;
                } else {
                    report.pending += 1;
                }
            }
        }
        Ok(report)
    }

    /// Reload what one owed delivery needs: the stored envelope and its blob
    /// bytes from the local cache (put there at send). `None` = the envelope
    /// is gone, so no retry could ever fulfil this entry and it is dropped
    /// from the ledger.
    #[allow(clippy::type_complexity)]
    fn reload_owed(
        &self,
        entry: &crate::state::OutboxEntry,
    ) -> Result<Option<(MessageEnvelope, Vec<zink_protocol::EncryptedBlob>)>, Error> {
        let envelope = match self.state.load_envelope(entry.conversation, entry.message) {
            Ok(envelope) => envelope,
            Err(error) => {
                tracing::warn!(%error, "dropping unfulfillable outbox entry");
                self.state.clear_outbox(entry.message, &entry.relay);
                return Ok(None);
            }
        };
        let encrypted: Vec<zink_protocol::EncryptedBlob> = envelope
            .core
            .blob_refs
            .iter()
            .filter_map(|blob_ref| {
                let bytes = self.state.load_blob(&blob_ref.hash);
                if bytes.is_none() {
                    tracing::warn!(blob = %hex::encode(&blob_ref.hash.0), "blob missing from cache; delivering without it");
                }
                Some(zink_protocol::EncryptedBlob {
                    hash: blob_ref.hash,
                    bytes: bytes?,
                })
            })
            .collect();
        Ok(Some((envelope, encrypted)))
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adapters::system_clock::SystemClock;
    use crate::client::ClientConfig;
    use crate::client::test_kit::{deposited_frame, mailbox_spec, temp_key, temp_root};
    use crate::keystore;
    use crate::ports::clock::TestClock;
    use crate::ports::transport::TestTransport;
    use zink_protocol::{ContactRecord, DeviceKey, RelayEntry};

    #[tokio::test]
    async fn delivery__should_recover_when_a_dead_relay_returns() {
        // Given: one relay, held silent — the send falls back to the outbox
        // on a deadline only the TestClock moves. The §4 archetype: silence,
        // deterministic timeout, fallback, then recovery — a scenario a real
        // network won't produce on command.
        const DEADLINE: Duration = Duration::from_secs(10);
        let relay = DeviceKey::from_seed([81; 32]).public();
        let key_path = temp_key("recover", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            SystemClock,
            net.clone(),
        );
        a.add_contact(
            &ContactRecord::new(
                vec![DeviceKey::from_seed([80; 32]).public()],
                vec![],
                vec![RelayEntry {
                    mailbox: mailbox_spec(&relay),
                    relay_url: None,
                }],
            ),
            Some("ghost".to_string()),
        )
        .expect("add the contact");
        let recipients = [a.resolve_contact("ghost").expect("resolve")];

        // When: the send times out and is queued…
        let (result, ()) = tokio::join!(
            a.send(&recipients, b"catch up later".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(DEADLINE);
            },
        );
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        assert_eq!(a.state.outbox().len(), 1, "owed to the silent relay");

        // …and the relay comes back: the next dial connects, the deposit
        // is taken
        net.dial.connect(&relay).reply(deposited_frame());
        let report = a.flush_outbox().await.expect("flush");

        // Then: recovered, nothing owed — and no real time passed anywhere
        assert_eq!(report.delivered, 1);
        assert!(a.state.outbox().is_empty(), "the debt is settled");

        let _ = std::fs::remove_dir_all(temp_root("recover"));
    }
}
