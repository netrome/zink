//! The mailbox domain: one authenticated request in, one response out.
//! Transport-agnostic — the iroh edge (and any future WebSocket fallback)
//! calls this with the connection's verified key.

use zink_protocol::{
    MAX_FETCH_PAGE_BYTES, MailboxErrorCode, MailboxItem, MailboxOp, MailboxRequest,
    MailboxResponse, MailboxResult, PublicKey,
};

use crate::admission::{Admission, OpenToAll};
use crate::store::{DEFAULT_MAX_MAILBOXES, MailboxStore};

#[derive(Debug)]
pub struct MailboxService<S, A = OpenToAll> {
    store: S,
    /// Who may register (operator policy, SPEC §5.3). Consulted in the
    /// domain rather than the store: *who we serve* is a decision, storage
    /// is a mechanism.
    admission: A,
    /// The hard ceiling on hosted mailboxes — see `DEFAULT_MAX_MAILBOXES`.
    max_mailboxes: usize,
}

impl<S: MailboxStore> MailboxService<S> {
    /// Open to any key, bounded only by `DEFAULT_MAX_MAILBOXES`. The dev and
    /// test default; an operator on a shared box wants `with_admission`.
    pub fn new(store: S) -> Self {
        Self {
            store,
            admission: OpenToAll,
            max_mailboxes: DEFAULT_MAX_MAILBOXES,
        }
    }
}

impl<S: MailboxStore, A: Admission> MailboxService<S, A> {
    pub fn with_admission(store: S, admission: A, max_mailboxes: usize) -> Self {
        Self {
            store,
            admission,
            max_mailboxes,
        }
    }

    /// Handle one request from `caller` — the key that authenticated the
    /// connection. Register/fetch/ack act on the caller's own mailbox only;
    /// deposit fans the envelope into its recipients' registered mailboxes.
    /// A storage failure answers `Internal` — never a false acknowledgment.
    pub async fn handle(&self, caller: PublicKey, request: MailboxRequest) -> MailboxResponse {
        let result = self
            .dispatch(caller, request.op)
            .await
            .unwrap_or(MailboxResult::Error {
                code: MailboxErrorCode::Internal,
            });
        MailboxResponse::new(result)
    }

    async fn dispatch(&self, caller: PublicKey, op: MailboxOp) -> std::io::Result<MailboxResult> {
        Ok(match op {
            MailboxOp::Register => {
                if self.admits(caller).await? {
                    self.store.register(caller).await?;
                    MailboxResult::Registered
                } else {
                    // Honest refusal, not a false `Registered` (§handle): a
                    // caller told it is registered would sit waiting on mail
                    // this relay is never going to hold for it.
                    tracing::info!("refusing a mailbox registration (operator policy or full)");
                    MailboxResult::Error {
                        code: MailboxErrorCode::Refused,
                    }
                }
            }
            MailboxOp::Deposit { envelope } => {
                let id = envelope.id();
                for recipient in envelope.core.recipients.clone() {
                    // A partial deposit answers Internal; the sender's retry
                    // is safe (dedup by id).
                    self.store.append(recipient, (*envelope).clone()).await?;
                }
                MailboxResult::Deposited { id }
            }
            MailboxOp::Fetch { after } => {
                // Page the response: stop before it would exceed the fetch
                // budget, but always include at least one envelope so a
                // mailbox never wedges. The client loops until a page is
                // empty. Items are cursor-ascending, so a truncated page is
                // simply resumed from its last cursor.
                let mut items = Vec::new();
                let mut bytes = 0usize;
                for (cursor, envelope) in self.store.fetch(caller, after).await? {
                    let size = envelope.to_bytes().len();
                    if !items.is_empty() && bytes + size > MAX_FETCH_PAGE_BYTES {
                        break;
                    }
                    bytes += size;
                    items.push(MailboxItem { cursor, envelope });
                }
                MailboxResult::Envelopes { items }
            }
            MailboxOp::Ack { up_to } => {
                self.store.ack(caller, up_to).await?;
                MailboxResult::Acked
            }
        })
    }

