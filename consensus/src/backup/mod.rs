use codec::{Decode, Encode};
use std::fmt::Debug;

pub use loader::{BackupLoader, BackupReader, LoadedData};
pub use saver::{BackupSaver, BackupWriter};

use crate::{alerts::AlertData, units::UncheckedSignedUnit, Data, Hasher, MultiKeychain};

mod loader;
mod saver;

#[derive(Clone, Debug, Decode, Encode, PartialEq)]
pub enum BackupItem<H: Hasher, D: Data, MK: MultiKeychain> {
    Unit(UncheckedSignedUnit<H, D, MK::Signature>),
    AlertData(AlertData<H, D, MK>),
}
