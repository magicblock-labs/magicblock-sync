use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::{pubkey, Pubkey};

use crate::delegation;

const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ASSOCIATED_TOKEN: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const EATA: Pubkey = pubkey!("SPLxh1LVZzEkX99H6rqYizhytLWPZVV296zyYDPagv2");
const NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub(crate) fn is_eata(account: &AccountBuilder) -> bool {
    account.read().owner() == EATA
}

/// A raw eATA image, whether restored or temporarily owned by the DLP.
pub(crate) fn is_raw_eata(pubkey: Pubkey, account: &AccountBuilder) -> bool {
    eata_data(pubkey, account).is_some()
}

/// The canonical eATA derived from a token account's owner and mint.
pub(crate) fn companion(pubkey: Pubkey, account: &AccountBuilder) -> Option<Pubkey> {
    let image = account.read();
    let program = image.owner();
    if program != TOKEN && program != TOKEN_2022 {
        return None;
    }
    let data = image.data();
    if data.len() < 165 || !matches!(data[108], 1 | 2) {
        return None;
    }
    let mint = Pubkey::new_from_array(data[0..32].try_into().ok()?);
    let owner = Pubkey::new_from_array(data[32..64].try_into().ok()?);
    if derive_ata(owner, mint, program)? != pubkey {
        return None;
    }
    derive_eata(owner, mint).map(|v| v.0)
}

/// Projected ATAs carry an uncloseable marker and delegated lifecycle mode.
pub(crate) fn projected_for(ata: Pubkey, eata: Pubkey, account: &AccountBuilder) -> bool {
    let image = account.read();
    matches!(
        image.mode(),
        AccountMode::Delegated | AccountMode::Transient
    ) && companion(ata, account) == Some(eata)
        && image.data()[129..133] == 1u32.to_le_bytes()
        && image.data()[133..165] == [0; 32]
}

/// ATA candidates for a restored eATA. The token program is not stored in eATA.
pub(crate) fn candidates(pubkey: Pubkey, account: &AccountBuilder) -> Option<[Pubkey; 2]> {
    if !is_eata(account) {
        return None;
    }
    let (owner, mint, _) = eata_data(pubkey, account)?;
    Some([derive_ata(owner, mint, TOKEN_2022)?, derive_ata(owner, mint, TOKEN)?])
}

/// A valid local delegation projects only its balance and generation onto the ATA.
/// The base image retains its token program, layout, extensions, and rent fields.
pub(crate) fn project(
    ata: Pubkey,
    base: AccountBuilder,
    eata: Pubkey,
    delegated: &AccountBuilder,
    record: &[u8],
    authority: Pubkey,
) -> Option<AccountBuilder> {
    let metadata = delegation::record(record)?;
    if metadata.owner != EATA || metadata.authority != authority {
        return None;
    }
    let (_, mint, amount) = eata_data(eata, delegated)?;
    if companion(ata, &base)? != eata {
        return None;
    }
    let mut data = base.read().data().to_vec();
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    // SPL Token and Token-2022 share the 165-byte base account layout.
    // A projected ATA is virtual: local close is forbidden, and native-token
    // lamports cannot be spent as the eATA balance.
    data[129..133].copy_from_slice(&1u32.to_le_bytes());
    data[133..165].fill(0);
    let reserve = if mint == NATIVE_MINT && data[109..113] == 1u32.to_le_bytes() {
        data[109..113].fill(0);
        Some(u64::from_le_bytes(data[113..121].try_into().ok()?))
    } else {
        None
    };
    let projected = base.data(data).slot(metadata.delegation_slot).mode(AccountMode::Delegated);
    Some(match reserve {
        Some(lamports) => projected.lamports(lamports),
        None => projected,
    })
}

/// Parses only eATA data whose address and current or delegated owner agree.
fn eata_data(pubkey: Pubkey, account: &AccountBuilder) -> Option<(Pubkey, Pubkey, u64)> {
    let image = account.read();
    if image.owner() != EATA && image.owner() != dlp_api::id() {
        return None;
    }
    let (owner, mint, amount, bump) = parse_eata(image.data())?;
    (derive_eata(owner, mint)? == (pubkey, bump)).then_some((owner, mint, amount))
}

/// Validates both eATA layouts, including the PDA bump in the current layout.
fn parse_eata(data: &[u8]) -> Option<(Pubkey, Pubkey, u64, u8)> {
    if data.len() != 72 && data.len() != 80 {
        return None;
    }
    let owner = Pubkey::new_from_array(data[0..32].try_into().ok()?);
    let mint = Pubkey::new_from_array(data[32..64].try_into().ok()?);
    if mint == Pubkey::default() {
        return None;
    }
    let amount = u64::from_le_bytes(data[64..72].try_into().ok()?);
    let bump = if data.len() == 80 { data[72] } else { derive_eata(owner, mint)?.1 };
    Some((owner, mint, amount, bump))
}

fn derive_ata(owner: Pubkey, mint: Pubkey, program: Pubkey) -> Option<Pubkey> {
    Pubkey::try_find_program_address(
        &[owner.as_ref(), program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN,
    )
    .map(|v| v.0)
}

fn derive_eata(owner: Pubkey, mint: Pubkey) -> Option<(Pubkey, u8)> {
    Pubkey::try_find_program_address(&[owner.as_ref(), mint.as_ref()], &EATA)
}
