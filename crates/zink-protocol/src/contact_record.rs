//! The contact / pairing record (SPEC §3.6): everything needed to reach and
//! render a person, exchanged out-of-band (QR / link) when adding a contact
//! — and, later, when pairing your own next device.

use borsh::{BorshDeserialize, BorshSerialize};
use data_encoding::BASE32_NOPAD;

use crate::FORMAT_VERSION;
use crate::attestation::{Claim, SignedAttestation};
use crate::codec::{self, DecodeError};
use crate::keys::PublicKey;

/// QR payloads are `ZINK:<base32(borsh)>` — all uppercase-alphanumeric, so
/// QR encoders can use the compact alphanumeric mode.
const QR_PREFIX: &str = "ZINK:";

/// One relay *service* a device uses: the mailbox endpoint and — the same
/// binary — the iroh relay server coordinating peer connectivity (D0b).
/// One structured entry, not parallel lists: both fields address the same
/// service and must never drift apart. No own `version` field — the entry
/// is plain structure inside the record, governed by the record's version.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct RelayEntry {
    /// Mailbox dial string `<endpoint-id>@<ip:port>` — where deposits go
    /// and mailboxes are drained.
    pub mailbox: String,
    /// iroh relay URL (`http://host:port/`) of the same relay service —
    /// what a device homes to (`RelayMode::Custom`) and what a peer dials
    /// through to reach this device by key (rendezvous + holepunch
    /// coordination). `None` = mailbox-only knowledge (e.g. a raw contact
    /// spec): deposits work, dial-by-key doesn't.
    pub relay_url: Option<String>,
}

impl RelayEntry {
    /// Parse the human/CLI spec `<mailbox-dial>[#<relay-url>]` — the form
    /// `zink-relay` prints and profiles/QR flows pass around. Pure string
    /// splitting; the parts are validated where they're used (the mailbox
    /// dial by the dialing edge, the URL by the endpoint builder).
    pub fn from_spec(spec: &str) -> Self {
        match spec.split_once('#') {
            Some((mailbox, url)) if !url.trim().is_empty() => Self {
                mailbox: mailbox.trim().to_string(),
                relay_url: Some(url.trim().to_string()),
            },
            _ => Self {
                mailbox: spec.trim().trim_end_matches('#').to_string(),
                relay_url: None,
            },
        }
    }

    /// The inverse of [`Self::from_spec`].
    pub fn to_spec(&self) -> String {
        match &self.relay_url {
            Some(url) => format!("{}#{url}", self.mailbox),
            None => self.mailbox.clone(),
        }
    }
}

/// The rendezvous record: whom to fan out to, how to display them, and
/// where their mailboxes live when they're offline.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct ContactRecord {
    pub version: u16,
    /// The person's current device keys (one per device; multi-device D2).
    pub keys: Vec<PublicKey>,
    /// Self-attestations — name now; avatar / same-person-as links later.
    pub attestations: Vec<SignedAttestation>,
    /// The relay services hosting this person's mailboxes and coordinating
    /// peer connectivity. (Field added in-place at version 1 pre-deployment,
    /// 2026-07-18 — replaced bare dial strings; dev-stage records were
    /// re-exchanged.)
    pub relays: Vec<RelayEntry>,
}

impl ContactRecord {
    pub fn new(
        keys: Vec<PublicKey>,
        attestations: Vec<SignedAttestation>,
        relays: Vec<RelayEntry>,
    ) -> Self {
        Self {
            version: FORMAT_VERSION,
            keys,
            attestations,
            relays,
        }
    }

