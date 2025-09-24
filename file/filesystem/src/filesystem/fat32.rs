use alloc::boxed::Box;
use alloc::sync::Arc;
use block_device_api::BlockDevice;
use core::mem::MaybeUninit;
use typestate::Le;
use typestate::Unaligned;
use typestate::unalign_read;

use crate::FileSystemErr;
use crate::aligned_box::AlignedSliceBox;
use crate::filesystem::DirMeta;
use crate::filesystem::FileHandle;
use crate::filesystem::FileSystemTrait;
use crate::filesystem::OpenOptions;
use crate::filesystem::fat32::fat::FAT32FATIter;
use crate::filesystem::fat32::sector::FAT32BootSector;
use crate::filesystem::fat32::sector::FAT32ByteDirectoryEntry;
use crate::filesystem::fat32::sector::FAT32DirectoryEntryAttribute;
use crate::filesystem::fat32::sector::FAT32LongDirectoryEntry;
use crate::from_io_err;
mod fat;
pub(crate) mod sector;

pub(crate) struct FAT32FileSystem {
    /// Bytes per sector (BPB_BytsPerSec).
    /// Almost always 512, 1024, 2048, or 4096.
    bytes_per_sector: u16,

    /// Number of sectors per cluster (BPB_SecPerClus).
    /// Defines the cluster size in sectors; fundamental for cluster <-> sector calculations.
    sectors_per_cluster: u8,

    /// Number of reserved sectors (BPB_RsvdSecCnt).
    /// Includes the boot sector, FSInfo, backup boot sector, and unused reserved area.
    reserved_sectors: u16,

    /// Number of FAT copies (BPB_NumFATs).
    /// Usually 2 for redundancy.
    num_fats: u8,

    /// Sectors per FAT (BPB_FATSz32).
    /// The length of one FAT table in sectors.
    sectors_per_fat: u32,

    /// The starting cluster number of the root directory (BPB_RootClus).
    /// Typically cluster #2.
    root_dir_cluster: u32,

    /// Hidden sectors before this volume (BPB_HiddSec).
    /// Used to translate volume-relative LBAs into absolute disk LBAs.
    hidden_sector: u32,

    /// Total sector count of the volume (BPB_TotSec16/32).
    /// Used for volume size calculations and sanity checks.
    total_sectors: u32,

    /// Total number of clusters in the data region.
    /// Used to validate FAT type (e.g., FAT12, FAT16, FAT32) and for boundary checks.
    count_of_clusters: u32,

    /// first data sector
    first_data_sectors: u64,
}

impl FAT32FileSystem {
    pub(crate) fn new(
        block_size: usize,
        boot_sector: &FAT32BootSector,
        count_of_clusters: u32,
        first_sector: u64,
    ) -> Result<Self, FileSystemErr> {
        let bytes_per_sector = unalign_read!(boot_sector.bpb_bytes_per_sec => Le<Unaligned<u16>>);
        match bytes_per_sector {
            512 | 1024 | 2048 | 4096 => {}
            _ => return Err(FileSystemErr::Corrupted),
        }
        let sectors_per_cluster = boot_sector.bpb_sec_per_clus;
        match sectors_per_cluster {
            1 | 2 | 4 | 8 | 16 | 32 | 64 | 128 => {}
            _ => return Err(FileSystemErr::Corrupted),
        }
        let reserved_sectors = unalign_read!(boot_sector.bpb_rsvd_sec_cnt => Le<Unaligned <u16>>);
        let num_fats = boot_sector.bpb_num_fats;
        let sectors_per_fat = unalign_read!(boot_sector.bpb_fat_sz_32 => Le<Unaligned <u32>>);
        let hidden_sector = unalign_read!(boot_sector.bpb_hidd_sec => Le<Unaligned<u32>>);
        if bytes_per_sector != block_size as u16 {
            return Err(FileSystemErr::Corrupted); // hidden_sector may corrupted?
        }
        if hidden_sector as u64 != first_sector || num_fats == 0 || reserved_sectors == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        Ok(Self {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            sectors_per_fat,
            root_dir_cluster: unalign_read!(boot_sector.bpb_root_clus => Le::<Unaligned<u32>>),
            hidden_sector,
            total_sectors: unalign_read!(boot_sector.bpb_tot_sec_32 => Le::<Unaligned<u32>>),
            count_of_clusters,
            first_data_sectors: reserved_sectors as u64
                + (num_fats as u64 * sectors_per_fat as u64),
        })
    }

