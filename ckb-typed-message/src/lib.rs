#![no_std]
extern crate alloc;
pub mod blake2b;
pub mod schemas;

use alloc::vec::Vec;
use blake2b::new_blake2b;
use ckb_std::{
    ckb_constants::Source,
    ckb_types::packed::CellInput,
    error::SysError,
    high_level::{load_tx_hash, load_witness, QueryIter},
    syscalls::load_transaction,
};
use core::convert::Into;
use molecule::{
    error::VerificationError,
    prelude::{Entity, Reader},
    NUMBER_SIZE,
};
use schemas::{
    basic::SighashWithAction,
    top_level::{
        ExtendedWitness, ExtendedWitnessReader, ExtendedWitnessUnion, ExtendedWitnessUnionReader,
    },
};

#[derive(Eq, PartialEq, Debug, Clone, Copy)]
pub enum Error {
    Sys(SysError),
    MoleculeEncoding,
    WrongSighashWithAction,
    WrongWitnessLayout,
}

impl From<SysError> for Error {
    fn from(e: SysError) -> Self {
        Error::Sys(e)
    }
}

impl From<VerificationError> for Error {
    fn from(_: VerificationError) -> Self {
        Error::MoleculeEncoding
    }
}

///
/// Fetch the corresponding ExtendedWitness( Sighash or SighashWithAction)
/// Used by lock script
///
pub fn fetch_sighash() -> Result<ExtendedWitness, Error> {
    match load_witness(0, Source::GroupInput) {
        Ok(witness) => {
            if let Ok(r) = ExtendedWitnessReader::from_slice(&witness) {
                match r.to_enum() {
                    ExtendedWitnessUnionReader::SighashWithAction(_)
                    | ExtendedWitnessUnionReader::Sighash(_) => Ok(r.to_entity()),
                    _ => Err(Error::MoleculeEncoding),
                }
            } else {
                Err(Error::MoleculeEncoding)
            }
        }
        Err(e) => Err(e.into()),
    }
}

///
/// fetch the only SighashWithAction from all witnesses.
/// This function can also check the count of SighashWithAction is one.
///
pub fn fetch_sighash_with_action() -> Result<SighashWithAction, Error> {
    let mut result = None;

    for witness in QueryIter::new(load_witness, Source::Input) {
        if let Ok(r) = ExtendedWitnessReader::from_slice(&witness) {
            if let ExtendedWitnessUnionReader::SighashWithAction(s) = r.to_enum() {
                if result.is_some() {
                    return Err(Error::WrongSighashWithAction);
                } else {
                    result = Some(s.to_entity());
                }
            }
        }
    }
    if result.is_some() {
        return Ok(result.unwrap());
    } else {
        return Err(Error::WrongSighashWithAction);
    }
}

///
/// for lock script with typed message, the other witness in script group except
/// first one should be empty
///
pub fn check_others_in_group() -> Result<(), Error> {
    for witness in QueryIter::new(load_witness, Source::GroupInput).skip(1) {
        if witness.as_slice().len() != 0 {
            return Err(Error::WrongWitnessLayout);
        }
    }
    Ok(())
}

//
// Rule for hashing:
// 1. Variable length data should hash the length.
// 2. Fixed length data don't need to hash the length.
//
pub fn generate_skeleton_hash() -> Result<[u8; 32], Error> {
    let mut hasher = new_blake2b();
    hasher.update(&load_tx_hash()?);

    let mut i = calculate_inputs_len()?;
    loop {
        match load_witness(i, Source::Input) {
            Ok(w) => {
                hasher.update(&(w.len() as u64).to_le_bytes());
                hasher.update(&w);
            }
            Err(SysError::IndexOutOfBound) => {
                break;
            }
            Err(e) => return Err(e.into()),
        }
        i += 1;
    }

    let mut output = [0u8; 32];
    hasher.finalize(&mut output);

    Ok(output)
}

pub fn generate_final_hash(skeleton_hash: &[u8; 32], typed_message: &[u8]) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(&skeleton_hash[..]);
    hasher.update(&(typed_message.len() as u64).to_le_bytes());
    hasher.update(typed_message);
    let mut output = [0u8; 32];
    hasher.finalize(&mut output);
    return output;
}

///
/// the molecule data structure of transaction is:
/// full-size|raw-offset|witnesses-offset|raw-full-size|version-offset|cell_deps-offset|header_deps-offset|inputs-offset|outputs-offset|...
/// full-size and offset are 4 bytes, so we can read the inputs-offset and outputs-offset at [28, 36),
/// then we can get the length of inputs by calculating the difference between inputs-offset and outputs-offset
///
fn calculate_inputs_len() -> Result<usize, SysError> {
    let mut offsets = [0u8; 8];
    match load_transaction(&mut offsets, 28) {
        // this syscall will always return SysError::LengthNotEnough since we only load 8 bytes, let's ignore it
        Err(SysError::LengthNotEnough(_)) => {}
        Err(SysError::Unknown(e)) => return Err(SysError::Unknown(e)),
        _ => unreachable!(),
    }
    let inputs_offset = u32::from_le_bytes(offsets[0..4].try_into().unwrap());
    let outputs_offset = u32::from_le_bytes(offsets[4..8].try_into().unwrap());
    Ok((outputs_offset as usize - inputs_offset as usize - NUMBER_SIZE) / CellInput::TOTAL_SIZE)
}

///
/// parse transaction with typed message and return 2 values:
/// 1. digest message, 32 bytes message for signature verification
/// 2. lock, lock field in SighashWithAction or Sighash. Normally as signature.
/// This function is mainly used by lock script
///
pub fn parse_typed_message() -> Result<([u8; 32], Vec<u8>), Error> {
    check_others_in_group()?;
    // Ensure that a SighashWitAction is present throughout the entire transaction
    let sighash_with_action = fetch_sighash_with_action()?;
    // There are 2 possible values: Sighash or SighashWithAction
    let witness = fetch_sighash()?;
    let (lock, typed_message) = match witness.to_enum() {
        ExtendedWitnessUnion::SighashWithAction(s) => (s.lock(), s.message()),
        ExtendedWitnessUnion::Sighash(s) => (s.lock(), sighash_with_action.message()),
        _ => {
            return Err(Error::WrongSighashWithAction);
        }
    };
    let skeleton_hash = generate_skeleton_hash()?;
    let digest_message = generate_final_hash(&skeleton_hash, typed_message.as_slice());
    Ok((digest_message, lock.raw_data().into()))
}