    /// The name this person claims for themselves: the first `Name`
    /// attestation that verifies and is genuinely *self*-issued (attester =
    /// subject, and that key is in the record). Anything else — forged
    /// signatures, third-party claims, keys outside the record — is ignored,
    /// never an error: the record came from an untrusted channel.
    pub fn self_claimed_name(&self) -> Option<&str> {
        self.attestations.iter().find_map(|signed| {
            let attestation = &signed.attestation;
            let Claim::Name(name) = &attestation.claim else {
                return None;
            };
            let self_issued = attestation.attester == attestation.subject
                && self.keys.contains(&attestation.attester);
            (self_issued && signed.verify().is_ok()).then_some(name.as_str())
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        codec::canonical_bytes(self)
    }

    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        codec::decode_versioned(bytes)
    }

    /// The QR / link payload.
    pub fn to_qr_string(&self) -> String {
        format!("{QR_PREFIX}{}", BASE32_NOPAD.encode(&self.to_bytes()))
    }

    /// Parse a scanned / pasted payload. Whitespace-tolerant (QR scanners
    /// and clipboards mangle), case-tolerant on the prefix.
    pub fn from_qr_string(payload: &str) -> Result<Self, DecodeError> {
        let compact: String = payload.split_whitespace().collect();
        let rest = compact
            .strip_prefix(QR_PREFIX)
            .or_else(|| compact.strip_prefix("zink:"))
            .ok_or(DecodeError::Malformed)?;
        let bytes = BASE32_NOPAD
            .decode(rest.to_ascii_uppercase().as_bytes())
            .map_err(|_| DecodeError::Malformed)?;
        Self::try_from_bytes(&bytes)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::attestation::Attestation;
    use crate::keys::DeviceKey;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    fn name_attestation(attester: &DeviceKey, subject: PublicKey, name: &str) -> SignedAttestation {
        SignedAttestation::new(
            Attestation {
                version: FORMAT_VERSION,
                attester: attester.public(),
                subject,
                claim: Claim::Name(name.to_string()),
                revision: 0,
            },
            attester,
        )
    }

    fn record_for(device: &DeviceKey, name: &str) -> ContactRecord {
        ContactRecord::new(
            vec![device.public()],
            vec![name_attestation(device, device.public(), name)],
            vec![RelayEntry::from_spec(
                "someid@203.0.113.7:4400#http://203.0.113.7:4401",
            )],
        )
    }

    #[test]
    fn relay_entry_spec__should_roundtrip_with_and_without_a_relay_url() {
        // Given / When / Then
        for spec in [
            "someid@203.0.113.7:4400#http://203.0.113.7:4401",
            "someid@203.0.113.7:4400",
        ] {
            let entry = RelayEntry::from_spec(spec);
            assert_eq!(entry.to_spec(), spec);
        }
        assert_eq!(
            RelayEntry::from_spec("id@1.2.3.4:5#http://1.2.3.4:6").relay_url,
            Some("http://1.2.3.4:6".to_string())
        );
    }

    #[test]
    fn relay_entry_spec__should_treat_a_trailing_hash_or_whitespace_as_no_url() {
        // Given / When
        let entry = RelayEntry::from_spec("  someid@203.0.113.7:4400#  ");

        // Then
        assert_eq!(entry.mailbox, "someid@203.0.113.7:4400");
        assert_eq!(entry.relay_url, None);
    }

    #[test]
    fn qr_string__should_roundtrip_and_stay_alphanumeric() {
        // Given
        let record = record_for(&device_key(1), "Mårten");

        // When
        let payload = record.to_qr_string();

        // Then: QR alphanumeric mode charset, and a faithful roundtrip
        assert!(
            payload
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ':')
        );
        assert_eq!(ContactRecord::from_qr_string(&payload).unwrap(), record);
    }

    #[test]
    fn from_qr_string__should_tolerate_whitespace_and_lowercase_prefix() {
        // Given: a payload as a clipboard might mangle it
        let record = record_for(&device_key(1), "Alice");
        let mangled = record
            .to_qr_string()
            .replace("ZINK:", "zink:")
            .chars()
            .enumerate()
            .flat_map(|(i, c)| if i % 20 == 0 { vec![' ', c] } else { vec![c] })
            .collect::<String>();

        // When / Then
        assert_eq!(ContactRecord::from_qr_string(&mangled).unwrap(), record);
    }

    #[test]
    fn from_qr_string__should_error_on_hostile_input_without_panicking() {
        for bad in ["", "ZINK:", "ZINK:!!!!", "notaprefix", "ZINK:AAAA"] {
            assert!(ContactRecord::from_qr_string(bad).is_err());
        }
    }

    #[test]
    fn try_from_bytes__should_reject_an_unsupported_version() {
        // Given
        let mut bytes = record_for(&device_key(1), "Alice").to_bytes();
        bytes[0..2].copy_from_slice(&9u16.to_le_bytes());

        // When / Then
        assert_eq!(
            ContactRecord::try_from_bytes(&bytes),
            Err(DecodeError::UnsupportedVersion { found: 9 })
        );
    }

    #[test]
    fn self_claimed_name__should_return_a_valid_self_attested_name() {
        let record = record_for(&device_key(1), "Alice");
        assert_eq!(record.self_claimed_name(), Some("Alice"));
    }

    #[test]
    fn self_claimed_name__should_ignore_names_signed_by_a_key_outside_the_record() {
        // Given: an attestation whose attester is not one of the record keys
        let me = device_key(1);
        let outsider = device_key(9);
        let record = ContactRecord::new(
            vec![me.public()],
            vec![name_attestation(&outsider, outsider.public(), "Mallory")],
            vec![],
        );

        // When / Then
        assert_eq!(record.self_claimed_name(), None);
    }

    #[test]
    fn self_claimed_name__should_ignore_a_tampered_attestation() {
        // Given: a valid record whose name attestation gets its claim swapped
        let me = device_key(1);
        let mut record = record_for(&me, "Alice");
        record.attestations[0].attestation.claim = Claim::Name("Mallory".to_string());

        // When / Then: signature no longer matches — ignored
        assert_eq!(record.self_claimed_name(), None);
    }

    #[test]
    fn self_claimed_name__should_ignore_third_party_claims() {
        // Given: a name claim *about* me signed by me, but subject ≠ attester
        let me = device_key(1);
        let other = device_key(2);
        let record = ContactRecord::new(
            vec![me.public()],
            vec![name_attestation(&me, other.public(), "NotSelf")],
            vec![],
        );

        // When / Then
        assert_eq!(record.self_claimed_name(), None);
    }
}
