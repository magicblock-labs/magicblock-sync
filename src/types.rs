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
    /// The sync service has terminated.
    SyncTerminated,
}
