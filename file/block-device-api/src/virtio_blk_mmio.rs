#![allow(clippy::identity_op)]

#[cfg(test)]
extern crate std;

use crate::BlockDevice;
use crate::IoError;
use core::mem::MaybeUninit;
use core::mem::size_of;

pub const VIRTIO_MMIO_MAGIC_VALUE: u32 = 0x7472_6976;
pub const VIRTIO_MMIO_VERSION_MODERN: u32 = 2;
pub const VIRTIO_MMIO_DEVICE_ID_BLOCK: u32 = 2;
pub const VIRTIO_MMIO_VENDOR_ID: u32 = 0x554d_4551; // "QEMU" conventional value

pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1 << 0;
pub const VIRTIO_STATUS_DRIVER: u32 = 1 << 1;
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 1 << 2;
pub const VIRTIO_STATUS_FEATURES_OK: u32 = 1 << 3;
pub const VIRTIO_STATUS_DEVICE_NEEDS_RESET: u32 = 1 << 6;
pub const VIRTIO_STATUS_FAILED: u32 = 1 << 7;

pub const VIRTIO_F_VERSION_1: u64 = 1u64 << 32;
pub const VIRTIO_BLK_F_RO: u64 = 1u64 << 5;

pub const VIRTIO_INT_USED_RING: u32 = 1 << 0;

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

pub const SECTOR_SIZE: usize = 512;
pub const SECTOR_SIZE_U64: u64 = 512;

const REG_MAGIC_VALUE: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const REG_VENDOR_ID: usize = 0x00c;
const REG_DEVICE_FEATURES: usize = 0x010;
const REG_DEVICE_FEATURES_SEL: usize = 0x014;
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_INTERRUPT_STATUS: usize = 0x060;
const REG_INTERRUPT_ACK: usize = 0x064;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_DRIVER_LOW: usize = 0x090;
const REG_QUEUE_DRIVER_HIGH: usize = 0x094;
const REG_QUEUE_DEVICE_LOW: usize = 0x0a0;
const REG_QUEUE_DEVICE_HIGH: usize = 0x0a4;
const REG_CONFIG_GENERATION: usize = 0x0fc;
const REG_CONFIG_SPACE: usize = 0x100;

