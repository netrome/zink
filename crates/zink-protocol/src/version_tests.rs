//! Cross-type checks for [`crate::Versioned`] (SPEC §10).
//!
//! These guard the property the per-type scheme exists for: a bump to one
//! object must not fork the others. Per-type tests live with their types;
//! what needs a home of its own is the *relationship* between them.

#![allow(non_snake_case)]

use borsh::{BorshDeserialize, BorshSerialize};

use crate::codec::DecodeError;
use crate::{
    Attestation, ContactRecord, MailboxRequest, MailboxResponse, MessageCore, MessageEnvelope,
    SignedAttestation, SyncRequest, SyncResponse, Versioned,
};

/// `ACCEPTED` must contain `CURRENT`, or the type cannot decode what it
/// itself writes — an easy thing to get wrong when widening the set during a
/// future bump, and silent until something round-trips.
#[test]
fn accepted__should_contain_current_for_every_versioned_type() {
    fn check<T: Versioned>(name: &str) {
        assert!(
            T::ACCEPTED.contains(&T::CURRENT),
            "{name}: ACCEPTED {:?} does not contain CURRENT {}",
            T::ACCEPTED,
            T::CURRENT
        );
    }
    check::<MessageCore>("MessageCore");
    check::<MessageEnvelope>("MessageEnvelope");
    check::<Attestation>("Attestation");
    check::<SignedAttestation>("SignedAttestation");
    check::<ContactRecord>("ContactRecord");
    check::<MailboxRequest>("MailboxRequest");
    check::<MailboxResponse>("MailboxResponse");
    check::<SyncRequest>("SyncRequest");
    check::<SyncResponse>("SyncResponse");
}

/// `MessageCore`'s version lives *inside the hashed core*, so moving it
/// re-hashes every message that exists. Nothing stops a future bump — but it
/// must be a deliberate act with a migration behind it, not a side effect of
/// changing some neighbouring object. This test is the tripwire.
#[test]
fn message_core__should_still_be_at_version_one() {
    assert_eq!(
        MessageCore::CURRENT,
        1,
        "bumping MessageCore moves every message id — see Versioned's docs"
    );
}

/// The whole point, demonstrated: one type widening its accepted set leaves
/// every other type's decoding untouched. Uses local stand-ins so the real
/// constants stay at 1 while the *mechanism* is exercised end to end.
#[test]
fn decode_versioned__should_judge_each_type_by_its_own_accepted_set() {
    #[derive(BorshSerialize, BorshDeserialize, PartialEq, Eq, Debug)]
    struct Bumped {
        version: u16,
        payload: u8,
    }
    // A type mid-migration: writes 2, still reads 1.
    impl Versioned for Bumped {
        const CURRENT: u16 = 2;
        const ACCEPTED: &'static [u16] = &[1, 2];
    }

    #[derive(BorshSerialize, BorshDeserialize, PartialEq, Eq, Debug)]
    struct Unbumped {
        version: u16,
        payload: u8,
    }
    // Its neighbour, which never moved.
    impl Versioned for Unbumped {
        const CURRENT: u16 = 1;
        const ACCEPTED: &'static [u16] = &[1];
    }

    let at = |version: u16| {
        borsh::to_vec(&Bumped {
            version,
            payload: 7,
        })
        .expect("encode")
    };

    // Then: the bumped type reads both of its versions…
    assert_eq!(
        crate::codec::decode_versioned::<Bumped>(&at(1)),
        Ok(Bumped {
            version: 1,
            payload: 7
        })
    );
    assert_eq!(
        crate::codec::decode_versioned::<Bumped>(&at(2)),
        Ok(Bumped {
            version: 2,
            payload: 7
        })
    );
    // …and rejects one it never claimed.
    assert_eq!(
        crate::codec::decode_versioned::<Bumped>(&at(3)),
        Err(DecodeError::UnsupportedVersion { found: 3 })
    );
    // …while the neighbour is unaffected by the bump — the property the old
    // single global `FORMAT_VERSION` could not give us, since v2 bytes then
    // meant *every* type stopped decoding v1.
    assert_eq!(
        crate::codec::decode_versioned::<Unbumped>(&at(2)),
        Err(DecodeError::UnsupportedVersion { found: 2 })
    );
    assert!(crate::codec::decode_versioned::<Unbumped>(&at(1)).is_ok());
}
