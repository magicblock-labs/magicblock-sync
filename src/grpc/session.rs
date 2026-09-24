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

/// One task owns membership and pending delegations; Yellowstone owns transport recovery.
pub(super) struct Session {
    /// Provider endpoint, validator identity, and filter capacity.
    config: Config,
    /// One authoritative exact-membership set, also used to build the wire filter.
    accounts: CompressedAccountFilterSet,
    /// Shared with HTTP and WebSockets; not a recovery checkpoint.
    watermark: Arc<AtomicU64>,
    /// Ordered delivery; closing the receiver stops the task.
    events: mpsc::Sender<Event>,
    /// Account/record observations awaiting their counterpart.
    delegations: Delegations,
}

impl Session {
    /// Allocates the compressed filter; endpoint validation belongs to the transport.
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

    /// Keeps terminal failure behind previously queued events, including after delivery timeout.
    pub(super) async fn run(mut self, mut updates: mpsc::Receiver<SubscriptionUpdate>) {
        if let Err(error) = self.subscribe(&mut updates).await {
            let _ = self.events.send(Event::Disconnected(error)).await;
        }
    }

    /// Uses upstream reconnect/replay as-is; no local retries, checkpoints, or deduplication.
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

    /// Dispatches provider messages without mixing protocol handling into the I/O loop.
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

    /// Sends the complete membership snapshot used both now and after reconnect.
    async fn refresh(&mut self, sink: &mut SubscribeRequestSink) -> Result<(), Error> {
        timeout(TIMEOUT, sink.send(self.request()))
            .await
            .map_err(|_| Error::Timeout("subscription update"))??;
        Ok(())
    }

    /// Answers the heartbeat without leaving a ping-only reconnect request cached upstream.
    async fn ping(&mut self, sink: &mut SubscribeRequestSink) -> Result<(), Error> {
        let request = SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: PING_ID }),
            ..Default::default()
        };
        timeout(TIMEOUT, sink.send(request))
            .await
            .map_err(|_| Error::Timeout("ping"))??;
        // Restore the full request before polling the reconnecting stream again.
        self.refresh(sink).await
    }

    /// Refetching and local lifecycle transitions remain orchestration responsibilities.
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

    /// Applies and sends a batch before acknowledging it. Failure terminates the session.
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

    /// Records use owner/discriminator/authority filters, allowing appended actions.
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

    /// Routes exact members and eligible DLP observations before decoding their payloads.
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

    /// Resolved accounts use the same freshness watermark as raw subscription updates.
    async fn delegated(&self, delegation: Delegation) -> Result<(), Error> {
        self.watermark.fetch_max(delegation.account.slot(), Relaxed);
        self.send(Event::Delegated(delegation)).await
    }

    /// Slow consumers apply backpressure; timeout is followed by a terminal failure event.
    async fn send(&self, event: Event) -> Result<(), Error> {
        timeout(TIMEOUT, self.events.send(event))
            .await
            .map_err(|_| Error::Timeout("event delivery"))?
            .map_err(|_| Error::Closed)
    }
}

/// Outgoing filter labels; incoming updates are routed by payload and account identity.
const RETAINED_FILTER: &str = "retained";
/// DLP-owned application candidates.
const CANDIDATES_FILTER: &str = "candidates";
/// This validator's delegation records, including appended actions.
const RECORDS_FILTER: &str = "records";
/// Successful ownership-return transactions.
const RELEASES_FILTER: &str = "releases";
/// Endpoint prefix requiring TLS.
/// Opaque heartbeat identifier echoed by the server.
const PING_ID: i32 = 1;
/// Maximum decoded provider message size in bytes.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Binary memcmp at a field offset supplied by the DLP record layout.
fn memcmp(offset: u64, bytes: Vec<u8>) -> SubscribeRequestFilterAccountsFilter {
    let memcmp = SubscribeRequestFilterAccountsFilterMemcmp {
        offset,
        data: Some(Data::Bytes(bytes)),
    };
    SubscribeRequestFilterAccountsFilter {
        filter: Some(Filter::Memcmp(memcmp)),
    }
}

/// Bounded wait for network and consumer progress.
const TIMEOUT: Duration = Duration::from_secs(30);