const QUEUE_MAX: u16 = 256;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtioBlkReq {
    pub req_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtioBlkConfig {
    pub capacity: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

const _: () = assert!(size_of::<VirtioBlkReq>() == 16);
const _: () = assert!(size_of::<VirtqDesc>() == 16);
const _: () = assert!(size_of::<VirtqUsedElem>() == 8);
const VIRTIO_BLK_REQ_HEADER_RAW_LEN: usize = size_of::<VirtioBlkReq>();
const PARSE_FAILURE_DESCRIPTOR_CAPTURE_LIMIT: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioAccessError {
    InvalidSize,
    InvalidOffset,
    InvalidValue,
    QueueNotReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueParseError {
    InvalidQueueSize,
    InvalidChain,
    DescriptorLoop,
    IndirectUnsupported,
    OutOfRange,
    MissingStatus,
    InvalidRequestType,
    InvalidLayout,
    Align,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioBlkReqLayout {
    pub first_data_desc_idx: Option<u16>,
    pub status_desc_idx: u16,
    pub data_len: u32,
    pub status_addr: u64,
    pub req_type: u32,
    pub sector: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioBlkDescriptorSnapshot {
    pub descriptor_count_captured: u8,
    pub descriptors: [Option<VirtqDesc>; PARSE_FAILURE_DESCRIPTOR_CAPTURE_LIMIT],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioBlkParseFailureDetail {
    pub head: u16,
    pub queue_size: u16,
    pub status_addr_hint: Option<u64>,
    pub mapped_status: u8,
    pub parse_error: QueueParseError,
    pub header_addr: Option<u64>,
    pub header_readable: bool,
    pub header_raw: Option<[u8; VIRTIO_BLK_REQ_HEADER_RAW_LEN]>,
    pub descriptor_snapshot: VirtioBlkDescriptorSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioBlkExecutionFailureDetail {
    pub head: u16,
    pub layout: VirtioBlkReqLayout,
    pub mapped_status: u8,
    pub err: VirtioBlkMmioError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsedRingUpdate {
    pub used_elem_addr: u64,
    pub used_idx_addr: u64,
    pub ring_index: u16,
    pub new_used_idx: u16,
    pub id: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct FeaturePolicy {
    pub allow_read_only: bool,
}

pub const fn device_features(policy: FeaturePolicy) -> u64 {
    let mut features = VIRTIO_F_VERSION_1;
    if policy.allow_read_only {
        features |= VIRTIO_BLK_F_RO;
    }
    features
}

pub fn select_feature_bits(features: u64, sel: u32) -> u32 {
    match sel {
        0 => features as u32,
        1 => (features >> 32) as u32,
        _ => 0,
    }
}

pub fn merge_driver_features(old: u64, sel: u32, value: u32) -> Result<u64, MmioAccessError> {
    match sel {
        0 => Ok((old & !0xffff_ffffu64) | value as u64),
        1 => Ok((old & 0xffff_ffffu64) | ((value as u64) << 32)),
        _ => Err(MmioAccessError::InvalidValue),
    }
}

pub fn validate_negotiated_features(device: u64, driver: u64) -> Result<(), MmioAccessError> {
    if (driver & !device) != 0 {
        return Err(MmioAccessError::InvalidValue);
    }
    if (driver & VIRTIO_F_VERSION_1) == 0 {
        return Err(MmioAccessError::InvalidValue);
    }
    Ok(())
}

pub fn ack_interrupt_bits(old: u32, ack: u32) -> u32 {
    old & !ack
}

pub fn validate_queue_size(size: u16, queue_max: u16) -> Result<(), QueueParseError> {
    if size == 0 || size > queue_max {
        return Err(QueueParseError::InvalidQueueSize);
    }
    if !size.is_power_of_two() {
        return Err(QueueParseError::InvalidQueueSize);
    }
    Ok(())
}

pub fn desc_table_len_bytes(queue_size: u16) -> u64 {
    queue_size as u64 * size_of::<VirtqDesc>() as u64
}

pub fn avail_ring_len_bytes(queue_size: u16) -> u64 {
    (size_of::<VirtqAvail>() + queue_size as usize * size_of::<u16>()) as u64
}

pub fn used_ring_len_bytes(queue_size: u16) -> u64 {
    (size_of::<VirtqUsed>() + queue_size as usize * size_of::<VirtqUsedElem>()) as u64
}

pub fn avail_ring_entry_addr(avail_addr: u64, ring_index: u16) -> u64 {
    avail_addr + size_of::<VirtqAvail>() as u64 + ring_index as u64 * size_of::<u16>() as u64
}

pub fn used_ring_entry_addr(used_addr: u64, ring_index: u16) -> u64 {
    used_addr
        + size_of::<VirtqUsed>() as u64
        + ring_index as u64 * size_of::<VirtqUsedElem>() as u64
}

pub fn used_ring_idx_addr(used_addr: u64) -> u64 {
    used_addr + 2
}

pub fn next_avail_to_process(last_avail_idx: u16, avail_idx: u16) -> Result<u16, QueueParseError> {
    let delta = avail_idx.wrapping_sub(last_avail_idx);
    if delta == 0 {
        return Err(QueueParseError::OutOfRange);
    }
    Ok(last_avail_idx)
}

pub fn calc_ring_index(idx: u16, queue_size: u16) -> Result<u16, QueueParseError> {
    validate_queue_size(queue_size, queue_size)?;
    Ok(idx & (queue_size - 1))
}

pub fn check_aligned_sector_buffer(len: u32) -> Result<(), QueueParseError> {
    if len == 0 {
        return Err(QueueParseError::InvalidLayout);
    }
    if !(len as usize).is_multiple_of(SECTOR_SIZE) {
        return Err(QueueParseError::Align);
    }
    Ok(())
}

pub fn classify_io_status(io_unsupported: bool, ok: bool) -> u8 {
    if ok {
        VIRTIO_BLK_S_OK
    } else if io_unsupported {
        VIRTIO_BLK_S_UNSUPP
    } else {
        VIRTIO_BLK_S_IOERR
    }
}

pub fn used_update(
    used_addr: u64,
    queue_size: u16,
    used_idx_before: u16,
    head_id: u16,
    used_len: u32,
) -> Result<UsedRingUpdate, QueueParseError> {
    validate_queue_size(queue_size, queue_size)?;
    let ring_index = used_idx_before & (queue_size - 1);
    Ok(UsedRingUpdate {
        used_elem_addr: used_ring_entry_addr(used_addr, ring_index),
        used_idx_addr: used_ring_idx_addr(used_addr),
        ring_index,
        new_used_idx: used_idx_before.wrapping_add(1),
        id: head_id as u32,
        len: used_len,
    })
}

pub fn parse_req_layout(
    queue_size: u16,
    head: u16,
    desc_at: &mut dyn FnMut(u16) -> Option<VirtqDesc>,
    read_req_header: &mut dyn FnMut(u64) -> Option<VirtioBlkReq>,
) -> Result<VirtioBlkReqLayout, QueueParseError> {
    validate_queue_size(queue_size, queue_size)?;
    if head >= queue_size {
        return Err(QueueParseError::OutOfRange);
    }

    let mut visited = 0u16;
    let mut cur = head;
    let mut req_type = None;
    let mut sector = 0u64;
    let mut first_data_desc_idx = None;
    let mut data_desc_count = 0u16;
    let mut data_len = 0u32;

    loop {
        if visited >= queue_size {
            return Err(QueueParseError::DescriptorLoop);
        }
        visited = visited.wrapping_add(1);

        let desc = desc_at(cur).ok_or(QueueParseError::OutOfRange)?;
        if (desc.flags & VIRTQ_DESC_F_INDIRECT) != 0 {
            return Err(QueueParseError::IndirectUnsupported);
        }

        if visited == 1 {
            if (desc.flags & VIRTQ_DESC_F_WRITE) != 0 {
                return Err(QueueParseError::InvalidLayout);
            }
            if desc.len != size_of::<VirtioBlkReq>() as u32 {
                return Err(QueueParseError::InvalidLayout);
            }

            let req = read_req_header(desc.addr).ok_or(QueueParseError::InvalidLayout)?;
            if req.req_type != VIRTIO_BLK_T_IN
                && req.req_type != VIRTIO_BLK_T_OUT
                && req.req_type != VIRTIO_BLK_T_FLUSH
            {
                return Err(QueueParseError::InvalidRequestType);
            }
            req_type = Some(req.req_type);
            sector = req.sector;
        } else if (desc.flags & VIRTQ_DESC_F_NEXT) == 0 {
            if (desc.flags & VIRTQ_DESC_F_WRITE) == 0 || desc.len != 1 {
                return Err(QueueParseError::MissingStatus);
            }
            let req_type = req_type.ok_or(QueueParseError::InvalidLayout)?;
            if req_type == VIRTIO_BLK_T_FLUSH {
                if data_desc_count != 0 || data_len != 0 {
                    return Err(QueueParseError::InvalidLayout);
                }
            } else {
                if data_desc_count == 0 || data_len == 0 {
                    return Err(QueueParseError::InvalidLayout);
                }
                check_aligned_sector_buffer(data_len)?;
            }

            return Ok(VirtioBlkReqLayout {
                first_data_desc_idx,
                status_desc_idx: cur,
                data_len,
                status_addr: desc.addr,
                req_type,
                sector,
            });
        } else {
            let req_type = req_type.ok_or(QueueParseError::InvalidLayout)?;
            match req_type {
                VIRTIO_BLK_T_IN => {
                    if (desc.flags & VIRTQ_DESC_F_WRITE) == 0 {
                        return Err(QueueParseError::InvalidLayout);
                    }
                }
                VIRTIO_BLK_T_OUT => {
                    if (desc.flags & VIRTQ_DESC_F_WRITE) != 0 {
                        return Err(QueueParseError::InvalidLayout);
                    }
                }
                VIRTIO_BLK_T_FLUSH => return Err(QueueParseError::InvalidLayout),
                _ => return Err(QueueParseError::InvalidRequestType),
            }

            if first_data_desc_idx.is_none() {
                first_data_desc_idx = Some(cur);
            }
            data_desc_count = data_desc_count
                .checked_add(1)
                .ok_or(QueueParseError::InvalidChain)?;
            data_len = data_len
                .checked_add(desc.len)
                .ok_or(QueueParseError::InvalidLayout)?;
        }

        cur = desc.next;
        if cur >= queue_size {
            return Err(QueueParseError::OutOfRange);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioBlkMmioError {
    Access(MmioAccessError),
    Queue(QueueParseError),
    Backend(IoError),
    GuestMemory(&'static str),
    Interrupt(&'static str),
    InvalidState(&'static str),
}

impl VirtioBlkMmioError {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Access(MmioAccessError::InvalidSize) => "virtio-blk: invalid MMIO access size",
            Self::Access(MmioAccessError::InvalidOffset) => {
                "virtio-blk: invalid MMIO register offset"
            }
            Self::Access(MmioAccessError::InvalidValue) => {
                "virtio-blk: invalid MMIO register value"
            }
            Self::Access(MmioAccessError::QueueNotReady) => "virtio-blk: queue is not ready",
            Self::Queue(QueueParseError::InvalidQueueSize) => "virtio-blk: invalid queue size",
            Self::Queue(QueueParseError::InvalidChain) => "virtio-blk: invalid descriptor chain",
            Self::Queue(QueueParseError::DescriptorLoop) => "virtio-blk: descriptor loop detected",
            Self::Queue(QueueParseError::IndirectUnsupported) => {
                "virtio-blk: indirect descriptors are unsupported"
            }
            Self::Queue(QueueParseError::OutOfRange) => "virtio-blk: descriptor index out of range",
            Self::Queue(QueueParseError::MissingStatus) => {
                "virtio-blk: status descriptor is missing"
            }
            Self::Queue(QueueParseError::InvalidRequestType) => {
                "virtio-blk: unsupported request type"
            }
            Self::Queue(QueueParseError::InvalidLayout) => "virtio-blk: invalid request layout",
            Self::Queue(QueueParseError::Align) => "virtio-blk: request buffer alignment error",
            Self::Backend(IoError::InvalidParam) => {
                "virtio-blk: backend rejected request parameters"
            }
            Self::Backend(IoError::OutOfRange) => "virtio-blk: backend request out of range",
            Self::Backend(IoError::Align) => "virtio-blk: backend alignment error",
            Self::Backend(IoError::Busy) => "virtio-blk: backend busy",
            Self::Backend(IoError::Timeout) => "virtio-blk: backend timeout",
            Self::Backend(IoError::Device) => "virtio-blk: backend device error",
            Self::Backend(IoError::ReadOnly) => "virtio-blk: backend is read-only",
            Self::Backend(IoError::Unsupported) => "virtio-blk: backend operation unsupported",
            Self::Backend(IoError::NoMemory) => "virtio-blk: backend out of memory",
            Self::Backend(IoError::Io) => "virtio-blk: backend I/O error",
            Self::Backend(IoError::Protocol) => "virtio-blk: backend protocol error",
            Self::Backend(IoError::NotReady) => "virtio-blk: backend is not ready",
            Self::Backend(IoError::Corrupted) => "virtio-blk: backend state is corrupted",
            Self::GuestMemory(msg) | Self::Interrupt(msg) | Self::InvalidState(msg) => msg,
        }
    }
}

impl From<MmioAccessError> for VirtioBlkMmioError {
    fn from(value: MmioAccessError) -> Self {
        Self::Access(value)
    }
}

impl From<QueueParseError> for VirtioBlkMmioError {
    fn from(value: QueueParseError) -> Self {
        Self::Queue(value)
    }
}

impl From<IoError> for VirtioBlkMmioError {
    fn from(value: IoError) -> Self {
        Self::Backend(value)
    }
}

pub trait VirtioBlkMmioGuestMemory {
    fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), VirtioBlkMmioError>;

    fn write(&self, addr: u64, value: &[u8]) -> Result<(), VirtioBlkMmioError>;
}

pub trait VirtioBlkMmioInterrupt {
    fn set_irq_level(&self, asserted: bool) -> Result<(), VirtioBlkMmioError>;
}

pub trait VirtioBlkMmioTrace: Sync {
    fn on_parse_failure(&self, _detail: &VirtioBlkParseFailureDetail) {}

    fn on_execution_failure(&self, _detail: &VirtioBlkExecutionFailureDetail) {}
}

pub struct NoopVirtioBlkMmioTrace;

impl VirtioBlkMmioTrace for NoopVirtioBlkMmioTrace {}

static NOOP_VIRTIO_BLK_MMIO_TRACE: NoopVirtioBlkMmioTrace = NoopVirtioBlkMmioTrace;

#[derive(Clone, Copy, Debug)]
struct QueueState {
    size: u16,
    ready: bool,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    last_avail_idx: u16,
    used_idx: u16,
}

impl QueueState {
    const fn new() -> Self {
        Self {
            size: 0,
            ready: false,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            last_avail_idx: 0,
            used_idx: 0,
        }
    }

    fn reset_runtime(&mut self) {
        self.ready = false;
        self.last_avail_idx = 0;
        self.used_idx = 0;
    }

    fn reset_all(&mut self) {
        self.size = 0;
        self.desc_addr = 0;
        self.avail_addr = 0;
        self.used_addr = 0;
        self.reset_runtime();
    }
}

struct VirtioBlkBackend<'a, B: BlockDevice + ?Sized> {
    dev: &'a B,
    capacity_sectors: u64,
    read_only: bool,
}

impl<'a, B> VirtioBlkBackend<'a, B>
where
    B: BlockDevice + ?Sized,
{
    fn new(dev: &'a B) -> Result<Self, VirtioBlkMmioError> {
        if dev.block_size() != SECTOR_SIZE {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: backend block size must be 512 bytes",
            ));
        }
        let read_only = dev.is_read_only()?;
        Ok(Self {
            dev,
            capacity_sectors: dev.num_blocks(),
            read_only,
        })
    }

    fn read_sectors(&self, lba: u64, dst: &mut [u8]) -> Result<(), VirtioBlkMmioError> {
        self.validate_range(lba, dst.len())?;
        // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`, and the backend contract
        // guarantees that successful reads initialize the full destination buffer.
        let dst_uninit = unsafe {
            core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut MaybeUninit<u8>, dst.len())
        };
        self.dev.read_at(lba, dst_uninit).map_err(Into::into)
    }

    fn write_sectors(&self, lba: u64, src: &[u8]) -> Result<(), VirtioBlkMmioError> {
        self.validate_range(lba, src.len())?;
        if self.read_only {
            return Err(VirtioBlkMmioError::Backend(IoError::ReadOnly));
        }
        self.dev.write_at(lba, src).map_err(Into::into)
    }

    fn flush(&self) -> Result<(), VirtioBlkMmioError> {
        self.dev.flush().map_err(Into::into)
    }

    fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    fn validate_range(&self, lba: u64, bytes: usize) -> Result<(), VirtioBlkMmioError> {
        if !bytes.is_multiple_of(SECTOR_SIZE) {
            return Err(VirtioBlkMmioError::Backend(IoError::Align));
        }
        let sectors = (bytes / SECTOR_SIZE) as u64;
        if lba
            .checked_add(sectors)
            .filter(|end| *end <= self.capacity_sectors)
            .is_none()
        {
            return Err(VirtioBlkMmioError::Backend(IoError::OutOfRange));
        }
        Ok(())
    }
}

pub struct VirtioBlkMmioDevice<'a, B, M, I>
where
    B: BlockDevice + ?Sized,
    M: VirtioBlkMmioGuestMemory,
    I: VirtioBlkMmioInterrupt,
{
    backend: VirtioBlkBackend<'a, B>,
    memory: M,
    interrupt: I,
    trace: &'a dyn VirtioBlkMmioTrace,
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    queue: QueueState,
    interrupt_status: u32,
    config_generation: u32,
}

impl<'a, B, M, I> VirtioBlkMmioDevice<'a, B, M, I>
where
    B: BlockDevice + ?Sized,
    M: VirtioBlkMmioGuestMemory,
    I: VirtioBlkMmioInterrupt,
{
    pub fn new(backend: &'a B, memory: M, interrupt: I) -> Result<Self, VirtioBlkMmioError> {
        Self::new_with_trace(backend, memory, interrupt, &NOOP_VIRTIO_BLK_MMIO_TRACE)
    }

    pub fn new_with_trace(
        backend: &'a B,
        memory: M,
        interrupt: I,
        trace: &'a dyn VirtioBlkMmioTrace,
    ) -> Result<Self, VirtioBlkMmioError> {
        Ok(Self {
            backend: VirtioBlkBackend::new(backend)?,
            memory,
            interrupt,
            trace,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue: QueueState::new(),
            interrupt_status: 0,
            config_generation: 0,
        })
    }

    pub fn mmio_read(&mut self, offset: usize, size: u8) -> Result<u64, VirtioBlkMmioError> {
        if offset < REG_CONFIG_SPACE {
            validate_register_access(offset, size)?;
        } else {
            validate_config_access(offset, size)?;
        }

        // Only queue zero is implemented; other queue selections expose zeroed state.
        let queue_value = |value| if self.queue_sel == 0 { value } else { 0 };
        let value = match offset {
            REG_MAGIC_VALUE => VIRTIO_MMIO_MAGIC_VALUE as u64,
            REG_VERSION => VIRTIO_MMIO_VERSION_MODERN as u64,
            REG_DEVICE_ID => VIRTIO_MMIO_DEVICE_ID_BLOCK as u64,
            REG_VENDOR_ID => VIRTIO_MMIO_VENDOR_ID as u64,
            REG_DEVICE_FEATURES => {
                let policy = FeaturePolicy {
                    allow_read_only: self.backend.read_only,
                };
                select_feature_bits(device_features(policy), self.device_features_sel) as u64
            }
            REG_DEVICE_FEATURES_SEL => self.device_features_sel as u64,
            REG_DRIVER_FEATURES => {
                select_feature_bits(self.driver_features, self.driver_features_sel) as u64
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel as u64,
            REG_QUEUE_SEL => self.queue_sel as u64,
            REG_QUEUE_NUM_MAX => queue_value(QUEUE_MAX as u64),
            REG_QUEUE_NUM => queue_value(self.queue.size as u64),
            REG_QUEUE_READY => queue_value(u64::from(self.queue.ready)),
            REG_INTERRUPT_STATUS => self.interrupt_status as u64,
            REG_STATUS => self.status as u64,
            REG_QUEUE_DESC_LOW => queue_value(self.queue.desc_addr as u32 as u64),
            REG_QUEUE_DESC_HIGH => queue_value((self.queue.desc_addr >> 32) as u32 as u64),
            REG_QUEUE_DRIVER_LOW => queue_value(self.queue.avail_addr as u32 as u64),
            REG_QUEUE_DRIVER_HIGH => queue_value((self.queue.avail_addr >> 32) as u32 as u64),
            REG_QUEUE_DEVICE_LOW => queue_value(self.queue.used_addr as u32 as u64),
            REG_QUEUE_DEVICE_HIGH => queue_value((self.queue.used_addr >> 32) as u32 as u64),
            REG_CONFIG_GENERATION => self.config_generation as u64,
            _ if offset >= REG_CONFIG_SPACE => self.read_config(offset, size)?,
            _ => 0,
        };
        Ok(value)
    }

    pub fn mmio_write(
        &mut self,
        offset: usize,
        size: u8,
        value: u64,
    ) -> Result<(), VirtioBlkMmioError> {
        if offset < REG_CONFIG_SPACE {
            validate_register_access(offset, size)?;
        } else {
            validate_config_access(offset, size)?;
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: config is read-only",
            ));
        }
        let value = u32::try_from(value).map_err(|_| MmioAccessError::InvalidValue)?;

        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES => {
                self.driver_features =
                    merge_driver_features(self.driver_features, self.driver_features_sel, value)?;
            }
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if self.queue_sel != 0 {
                    return Err(VirtioBlkMmioError::InvalidState(
                        "virtio-blk: only queue 0 is supported",
                    ));
                }
                let queue_size = u16::try_from(value).map_err(|_| MmioAccessError::InvalidValue)?;
                validate_queue_size(queue_size, QUEUE_MAX)?;
                self.queue.size = queue_size;
            }
            REG_QUEUE_READY => self.set_queue_ready(value)?,
            REG_QUEUE_NOTIFY => self.process_queue_notify(value)?,
            REG_INTERRUPT_ACK => self.ack_interrupt(value)?,
            REG_STATUS => self.write_status(value)?,
            REG_QUEUE_DESC_LOW => {
                if self.queue_sel == 0 {
                    self.queue.desc_addr = (self.queue.desc_addr & !0xffff_ffffu64) | value as u64;
                }
            }
            REG_QUEUE_DESC_HIGH => {
                if self.queue_sel == 0 {
                    self.queue.desc_addr =
                        (self.queue.desc_addr & 0xffff_ffffu64) | ((value as u64) << 32);
                }
            }
            REG_QUEUE_DRIVER_LOW => {
                if self.queue_sel == 0 {
                    self.queue.avail_addr =
                        (self.queue.avail_addr & !0xffff_ffffu64) | value as u64;
                }
            }
            REG_QUEUE_DRIVER_HIGH => {
                if self.queue_sel == 0 {
                    self.queue.avail_addr =
                        (self.queue.avail_addr & 0xffff_ffffu64) | ((value as u64) << 32);
                }
            }
            REG_QUEUE_DEVICE_LOW => {
                if self.queue_sel == 0 {
                    self.queue.used_addr = (self.queue.used_addr & !0xffff_ffffu64) | value as u64;
                }
            }
            REG_QUEUE_DEVICE_HIGH => {
                if self.queue_sel == 0 {
                    self.queue.used_addr =
                        (self.queue.used_addr & 0xffff_ffffu64) | ((value as u64) << 32);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn read_config(&self, offset: usize, size: u8) -> Result<u64, VirtioBlkMmioError> {
        let cfg_offset =
            offset
                .checked_sub(REG_CONFIG_SPACE)
                .ok_or(VirtioBlkMmioError::InvalidState(
                    "virtio-blk: config offset underflow",
                ))?;
        let mut bytes = [0u8; size_of::<VirtioBlkConfig>()];
        bytes[..8].copy_from_slice(&self.backend.capacity_sectors().to_le_bytes());

        let mut value = 0u64;
        for idx in 0..size as usize {
            value |= (bytes[cfg_offset + idx] as u64) << (idx * 8);
        }
        Ok(value)
    }

    fn write_status(&mut self, value: u32) -> Result<(), VirtioBlkMmioError> {
        if value == 0 {
            self.reset()?;
            return Ok(());
        }
        self.status = value;
        if (self.status & VIRTIO_STATUS_FEATURES_OK) != 0 {
            let policy = FeaturePolicy {
                allow_read_only: self.backend.read_only,
            };
            let device = device_features(policy);
            if validate_negotiated_features(device, self.driver_features).is_err() {
                self.status &= !VIRTIO_STATUS_FEATURES_OK;
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), VirtioBlkMmioError> {
        self.status = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.queue.reset_all();
        self.set_interrupt_status(0)
    }

    fn set_queue_ready(&mut self, value: u32) -> Result<(), VirtioBlkMmioError> {
        if self.queue_sel != 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: only queue 0 is supported",
            ));
        }
        match value {
            0 => {
                self.queue.reset_runtime();
                Ok(())
            }
            1 => {
                validate_queue_size(self.queue.size, QUEUE_MAX)?;
                if self.queue.desc_addr == 0
                    || self.queue.avail_addr == 0
                    || self.queue.used_addr == 0
                {
                    return Err(VirtioBlkMmioError::InvalidState(
                        "virtio-blk: queue address is not configured",
                    ));
                }
                self.validate_queue_alignment()?;
                self.queue.ready = true;
                self.queue.last_avail_idx = self.read_guest_u16(self.queue.avail_addr + 2)?;
                self.queue.used_idx = self.read_guest_u16(self.queue.used_addr + 2)?;
                Ok(())
            }
            _ => Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: queue_ready accepts only 0 or 1",
            )),
        }
    }

    fn validate_queue_alignment(&self) -> Result<(), VirtioBlkMmioError> {
        if (self.queue.desc_addr & 0xf) != 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: descriptor table is not 16-byte aligned",
            ));
        }
        if (self.queue.avail_addr & 0x1) != 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: avail ring is not 2-byte aligned",
            ));
        }
        if (self.queue.used_addr & 0x3) != 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: used ring is not 4-byte aligned",
            ));
        }
        Ok(())
    }

    fn ack_interrupt(&mut self, ack: u32) -> Result<(), VirtioBlkMmioError> {
        let status = ack_interrupt_bits(self.interrupt_status, ack);
        self.set_interrupt_status(status)
    }

    fn set_interrupt_status(&mut self, status: u32) -> Result<(), VirtioBlkMmioError> {
        let had_interrupt = self.interrupt_status != 0;
        let has_interrupt = status != 0;
        self.interrupt_status = status;
        if had_interrupt == has_interrupt {
            return Ok(());
        }
        self.interrupt.set_irq_level(has_interrupt)
    }

    fn process_queue_notify(&mut self, queue_index: u32) -> Result<(), VirtioBlkMmioError> {
        if queue_index != 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: only queue 0 notify is supported",
            ));
        }
        if !self.queue.ready {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: queue is not ready",
            ));
        }
        if (self.status & VIRTIO_STATUS_DRIVER_OK) == 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: driver is not ready",
            ));
        }

        // Ensure the device observes the guest's descriptor table and avail-ring writes in
        // normal memory before consuming the avail index published by the queue notify.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let avail_idx = self.read_guest_u16(self.queue.avail_addr + 2)?;
        let pending = avail_idx.wrapping_sub(self.queue.last_avail_idx);
        if pending == 0 {
            return Ok(());
        }
        if pending as u32 > self.queue.size as u32 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: avail ring overrun",
            ));
        }

        for _ in 0..pending {
            let ring_index = calc_ring_index(self.queue.last_avail_idx, self.queue.size)?;
            let ring_addr = avail_ring_entry_addr(self.queue.avail_addr, ring_index);
            let head = self.read_guest_u16(ring_addr)?;
            let outcome = self.process_one_request(head);

            if let Some(status_addr) = outcome.status_addr {
                self.write_guest_u8(status_addr, outcome.status)?;
            }
            let update = used_update(
                self.queue.used_addr,
                self.queue.size,
                self.queue.used_idx,
                head,
                outcome.used_len,
            )?;
            self.write_guest_u32(update.used_elem_addr, update.id)?;
            self.write_guest_u32(
                update
                    .used_elem_addr
                    .checked_add(4)
                    .ok_or(VirtioBlkMmioError::InvalidState(
                        "virtio-blk: used elem overflow",
                    ))?,
                update.len,
            )?;
            self.write_guest_u16(update.used_idx_addr, update.new_used_idx)?;
            self.queue.used_idx = update.new_used_idx;
            self.queue.last_avail_idx = self.queue.last_avail_idx.wrapping_add(1);
        }

        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.set_interrupt_status(self.interrupt_status | VIRTIO_INT_USED_RING)
    }

    fn process_one_request(&self, head: u16) -> RequestOutcome {
        let status_addr_hint = self.find_status_addr_best_effort(head);
        let mut desc_reader = |idx: u16| self.read_descriptor(idx).ok();
        let mut req_reader = |addr: u64| self.read_req_header(addr).ok();
        let parsed = parse_req_layout(self.queue.size, head, &mut desc_reader, &mut req_reader);
        let layout = match parsed {
            Ok(layout) => layout,
            Err(err) => {
                let detail = self.collect_parse_failure_detail(head, status_addr_hint, err);
                self.trace.on_parse_failure(&detail);
                return RequestOutcome {
                    status: detail.mapped_status,
                    status_addr: detail.status_addr_hint,
                    used_len: 0,
                };
            }
        };
        self.execute_request(head, &layout)
    }

    fn execute_request(&self, head: u16, layout: &VirtioBlkReqLayout) -> RequestOutcome {
        match layout.req_type {
            VIRTIO_BLK_T_IN => {
                let io_res = (|| -> Result<(), VirtioBlkMmioError> {
                    let mut cursor = RequestDataCursor::new(self, layout)?;
                    let mut staging = [0u8; SECTOR_SIZE];
                    let sector_count = layout.data_len as usize / SECTOR_SIZE;
                    for sector_idx in 0..sector_count {
                        self.backend
                            .read_sectors(layout.sector + sector_idx as u64, &mut staging)?;
                        cursor.write_to_guest(&staging)?;
                    }
                    Ok(())
                })();
                self.map_io_to_outcome(head, layout, io_res, layout.data_len)
            }
            VIRTIO_BLK_T_OUT => {
                let io_res = (|| -> Result<(), VirtioBlkMmioError> {
                    let mut cursor = RequestDataCursor::new(self, layout)?;
                    let mut staging = [0u8; SECTOR_SIZE];
                    let sector_count = layout.data_len as usize / SECTOR_SIZE;
                    for sector_idx in 0..sector_count {
                        cursor.read_from_guest(&mut staging)?;
                        self.backend
                            .write_sectors(layout.sector + sector_idx as u64, &staging)?;
                    }
                    Ok(())
                })();
                self.map_io_to_outcome(head, layout, io_res, layout.data_len)
            }
            VIRTIO_BLK_T_FLUSH => {
                let io_res = self.backend.flush();
                self.map_io_to_outcome(head, layout, io_res, 0)
            }
            _ => {
                let detail = VirtioBlkExecutionFailureDetail {
                    head,
                    layout: *layout,
                    mapped_status: VIRTIO_BLK_S_UNSUPP,
                    err: VirtioBlkMmioError::Queue(QueueParseError::InvalidRequestType),
                };
                self.trace.on_execution_failure(&detail);
                RequestOutcome {
                    status: VIRTIO_BLK_S_UNSUPP,
                    status_addr: Some(layout.status_addr),
                    used_len: 0,
                }
            }
        }
    }

    fn map_io_to_outcome(
        &self,
        head: u16,
        layout: &VirtioBlkReqLayout,
        io_result: Result<(), VirtioBlkMmioError>,
        used_len: u32,
    ) -> RequestOutcome {
        match io_result {
            Ok(()) => RequestOutcome {
                status: VIRTIO_BLK_S_OK,
                status_addr: Some(layout.status_addr),
                used_len,
            },
            Err(err) => {
                let mapped_status = match err {
                    VirtioBlkMmioError::Backend(IoError::Unsupported) => VIRTIO_BLK_S_UNSUPP,
                    _ => VIRTIO_BLK_S_IOERR,
                };
                let detail = VirtioBlkExecutionFailureDetail {
                    head,
                    layout: *layout,
                    mapped_status,
                    err,
                };
                self.trace.on_execution_failure(&detail);
                RequestOutcome {
                    status: mapped_status,
                    status_addr: Some(layout.status_addr),
                    used_len: 0,
                }
            }
        }
    }

    fn collect_parse_failure_detail(
        &self,
        head: u16,
        status_addr_hint: Option<u64>,
        err: QueueParseError,
    ) -> VirtioBlkParseFailureDetail {
        let mut descriptors = [None; PARSE_FAILURE_DESCRIPTOR_CAPTURE_LIMIT];
        let mut descriptor_count_captured = 0u8;
        let mut seen = [0u16; PARSE_FAILURE_DESCRIPTOR_CAPTURE_LIMIT];
        let mut seen_count = 0usize;
        let mut cur = head;
        let mut header_addr = None;
        let mut header_readable = false;
        let mut header_raw = None;

        while usize::from(descriptor_count_captured) < PARSE_FAILURE_DESCRIPTOR_CAPTURE_LIMIT {
            if cur >= self.queue.size {
                break;
            }
            if seen[..seen_count].contains(&cur) {
                break;
            }

            seen[seen_count] = cur;
            seen_count += 1;

            let slot = usize::from(descriptor_count_captured);
            descriptor_count_captured = descriptor_count_captured.wrapping_add(1);

            let Ok(desc) = self.read_descriptor(cur) else {
                break;
            };
            descriptors[slot] = Some(desc);

            if slot == 0 {
                header_addr = Some(desc.addr);
                header_readable = (desc.flags & VIRTQ_DESC_F_WRITE) == 0
                    && desc.len >= VIRTIO_BLK_REQ_HEADER_RAW_LEN as u32
                    && desc.addr != 0;
                if header_readable {
                    let mut raw = [0u8; VIRTIO_BLK_REQ_HEADER_RAW_LEN];
                    if self.memory.read(desc.addr, &mut raw).is_ok() {
                        header_raw = Some(raw);
                    }
                }
            }

            if (desc.flags & VIRTQ_DESC_F_NEXT) == 0 {
                break;
            }
            if desc.next >= self.queue.size {
                break;
            }
            cur = desc.next;
        }

        VirtioBlkParseFailureDetail {
            head,
            queue_size: self.queue.size,
            status_addr_hint,
            mapped_status: parse_error_status(err),
            parse_error: err,
            header_addr,
            header_readable,
            header_raw,
            descriptor_snapshot: VirtioBlkDescriptorSnapshot {
                descriptor_count_captured,
                descriptors,
            },
        }
    }

    fn find_status_addr_best_effort(&self, head: u16) -> Option<u64> {
        if head >= self.queue.size {
            return None;
        }

        let mut visited = 0u16;
        let mut cur = head;
        loop {
            if visited >= self.queue.size {
                return None;
            }
            visited = visited.wrapping_add(1);

            let desc = self.read_descriptor(cur).ok()?;
            if (desc.flags & VIRTQ_DESC_F_INDIRECT) != 0 {
                return None;
            }
            if (desc.flags & VIRTQ_DESC_F_NEXT) == 0 {
                return ((desc.flags & VIRTQ_DESC_F_WRITE) != 0 && desc.len == 1)
                    .then_some(desc.addr);
            }

            cur = desc.next;
            if cur >= self.queue.size {
                return None;
            }
        }
    }
}

