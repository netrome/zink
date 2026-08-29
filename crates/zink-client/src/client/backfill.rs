//! Peer sync, the fetching side (D0d, D3d): backfill a partially-known
//! conversation from a peer — backward to the genesis, then forward via
//! `get-successors` — the auto-sync that heals orphans after every drain,
//! and the re-wrap flows that make pre-pairing history readable on a
//! paired device. The serving side lives in `crate::sync`.

use std::collections::{BTreeMap, BTreeSet};

use zink_protocol::{
    MAX_GET_KEYS_IDS, MessageEnvelope, MessageId, PublicKey, SYNC_ALPN, SyncOp, SyncResult,
};

use crate::error::Error;
use crate::net;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::{Draw, Mint};
use crate::ports::transport::{Peer, Request, Transport};

use super::{Client, Received};

impl<C: Clock, W: WallClock, N: Transport, R: Draw + Mint> Client<C, W, N, R> {
    /// Sync a partially-known conversation with a peer (SPEC §5.2): walk
    /// `parents` **backward** to the genesis (what lets a device added
    /// mid-conversation build the DAG and reply — without the genesis,
    /// `load_dag` can't even start), then pull **forward** via
    /// `get-successors` (D0d — catches messages that expired from the
    /// mailbox or live on concurrent branches). `from` is the peer's
    /// `<endpoint-id>@<ip:port>`.
    ///
    /// Best-effort (tenet 6): an unreachable peer, or one that declines to
    /// serve, just stops the walk — we never fabricate a root. A served peer
    /// is trusted no more than a relay: every envelope is verified, checked
    /// to be the id we asked for, and checked to belong to this conversation
    /// before it's stored. Returns the number of newly-stored messages.
    pub async fn backfill(&self, conversation: MessageId, from: &str) -> Result<usize, Error> {
        self.backfill_addr(conversation, crate::adapters::iroh::parse_dial(from)?)
            .await
    }

    /// `backfill` reaching the peer **by key alone** (D0b): the peer's relay
    /// URLs come from their stored ContactRecord, iroh routes the initial
    /// signaling via their relay and holepunches to a direct path (relaying
    /// the encrypted QUIC as fallback). The device key IS the endpoint key —
    /// no lookup service involved. Fails without a stored record carrying a
    /// relay URL for `peer` (a mailbox-only record can't rendezvous).
    pub async fn backfill_by_key(
        &self,
        conversation: MessageId,
        peer: PublicKey,
    ) -> Result<usize, Error> {
        // A contact's record, or a recognized own device's (D3c/D3d) — a
        // fresh paired device backfills its sibling by key.
        let record = self
            .trusted_record_for(&peer)
            .ok_or_else(|| Error::NotAContact("no stored record for that key".into()))?;
        self.backfill_addr(conversation, self.peer_addr_for(peer, Some(&record))?)
            .await
    }

    /// `backfill` with the peer address already resolved — the seam the string
    /// API parses into, and the one tests use to dial a locally-bound peer's
    /// full multi-address `EndpointAddr` (a bare public socket isn't reliably
    /// self-reachable on one host).
    pub(super) async fn backfill_addr(
        &self,
        conversation: MessageId,
        from: Peer,
    ) -> Result<usize, Error> {
        // A hostile peer could feed an unbounded fake chain; one budget
        // bounds the whole walk — the forward pass gets what the backward
        // pass didn't spend.
        const MAX_SYNC_FETCH: usize = 10_000;
        let connection = net::connect_peer(
            &self.transport,
            &from,
            SYNC_ALPN,
            self.config.connect_timeout,
            &self.clock,
        )
        .await?;
        let backward = self
            .fill_backward(conversation, &connection, MAX_SYNC_FETCH)
            .await?;
        let forward = self
            .fill_forward(conversation, &connection, MAX_SYNC_FETCH - backward)
            .await?;
        Ok(backward + forward)
    }

