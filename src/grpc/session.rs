use std::{
    mem::{self, offset_of},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use ahash::AHashMap;
use dlp_api::state::{
    discriminator::{AccountDiscriminator, AccountWithDiscriminator},
    DelegationRecord,
};
use futures::{SinkExt, StreamExt};
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;
use tokio::{
    sync::mpsc::{self, UnboundedReceiver},
    time::Instant,
};
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
    client::Command, delegation::Delegations, transaction, Delegation, Error, Event, StreamConfig,
};
use crate::{AccountSubscription, DUPLICATION_DELAY};

/// Desired account interest and delegation state for one provider stream.
pub(super) struct Session {
    /// Stable index in the configured provider list.
    id: usize,
    /// Endpoint and provider credentials.
    config: StreamConfig,
    /// Authority observed for delegation lifecycle events.
    authority: Pubkey,
    /// Last full filter sent on this live stream.
    accounts: CompressedAccountFilterSet,
    /// Logical interest, including WS copies still within the duplication delay.
    desired: AHashMap<Pubkey, Desired>,
    /// Shared confirmed-update floor, not a replay checkpoint.
    watermark: Arc<AtomicU64>,
    /// Ordered account and lifecycle event delivery.
    events: mpsc::Sender<Event>,
    /// Same-slot application and record matching.
    delegations: Delegations,
}

/// One WS-confirmed account awaiting or retaining a gRPC filter entry.
struct Desired {
    /// Exact key and optional ProgramData target.
    sub: AccountSubscription,
    /// Owner-issued logical subscription generation.
    gen: u64,
    /// Receipt of the track command that starts the duplication delay.
    ts: Instant,
}

impl Session {
    /// Keeps the exact retained-account filter across Yellowstone reconnects.
    pub(super) fn new(
        id: usize,
        config: StreamConfig,
        authority: Pubkey,
        watermark: Arc<AtomicU64>,
        events: mpsc::Sender<Event>,
    ) -> Result<Self, Error> {
        Ok(Self {
            id,
            accounts: CompressedAccountFilterSet::with_capacity(u16::MAX as usize * 4)?,
            desired: AHashMap::new(),
            delegations: Delegations::new(authority),
            config,
            authority,
            watermark,
            events,
        })
    }

