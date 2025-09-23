use core::ffi::CStr;
use core::mem::MaybeUninit;
use core::ops::ControlFlow;
use core::ptr::addr_of;
use core::usize;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::sync::Weak;
use block_device_api::BlockDevice;

use crate::FileSystemErr;
use crate::PartitionIndex;
use crate::aligned_box::AlignedSliceBox;
use crate::filesystem::fat32::FAT32FileSystem;
use crate::filesystem::fat32::sector::FAT32BootSector;
use crate::from_io_err;

pub(crate) mod fat32;

pub(crate) mod file_system {
    use core::panic;

    use typestate::Le;
    use typestate::Unaligned;
    use typestate::unalign_read;

    use super::*;

    pub fn new(
        block_device: &Arc<dyn BlockDevice>,
        start_sector: u64,
        total_sector: u64,
    ) -> Result<Arc<dyn FileSystemTrait>, FileSystemErr> {
        let mut boot_sector: Box<[MaybeUninit<u8>]> =
            Box::new_uninit_slice(block_device.block_size());
        block_device
            .read_at(start_sector, &mut boot_sector)
            .map_err(from_io_err)?;
        // The device promises the buffer is fully initialized on success
        let boot_sector_bytes: Box<[u8]> = unsafe { boot_sector.assume_init() };
        let fat32_boot_sector = unsafe { &*(boot_sector_bytes.as_ptr() as *const FAT32BootSector) };
        // check boot signature (using unaligned-safe wrapper)
        if unalign_read!(fat32_boot_sector.bs_boot_sign =>Le<Unaligned<u16>>)
            != PartitionIndex::BOOT_SIGNATURE
        {
            return Err(FileSystemErr::UnsupportedFileSystem);
        }
        // assume the partition is fat
        if unalign_read!(fat32_boot_sector.bpb_fat_sz16 => Le<Unaligned<u16>>) != 0
            || unalign_read!(fat32_boot_sector.bpb_tot_sec_16 => Le<Unaligned<u16>>) != 0
        {
            // partition is fat12 or fat16
            return Err(FileSystemErr::UnsupportedFileSystem);
        }
        let root_dir_sectors = ((unalign_read!(fat32_boot_sector.bpb_root_ent_cnt => Le<Unaligned<u16>>)
            as u32
            * 32)
            + (unalign_read!(fat32_boot_sector.bpb_bytes_per_sec => Le<Unaligned<u16>>) as u32
                - 1))
            / unalign_read!(fat32_boot_sector.bpb_bytes_per_sec => Le<Unaligned<u16>>) as u32;
        let data_sec = unalign_read!(fat32_boot_sector.bpb_tot_sec_32 => Le<Unaligned<u32>>)
            - (unalign_read!(fat32_boot_sector.bpb_rsvd_sec_cnt => Le<Unaligned<u16>>) as u32
                + (fat32_boot_sector.bpb_num_fats as u32
                    * unalign_read!(fat32_boot_sector.bpb_fat_sz_32 => Le<Unaligned<u32>>))
                + root_dir_sectors);
        let count_of_clusters = data_sec / fat32_boot_sector.bpb_sec_per_clus as u32;
        if count_of_clusters < 65525 {
            // partition is fat12 or fat16
            return Err(FileSystemErr::UnsupportedFileSystem);
        }
        // assume partition is fat32
        let fat32_filesystem = FAT32FileSystem::new(
            block_device.block_size(),
            fat32_boot_sector,
            count_of_clusters,
            start_sector,
        )?;
        Ok(Arc::new(fat32_filesystem))
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum OpenOptions {
    Read,
    Write,
}

pub(crate) trait FileSystemTrait {
    // file
    fn open(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        file_system: &Arc<dyn FileSystemTrait>,
        path: &str,
        opts: &OpenOptions,
    ) -> Result<FileHandle, FileSystemErr>;
    fn remove_file(&self, path: &str) -> Result<(), FileSystemErr>;
    fn copy(&self, from: &str, to: &str) -> Result<(), FileSystemErr>;
    fn rename(&self, from: &str, to: &str) -> Result<(), FileSystemErr>;

    // dir
    fn create_dir(&self, path: &str) -> Result<(), FileSystemErr>;
    fn remove_dir(&self, path: &str) -> Result<(), FileSystemErr>;

    fn read(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        align: usize,
        meta: &DirMeta,
    ) -> Result<AlignedSliceBox<u8>, FileSystemErr>;

    fn read_at(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        offset: u64,
        buf: &mut [MaybeUninit<u8>],
        meta: &DirMeta,
    ) -> Result<u64, FileSystemErr>;
}

pub struct DirMeta {
    is_dir: bool,
    is_readonly: bool,
    first_cluster: u32,
    file_size: u32,
}

pub struct FileHandle {
    dev_handle: Weak<dyn BlockDevice>,
    file_handle: Weak<dyn FileSystemTrait>,
    meta: DirMeta,
    opts: OpenOptions,
}

impl FileHandle {
    pub fn read(&self, align: usize) -> Result<AlignedSliceBox<u8>, FileSystemErr> {
        let Some(dev) = self.dev_handle.upgrade() else {
            return Err(FileSystemErr::Closed);
        };
        let Some(file) = self.file_handle.upgrade() else {
            return Err(FileSystemErr::Closed);
        };
        file.read(&dev, align, &self.meta)
    }

    pub fn read_at(&self, offset: u64, buf: &mut [MaybeUninit<u8>]) -> Result<u64, FileSystemErr> {
        let Some(dev) = self.dev_handle.upgrade() else {
            return Err(FileSystemErr::Closed);
        };
        let Some(file) = self.file_handle.upgrade() else {
            return Err(FileSystemErr::Closed);
        };
        file.read_at(&dev, offset, buf, &self.meta)
    }

    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<u64, FileSystemErr> {
        todo!()
    }

    pub fn size(&self) -> Result<u64, FileSystemErr> {
        Ok(self.meta.file_size as u64)
    }

    pub fn flush(&self) -> Result<(), FileSystemErr> {
        let Some(dev) = self.dev_handle.upgrade() else {
            return Err(FileSystemErr::Closed);
        };
        dev.flush().map_err(from_io_err)
    }
}
