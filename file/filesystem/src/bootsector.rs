use crate::bootsector::mbr::MasterBootRecordPartitionKind;

pub(crate) mod mbr;

pub(crate) struct MBRPartition {
    pub(crate) kind: MasterBootRecordPartitionKind,
    pub(crate) first_sector: u32,
    pub(crate) total_sector: u32,
}

pub(crate) struct MBRConfig {
    pub(crate) partition: [MBRPartition; 4],
}

pub(crate) enum BootSector {
    MBR(MBRConfig),
    GPT,
    Unknown,
}
