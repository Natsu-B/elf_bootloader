use typestate::Le;

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkConfig {
    pub capacity: Le<u64>,
    pub size_max: Le<u32>,
    pub seg_max: Le<u32>,
    pub geometry: VirtioBlkGeometry,
    pub blk_size: Le<u32>,
    pub topology: VirtioBlkTopology,
    pub writeback: u8,
    pub unused0: u8,
    pub num_queues: Le<u16>,
    pub max_discard_sectors: Le<u32>,
    pub max_discard_seg: Le<u32>,
    pub discard_sector_alignment: Le<u32>,
    pub max_write_zeroes_sectors: Le<u32>,
    pub max_write_zeroes_seg: Le<u32>,
    pub write_zeroes_may_unmap: u8,
    pub unused1: [u8; 3],
    pub max_secure_erase_sectors: Le<u32>,
    pub max_secure_erase_seg: Le<u32>,
    pub secure_erase_sector_alignment: Le<u32>,
    pub zoned: VirtioBlkZonedCharacteristics,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkGeometry {
    pub cylinders: Le<u16>,
    pub heads: u8,
    pub sectors: u8,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkTopology {
    pub physical_block_exp: u8,
    pub alignment_offset: u8,
    pub min_io_size: Le<u16>,
    pub opt_io_size: Le<u32>,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtioBlkZonedCharacteristics {
    pub zone_sectors: Le<u32>,
    pub max_open_zones: Le<u32>,
    pub max_active_zones: Le<u32>,
    pub max_append_sectors: Le<u32>,
    pub write_granularity: Le<u32>,
    pub model: u8,
    pub unused2: [u8; 3],
}