struct RequestDataCursor<'device, 'backend, B, M, I>
where
    B: BlockDevice + ?Sized,
    M: VirtioBlkMmioGuestMemory,
    I: VirtioBlkMmioInterrupt,
{
    device: &'device VirtioBlkMmioDevice<'backend, B, M, I>,
    layout: &'device VirtioBlkReqLayout,
    current_desc: Option<VirtqDesc>,
    offset_in_desc: usize,
    remaining: usize,
}

impl<'device, 'backend, B, M, I> RequestDataCursor<'device, 'backend, B, M, I>
where
    B: BlockDevice + ?Sized,
    M: VirtioBlkMmioGuestMemory,
    I: VirtioBlkMmioInterrupt,
{
    fn new(
        device: &'device VirtioBlkMmioDevice<'backend, B, M, I>,
        layout: &'device VirtioBlkReqLayout,
    ) -> Result<Self, VirtioBlkMmioError> {
        let current_desc = match layout.first_data_desc_idx {
            Some(desc_idx) => Some(Self::read_and_validate_data_desc(device, layout, desc_idx)?),
            None => None,
        };
        Ok(Self {
            device,
            layout,
            current_desc,
            offset_in_desc: 0,
            remaining: layout.data_len as usize,
        })
    }

    fn read_from_guest(&mut self, dst: &mut [u8]) -> Result<(), VirtioBlkMmioError> {
        if dst.len() > self.remaining {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: request data shorter than parsed length",
            ));
        }

        let mut copied = 0usize;
        while copied < dst.len() {
            let desc = self.current_desc.ok_or(VirtioBlkMmioError::InvalidState(
                "virtio-blk: request data chain ended before transfer completed",
            ))?;
            let desc_len = desc.len as usize;
            if self.offset_in_desc > desc_len {
                return Err(VirtioBlkMmioError::InvalidState(
                    "virtio-blk: request data cursor exceeded descriptor length",
                ));
            }

            let available = desc_len - self.offset_in_desc;
            if available == 0 {
                self.advance_desc()?;
                continue;
            }

            let chunk = available.min(dst.len() - copied);
            let addr = desc.addr.checked_add(self.offset_in_desc as u64).ok_or(
                VirtioBlkMmioError::GuestMemory("virtio-blk: guest data address overflow"),
            )?;
            let dst_chunk = &mut dst[copied..copied + chunk];
            self.device.memory.read(addr, dst_chunk)?;

            copied += chunk;
            self.remaining -= chunk;
            self.offset_in_desc += chunk;
            if self.offset_in_desc == desc_len {
                self.advance_desc()?;
            }
        }

        Ok(())
    }

    fn write_to_guest(&mut self, src: &[u8]) -> Result<(), VirtioBlkMmioError> {
        if src.len() > self.remaining {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: request data shorter than parsed length",
            ));
        }

        let mut copied = 0usize;
        while copied < src.len() {
            let desc = self.current_desc.ok_or(VirtioBlkMmioError::InvalidState(
                "virtio-blk: request data chain ended before transfer completed",
            ))?;
            let desc_len = desc.len as usize;
            if self.offset_in_desc > desc_len {
                return Err(VirtioBlkMmioError::InvalidState(
                    "virtio-blk: request data cursor exceeded descriptor length",
                ));
            }

            let available = desc_len - self.offset_in_desc;
            if available == 0 {
                self.advance_desc()?;
                continue;
            }

            let chunk = available.min(src.len() - copied);
            let addr = desc.addr.checked_add(self.offset_in_desc as u64).ok_or(
                VirtioBlkMmioError::GuestMemory("virtio-blk: guest data address overflow"),
            )?;
            let src_chunk = &src[copied..copied + chunk];
            self.device.memory.write(addr, src_chunk)?;

            copied += chunk;
            self.remaining -= chunk;
            self.offset_in_desc += chunk;
            if self.offset_in_desc == desc_len {
                self.advance_desc()?;
            }
        }

        Ok(())
    }

    fn advance_desc(&mut self) -> Result<(), VirtioBlkMmioError> {
        let desc = self.current_desc.ok_or(VirtioBlkMmioError::InvalidState(
            "virtio-blk: request data chain ended before transfer completed",
        ))?;
        if (desc.flags & VIRTQ_DESC_F_NEXT) == 0 {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: data descriptor chain terminated before status descriptor",
            ));
        }

        let next_idx = desc.next;
        if next_idx == self.layout.status_desc_idx {
            self.current_desc = None;
            self.offset_in_desc = 0;
            return Ok(());
        }

        self.current_desc = Some(Self::read_and_validate_data_desc(
            self.device,
            self.layout,
            next_idx,
        )?);
        self.offset_in_desc = 0;
        Ok(())
    }

    fn read_and_validate_data_desc(
        device: &VirtioBlkMmioDevice<'backend, B, M, I>,
        layout: &VirtioBlkReqLayout,
        desc_idx: u16,
    ) -> Result<VirtqDesc, VirtioBlkMmioError> {
        if desc_idx == layout.status_desc_idx {
            return Err(VirtioBlkMmioError::InvalidState(
                "virtio-blk: status descriptor reached while walking request data",
            ));
        }

        let desc = device.read_descriptor(desc_idx)?;
        if (desc.flags & VIRTQ_DESC_F_INDIRECT) != 0 {
            return Err(VirtioBlkMmioError::Queue(
                QueueParseError::IndirectUnsupported,
            ));
        }

        match layout.req_type {
            VIRTIO_BLK_T_IN => {
                if (desc.flags & VIRTQ_DESC_F_WRITE) == 0 {
                    return Err(VirtioBlkMmioError::Queue(QueueParseError::InvalidLayout));
                }
            }
            VIRTIO_BLK_T_OUT => {
                if (desc.flags & VIRTQ_DESC_F_WRITE) != 0 {
                    return Err(VirtioBlkMmioError::Queue(QueueParseError::InvalidLayout));
                }
            }
            _ => {
                return Err(VirtioBlkMmioError::Queue(QueueParseError::InvalidLayout));
            }
        }

        Ok(desc)
    }
}