    /// The backward pass: fetch referenced-but-missing parents until the
    /// stored slice is ancestor-closed (genesis reached), the peer stops
    /// yielding, or `budget` is spent. Returns the number fetched.
    async fn fill_backward(
        &self,
        conversation: MessageId,
        connection: &impl Request,
        budget: usize,
    ) -> Result<usize, Error> {
        let mut fetched = 0usize;
        loop {
            let frontier = self.state.missing_ancestors(conversation);
            if frontier.is_empty() {
                break; // ancestor-closed: the genesis (parents=[]) is reachable
            }
            let mut progressed = false;
            for id in frontier {
                if fetched >= budget {
                    tracing::warn!("sync hit the fetch budget; stopping");
                    return Ok(fetched);
                }
                if self.fetch_one(connection, id, conversation).await? {
                    fetched += 1;
                    progressed = true;
                }
            }
            if !progressed {
                break; // this peer can't take us any closer to the genesis
            }
        }
        Ok(fetched)
    }

    /// The forward pass (D0d): `get-successors` to learn children we lack —
    /// messages the mailbox never delivered (expired, or sent while we were
    /// unreachable) and concurrent branches. The first round queries every
    /// stored id (a fork can hang off any interior message); later rounds
    /// query only what the previous round fetched, so the walk converges.
    /// Chatty at one round-trip per id — fine at friend/family scale.
    /// Returns the number fetched, at most `budget`.
    async fn fill_forward(
        &self,
        conversation: MessageId,
        connection: &impl Request,
        budget: usize,
    ) -> Result<usize, Error> {
        let mut fetched = 0usize;
        let mut stored: BTreeSet<MessageId> = self
            .state
            .load_envelopes(conversation)
            .unwrap_or_default()
            .iter()
            .map(|envelope| envelope.id())
            .collect();
        let mut query: Vec<MessageId> = stored.iter().copied().collect();
        while !query.is_empty() {
            let mut learned: Vec<MessageId> = Vec::new();
            for id in query {
                let ids = match net::sync_request(connection, SyncOp::GetSuccessors { id }).await? {
                    SyncResult::Successors { ids } => ids,
                    other => return Err(Error::UnexpectedResponse(format!("sync: {other:?}"))),
                };
                for child in ids {
                    if fetched >= budget {
                        tracing::warn!("sync hit the fetch budget; stopping");
                        return Ok(fetched);
                    }
                    if stored.contains(&child) {
                        continue;
                    }
                    if self.fetch_one(connection, child, conversation).await? {
                        stored.insert(child);
                        learned.push(child);
                        fetched += 1;
                    }
                }
            }
            query = learned;
        }
        Ok(fetched)
    }

    /// One `get` round-trip: fetch `id`, validate, store. `Ok(true)` iff a
    /// new envelope was stored. A served peer is trusted no more than a
    /// relay: the envelope must hash to the id we asked for, carry a valid
    /// sender signature, and belong to the conversation being synced — the
    /// last check matters for the forward pass, where ids are the *peer's
    /// claim* rather than parents read from envelopes we already verified.
    async fn fetch_one(
        &self,
        connection: &impl Request,
        id: MessageId,
        conversation: MessageId,
    ) -> Result<bool, Error> {
        match net::sync_request(connection, SyncOp::Get { id }).await? {
            SyncResult::Envelope { envelope } => {
                if envelope.id() != id {
                    tracing::warn!("peer returned a mismatched id; skipping");
                    return Ok(false);
                }
                if !envelope.version_supported() {
                    tracing::warn!("skipping synced message with unsupported version");
                    return Ok(false);
                }
                if envelope.verify().is_err() {
                    tracing::warn!("peer returned an unverifiable envelope; skipping");
                    return Ok(false);
                }
                if envelope.core.conversation.unwrap_or_else(|| envelope.id()) != conversation {
                    tracing::warn!("peer served a message from another conversation; skipping");
                    return Ok(false);
                }
                self.remember(&envelope)?;
                Ok(true)
            }
            SyncResult::NotHeld => Ok(false), // peer doesn't have it / declined
            other => Err(Error::UnexpectedResponse(format!("sync: {other:?}"))),
        }
    }

