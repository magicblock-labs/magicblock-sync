use crate::types::Pubkey;

/// Size of a Solana public key in bytes.
pub const PUBKEY_LEN: usize = 32;

/// Delegation program address.
pub const DELEGATION_PROGRAM: &str = "DELeGGvXpWV2fqJUhqcF5ZSYMS4JTLjteaAMARRSaeSh";

/// Delegation program pubkey in bytes.
pub const DELEGATION_PROGRAM_PUBKEY: &Pubkey = &[
    181, 183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184, 53, 6, 235, 140, 229,
    25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
];

/// Size of a delegation record account in bytes.
pub const DELEGATION_RECORD_SIZE: u64 = 96;

/// Instruction discriminator for undelegate operations.
pub const UNDELEGATE_DISCRIMINATOR: u8 = 3;

/// Length of an instruction discriminator (Anchor programs).
pub const DISCRIMINATOR_LEN: usize = 8;

/// Index of the delegation record account in undelegate instruction accounts.
pub const DELEGATION_RECORD_ACCOUNT_INDEX: usize = 6;

/// Maximum pending subscription/unsubscription requests.
pub const MAX_PENDING_REQUESTS: usize = 256;

/// Maximum pending account/transaction updates.
pub const MAX_PENDING_UPDATES: usize = 8192;

/// Maximum reconnection attempts to the Laserstream.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 16;
