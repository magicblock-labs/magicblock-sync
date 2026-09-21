use std::borrow::Cow;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use solana_account::AccountBuilder;

use crate::{rpc::ENCODING, Error, OwnedAccount, Pubkey};

/// Borrowed account representation shared by HTTP snapshots and WebSocket updates.
#[derive(Deserialize)]
pub(crate) struct WireAccount<'a> {
    /// Encoded bytes and their declared representation, validated before decoding.
    #[serde(borrow)]
    data: Data<'a>,
    /// Base58 program owner; parsed before constructing an Engine account.
    #[serde(borrow)]
    owner: Cow<'a, str>,
    /// Balance observed at the response context slot.
    lamports: u64,
    /// Whether the account contains executable program code.
    executable: bool,
}

/// Borrows ordinary strings while retaining owned fallback for escaped JSON strings.
#[derive(Deserialize)]
struct Data<'a>(
    /// Base64-encoded compressed account bytes.
    #[serde(borrow)]
    Cow<'a, str>,
    /// Must match the encoding requested by both transports.
    #[serde(borrow)]
    Cow<'a, str>,
);

impl WireAccount<'_> {
    /// Validates and decodes the account, retaining Uninit mode for caller classification.
    pub(crate) fn decode(self, slot: u64) -> Result<OwnedAccount, Error> {
        if self.data.1 != ENCODING {
            return Err(Error::Protocol("unsupported account encoding"));
        }
        let owner: Pubkey = self.owner.parse()?;
        let data = STANDARD.decode(self.data.0.as_bytes())?;
        let data = zstd::stream::decode_all(data.as_slice()).map_err(Error::Zstd)?;
        // Classification belongs to the caller; the builder retains Uninit mode.
        let account = AccountBuilder::default()
            .owner(owner)
            .lamports(self.lamports)
            .executable(self.executable)
            .slot(slot)
            .data(data)
            .build();
        Ok(account)
    }
}
