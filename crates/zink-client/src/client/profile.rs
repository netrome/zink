//! The profile: this device's public identity — display name, home relays,
//! the published self-record, registration at the home mailboxes, and the
//! avatar flows (D1d). `my_record` publishes exactly what the sync handler
//! serves for a who-is about our own key (`build_own_record` — the two
//! can't drift).

use rand_core::OsRng;
use zink_protocol::{
    Attestation, BlobHash, Claim, ContactRecord, DeviceKey, EncryptedBlob, PublicKey, RelayEntry,
    SignedAttestation, Versioned, open_avatar, seal_avatar,
};

use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::Transport;
use crate::state::ClientState;
use crate::{blobs, net};

use super::Client;

/// What `set_avatar` accomplished (D1d).
pub struct AvatarReceipt {
    /// The ciphertext's content address — what relays cache and serve.
    pub hash: BlobHash,
    /// The claim's supersession counter (bumped per avatar change).
    pub revision: u64,
    /// Home relays that took the push just now. 0 = fetchable by no one
    /// until a later `push_avatar` succeeds — set, but not yet published.
    pub pushed_relays: usize,
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
    /// Set this device's display name and home relays — what `my_record`
    /// publishes and what `recv` drains by default. Each relay is the spec
    /// `zink-relay` prints: `<endpoint-id>@<ip:port>[#<relay-url>]` — the
    /// mailbox dial string, plus the same service's iroh relay URL, which
    /// makes this device reachable by key (D0b; applied at the next open,
    /// since the endpoint's relay transport is fixed at bind time).
    pub async fn set_profile(&self, name: &str, relays: &[String]) -> Result<(), Error> {
        if name.trim().is_empty() {
            return Err(Error::ProfileIncomplete("name must not be empty"));
        }
        let entries: Vec<RelayEntry> = relays.iter().map(|s| RelayEntry::from_spec(s)).collect();
        for entry in &entries {
            crate::adapters::iroh::parse_dial(&entry.mailbox)?;
            if let Some(url) = &entry.relay_url {
                crate::adapters::iroh::parse_relay_url(url)?;
            }
        }
        // A rename supersedes the previous name attestation (SPEC §3.2):
        // bump the persisted revision so receivers holding both claims have
        // a winner. Only *name* changes bump — the counter is scoped per
        // claim-kind; relay changes order by receipt time instead (D1b).
        if let Some(previous) = self.state.profile_name()
            && previous != name.trim()
        {
            self.state
                .save_profile_revision(self.state.profile_revision() + 1)?;
        }
        let previous = self.home_relay_urls()?;
        self.state.save_profile(name.trim(), &entries)?;
        // Home the RUNNING endpoint (De5): the relay transport is always
        // bound (net::bind_endpoint), so map changes apply immediately —
        // a profile save no longer needs a restart to take effect.
        let next = self.home_relay_urls()?;
        for url in next.iter().filter(|url| !previous.contains(url)) {
            self.transport
                .insert_relay(url)
                .await
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
        }
        for url in previous.iter().filter(|url| !next.contains(url)) {
            self.transport.remove_relay(url).await;
        }
        Ok(())
    }