    /// Auto-sync (D0d): after a drain stores new messages, heal every
    /// conversation left with missing ancestors by syncing from the received
    /// message's `sender` — the peer most likely to hold the history
    /// (sync-primitives.md §5). Runs *before* the edge renders, so a healed
    /// conversation appears whole. Best-effort and non-fatal: an unreachable
    /// sender or a missing/mailbox-only record just logs — a drain must
    /// never fail because a peer can't be dialed. Returns messages fetched.
    pub(super) async fn auto_sync(&self, received: &[Received]) -> usize {
        let me = self.device.public();
        let mut targets: BTreeMap<MessageId, PublicKey> = BTreeMap::new();
        for message in received {
            let sender = message.envelope.core.sender;
            if sender == me {
                continue;
            }
            let conversation = message
                .envelope
                .core
                .conversation
                .unwrap_or_else(|| message.envelope.id());
            targets.entry(conversation).or_insert(sender);
        }
        let mut healed = 0usize;
        for (conversation, sender) in targets {
            if self.state.missing_ancestors(conversation).is_empty() {
                continue; // ancestor-closed — nothing to heal
            }
            match self.backfill_by_key(conversation, sender).await {
                Ok(fetched) => {
                    healed += fetched;
                    tracing::info!(fetched, "auto-sync healed a conversation");
                }
                Err(error) => tracing::debug!(%error, "auto-sync could not reach the sender"),
            }
        }
        healed
    }

    /// Heal unopenable history from paired devices (D3d, multi-device.md
    /// §6): after a skeleton sync leaves envelopes stored but unopenable,
    /// batch their ids to each recognized device and append the verified
    /// wraps to the stored envelopes. Ids never move — wraps live outside
    /// the hashed core; storage just rewrites the file. Best-effort like
    /// every peer op; returns how many messages became readable.
    pub async fn rewrap_backlog(&self) -> usize {
        let mut healed = 0;
        for conversation in self.state.conversations() {
            healed += self.rewrap_conversation(conversation).await;
        }
        if healed > 0 {
            // Bodies just became openable — the new-device bootstrap's
            // lens history adopts now (lens-sync.md §5).
            self.adopt_lens_ops();
        }
        healed
    }

