use std::{
    mem::{self, offset_of},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use ahash::AHashSet;
use dlp_api::state::{
    discriminator::{AccountDiscriminator, AccountWithDiscriminator},
    DelegationRecord,
};
use futures::{SinkExt, StreamExt};
use solana_account::AccountBuilder;
use tokio::{sync::mpsc, time::timeout};
use yellowstone_grpc_client::{
    ClientTlsConfig, GeyserGrpcClient, ReconnectConfig, SubscribeRequestSink,
};
use yellowstone_grpc_proto::{
    cuckoo::CompressedAccountFilterSet,
    geyser::{
        subscribe_request_filter_accounts_filter::Filter,
        subscribe_request_filter_accounts_filter_memcmp::Data, subscribe_update::UpdateOneof,
    },
    prelude::*,
};

use super::{
    client::SubscriptionUpdate, delegation::Delegations, transaction, Config, Delegation, Error,
    Event,
};

/// Owns retained membership and delegation state for one provider stream.
pub(super) struct Session {
    /// Endpoint, authority, and provider credentials.
    config: Config,
    /// Authoritative exact-membership filter, including reconnect snapshots.
    accounts: CompressedAccountFilterSet,
    /// Shared confirmed-update floor, not a replay checkpoint.
    watermark: Arc<AtomicU64>,
    /// Ordered account and lifecycle event delivery.
    events: mpsc::Sender<Event>,
    /// Same-slot application and record matching.
    delegations: Delegations,
}

impl Session {
    /// Allocates the retained-account filter for one provider session.
    pub(super) fn new(
        config: Config,
        watermark: Arc<AtomicU64>,
        events: mpsc::Sender<Event>,
    ) -> Result<Self, Error> {
        Ok(Self {
            accounts: CompressedAccountFilterSet::with_capacity(u16::MAX as usize * 4)?,
            delegations: Delegations::new(config.authority),
            config,
            watermark,
            events,
        })
    }

    /// Reports a terminal stream failure after earlier queued events.
    pub(super) async fn run(mut self, mut updates: mpsc::Receiver<SubscriptionUpdate>) {
        if let Err(error) = self.subscribe(&mut updates).await {
            let _ = self.events.send(Event::Disconnected(error)).await;
        }
    }

    /// Lets Yellowstone reconnect while processing membership changes and updates.
    async fn subscribe(
        &mut self,
        updates: &mut mpsc::Receiver<SubscriptionUpdate>,
    ) -> Result<(), Error> {
        let mut builder = GeyserGrpcClient::build_from_shared(self.config.endpoint.to_string())?
            .x_token(self.config.token.clone())?
            .connect_timeout(TIMEOUT)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .set_reconnect_config(ReconnectConfig::default());
        if self.config.endpoint.scheme() == "https" {
            builder = builder.tls_config(ClientTlsConfig::new().with_native_roots())?;
        }
        let mut client = timeout(TIMEOUT, builder.connect())
            .await
            .map_err(|_| Error::Timeout("connection"))??;
        let request = self.request();
        let subscription = client.subscribe_with_request(Some(request));
        let (mut sink, mut stream) = timeout(TIMEOUT, subscription)
            .await
            .map_err(|_| Error::Timeout("subscribe"))??;
        loop {
            tokio::select! {
                update = updates.recv() => {
                    let Some(update) = update else { return Ok(()) };
                    self.update_subscription(update, &mut sink).await?;
                }
                update = timeout(TIMEOUT, stream.next()) => {
                    let update = update.map_err(|_| Error::Timeout("stream"))?;
                    let update = update.ok_or(Error::Closed)??;
                    self.process(update, &mut sink).await?;
                }
            }
        }
    }

    /// Routes provider updates without changing transport recovery ownership.
    async fn process(
        &mut self,
        update: SubscribeUpdate,
        sink: &mut SubscribeRequestSink,
    ) -> Result<(), Error> {
        match update.update_oneof {
            Some(UpdateOneof::Ping(_)) => self.ping(sink).await?,
            Some(UpdateOneof::Account(account)) => self.account(account).await?,
            Some(UpdateOneof::Transaction(transaction)) => self.transaction(transaction).await?,
            _ => {}
        }
        Ok(())
    }

    /// Sends the complete current membership filter for this stream and reconnects.
    async fn refresh(&mut self, sink: &mut SubscribeRequestSink) -> Result<(), Error> {
        timeout(TIMEOUT, sink.send(self.request()))
            .await
            .map_err(|_| Error::Timeout("subscription update"))??;
        Ok(())
    }

    /// Answers a heartbeat, then restores the full subscription request.
    async fn ping(&mut self, sink: &mut SubscribeRequestSink) -> Result<(), Error> {
        let request = SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: PING_ID }),
            ..Default::default()
        };
        timeout(TIMEOUT, sink.send(request))
            .await
            .map_err(|_| Error::Timeout("ping"))??;
        self.refresh(sink).await
    }

    /// Emits refetch requests for successful ownership returns.
    async fn transaction(&mut self, update: SubscribeUpdateTransaction) -> Result<(), Error> {
        self.delegations.set_slot(update.slot);
        let transaction = update.transaction.ok_or(Error::Protocol("missing transaction"))?;
        let pubkeys = transaction::released(&transaction)?;
        if !pubkeys.is_empty() {
            let event = Event::Refetch {
                pubkeys,
                min_context_slot: update.slot,
            };
            self.send(event).await?;
        }
        Ok(())
    }

    /// Applies a membership batch before acknowledging request delivery.
    async fn update_subscription(
        &mut self,
        update: SubscriptionUpdate,
        sink: &mut SubscribeRequestSink,
    ) -> Result<(), Error> {
        let remove: AHashSet<_> = update.remove.into_iter().collect();
        for key in &remove {
            self.accounts.remove(*key);
        }
        for key in update.add {
            if remove.contains(&key) {
                continue;
            }
            self.accounts.insert(key)?;
        }
        self.refresh(sink).await?;
        let _ = update.reply.send(());
        Ok(())
    }

    /// Builds filters for retained accounts, delegation discovery, and returns.
    fn request(&mut self) -> SubscribeRequest {
        let mut request = SubscribeRequest {
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        };
        if !self.accounts.is_empty() {
            self.accounts.insert_into_subscribe_request(&mut request, RETAINED_FILTER);
        }
        let owner = vec![dlp_api::id().to_string()];
        let candidates = SubscribeRequestFilterAccounts {
            owner: owner.clone(),
            nonempty_txn_signature: Some(true),
            ..Default::default()
        };
        request.accounts.insert(CANDIDATES_FILTER.into(), candidates);

        let authority_offset =
            AccountDiscriminator::SPACE + offset_of!(DelegationRecord, authority);
        let discriminator = DelegationRecord::discriminator().to_bytes().to_vec();
        let authority = self.config.authority.to_bytes().to_vec();
        let filters = vec![memcmp(0, discriminator), memcmp(authority_offset as u64, authority)];
        let records = SubscribeRequestFilterAccounts {
            owner,
            nonempty_txn_signature: Some(true),
            filters,
            ..Default::default()
        };
        request.accounts.insert(RECORDS_FILTER.into(), records);

        let releases = SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: vec![dlp_api::id().to_string()],
            ..Default::default()
        };
        request.transactions.insert(RELEASES_FILTER.into(), releases);
        request
    }

    /// Routes retained updates and discovers same-slot delegation pairs.
    async fn account(&mut self, update: SubscribeUpdateAccount) -> Result<(), Error> {
        let slot = update.slot;
        self.delegations.set_slot(slot);
        let mut account = update.account.ok_or(Error::Protocol("missing account image"))?;
        let key = super::pubkey(&account.pubkey)?;
        let candidate = account.owner == dlp_api::id().as_ref();
        if self.accounts.contains(key) {
            let owner = super::pubkey(&account.owner)?;
            let data = if candidate { account.data.clone() } else { mem::take(&mut account.data) };
            let image = AccountBuilder::default()
                .owner(owner)
                .lamports(account.lamports)
                .executable(account.executable)
                .slot(slot)
                .data(data)
                .build();
            self.watermark.fetch_max(slot, Relaxed);
            let event = Event::Update {
                pubkey: key,
                slot,
                account: image,
            };
            self.send(event).await?;
        }
        if !candidate {
            return Ok(());
        }
        if let Ok(record) = DelegationRecord::try_from_bytes_with_discriminator(&account.data) {
            if let Some(delegation) = self.delegations.record(key, &account, record)? {
                self.delegated(delegation).await?;
            }
        }
        // Application data can resemble a record. Only the canonical PDA establishes
        // its role, so the same update may participate in both interpretations.
        if let Some(delegation) = self.delegations.account(key, account) {
            self.delegated(delegation).await?;
        }
        Ok(())
    }

    /// Raises the shared watermark for a resolved delegation before delivery.
    async fn delegated(&self, delegation: Delegation) -> Result<(), Error> {
        self.watermark.fetch_max(delegation.account.slot(), Relaxed);
        self.send(Event::Delegated(delegation)).await
    }

    /// Bounds consumer backpressure so a stalled receiver terminates the stream.
    async fn send(&self, event: Event) -> Result<(), Error> {
        timeout(TIMEOUT, self.events.send(event))
            .await
            .map_err(|_| Error::Timeout("event delivery"))?
            .map_err(|_| Error::Closed)
    }
}

/// Label for exact retained-account membership.
const RETAINED_FILTER: &str = "retained";
/// Label for DLP-owned application candidates.
const CANDIDATES_FILTER: &str = "candidates";
/// Label for canonical delegation-record candidates.
const RECORDS_FILTER: &str = "records";
/// Label for successful ownership-return transactions.
const RELEASES_FILTER: &str = "releases";
/// Opaque heartbeat identity echoed to Yellowstone.
const PING_ID: i32 = 1;
/// Maximum decoded provider message size.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Builds a byte-level field comparison for an account filter.
fn memcmp(offset: u64, bytes: Vec<u8>) -> SubscribeRequestFilterAccountsFilter {
    let memcmp = SubscribeRequestFilterAccountsFilterMemcmp {
        offset,
        data: Some(Data::Bytes(bytes)),
    };
    SubscribeRequestFilterAccountsFilter {
        filter: Some(Filter::Memcmp(memcmp)),
    }
}

/// Budget for network and event-consumer progress.
const TIMEOUT: Duration = Duration::from_secs(30);
