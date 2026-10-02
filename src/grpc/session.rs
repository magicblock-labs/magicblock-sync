use std::{
    mem::{self, offset_of},
    time::Duration,
};

use ahash::AHashMap;
use dlp_api::state::{
    discriminator::{AccountDiscriminator, AccountWithDiscriminator},
    DelegationRecord,
};
use engine::Engine;
use futures::{SinkExt, StreamExt};
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;
use tokio::{
    sync::mpsc::{self, UnboundedReceiver},
    time::{self, Instant},
};
use tracing::error;
use yellowstone_grpc_client::{
    ClientTlsConfig, GeyserGrpcClient, GeyserGrpcClientError, ReconnectConfig, SubscribeRequestSink,
};
use yellowstone_grpc_proto::{
    cuckoo::CompressedAccountFilterSet,
    geyser::{
        subscribe_request_filter_accounts_filter::Filter,
        subscribe_request_filter_accounts_filter_memcmp::Data, subscribe_update::UpdateOneof,
    },
    prelude::*,
    tonic,
};

use super::{
    client::Command, delegation::Delegations, transaction, Delegation, Error, Event, Result,
    StreamConfig,
};
use crate::metrics::{self, Transport};
use crate::{delegation, AccountSubscription, DUPLICATION_DELAY};

/// Desired account interest and delegation state for one provider stream.
pub(super) struct Session {
    /// Stable index in the configured provider list.
    id: usize,
    config: StreamConfig,
    authority: Pubkey,
    /// Physical account filter; a rebuild sends changes before confirming coverage.
    retained_filter: CompressedAccountFilterSet,
    /// Logical interest, including WS copies still within the duplication delay.
    desired: AHashMap<Pubkey, TrackedAccount>,
    /// Engine owns the confirmed-observation watermark.
    engine: Engine,
    events: mpsc::Sender<Event>,
    /// Same-slot application and record matching.
    delegations: Delegations,
}

/// One WS-confirmed account awaiting or retaining a gRPC filter entry.
struct TrackedAccount {
    /// Exact key and optional ProgramData target.
    sub: AccountSubscription,
    /// Generation assigned by the coverage registry to reject stale confirmations.
    gen: u64,
    /// Time the track command arrived, which starts the duplication delay.
    tracked_at: Instant,
}

impl Session {
    /// Keeps the exact retained-account filter across Yellowstone reconnects.
    pub(super) fn new(
        id: usize,
        config: StreamConfig,
        authority: Pubkey,
        engine: Engine,
        events: mpsc::Sender<Event>,
    ) -> Result<Self> {
        Ok(Self {
            id,
            retained_filter: CompressedAccountFilterSet::with_capacity(u16::MAX as usize * 8)?,
            desired: AHashMap::new(),
            delegations: Delegations::new(authority),
            config,
            authority,
            engine,
            events,
        })
    }

    /// Reports lost coverage before retrying recoverable stream failures.
    /// Yellowstone's reconnect backoff begins only after a subscription succeeds;
    /// initial connection and subscription failures need an outer retry delay.
    pub(super) async fn run(mut self, mut commands: UnboundedReceiver<Command>) -> Result<()> {
        loop {
            let Err(error) = self.subscribe(&mut commands).await else { return Ok(()) };
            let retryable = recoverable(&error);
            metrics::transport(self.id, Transport::Grpc);
            error!(stream = self.id, retryable, %error, "gRPC session failed");
            let _ = self.events.send(Event::Lost(self.id)).await;
            if retryable {
                #[allow(clippy::disallowed_methods)]
                time::sleep(RETRY_DELAY).await;
            } else {
                return Err(error);
            }
        }
    }