    async fn rewrap_conversation(&self, conversation: MessageId) -> usize {
        let me = self.device.public();
        let Ok(envelopes) = self.state.load_envelopes(conversation) else {
            return 0;
        };
        let mut unopenable: BTreeMap<MessageId, MessageEnvelope> = envelopes
            .into_iter()
            .filter(|envelope| !envelope.key_wraps.iter().any(|wrap| wrap.recipient == me))
            .map(|envelope| (envelope.id(), envelope))
            .collect();
        if unopenable.is_empty() {
            return 0;
        }
        let mut healed = 0;
        for (device_key, record) in self.state.recognized_devices() {
            if unopenable.is_empty() {
                break;
            }
            let Ok(addr) = self.peer_addr_for(device_key, Some(&record)) else {
                continue;
            };
            let Ok(connection) = net::connect_peer(
                &self.transport,
                &addr,
                SYNC_ALPN,
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            else {
                continue;
            };
            let missing: Vec<MessageId> = unopenable.keys().copied().collect();
            for chunk in missing.chunks(MAX_GET_KEYS_IDS) {
                let Ok(SyncResult::Wraps { wraps }) = net::sync_request(
                    &connection,
                    SyncOp::GetKeys {
                        ids: chunk.to_vec(),
                    },
                )
                .await
                else {
                    break; // declined or failed — try the next device
                };
                for (id, wrap) in wraps {
                    let Some(envelope) = unopenable.get(&id) else {
                        continue;
                    };
                    // Verify before trusting: the wrap is ours and the
                    // body opens under it (the commitment check inside
                    // `open` rejects a wrong key). A bad wrap is dropped
                    // with a warning, never stored.
                    if wrap.recipient != me {
                        tracing::warn!("re-wrap for a different key; dropped");
                        continue;
                    }
                    let mut updated = envelope.clone();
                    updated.key_wraps.push(wrap);
                    if updated.open(&self.device).is_err() {
                        tracing::warn!("re-wrap does not open the body; dropped");
                        continue;
                    }
                    if self.state.store_envelope(conversation, &updated).is_err() {
                        continue;
                    }
                    unopenable.remove(&id);
                    healed += 1;
                }
            }
        }
        healed
    }

    /// The opportunistic re-wrap (D3d): after a drain — whose auto-sync
    /// may just have pulled pre-pairing history — heal the touched
    /// conversations. Free for single-device clients: no recognized
    /// devices, no scan.
    pub(super) async fn auto_rewrap(&self, received: &[Received]) {
        if self.state.recognized_devices().is_empty() {
            return;
        }
        let conversations: BTreeSet<MessageId> = received
            .iter()
            .map(|message| {
                message
                    .envelope
                    .core
                    .conversation
                    .unwrap_or_else(|| message.envelope.id())
            })
            .collect();
        for conversation in conversations {
            let healed = self.rewrap_conversation(conversation).await;
            if healed > 0 {
                tracing::info!(healed, "re-wrapped history from a paired device");
                // Freshly openable bodies may be lens ops (lens-sync.md §5).
                self.adopt_lens_ops();
            }
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::adapters::system_clock::SystemClock;
    use crate::client::test_kit::{
        befriend, chain, loop_client, mailbox_only, open_homed, sealed_chain, spawn_test_relay,
        temp_key, temp_root,
    };
    use crate::ports::transport::{Home, Loopback};
    use zink_protocol::{ContactRecord, DeviceKey, RelayEntry};

    #[tokio::test]
    async fn backfill__should_walk_a_conversation_back_to_its_genesis() {
        // Given: a 3-message conversation A holds in full, while B — added
        // mid-conversation — holds only the latest message, so it can't even
        // build the DAG (no genesis on disk) and thus can't reply.
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("walk", "server", &wire);
        let (b, _b_net, _b_clock) = loop_client("walk", "client", &wire);
        befriend(&a.state, b.public_key()); // the D0c gate serves contacts only

        let author = DeviceKey::from_seed([9; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        let latest = msgs.last().unwrap();
        b.state.store_envelope(conversation, latest).unwrap();
        assert!(
            b.state.load_dag(conversation).is_err(),
            "B lacks the genesis before backfill"
        );

        // When: B backfills the missing ancestors from A (dialing A's full
        // address — a locally-bound peer's bare public socket isn't reliably
        // self-reachable on one host; the string API is exercised separately)
        let fetched = b
            .backfill_addr(
                conversation,
                Peer {
                    key: a.public_key(),
                    relays: vec![],
                    sockets: vec![],
                },
            )
            .await
            .expect("backfill");

        // Then: B pulled the two missing ancestors, can now build the DAG, and
        // would thread a reply onto the true head at the next logical clock
        assert_eq!(fetched, 2, "genesis + the middle message");
        let dag = b
            .state
            .load_dag(conversation)
            .expect("DAG builds after backfill");
        assert_eq!(dag.heads(), vec![latest.id()]);
        assert_eq!(dag.next_logical(), 3);

        let _ = std::fs::remove_dir_all(temp_root("walk"));
    }

    #[tokio::test]
    async fn backfill_by_key__should_reach_a_peer_via_its_relay_across_two_relays() {
        // REAL-NETWORK SMOKE (P7, transport.md §8): cross-relay rendezvous, by key alone.
        // Given: two relay services; A homes to one, B to the other — the
        // D0b acceptance shape (never a single shared relay). B knows only
        // A's *key* plus A's stored ContactRecord naming A's relay URL.
        let (_relay_a, url_a) = spawn_test_relay().await;
        let (_relay_b, url_b) = spawn_test_relay().await;
        let a = open_homed("bykey", "server", &url_a).await;
        let b = open_homed("bykey", "client", &url_b).await;
        befriend(&a.state, b.public_key()); // the D0c gate serves contacts only
        a.transport.online().await; // A must be homed before B rendezvouses via its relay

        let author = DeviceKey::from_seed([5; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();
        let record = ContactRecord::new(
            vec![a.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: Some(url_a.clone()),
            }],
        );
        b.add_contact(&record, Some("a".to_string()))
            .expect("add contact");

        // When: B backfills by key alone — no ip:port anywhere; iroh
        // rendezvouses via A's relay and holepunches (or relays) from there.
        let fetched = b
            .backfill_by_key(conversation, a.public_key())
            .await
            .expect("backfill by key");

        // Then: the missing ancestors arrived and the DAG is reply-ready.
        assert_eq!(fetched, 2, "genesis + the middle message");
        let dag = b.state.load_dag(conversation).expect("DAG builds");
        assert_eq!(dag.next_logical(), 3);

        let _ = std::fs::remove_dir_all(temp_root("bykey"));
    }

    #[tokio::test]
    async fn backfill__should_be_refused_until_the_requester_is_a_contact() {
        // Given: A holds a full conversation; B — NOT in A's contact store —
        // holds only the latest message. D0b made peers dialable by anyone
        // holding key + relay URL; the D0c gate is what keeps "dialable"
        // from meaning "served".
        let a = Client::open_or_create(&temp_key("gate", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("gate", "client"))
            .await
            .expect("open B");
        let author = DeviceKey::from_seed([8; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();

        // When: the stranger backfills — the answers must be
        // indistinguishable from a peer that holds nothing
        let fetched = b
            .backfill_addr(conversation, a.transport.peer())
            .await
            .expect("gate declines, not errors");

        // Then: nothing served, and the successor view is empty too
        assert_eq!(fetched, 0, "a non-contact is served nothing");
        assert!(b.state.load_dag(conversation).is_err());
        let connection = net::connect_peer(
            &b.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            b.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let successors = net::sync_request(
            &connection,
            SyncOp::GetSuccessors {
                id: conversation, // the genesis id — A holds its children
            },
        )
        .await
        .expect("round-trip");
        assert_eq!(
            successors,
            SyncResult::Successors { ids: vec![] },
            "successors of a held message hide behind the gate too"
        );

        // When: A stores B's record — B is now a contact and gets served
        befriend(&a.state, b.public_key());
        let fetched = b
            .backfill_addr(conversation, a.transport.peer())
            .await
            .expect("backfill as a contact");

        // Then: the walk reaches the genesis
        assert_eq!(fetched, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conversation).is_ok());

        let _ = std::fs::remove_dir_all(temp_root("gate"));
    }

    #[tokio::test]
    async fn backfill__should_pull_forward_successors_after_the_backward_walk() {
        // Given: A holds a 5-message chain; B holds only the MIDDLE message —
        // missing both its ancestors (backward) and everything sent after it
        // (forward — e.g. expired from B's mailbox before B fetched).
        let a = Client::open_or_create(&temp_key("forward", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("forward", "client"))
            .await
            .expect("open B");
        befriend(&a.state, b.public_key());
        let author = DeviceKey::from_seed([4; 32]);
        let msgs = chain(&author, b.public_key(), 5);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state.store_envelope(conversation, &msgs[2]).unwrap();

        // When
        let fetched = b
            .backfill_addr(conversation, a.transport.peer())
            .await
            .expect("sync");

        // Then: 2 ancestors + 2 successors, and the DAG ends on the true head
        assert_eq!(fetched, 4);
        let dag = b.state.load_dag(conversation).expect("DAG builds");
        assert_eq!(dag.heads(), vec![msgs[4].id()]);
        assert_eq!(dag.next_logical(), 5);

        let _ = std::fs::remove_dir_all(temp_root("forward"));
    }

    #[tokio::test]
    async fn auto_sync__should_heal_an_orphaned_conversation_from_its_sender() {
        // Given: A authored a 3-message conversation to B and serves it
        // (homed to its own relay); B — on a different relay — receives only
        // the latest message, as a mid-conversation joiner would. B holds
        // A's record (key + relay URL), as any messageable contact does.
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("autosync", "server", &wire);
        let (b, _b_net, _b_clock) = loop_client("autosync", "client", &wire);
        befriend(&a.state, b.public_key());

        let msgs = chain(&a.device, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        let latest = msgs.last().unwrap();
        b.state.store_envelope(conversation, latest).unwrap();
        let record = ContactRecord::new(
            vec![a.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: Some("http://203.0.113.1:1".to_string()),
            }],
        );
        b.add_contact(&record, Some("a".to_string()))
            .expect("add contact");
        assert!(b.state.load_dag(conversation).is_err(), "orphaned before");

        // When: the drain hands the orphan to auto-sync (what recv and the
        // subscription loops now do) — the sender is dialed by key
        let healed = b
            .auto_sync(&[Received {
                envelope: latest.clone(),
                relay: None,
                body: Ok(vec![]),
            }])
            .await;

        // Then: the conversation is whole with zero explicit action
        assert_eq!(healed, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conversation).is_ok());

        let _ = std::fs::remove_dir_all(temp_root("autosync"));
    }

    #[tokio::test]
    async fn rewrap__should_make_pre_pairing_history_readable_on_the_paired_device() {
        // Given: the phone holds a fully-sealed conversation from before
        // the laptop's key existed; both home to a relay for dial-by-key
        let wire = Loopback::new();
        let (phone, _p_net, _p_clock) = loop_client("rewrap", "phone", &wire);
        let (laptop, _l_net, _l_clock) = loop_client("rewrap", "laptop", &wire);
        let author = DeviceKey::from_seed([60; 32]);
        let msgs = sealed_chain(&author, phone.public_key(), &[b"one", b"two", b"three"]);
        let conversation = msgs[0].id();
        let ids: BTreeSet<MessageId> = msgs.iter().map(|m| m.id()).collect();
        for envelope in &msgs {
            phone.state.store_envelope(conversation, envelope).unwrap();
        }
        // The pair: the phone recognizes the laptop (what authorizes
        // GetKeys), the laptop recognizes the phone with its homed record
        // (the dial route for backfill + pull)
        phone
            .recognize_device(&ContactRecord::new(
                vec![laptop.public_key()],
                vec![],
                mailbox_only("ll@203.0.113.5:5"),
            ))
            .expect("recognize");
        laptop
            .recognize_device(&ContactRecord::new(
                vec![phone.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            ))
            .expect("recognize back");

        // When: the D2a-style full flow — the laptop holds only the tip,
        // backfills the skeleton by key, then pulls re-wraps
        laptop
            .state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();
        let fetched = laptop
            .backfill_by_key(conversation, phone.public_key())
            .await
            .expect("backfill via the devices store");
        assert_eq!(fetched, 2, "genesis + middle");
        let unreadable = laptop
            .history(conversation)
            .expect("history")
            .iter()
            .filter(|message| message.body.is_err())
            .count();
        assert_eq!(unreadable, 3, "skeleton synced; nothing readable yet");
        let healed = laptop.rewrap_backlog().await;

        // Then: every body opens, and no id moved
        assert_eq!(healed, 3);
        let history = laptop.history(conversation).expect("history");
        let bodies: Vec<Vec<u8>> = history
            .iter()
            .map(|message| message.body.as_ref().expect("opens").clone())
            .collect();
        assert_eq!(
            bodies,
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        assert_eq!(
            history.iter().map(|m| m.id).collect::<BTreeSet<_>>(),
            ids,
            "wraps appended outside the hashed core — ids unchanged"
        );

        let _ = std::fs::remove_dir_all(temp_root("rewrap"));
    }

    #[tokio::test]
    async fn get_keys__should_decline_anyone_but_a_recognized_device() {
        // Given: the phone holds a sealed message; alice is a full
        // contact — served history by the gate — but not a device
        let phone = Client::open_or_create(&temp_key("getkeys", "phone"))
            .await
            .expect("open phone");
        let alice = Client::open_or_create(&temp_key("getkeys", "alice"))
            .await
            .expect("open alice");
        let laptop = Client::open_or_create(&temp_key("getkeys", "laptop"))
            .await
            .expect("open laptop");
        let author = DeviceKey::from_seed([61; 32]);
        let msgs = sealed_chain(&author, phone.public_key(), &[b"secret"]);
        let id = msgs[0].id();
        phone.state.store_envelope(id, &msgs[0]).unwrap();
        befriend(&phone.state, alice.public_key());

        // When: the contact asks for re-wraps
        let connection = net::connect_peer(
            &alice.transport,
            &phone.transport.peer(),
            SYNC_ALPN,
            alice.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let declined = net::sync_request(&connection, SyncOp::GetKeys { ids: vec![id] })
            .await
            .expect("round-trip");

        // Then: declined like a miss — re-wrap serving is narrower than
        // the history gate (own devices only at D3)
        assert_eq!(declined, SyncResult::NotHeld);

        // When: a recognized device asks
        phone
            .recognize_device(&ContactRecord::new(
                vec![laptop.public_key()],
                vec![],
                mailbox_only("ll@203.0.113.5:5"),
            ))
            .expect("recognize");
        let connection = net::connect_peer(
            &laptop.transport,
            &phone.transport.peer(),
            SYNC_ALPN,
            laptop.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let served = net::sync_request(&connection, SyncOp::GetKeys { ids: vec![id] })
            .await
            .expect("round-trip");

        // Then: one fresh wrap, sealed to the caller
        let SyncResult::Wraps { wraps } = served else {
            panic!("expected wraps, got {served:?}");
        };
        assert_eq!(wraps.len(), 1);
        assert_eq!(wraps[0].0, id);
        assert_eq!(wraps[0].1.recipient, laptop.public_key());

        let _ = std::fs::remove_dir_all(temp_root("getkeys"));
    }

    #[tokio::test]
    async fn backfill_by_key__should_fail_plainly_without_a_relay_url_in_the_record() {
        // Given: a stored record that is mailbox-only (raw-contact shape)
        let b = Client::open_or_create(&temp_key("nourl", "client"))
            .await
            .expect("open B");
        let peer = DeviceKey::from_seed([6; 32]).public();
        let record = ContactRecord::new(
            vec![peer],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: None,
            }],
        );
        b.add_contact(&record, Some("peer".to_string()))
            .expect("add contact");

        // When / Then: dial-by-key is impossible and says so — no fabricated
        // reachability, no hang.
        let err = b
            .backfill_by_key(MessageId([1; 32]), peer)
            .await
            .expect_err("no relay url to rendezvous through");
        assert!(matches!(err, Error::NoRelayUrl), "got: {err}");

        let _ = std::fs::remove_dir_all(temp_root("nourl"));
    }

    #[tokio::test]
    async fn backfill__should_stop_when_the_peer_lacks_the_ancestors() {
        // Given: B holds only the latest message; A (the peer) holds nothing.
        let a = Client::open_or_create(&temp_key("stuck", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("stuck", "client"))
            .await
            .expect("open B");
        let author = DeviceKey::from_seed([7; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        b.state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();

        // When: B backfills from a peer that serves nothing
        let fetched = b
            .backfill_addr(conversation, a.transport.peer())
            .await
            .expect("backfill returns Ok even with nothing to fetch");

        // Then: it fetches nothing and gives up rather than looping — honesty
        // over a fabricated root (the genesis is still missing).
        assert_eq!(fetched, 0);
        assert!(b.state.load_dag(conversation).is_err());

        let _ = std::fs::remove_dir_all(temp_root("stuck"));
    }
}