    /// Returns a terminal stream failure after reporting lost coverage.
    pub(super) async fn run(
        mut self,
        mut commands: UnboundedReceiver<Command>,
    ) -> Result<(), Error> {
        loop {
            let Err(error) = self.subscribe(&mut commands).await else { return Ok(()) };
            let _ = self.events.send(Event::Lost(self.id)).await;
            if recoverable(&error) {
                tracing::warn!(%error, "gRPC unavailable; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            } else {
                return Err(error);
            }
        }
    }

    /// Lets Yellowstone reconnect while processing filter changes and updates.
    async fn subscribe(&mut self, commands: &mut UnboundedReceiver<Command>) -> Result<(), Error> {
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
        let request = self.request();
        let (mut sink, mut stream) = client.subscribe_with_request(Some(request)).await?;
        let current: Vec<_> = self
            .desired
            .iter()
            .filter_map(|(&pubkey, entry)| {
                self.accounts.contains(pubkey).then_some((pubkey, entry.gen))
            })
            .collect();
        self.confirm(current).await;
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
    ) -> Result<(), Error> {
        match update.update_oneof {
            Some(UpdateOneof::Ping(_)) => self.ping(sink).await?,
            Some(UpdateOneof::Account(account)) => self.account(account).await?,
            Some(UpdateOneof::Transaction(transaction)) => self.transaction(transaction).await?,
            _ => {}
        }
        Ok(())
    }

    /// Sends the complete current physical filter on the live stream.
    async fn refresh(&mut self, sink: &mut SubscribeRequestSink) -> Result<(), Error> {
        sink.send(self.request()).await.map_err(Into::into)
    }

    /// Answers a heartbeat, then restores the full subscription request.
    async fn ping(&mut self, sink: &mut SubscribeRequestSink) -> Result<(), Error> {
        let request = SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: PING_ID }),
            ..Default::default()
        };
        sink.send(request).await?;
        self.refresh(sink).await
    }

    /// Reports accounts undelegated by successful transactions.
    async fn transaction(&mut self, update: SubscribeUpdateTransaction) -> Result<(), Error> {
        self.delegations.set_slot(update.slot);
        let transaction = update.transaction.ok_or(Error::Protocol("missing transaction"))?;
        let pubkeys = transaction::released(&transaction)?;
        if !pubkeys.is_empty() {
            let event = Event::Undelegated { pubkeys, slot: update.slot };
            self.send(event).await;
        }
        Ok(())
    }

    /// Applies logical interest immediately and sends its full filter only when it changes.
    async fn command(
        &mut self,
        command: Command,
        sink: &mut SubscribeRequestSink,
    ) -> Result<(), Error> {
        match command {
            Command::Track { sub, gen } => {
                let entry = Desired { sub, gen, ts: Instant::now() };
                self.desired.insert(sub.pubkey, entry);
                if self.accounts.contains(sub.pubkey) {
                    self.confirm([(sub.pubkey, gen)]).await;
                }
            }
            Command::Remove(pubkey) => {
                self.desired.remove(&pubkey);
            }
            Command::Rebuild => {
                let added = self.sync_filter()?;
                if let Some(added) = added {
                    self.refresh(sink).await?;
                    self.confirm(added).await;
                }
            }
        }
        Ok(())
    }

    /// Prunes removed keys and adds aged keys without reallocating an unchanged filter.
    fn sync_filter(&mut self) -> Result<Option<Vec<(Pubkey, u64)>>, Error> {
        let remove: Vec<_> = self
            .accounts
            .iter()
            .map(|bytes| Pubkey::new_from_array(*bytes))
            .filter(|pubkey| !self.desired.contains_key(pubkey))
            .collect();
        for pubkey in &remove {
            self.accounts.remove(*pubkey);
        }
        let mut added = Vec::new();
        for (&pubkey, entry) in &self.desired {
            if !self.accounts.contains(pubkey) && entry.ts.elapsed() >= DUPLICATION_DELAY {
                self.accounts.insert(pubkey)?;
                added.push((pubkey, entry.gen));
            }
        }
        Ok((!remove.is_empty() || !added.is_empty()).then_some(added))
    }

    /// Reports account filter delivery to the single coverage owner.
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
        let authority = self.authority.to_bytes().to_vec();
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
        if let Some(desired) = self.desired.get(&key).filter(|_| self.accounts.contains(key)) {
            let owner = super::pubkey(&account.owner)?;
            let data = if candidate { account.data.clone() } else { mem::take(&mut account.data) };
            let image = AccountBuilder::default()
                .owner(owner)
                .lamports(account.lamports)
                .executable(account.executable)
                .slot(slot)
                .data(data);
            self.watermark.fetch_max(slot, Relaxed);
            let event = Event::Update {
                stream: self.id,
                pubkey: key,
                target: desired.sub.target,
                account: image,
            };
            self.send(event).await;
        }
        if !candidate {
            return Ok(());
        }
        if let Some(record) = crate::delegation::record(&account.data) {
            if let Some(delegation) = self.delegations.record(key, &account, record)? {
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
        self.watermark.fetch_max(delegation.account.read().slot(), Relaxed);
        self.send(Event::Delegated(delegation)).await;
    }

    /// Sends an event to the coverage owner.
    async fn send(&self, event: Event) {
        let _ = self.events.send(event).await;
    }
}

/// Label for the exact retained-account filter.
const RETAINED_FILTER: &str = "retained";
/// Label for DLP-owned application candidates.
const CANDIDATES_FILTER: &str = "candidates";
/// Label for delegation record candidates.
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