impl<'a, B, M, I> VirtioBlkMmioDevice<'a, B, M, I>
where
    B: BlockDevice + ?Sized,
    M: VirtioBlkMmioGuestMemory,
    I: VirtioBlkMmioInterrupt,
{
    fn read_guest_u16(&self, addr: u64) -> Result<u16, VirtioBlkMmioError> {
        let mut bytes = [0u8; 2];
        self.memory.read(addr, &mut bytes)?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn write_guest_u16(&self, addr: u64, value: u16) -> Result<(), VirtioBlkMmioError> {
        self.memory.write(addr, &value.to_le_bytes())
    }

    fn write_guest_u32(&self, addr: u64, value: u32) -> Result<(), VirtioBlkMmioError> {
        self.memory.write(addr, &value.to_le_bytes())
    }

    fn write_guest_u8(&self, addr: u64, value: u8) -> Result<(), VirtioBlkMmioError> {
        self.memory.write(addr, &[value])
    }

    fn read_descriptor(&self, idx: u16) -> Result<VirtqDesc, VirtioBlkMmioError> {
        if idx >= self.queue.size {
            return Err(VirtioBlkMmioError::Queue(QueueParseError::OutOfRange));
        }
        let offset = (idx as u64)
            .checked_mul(size_of::<VirtqDesc>() as u64)
            .ok_or(VirtioBlkMmioError::GuestMemory(
                "virtio-blk: descriptor offset overflow",
            ))?;
        let addr =
            self.queue
                .desc_addr
                .checked_add(offset)
                .ok_or(VirtioBlkMmioError::GuestMemory(
                    "virtio-blk: descriptor address overflow",
                ))?;
        let mut bytes = [0u8; size_of::<VirtqDesc>()];
        self.memory.read(addr, &mut bytes)?;
        Ok(VirtqDesc {
            addr: u64::from_le_bytes(bytes[0..8].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid descriptor address bytes")
            })?),
            len: u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid descriptor length bytes")
            })?),
            flags: u16::from_le_bytes(bytes[12..14].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid descriptor flags bytes")
            })?),
            next: u16::from_le_bytes(bytes[14..16].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid descriptor next bytes")
            })?),
        })
    }

    fn read_req_header(&self, addr: u64) -> Result<VirtioBlkReq, VirtioBlkMmioError> {
        let mut bytes = [0u8; size_of::<VirtioBlkReq>()];
        self.memory.read(addr, &mut bytes)?;
        Ok(VirtioBlkReq {
            req_type: u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid request type bytes")
            })?),
            reserved: u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid request reserved bytes")
            })?),
            sector: u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk: invalid request sector bytes")
            })?),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestOutcome {
    status: u8,
    status_addr: Option<u64>,
    used_len: u32,
}

