use typestate::Le;
use typestate_macro::RawReg;

#[allow(clippy::assertions_on_constants)]
const _: () = assert!(size_of::<MasterBootRecord>() == 512);

#[repr(packed)]
pub(crate) struct MasterBootRecord {
    loader: [u8; 446],
    pub(crate) first_partition: MasterBootRecordPartitionTable,
    second_partition: MasterBootRecordPartitionTable,
    third_partition: MasterBootRecordPartitionTable,
    fourth_partition: MasterBootRecordPartitionTable,
    pub(crate) boot_signature: [u8; 2],
}

#[repr(C)]
pub(crate) struct MasterBootRecordPartitionTable {
    boot_flags: u8,
    chs_first_sector: [u8; 3],
    pub(crate) kind: MasterBootRecordPartitionKind,
    chs_last_sector: [u8; 3],
    lba_first_sector: [u8; 4],
    num_of_total_sector: [u8; 4],
}

#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq)]
pub(crate) struct MasterBootRecordPartitionKind(u8);

impl MasterBootRecordPartitionKind {
    pub(crate) const TYPE_FAT32: Self = Self(0x0C); // LBA
    pub(crate) const TYPE_GPT: Self = Self(0xEE);
}
