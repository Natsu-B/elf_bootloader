//! Filesystem mounting and shared filesystem error types.

#![no_std]
extern crate alloc;

use alloc::sync::Arc;
use block_device_api::BlockDevice;
use block_device_api::IoError;

pub mod filesystem;

use crate::filesystem::FileSystemTrait;
use crate::filesystem::file_system;

pub fn mount_filesystem(
    block_device: &Arc<dyn BlockDevice>,
    start_sector: u64,
    total_sector: u64,
) -> Result<Arc<dyn FileSystemTrait>, FileSystemErr> {
    file_system::new(block_device, start_sector, total_sector)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystemErr {
    BlockDeviceErr(IoError),
    UnknownPartition,
    UnusedPartition,
    UnsupportedFileSystem,
    NotFound,
    AlreadyExists,
    IsDir,
    NotDir,
    ReadOnly,
    NoSpace,
    InvalidInput,
    Busy,
    Corrupted,
    Closed,
    NotRootDir,
    UnsupportedFileName,
    TooBigBuffer,
    IncompleteRead,
}

pub(crate) fn from_io_err(err: IoError) -> FileSystemErr {
    // TODO restart block device when IoError::Io returned
    FileSystemErr::BlockDeviceErr(err)
}
