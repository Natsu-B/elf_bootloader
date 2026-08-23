use crate::FileSystemErr;
use crate::filesystem::DirMeta;
use crate::filesystem::FileHandle;
use crate::filesystem::FileSystemTrait;
use crate::filesystem::OpenOptions;
use crate::filesystem::fat::FatType;
use crate::filesystem::fat::FatVolume;
use crate::filesystem::fat32::fat::FAT32FAT;
use crate::filesystem::fat32::fat::FAT32FATIter;
use crate::filesystem::fat32::sector::FAT32ByteDirectoryEntry;
use crate::filesystem::fat32::sector::FAT32DirectoryEntryAttribute;
use crate::filesystem::fat32::sector::FAT32LongDirectoryEntry;
use crate::from_io_err;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use allocator::AlignedSliceBox;
use block_device_api::BlockDevice;
use core::cmp;
use core::convert::TryInto;
use core::mem::MaybeUninit;
use core::mem::size_of;
mod fat;
pub(crate) mod sector;

pub(crate) struct FAT32FileSystem {
    volume: FatVolume,
    root_dir_cluster: u32,
}

#[derive(Clone, Copy)]
struct DirEntrySpan {
    start_cluster: u32,
    start_offset: usize,
}

impl FAT32FileSystem {
    pub(crate) fn new(volume: FatVolume) -> Result<Self, FileSystemErr> {
        if volume.kind != FatType::Fat32 {
            return Err(FileSystemErr::UnsupportedFileSystem);
        }
        let Some(root) = volume.root_dir_cluster else {
            return Err(FileSystemErr::Corrupted);
        };
        Ok(Self {
            volume,
            root_dir_cluster: root,
        })
    }

