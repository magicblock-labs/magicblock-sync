use std::io;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use solana_account::{AccountBuilder, OwnedAccount};
use solana_pubkey::Pubkey;

/// Encoding accepted by the shared account decoder.
pub(super) const ENCODING: &str = "base64+zstd";

/// Borrowed RPC account before owner and payload validation.
#[derive(Deserialize)]
pub(crate) struct WireAccount<'a> {
    /// Compressed bytes and their declared encoding.
    #[serde(borrow)]
    data: Data<'a>,
    /// Base58 owner pending public-key validation.
    owner: &'a str,
    /// Balance at the response context slot.
    lamports: u64,
    /// Executable flag from the provider.
    executable: bool,
}

/// Encoded payload and its encoding label.
#[derive(Deserialize)]
struct Data<'a>(
    /// Base64-encoded compressed account bytes.
    &'a str,
    /// Encoding declared by the provider.
    &'a str,
);

impl WireAccount<'_> {
    /// Decodes an account in `Uninit` mode for caller classification.
    pub(crate) fn decode(self, slot: u64) -> Result<OwnedAccount, DecodeError> {
        if self.data.1 != ENCODING {
            return Err(DecodeError::Protocol("unsupported account encoding"));
        }
        let owner: Pubkey = self.owner.parse()?;
        let data = STANDARD.decode(self.data.0.as_bytes())?;
        let data = zstd::stream::decode_all(data.as_slice()).map_err(DecodeError::Zstd)?;
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
    #[error("invalid account base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("invalid account owner: {0}")]
    Owner(#[from] solana_pubkey::ParsePubkeyError),
    #[error("invalid account zstd: {0}")]
    Zstd(#[source] io::Error),
    #[error("invalid provider message: {0}")]
    Protocol(&'static str),
}
