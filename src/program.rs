//! Converts remote loader layouts into Engine's ELF account format.

use solana_account::{AccountBuilder, AccountMode, OwnedAccount};
use solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState};
use solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status};
use solana_pubkey::Pubkey;
use solana_rent::Rent;
use solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4};

use crate::{AccountProperty, Error, SyncAccount};

/// Replaces program snapshots after all companion accounts have been read.
pub(super) fn normalize_batch(
    requests: impl Iterator<Item = SyncAccount>,
    keys: &[Pubkey],
    accounts: &mut [Option<OwnedAccount>],
) -> Result<Vec<Pubkey>, Error> {
    let mut normalized = Vec::new();
    let mut unused = Vec::new();
    for request in requests.filter(|request| request.property == AccountProperty::Program) {
        let key = request.pubkey;
        let data_key = get_program_data_address(&key);
        let index = keys.binary_search(&key).expect("requested key is in the batch");
        let data_index = keys.binary_search(&data_key).expect("companion is in the batch");
        let program =
            accounts[index].as_ref().ok_or(Error::Program("program account is missing"))?;
        // Only Loader V3 needs a persistent ProgramData subscription.
        if program.owner() != bpf_loader_upgradeable::ID {
            unused.push(data_key);
        }
        normalized.push((index, normalize(program, accounts[data_index].as_ref())?));
    }
    // A companion can also be a primary key, so finish reading before replacing snapshots.
    for (index, program) in normalized {
        accounts[index] = Some(program);
    }
    Ok(unused)
}

/// Produces the executable account representation expected by Engine.
fn normalize(
    account: &OwnedAccount,
    program_data: Option<&OwnedAccount>,
) -> Result<OwnedAccount, Error> {
    // Legacy ABI keeps Loader V1 ownership; newer ELF uses Loader V4 ownership.
    let (data, slot, owner) = match account.owner() {
        id if id == bpf_loader_deprecated::ID => (account.data(), account.slot(), id),
        id if id == bpf_loader::ID => (account.data(), account.slot(), loader_v4::ID),
        id if id == bpf_loader_upgradeable::ID => {
            let program_data =
                program_data.ok_or(Error::Program("Loader V3 ProgramData is missing"))?;
            (v3_elf(program_data)?, program_data.slot(), loader_v4::ID)
        }
        id if id == loader_v4::ID => (v4_elf(account)?, account.slot(), id),
        _ => return Err(Error::Program("unsupported loader")),
    };

    let lamports = Rent::default().minimum_balance(data.len());
    Ok(AccountBuilder::default()
        .lamports(lamports)
        .data(data.to_vec())
        .owner(owner)
        .mode(AccountMode::ReadOnly)
        .executable(true)
        .slot(slot)
        .build())
}

/// Returns the ELF bytes following validated Loader V3 ProgramData metadata.
fn v3_elf(account: &OwnedAccount) -> Result<&[u8], Error> {
    let data = account.data();
    let metadata = data
        .get(..UpgradeableLoaderState::size_of_programdata_metadata())
        .ok_or(Error::Program("Loader V3 ProgramData is too short"))?;
    let state = bincode::deserialize::<UpgradeableLoaderState>(metadata)?;
    if !matches!(state, UpgradeableLoaderState::ProgramData { .. }) {
        return Err(Error::Program("Loader V3 companion is not ProgramData"));
    }
    Ok(&data[metadata.len()..])
}

/// Returns deployed or finalized Loader V4 ELF bytes.
fn v4_elf(account: &OwnedAccount) -> Result<&[u8], Error> {
    let data = account.data();
    let offset = LoaderV4State::program_data_offset();
    let header = data.get(..offset).ok_or(Error::Program("Loader V4 account is too short"))?;
    // Decode the discriminant as bytes so malformed input cannot create an invalid enum.
    let status_offset = std::mem::offset_of!(LoaderV4State, status);
    let status = header
        .get(status_offset..status_offset + std::mem::size_of::<u64>())
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(Error::Program("Loader V4 status is missing"))?;
    if status == LoaderV4Status::Retracted as u64 {
        return Err(Error::Program("Loader V4 program is retracted"));
    }
    if status != LoaderV4Status::Deployed as u64 && status != LoaderV4Status::Finalized as u64 {
        return Err(Error::Program("Loader V4 status is invalid"));
    }
    Ok(&data[offset..])
}