    fn cluster_size_bytes(&self) -> usize {
        self.volume.cluster_size_bytes()
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

    fn calculate_next_dir(
        sde: &FAT32ByteDirectoryEntry,
        entry_lba: u64,
        entry_offset: u16,
    ) -> DirMeta {
        let cluster =
            ((sde.dir_fst_clus_hi.read() as u32) << 16) | sde.dir_fst_clus_lo.read() as u32;
        DirMeta {
            is_dir: sde.dir_attr & FAT32DirectoryEntryAttribute::ATTR_DIRECTORY
                == FAT32DirectoryEntryAttribute::ATTR_DIRECTORY,
            is_readonly: sde.dir_attr & FAT32DirectoryEntryAttribute::ATTR_READ_ONLY
                == FAT32DirectoryEntryAttribute::ATTR_READ_ONLY,
            first_cluster: cluster,
            file_size: sde.dir_file_size.read(),
            entry_lba,
            entry_offset,
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
        let cluster_bytes = self.cluster_size_bytes();
        let sector_bytes = self.volume.bytes_per_sector as usize;

        // Track long directory entry count across clusters so LFN sequences that
        // span a cluster boundary can still be matched with their short entry.
        let mut lde_num = 0;
        for cluster in FAT32FATIter::new(block_device, self, dir_cluster) {
            let cluster = cluster?;
            let mut data = AlignedSliceBox::<u8>::new_uninit_with_align(cluster_bytes, 2).unwrap();
            block_device
                .read_at(cluster.lba, &mut data)
                .map_err(from_io_err)?;
            let data = unsafe { data.assume_init() };
            'outer: for entry_off in
                (0..cluster_bytes).step_by(size_of::<FAT32ByteDirectoryEntry>())
            {
                let entry_ptr = unsafe { data.as_ptr().add(entry_off) };
                let name0 = unsafe { *entry_ptr };

                if name0 == 0x00 {
                    break;
                }
                if name0 == 0xE5 {
                    lde_num = 0;
                    continue;
                }

                if FAT32DirectoryEntryAttribute::is_sde(data.as_ptr() as usize + entry_off) {
                    let lde = lde_num;
                    lde_num = 0;
                    let sde = unsafe {
                        &*((data.as_ptr() as usize + entry_off) as *const FAT32ByteDirectoryEntry)
                    };
                    let entry_lba = cluster.lba + (entry_off / sector_bytes) as u64;
                    let entry_offset = (entry_off % sector_bytes) as u16;
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
                        return Ok(Some(Self::calculate_next_dir(sde, entry_lba, entry_offset)));
                    }
                    if lde == 0 {
                        continue;
                    }
                    let required_entries = file_name
                        .len()
                        .checked_add(13)
                        .ok_or(FileSystemErr::TooBigBuffer)?
                        / 13;
                    if lde != required_entries {
                        continue;
                    }
                    let mut check_sum: u8 = 0;
                    for i in 0..11 {
                        check_sum = check_sum.rotate_right(1).wrapping_add(sde.dir_name[i]);
                    }
                    let mut long_name = file_name;
                    for j in 0..lde {
                        let lde_ref = unsafe {
                            &*((data.as_ptr() as usize + entry_off
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
                        if !Self::compare_utf16_and_ascii(&lde_ref.ldir_name1, &mut long_name)? {
                            continue 'outer;
                        }
                        if long_name.is_empty() || long_name.bytes().all(|c| c == b' ' || c == b'.')
                        {
                            if j == lde - 1 {
                                return Ok(Some(Self::calculate_next_dir(
                                    sde,
                                    entry_lba,
                                    entry_offset,
                                )));
                            }
                        }
                        if !Self::compare_utf16_and_ascii(&lde_ref.ldir_name2, &mut long_name)? {
                            continue 'outer;
                        }
                        if long_name.is_empty() || long_name.bytes().all(|c| c == b' ' || c == b'.')
                        {
                            if j == lde - 1 {
                                return Ok(Some(Self::calculate_next_dir(
                                    sde,
                                    entry_lba,
                                    entry_offset,
                                )));
                            }
                        }
                        if !Self::compare_utf16_and_ascii(&lde_ref.ldir_name3, &mut long_name)? {
                            continue 'outer;
                        }
                        if long_name.is_empty() || long_name.bytes().all(|c| c == b' ' || c == b'.')
                        {
                            if j == lde - 1 {
                                return Ok(Some(Self::calculate_next_dir(
                                    sde,
                                    entry_lba,
                                    entry_offset,
                                )));
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

    fn collect_cluster_chain(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        first_cluster: u32,
    ) -> Result<Vec<u32>, FileSystemErr> {
        let mut chain = Vec::new();
        if first_cluster == 0 {
            return Ok(chain);
        }
        for entry in FAT32FATIter::new(block_device, self, first_cluster) {
            chain.push(entry?.cluster);
        }
        Ok(chain)
    }

    fn ensure_cluster_capacity(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        chain: &mut Vec<u32>,
        needed: usize,
        meta: &mut DirMeta,
    ) -> Result<(), FileSystemErr> {
        if needed == 0 || chain.len() >= needed {
            if meta.first_cluster == 0 && !chain.is_empty() {
                meta.first_cluster = chain[0];
            }
            return Ok(());
        }
        let mut last = chain.last().copied();
        while chain.len() < needed {
            let new_cluster = self.find_free_cluster(block_device)?;
            self.write_fat_entry(block_device, new_cluster, FAT32FAT::END_OF_CHAIN)?;
            if let Some(prev) = last {
                self.write_fat_entry(block_device, prev, new_cluster)?;
            } else if meta.first_cluster != 0 {
                self.write_fat_entry(block_device, meta.first_cluster, new_cluster)?;
            } else {
                meta.first_cluster = new_cluster;
            }
            self.zero_cluster(block_device, new_cluster)?;
            chain.push(new_cluster);
            last = Some(new_cluster);
        }
        if meta.first_cluster == 0 && !chain.is_empty() {
            meta.first_cluster = chain[0];
        }
        Ok(())
    }

    fn find_free_cluster(&self, block_device: &Arc<dyn BlockDevice>) -> Result<u32, FileSystemErr> {
        let mut sector_cache = vec![0u8; self.volume.bytes_per_sector as usize];
        let mut cached_sector = None;
        let max_cluster = self.volume.count_of_clusters + 1;
        for cluster in 2..=max_cluster {
            let entry_byte = cluster as u64 * size_of::<FAT32FAT>() as u64;
            let sector_index = entry_byte / self.volume.bytes_per_sector as u64;
            if cached_sector != Some(sector_index) {
                let lba = self.fat_lba(0, sector_index);
                let mut uninit = slice_as_uninit(&mut sector_cache);
                block_device
                    .read_at(lba, &mut uninit)
                    .map_err(from_io_err)?;
                cached_sector = Some(sector_index);
            }
            let offset = (entry_byte % self.volume.bytes_per_sector as u64) as usize;
            let value = u32::from_le_bytes(
                sector_cache[offset..offset + size_of::<FAT32FAT>()]
                    .try_into()
                    .unwrap(),
            ) & FAT32FAT::MASK;
            if value == 0 {
                return Ok(cluster);
            }
        }
        Err(FileSystemErr::NoSpace)
    }

    fn write_fat_entry(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        cluster: u32,
        value: u32,
    ) -> Result<(), FileSystemErr> {
        let entry_byte = cluster as u64 * size_of::<FAT32FAT>() as u64;
        let sector_index = entry_byte / self.volume.bytes_per_sector as u64;
        let offset = (entry_byte % self.volume.bytes_per_sector as u64) as usize;
        let mut sector_buf = vec![0u8; self.volume.bytes_per_sector as usize];
        for fat_idx in 0..self.volume.num_fats {
            let lba = self.fat_lba(fat_idx, sector_index);
            let mut uninit = slice_as_uninit(&mut sector_buf);
            block_device
                .read_at(lba, &mut uninit)
                .map_err(from_io_err)?;
            sector_buf[offset..offset + size_of::<FAT32FAT>()]
                .copy_from_slice(&(value & FAT32FAT::MASK).to_le_bytes());
            block_device
                .write_at(lba, &sector_buf)
                .map_err(from_io_err)?;
        }
        Ok(())
    }

    fn fat_lba(&self, fat_idx: u8, sector_index: u64) -> u64 {
        self.volume.fat_region_lba()
            + sector_index
            + fat_idx as u64 * self.volume.sectors_per_fat as u64
    }

    fn zero_cluster(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        cluster: u32,
    ) -> Result<(), FileSystemErr> {
        let buf = vec![0u8; self.cluster_size_bytes()];
        let lba = self.volume.cluster_to_lba(cluster)?;
        block_device.write_at(lba, &buf).map_err(from_io_err)
    }

    fn update_dir_entry(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        meta: &DirMeta,
    ) -> Result<(), FileSystemErr> {
        if meta.entry_lba == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        let mut sector = vec![0u8; self.volume.bytes_per_sector as usize];
        let mut uninit = slice_as_uninit(&mut sector);
        block_device
            .read_at(meta.entry_lba, &mut uninit)
            .map_err(from_io_err)?;
        let offset = meta.entry_offset as usize;
        if offset + size_of::<FAT32ByteDirectoryEntry>() > sector.len() {
            return Err(FileSystemErr::Corrupted);
        }
        let entry_ptr = unsafe { sector.as_mut_ptr().add(offset) as *mut FAT32ByteDirectoryEntry };
        let entry = unsafe { &mut *entry_ptr };
        entry.dir_file_size.write(meta.file_size);
        entry.dir_fst_clus_lo.write(meta.first_cluster as u16);
        entry
            .dir_fst_clus_hi
            .write(((meta.first_cluster >> 16) & 0xFFFF) as u16);
        block_device
            .write_at(meta.entry_lba, &sector)
            .map_err(from_io_err)
    }

    fn split_path_components<'a>(path: &'a str) -> Result<Vec<&'a str>, FileSystemErr> {
        let mut chars = path.chars();
        match chars.next() {
            Some('/') => {}
            Some(_) => return Err(FileSystemErr::NotRootDir),
            None => return Err(FileSystemErr::InvalidInput),
        }
        let mut components = Vec::new();
        for comp in chars.as_str().split('/') {
            if comp.is_empty() {
                return Err(FileSystemErr::InvalidInput);
            }
            components.push(comp);
        }
        if components.is_empty() {
            return Err(FileSystemErr::InvalidInput);
        }
        Ok(components)
    }

    fn resolve_directory<'a>(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        components: &[&'a str],
    ) -> Result<(u32, Option<DirMeta>), FileSystemErr> {
        let mut dir_cluster = self.root_dir_cluster;
        let mut dir_meta = None;
        for comp in components {
            let Some(meta) =
                self.search_file_name_with_cluster_dir(block_device, dir_cluster, comp)?
            else {
                return Err(FileSystemErr::NotFound);
            };
            if !meta.is_dir {
                return Err(FileSystemErr::NotDir);
            }
            dir_cluster = meta.first_cluster;
            dir_meta = Some(meta);
        }
        Ok((dir_cluster, dir_meta))
    }

    fn resolve_parent<'a>(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        path: &'a str,
    ) -> Result<(u32, Option<DirMeta>, &'a str), FileSystemErr> {
        let components = Self::split_path_components(path)?;
        let (parent_components, name) = components.split_at(components.len() - 1);
        let name = name[0];
        // Reserved final components are rejected before accessing the parent directory.
        if name == "." || name == ".." {
            return Err(FileSystemErr::InvalidInput);
        }
        let (cluster, meta) = self.resolve_directory(block_device, parent_components)?;
        Ok((cluster, meta, name))
    }

    fn ensure_directory_writable(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        parent_meta: Option<&DirMeta>,
    ) -> Result<(), FileSystemErr> {
        if block_device.is_read_only().map_err(from_io_err)? {
            return Err(FileSystemErr::ReadOnly);
        }
        if let Some(meta) = parent_meta {
            if meta.is_readonly {
                return Err(FileSystemErr::ReadOnly);
            }
        }
        Ok(())
    }

    fn encode_short_name_bytes(name: &str) -> Result<[u8; 11], FileSystemErr> {
        let Some((base, ext)) = Self::is_encode_83(name)? else {
            return Err(FileSystemErr::UnsupportedFileName);
        };
        let mut encoded = [b' '; 11];
        for (idx, ch) in base.bytes().enumerate() {
            encoded[idx] = ch.to_ascii_uppercase();
        }
        for (idx, ch) in ext.bytes().enumerate() {
            encoded[8 + idx] = ch.to_ascii_uppercase();
        }
        Ok(encoded)
    }

    fn build_base_entry(short_name: [u8; 11]) -> FAT32ByteDirectoryEntry {
        let entry = MaybeUninit::<FAT32ByteDirectoryEntry>::zeroed();
        let mut entry = unsafe { entry.assume_init() };
        entry.dir_name = short_name;
        entry.dir_file_size.write(0);
        entry.dir_fst_clus_lo.write(0);
        entry.dir_fst_clus_hi.write(0);
        entry
    }

    fn build_directory_entry(short_name: [u8; 11], cluster: u32) -> FAT32ByteDirectoryEntry {
        let mut entry = Self::build_base_entry(short_name);
        entry.dir_attr = FAT32DirectoryEntryAttribute::ATTR_DIRECTORY;
        entry.dir_fst_clus_lo.write((cluster & 0xFFFF) as u16);
        entry
            .dir_fst_clus_hi
            .write(((cluster >> 16) & 0xFFFF) as u16);
        entry
    }

    fn build_file_entry(short_name: [u8; 11]) -> FAT32ByteDirectoryEntry {
        let mut entry = Self::build_base_entry(short_name);
        entry.dir_attr = FAT32DirectoryEntryAttribute::empty();
        entry
    }

    fn write_directory_entry(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        entry_lba: u64,
        entry_offset: u16,
        entry: &FAT32ByteDirectoryEntry,
    ) -> Result<(), FileSystemErr> {
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(
                entry as *const FAT32ByteDirectoryEntry as *const u8,
                size_of::<FAT32ByteDirectoryEntry>(),
            )
        };
        self.write_entry_data(block_device, entry_lba, entry_offset, entry_bytes)
    }

    fn write_long_directory_entry(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        entry_lba: u64,
        entry_offset: u16,
        entry: &FAT32LongDirectoryEntry,
    ) -> Result<(), FileSystemErr> {
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(
                entry as *const FAT32LongDirectoryEntry as *const u8,
                size_of::<FAT32LongDirectoryEntry>(),
            )
        };
        self.write_entry_data(block_device, entry_lba, entry_offset, entry_bytes)
    }

    fn write_entry_data(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        entry_lba: u64,
        entry_offset: u16,
        entry_bytes: &[u8],
    ) -> Result<(), FileSystemErr> {
        let mut sector = vec![0u8; self.volume.bytes_per_sector as usize];
        let mut uninit = slice_as_uninit(&mut sector);
        block_device
            .read_at(entry_lba, &mut uninit)
            .map_err(from_io_err)?;
        let offset = entry_offset as usize;
        if offset + entry_bytes.len() > sector.len() {
            return Err(FileSystemErr::Corrupted);
        }
        sector[offset..offset + entry_bytes.len()].copy_from_slice(entry_bytes);
        block_device
            .write_at(entry_lba, &sector)
            .map_err(from_io_err)
    }

    fn read_directory_entry(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        meta: &DirMeta,
    ) -> Result<FAT32ByteDirectoryEntry, FileSystemErr> {
        if meta.entry_lba == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        let mut sector = vec![0u8; self.volume.bytes_per_sector as usize];
        let mut uninit = slice_as_uninit(&mut sector);
        block_device
            .read_at(meta.entry_lba, &mut uninit)
            .map_err(from_io_err)?;
        let offset = meta.entry_offset as usize;
        if offset + size_of::<FAT32ByteDirectoryEntry>() > sector.len() {
            return Err(FileSystemErr::Corrupted);
        }
        let entry_ptr = unsafe { sector.as_ptr().add(offset) as *const FAT32ByteDirectoryEntry };
        Ok(unsafe { core::ptr::read(entry_ptr) })
    }

    fn prepare_entry_names(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        name: &str,
    ) -> Result<([u8; 11], Vec<FAT32LongDirectoryEntry>), FileSystemErr> {
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemErr::InvalidInput);
        }
        if let Some(_) = Self::is_encode_83(name)? {
            let short = Self::encode_short_name_bytes(name)?;
            return Ok((short, Vec::new()));
        }
        if !name.is_ascii() {
            return Err(FileSystemErr::UnsupportedFileName);
        }
        let short = self.generate_short_name(block_device, dir_cluster, name)?;
        let long = Self::build_long_entries(name, &short)?;
        Ok((short, long))
    }

    fn insert_entry_with_builder<F>(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        name: &str,
        build_entry: F,
    ) -> Result<(u64, u16), FileSystemErr>
    where
        F: FnOnce([u8; 11]) -> FAT32ByteDirectoryEntry,
    {
        let (short_name, long_entries) =
            self.prepare_entry_names(block_device, dir_cluster, name)?;
        let entry = build_entry(short_name);
        self.write_entry_sequence(block_device, dir_cluster, &long_entries, &entry)
    }

    fn write_entry_sequence(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        long_entries: &[FAT32LongDirectoryEntry],
        entry: &FAT32ByteDirectoryEntry,
    ) -> Result<(u64, u16), FileSystemErr> {
        let span = self.find_free_dir_entries(block_device, dir_cluster, long_entries.len() + 1)?;
        for (idx, long_entry) in long_entries.iter().enumerate().rev() {
            let slot = long_entries.len() - 1 - idx;
            let (lba, offset) = self.entry_location(block_device, &span, slot)?;
            self.write_long_directory_entry(block_device, lba, offset, long_entry)?;
        }
        let (entry_lba, entry_offset) =
            self.entry_location(block_device, &span, long_entries.len())?;
        self.write_directory_entry(block_device, entry_lba, entry_offset, entry)?;
        Ok((entry_lba, entry_offset))
    }

    fn entry_location(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        span: &DirEntrySpan,
        index: usize,
    ) -> Result<(u64, u16), FileSystemErr> {
        let entry_size = size_of::<FAT32ByteDirectoryEntry>() as u64;
        let cluster_size = self.cluster_size_bytes() as u64;
        let start_offset = span.start_offset as u64;
        let total_offset = start_offset
            .checked_add(
                entry_size
                    .checked_mul(index as u64)
                    .ok_or(FileSystemErr::TooBigBuffer)?,
            )
            .ok_or(FileSystemErr::TooBigBuffer)?;
        let clusters_to_skip = (total_offset / cluster_size) as usize;
        let mut offset_in_cluster = (total_offset % cluster_size) as usize;
        let mut iter = FAT32FATIter::new(block_device, self, span.start_cluster);
        let mut info = None;
        for _ in 0..=clusters_to_skip {
            let Some(cluster_info) = iter.next() else {
                return Err(FileSystemErr::Corrupted);
            };
            info = Some(cluster_info?);
        }
        let info = info.ok_or(FileSystemErr::Corrupted)?;
        let sector_size = self.volume.bytes_per_sector as usize;
        let entry_lba = info.lba + (offset_in_cluster / sector_size) as u64;
        offset_in_cluster %= sector_size;
        Ok((entry_lba, offset_in_cluster as u16))
    }

    fn find_free_dir_entries(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        count: usize,
    ) -> Result<DirEntrySpan, FileSystemErr> {
        if dir_cluster == 0 || count == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        let cluster_size = self.cluster_size_bytes();
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        let mut buffer = vec![0u8; cluster_size];
        let mut last_cluster = None;
        let mut run_count = 0usize;
        let mut run_start_cluster = dir_cluster;
        let mut run_start_offset = 0usize;
        for info in FAT32FATIter::new(block_device, self, dir_cluster) {
            let info = info?;
            last_cluster = Some(info.cluster);
            let mut uninit = slice_as_uninit(&mut buffer);
            block_device
                .read_at(info.lba, &mut uninit)
                .map_err(from_io_err)?;
            for entry_off in (0..cluster_size).step_by(entry_size) {
                let value = buffer[entry_off];
                if value == 0x00 || value == 0xE5 {
                    if run_count == 0 {
                        run_start_cluster = info.cluster;
                        run_start_offset = entry_off;
                    }
                    run_count += 1;
                    if run_count >= count {
                        return Ok(DirEntrySpan {
                            start_cluster: run_start_cluster,
                            start_offset: run_start_offset,
                        });
                    }
                } else {
                    run_count = 0;
                }
            }
        }
        let last_cluster = last_cluster.ok_or(FileSystemErr::Corrupted)?;
        self.extend_directory_with_entries(block_device, last_cluster, count)
    }

    fn extend_directory_with_entries(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        mut last_cluster: u32,
        count: usize,
    ) -> Result<DirEntrySpan, FileSystemErr> {
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        let cluster_size = self.cluster_size_bytes();
        let entries_per_cluster = cluster_size / entry_size;
        if entries_per_cluster == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        let clusters_needed = (count + entries_per_cluster - 1) / entries_per_cluster;
        let mut first_new = None;
        for _ in 0..clusters_needed {
            let new_cluster = self.find_free_cluster(block_device)?;
            self.write_fat_entry(block_device, new_cluster, FAT32FAT::END_OF_CHAIN)?;
            self.write_fat_entry(block_device, last_cluster, new_cluster)?;
            self.zero_cluster(block_device, new_cluster)?;
            if first_new.is_none() {
                first_new = Some(new_cluster);
            }
            last_cluster = new_cluster;
        }
        let start = first_new.ok_or(FileSystemErr::Corrupted)?;
        Ok(DirEntrySpan {
            start_cluster: start,
            start_offset: 0,
        })
    }

    fn short_name_exists(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        short_name: &[u8; 11],
    ) -> Result<bool, FileSystemErr> {
        let cluster_size = self.cluster_size_bytes();
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        let mut buffer = vec![0u8; cluster_size];
        for info in FAT32FATIter::new(block_device, self, dir_cluster) {
            let info = info?;
            let mut uninit = slice_as_uninit(&mut buffer);
            block_device
                .read_at(info.lba, &mut uninit)
                .map_err(from_io_err)?;
            for entry_off in (0..cluster_size).step_by(entry_size) {
                let name0 = buffer[entry_off];
                if name0 == 0x00 {
                    return Ok(false);
                }
                if name0 == 0xE5 {
                    continue;
                }
                if !FAT32DirectoryEntryAttribute::is_sde(buffer.as_ptr() as usize + entry_off) {
                    continue;
                }
                let entry = unsafe {
                    &*((buffer.as_ptr().add(entry_off)) as *const FAT32ByteDirectoryEntry)
                };
                if entry.dir_attr & FAT32DirectoryEntryAttribute::ATTR_VOLUME_ID
                    == FAT32DirectoryEntryAttribute::ATTR_VOLUME_ID
                {
                    continue;
                }
                if entry.dir_name == *short_name {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn generate_short_name(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        name: &str,
    ) -> Result<[u8; 11], FileSystemErr> {
        let (base_part, ext_part) = Self::split_long_name(name);
        let mut base = Self::sanitize_short_component(base_part);
        if base.is_empty() {
            base.push(b'_');
        }
        let mut extension = ext_part
            .map(Self::sanitize_short_component)
            .unwrap_or_default();
        extension.truncate(3);
        let mut counter = 1usize;
        loop {
            let mut base_candidate = base.clone();
            let suffix = Self::format_short_suffix(counter);
            while base_candidate.len() + suffix.len() > 8 {
                if base_candidate.is_empty() {
                    return Err(FileSystemErr::NoSpace);
                }
                base_candidate.pop();
            }
            base_candidate.extend_from_slice(&suffix);
            let mut short = [b' '; 11];
            for (idx, ch) in base_candidate.iter().take(8).enumerate() {
                short[idx] = *ch;
            }
            for (idx, ch) in extension.iter().enumerate() {
                short[8 + idx] = *ch;
            }
            if !self.short_name_exists(block_device, dir_cluster, &short)? {
                return Ok(short);
            }
            counter += 1;
            if counter > 999_999 {
                return Err(FileSystemErr::NoSpace);
            }
        }
    }

    fn split_long_name(name: &str) -> (&str, Option<&str>) {
        if let Some(idx) = name.rfind('.') {
            if idx > 0 && idx < name.len() - 1 {
                return (&name[..idx], Some(&name[idx + 1..]));
            }
        }
        (name, None)
    }

    fn sanitize_short_component(component: &str) -> Vec<u8> {
        let mut result = Vec::new();
        for byte in component.bytes() {
            let upper = byte.to_ascii_uppercase();
            let valid = matches!(
                upper,
                b'A'..=b'Z'
                    | b'0'..=b'9'
                    | b'$'
                    | b'%'
                    | b'`'
                    | b'-'
                    | b'_'
                    | b'@'
                    | b'~'
                    | b'!'
                    | b'('
                    | b')'
                    | b'{'
                    | b'}'
                    | b'^'
                    | b'#'
                    | b'&'
            );
            let ch = if valid { upper } else { b'_' };
            if ch != b'.' {
                result.push(ch);
            }
        }
        result
    }

    fn format_short_suffix(counter: usize) -> Vec<u8> {
        let mut suffix = Vec::new();
        suffix.push(b'~');
        let mut digits = Vec::new();
        let mut value = counter;
        while value > 0 {
            digits.push(b'0' + (value % 10) as u8);
            value /= 10;
        }
        if digits.is_empty() {
            digits.push(b'0');
        }
        digits.reverse();
        suffix.extend_from_slice(&digits);
        suffix
    }

    fn build_long_entries(
        name: &str,
        short_name: &[u8; 11],
    ) -> Result<Vec<FAT32LongDirectoryEntry>, FileSystemErr> {
        if !name.is_ascii() {
            return Err(FileSystemErr::UnsupportedFileName);
        }
        let bytes = name.as_bytes();
        let chars_len = bytes.len() + 1;
        let total_entries = (chars_len + 12) / 13;
        let checksum = Self::short_name_checksum(short_name);
        let mut entries = Vec::with_capacity(total_entries);
        for seq in 1..=total_entries {
            let mut encoded = [0xFFFFu16; 13];
            let start = (seq - 1) * 13;
            for i in 0..13 {
                let idx = start + i;
                let ch = if idx < bytes.len() {
                    bytes[idx] as u16
                } else if idx == bytes.len() {
                    0
                } else {
                    break;
                };
                encoded[i] = ch;
                if ch == 0 {
                    break;
                }
            }
            let entry = MaybeUninit::<FAT32LongDirectoryEntry>::zeroed();
            let mut entry = unsafe { entry.assume_init() };
            entry.ldir_ord = seq as u8;
            entry.ldir_attr = FAT32DirectoryEntryAttribute::ATTR_LONG_NAME;
            entry.ldir_type = 0;
            entry.ldir_chksum = checksum;
            entry.ldir_fst_clus_lo = 0;
            Self::fill_long_entry_name(&mut entry.ldir_name1, &encoded[0..5]);
            Self::fill_long_entry_name(&mut entry.ldir_name2, &encoded[5..11]);
            Self::fill_long_entry_name(&mut entry.ldir_name3, &encoded[11..13]);
            entries.push(entry);
        }
        if let Some(last) = entries.last_mut() {
            last.ldir_ord |= 0x40;
        }
        Ok(entries)
    }

    fn fill_long_entry_name(target: &mut [u8], values: &[u16]) {
        for (idx, value) in values.iter().enumerate() {
            let bytes = value.to_le_bytes();
            let dst = idx * 2;
            target[dst] = bytes[0];
            target[dst + 1] = bytes[1];
        }
    }

    fn short_name_checksum(short_name: &[u8; 11]) -> u8 {
        let mut check_sum: u8 = 0;
        for byte in short_name.iter() {
            check_sum = check_sum.rotate_right(1).wrapping_add(*byte);
        }
        check_sum
    }

    fn dot_name(count: usize) -> [u8; 11] {
        let mut name = [b' '; 11];
        for i in 0..count {
            name[i] = b'.';
        }
        name
    }

    fn initialize_directory_cluster(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        cluster: u32,
        parent_cluster: u32,
    ) -> Result<(), FileSystemErr> {
        let mut cluster_buf = vec![0u8; self.cluster_size_bytes()];
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        let dot = Self::build_directory_entry(Self::dot_name(1), cluster);
        let dotdot = Self::build_directory_entry(Self::dot_name(2), parent_cluster);
        let dot_bytes = unsafe {
            core::slice::from_raw_parts(
                &dot as *const FAT32ByteDirectoryEntry as *const u8,
                entry_size,
            )
        };
        cluster_buf[0..entry_size].copy_from_slice(dot_bytes);
        let dotdot_bytes = unsafe {
            core::slice::from_raw_parts(
                &dotdot as *const FAT32ByteDirectoryEntry as *const u8,
                entry_size,
            )
        };
        cluster_buf[entry_size..entry_size * 2].copy_from_slice(dotdot_bytes);
        let lba = self.volume.cluster_to_lba(cluster)?;
        block_device
            .write_at(lba, &cluster_buf)
            .map_err(from_io_err)
    }

    fn read_directory_parent_cluster(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
    ) -> Result<u32, FileSystemErr> {
        if dir_cluster == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        let mut buffer = vec![0u8; self.cluster_size_bytes()];
        let mut uninit = slice_as_uninit(&mut buffer);
        let lba = self.volume.cluster_to_lba(dir_cluster)?;
        block_device
            .read_at(lba, &mut uninit)
            .map_err(from_io_err)?;
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        if entry_size * 2 > buffer.len() {
            return Err(FileSystemErr::Corrupted);
        }
        let parent_ptr =
            unsafe { buffer.as_ptr().add(entry_size) as *const FAT32ByteDirectoryEntry };
        let parent = unsafe { &*parent_ptr };
        let parent_cluster =
            ((parent.dir_fst_clus_hi.read() as u32) << 16) | parent.dir_fst_clus_lo.read() as u32;
        Ok(parent_cluster)
    }

    fn update_directory_parent_cluster(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        parent_cluster: u32,
    ) -> Result<(), FileSystemErr> {
        if dir_cluster == 0 {
            return Err(FileSystemErr::Corrupted);
        }
        let mut buffer = vec![0u8; self.cluster_size_bytes()];
        let mut uninit = slice_as_uninit(&mut buffer);
        let lba = self.volume.cluster_to_lba(dir_cluster)?;
        block_device
            .read_at(lba, &mut uninit)
            .map_err(from_io_err)?;
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        if entry_size * 2 > buffer.len() {
            return Err(FileSystemErr::Corrupted);
        }
        let parent_ptr =
            unsafe { buffer.as_mut_ptr().add(entry_size) as *mut FAT32ByteDirectoryEntry };
        let parent = unsafe { &mut *parent_ptr };
        if parent.dir_name[0] != b'.' || parent.dir_name[1] != b'.' {
            return Err(FileSystemErr::Corrupted);
        }
        parent
            .dir_fst_clus_lo
            .write((parent_cluster & 0xFFFF) as u16);
        parent
            .dir_fst_clus_hi
            .write(((parent_cluster >> 16) & 0xFFFF) as u16);
        block_device.write_at(lba, &buffer).map_err(from_io_err)
    }

    fn directory_contains_cluster(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        ancestor: u32,
        mut cluster: u32,
    ) -> Result<bool, FileSystemErr> {
        if ancestor == 0 || cluster == 0 {
            return Ok(false);
        }
        if ancestor == cluster {
            return Ok(true);
        }
        while cluster != self.root_dir_cluster {
            cluster = self.read_directory_parent_cluster(block_device, cluster)?;
            if cluster == ancestor {
                return Ok(true);
            }
        }
        Ok(cluster == ancestor)
    }

    fn delete_directory_entry(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
        meta: &DirMeta,
    ) -> Result<(), FileSystemErr> {
        let cluster_size = self.cluster_size_bytes();
        let sector_size = self.volume.bytes_per_sector as usize;
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        let mut buffer = vec![0u8; cluster_size];
        let mut cluster_lbas = Vec::new();
        for info in FAT32FATIter::new(block_device, self, dir_cluster) {
            let info = info?;
            cluster_lbas.push(info.lba);
            let start = info.lba;
            let end = start + self.volume.sectors_per_cluster as u64;
            if meta.entry_lba < start || meta.entry_lba >= end {
                continue;
            }
            let mut uninit = slice_as_uninit(&mut buffer);
            block_device
                .read_at(info.lba, &mut uninit)
                .map_err(from_io_err)?;
            let sector_off = meta.entry_lba - start;
            let mut entry_off = sector_off as usize * sector_size + meta.entry_offset as usize;
            if entry_off + entry_size > buffer.len() {
                return Err(FileSystemErr::Corrupted);
            }
            buffer[entry_off] = 0xE5;
            let mut current_cluster_idx = cluster_lbas.len() - 1;
            let mut dirty = true;
            loop {
                if entry_off >= entry_size {
                    let prev = entry_off - entry_size;
                    let attr = buffer[prev + FAT32DirectoryEntryAttribute::OFFSET];
                    if attr != FAT32DirectoryEntryAttribute::ATTR_LONG_NAME.raw() {
                        if dirty {
                            block_device
                                .write_at(cluster_lbas[current_cluster_idx], &buffer)
                                .map_err(from_io_err)?;
                        }
                        return Ok(());
                    }
                    buffer[prev] = 0xE5;
                    entry_off = prev;
                    dirty = true;
                    continue;
                }
                if dirty {
                    block_device
                        .write_at(cluster_lbas[current_cluster_idx], &buffer)
                        .map_err(from_io_err)?;
                    dirty = false;
                }
                if current_cluster_idx == 0 {
                    return Ok(());
                }
                current_cluster_idx -= 1;
                let mut uninit = slice_as_uninit(&mut buffer);
                block_device
                    .read_at(cluster_lbas[current_cluster_idx], &mut uninit)
                    .map_err(from_io_err)?;
                entry_off = buffer.len();
            }
        }
        Err(FileSystemErr::Corrupted)
    }

    fn directory_is_empty(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        dir_cluster: u32,
    ) -> Result<bool, FileSystemErr> {
        let cluster_size = self.cluster_size_bytes();
        let entry_size = size_of::<FAT32ByteDirectoryEntry>();
        let mut buffer = vec![0u8; cluster_size];
        for info in FAT32FATIter::new(block_device, self, dir_cluster) {
            let info = info?;
            let mut uninit = slice_as_uninit(&mut buffer);
            block_device
                .read_at(info.lba, &mut uninit)
                .map_err(from_io_err)?;
            for entry_off in (0..cluster_size).step_by(entry_size) {
                let name0 = buffer[entry_off];
                if name0 == 0x00 {
                    return Ok(true);
                }
                if name0 == 0xE5 {
                    continue;
                }
                let attr = buffer[entry_off + FAT32DirectoryEntryAttribute::OFFSET];
                if attr == FAT32DirectoryEntryAttribute::ATTR_LONG_NAME.raw() {
                    continue;
                }
                let entry = unsafe {
                    &*((buffer.as_ptr().add(entry_off)) as *const FAT32ByteDirectoryEntry)
                };
                if entry.dir_attr & FAT32DirectoryEntryAttribute::ATTR_VOLUME_ID
                    == FAT32DirectoryEntryAttribute::ATTR_VOLUME_ID
                {
                    continue;
                }
                if entry.dir_name[0] == b'.'
                    && (entry.dir_name[1] == b' ' || entry.dir_name[1] == b'.')
                {
                    if entry.dir_name[1] == b' ' {
                        continue;
                    }
                    if entry.dir_name[1] == b'.' && entry.dir_name[2] == b' ' {
                        continue;
                    }
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn free_cluster_chain(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        first_cluster: u32,
    ) -> Result<(), FileSystemErr> {
        if first_cluster == 0 {
            return Ok(());
        }
        let chain = self.collect_cluster_chain(block_device, first_cluster)?;
        for cluster in chain {
            self.write_fat_entry(block_device, cluster, 0)?;
        }
        Ok(())
    }

    fn create_directory(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        let (parent_cluster, parent_meta, dir_name) = self.resolve_parent(block_device, path)?;
        self.ensure_directory_writable(block_device, parent_meta.as_ref())?;
        if self
            .search_file_name_with_cluster_dir(block_device, parent_cluster, dir_name)?
            .is_some()
        {
            return Err(FileSystemErr::AlreadyExists);
        }
        let new_cluster = self.find_free_cluster(block_device)?;
        self.write_fat_entry(block_device, new_cluster, FAT32FAT::END_OF_CHAIN)?;
        self.initialize_directory_cluster(block_device, new_cluster, parent_cluster)?;
        self.insert_entry_with_builder(block_device, parent_cluster, dir_name, |short_name| {
            Self::build_directory_entry(short_name, new_cluster)
        })
        .map(|_| ())
    }

    fn remove_directory(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        let (parent_cluster, parent_meta, dir_name) = self.resolve_parent(block_device, path)?;
        self.ensure_directory_writable(block_device, parent_meta.as_ref())?;
        let Some(meta) =
            self.search_file_name_with_cluster_dir(block_device, parent_cluster, dir_name)?
        else {
            return Err(FileSystemErr::NotFound);
        };
        if !meta.is_dir {
            return Err(FileSystemErr::NotDir);
        }
        if meta.is_readonly {
            return Err(FileSystemErr::ReadOnly);
        }
        if !self.directory_is_empty(block_device, meta.first_cluster)? {
            return Err(FileSystemErr::Busy);
        }
        self.free_cluster_chain(block_device, meta.first_cluster)?;
        self.delete_directory_entry(block_device, parent_cluster, &meta)
    }
}

fn slice_as_uninit(buf: &mut [u8]) -> &mut [MaybeUninit<u8>] {
    unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut MaybeUninit<u8>, buf.len()) }
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
            entry_lba: 0,
            entry_offset: 0,
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

    fn create_file(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        file_system: &Arc<dyn FileSystemTrait>,
        path: &str,
    ) -> Result<FileHandle, FileSystemErr> {
        let (parent_cluster, parent_meta, file_name) = self.resolve_parent(block_device, path)?;
        self.ensure_directory_writable(block_device, parent_meta.as_ref())?;
        if self
            .search_file_name_with_cluster_dir(block_device, parent_cluster, file_name)?
            .is_some()
        {
            return Err(FileSystemErr::AlreadyExists);
        }
        let (entry_lba, entry_offset) = self.insert_entry_with_builder(
            block_device,
            parent_cluster,
            file_name,
            |short_name| Self::build_file_entry(short_name),
        )?;
        let meta = DirMeta {
            is_dir: false,
            is_readonly: false,
            first_cluster: 0,
            file_size: 0,
            entry_lba,
            entry_offset,
        };
        Ok(FileHandle {
            dev_handle: Arc::downgrade(block_device),
            file_handle: Arc::downgrade(file_system),
            meta,
            opts: OpenOptions::Write,
        })
    }

    fn remove_file(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        let (parent_cluster, parent_meta, file_name) = self.resolve_parent(block_device, path)?;
        self.ensure_directory_writable(block_device, parent_meta.as_ref())?;
        let Some(meta) =
            self.search_file_name_with_cluster_dir(block_device, parent_cluster, file_name)?
        else {
            return Err(FileSystemErr::NotFound);
        };
        if meta.is_dir {
            return Err(FileSystemErr::IsDir);
        }
        if meta.is_readonly {
            return Err(FileSystemErr::ReadOnly);
        }
        self.free_cluster_chain(block_device, meta.first_cluster)?;
        self.delete_directory_entry(block_device, parent_cluster, &meta)
    }

    fn copy(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        from: &str,
        to: &str,
    ) -> Result<(), FileSystemErr> {
        let (src_parent_cluster, _, src_file_name) = self.resolve_parent(block_device, from)?;
        let Some(src_meta) = self.search_file_name_with_cluster_dir(
            block_device,
            src_parent_cluster,
            src_file_name,
        )?
        else {
            return Err(FileSystemErr::NotFound);
        };
        if src_meta.is_dir {
            return Err(FileSystemErr::IsDir);
        }

        let (dst_parent_cluster, dst_parent_meta, dst_file_name) =
            self.resolve_parent(block_device, to)?;
        self.ensure_directory_writable(block_device, dst_parent_meta.as_ref())?;
        if self
            .search_file_name_with_cluster_dir(block_device, dst_parent_cluster, dst_file_name)?
            .is_some()
        {
            return Err(FileSystemErr::AlreadyExists);
        }

        let (entry_lba, entry_offset) = self.insert_entry_with_builder(
            block_device,
            dst_parent_cluster,
            dst_file_name,
            |short_name| Self::build_file_entry(short_name),
        )?;

        let mut new_meta = DirMeta {
            is_dir: false,
            is_readonly: false,
            first_cluster: 0,
            file_size: 0,
            entry_lba,
            entry_offset,
        };

        let mut remaining = src_meta.file_size as u64;
        if remaining == 0 {
            return Ok(());
        }
        let chunk_size = cmp::max(1, self.cluster_size_bytes());
        let mut buffer = vec![0u8; chunk_size];
        let mut offset = 0u64;

        while remaining > 0 {
            let current = cmp::min(remaining, chunk_size as u64) as usize;
            {
                let chunk = &mut buffer[..current];
                let mut uninit = slice_as_uninit(chunk);
                self.read_at(block_device, offset, &mut uninit, &src_meta)?;
            }
            let chunk = &buffer[..current];
            self.write_at(block_device, offset, chunk, &mut new_meta)?;
            remaining -= current as u64;
            offset += current as u64;
        }
        Ok(())
    }

    fn rename(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        from: &str,
        to: &str,
    ) -> Result<(), FileSystemErr> {
        if from == to {
            return Ok(());
        }
        let (src_parent_cluster, src_parent_meta, src_file_name) =
            self.resolve_parent(block_device, from)?;
        self.ensure_directory_writable(block_device, src_parent_meta.as_ref())?;
        let Some(src_meta) = self.search_file_name_with_cluster_dir(
            block_device,
            src_parent_cluster,
            src_file_name,
        )?
        else {
            return Err(FileSystemErr::NotFound);
        };
        if src_meta.is_readonly {
            return Err(FileSystemErr::ReadOnly);
        }

        let (dst_parent_cluster, dst_parent_meta, dst_file_name) =
            self.resolve_parent(block_device, to)?;
        self.ensure_directory_writable(block_device, dst_parent_meta.as_ref())?;

        if src_meta.is_dir {
            if src_meta.first_cluster == 0 {
                return Err(FileSystemErr::Corrupted);
            }
            if dst_parent_cluster == src_meta.first_cluster
                || self.directory_contains_cluster(
                    block_device,
                    src_meta.first_cluster,
                    dst_parent_cluster,
                )?
            {
                return Err(FileSystemErr::Busy);
            }
        }

        if let Some(existing) =
            self.search_file_name_with_cluster_dir(block_device, dst_parent_cluster, dst_file_name)?
        {
            if existing.entry_lba == src_meta.entry_lba
                && existing.entry_offset == src_meta.entry_offset
            {
                return Ok(());
            }
            return Err(FileSystemErr::AlreadyExists);
        }

        let old_entry = self.read_directory_entry(block_device, &src_meta)?;
        let same_parent = src_parent_cluster == dst_parent_cluster;
        let entry_template = old_entry;
        let (new_entry_lba, new_entry_offset) = self.insert_entry_with_builder(
            block_device,
            dst_parent_cluster,
            dst_file_name,
            |short_name| {
                let mut entry = entry_template;
                entry.dir_name = short_name;
                entry
            },
        )?;
        let new_entry_meta = DirMeta {
            is_dir: src_meta.is_dir,
            is_readonly: src_meta.is_readonly,
            first_cluster: src_meta.first_cluster,
            file_size: src_meta.file_size,
            entry_lba: new_entry_lba,
            entry_offset: new_entry_offset,
        };

        if let Err(err) = self.delete_directory_entry(block_device, src_parent_cluster, &src_meta) {
            let _ = self.delete_directory_entry(block_device, dst_parent_cluster, &new_entry_meta);
            return Err(err);
        }

        if !same_parent && src_meta.is_dir {
            if let Err(err) = self.update_directory_parent_cluster(
                block_device,
                src_meta.first_cluster,
                dst_parent_cluster,
            ) {
                let _ =
                    self.delete_directory_entry(block_device, dst_parent_cluster, &new_entry_meta);
                let _ = self.insert_entry_with_builder(
                    block_device,
                    src_parent_cluster,
                    src_file_name,
                    |short_name| {
                        let mut entry = old_entry;
                        entry.dir_name = short_name;
                        entry
                    },
                );
                return Err(err);
            }
        }
        Ok(())
    }

    fn create_dir(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        self.create_directory(block_device, path)
    }

    fn remove_dir(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        path: &str,
    ) -> Result<(), FileSystemErr> {
        self.remove_directory(block_device, path)
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
        let spc = self.volume.sectors_per_cluster as usize;
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

        for (i, info) in FAT32FATIter::new(block_device, self, meta.first_cluster).enumerate() {
            let info = info?;
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
                    info.lba + first_sector_in_this_cluster as u64,
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

    // write_at invariants exercised by integration tests:
    // - refuses holes by rejecting offset beyond current file size.
    // - enforces FAT32 max file size (4 GiB - 1) and read-only flags.
    // - extends cluster chains without sharing clusters and zero-fills new clusters.
    // - operates with read-modify-write per cluster, preserving untouched bytes.
    fn write_at(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        offset: u64,
        buf: &[u8],
        meta: &mut DirMeta,
    ) -> Result<u64, FileSystemErr> {
        if buf.is_empty() {
            return Ok(0);
        }
        if meta.is_dir {
            return Err(FileSystemErr::IsDir);
        }
        if meta.is_readonly || block_device.is_read_only().map_err(from_io_err)? {
            return Err(FileSystemErr::ReadOnly);
        }
        if offset > meta.file_size as u64 {
            return Err(FileSystemErr::InvalidInput);
        }
        let new_size = offset
            .checked_add(buf.len() as u64)
            .ok_or(FileSystemErr::TooBigBuffer)?;
        if new_size > u32::MAX as u64 {
            return Err(FileSystemErr::TooBigBuffer);
        }
        let cluster_size = self.cluster_size_bytes();
        let required_clusters = if new_size == 0 {
            0
        } else {
            ((new_size + cluster_size as u64 - 1) / cluster_size as u64) as usize
        };
        let mut chain = self.collect_cluster_chain(block_device, meta.first_cluster)?;
        self.ensure_cluster_capacity(block_device, &mut chain, required_clusters, meta)?;

        let mut remaining = buf.len();
        let mut buf_offset = 0usize;
        let mut current_offset = offset;
        if remaining == 0 {
            return Ok(0);
        }
        let mut cluster_buf = vec![0; cluster_size];
        while remaining > 0 {
            let cluster_index = if cluster_size == 0 {
                0
            } else {
                (current_offset / cluster_size as u64) as usize
            };
            let cluster = *chain.get(cluster_index).ok_or(FileSystemErr::Corrupted)?;
            let lba = self.volume.cluster_to_lba(cluster)?;
            let mut uninit = slice_as_uninit(cluster_buf.as_mut_slice());
            block_device
                .read_at(lba, &mut uninit)
                .map_err(from_io_err)?;
            let within_cluster = (current_offset % cluster_size as u64) as usize;
            let writable = cmp::min(cluster_size - within_cluster, remaining);
            cluster_buf[within_cluster..within_cluster + writable]
                .copy_from_slice(&buf[buf_offset..buf_offset + writable]);
            block_device
                .write_at(lba, &cluster_buf)
                .map_err(from_io_err)?;
            remaining -= writable;
            buf_offset += writable;
            current_offset += writable as u64;
        }

        let mut need_update_entry = false;
        if new_size > meta.file_size as u64 {
            meta.file_size = new_size as u32;
            need_update_entry = true;
        }
        if meta.first_cluster == 0 && !chain.is_empty() {
            meta.first_cluster = chain[0];
            need_update_entry = true;
        }

        if need_update_entry {
            self.update_dir_entry(block_device, meta)?;
        }
        Ok(buf.len() as u64)
    }
}
