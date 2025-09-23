use core::mem::size_of;
use typestate::Le;
use typestate::Unaligned;
use typestate_macro::RawReg;

#[allow(clippy::assertions_on_constants)]
const _: () = assert!(size_of::<MasterBootRecord>() == 512);

#[repr(packed)]
pub(crate) struct MasterBootRecord {
    loader: [u8; 446],
    pub(crate) first_partition: MasterBootRecordPartitionTable,
    pub(crate) second_partition: MasterBootRecordPartitionTable,
    pub(crate) third_partition: MasterBootRecordPartitionTable,
    pub(crate) fourth_partition: MasterBootRecordPartitionTable,
    pub(crate) boot_signature: Le<Unaligned<u16>>,
}

#[repr(C)]
pub(crate) struct MasterBootRecordPartitionTable {
    boot_flags: u8,
    chs_first_sector: [u8; 3],
    pub(crate) kind: MasterBootRecordPartitionKind,
    chs_last_sector: [u8; 3],
    pub(crate) lba_first_sector: Le<Unaligned<u32>>,
    pub(crate) num_of_total_sector: Le<Unaligned<u32>>,
}

#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq)]
pub(crate) struct MasterBootRecordPartitionKind(u8);

impl MasterBootRecordPartitionKind {
    pub(crate) const TYPE_FAT32: Self = Self(0x0C); // LBA
    pub(crate) const TYPE_GPT: Self = Self(0xEE);
}
