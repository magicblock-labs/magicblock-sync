use std::{
    mem::{self, offset_of},
    time::Duration,
};

use ahash::AHashMap;
use dlp_api::{
    pda::undelegation_request_pda_from_delegated_account,
    state::{
        discriminator::{AccountDiscriminator, AccountWithDiscriminator},
        DelegationRecord, UndelegationRequest,
    },
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

/// Account subscriptions and delegation matching for one Yellowstone provider stream.
pub(super) struct Session {
    /// Stable index in the configured provider list.
    id: usize,
    config: StreamConfig,
    authority: Pubkey,
    /// Account filter sent to Yellowstone; may retain removed keys until the next rebuild.
    retained_filter: CompressedAccountFilterSet,
    /// Accounts to track, including WebSocket subscriptions still waiting for their gRPC copy.
    desired: AHashMap<Pubkey, TrackedAccount>,
    /// Stores the confirmed chain slot used to seed replay and constrain HTTP snapshots.
    engine: Engine,
    events: mpsc::Sender<Event>,
    /// Matches DLP-owned account updates to delegation records observed in the same slot.
    delegations: Delegations,
}

/// WebSocket-acknowledged subscription waiting for or already included in the gRPC filter.
struct TrackedAccount {
    /// Remote account address and the local account its updates belong to.
    sub: AccountSubscription,
    /// Generation assigned by the coverage registry to reject stale confirmations.
    gen: u64,
    /// Time the track command arrived, which starts the duplication delay.
    tracked_at: Instant,
}

impl Session {
    /// Creates a session whose account filter survives Yellowstone reconnects.
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
            metrics::transport(Transport::Grpc);
            let provider = self.config.endpoint.host_str();
            error!(provider, retryable, %error, "gRPC session failed");
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
        // Set the replay start only when opening a stream; Yellowstone handles replay
        // on its internal reconnects, and filter refreshes must not restart it.
        request.from_slot = Some(self.engine.accounts().chain_slot().saturating_sub(2));
        let (mut sink, mut stream) = client.subscribe_with_request(Some(request)).await?;
        // Report only tracked accounts included in the initial filter, not those still waiting.
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
            Some(UpdateOneof::Ping(_)) => self.ping(sink).await,
            Some(UpdateOneof::Account(account)) => self.account(account).await,
            Some(UpdateOneof::Transaction(transaction)) => self.transaction(transaction).await,
            _ => Ok(()),
        }
    }

    /// Sends the full subscription request, including account and delegation filters.
    async fn refresh(&mut self, sink: &mut SubscribeRequestSink) -> Result<()> {
        sink.send(self.request()).await.map_err(Into::into)
    }

    /// Answers a server ping, then resends the full subscription request.
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

    /// Updates tracked accounts; only a rebuild sends a changed filter to Yellowstone.
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
                    // Notify the worker only after sending the filter; Yellowstone
                    // does not acknowledge individual account subscriptions.
                    self.refresh(sink).await?;
                    self.confirm(added).await;
                }
            }
        }
        Ok(())
    }

    /// Removes untracked keys and adds accounts whose duplication delay has elapsed.
    /// Returns newly added keys if the filter changed, or `None` if it did not.
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

    /// Reports accounts included in a sent filter; this is not a server acknowledgement.
    async fn confirm(&self, confirmations: impl IntoIterator<Item = (Pubkey, u64)>) {
        for (pubkey, gen) in confirmations {
            self.send(Event::Confirmed { stream: self.id, pubkey, gen }).await;
        }
    }

    /// Builds a request for tracked accounts, DLP-owned accounts, accepted delegation records,
    /// and successful DLP transactions used to detect undelegation.
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

    /// Forwards tracked updates, discovers requests, and matches same-slot delegation pairs.
    async fn account(&mut self, update: SubscribeUpdateAccount) -> Result<()> {
        let slot = update.slot;
        self.delegations.set_slot(slot);
        let mut account = update.account.ok_or(Error::Protocol("missing account image"))?;
        let key = super::pubkey(&account.pubkey)?;
        let candidate = account.owner == dlp_api::id().as_ref();
        if let Some(desired) = self.desired.get(&key).filter(|_| self.retained_filter.contains(key))
        {
            let owner = super::pubkey(&account.owner)?;
            // Keep DLP-owned data for delegation matching after forwarding the account update.
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
        if !candidate || account.lamports == 0 {
            return Ok(());
        }
        let request = UndelegationRequest::try_from_bytes_with_discriminator(&account.data)
            .ok()
            .map(|request| request.delegated_account)
            .filter(|pubkey| key == undelegation_request_pda_from_delegated_account(pubkey));
        if let Some(pubkey) = request {
            self.engine.accounts().advance_chain_slot(slot);
            self.send(Event::UndelegationRequested { pubkey, slot }).await;
        }
        let delegation = delegation::record(&account.data)
            .and_then(|record| self.delegations.record(key, &account, record));
        if let Some(delegation) = delegation {
            self.delegated(delegation).await;
        }
        // Account data may parse as a record by coincidence. Consider both roles;
        // a match is valid only at the application's derived delegation-record address.
        if let Some(delegation) = self.delegations.account(key, account) {
            self.delegated(delegation).await;
        }
        Ok(())
    }

    /// Advances Engine's confirmed chain slot before reporting the resolved delegation.
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
