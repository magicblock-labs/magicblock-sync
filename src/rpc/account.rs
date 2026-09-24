use std::io;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use solana_account::{AccountBuilder, OwnedAccount};
use solana_pubkey::Pubkey;

/// Compressed representation supported by the shared account decoder.
pub(super) const ENCODING: &str = "base64+zstd";

/// Borrowed account representation shared by HTTP snapshots and WebSocket updates.
#[derive(Deserialize)]
pub(crate) struct WireAccount<'a> {
    /// Encoded bytes and their declared representation, validated before decoding.
    #[serde(borrow)]
    data: Data<'a>,
    /// Base58 program owner; parsed before constructing an Engine account.
    owner: &'a str,
    /// Balance observed at the response context slot.
    lamports: u64,
    /// Whether the account contains executable program code.
    executable: bool,
}

/// Borrowed account data and its declared encoding.
#[derive(Deserialize)]
struct Data<'a>(
    /// Base64-encoded compressed account bytes.
    &'a str,
    /// Must match the encoding requested by both transports.
    &'a str,
);

impl WireAccount<'_> {
    /// Validates and decodes the account in `Uninit` mode for caller classification.
    pub(crate) fn decode(self, slot: u64) -> Result<OwnedAccount, DecodeError> {
        if self.data.1 != ENCODING {
            return Err(DecodeError::Protocol("unsupported account encoding"));
        }
        let owner: Pubkey = self.owner.parse()?;
        let data = STANDARD.decode(self.data.0.as_bytes())?;
        let data = zstd::stream::decode_all(data.as_slice()).map_err(DecodeError::Zstd)?;
        // Classification belongs to the caller; the builder retains Uninit mode.
        let account = AccountBuilder::default()
            .owner(owner)
            .lamports(self.lamports)
            .executable(self.executable)
            .slot(slot)
            .data(data);
        Ok(account.build())
    }
}

/// Invalid encoded account data shared by HTTP and WebSocket responses.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The declared account payload is not valid base64.
    #[error("invalid account base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The account owner is not a valid public key.
    #[error("invalid account owner: {0}")]
    Owner(#[from] solana_pubkey::ParsePubkeyError),
    /// The decoded bytes do not form a valid zstd payload.
    #[error("invalid account zstd: {0}")]
    Zstd(#[source] io::Error),
    /// The provider returned an unsupported account representation.
    #[error("invalid provider message: {0}")]
    Protocol(&'static str),
}