    /// The profile's home-relay URLs (entries without one skipped),
    /// normalized through the parser so `set_profile`'s diff compares the
    /// way the endpoint's relay map does — not raw string spellings.
    fn home_relay_urls(&self) -> Result<Vec<String>, Error> {
        self.state
            .home_relay_entries()
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref())
            .map(|url| crate::adapters::iroh::parse_relay_url(url).map(|url| url.to_string()))
            .collect()
    }

    pub fn profile_name(&self) -> Option<String> {
        self.state.profile_name()
    }

    /// Set this device's label — the device qualifier beside the person
    /// name ("phone", "laptop"; SPEC §3.2 `DeviceLabel`). Optional, and
    /// superseded independently of the name: a relabel bumps its own
    /// revision, a rename never touches it.
    pub fn set_device_label(&self, label: &str) -> Result<(), Error> {
        let label = label.trim();
        if label.is_empty() {
            return Err(Error::InvalidInput("device label must not be empty".into()));
        }
        let revision = match self.state.device_label_meta() {
            Some((previous, revision)) if previous == label => revision,
            Some((_, revision)) => revision + 1,
            None => 0,
        };
        self.state.save_device_label(label, revision)
    }

    pub fn device_label(&self) -> Option<String> {
        self.state.device_label_meta().map(|(label, _)| label)
    }

    /// The home relays' mailbox dial strings — what the mailbox paths
    /// (recv, subscribe, register) dial.
    pub fn home_relays(&self) -> Vec<String> {
        self.state.home_relays()
    }

    /// The home relays as full specs (`dial[#relay-url]`) — the round-trip
    /// form: what an edge shows in a profile form and feeds back into
    /// `set_profile`. Using `home_relays` there instead would silently drop
    /// the relay URL on a re-save.
    pub fn home_relay_specs(&self) -> Vec<String> {
        self.state
            .home_relay_entries()
            .iter()
            .map(RelayEntry::to_spec)
            .collect()
    }

    /// This device's ContactRecord: key, self-attested name, home relays.
    /// The QR/paste payload is `record.to_qr_string()`.
    pub fn my_record(&self) -> Result<ContactRecord, Error> {
        if self.state.profile_name().is_none() {
            return Err(Error::ProfileIncomplete("set a profile name first"));
        }
        build_own_record(&self.device, &self.state)
            .ok_or(Error::ProfileIncomplete("set a home relay first"))
    }

    /// Ensure a mailbox exists on every home relay. Called when publishing
    /// a record: anyone who scans it must be able to deposit immediately —
    /// a record that names a relay where you have no mailbox is a lie.
    pub async fn register_at_home_relays(&self) -> Result<(), Error> {
        // Concurrent (De6d), but still **all-or-error**: publishing a record
        // that names a relay where we have no mailbox is a lie, so any
        // failure is reported. What changed is the price of learning that —
        // one deadline for n relays instead of n, and every reachable relay
        // gets its mailbox even when a sibling is down (serially, a dead
        // *first* relay meant the later ones were never even tried).
        let registrations = self
            .state
            .home_relays()
            .into_iter()
            .map(|relay| async move {
                let connection = net::connect(
                    &self.transport,
                    &relay,
                    zink_protocol::MAILBOX_ALPN,
                    self.config.connect_timeout,
                    &self.clock,
                )
                .await?;
                net::register(&connection, &relay).await?;
                Ok(())
            });
        n0_future::join_all(registrations)
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Error>>()?;
        Ok(())
    }

    /// Set this device's avatar (D1d, who-is-this.md §8): encrypt once
    /// with a fresh key, cache the ciphertext locally (rendering our own
    /// avatar must survive relay TTLs), persist the claim materials at the
    /// next supersession revision, and push the ciphertext to the home
    /// relays. The image should arrive edge-downscaled; the size cap here
    /// is a backstop, not the policy. Republish the record (QR /
    /// `who-is`) for contacts to pick the new claim up.
    pub async fn set_avatar(&self, image: Vec<u8>) -> Result<AvatarReceipt, Error> {
        const MAX_AVATAR_BYTES: usize = 512 * 1024;
        if image.is_empty() {
            return Err(Error::InvalidInput("empty avatar image".into()));
        }
        if image.len() > MAX_AVATAR_BYTES {
            return Err(Error::InvalidInput(format!(
                "avatar too large ({} bytes; max {MAX_AVATAR_BYTES})",
                image.len()
            )));
        }
        let (blob, key) = seal_avatar(&image, &mut OsRng);
        self.state.save_blob(&blob.hash, &blob.bytes)?;
        let revision = self
            .state
            .avatar_meta()
            .map(|(_, _, revision)| revision + 1)
            .unwrap_or(0);
        self.state.save_avatar_meta(&blob.hash, &key, revision)?;
        Ok(AvatarReceipt {
            hash: blob.hash,
            revision,
            pushed_relays: self.push_avatar().await,
        })
    }

    /// Push the current avatar ciphertext to every home relay (relays
    /// dedup by hash) — run at publish, and re-run by long-lived edges on
    /// startup: relay caches expire (30-day TTL), and the publisher's push
    /// is the only source contacts can fetch from. Best-effort per relay;
    /// returns how many took it.
    pub async fn push_avatar(&self) -> usize {
        let Some((hash, _, _)) = self.state.avatar_meta() else {
            return 0;
        };
        let Some(bytes) = self.state.load_blob(&hash) else {
            return 0;
        };
        let blob = EncryptedBlob { hash, bytes };
        let mut pushed = 0;
        for relay in self.state.home_relays() {
            match blobs::push_blobs(
                &self.transport,
                &relay,
                std::slice::from_ref(&blob),
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            {
                Ok(()) => pushed += 1,
                Err(error) => tracing::warn!(relay, %error, "avatar push failed"),
            }
        }
        pushed
    }

    /// Set a local avatar override for a contact (U6, my lens): a photo *I*
    /// chose, stored plaintext on this device only — never published, never a
    /// claim. Wins over the resolved self-claim in `avatar`.
    pub fn set_local_avatar(&self, key: PublicKey, image: Vec<u8>) -> Result<(), Error> {
        if image.len() > 512 * 1024 {
            return Err(Error::InvalidInput("image too large (max 512 KiB)".into()));
        }
        self.state.save_local_avatar(&key, &image)
    }

    /// Drop the local avatar override — `avatar` falls back to the self-claim.
    pub fn clear_local_avatar(&self, key: PublicKey) {
        self.state.remove_local_avatar(&key);
    }

    /// Whether a local avatar override is set for a key (drives the "remove
    /// your photo" affordance).
    pub fn has_local_avatar(&self, key: &PublicKey) -> bool {
        self.state.local_avatar(key).is_some()
    }

    /// The best-believed avatar for a key (D1d): the highest-revision
    /// verified self-issued `Avatar` claim across the stored record and
    /// every learned record; ciphertext from the local cache, else fetched
    /// from the relays of the record that carried the winning claim
    /// (that's where its owner pushes), verified against the claim (hash +
    /// AEAD) and cached. `Ok(None)` for no claim *and* for a claim whose
    /// blob is currently unfetchable — display data is best-effort.
    pub async fn avatar(&self, subject: PublicKey) -> Result<Option<Vec<u8>>, Error> {
        // A local override (U6, my lens) wins over any claim — a photo I
        // chose for them, stored on this device only, never fetched.
        if let Some(bytes) = self.state.local_avatar(&subject) {
            return Ok(Some(bytes));
        }
        if subject == self.device.public() {
            let Some((hash, key, _)) = self.state.avatar_meta() else {
                return Ok(None);
            };
            let Some(bytes) = self.state.load_blob(&hash) else {
                return Ok(None);
            };
            return Ok(Some(open_avatar(&bytes, &hash, &key).map_err(Error::Open)?));
        }
        let mut best: Option<(BlobHash, [u8; 32], u64, Vec<RelayEntry>)> = None;
        let mut consider = |record: &ContactRecord| {
            if let Some((hash, key, revision)) = record.self_avatar_claim()
                && best.as_ref().is_none_or(|(_, _, held, _)| revision > *held)
            {
                best = Some((hash, key, revision, record.relays.clone()));
            }
        };
        for (_, record) in self.state.contacts()? {
            if record.keys.contains(&subject) {
                consider(&record);
            }
        }
        for learned in self.state.learned(&subject) {
            consider(&learned.record);
        }
        let Some((hash, key, _, relays)) = best else {
            return Ok(None);
        };
        if let Some(bytes) = self.state.load_blob(&hash)
            && let Ok(plaintext) = open_avatar(&bytes, &hash, &key)
        {
            return Ok(Some(plaintext));
        }
        for relay in relays {
            match blobs::fetch_encrypted(
                &self.transport,
                &relay.mailbox,
                &hash,
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            {
                Ok(bytes) => match open_avatar(&bytes, &hash, &key) {
                    Ok(plaintext) => {
                        self.state.save_blob(&hash, &bytes)?;
                        return Ok(Some(plaintext));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "served avatar failed verification; skipping")
                    }
                },
                Err(error) => {
                    tracing::debug!(relay = relay.mailbox, %error, "avatar fetch failed")
                }
            }
        }
        Ok(None)
    }
}

