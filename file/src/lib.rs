#![no_std]

extern crate alloc;
use alloc::boxed::Box;
use alloc::sync::Arc;
use block_device::VirtIoBlk;
use block_device_api::BlockDevice;
use block_device_api::IoError;
use filesystem::FileSystemErr;
use filesystem::PartitionIndex;

pub use filesystem::filesystem::FileHandle;
pub use filesystem::filesystem::OpenOptions;

pub struct StorageDevice {
    dev: Arc<dyn BlockDevice>,
    partition: PartitionIndex,
}

impl StorageDevice {
    pub fn new_virtio(mmio: usize) -> Result<Self, StorageDeviceErr> {
        let mut io = VirtIoBlk::new(mmio).map_err(error_from_ioerror)?;
        io.init().map_err(error_from_ioerror)?;
        let dev = Arc::new(io);
        Ok(Self {
            partition: PartitionIndex::new(dev.as_ref()).map_err(error_from_file_system_err)?,
            dev,
        })
    }

    pub fn open(
        &self,
        partition_idx: u8,
        path: &str,
        opts: &OpenOptions,
    ) -> Result<FileHandle, StorageDeviceErr> {
        self.partition
            .open(&self.dev, partition_idx, path, opts)
            .map_err(error_from_file_system_err)
    }
}

impl Drop for StorageDevice {
    fn drop(&mut self) {
        self.dev.uninstall();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageDeviceErr {
    IoErr(IoError),
    FileSystemErr(FileSystemErr),
    StillUsed,
}

fn error_from_ioerror(err: IoError) -> StorageDeviceErr {
    StorageDeviceErr::IoErr(err)
}

fn error_from_file_system_err(err: FileSystemErr) -> StorageDeviceErr {
    StorageDeviceErr::FileSystemErr(err)
}
