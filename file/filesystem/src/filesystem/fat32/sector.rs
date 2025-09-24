#![allow(unused)]

use core::ffi::c_char;
use core::mem::size_of;
use typestate::Le;
use typestate::Unaligned;
use typestate_macro::RawReg;

#[allow(clippy::assertions_on_constants)]
const _: () = assert!(size_of::<FAT32BootSector>() == 512);
const _: () = assert!(size_of::<FAT32FSInfoSector>() == 512);
const _: () = assert!(size_of::<FAT32ByteDirectoryEntry>() == 32);
const _: () = assert!(size_of::<FAT32LongDirectoryEntry>() == 32);

#[repr(packed)]
pub(crate) struct FAT32BootSector {
    bs_jmp_boot: [u8; 3],
    bs_oem_name: [u8; 8],
    pub(crate) bpb_bytes_per_sec: Le<Unaligned<u16>>,
    pub(crate) bpb_sec_per_clus: u8,
    pub(crate) bpb_rsvd_sec_cnt: Le<Unaligned<u16>>,
    pub(crate) bpb_num_fats: u8,
    pub(crate) bpb_root_ent_cnt: Le<Unaligned<u16>>,
    pub(crate) bpb_tot_sec_16: Le<Unaligned<u16>>,
    bpb_media: u8,
    pub(crate) bpb_fat_sz16: Le<Unaligned<u16>>,
    bpb_sec_per_trk: Le<Unaligned<u16>>,
    bpb_num_heads: Le<Unaligned<u16>>,
    pub(crate) bpb_hidd_sec: Le<Unaligned<u32>>,
    pub(crate) bpb_tot_sec_32: Le<Unaligned<u32>>,
    pub(crate) bpb_fat_sz_32: Le<Unaligned<u32>>,
    bpb_ext_flags: Le<Unaligned<u16>>,
    bpb_fs_ver: Le<Unaligned<u16>>,
    pub(crate) bpb_root_clus: Le<Unaligned<u32>>,
    bpb_fs_info: Le<Unaligned<u16>>,
    bpb_bk_boot_sec: Le<Unaligned<u16>>,
    bpb_reserved: [u8; 12],
    bs_drv_num: u8,
    bs_reserved1: u8,
    bs_boot_sig: u8,
    bs_vol_id: Le<Unaligned<u32>>,
    bs_vol_lab: [u8; 11],
    bs_fil_sys_type: [c_char; 8],
    bs_boot_ode32: [u8; 420],
    pub(crate) bs_boot_sign: Le<Unaligned<u16>>,
}

#[repr(packed)]
struct FAT32FSInfoSector {
    fsi_lead_sig: Le<Unaligned<u32>>,
    fsi_reserved1: [u8; 480],
    fsi_struc_sig: Le<Unaligned<u32>>,
    fsi_free_count: Le<Unaligned<u32>>,
    fsi_nxt_free: Le<Unaligned<u32>>,
    fsi_reserved2: [u8; 12],
    fsi_trail_sig: Le<Unaligned<u32>>,
}

/// # Safety
/// require 2byte alignment
#[repr(C)]
pub(crate) struct FAT32ByteDirectoryEntry {
    pub(crate) dir_name: [u8; 11],
    pub(crate) dir_attr: FAT32DirectoryEntryAttribute,
    dir_nt_res: u8,
    dir_crt_time_tenth: u8,
    dir_crt_time: Le<u16>,
    dir_ctr_data: Le<u16>,
    dir_lst_acc_data: Le<u16>,
    pub(crate) dir_fst_clus_hi: Le<u16>,
    dir_wrt_time: Le<u16>,
    dir_wrt_data: Le<u16>,
    pub(crate) dir_fst_clus_lo: Le<u16>,
    pub(crate) dir_file_size: Le<u32>,
}

/// # Safety
/// require 2byte alignment
#[repr(C)]
pub(crate) struct FAT32LongDirectoryEntry {
    pub(crate) ldir_ord: u8,
    pub(crate) ldir_name1: [u8; 10],
    ldir_attr: FAT32DirectoryEntryAttribute,
    ldir_type: u8,
    pub(crate) ldir_chksum: u8,
    pub(crate) ldir_name2: [u8; 12],
    pub(crate) ldir_fst_clus_lo: u16,
    pub(crate) ldir_name3: [u8; 4],
}

#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq)]
pub(crate) struct FAT32DirectoryEntryAttribute(u8);

impl FAT32DirectoryEntryAttribute {
    const OFFSET: usize = 11;
    pub(crate) const ATTR_READ_ONLY: Self = Self(0x01);
    const ATTR_HIDDEN: Self = Self(0x02);
    const ATTR_SYSTEM: Self = Self(0x04);
    pub(crate) const ATTR_VOLUME_ID: Self = Self(0x08);
    pub(crate) const ATTR_DIRECTORY: Self = Self(0x10);
    const ATTR_ARCHIVE: Self = Self(0x20);
    const ATTR_LONG_NAME: Self = Self(0x0F);

    #[inline]
    pub(crate) fn is_sde(ptr: usize) -> bool {
        let ptr = (ptr + Self::OFFSET) as *const FAT32DirectoryEntryAttribute;
        unsafe { *ptr != Self::ATTR_LONG_NAME }
    }
}
