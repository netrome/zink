//! The mailbox domain: one authenticated request in, one response out.
//! Transport-agnostic — the iroh edge (and any future WebSocket fallback)
//! calls this with the connection's verified key.

use zink_protocol::{
    MailboxErrorCode, MailboxItem, MailboxOp, MailboxRequest, MailboxResponse, MailboxResult,
    PublicKey,
};

use crate::store::MailboxStore;

#[derive(Debug)]
pub struct MailboxService<S> {
    store: S,
}

impl<S: MailboxStore> MailboxService<S> {
    pub fn new(store: S) -> Self {
        Self { store }
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
                self.store.register(caller).await?;
                MailboxResult::Registered
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
                let items = self
                    .store
                    .fetch(caller, after)
                    .await?
                    .into_iter()
                    .map(|(cursor, envelope)| MailboxItem { cursor, envelope })
                    .collect();
                MailboxResult::Envelopes { items }
            }
            MailboxOp::Ack { up_to } => {
                self.store.ack(caller, up_to).await?;
                MailboxResult::Acked
            }
        })
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use zink_protocol::{DeviceKey, FORMAT_VERSION, KeyCommitment, MessageCore, MessageEnvelope};

    use super::*;
    use crate::store::InMemoryStore;

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
}
