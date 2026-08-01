use helius_laserstream::LaserstreamError;

/// Pubkey type alias for Solana public keys (32 bytes).
pub type Pubkey = [u8; 32];

/// Solana slot number.
pub type Slot = u64;

/// Errors that can occur during DLP synchronization.
#[derive(Debug)]
pub enum DlpSyncError {
    /// Connection-related error.
    Connection(&'static str),
    /// Laserstream error.
    LaserStream(LaserstreamError),
}

/// Account updates from the Laserstream.
#[derive(Debug)]
pub enum AccountUpdate {
    /// A delegation record was updated.
    Delegated {
        /// The delegation record pubkey.
        record: Pubkey,
        /// The account data.
        data: Vec<u8>,
        /// The slot at which the update occurred.
        slot: Slot,
    },
    /// A delegation record was undelegated.
    Undelegated {
        /// The delegation record pubkey.
        record: Pubkey,
        /// The slot at which the undelegation occurred.
        slot: Slot,
    },
    /// A delegate instruction was observed on chain (firehose mode only),
    /// parsed from the transaction stream including CPI-invoked delegations.
    /// Unlike [`AccountUpdate::Delegated`] — whose record PDA cannot be
    /// reversed — this names the delegated account, so consumers can
    /// discover new delegations without an account-side firehose.
    DelegationObserved {
        /// The account being delegated.
        delegated_account: Pubkey,
        /// The delegation record PDA written by the same instruction.
        record: Pubkey,
        /// The slot the delegation landed in.
        slot: Slot,
    },
    /// An `UndelegationRequest` account was written (firehose mode only):
    /// an owner program asked for the account to be undelegated, which the
    /// delegating validator should honor promptly.
    UndelegationRequested {
        /// The `UndelegationRequest` account that was written.
        request_pda: Pubkey,
        /// The delegated account the request is for.
        delegated_account: Pubkey,
        /// The first slot at which timeout rollback is allowed.
        expires_at_slot: Slot,
        /// The slot the request account was written in.
        slot: Slot,
    },
    /// The confirmed slot advanced. Only emitted in firehose mode, in-band
    /// with record updates: receiving `SlotAdvanced(s)` proves every record
    /// update up to slot `s` has already been delivered on this channel.
    SlotAdvanced(Slot),
    /// The stream was re-established without replay: updates in between were
    /// lost, so cached delegation state derived from earlier updates must be
    /// revalidated at the source.
    SyncInterrupted,
    /// The sync service has terminated.
    SyncTerminated,
}
