#![no_std]
#![feature(maybe_uninit_array_assume_init)]
#![feature(maybe_uninit_as_bytes)]
#![feature(maybe_uninit_slice)]

extern crate alloc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block_device_api::BlockDevice;
use block_device_api::IoError;
use core::mem::MaybeUninit;
use core::num::ParseIntError;
use core::ptr::addr_of;
use core::ptr::read_unaligned;
use mutex::SpinLock;
use typestate::Le;
use typestate::Unaligned;
use typestate::unalign_read;

mod bootsector;
use bootsector::mbr::MasterBootRecord;
pub mod aligned_box;
pub mod filesystem;

use crate::aligned_box::AlignedSliceBox;
use crate::bootsector::BootSector;
use crate::bootsector::MBRConfig;
use crate::bootsector::MBRPartition;
use crate::bootsector::mbr::MasterBootRecordPartitionKind;
use crate::filesystem::FileHandle;
use crate::filesystem::FileSystemTrait;
use crate::filesystem::OpenOptions;
use crate::filesystem::file_system;

pub struct PartitionIndex {
    sector_kind: BootSector,
    partitions: SpinLock<Vec<(u8, Arc<dyn FileSystemTrait>)>>,
}

impl PartitionIndex {
    pub(crate) const BOOT_SIGNATURE: u16 = 0xAA55;

    pub fn new<D>(block_device: &D) -> Result<Self, FileSystemErr>
    where
        D: BlockDevice,
    {
        let mut buffer =
            AlignedSliceBox::<u8>::new_uninit_with_align(block_device.block_size(), 1).unwrap();
        block_device.read_at(0, &mut buffer).map_err(from_io_err)?;
        let mut buffer = unsafe { buffer.assume_init() };
        let boot_record = buffer.as_mut_ptr() as *mut MasterBootRecord;
        if unalign_read!((*boot_record).boot_signature => Le<Unaligned<u16>>)
            != Self::BOOT_SIGNATURE
        {
            return Ok(Self {
                sector_kind: BootSector::Unknown,
                partitions: SpinLock::new(Vec::with_capacity(1)),
            });
        }
        let config = MBRConfig {
            partition: [
                MBRPartition {
                    kind: unsafe { *addr_of!((*boot_record).first_partition.kind) },
                    first_sector: unalign_read!((*boot_record).first_partition.lba_first_sector => Le<Unaligned<u32>>),
                    total_sector: unalign_read!((*boot_record).first_partition.num_of_total_sector => Le<Unaligned<u32>>),
                },
                MBRPartition {
                    kind: unsafe { *addr_of!((*boot_record).second_partition.kind) },
                    first_sector: unalign_read!((*boot_record).second_partition.lba_first_sector => Le<Unaligned<u32>>),
                    total_sector: unalign_read!((*boot_record).second_partition.num_of_total_sector => Le<Unaligned<u32>>),
                },
                MBRPartition {
                    kind: unsafe { *addr_of!((*boot_record).third_partition.kind) },
                    first_sector: unalign_read!((*boot_record).third_partition.lba_first_sector => Le<Unaligned<u32>>),
                    total_sector: unalign_read!((*boot_record).third_partition.num_of_total_sector => Le<Unaligned<u32>>),
                },
                MBRPartition {
                    kind: unsafe { *addr_of!((*boot_record).fourth_partition.kind) },
                    first_sector: unalign_read!((*boot_record).fourth_partition.lba_first_sector => Le<Unaligned<u32>>),
                    total_sector: unalign_read!((*boot_record).fourth_partition.num_of_total_sector => Le<Unaligned<u32>>),
                },
            ],
        };
        Ok(Self {
            sector_kind: match config.partition[0].kind {
                MasterBootRecordPartitionKind::TYPE_GPT => {
                    // TODO GPT check
                    todo!();
                    BootSector::GPT
                }
                _ => BootSector::MBR(config),
            },
            partitions: SpinLock::new(Vec::with_capacity(2)),
        })
    }

    /// Semantics
    /// - Ok((u64/* start_sector */, u64 /* total_sector */))
    /// - Err(FileSystemErr)
    fn get_partition_start_total_sector(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
    ) -> Result<(u64, u64), FileSystemErr> {
        match &self.sector_kind {
            BootSector::MBR(x) => {
                if partition_idx >= 4 {
                    return Err(FileSystemErr::UnknownPartition);
                }
                if x.partition[partition_idx as usize].kind == MasterBootRecordPartitionKind::UNUSED
                {
                    return Err(FileSystemErr::UnusedPartition);
                }
                let start = x.partition[partition_idx as usize].first_sector;
                let total = x.partition[partition_idx as usize].total_sector;
                Ok((start as u64, total as u64))
            }
            BootSector::GPT => {
                todo!()
            }
            BootSector::Unknown => {
                if partition_idx == 0 {
                    Ok((0, block_device.num_blocks()))
                } else {
                    Err(FileSystemErr::UnknownPartition)
                }
            }
        }
    }

    fn get_partition_driver(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
    ) -> Result<Arc<dyn FileSystemTrait>, FileSystemErr> {
        if let Some(partition_driver) = self.partitions.lock().iter().find(|x| x.0 == partition_idx)
        {
            return Ok(partition_driver.1.clone());
        };
        let (start_sector, total_sector) =
            self.get_partition_start_total_sector(block_device, partition_idx)?;
        // TODO file system type hint when MBR
        let file_driver = file_system::new(block_device, start_sector, total_sector)?;
        self.partitions
            .lock()
            .push((partition_idx, file_driver.clone()));
        Ok(file_driver)
    }

    pub fn open(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
        path: &str,
        opts: &OpenOptions,
    ) -> Result<FileHandle, FileSystemErr> {
        let file_driver = self.get_partition_driver(block_device, partition_idx)?;
        file_driver.open(block_device, &file_driver, path, opts)
    }

    pub fn remove_file(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        let file_driver = self.get_partition_driver(block_device, partition_idx)?;
        file_driver.remove_file(path)
    }

    pub fn copy(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
        from: &str,
        to: &str,
    ) -> Result<(), FileSystemErr> {
        let file_driver = self.get_partition_driver(block_device, partition_idx)?;
        file_driver.copy(from, to)
    }

    pub fn rename(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
        from: &str,
        to: &str,
    ) -> Result<(), FileSystemErr> {
        let file_driver = self.get_partition_driver(block_device, partition_idx)?;
        file_driver.rename(from, to)
    }

    pub fn create_dir(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        let file_driver = self.get_partition_driver(block_device, partition_idx)?;
        file_driver.create_dir(path)
    }

    pub fn remove_dir(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        partition_idx: u8,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        let file_driver = self.get_partition_driver(block_device, partition_idx)?;
        file_driver.remove_dir(path)
    }
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
