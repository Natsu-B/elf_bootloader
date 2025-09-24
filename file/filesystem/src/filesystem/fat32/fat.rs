// File Allocation Table

use alloc::sync::Arc;
use block_device_api::BlockDevice;
use typestate::Le;
use typestate_macro::BytePod;

use crate::FileSystemErr;
use crate::aligned_box::AlignedSliceBox;
use crate::filesystem::fat32::FAT32FileSystem;
use crate::from_io_err;

#[repr(transparent)]
#[derive(Clone, Copy, BytePod)]
pub(crate) struct FAT32FAT(u32);

impl FAT32FAT {
    const MASK: u32 = 0x0FFF_FFFF;
}

pub(crate) struct FAT32FATIter<'a> {
    block_device: &'a Arc<dyn BlockDevice>,
    file_system: &'a FAT32FileSystem,
    next_cluster: Option<u32>,
    fat_cache: Option<AlignedSliceBox<Le<u32>>>,
}

impl<'a> FAT32FATIter<'a> {
    const ALLOCATE_SIZE: u64 = 4;
    pub fn new(
        block_device: &'a Arc<dyn BlockDevice>,
        file_system: &'a FAT32FileSystem,
        first_cluster: u32,
    ) -> Self {
        Self {
            block_device,
            file_system,
            next_cluster: Some(first_cluster),
            fat_cache: None,
        }
    }
}

impl<'a> Iterator for FAT32FATIter<'a> {
    type Item = Result<u64, FileSystemErr>;

    fn next(&mut self) -> Option<Self::Item> {
        // TODO BPB_ExtFlags
        let Some(cluster) = self.next_cluster else {
            return None;
        };
        let fat_relative = (cluster as u64 * size_of::<FAT32FAT>() as u64
            / self.file_system.bytes_per_sector as u64)
            .saturating_sub(self.file_system.sectors_per_cluster as u64 * Self::ALLOCATE_SIZE / 2);
        let fat_lba = self.file_system.hidden_sector as u64
            + self.file_system.reserved_sectors as u64
            + fat_relative;
        let mut fat_buf = self.fat_cache.take();

        let fat = if let Some(ref fat_cache) = fat_buf {
            &**fat_cache
        } else {
            let mut data = AlignedSliceBox::<Le<u32>>::new_uninit_with_align(
                self.block_device.block_size()
                    * self.file_system.sectors_per_cluster as usize
                    * Self::ALLOCATE_SIZE as usize,
                4,
            )
            .unwrap();
            if let Err(e) = self
                .block_device
                .read_at(fat_lba, data.deref_uninit_u8_mut())
            {
                return Some(Err(from_io_err(e)));
            }
            let data = unsafe { data.assume_init() };
            fat_buf = Some(data);
            &**fat_buf.as_ref().unwrap()
        };
        let current_fat_ptr = cluster as u64 * size_of::<FAT32FAT>() as u64
            - fat_relative * self.file_system.bytes_per_sector as u64;
        let current_fat =
            fat[current_fat_ptr as usize / size_of::<FAT32FAT>()].read() & FAT32FAT::MASK;
        match current_fat {
            0x000_0000 | 0x0FFF_FFF7 => return Some(Err(FileSystemErr::Corrupted)),
            x if x >= 0x0FFF_FFF8 => self.next_cluster = None,
            x => {
                self.next_cluster = Some(x);
                // cache
                let cache_start = fat_relative * self.file_system.bytes_per_sector as u64;
                let next = x as u64 * size_of::<FAT32FAT>() as u64;
                if cache_start <= next
                    && next
                        < cache_start
                            + self.file_system.sectors_per_cluster as u64
                                * self.block_device.block_size() as u64
                                * Self::ALLOCATE_SIZE
                {
                    self.fat_cache = fat_buf;
                } else {
                    self.fat_cache = None;
                }
            }
        }
        Some(Ok(self.file_system.hidden_sector as u64
            + self.file_system.reserved_sectors as u64
            + self.file_system.num_fats as u64
                * self.file_system.sectors_per_fat as u64
            + (cluster as u64 - 2)
                * self.file_system.sectors_per_cluster as u64))
    }
}
