//! Converts remote loader layouts into Engine's ELF account format.

use solana_account::{AccountBuilder, AccountMode};
use solana_loader_v3_interface::state::UpgradeableLoaderState;
use solana_loader_v4_interface::state::{LoaderV4State, LoaderV4Status};
use solana_rent::Rent;
use solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4};

use crate::Error;

/// Replaces each present program with its Engine ELF image and consumes its
/// ProgramData companion. A missing program remains absent.
pub(super) fn normalize_batch(
    programs: &[(usize, usize)],
    accounts: &mut [Option<AccountBuilder>],
    rent: &Rent,
) -> Result<(), Error> {
    for &(index, data_index) in programs {
        let Some(program) = accounts[index].take() else { continue };
        let program_data = accounts[data_index].take();
        accounts[index].replace(normalize(program, program_data, rent)?);
    }
    Ok(())
}

/// Accepts supported loader layouts; Loader V3 requires its ProgramData image.
/// The resulting ELF account is read-only and rent-funded for its normalized size.
pub(super) fn normalize(
    account: AccountBuilder,
    program_data: Option<AccountBuilder>,
    rent: &Rent,
) -> Result<AccountBuilder, Error> {
    // Legacy ABI keeps Loader V1 ownership; newer ELF uses Loader V4 ownership.
    match account.read().owner() {
        id if id == bpf_loader_deprecated::ID => Ok(elf_account(account, id, rent)),
        id if id == bpf_loader::ID => Ok(elf_account(account, loader_v4::ID, rent)),
        id if id == bpf_loader_upgradeable::ID => {
            let program_data =
                program_data.ok_or(Error::Program("Loader V3 ProgramData is missing"))?;
            normalize_data(program_data, rent)
        }
        id if id == loader_v4::ID => {
            let elf = v4_elf(account.read().data())?.to_vec();
            Ok(elf_account(account.data(elf), id, rent))
        }
        _ => Err(Error::Program("unsupported loader")),
    }
}

/// Uses a ProgramData update to refresh the target program without refetching
/// its unchanged Loader V3 parent.
pub(super) fn normalize_data(
    account: AccountBuilder,
    rent: &Rent,
) -> Result<AccountBuilder, Error> {
    let elf = v3_elf(account.read().data())?.to_vec();
    Ok(elf_account(account.data(elf), loader_v4::ID, rent))
}

/// Sets the normalized ELF's owner, rent-exempt balance, and read-only mode.
fn elf_account(
    account: AccountBuilder,
    owner: solana_pubkey::Pubkey,
    rent: &Rent,
) -> AccountBuilder {
    let len = account.read().data().len();
    account
        .lamports(rent.minimum_balance(len))
        .owner(owner)
        .mode(AccountMode::ReadOnly)
        .executable(true)
}

/// Requires a Loader V3 ProgramData header before exposing the following ELF.
fn v3_elf(data: &[u8]) -> Result<&[u8], Error> {
    let metadata = data
        .get(..UpgradeableLoaderState::size_of_programdata_metadata())
        .ok_or(Error::Program("Loader V3 ProgramData is too short"))?;
    let state = bincode::deserialize::<UpgradeableLoaderState>(metadata)?;
    if !matches!(state, UpgradeableLoaderState::ProgramData { .. }) {
        return Err(Error::Program("Loader V3 companion is not ProgramData"));
    }
    Ok(&data[metadata.len()..])
}

/// Rejects retracted or invalid Loader V4 status before exposing its ELF.
fn v4_elf(data: &[u8]) -> Result<&[u8], Error> {
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