    /// May `caller` hold a mailbox here? Operator policy first, then the
    /// ceiling. **A key we already host always passes**: a re-registration
    /// takes no new slot, and refusing it would break every reconnect the
    /// moment the relay filled up — turning a capacity limit into an outage
    /// for the people already using it.
    async fn admits(&self, caller: PublicKey) -> std::io::Result<bool> {
        if !self.admission.permits(&caller) {
            return Ok(false);
        }
        if self.store.is_registered(caller).await? {
            return Ok(true);
        }
        Ok(self.store.registered_count().await? < self.max_mailboxes)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use zink_protocol::{DeviceKey, FORMAT_VERSION, KeyCommitment, MessageCore, MessageEnvelope};

    use super::*;
    use crate::store::{DEFAULT_MAILBOX_MAX_ITEMS, DEFAULT_MAILBOX_RETENTION, InMemoryStore};

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    fn envelope_to(recipients: &[PublicKey], body: &[u8]) -> MessageEnvelope {
        let sender = device_key(1);
        let core = MessageCore {
            version: FORMAT_VERSION,
            conversation: None,
            parents: vec![],
            recipients: recipients.to_vec(),
            sender: sender.public(),
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            body: body.to_vec(),
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        };
        MessageEnvelope::new(core, &sender)
    }

    fn service() -> MailboxService<InMemoryStore> {
        MailboxService::new(InMemoryStore::new())
    }

    fn deposit(envelope: MessageEnvelope) -> MailboxRequest {
        MailboxRequest::new(MailboxOp::Deposit {
            envelope: Box::new(envelope),
        })
    }

    async fn fetched_items(
        service: &MailboxService<InMemoryStore>,
        caller: PublicKey,
        after: u64,
    ) -> Vec<MailboxItem> {
        match service
            .handle(caller, MailboxRequest::new(MailboxOp::Fetch { after }))
            .await
            .result
        {
            MailboxResult::Envelopes { items } => items,
            other => panic!("expected Envelopes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deposit__should_be_fetchable_by_a_registered_recipient() {
        // Given
        let service = service();
        let recipient = device_key(2).public();
        let sender = device_key(1).public();
        service
            .handle(recipient, MailboxRequest::new(MailboxOp::Register))
            .await;

        // When
        let envelope = envelope_to(&[recipient], b"ciphertext");
        service.handle(sender, deposit(envelope.clone())).await;

        // Then
        let items = fetched_items(&service, recipient, 0).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].envelope, envelope);
    }

    #[tokio::test]
    async fn deposit__should_skip_unregistered_recipients() {
        // Given: nobody registered
        let service = service();
        let recipient = device_key(2).public();

        // When
        let envelope = envelope_to(&[recipient], b"x");
        service
            .handle(device_key(1).public(), deposit(envelope))
            .await;
        service
            .handle(recipient, MailboxRequest::new(MailboxOp::Register))
            .await;

        // Then: registering afterwards does not resurrect the deposit
        assert!(fetched_items(&service, recipient, 0).await.is_empty());
    }

    #[tokio::test]
    async fn deposit__should_dedup_by_message_id() {
        // Given
        let service = service();
        let recipient = device_key(2).public();
        let sender = device_key(1).public();
        service
            .handle(recipient, MailboxRequest::new(MailboxOp::Register))
            .await;

        // When: the same envelope deposited twice (sender retry)
        let envelope = envelope_to(&[recipient], b"once");
        for _ in 0..2 {
            service.handle(sender, deposit(envelope.clone())).await;
        }

        // Then
        assert_eq!(fetched_items(&service, recipient, 0).await.len(), 1);
    }

    #[tokio::test]
    async fn fetch__should_only_return_items_after_the_cursor() {
        // Given: two deposits
        let service = service();
        let recipient = device_key(2).public();
        let sender = device_key(1).public();
        service
            .handle(recipient, MailboxRequest::new(MailboxOp::Register))
            .await;
        for body in [b"first".as_slice(), b"second".as_slice()] {
            service
                .handle(sender, deposit(envelope_to(&[recipient], body)))
                .await;
        }

        // When
        let all = fetched_items(&service, recipient, 0).await;
        let after_first = fetched_items(&service, recipient, all[0].cursor).await;

        // Then
        assert_eq!(all.len(), 2);
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].envelope, all[1].envelope);
    }

    #[tokio::test]
    async fn ack__should_drop_delivered_envelopes() {
        // Given
        let service = service();
        let recipient = device_key(2).public();
        service
            .handle(recipient, MailboxRequest::new(MailboxOp::Register))
            .await;
        service
            .handle(
                device_key(1).public(),
                deposit(envelope_to(&[recipient], b"drop me")),
            )
            .await;
        let cursor = fetched_items(&service, recipient, 0).await[0].cursor;

        // When
        service
            .handle(
                recipient,
                MailboxRequest::new(MailboxOp::Ack { up_to: cursor }),
            )
            .await;

        // Then
        assert!(fetched_items(&service, recipient, 0).await.is_empty());
    }

    #[tokio::test]
    async fn deposit__should_answer_internal_when_storage_fails() {
        // Given: a store whose writes fail (e.g. disk full)
        struct FailingStore;
        impl crate::store::MailboxStore for FailingStore {
            async fn register(&self, _: PublicKey) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
            async fn append(&self, _: PublicKey, _: MessageEnvelope) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
            async fn fetch(
                &self,
                _: PublicKey,
                _: u64,
            ) -> std::io::Result<Vec<(u64, MessageEnvelope)>> {
                Err(std::io::Error::other("disk full"))
            }
            async fn ack(&self, _: PublicKey, _: u64) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
            async fn registered_count(&self) -> std::io::Result<usize> {
                Ok(0)
            }
            async fn is_registered(&self, _: PublicKey) -> std::io::Result<bool> {
                Ok(false)
            }
        }
        let service = MailboxService::new(FailingStore);

        // When
        let response = service
            .handle(
                device_key(1).public(),
                deposit(envelope_to(&[device_key(2).public()], b"x")),
            )
            .await;

        // Then: never a false `Deposited`
        assert_eq!(
            response.result,
            MailboxResult::Error {
                code: MailboxErrorCode::Internal
            }
        );
    }

    #[tokio::test]
    async fn deposit__should_fan_out_to_every_registered_recipient() {
        // Given
        let service = service();
        let (b, c) = (device_key(2).public(), device_key(3).public());
        for mailbox in [b, c] {
            service
                .handle(mailbox, MailboxRequest::new(MailboxOp::Register))
                .await;
        }

        // When: one envelope addressed to both
        service
            .handle(
                device_key(1).public(),
                deposit(envelope_to(&[b, c], b"both")),
            )
            .await;

        // Then
        assert_eq!(fetched_items(&service, b, 0).await.len(), 1);
        assert_eq!(fetched_items(&service, c, 0).await.len(), 1);
    }

    /// Register and report whether the relay accepted.
    async fn register<S: MailboxStore, A: Admission>(
        service: &MailboxService<S, A>,
        key: PublicKey,
    ) -> MailboxResult {
        service
            .handle(key, MailboxRequest::new(MailboxOp::Register))
            .await
            .result
    }

    #[tokio::test]
    async fn register__should_refuse_a_key_the_operator_did_not_allow() {
        // Given: an allow-list naming exactly one key (R1 — operator policy,
        // SPEC §5.3)
        #[derive(Debug)]
        struct OnlyFirst;
        impl Admission for OnlyFirst {
            fn permits(&self, key: &PublicKey) -> bool {
                *key == device_key(1).public()
            }
        }
        let service =
            MailboxService::with_admission(InMemoryStore::new(), OnlyFirst, DEFAULT_MAX_MAILBOXES);

        // When
        let allowed = register(&service, device_key(1).public()).await;
        let stranger = register(&service, device_key(2).public()).await;

        // Then: refused honestly — never a false `Registered`, which would
        // leave the caller waiting on mail this relay will not hold.
        assert_eq!(allowed, MailboxResult::Registered);
        assert_eq!(
            stranger,
            MailboxResult::Error {
                code: MailboxErrorCode::Refused
            }
        );
    }

    #[tokio::test]
    async fn register__should_stop_at_the_mailbox_ceiling() {
        // Given: a relay that will host two mailboxes
        let service = MailboxService::with_admission(InMemoryStore::new(), OpenToAll, 2);

        // When: three distinct keys register
        let first = register(&service, device_key(1).public()).await;
        let second = register(&service, device_key(2).public()).await;
        let third = register(&service, device_key(3).public()).await;

        // Then: the disk ceiling holds — open registration alone bounds
        // nothing, which is the whole point of the backstop.
        assert_eq!(first, MailboxResult::Registered);
        assert_eq!(second, MailboxResult::Registered);
        assert_eq!(
            third,
            MailboxResult::Error {
                code: MailboxErrorCode::Refused
            }
        );
    }

    #[tokio::test]
    async fn register__should_still_admit_a_key_it_already_hosts_when_full() {
        // Given: a relay filled to its ceiling by one key
        let service = MailboxService::with_admission(InMemoryStore::new(), OpenToAll, 1);
        let resident = device_key(1).public();
        register(&service, resident).await;

        // When: that key reconnects (every subscription re-registers)
        let again = register(&service, resident).await;

        // Then: admitted — it takes no new slot. Refusing here would turn a
        // capacity limit into an outage for the people already using it.
        assert_eq!(again, MailboxResult::Registered);
        assert_eq!(
            register(&service, device_key(2).public()).await,
            MailboxResult::Error {
                code: MailboxErrorCode::Refused
            },
            "…while a genuinely new key is still refused"
        );
    }

    #[tokio::test]
    async fn deposit__should_stop_at_the_byte_cap_well_under_the_item_cap() {
        // Given: a mailbox capped at 4 KiB but 1024 items — the item cap
        // alone bounds nothing, since an envelope may be up to 1 MiB.
        let store = InMemoryStore::with_caps(
            DEFAULT_MAILBOX_RETENTION,
            DEFAULT_MAILBOX_MAX_ITEMS,
            4096,
            crate::clock::SystemClock,
        );
        let service = MailboxService::new(store);
        let mailbox = device_key(2).public();
        register(&service, mailbox).await;

        // When: 1 KiB bodies are deposited until they stop landing
        for i in 0..20u32 {
            let mut body = vec![0u8; 1024];
            body[0..4].copy_from_slice(&i.to_le_bytes()); // distinct ids
            service
                .handle(
                    device_key(1).public(),
                    deposit(envelope_to(&[mailbox], &body)),
                )
                .await;
        }

        // Then: bounded by bytes, far short of the 1024-item cap
        let items = fetched_items(&service, mailbox, 0).await;
        assert!(!items.is_empty(), "some mail got through");
        assert!(items.len() < 20, "the byte cap stopped the rest");
        let held: usize = items.iter().map(|i| i.envelope.to_bytes().len()).sum();
        assert!(held <= 4096, "held {held} bytes, over the cap");
    }

    #[tokio::test]
    async fn fetch__should_page_a_mailbox_too_large_for_one_response() {
        // Given: enough ~1 MiB envelopes to exceed the fetch page budget.
        // This test is about *paging*, so it opts out of R1's byte cap —
        // the default (8 MiB) would stop the deposits long before the page
        // budget was reached, which is the cap working, not a paging bug.
        let service = MailboxService::new(InMemoryStore::with_caps(
            DEFAULT_MAILBOX_RETENTION,
            DEFAULT_MAILBOX_MAX_ITEMS,
            u64::MAX,
            crate::clock::SystemClock,
        ));
        let mailbox = device_key(2).public();
        service
            .handle(mailbox, MailboxRequest::new(MailboxOp::Register))
            .await;
        let count = (MAX_FETCH_PAGE_BYTES / (1 << 20)) + 3;
        for i in 0..count {
            let mut body = vec![0u8; 1 << 20];
            body[0] = i as u8; // distinct bodies → distinct ids
            service
                .handle(
                    device_key(1).public(),
                    deposit(envelope_to(&[mailbox], &body)),
                )
                .await;
        }

        // When: draining page-by-page like the client does
        let mut drained = 0;
        let mut after = 0;
        loop {
            let page = fetched_items(&service, mailbox, after).await;
            if page.is_empty() {
                break;
            }
            // Each page is bounded — never the whole mailbox at once.
            assert!(page.len() < count, "page was not bounded: {}", page.len());
            after = page.iter().map(|item| item.cursor).max().unwrap();
            drained += page.len();
        }

        // Then: every message is delivered across the pages
        assert_eq!(drained, count);
    }
}