    /// Lets Yellowstone reconnect while processing filter changes and updates.
    async fn subscribe(&mut self, commands: &mut UnboundedReceiver<Command>) -> Result<()> {
        let mut builder = GeyserGrpcClient::build_from_shared(self.config.endpoint.to_string())?
            .x_token(self.config.token.clone())?
            .connect_timeout(TIMEOUT)
            .timeout(TIMEOUT)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .set_reconnect_config(ReconnectConfig::default());
        if self.config.endpoint.scheme() == "https" {
            builder = builder.tls_config(ClientTlsConfig::new().with_native_roots())?;
        }
        let mut client = builder.connect().await?;
        self.sync_filter()?;
        let mut request = self.request();
        // Seed only new sessions; filter refreshes leave replay to Yellowstone.
        request.from_slot = Some(self.engine.accounts().chain_slot().saturating_sub(2));
        let (mut sink, mut stream) = client.subscribe_with_request(Some(request)).await?;
        // Reconnect keeps local filter membership; confirm only keys included in this request.
        for (&pubkey, entry) in &self.desired {
            if self.retained_filter.contains(pubkey) {
                self.confirm([(pubkey, entry.gen)]).await;
            }
        }
        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()) };
                    self.command(command, &mut sink).await?;
                }
                update = stream.next() => {
                    let update = update.ok_or(Error::Closed)?;
                    self.process(update?, &mut sink).await?;
                }
            }
        }
    }

    /// Handles ping, account, and transaction updates; other provider messages
    /// do not change retained-account interest or delegation state.
    async fn process(
        &mut self,
        update: SubscribeUpdate,
        sink: &mut SubscribeRequestSink,
    ) -> Result<()> {
        match update.update_oneof {
            Some(UpdateOneof::Ping(_)) => self.ping(sink).await?,
            Some(UpdateOneof::Account(account)) => self.account(account).await?,
            Some(UpdateOneof::Transaction(transaction)) => self.transaction(transaction).await?,
            _ => {}
        }
        Ok(())
    }

    /// Sends the complete current physical filter on the live stream.
    async fn refresh(&mut self, sink: &mut SubscribeRequestSink) -> Result<()> {
        sink.send(self.request()).await.map_err(Into::into)
    }

    /// Answers a heartbeat, then restores the full subscription request.
    async fn ping(&mut self, sink: &mut SubscribeRequestSink) -> Result<()> {
        let request = SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: PING_ID }),
            ..Default::default()
        };
        sink.send(request).await?;
        self.refresh(sink).await
    }

    /// Reports accounts undelegated by successful transactions.
    async fn transaction(&mut self, update: SubscribeUpdateTransaction) -> Result<()> {
        self.delegations.set_slot(update.slot);
        let transaction = update.transaction.ok_or(Error::Protocol("missing transaction"))?;
        let pubkeys = transaction::undelegated_accounts(&transaction)?;
        if !pubkeys.is_empty() {
            let event = Event::Undelegated { pubkeys, slot: update.slot };
            self.send(event).await;
        }
        Ok(())
    }

    /// Applies logical interest immediately and sends its full filter only when it changes.
    async fn command(&mut self, command: Command, sink: &mut SubscribeRequestSink) -> Result<()> {
        match command {
            Command::Track { sub, gen } => {
                let entry = TrackedAccount {
                    sub,
                    gen,
                    tracked_at: Instant::now(),
                };
                self.desired.insert(sub.pubkey, entry);
                if self.retained_filter.contains(sub.pubkey) {
                    self.confirm([(sub.pubkey, gen)]).await;
                }
            }
            Command::Remove(pubkey) => {
                self.desired.remove(&pubkey);
            }
            Command::Rebuild => {
                let added = self.sync_filter()?;
                if let Some(added) = added {
                    // Publish the new filter before claiming its added keys as covered.
                    self.refresh(sink).await?;
                    self.confirm(added).await;
                }
            }
        }
        Ok(())
    }

    /// Prunes removed keys and adds aged keys without reallocating an unchanged filter.
    fn sync_filter(&mut self) -> Result<Option<Vec<(Pubkey, u64)>>> {
        let remove: Vec<_> = self
            .retained_filter
            .iter()
            .map(|bytes| Pubkey::new_from_array(*bytes))
            .filter(|pubkey| !self.desired.contains_key(pubkey))
            .collect();
        for pubkey in &remove {
            self.retained_filter.remove(*pubkey);
        }
        let mut added = Vec::new();
        for (&pubkey, entry) in &self.desired {
            if !self.retained_filter.contains(pubkey)
                && entry.tracked_at.elapsed() >= DUPLICATION_DELAY
            {
                self.retained_filter.insert(pubkey)?;
                added.push((pubkey, entry.gen));
            }
        }
        Ok((!remove.is_empty() || !added.is_empty()).then_some(added))
    }

    /// Reports account filter delivery to the ChainSync worker.
    async fn confirm(&self, confirmations: impl IntoIterator<Item = (Pubkey, u64)>) {
        for (pubkey, gen) in confirmations {
            self.send(Event::Confirmed { stream: self.id, pubkey, gen }).await;
        }
    }

    /// Sends the exact retained-account filter alongside DLP delegation discovery and
    /// successful ownership-return transaction filters, including on reconnect.
    fn request(&mut self) -> SubscribeRequest {
        let mut request = SubscribeRequest {
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        };
        if !self.retained_filter.is_empty() {
            self.retained_filter
                .insert_into_subscribe_request(&mut request, RETAINED_FILTER);
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
        for (label, authority) in
            [(RECORDS_FILTER, self.authority), (CONFINED_FILTER, Pubkey::default())]
        {
            let filters = vec![
                memcmp(0, discriminator.clone()),
                memcmp(authority_offset as u64, authority.to_bytes().to_vec()),
            ];
            let records = SubscribeRequestFilterAccounts {
                owner: owner.clone(),
                nonempty_txn_signature: Some(true),
                filters,
                ..Default::default()
            };
            request.accounts.insert(label.into(), records);
        }

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
    async fn account(&mut self, update: SubscribeUpdateAccount) -> Result<()> {
        let slot = update.slot;
        self.delegations.set_slot(slot);
        let mut account = update.account.ok_or(Error::Protocol("missing account image"))?;
        let key = super::pubkey(&account.pubkey)?;
        let candidate = account.owner == dlp_api::id().as_ref();
        if let Some(desired) = self.desired.get(&key).filter(|_| self.retained_filter.contains(key))
        {
            let owner = super::pubkey(&account.owner)?;
            // Delegation matching still needs the DLP bytes after the retained update is sent.
            let data = if candidate { account.data.clone() } else { mem::take(&mut account.data) };
            let image = AccountBuilder::default()
                .owner(owner)
                .lamports(account.lamports)
                .executable(account.executable)
                .slot(slot)
                .data(data);
            self.engine.accounts().advance_chain_slot(slot);
            let event = Event::Update {
                stream: self.id,
                sub: desired.sub,
                account: image,
            };
            self.send(event).await;
        }
        if !candidate {
            return Ok(());
        }
        if let Some(record) = delegation::record(&account.data) {
            if let Some(delegation) = self.delegations.record(key, &account, record) {
                self.delegated(delegation).await;
            }
        }
        // Application data can resemble a record. Only a matching record PDA
        // establishes its role, so the update may be considered both ways.
        if let Some(delegation) = self.delegations.account(key, account) {
            self.delegated(delegation).await;
        }
        Ok(())
    }

    /// Raises the shared watermark for a resolved delegation before delivery.
    async fn delegated(&self, delegation: Delegation) {
        self.engine.accounts().advance_chain_slot(delegation.account.read().slot());
        self.send(Event::Delegated(delegation)).await;
    }

    async fn send(&self, event: Event) {
        let _ = self.events.send(event).await;
    }
}

const RETAINED_FILTER: &str = "retained";
const CANDIDATES_FILTER: &str = "candidates";
const RECORDS_FILTER: &str = "records";
const CONFINED_FILTER: &str = "confined";
const RELEASES_FILTER: &str = "releases";
const PING_ID: i32 = 1;
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

/// Yellowstone transport request and connection budget.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Delay before reconnecting after a recoverable transport failure.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Transport interruptions are retried; invalid configuration and stream data are terminal.
fn recoverable(error: &Error) -> bool {
    match error {
        Error::Client(GeyserGrpcClientError::TonicStatus(status)) | Error::Status(status) => {
            !matches!(
                status.code(),
                tonic::Code::Unauthenticated
                    | tonic::Code::PermissionDenied
                    | tonic::Code::InvalidArgument
            )
        }
        Error::Client(GeyserGrpcClientError::TransportError(_))
        | Error::Send(_)
        | Error::Closed => true,
        _ => false,
    }
}
