#![no_std]

#[cfg(feature = "allocator")]
extern crate alloc;
use block_device_api::BlockDevice;
use block_device_api::IoError;
use core::mem::MaybeUninit;

mod bootsector;
use bootsector::mbr::MasterBootRecord;
mod filesystem;

use crate::bootsector::mbr::MasterBootRecordPartitionKind;

/// return LBA and size
/// Semantics
/// - Ok(Some((start_lba, num_lba))) if index n is exist
/// - Ok(None) index n is not exist
/// - Err(IoError) corrupted partition
///

enum BootSector {
    MBR,
    GPT,
    Unknown,
}

pub struct PartitionIndex {
    boot_sector: BootSector,
}

impl PartitionIndex {
    pub fn new<D>(block_device: &D) -> Result<Self, FileSystemErr>
    where
        D: BlockDevice,
    {
        let mut buffer = [MaybeUninit::uninit(); 512];
        block_device.read_at(0, &mut buffer).map_err(from_io_err)?;
        let boot_record = buffer.as_mut_ptr() as *mut MasterBootRecord;
        let signature = unsafe { (*boot_record).boot_signature };
        if signature[0] != 0x55 || signature[1] != 0xAA {
            return Ok(Self {
                boot_sector: BootSector::Unknown,
            });
        }
        let kind = unsafe { (*boot_record).first_partition.kind };
        return Ok(Self {
            boot_sector: match kind {
                MasterBootRecordPartitionKind::TYPE_GPT => {
                    // TODO GPT check
                    BootSector::GPT
                }
                _ => BootSector::MBR,
            },
        });
    }

    #[cfg(feature = "allocator")]
    pub fn get_file<'a, D>(
        &self,
        block_device: &D,
        partition_idx: u8,
        file_name: &'a str,
        limit_size: Option<u64>,
    ) -> Result<alloc::boxed::Box<[u8]>, FileSystemErr>
    where
        D: BlockDevice,
    {
        todo!()
    }

    pub fn get_file_to_buffer<'a, D>(
        &self,
        block_device: &D,
        partition_idx: u8,
        file_name: &'a str,
        buffer: &mut [MaybeUninit<u8>],
    ) -> Result<(), FileSystemErr>
    where
        D: BlockDevice,
    {
        todo!()
    }
}

pub enum FileSystemErr {
    BlockDeviceErr(IoError),
}

fn from_io_err(err: IoError) -> FileSystemErr {
    // TODO restart block device when IoError::Io returned
    FileSystemErr::BlockDeviceErr(err)
}