    fn is_encode_83(name: &str) -> Result<Option<(&str, &str)>, FileSystemErr> {
        let mut ret8: MaybeUninit<&str> = MaybeUninit::uninit();
        let mut ret3: &str = core::default::Default::default();
        if !name.is_ascii() {
            return Err(FileSystemErr::InvalidInput);
        }

        for i in name.bytes() {
            if !match i {
                i if i.is_ascii_alphanumeric() => true,
                b'.' | b'$' | b'%' | b'`' | b'-' | b'_' | b'@' | b'~' | b'\'' | b'!' | b'('
                | b')' | b'{' | b'}' | b'^' | b'#' | b'&' => true,
                b'+' | b',' | b';' | b'=' | b'[' | b']' | b' ' => false,
                _ => return Err(FileSystemErr::InvalidInput),
            } {
                return Ok(None);
            }
        }

        for (i, chars) in name.split('.').enumerate() {
            match i {
                0 => {
                    if chars.is_empty() || chars.len() > 8 {
                        return Ok(None);
                    }
                    ret8.write(chars);
                }
                1 => {
                    if chars.is_empty() || chars.len() > 3 {
                        return Ok(None);
                    }
                    ret3 = chars;
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(unsafe { (ret8.assume_init(), ret3) }))
    }

    fn calculate_next_dir(sde: &FAT32ByteDirectoryEntry) -> DirMeta {
        let cluster =
            ((sde.dir_fst_clus_hi.read() as u32) << 16) | sde.dir_fst_clus_lo.read() as u32;
        DirMeta {
            is_dir: sde.dir_attr & FAT32DirectoryEntryAttribute::ATTR_DIRECTORY
                == FAT32DirectoryEntryAttribute::ATTR_DIRECTORY,
            is_readonly: sde.dir_attr & FAT32DirectoryEntryAttribute::ATTR_READ_ONLY
                == FAT32DirectoryEntryAttribute::ATTR_READ_ONLY,
            first_cluster: cluster,
            file_size: sde.dir_file_size.read(),
        }
    }

    /// compare utf16 and ascii
    /// when return false or utf16 has null terminator, ascii &str pointer is undefined
    fn compare_utf16_and_ascii(utf16: &[u8], ascii: &mut &str) -> Result<bool, FileSystemErr> {
        let mut chunks = match utf16.chunks_exact(2) {
            c if c.remainder().is_empty() => c,
            _ => unreachable!(),
        };

        let ascii_bytes = ascii.as_bytes();
        let mut idx = 0;

        for pair in &mut chunks {
            let u = u16::from_le_bytes([pair[0], pair[1]]);

            match u {
                0x0000 => {
                    *ascii = core::str::from_utf8(&ascii_bytes[idx..]).unwrap();
                    return Ok(idx == ascii_bytes.len());
                }
                0xFFFF => {
                    continue;
                }
                0x0001..=0x007F => {
                    if idx >= ascii_bytes.len() {
                        *ascii = core::str::from_utf8(&ascii_bytes[idx..]).unwrap();
                        return Ok(false);
                    }
                    let a = ascii_bytes[idx];
                    let u8v = u as u8;
                    if !a.eq_ignore_ascii_case(&u8v) {
                        *ascii = core::str::from_utf8(&ascii_bytes[idx..]).unwrap();
                        return Ok(false);
                    }
                    idx += 1;
                }
                _ => {
                    return Err(FileSystemErr::UnsupportedFileName);
                }
            }
        }
        *ascii = core::str::from_utf8(&ascii_bytes[idx..]).unwrap();
        Ok(true)
    }

    fn search_file_name_with_cluster_dir(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        file_name: &str,
    ) -> Result<Option<DirMeta>, FileSystemErr> {
        let is_short_name = Self::is_encode_83(file_name)?;

        for lba in FAT32FATIter::new(block_device, self, dir_cluster) {
            let lba = lba?;
            let mut data = AlignedSliceBox::<u8>::new_uninit_with_align(
                self.sectors_per_cluster as usize * block_device.block_size(),
                2,
            )
            .unwrap();
            block_device.read_at(lba, &mut data).map_err(from_io_err)?;
            let data = unsafe { data.assume_init() };
            let mut lde_num = 0;
            'outer: for i in (0..self.sectors_per_cluster as usize * block_device.block_size())
                .step_by(size_of::<FAT32ByteDirectoryEntry>())
            {
                let entry_ptr = unsafe { data.as_ptr().add(i) };
                let name0 = unsafe { *entry_ptr };

                if name0 == 0x00 {
                    break;
                }
                if name0 == 0xE5 {
                    lde_num = 0;
                    continue;
                }

                if FAT32DirectoryEntryAttribute::is_sde(data.as_ptr() as usize + i) {
                    let lde = lde_num;
                    lde_num = 0;
                    let sde = unsafe {
                        &*((data.as_ptr() as usize + i) as *const FAT32ByteDirectoryEntry)
                    };
                    if sde.dir_attr & FAT32DirectoryEntryAttribute::ATTR_VOLUME_ID
                        == FAT32DirectoryEntryAttribute::ATTR_VOLUME_ID
                    {
                        continue;
                    }
                    if let Some((name, extension)) = is_short_name {
                        for i in 0..8 {
                            if let Some(char) = name.as_bytes().get(i) {
                                if char.to_ascii_uppercase() != sde.dir_name[i] {
                                    continue 'outer;
                                }
                            } else if sde.dir_name[i] != b' ' {
                                continue 'outer;
                            }
                        }
                        for i in 0..3 {
                            if let Some(char) = extension.as_bytes().get(i) {
                                if char.to_ascii_uppercase() != sde.dir_name[i + 8] {
                                    continue 'outer;
                                }
                            } else if sde.dir_name[i + 8] != b' ' {
                                continue 'outer;
                            }
                        }
                        return Ok(Some(Self::calculate_next_dir(sde)));
                    } else {
                        if lde == 0 {
                            continue;
                        }
                        if lde != file_name.len().div_ceil(13) {
                            continue;
                        }
                        let mut check_sum: u8 = 0;
                        for i in 0..11 {
                            check_sum = check_sum.rotate_right(1).wrapping_add(sde.dir_name[i]);
                        }
                        let mut file_name = file_name;
                        for j in 0..lde {
                            let lde_ref = unsafe {
                                &*((data.as_ptr() as usize + i
                                    - (j + 1) * size_of::<FAT32LongDirectoryEntry>())
                                    as *const FAT32LongDirectoryEntry)
                            };
                            let ord = lde_ref.ldir_ord;
                            let seq = ord & 0x3F;
                            let last = (ord & 0x40) != 0;
                            let expected = (j + 1) as u8;

                            if seq != expected {
                                return Err(FileSystemErr::Corrupted);
                            }
                            if j == lde - 1 {
                                if !last {
                                    return Err(FileSystemErr::Corrupted);
                                }
                            } else if last {
                                return Err(FileSystemErr::Corrupted);
                            }
                            if lde_ref.ldir_chksum != check_sum || lde_ref.ldir_fst_clus_lo != 0 {
                                return Err(FileSystemErr::Corrupted);
                            }
                            if !Self::compare_utf16_and_ascii(&lde_ref.ldir_name1, &mut file_name)?
                            {
                                continue 'outer;
                            }
                            if file_name.is_empty()
                                || file_name.bytes().all(|c| c == b' ' || c == b'.')
                            {
                                if j == lde - 1 {
                                    return Ok(Some(Self::calculate_next_dir(sde)));
                                } else {
                                    continue 'outer;
                                }
                            }
                            if !Self::compare_utf16_and_ascii(&lde_ref.ldir_name2, &mut file_name)?
                            {
                                continue 'outer;
                            }
                            if file_name.is_empty()
                                || file_name.bytes().all(|c| c == b' ' || c == b'.')
                            {
                                if j == lde - 1 {
                                    return Ok(Some(Self::calculate_next_dir(sde)));
                                } else {
                                    continue 'outer;
                                }
                            }
                            if !Self::compare_utf16_and_ascii(&lde_ref.ldir_name3, &mut file_name)?
                            {
                                continue 'outer;
                            }
                            if file_name.is_empty()
                                || file_name.bytes().all(|c| c == b' ' || c == b'.')
                            {
                                if j == lde - 1 {
                                    return Ok(Some(Self::calculate_next_dir(sde)));
                                } else {
                                    continue 'outer;
                                }
                            }
                        }
                    }
                } else {
                    lde_num += 1;
                }
            }
        }
        Ok(None)
    }
}

impl FileSystemTrait for FAT32FileSystem {
    fn open(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        file_system: &Arc<dyn FileSystemTrait>,
        path: &str,
        opts: &super::OpenOptions,
    ) -> Result<FileHandle, FileSystemErr> {
        let mut path = path.chars();
        match path.next() {
            Some('/') => {}
            Some(_) => return Err(FileSystemErr::NotRootDir),
            None => return Err(FileSystemErr::InvalidInput),
        }
        let mut dir_clusters = self.root_dir_cluster;
        let mut meta = DirMeta {
            is_readonly: false,
            is_dir: false,
            first_cluster: 0,
            file_size: 0,
        };
        for dir_name in path.as_str().split('/') {
            if dir_name.is_empty() {
                return Err(FileSystemErr::InvalidInput);
            }
            let Some(dir_meta) =
                self.search_file_name_with_cluster_dir(block_device, dir_clusters, dir_name)?
            else {
                return Err(FileSystemErr::NotFound);
            };
            dir_clusters = dir_meta.first_cluster;
            meta = dir_meta;
        }
        if meta.is_dir {
            return Err(FileSystemErr::IsDir);
        }
        if *opts == OpenOptions::Write
            && (meta.is_readonly || block_device.is_read_only().map_err(from_io_err)?)
        {
            return Err(FileSystemErr::ReadOnly);
        }
        Ok(FileHandle {
            dev_handle: Arc::downgrade(block_device),
            file_handle: Arc::downgrade(file_system),
            meta,
            opts: *opts,
        })
    }

    fn remove_file(&self, path: &str) -> Result<(), FileSystemErr> {
        todo!()
    }

    fn copy(&self, from: &str, to: &str) -> Result<(), FileSystemErr> {
        todo!()
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FileSystemErr> {
        todo!()
    }

    fn create_dir(&self, path: &str) -> Result<(), FileSystemErr> {
        todo!()
    }

    fn remove_dir(&self, path: &str) -> Result<(), FileSystemErr> {
        todo!()
    }

    fn read(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        align: usize,
        meta: &DirMeta,
    ) -> Result<AlignedSliceBox<u8>, FileSystemErr> {
        let mut data =
            AlignedSliceBox::new_uninit_with_align(meta.file_size as usize, align).unwrap();
        let len = self.read_at(block_device, 0, data.deref_uninit_u8_mut(), meta)?;
        assert_eq!(data.len() as u64, len);
        Ok(unsafe { data.assume_init() })
    }

    fn read_at(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        offset: u64,
        buf: &mut [MaybeUninit<u8>],
        meta: &DirMeta,
    ) -> Result<u64, FileSystemErr> {
        let file_size = meta.file_size as u64;
        let bs = block_device.block_size();
        let spc = self.sectors_per_cluster as usize;
        let bpc = (bs * spc) as u64;

        if offset > file_size {
            return Err(FileSystemErr::InvalidInput);
        }
        let max_read = (file_size - offset) as usize;
        if buf.len() > max_read {
            return Err(FileSystemErr::TooBigBuffer);
        }
        let to_read = buf.len();

        let start_cluster = (offset / bpc) as usize;
        let cluster_off = (offset % bpc) as usize;
        let start_sector_off = cluster_off / bs;
        let start_byte_in_sector = cluster_off % bs;

        let total_bytes_from_first_sector = start_byte_in_sector + to_read;
        let sectors_needed = total_bytes_from_first_sector.div_ceil(bs);

        let mut tmp = Box::new_uninit_slice(sectors_needed * bs);

        let mut sectors_remaining = sectors_needed;
        let mut tmp_ptr_sectors = 0usize;

        for (i, lba) in FAT32FATIter::new(block_device, self, meta.first_cluster).enumerate() {
            let lba = lba?;
            if i < start_cluster {
                continue;
            }
            if sectors_remaining == 0 {
                break;
            }

            let first_sector_in_this_cluster = if i == start_cluster {
                start_sector_off
            } else {
                0
            };

            let can_read_in_this_cluster = spc - first_sector_in_this_cluster;
            let read_sectors = can_read_in_this_cluster.min(sectors_remaining);

            let byte_off = tmp_ptr_sectors * bs;
            let byte_len = read_sectors * bs;
            block_device
                .read_at(
                    lba + first_sector_in_this_cluster as u64,
                    &mut tmp[byte_off..byte_off + byte_len],
                )
                .map_err(from_io_err)?;

            tmp_ptr_sectors += read_sectors;
            sectors_remaining -= read_sectors;
        }

        if sectors_remaining != 0 {
            return Err(FileSystemErr::IncompleteRead);
        }

        let start = start_byte_in_sector;
        let end = start + to_read;
        let src = unsafe { core::slice::from_raw_parts(tmp.as_ptr() as *const u8, tmp.len()) };
        let src = &src[start..end];

        let dst =
            unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len()) };
        dst.copy_from_slice(src);

        Ok(to_read as u64)
    }
}
