use typestate::Le;
use typestate::ReadPure;

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkConfig {
    pub capacity: ReadPure<Le<u64>>,
    pub size_max: ReadPure<Le<u32>>,
    pub seg_max: ReadPure<Le<u32>>,
    pub geometry: VirtioBlkGeometry,
    pub blk_size: ReadPure<Le<u32>>,
    pub topology: VirtioBlkTopology,
    pub writeback: ReadPure<u8>,
    pub _unused0: u8,
    pub num_queues: ReadPure<Le<u16>>,
    pub max_discard_sectors: ReadPure<Le<u32>>,
    pub max_discard_seg: ReadPure<Le<u32>>,
    pub discard_sector_alignment: ReadPure<Le<u32>>,
    pub max_write_zeroes_sectors: ReadPure<Le<u32>>,
    pub max_write_zeroes_seg: ReadPure<Le<u32>>,
    pub write_zeroes_may_unmap: ReadPure<u8>,
    pub _unused1: [u8; 3],
    pub max_secure_erase_sectors: ReadPure<Le<u32>>,
    pub max_secure_erase_seg: ReadPure<Le<u32>>,
    pub secure_erase_sector_alignment: ReadPure<Le<u32>>,
    pub zoned: VirtioBlkZonedCharacteristics,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkGeometry {
    pub cylinders: ReadPure<Le<u16>>,
    pub heads: ReadPure<u8>,
    pub sectors: ReadPure<u8>,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkTopology {
    pub physical_block_exp: ReadPure<u8>,
    pub alignment_offset: ReadPure<u8>,
    pub min_io_size: ReadPure<Le<u16>>,
    pub opt_io_size: ReadPure<Le<u32>>,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkZonedCharacteristics {
    pub zone_sectors: ReadPure<Le<u32>>,
    pub max_open_zones: ReadPure<Le<u32>>,
    pub max_active_zones: ReadPure<Le<u32>>,
    pub max_append_sectors: ReadPure<Le<u32>>,
    pub write_granularity: ReadPure<Le<u32>>,
    pub model: ReadPure<u8>,
    pub _unused: [u8; 3],
}
