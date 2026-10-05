use solana_account::{AccountBuilder, AccountMode};
use solana_program_option::COption;
use solana_program_pack::Pack;
use solana_pubkey::{pubkey, Pubkey};
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_2022_interface::{extension::StateWithExtensions, state::Account as TokenAccount};

use crate::delegation;

/// Program owning eATA balances projected onto canonical token ATAs.
const EATA_PROGRAM_ID: Pubkey = pubkey!("SPLxh1LVZzEkX99H6rqYizhytLWPZVV296zyYDPagv2");

/// Fields needed from eATA's custom wire state.
struct Eata {
    /// Token authority used in both eATA and canonical ATA derivation.
    owner: Pubkey,
    /// Token mint used in address derivation and the projected account image.
    mint: Pubkey,
    amount: u64,
}

/// Identifies restored eATA ownership; raw DLP-owned images require address/layout validation instead.
pub(crate) fn is_eata(account: &AccountBuilder) -> bool {
    account.read().owner() == EATA_PROGRAM_ID
}

/// Recognizes valid eATA data at its derived address, including while DLP-owned.
pub(crate) fn is_raw_eata(pubkey: Pubkey, account: &AccountBuilder) -> bool {
    eata_data(pubkey, account).is_some()
}

/// The canonical eATA derived from a token account's owner and mint.
pub(crate) fn companion(pubkey: Pubkey, account: &AccountBuilder) -> Option<Pubkey> {
    let token = token_account(pubkey, account)?;
    derive_eata(token.owner, token.mint).map(|(pubkey, _)| pubkey)
}

/// Checks whether this delegated or transient ATA is a projection of the given eATA.
/// Projections have the default pubkey as close authority, preventing local closure.
pub(crate) fn is_projection_of(ata: Pubkey, eata: Pubkey, account: &AccountBuilder) -> bool {
    if !matches!(
        account.read().mode(),
        AccountMode::Delegated | AccountMode::Transient
    ) {
        return false;
    }
    let Some(token) = token_account(ata, account) else { return false };
    token.close_authority == COption::Some(Pubkey::default())
        && derive_eata(token.owner, token.mint).is_some_and(|(pubkey, _)| pubkey == eata)
}

/// Returns legacy-token and Token-2022 ATA addresses for an eATA-owned account.
/// Both are needed because eATA data does not identify the token program.
pub(crate) fn candidates(pubkey: Pubkey, account: &AccountBuilder) -> Option<[Pubkey; 2]> {
    if !is_eata(account) {
        return None;
    }
    let eata = eata_data(pubkey, account)?;
    Some(
        [spl_token_2022_interface::id(), spl_token_interface::id()].map(|program| {
            get_associated_token_address_with_program_id(&eata.owner, &eata.mint, &program)
        }),
    )
}

/// Projects an eATA's balance and delegation slot onto a canonical ATA.
/// Preserves the base ATA's token program and extensions; native-token ATAs retain only rent lamports.
pub(crate) fn project(
    ata: Pubkey,
    base: AccountBuilder,
    eata: Pubkey,
    delegated: &AccountBuilder,
    record: &[u8],
    authority: Pubkey,
) -> Option<AccountBuilder> {
    let metadata = delegation::record(record)?;
    if metadata.owner != EATA_PROGRAM_ID || metadata.authority != authority {
        return None;
    }
    let eata_state = eata_data(eata, delegated)?;
    let mut token = token_account(ata, &base)?;
    if token.owner != eata_state.owner || token.mint != eata_state.mint {
        return None;
    }

    // The projected ATA is virtual: it cannot be closed locally, and native-token
    // lamports cannot be spent as part of the eATA balance.
    token.amount = eata_state.amount;
    token.close_authority = COption::Some(Pubkey::default());
    let reserve = token.is_native;
    token.is_native = COption::None;
    let mut data = base.read().data().to_vec();
    // Both token programs share this base layout; preserve any extension bytes.
    token.pack_into_slice(&mut data[..TokenAccount::LEN]);
    let projected = base.data(data).slot(metadata.delegation_slot).mode(AccountMode::Delegated);
    Some(match reserve {
        COption::Some(lamports) => projected.lamports(lamports),
        COption::None => projected,
    })
}

/// Recognizes an initialized token account at its canonical ATA address.
fn token_account(pubkey: Pubkey, account: &AccountBuilder) -> Option<TokenAccount> {
    let image = account.read();
    let program = image.owner();
    let token = if program == spl_token_interface::id() {
        TokenAccount::unpack(image.data()).ok()?
    } else if program == spl_token_2022_interface::id() {
        StateWithExtensions::<TokenAccount>::unpack(image.data()).ok()?.base
    } else {
        return None;
    };
    let canonical =
        get_associated_token_address_with_program_id(&token.owner, &token.mint, &program);
    (canonical == pubkey).then_some(token)
}

/// An eATA is trusted only when its owner and derived address agree with its data.
fn eata_data(pubkey: Pubkey, account: &AccountBuilder) -> Option<Eata> {
    /// Width of each owner or mint key in the eATA wire layout.
    const KEY_LEN: usize = size_of::<Pubkey>();
    /// First byte of the token amount, following owner and mint.
    const MINT_END: usize = KEY_LEN * 2;
    /// Legacy layout length, ending after the token amount.
    const BASE_LEN: usize = MINT_END + size_of::<u64>();
    /// Current layout length, including the bump byte and trailing bytes.
    const CURRENT_LEN: usize = BASE_LEN + 8;

    let image = account.read();
    if image.owner() != EATA_PROGRAM_ID && image.owner() != dlp_api::id() {
        return None;
    }
    let data = image.data();
    if data.len() != BASE_LEN && data.len() != CURRENT_LEN {
        return None;
    }
    let owner = Pubkey::new_from_array(data[..KEY_LEN].try_into().ok()?);
    let mint = Pubkey::new_from_array(data[KEY_LEN..MINT_END].try_into().ok()?);
    if mint == Pubkey::default() {
        return None;
    }
    let (address, bump) = derive_eata(owner, mint)?;
    if address != pubkey || (data.len() == CURRENT_LEN && data[BASE_LEN] != bump) {
        return None;
    }
    let amount = u64::from_le_bytes(data[MINT_END..BASE_LEN].try_into().ok()?);
    Some(Eata { owner, mint, amount })
}

/// Derives the eATA address and bump shared by both supported eATA layouts.
fn derive_eata(owner: Pubkey, mint: Pubkey) -> Option<(Pubkey, u8)> {
    Pubkey::try_find_program_address(&[owner.as_ref(), mint.as_ref()], &EATA_PROGRAM_ID)
}

#[cfg(test)]
mod test;
