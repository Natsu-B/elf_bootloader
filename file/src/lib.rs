//! High-level storage facade combining block devices and filesystem access.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use block_device::VirtIoBlk;
use block_device_api::BlockDevice;
use block_device_api::IoError;
use filesystem::FileSystemErr;

mod partition;
use partition::PartitionIndex;

pub use allocator::AlignedSliceBox;
pub use filesystem::filesystem::FileHandle;
pub use filesystem::filesystem::OpenOptions;

pub struct StorageDevice {
    dev: Arc<dyn BlockDevice>,
    partition: PartitionIndex,
}

struct BorrowedBlockDevice {
    dev: &'static dyn BlockDevice,
}

impl StorageDevice {
    pub fn new_virtio(mmio: usize) -> Result<Self, StorageDeviceErr> {
        let mut io = VirtIoBlk::new(mmio).map_err(StorageDeviceErr::IoErr)?;
        io.init().map_err(StorageDeviceErr::IoErr)?;
        let dev = Arc::new(io);
        let partition = PartitionIndex::new(dev.as_ref())?;
        Ok(Self { partition, dev })
    }

    /// Builds a storage facade on top of an already initialized block device.
    ///
    /// The returned `StorageDevice` borrows the backend instead of owning it, so dropping this
    /// facade does not uninstall the underlying device.
    pub fn from_ready_block_device(
        dev: &'static dyn BlockDevice,
    ) -> Result<Self, StorageDeviceErr> {
        let dev: Arc<dyn BlockDevice> = Arc::new(BorrowedBlockDevice { dev });
        let partition = PartitionIndex::new(dev.as_ref())?;
        Ok(Self { partition, dev })
    }

    pub fn open(
        &self,
        partition_idx: u8,
        path: &str,
        opts: &OpenOptions,
    ) -> Result<FileHandle, StorageDeviceErr> {
        self.partition
            .open(&self.dev, partition_idx, path, opts)
            .map_err(StorageDeviceErr::FileSystemErr)
    }

    pub fn create_file(
        &self,
        partition_idx: u8,
        path: &str,
    ) -> Result<FileHandle, StorageDeviceErr> {
        self.partition
            .create_file(&self.dev, partition_idx, path)
            .map_err(StorageDeviceErr::FileSystemErr)
    }

    pub fn remove_file(&self, partition_idx: u8, path: &str) -> Result<(), StorageDeviceErr> {
        self.partition
            .remove_file(&self.dev, partition_idx, path)
            .map_err(StorageDeviceErr::FileSystemErr)
    }

    pub fn copy(&self, partition_idx: u8, from: &str, to: &str) -> Result<(), StorageDeviceErr> {
        self.partition
            .copy(&self.dev, partition_idx, from, to)
            .map_err(StorageDeviceErr::FileSystemErr)
    }

    pub fn rename(&self, partition_idx: u8, from: &str, to: &str) -> Result<(), StorageDeviceErr> {
        self.partition
            .rename(&self.dev, partition_idx, from, to)
            .map_err(StorageDeviceErr::FileSystemErr)
    }

    pub fn create_dir(&self, partition_idx: u8, path: &str) -> Result<(), StorageDeviceErr> {
        self.partition
            .create_dir(&self.dev, partition_idx, path)
            .map_err(StorageDeviceErr::FileSystemErr)
    }

    pub fn remove_dir(&self, partition_idx: u8, path: &str) -> Result<(), StorageDeviceErr> {
        self.partition
            .remove_dir(&self.dev, partition_idx, path)
            .map_err(StorageDeviceErr::FileSystemErr)
    }
}

impl Drop for StorageDevice {
    fn drop(&mut self) {
        self.dev.uninstall();
    }
}

impl BlockDevice for BorrowedBlockDevice {
    fn init(&mut self) -> Result<(), IoError> {
        Ok(())
    }

    fn block_size(&self) -> usize {
        self.dev.block_size()
    }

    fn num_blocks(&self) -> u64 {
        self.dev.num_blocks()
    }

    fn read_at(
        &self,
        lba: block_device_api::Lba,
        buf: &mut [core::mem::MaybeUninit<u8>],
    ) -> Result<(), IoError> {
        self.dev.read_at(lba, buf)
    }

    fn write_at(&self, lba: block_device_api::Lba, buf: &[u8]) -> Result<(), IoError> {
        self.dev.write_at(lba, buf)
    }

    fn flush(&self) -> Result<(), IoError> {
        self.dev.flush()
    }

    fn max_io_bytes(&self) -> Result<Option<usize>, IoError> {
        self.dev.max_io_bytes()
    }

    fn is_read_only(&self) -> Result<bool, IoError> {
        self.dev.is_read_only()
    }

    fn uninstall(&self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageDeviceErr {
    IoErr(IoError),
    FileSystemErr(FileSystemErr),
    StillUsed,
}