/// The self-record — key, self-attested name, home relays — or `None`
/// until the profile is complete (both parts). Shared by `my_record` (the
/// QR/paste publishing path) and the sync handler (serving `WhoIs` about
/// our own key, D1a), so the two can't drift. The attestation `revision`
/// is the persisted supersession counter, bumped per rename (D1b).
pub(crate) fn build_own_record(device: &DeviceKey, state: &ClientState) -> Option<ContactRecord> {
    let name = state.profile_name()?;
    let relays = state.home_relay_entries();
    if relays.is_empty() {
        return None;
    }
    let me = device.public();
    let self_claim = |claim: Claim, revision: u64| {
        SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: me,
                subject: me,
                claim,
                revision,
            },
            device,
        )
    };
    let mut attestations = vec![self_claim(Claim::Name(name), state.profile_revision())];
    // The device label (S1, profile pages): the qualifier beside the name,
    // its own claim kind so the two supersede independently (SPEC §3.2).
    if let Some((label, revision)) = state.device_label_meta() {
        attestations.push(self_claim(Claim::DeviceLabel(label), revision));
    }
    // The avatar claim (D1d): hash + key together, under the signature —
    // whoever holds the record can fetch and decrypt; relays cannot.
    if let Some((hash, key, revision)) = state.avatar_meta() {
        attestations.push(self_claim(Claim::Avatar { hash, key }, revision));
    }
    // The outgoing device vouches (D3b, multi-device.md §4): the record
    // gains exactly this — links live in the record's attestations
    // (SPEC §3.6). `keys` stays this device's own key; observers gather
    // link evidence across the records they hold.
    attestations.extend(state.device_vouches());
    // …and the issued repudiations (D4b, web-of-trust.md §5): a lost key's
    // disavowal reaches contacts through any freshness pull on US — the
    // endorsement channel needs a servable record for the *subject*, which
    // an un-recognized key no longer has.
    attestations.extend(state.issued_negatives());
    Some(ContactRecord::new(vec![me], attestations, relays))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::client::test_kit::{temp_key, temp_root};
    use crate::hex;

    #[tokio::test]
    async fn set_profile__should_bump_the_name_attestation_revision_on_rename_only() {
        // Given: a valid dial string (any 32-byte key is an endpoint id)
        let a = Client::open_or_create(&temp_key("rev", "me"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        let revision = |client: &Client| {
            client
                .my_record()
                .expect("record")
                .self_name_claim()
                .expect("claim")
                .1
        };

        // When / Then: first profile starts at 0; a re-save of the same
        // name doesn't bump; a rename supersedes (SPEC §3.2)
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("set");
        assert_eq!(revision(&a), 0);
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("re-set");
        assert_eq!(revision(&a), 0);
        a.set_profile("alicia", std::slice::from_ref(&relay))
            .await
            .expect("rename");
        assert_eq!(revision(&a), 1);

        let _ = std::fs::remove_dir_all(temp_root("rev"));
    }

    #[tokio::test]
    async fn set_device_label__should_supersede_independently_of_the_name() {
        // Given
        let a = Client::open_or_create(&temp_key("label", "me"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("mårten", std::slice::from_ref(&relay))
            .await
            .expect("profile");

        // When: label set, re-set unchanged, relabeled — then a rename
        a.set_device_label("phone").expect("set");
        let set = a.my_record().expect("record");
        a.set_device_label("phone").expect("re-set");
        let re_set = a.my_record().expect("record");
        a.set_device_label("laptop").expect("relabel");
        a.set_profile("mårten ii", std::slice::from_ref(&relay))
            .await
            .expect("rename");
        let renamed = a.my_record().expect("record");

        // Then: the label supersedes on change only, and the rename bumps
        // the name's revision without touching the label's (SPEC §3.2
        // per-claim-kind scopes)
        assert_eq!(set.self_device_label_claim(), Some(("phone", 0)));
        assert_eq!(re_set.self_device_label_claim(), Some(("phone", 0)));
        assert_eq!(renamed.self_device_label_claim(), Some(("laptop", 1)));
        assert_eq!(renamed.self_name_claim(), Some(("mårten ii", 1)));

        let _ = std::fs::remove_dir_all(temp_root("label"));
    }

    #[tokio::test]
    async fn set_avatar__should_supersede_and_render_our_own() {
        // Given (avatars first: no profile relays yet, so the push loop has
        // nothing to dial and the test stays offline)
        let a = Client::open_or_create(&temp_key("avatar", "me"))
            .await
            .expect("open");

        // When: an avatar is set, then replaced
        let first = a
            .set_avatar(b"first image bytes".to_vec())
            .await
            .expect("set");
        let second = a
            .set_avatar(b"second image bytes".to_vec())
            .await
            .expect("replace");

        // Then: supersession counts up; the published record carries the
        // current claim; our own avatar renders from the local cache
        assert_eq!((first.revision, second.revision), (0, 1));
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let record = a.my_record().expect("record");
        assert_eq!(
            record
                .self_avatar_claim()
                .map(|(hash, _, revision)| (hash, revision)),
            Some((second.hash, 1))
        );
        let rendered = a.avatar(a.public_key()).await.expect("avatar");
        assert_eq!(rendered.as_deref(), Some(b"second image bytes".as_slice()));

        let _ = std::fs::remove_dir_all(temp_root("avatar"));
    }

    #[tokio::test]
    async fn avatar__should_render_a_contacts_avatar_from_the_verified_cache() {
        // Given: A set an avatar and published a record carrying the claim;
        // B stores that record as a contact and holds the ciphertext in its
        // blob cache — exactly what a successful fetch leaves behind
        let a = Client::open_or_create(&temp_key("avatarb", "a"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("avatarb", "b"))
            .await
            .expect("open B");
        let receipt = a.set_avatar(b"portrait".to_vec()).await.expect("set");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let ciphertext = a.state.load_blob(&receipt.hash).expect("cached at set");
        b.state.save_blob(&receipt.hash, &ciphertext).expect("seed");
        b.add_contact(&a.my_record().expect("record"), None)
            .expect("add");

        // When
        let rendered = b.avatar(a.public_key()).await.expect("avatar");

        // Then: decrypted via the claim's key; at rest it stays ciphertext
        assert_eq!(rendered.as_deref(), Some(b"portrait".as_slice()));
        assert_ne!(
            b.state.load_blob(&receipt.hash).expect("still cached"),
            b"portrait".to_vec(),
            "cache holds ciphertext, like a relay would"
        );

        let _ = std::fs::remove_dir_all(temp_root("avatarb"));
    }
}
