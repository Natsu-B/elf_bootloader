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
pub(crate) struct FAT32FAT(Le<u32>);

impl FAT32FAT {
    const MASK: u32 = 0x0FFF_FFFF;
}

pub(crate) struct FAT32FATIter<'a> {
    block_device: &'a Arc<dyn BlockDevice>,
    file_system: &'a FAT32FileSystem,
    next_cluster: Option<u32>,
    fat_cache: Option<(
        AlignedSliceBox<FAT32FAT>,
        u64, /* start sector */
        u64, /* sector len */
    )>,
}

impl<'a> FAT32FATIter<'a> {
    const ALLOCATE_SIZE: u64 = 16;
    pub fn new(
        block_device: &'a Arc<dyn BlockDevice>,
        file_system: &'a FAT32FileSystem,
        first_cluster: u32,
    ) -> Self {
        Self {
            block_device,
            file_system,
            next_cluster: if first_cluster == 0 {
                None // for 0 len file
            } else {
                Some(first_cluster)
            },
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
        let spf = self.file_system.sectors_per_fat as u64;
        let bps = self.file_system.bytes_per_sector as u64;

        let entry_byte = cluster as u64 * size_of::<FAT32FAT>() as u64;
        let entry_sector = entry_byte / bps;
        let half = Self::ALLOCATE_SIZE / 2;

        debug_assert_eq!(entry_byte % size_of::<FAT32FAT>() as u64, 0);
        let mut fat_relative = entry_sector.saturating_sub(half);

        let mut read_sectors = Self::ALLOCATE_SIZE.min(spf);

        if fat_relative + read_sectors > spf {
            if read_sectors > spf {
                fat_relative = 0;
                read_sectors = spf;
            } else {
                fat_relative = spf - read_sectors;
            }
        }

        let fat_lba = self.file_system.hidden_sector as u64
            + self.file_system.reserved_sectors as u64
            + fat_relative;

        let allocate_size = (read_sectors * bps) as usize;
        let mut fat_buf = self.fat_cache.take();

        let fat = if let Some(ref fat_cache) = fat_buf {
            fat_cache
        } else {
            let mut data = AlignedSliceBox::<FAT32FAT>::new_uninit_with_align(
                allocate_size / size_of::<FAT32FAT>(),
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
            fat_buf = Some((data, fat_relative, read_sectors));
            &fat_buf.as_ref().unwrap()
        };
        let cache_start = fat.1 * bps;
        let cache_end = cache_start + fat.2 * bps;
        let current_fat_ptr = entry_byte - cache_start;

        debug_assert!(((entry_byte - cache_start) as usize / size_of::<FAT32FAT>()) < fat.0.len());
        debug_assert_eq!(
            fat.0.len() * core::mem::size_of::<FAT32FAT>(),
            (fat.2 * bps) as usize
        );

        let current_fat = fat.0[current_fat_ptr as usize / size_of::<FAT32FAT>()]
            .0
            .read()
            & FAT32FAT::MASK;
        match current_fat {
            0x0000_0000 | 0x0000_0001 | 0x0FFF_FFF7 | 0x0FFF_FFF0..=0x0FFF_FFF6 => {
                return Some(Err(FileSystemErr::Corrupted));
            }
            x if x >= 0x0FFF_FFF8 => self.next_cluster = None,
            x if x > self.file_system.count_of_clusters + 1 => {
                return Some(Err(FileSystemErr::Corrupted));
            }
            x => {
                self.next_cluster = Some(x);
                // cache
                let next = x as u64 * size_of::<FAT32FAT>() as u64;
                if cache_start <= next && next < cache_end {
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