fn parse_error_status(err: QueueParseError) -> u8 {
    match err {
        QueueParseError::InvalidRequestType => VIRTIO_BLK_S_UNSUPP,
        _ => VIRTIO_BLK_S_IOERR,
    }
}

fn validate_register_access(offset: usize, size: u8) -> Result<(), VirtioBlkMmioError> {
    if size != 4 {
        return Err(VirtioBlkMmioError::Access(MmioAccessError::InvalidSize));
    }
    if (offset & 0x3) != 0 {
        return Err(VirtioBlkMmioError::Access(MmioAccessError::InvalidOffset));
    }
    Ok(())
}

fn validate_config_access(offset: usize, size: u8) -> Result<(), VirtioBlkMmioError> {
    if !matches!(size, 1 | 2 | 4) {
        return Err(VirtioBlkMmioError::Access(MmioAccessError::InvalidSize));
    }
    let cfg_offset =
        offset
            .checked_sub(REG_CONFIG_SPACE)
            .ok_or(VirtioBlkMmioError::InvalidState(
                "virtio-blk: config offset underflow",
            ))?;
    let cfg_end = cfg_offset
        .checked_add(size as usize)
        .ok_or(VirtioBlkMmioError::InvalidState(
            "virtio-blk: config access overflow",
        ))?;
    if cfg_end > size_of::<VirtioBlkConfig>() {
        return Err(VirtioBlkMmioError::Access(MmioAccessError::InvalidOffset));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::vec;
    use std::vec::Vec;

    fn make_req(req_type: u32, sector: u64) -> VirtioBlkReq {
        VirtioBlkReq {
            req_type,
            reserved: 0,
            sector,
        }
    }

    fn desc(addr: u64, len: u32, flags: u16, next: u16) -> VirtqDesc {
        VirtqDesc {
            addr,
            len,
            flags,
            next,
        }
    }

    #[test]
    fn features_require_version_1() {
        let device = VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_RO;
        let driver_ok = VIRTIO_F_VERSION_1;
        let driver_bad = 0;
        assert!(validate_negotiated_features(device, driver_ok).is_ok());
        assert_eq!(
            validate_negotiated_features(device, driver_bad),
            Err(MmioAccessError::InvalidValue)
        );
    }

    #[test]
    fn merge_driver_features_keeps_64bit_state() {
        let v0 = merge_driver_features(0, 0, 0x1122_3344).unwrap();
        let v1 = merge_driver_features(v0, 1, 0x5566_7788).unwrap();
        assert_eq!(v1, 0x5566_7788_1122_3344u64);
    }

    #[test]
    fn queue_size_must_be_pow2() {
        assert!(validate_queue_size(8, 256).is_ok());
        assert_eq!(
            validate_queue_size(7, 256),
            Err(QueueParseError::InvalidQueueSize)
        );
    }

    #[test]
    fn parse_multi_desc_read_request() {
        let req = make_req(VIRTIO_BLK_T_IN, 32);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x2000, 256, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2),
            desc(0x2100, 256, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 3),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        let parsed = parse_req_layout(8, 0, &mut get_desc, &mut read_req).unwrap();
        assert_eq!(parsed.req_type, VIRTIO_BLK_T_IN);
        assert_eq!(parsed.first_data_desc_idx, Some(1));
        assert_eq!(parsed.status_desc_idx, 3);
        assert_eq!(parsed.data_len, SECTOR_SIZE as u32);
        assert_eq!(parsed.status_addr, 0x3000);
    }

    #[test]
    fn parse_multi_desc_write_request() {
        let req = make_req(VIRTIO_BLK_T_OUT, 9);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x2000, 768, VIRTQ_DESC_F_NEXT, 2),
            desc(0x2300, 256, VIRTQ_DESC_F_NEXT, 3),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        let parsed = parse_req_layout(8, 0, &mut get_desc, &mut read_req).unwrap();
        assert_eq!(parsed.req_type, VIRTIO_BLK_T_OUT);
        assert_eq!(parsed.first_data_desc_idx, Some(1));
        assert_eq!(parsed.status_desc_idx, 3);
        assert_eq!(parsed.data_len, (SECTOR_SIZE * 2) as u32);
        assert_eq!(parsed.status_addr, 0x3000);
        assert_eq!(parsed.sector, 9);
    }

    #[test]
    fn parse_flush_two_desc_request() {
        let req = make_req(VIRTIO_BLK_T_FLUSH, 0);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        let parsed = parse_req_layout(8, 0, &mut get_desc, &mut read_req).unwrap();
        assert_eq!(parsed.req_type, VIRTIO_BLK_T_FLUSH);
        assert_eq!(parsed.first_data_desc_idx, None);
        assert_eq!(parsed.data_len, 0);
        assert_eq!(parsed.status_desc_idx, 1);
    }

    #[test]
    fn reject_in_request_with_readable_data_desc() {
        let req = make_req(VIRTIO_BLK_T_IN, 32);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x2000, SECTOR_SIZE as u32, VIRTQ_DESC_F_NEXT, 2),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(8, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::InvalidLayout)
        );
    }

    #[test]
    fn reject_out_request_with_writable_data_desc() {
        let req = make_req(VIRTIO_BLK_T_OUT, 32);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(
                0x2000,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                2,
            ),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(8, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::InvalidLayout)
        );
    }

    #[test]
    fn reject_misaligned_total_data_length() {
        let req = make_req(VIRTIO_BLK_T_OUT, 2);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x2000, 256, VIRTQ_DESC_F_NEXT, 2),
            desc(0x2100, 128, VIRTQ_DESC_F_NEXT, 3),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(8, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::Align)
        );
    }

    #[test]
    fn reject_status_descriptor_that_is_not_final() {
        let req = make_req(VIRTIO_BLK_T_OUT, 2);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x2000, SECTOR_SIZE as u32, VIRTQ_DESC_F_NEXT, 2),
            desc(0x3000, 1, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 3),
            desc(0x4000, SECTOR_SIZE as u32, 0, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(8, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::InvalidLayout)
        );
    }

    #[test]
    fn reject_indirect_descriptor() {
        let req = make_req(VIRTIO_BLK_T_IN, 1);
        let table = [desc(
            0x1000,
            size_of::<VirtioBlkReq>() as u32,
            VIRTQ_DESC_F_INDIRECT,
            0,
        )];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(1, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::IndirectUnsupported)
        );
    }

    #[test]
    fn reject_descriptor_loop() {
        let req = make_req(VIRTIO_BLK_T_IN, 1);
        let table = [desc(
            0x1000,
            size_of::<VirtioBlkReq>() as u32,
            VIRTQ_DESC_F_NEXT,
            0,
        )];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(1, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::DescriptorLoop)
        );
    }

    #[test]
    fn reject_status_not_writable() {
        let req = make_req(VIRTIO_BLK_T_IN, 2);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x2000, 512, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2),
            desc(0x3000, 1, 0, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(8, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::MissingStatus)
        );
    }

    #[test]
    fn reject_non_flush_two_desc_chain() {
        let req = make_req(VIRTIO_BLK_T_OUT, 0);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        assert_eq!(
            parse_req_layout(8, 0, &mut get_desc, &mut read_req),
            Err(QueueParseError::InvalidLayout)
        );
    }

    #[test]
    fn parse_req_layout_allows_zero_guest_address_for_data_desc() {
        let req = make_req(VIRTIO_BLK_T_IN, 32);
        let table = [
            desc(
                0x1000,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
            desc(
                0,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                2,
            ),
            desc(0x3000, 1, VIRTQ_DESC_F_WRITE, 0),
        ];
        let mut read_req = |addr: u64| (addr == 0x1000).then_some(req);
        let mut get_desc = |idx: u16| table.get(idx as usize).copied();
        let parsed = parse_req_layout(8, 0, &mut get_desc, &mut read_req).unwrap();
        assert_eq!(parsed.first_data_desc_idx, Some(1));
        assert_eq!(parsed.data_len, SECTOR_SIZE as u32);
        assert_eq!(parsed.status_addr, 0x3000);
    }

    #[test]
    fn used_ring_update_wraps_index() {
        let update = used_update(0x5000, 8, 0xffff, 3, 4096).unwrap();
        assert_eq!(update.ring_index, 7);
        assert_eq!(update.new_used_idx, 0);
        assert_eq!(update.id, 3);
    }

    #[test]
    fn interrupt_ack_clears_only_requested_bits() {
        let old = VIRTIO_INT_USED_RING | (1 << 7);
        let new = ack_interrupt_bits(old, VIRTIO_INT_USED_RING);
        assert_eq!(new, 1 << 7);
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TraceEvent {
        Parse(VirtioBlkParseFailureDetail),
        Execution(VirtioBlkExecutionFailureDetail),
    }

    #[derive(Default)]
    struct FakeTrace {
        events: Mutex<Vec<TraceEvent>>,
    }

    impl FakeTrace {
        fn events(&self) -> Vec<TraceEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl VirtioBlkMmioTrace for FakeTrace {
        fn on_parse_failure(&self, detail: &VirtioBlkParseFailureDetail) {
            self.events.lock().unwrap().push(TraceEvent::Parse(*detail));
        }

        fn on_execution_failure(&self, detail: &VirtioBlkExecutionFailureDetail) {
            self.events
                .lock()
                .unwrap()
                .push(TraceEvent::Execution(*detail));
        }
    }

    struct FakeBlockDevice {
        storage: Mutex<Vec<u8>>,
        read_only: bool,
        read_error: Option<IoError>,
        flush_error: Option<IoError>,
    }

    impl FakeBlockDevice {
        fn new(storage: Vec<u8>, read_only: bool) -> Self {
            assert_eq!(storage.len() % SECTOR_SIZE, 0);
            Self {
                storage: Mutex::new(storage),
                read_only,
                read_error: None,
                flush_error: None,
            }
        }

        fn with_read_error(mut self, err: IoError) -> Self {
            self.read_error = Some(err);
            self
        }

        fn with_flush_error(mut self, err: IoError) -> Self {
            self.flush_error = Some(err);
            self
        }
    }

    impl BlockDevice for FakeBlockDevice {
        fn init(&mut self) -> Result<(), IoError> {
            Ok(())
        }

        fn block_size(&self) -> usize {
            SECTOR_SIZE
        }

        fn num_blocks(&self) -> u64 {
            (self.storage.lock().unwrap().len() / SECTOR_SIZE) as u64
        }

        fn read_at(&self, lba: u64, buf: &mut [MaybeUninit<u8>]) -> Result<(), IoError> {
            if let Some(err) = self.read_error {
                return Err(err);
            }
            if (buf.len() % SECTOR_SIZE) != 0 {
                return Err(IoError::Align);
            }
            let start = (lba as usize)
                .checked_mul(SECTOR_SIZE)
                .ok_or(IoError::OutOfRange)?;
            let end = start.checked_add(buf.len()).ok_or(IoError::OutOfRange)?;
            let storage = self.storage.lock().unwrap();
            let src = storage.get(start..end).ok_or(IoError::OutOfRange)?;
            for (dst, byte) in buf.iter_mut().zip(src.iter().copied()) {
                dst.write(byte);
            }
            Ok(())
        }

        fn write_at(&self, lba: u64, buf: &[u8]) -> Result<(), IoError> {
            if self.read_only {
                return Err(IoError::ReadOnly);
            }
            if (buf.len() % SECTOR_SIZE) != 0 {
                return Err(IoError::Align);
            }
            let start = (lba as usize)
                .checked_mul(SECTOR_SIZE)
                .ok_or(IoError::OutOfRange)?;
            let end = start.checked_add(buf.len()).ok_or(IoError::OutOfRange)?;
            let mut storage = self.storage.lock().unwrap();
            let dst = storage.get_mut(start..end).ok_or(IoError::OutOfRange)?;
            dst.copy_from_slice(buf);
            Ok(())
        }

        fn flush(&self) -> Result<(), IoError> {
            if let Some(err) = self.flush_error {
                return Err(err);
            }
            Ok(())
        }

        fn max_io_bytes(&self) -> Result<Option<usize>, IoError> {
            Ok(None)
        }

        fn is_read_only(&self) -> Result<bool, IoError> {
            Ok(self.read_only)
        }

        fn uninstall(&self) {}
    }

    struct FakeGuestMemory {
        bytes: Mutex<Vec<u8>>,
    }

    impl FakeGuestMemory {
        fn new(size: usize) -> Self {
            Self {
                bytes: Mutex::new(vec![0; size]),
            }
        }

        fn range(addr: u64, len: usize) -> Result<(usize, usize), VirtioBlkMmioError> {
            let start = usize::try_from(addr).map_err(|_| {
                VirtioBlkMmioError::GuestMemory("virtio-blk test: guest address overflow")
            })?;
            let end = start
                .checked_add(len)
                .ok_or(VirtioBlkMmioError::GuestMemory(
                    "virtio-blk test: guest address overflow",
                ))?;
            Ok((start, end))
        }

        fn write_bytes(&self, addr: u64, value: &[u8]) {
            let (start, end) = Self::range(addr, value.len()).unwrap();
            let mut bytes = self.bytes.lock().unwrap();
            bytes[start..end].copy_from_slice(value);
        }

        fn read_bytes(&self, addr: u64, len: usize) -> Vec<u8> {
            let (start, end) = Self::range(addr, len).unwrap();
            self.bytes.lock().unwrap()[start..end].to_vec()
        }

        fn write_u16(&self, addr: u64, value: u16) {
            self.write_bytes(addr, &value.to_le_bytes());
        }

        fn write_u32(&self, addr: u64, value: u32) {
            self.write_bytes(addr, &value.to_le_bytes());
        }

        fn write_u64(&self, addr: u64, value: u64) {
            self.write_bytes(addr, &value.to_le_bytes());
        }

        fn write_u8(&self, addr: u64, value: u8) {
            self.write_bytes(addr, &[value]);
        }

        fn write_req(&self, addr: u64, req: VirtioBlkReq) {
            self.write_u32(addr, req.req_type);
            self.write_u32(addr + 4, req.reserved);
            self.write_u64(addr + 8, req.sector);
        }

        fn write_desc(&self, table_addr: u64, idx: u16, desc: VirtqDesc) {
            let addr = table_addr + idx as u64 * size_of::<VirtqDesc>() as u64;
            self.write_u64(addr, desc.addr);
            self.write_u32(addr + 8, desc.len);
            self.write_u16(addr + 12, desc.flags);
            self.write_u16(addr + 14, desc.next);
        }

        fn read_u16(&self, addr: u64) -> u16 {
            u16::from_le_bytes(self.read_bytes(addr, 2).try_into().unwrap())
        }

        fn read_u32(&self, addr: u64) -> u32 {
            u32::from_le_bytes(self.read_bytes(addr, 4).try_into().unwrap())
        }

        fn read_u8(&self, addr: u64) -> u8 {
            self.read_bytes(addr, 1)[0]
        }
    }

    impl VirtioBlkMmioGuestMemory for &FakeGuestMemory {
        fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), VirtioBlkMmioError> {
            let (start, end) = FakeGuestMemory::range(addr, out.len())?;
            let bytes = self.bytes.lock().unwrap();
            out.copy_from_slice(
                bytes
                    .get(start..end)
                    .ok_or(VirtioBlkMmioError::GuestMemory(
                        "virtio-blk test: guest memory read out of range",
                    ))?,
            );
            Ok(())
        }

        fn write(&self, addr: u64, value: &[u8]) -> Result<(), VirtioBlkMmioError> {
            let (start, end) = FakeGuestMemory::range(addr, value.len())?;
            let mut bytes = self.bytes.lock().unwrap();
            bytes
                .get_mut(start..end)
                .ok_or(VirtioBlkMmioError::GuestMemory(
                    "virtio-blk test: guest memory write out of range",
                ))?
                .copy_from_slice(value);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeInterrupt {
        levels: Mutex<Vec<bool>>,
    }

    impl FakeInterrupt {
        fn levels(&self) -> Vec<bool> {
            self.levels.lock().unwrap().clone()
        }
    }

    impl VirtioBlkMmioInterrupt for &FakeInterrupt {
        fn set_irq_level(&self, asserted: bool) -> Result<(), VirtioBlkMmioError> {
            self.levels.lock().unwrap().push(asserted);
            Ok(())
        }
    }

    const DESC_ADDR: u64 = 0x1000;
    const AVAIL_ADDR: u64 = 0x2000;
    const USED_ADDR: u64 = 0x3000;
    const REQ_ADDR: u64 = 0x4000;
    const DATA_ADDR: u64 = 0x5000;
    const STATUS_ADDR: u64 = 0x6000;

    fn make_device<'a>(
        backend: &'a FakeBlockDevice,
        memory: &'a FakeGuestMemory,
        irq: &'a FakeInterrupt,
    ) -> VirtioBlkMmioDevice<'a, FakeBlockDevice, &'a FakeGuestMemory, &'a FakeInterrupt> {
        VirtioBlkMmioDevice::new(backend, memory, irq).unwrap()
    }

    fn make_device_with_trace<'a>(
        backend: &'a FakeBlockDevice,
        memory: &'a FakeGuestMemory,
        irq: &'a FakeInterrupt,
        trace: &'a dyn VirtioBlkMmioTrace,
    ) -> VirtioBlkMmioDevice<'a, FakeBlockDevice, &'a FakeGuestMemory, &'a FakeInterrupt> {
        VirtioBlkMmioDevice::new_with_trace(backend, memory, irq, trace).unwrap()
    }

    fn enable_driver<B, M, I>(device: &mut VirtioBlkMmioDevice<'_, B, M, I>)
    where
        B: BlockDevice + ?Sized,
        M: VirtioBlkMmioGuestMemory,
        I: VirtioBlkMmioInterrupt,
    {
        device.mmio_write(REG_DRIVER_FEATURES_SEL, 4, 1).unwrap();
        device.mmio_write(REG_DRIVER_FEATURES, 4, 1).unwrap();
        device
            .mmio_write(
                REG_STATUS,
                4,
                (VIRTIO_STATUS_ACKNOWLEDGE
                    | VIRTIO_STATUS_DRIVER
                    | VIRTIO_STATUS_FEATURES_OK
                    | VIRTIO_STATUS_DRIVER_OK) as u64,
            )
            .unwrap();
    }

    fn configure_ready_queue<B, M, I>(
        device: &mut VirtioBlkMmioDevice<'_, B, M, I>,
        memory: &FakeGuestMemory,
        queue_size: u16,
    ) where
        B: BlockDevice + ?Sized,
        M: VirtioBlkMmioGuestMemory,
        I: VirtioBlkMmioInterrupt,
    {
        memory.write_u16(AVAIL_ADDR + 2, 0);
        memory.write_u16(USED_ADDR + 2, 0);
        device.mmio_write(REG_QUEUE_SEL, 4, 0).unwrap();
        device
            .mmio_write(REG_QUEUE_NUM, 4, queue_size as u64)
            .unwrap();
        device.mmio_write(REG_QUEUE_DESC_LOW, 4, DESC_ADDR).unwrap();
        device.mmio_write(REG_QUEUE_DESC_HIGH, 4, 0).unwrap();
        device
            .mmio_write(REG_QUEUE_DRIVER_LOW, 4, AVAIL_ADDR)
            .unwrap();
        device.mmio_write(REG_QUEUE_DRIVER_HIGH, 4, 0).unwrap();
        device
            .mmio_write(REG_QUEUE_DEVICE_LOW, 4, USED_ADDR)
            .unwrap();
        device.mmio_write(REG_QUEUE_DEVICE_HIGH, 4, 0).unwrap();
        device.mmio_write(REG_QUEUE_READY, 4, 1).unwrap();
    }

    #[test]
    fn mmio_registers_expose_modern_block_device() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 3], false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        assert_eq!(
            device.mmio_read(REG_MAGIC_VALUE, 4).unwrap(),
            VIRTIO_MMIO_MAGIC_VALUE as u64
        );
        assert_eq!(
            device.mmio_read(REG_VERSION, 4).unwrap(),
            VIRTIO_MMIO_VERSION_MODERN as u64
        );
        assert_eq!(
            device.mmio_read(REG_DEVICE_ID, 4).unwrap(),
            VIRTIO_MMIO_DEVICE_ID_BLOCK as u64
        );
        assert_eq!(
            device.mmio_read(REG_VENDOR_ID, 4).unwrap(),
            VIRTIO_MMIO_VENDOR_ID as u64
        );
        assert_eq!(
            device.mmio_read(REG_QUEUE_NUM_MAX, 4).unwrap(),
            QUEUE_MAX as u64
        );
        assert_eq!(device.mmio_read(REG_CONFIG_SPACE, 4).unwrap(), 3);
        assert_eq!(device.mmio_read(REG_CONFIG_SPACE + 4, 4).unwrap(), 0);
        assert_eq!(
            device.mmio_read(REG_MAGIC_VALUE, 1),
            Err(VirtioBlkMmioError::Access(MmioAccessError::InvalidSize))
        );
    }

    #[test]
    fn queue_registers_follow_selected_queue() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 2], false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        configure_ready_queue(&mut device, &memory, 8);

        assert_eq!(device.mmio_read(REG_QUEUE_NUM, 4).unwrap(), 8);
        assert_eq!(device.mmio_read(REG_QUEUE_READY, 4).unwrap(), 1);

        device.mmio_write(REG_QUEUE_SEL, 4, 1).unwrap();
        for offset in [
            REG_QUEUE_NUM_MAX,
            REG_QUEUE_NUM,
            REG_QUEUE_READY,
            REG_QUEUE_DESC_LOW,
            REG_QUEUE_DESC_HIGH,
            REG_QUEUE_DRIVER_LOW,
            REG_QUEUE_DRIVER_HIGH,
            REG_QUEUE_DEVICE_LOW,
            REG_QUEUE_DEVICE_HIGH,
        ] {
            assert_eq!(device.mmio_read(offset, 4).unwrap(), 0);
        }
    }

    #[test]
    fn best_effort_status_lookup_walks_long_chain() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 2], false);
        let memory = FakeGuestMemory::new(0x9000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        configure_ready_queue(&mut device, &memory, 16);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(DATA_ADDR, 128, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 2),
        );
        memory.write_desc(
            DESC_ADDR,
            2,
            desc(
                DATA_ADDR + 0x100,
                128,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                3,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            3,
            desc(
                DATA_ADDR + 0x200,
                256,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                4,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            4,
            desc(STATUS_ADDR + 0x100, 1, VIRTQ_DESC_F_WRITE, 0),
        );

        assert_eq!(
            device.find_status_addr_best_effort(0),
            Some(STATUS_ADDR + 0x100)
        );
    }

    #[test]
    fn valid_read_request_updates_used_ring_and_irq_ack_deasserts() {
        let mut storage = vec![0u8; SECTOR_SIZE * 3];
        for (idx, byte) in storage[SECTOR_SIZE..SECTOR_SIZE * 2].iter_mut().enumerate() {
            *byte = (idx as u8).wrapping_mul(3).wrapping_add(1);
        }
        let backend = FakeBlockDevice::new(storage.clone(), false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        configure_ready_queue(&mut device, &memory, 8);
        enable_driver(&mut device);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(
                DATA_ADDR,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                2,
            ),
        );
        memory.write_desc(DESC_ADDR, 2, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_IN, 1));
        memory.write_u16(AVAIL_ADDR + 2, 1);
        memory.write_u16(AVAIL_ADDR + 4, 0);
        memory.write_u8(STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        assert_eq!(
            memory.read_bytes(DATA_ADDR, SECTOR_SIZE),
            storage[SECTOR_SIZE..SECTOR_SIZE * 2].to_vec()
        );
        assert_eq!(memory.read_u8(STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 1);
        assert_eq!(memory.read_u32(USED_ADDR + 4), 0);
        assert_eq!(memory.read_u32(USED_ADDR + 8), SECTOR_SIZE as u32);
        assert_eq!(
            device.mmio_read(REG_INTERRUPT_STATUS, 4).unwrap(),
            VIRTIO_INT_USED_RING as u64
        );
        assert_eq!(irq.levels(), vec![true]);

        device
            .mmio_write(REG_INTERRUPT_ACK, 4, VIRTIO_INT_USED_RING as u64)
            .unwrap();

        assert_eq!(device.mmio_read(REG_INTERRUPT_STATUS, 4).unwrap(), 0);
        assert_eq!(irq.levels(), vec![true, false]);
    }

    #[test]
    fn queue_notify_read_request_to_guest_addr_zero_succeeds() {
        let mut storage = vec![0u8; SECTOR_SIZE * 3];
        for (idx, byte) in storage[SECTOR_SIZE..SECTOR_SIZE * 2].iter_mut().enumerate() {
            *byte = (idx as u8).wrapping_mul(5).wrapping_add(7);
        }
        let backend = FakeBlockDevice::new(storage.clone(), false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let trace = FakeTrace::default();
        let mut device = make_device_with_trace(&backend, &memory, &irq, &trace);

        configure_ready_queue(&mut device, &memory, 8);
        enable_driver(&mut device);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(
                0,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                2,
            ),
        );
        memory.write_desc(DESC_ADDR, 2, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_IN, 1));
        memory.write_u16(AVAIL_ADDR + 2, 1);
        memory.write_u16(AVAIL_ADDR + 4, 0);
        memory.write_u8(STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        assert_eq!(
            memory.read_bytes(0, SECTOR_SIZE),
            storage[SECTOR_SIZE..SECTOR_SIZE * 2].to_vec()
        );
        assert_eq!(memory.read_u8(STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 1);
        assert!(trace.events().is_empty());
    }

    #[test]
    fn queue_notify_write_request_from_guest_addr_zero_succeeds() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 3], false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let trace = FakeTrace::default();
        let mut device = make_device_with_trace(&backend, &memory, &irq, &trace);

        configure_ready_queue(&mut device, &memory, 8);
        enable_driver(&mut device);

        let pattern: Vec<u8> = (0..SECTOR_SIZE)
            .map(|idx| (idx as u8).wrapping_mul(13).wrapping_add(9))
            .collect();
        memory.write_bytes(0, &pattern);
        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(0, SECTOR_SIZE as u32, VIRTQ_DESC_F_NEXT, 2),
        );
        memory.write_desc(DESC_ADDR, 2, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_OUT, 1));
        memory.write_u16(AVAIL_ADDR + 2, 1);
        memory.write_u16(AVAIL_ADDR + 4, 0);
        memory.write_u8(STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        assert_eq!(
            backend.storage.lock().unwrap()[SECTOR_SIZE..SECTOR_SIZE * 2].to_vec(),
            pattern
        );
        assert_eq!(memory.read_u8(STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 1);
        assert!(trace.events().is_empty());
    }

    #[test]
    fn split_descriptor_requests_round_trip_through_mmio_device() {
        const WRITE_STATUS_ADDR: u64 = STATUS_ADDR;
        const READ_REQ_ADDR: u64 = REQ_ADDR + 0x1000;
        const READ_STATUS_ADDR: u64 = STATUS_ADDR + 0x1000;
        const WRITE_DATA_0: u64 = DATA_ADDR;
        const WRITE_DATA_1: u64 = DATA_ADDR + 0x100;
        const WRITE_DATA_2: u64 = DATA_ADDR + 0x200;
        const READ_DATA_0: u64 = DATA_ADDR + 0x1000;
        const READ_DATA_1: u64 = DATA_ADDR + 0x1100;
        const READ_DATA_2: u64 = DATA_ADDR + 0x1200;

        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 4], false);
        let memory = FakeGuestMemory::new(0x20_000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        configure_ready_queue(&mut device, &memory, 16);
        enable_driver(&mut device);

        let write_pattern: Vec<u8> = (0..SECTOR_SIZE)
            .map(|idx| (idx as u8).wrapping_mul(11).wrapping_add(5))
            .collect();
        memory.write_bytes(WRITE_DATA_0, &write_pattern[..100]);
        memory.write_bytes(WRITE_DATA_1, &write_pattern[100..256]);
        memory.write_bytes(WRITE_DATA_2, &write_pattern[256..SECTOR_SIZE]);
        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(DESC_ADDR, 1, desc(WRITE_DATA_0, 100, VIRTQ_DESC_F_NEXT, 2));
        memory.write_desc(DESC_ADDR, 2, desc(WRITE_DATA_1, 156, VIRTQ_DESC_F_NEXT, 3));
        memory.write_desc(DESC_ADDR, 3, desc(WRITE_DATA_2, 256, VIRTQ_DESC_F_NEXT, 4));
        memory.write_desc(
            DESC_ADDR,
            4,
            desc(WRITE_STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0),
        );
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_OUT, 1));
        memory.write_u16(AVAIL_ADDR + 2, 1);
        memory.write_u16(AVAIL_ADDR + 4, 0);
        memory.write_u8(WRITE_STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        assert_eq!(memory.read_u8(WRITE_STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 1);
        assert_eq!(memory.read_u32(USED_ADDR + 4), 0);
        assert_eq!(memory.read_u32(USED_ADDR + 8), SECTOR_SIZE as u32);
        assert_eq!(
            backend.storage.lock().unwrap()[SECTOR_SIZE..SECTOR_SIZE * 2].to_vec(),
            write_pattern
        );
        assert_eq!(irq.levels(), vec![true]);

        device
            .mmio_write(REG_INTERRUPT_ACK, 4, VIRTIO_INT_USED_RING as u64)
            .unwrap();

        memory.write_desc(
            DESC_ADDR,
            5,
            desc(
                READ_REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                6,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            6,
            desc(READ_DATA_0, 200, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 7),
        );
        memory.write_desc(
            DESC_ADDR,
            7,
            desc(READ_DATA_1, 200, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 8),
        );
        memory.write_desc(
            DESC_ADDR,
            8,
            desc(READ_DATA_2, 112, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 9),
        );
        memory.write_desc(
            DESC_ADDR,
            9,
            desc(READ_STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0),
        );
        memory.write_req(READ_REQ_ADDR, make_req(VIRTIO_BLK_T_IN, 1));
        memory.write_u16(AVAIL_ADDR + 2, 2);
        memory.write_u16(AVAIL_ADDR + 6, 5);
        memory.write_u8(READ_STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        let mut read_back = Vec::new();
        read_back.extend(memory.read_bytes(READ_DATA_0, 200));
        read_back.extend(memory.read_bytes(READ_DATA_1, 200));
        read_back.extend(memory.read_bytes(READ_DATA_2, 112));
        assert_eq!(read_back, write_pattern);
        assert_eq!(memory.read_u8(READ_STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 2);
        assert_eq!(memory.read_u32(USED_ADDR + 12), 5);
        assert_eq!(memory.read_u32(USED_ADDR + 16), SECTOR_SIZE as u32);
        assert_eq!(irq.levels(), vec![true, false, true]);

        device
            .mmio_write(REG_INTERRUPT_ACK, 4, VIRTIO_INT_USED_RING as u64)
            .unwrap();

        assert_eq!(device.mmio_read(REG_INTERRUPT_STATUS, 4).unwrap(), 0);
        assert_eq!(irq.levels(), vec![true, false, true, false]);
    }

    #[test]
    fn parse_failure_detail_captures_first_descriptors() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 2], false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        configure_ready_queue(&mut device, &memory, 8);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(
                DATA_ADDR,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                2,
            ),
        );
        memory.write_desc(DESC_ADDR, 2, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_OUT, 0));

        let detail = device.collect_parse_failure_detail(
            0,
            Some(STATUS_ADDR),
            QueueParseError::InvalidLayout,
        );

        assert_eq!(detail.mapped_status, VIRTIO_BLK_S_IOERR);
        assert_eq!(detail.parse_error, QueueParseError::InvalidLayout);
        assert_eq!(detail.status_addr_hint, Some(STATUS_ADDR));
        assert_eq!(detail.header_addr, Some(REQ_ADDR));
        assert!(detail.header_readable);
        assert_eq!(
            detail.header_raw,
            Some([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,])
        );
        assert_eq!(detail.descriptor_snapshot.descriptor_count_captured, 3);
        assert_eq!(
            detail.descriptor_snapshot.descriptors[0],
            Some(desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ))
        );
    }

    #[test]
    fn parse_failure_invalid_request_type_maps_to_unsupp_and_logs_detail() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 2], false);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let trace = FakeTrace::default();
        let mut device = make_device_with_trace(&backend, &memory, &irq, &trace);

        configure_ready_queue(&mut device, &memory, 8);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(DESC_ADDR, 1, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(99, 7));

        let outcome = device.process_one_request(0);

        assert_eq!(outcome.status, VIRTIO_BLK_S_UNSUPP);
        assert_eq!(outcome.status_addr, Some(STATUS_ADDR));
        assert_eq!(outcome.used_len, 0);
        assert_eq!(
            trace.events(),
            vec![TraceEvent::Parse(VirtioBlkParseFailureDetail {
                head: 0,
                queue_size: 8,
                status_addr_hint: Some(STATUS_ADDR),
                mapped_status: VIRTIO_BLK_S_UNSUPP,
                parse_error: QueueParseError::InvalidRequestType,
                header_addr: Some(REQ_ADDR),
                header_readable: true,
                header_raw: Some([99, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]),
                descriptor_snapshot: VirtioBlkDescriptorSnapshot {
                    descriptor_count_captured: 2,
                    descriptors: [
                        Some(desc(
                            REQ_ADDR,
                            size_of::<VirtioBlkReq>() as u32,
                            VIRTQ_DESC_F_NEXT,
                            1,
                        )),
                        Some(desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0)),
                        None,
                        None,
                    ],
                },
            })]
        );
    }

    #[test]
    fn execution_failure_preserves_original_error_for_trace() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 2], false)
            .with_read_error(IoError::Device);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let trace = FakeTrace::default();
        let mut device = make_device_with_trace(&backend, &memory, &irq, &trace);

        configure_ready_queue(&mut device, &memory, 8);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(
                DATA_ADDR,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                2,
            ),
        );
        memory.write_desc(DESC_ADDR, 2, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_IN, 0));

        let outcome = device.process_one_request(0);

        assert_eq!(outcome.status, VIRTIO_BLK_S_IOERR);
        assert_eq!(outcome.status_addr, Some(STATUS_ADDR));
        assert_eq!(outcome.used_len, 0);
        assert_eq!(
            trace.events(),
            vec![TraceEvent::Execution(VirtioBlkExecutionFailureDetail {
                head: 0,
                layout: VirtioBlkReqLayout {
                    first_data_desc_idx: Some(1),
                    status_desc_idx: 2,
                    data_len: SECTOR_SIZE as u32,
                    status_addr: STATUS_ADDR,
                    req_type: VIRTIO_BLK_T_IN,
                    sector: 0,
                },
                err: VirtioBlkMmioError::Backend(IoError::Device),
                mapped_status: VIRTIO_BLK_S_IOERR,
            })]
        );
    }

    #[test]
    fn execution_failure_backend_unsupported_maps_to_unsupp() {
        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 2], false)
            .with_flush_error(IoError::Unsupported);
        let memory = FakeGuestMemory::new(0x8000);
        let irq = FakeInterrupt::default();
        let trace = FakeTrace::default();
        let mut device = make_device_with_trace(&backend, &memory, &irq, &trace);

        configure_ready_queue(&mut device, &memory, 8);

        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(DESC_ADDR, 1, desc(STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0));
        memory.write_req(REQ_ADDR, make_req(VIRTIO_BLK_T_FLUSH, 0));

        let outcome = device.process_one_request(0);

        assert_eq!(outcome.status, VIRTIO_BLK_S_UNSUPP);
        assert_eq!(outcome.status_addr, Some(STATUS_ADDR));
        assert_eq!(outcome.used_len, 0);
        assert_eq!(
            trace.events(),
            vec![TraceEvent::Execution(VirtioBlkExecutionFailureDetail {
                head: 0,
                layout: VirtioBlkReqLayout {
                    first_data_desc_idx: None,
                    status_desc_idx: 1,
                    data_len: 0,
                    status_addr: STATUS_ADDR,
                    req_type: VIRTIO_BLK_T_FLUSH,
                    sector: 0,
                },
                err: VirtioBlkMmioError::Backend(IoError::Unsupported),
                mapped_status: VIRTIO_BLK_S_UNSUPP,
            })]
        );
    }

    #[test]
    fn full_mmio_handshake_round_trips_write_then_read() {
        const WRITE_REQ_ADDR: u64 = REQ_ADDR;
        const WRITE_DATA_ADDR: u64 = DATA_ADDR;
        const WRITE_STATUS_ADDR: u64 = STATUS_ADDR;
        const READ_REQ_ADDR: u64 = 0x7000;
        const READ_DATA_ADDR: u64 = 0x8000;
        const READ_STATUS_ADDR: u64 = 0x9000;

        let backend = FakeBlockDevice::new(vec![0u8; SECTOR_SIZE * 4], false);
        let memory = FakeGuestMemory::new(0x10_000);
        let irq = FakeInterrupt::default();
        let mut device = make_device(&backend, &memory, &irq);

        assert_eq!(
            device.mmio_read(REG_MAGIC_VALUE, 4).unwrap(),
            VIRTIO_MMIO_MAGIC_VALUE as u64
        );
        assert_eq!(
            device.mmio_read(REG_DEVICE_ID, 4).unwrap(),
            VIRTIO_MMIO_DEVICE_ID_BLOCK as u64
        );

        device
            .mmio_write(REG_STATUS, 4, VIRTIO_STATUS_ACKNOWLEDGE as u64)
            .unwrap();
        device
            .mmio_write(
                REG_STATUS,
                4,
                (VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER) as u64,
            )
            .unwrap();
        device.mmio_write(REG_DRIVER_FEATURES_SEL, 4, 1).unwrap();
        device.mmio_write(REG_DRIVER_FEATURES, 4, 1).unwrap();
        device
            .mmio_write(
                REG_STATUS,
                4,
                (VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK)
                    as u64,
            )
            .unwrap();

        configure_ready_queue(&mut device, &memory, 8);

        device
            .mmio_write(
                REG_STATUS,
                4,
                (VIRTIO_STATUS_ACKNOWLEDGE
                    | VIRTIO_STATUS_DRIVER
                    | VIRTIO_STATUS_FEATURES_OK
                    | VIRTIO_STATUS_DRIVER_OK) as u64,
            )
            .unwrap();

        let write_pattern: Vec<u8> = (0..SECTOR_SIZE)
            .map(|idx| (idx as u8).wrapping_mul(7).wrapping_add(3))
            .collect();
        memory.write_bytes(WRITE_DATA_ADDR, &write_pattern);
        memory.write_desc(
            DESC_ADDR,
            0,
            desc(
                WRITE_REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                1,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            1,
            desc(WRITE_DATA_ADDR, SECTOR_SIZE as u32, VIRTQ_DESC_F_NEXT, 2),
        );
        memory.write_desc(
            DESC_ADDR,
            2,
            desc(WRITE_STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0),
        );
        memory.write_req(WRITE_REQ_ADDR, make_req(VIRTIO_BLK_T_OUT, 2));
        memory.write_u16(AVAIL_ADDR + 2, 1);
        memory.write_u16(AVAIL_ADDR + 4, 0);
        memory.write_u8(WRITE_STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        assert_eq!(memory.read_u8(WRITE_STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 1);
        assert_eq!(memory.read_u32(USED_ADDR + 4), 0);
        assert_eq!(memory.read_u32(USED_ADDR + 8), SECTOR_SIZE as u32);
        assert_eq!(
            backend.storage.lock().unwrap()[SECTOR_SIZE * 2..SECTOR_SIZE * 3].to_vec(),
            write_pattern
        );
        assert_eq!(
            device.mmio_read(REG_INTERRUPT_STATUS, 4).unwrap(),
            VIRTIO_INT_USED_RING as u64
        );
        assert_eq!(irq.levels(), vec![true]);

        device
            .mmio_write(REG_INTERRUPT_ACK, 4, VIRTIO_INT_USED_RING as u64)
            .unwrap();

        let mut expected_readback = vec![0u8; SECTOR_SIZE];
        expected_readback
            .copy_from_slice(&backend.storage.lock().unwrap()[SECTOR_SIZE * 2..SECTOR_SIZE * 3]);
        memory.write_desc(
            DESC_ADDR,
            3,
            desc(
                READ_REQ_ADDR,
                size_of::<VirtioBlkReq>() as u32,
                VIRTQ_DESC_F_NEXT,
                4,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            4,
            desc(
                READ_DATA_ADDR,
                SECTOR_SIZE as u32,
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
                5,
            ),
        );
        memory.write_desc(
            DESC_ADDR,
            5,
            desc(READ_STATUS_ADDR, 1, VIRTQ_DESC_F_WRITE, 0),
        );
        memory.write_req(READ_REQ_ADDR, make_req(VIRTIO_BLK_T_IN, 2));
        memory.write_u16(AVAIL_ADDR + 2, 2);
        memory.write_u16(AVAIL_ADDR + 6, 3);
        memory.write_u8(READ_STATUS_ADDR, 0xff);

        device.mmio_write(REG_QUEUE_NOTIFY, 4, 0).unwrap();

        assert_eq!(
            memory.read_bytes(READ_DATA_ADDR, SECTOR_SIZE),
            expected_readback
        );
        assert_eq!(memory.read_u8(READ_STATUS_ADDR), VIRTIO_BLK_S_OK);
        assert_eq!(memory.read_u16(USED_ADDR + 2), 2);
        assert_eq!(memory.read_u32(USED_ADDR + 12), 3);
        assert_eq!(memory.read_u32(USED_ADDR + 16), SECTOR_SIZE as u32);
        assert_eq!(
            device.mmio_read(REG_INTERRUPT_STATUS, 4).unwrap(),
            VIRTIO_INT_USED_RING as u64
        );
        assert_eq!(irq.levels(), vec![true, false, true]);

        device
            .mmio_write(REG_INTERRUPT_ACK, 4, VIRTIO_INT_USED_RING as u64)
            .unwrap();

        assert_eq!(device.mmio_read(REG_INTERRUPT_STATUS, 4).unwrap(), 0);
        assert_eq!(irq.levels(), vec![true, false, true, false]);
    }
}
