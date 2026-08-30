use allocator::AlignedSliceBox;
use core::cell::OnceCell;
use core::cmp::min;
use core::mem::MaybeUninit;
use core::mem::size_of;
use core::ptr::copy_nonoverlapping;

use block_device_api::BlockDevice;
use block_device_api::IoError;
use block_device_api::Lba;
use mutex::SpinLock;
use typestate::Le;
use typestate::Readable;
use virtio::VirtIoCore;
use virtio::VirtIoDevice;
use virtio::VirtioErr;
use virtio::VirtioFeatures;
use virtio::device_type::VirtIoDeviceTypes;
use virtio::mmio::VirtIoMmio;
use virtio::queue::VirtqDescFlags;

mod configuration;
mod operation;

use crate::virtio_blk::operation::VirtioBlkReq;
use crate::virtio_blk::operation::VirtioBlkReqStatus;
use crate::virtio_blk::operation::VirtioBlkReqType;
use configuration::VirtioBlkConfig;
use cpu::clean_dcache_range;
use cpu::invalidate_dcache_range;

const SECTOR_SIZE: usize = 512;
const DMA_ALIGN: usize = 64;
const DMA_BOUNCE_BYTES: usize = SECTOR_SIZE * 2;

pub struct VirtIoBlk {
    virtio: VirtIoCore<VirtIoMmio>,
    is_readonly: OnceCell<bool>,
    configuration_space: &'static VirtioBlkConfig,
    dma: SpinLock<DmaRequestContext>,
}

unsafe impl Sync for VirtIoBlk {}
unsafe impl Send for VirtIoBlk {}

impl VirtIoBlk {
    #![allow(unused)]
    const VIRTIO_BLK_F_SIZE_MAX: VirtioFeatures = VirtioFeatures(1 << 1);
    const VIRTIO_BLK_F_SEG_MAX: VirtioFeatures = VirtioFeatures(1 << 2);
    const VIRTIO_BLK_F_GEOMETRY: VirtioFeatures = VirtioFeatures(1 << 4);
    const VIRTIO_BLK_F_RO: VirtioFeatures = VirtioFeatures(1 << 5);
    const VIRTIO_BLK_F_BLK_SIZE: VirtioFeatures = VirtioFeatures(1 << 6);
    const VIRTIO_BLK_F_FLUSH: VirtioFeatures = VirtioFeatures(1 << 9);
    const VIRTIO_BLK_F_TOPOLOGY: VirtioFeatures = VirtioFeatures(1 << 10);
    const VIRTIO_BLK_F_CONFIG_WCE: VirtioFeatures = VirtioFeatures(1 << 11);
    const VIRTIO_BLK_F_MQ: VirtioFeatures = VirtioFeatures(1 << 12);
    const VIRTIO_BLK_F_DISCARD: VirtioFeatures = VirtioFeatures(1 << 13);
    const VIRTIO_BLK_F_WRITE_ZEROES: VirtioFeatures = VirtioFeatures(1 << 14);
    const VIRTIO_BLK_F_LIFETIME: VirtioFeatures = VirtioFeatures(1 << 15);
    const VIRTIO_BLK_F_SECURE_ERASE: VirtioFeatures = VirtioFeatures(1 << 16);
    const VIRTIO_BLK_F_ZONED: VirtioFeatures = VirtioFeatures(1 << 17);
}

struct DmaRequestContext {
    req: AlignedSliceBox<VirtioBlkReq>,
    status: AlignedSliceBox<Le<VirtioBlkReqStatus>>,
    bounce: AlignedSliceBox<u8>,
}

impl DmaRequestContext {
    fn new() -> Result<Self, IoError> {
        Ok(Self {
            req: allocate_aligned_value(
                VirtioBlkReq {
                    reg_type: Le::new(VirtioBlkReqType::VIRTIO_BLK_T_FLUSH),
                    reserved: Le::new(0),
                    sector: Le::new(0),
                },
                DMA_ALIGN,
            )?,
            status: allocate_aligned_value(
                Le::new(VirtioBlkReqStatus::VIRTIO_BLK_S_RESERVED),
                DMA_ALIGN,
            )?,
            bounce: allocate_aligned_zeroed_bytes(DMA_BOUNCE_BYTES, DMA_ALIGN)?,
        })
    }

    fn req_mut(&mut self) -> &mut VirtioBlkReq {
        &mut self.req[0]
    }

    fn status_mut(&mut self) -> &mut Le<VirtioBlkReqStatus> {
        &mut self.status[0]
    }

    fn req_paddr(&self) -> u64 {
        self.req.as_ptr() as u64
    }

    fn status_paddr(&self) -> u64 {
        self.status.as_ptr() as u64
    }

    fn bounce_paddr(&self) -> u64 {
        self.bounce.as_ptr() as u64
    }

    fn bounce_len(&self) -> usize {
        self.bounce.len()
    }

    fn bounce_ptr(&mut self) -> *mut u8 {
        self.bounce.as_mut_ptr()
    }
}

struct VirtIoBlkAdapter {
    is_read_only: OnceCell<bool>,
}

impl VirtIoBlkAdapter {
    fn new() -> Self {
        Self {
            is_read_only: OnceCell::new(),
        }
    }

    fn is_read_only(&self) -> bool {
        *self.is_read_only.get().unwrap()
    }
}

impl VirtIoDevice for VirtIoBlkAdapter {
    fn driver_features(
        &self,
        select: u32,
        device_feature: VirtioFeatures,
    ) -> Result<VirtioFeatures, VirtioErr> {
        if select == 0 {
            if device_feature & VirtIoBlk::VIRTIO_BLK_F_RO != VirtioFeatures(0) {
                self.is_read_only.set(true).unwrap();
            } else {
                self.is_read_only.set(false).unwrap();
            }
        }
        Ok(VirtioFeatures(0))
    }

    fn num_of_queue(&self) -> Result<u32, VirtioErr> {
        Ok(1)
    }
}

impl VirtIoBlk {
    pub fn new(addr: usize) -> Result<Self, IoError> {
        let virtio = VirtIoCore::new_mmio(addr).map_err(error_from)?;
        if virtio.get_device() != VirtIoDeviceTypes::BlockDevice {
            return Err(IoError::Unsupported);
        }
        // SAFETY: `get_configuration_addr()` returns the transport-provided MMIO configuration
        // window for this virtio device, and the block-device type check above guarantees the
        // layout matches `VirtioBlkConfig` for the lifetime of the transport mapping.
        let configuration_space =
            unsafe { &*(virtio.get_configuration_addr() as *const VirtioBlkConfig) };
        Ok(Self {
            virtio,
            is_readonly: OnceCell::new(),
            configuration_space,
            dma: SpinLock::new(DmaRequestContext::new()?),
        })
    }
}

impl BlockDevice for VirtIoBlk {
    fn init(&mut self) -> Result<(), IoError> {
        let adapter = VirtIoBlkAdapter::new();
        self.virtio.init(&adapter).map_err(error_from)?;
        self.is_readonly.set(adapter.is_read_only()).unwrap();
        Ok(())
    }

    fn block_size(&self) -> usize {
        SECTOR_SIZE
    }

    fn num_blocks(&self) -> u64 {
        self.configuration_space.capacity.read()
    }

    fn read_at(&self, lba: Lba, buf: &mut [MaybeUninit<u8>]) -> Result<(), IoError> {
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }

        let len = buf.len();
        if len == 0 {
            return Err(IoError::InvalidParam);
        }
        if len % self.block_size() != 0 {
            return Err(IoError::Align);
        }

        let blocks = (len / self.block_size()) as u64;
        if lba
            .checked_add(blocks)
            .filter(|end| *end <= self.num_blocks())
            .is_none()
        {
            return Err(IoError::OutOfRange);
        }

        self.submit_rw(false, lba, buf.as_mut_ptr() as *mut u8, len)
    }

    fn write_at(&self, lba: Lba, buf: &[u8]) -> Result<(), IoError> {
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }
        if self.is_read_only()? {
            return Err(IoError::ReadOnly);
        }

        let len = buf.len();
        if len == 0 {
            return Err(IoError::InvalidParam);
        }
        if len % self.block_size() != 0 {
            return Err(IoError::Align);
        }

        let blocks = (len / self.block_size()) as u64;
        if lba
            .checked_add(blocks)
            .filter(|end| *end <= self.num_blocks())
            .is_none()
        {
            return Err(IoError::OutOfRange);
        }

        self.submit_rw(true, lba, buf.as_ptr() as *mut u8, len)
    }

    fn flush(&self) -> Result<(), IoError> {
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }

        let mut dma = self.dma.lock();
        self.submit_request(&mut dma, VirtioBlkReqType::VIRTIO_BLK_T_FLUSH, 0, 0, false)
    }

    fn max_io_bytes(&self) -> Result<Option<usize>, IoError> {
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }
        Ok(None)
    }

    fn is_read_only(&self) -> Result<bool, IoError> {
        self.is_readonly.get().copied().ok_or(IoError::NotReady)
    }

    fn uninstall(&self) {
        self.virtio.reset();
    }
}

impl VirtIoBlk {
    fn submit_rw(
        &self,
        is_write: bool,
        mut lba: u64,
        buf_ptr: *mut u8,
        buf_len: usize,
    ) -> Result<(), IoError> {
        let mut dma = self.dma.lock();
        let mut offset = 0usize;

        while offset < buf_len {
            let chunk_len = min(buf_len - offset, dma.bounce_len());
            if is_write {
                // SAFETY: `buf_ptr` originates from a live caller-provided slice, `offset` and
                // `chunk_len` stay within that slice's validated bounds, and the bounce buffer is
                // an internal non-overlapping allocation of at least `chunk_len` bytes.
                unsafe {
                    copy_nonoverlapping(
                        buf_ptr.add(offset) as *const u8,
                        dma.bounce_ptr(),
                        chunk_len,
                    );
                }
            }

            self.submit_request(
                &mut dma,
                if is_write {
                    VirtioBlkReqType::VIRTIO_BLK_T_OUT
                } else {
                    VirtioBlkReqType::VIRTIO_BLK_T_IN
                },
                lba,
                chunk_len,
                !is_write,
            )?;

            if !is_write {
                // SAFETY: `buf_ptr` originates from a live mutable caller-provided slice,
                // `offset` and `chunk_len` stay within that slice's validated bounds, and the
                // DMA bounce buffer contains the device-written bytes for this completed chunk.
                unsafe {
                    copy_nonoverlapping(dma.bounce.as_ptr(), buf_ptr.add(offset), chunk_len);
                }
            }

            offset += chunk_len;
            lba += (chunk_len / SECTOR_SIZE) as u64;
        }

        Ok(())
    }

    fn submit_request(
        &self,
        dma: &mut DmaRequestContext,
        req_type: VirtioBlkReqType,
        sector: u64,
        data_len: usize,
        device_writes_data: bool,
    ) -> Result<(), IoError> {
        let mut allocated = [0u16; 3];
        let mut allocated_len = 0usize;
        let mut data_desc_ptr_opt: Option<*mut virtio::queue::VirtqDesc> = None;

        *dma.req_mut() = VirtioBlkReq {
            reg_type: Le::new(req_type),
            reserved: Le::new(0),
            sector: Le::new(sector),
        };
        *dma.status_mut() = Le::new(VirtioBlkReqStatus::VIRTIO_BLK_S_RESERVED);

        let (first_desc_idx, first_desc_ptr) =
            self.virtio.allocate_descriptor(0).map_err(error_from)?;
        allocated[allocated_len] = first_desc_idx;
        allocated_len += 1;
        first_desc_ptr.addr = Le::new(dma.req_paddr());
        first_desc_ptr.len = Le::new(size_of::<VirtioBlkReq>() as u32);
        first_desc_ptr.flags = Le::new(VirtqDescFlags::VIRTQ_DESC_F_NEXT);

        let status_desc_ptr;

        if data_len == 0 {
            let (desc_idx, desc_ptr) = self.virtio.allocate_descriptor(0).map_err(|err| {
                let _ = self.release_descriptors(&allocated[..allocated_len]);
                error_from(err)
            })?;
            allocated[allocated_len] = desc_idx;
            allocated_len += 1;
            first_desc_ptr.next = Le::new(desc_idx);
            status_desc_ptr = desc_ptr;
        } else {
            let (data_desc_idx, data_desc_ptr) =
                self.virtio.allocate_descriptor(0).map_err(|err| {
                    let _ = self.release_descriptors(&allocated[..allocated_len]);
                    error_from(err)
                })?;
            allocated[allocated_len] = data_desc_idx;
            allocated_len += 1;
            first_desc_ptr.next = Le::new(data_desc_idx);
            data_desc_ptr.addr = Le::new(dma.bounce_paddr());
            data_desc_ptr.len =
                Le::new(u32::try_from(data_len).map_err(|_| IoError::InvalidParam)?);
            data_desc_ptr.flags = Le::new(if device_writes_data {
                VirtqDescFlags::VIRTQ_DESC_F_NEXT | VirtqDescFlags::VIRTQ_DESC_F_WRITE
            } else {
                VirtqDescFlags::VIRTQ_DESC_F_NEXT
            });

            let (desc_idx, desc_ptr) = self.virtio.allocate_descriptor(0).map_err(|err| {
                let _ = self.release_descriptors(&allocated[..allocated_len]);
                error_from(err)
            })?;
            allocated[allocated_len] = desc_idx;
            allocated_len += 1;
            data_desc_ptr.next = Le::new(desc_idx);
            data_desc_ptr_opt = Some(data_desc_ptr as *mut _);
            status_desc_ptr = desc_ptr;
        }

        status_desc_ptr.addr = Le::new(dma.status_paddr());
        status_desc_ptr.len = Le::new(size_of::<u8>() as u32);
        status_desc_ptr.flags = Le::new(VirtqDescFlags::VIRTQ_DESC_F_WRITE);

        let desc_size = size_of::<virtio::queue::VirtqDesc>();
        clean_dcache_range(first_desc_ptr as *const _ as *const u8, desc_size);
        if let Some(data_desc_ptr) = data_desc_ptr_opt {
            clean_dcache_range(data_desc_ptr as *const u8, desc_size);
        }
        clean_dcache_range(status_desc_ptr as *const _ as *const u8, desc_size);
        clean_dcache_range(dma.req.as_ptr() as *const u8, size_of::<VirtioBlkReq>());
        clean_dcache_range(dma.status.as_ptr() as *const u8, size_of::<u8>());
        if data_len != 0 {
            clean_dcache_range(dma.bounce.as_ptr(), data_len);
        }

        let exec_result = (|| -> Result<(), IoError> {
            self.virtio
                .set_and_notify(0, first_desc_idx)
                .map_err(error_from)?;
            let (completed_idx, _used_len) = loop {
                match self.virtio.pop_used(0).map_err(error_from)? {
                    Some(entry) => break entry,
                    None => core::hint::spin_loop(),
                }
            };

            if completed_idx != first_desc_idx {
                return Err(IoError::Corrupted);
            }

            invalidate_dcache_range(dma.status.as_ptr() as *const u8, size_of::<u8>());
            if device_writes_data && data_len != 0 {
                invalidate_dcache_range(dma.bounce.as_ptr(), data_len);
            }

            match dma.status[0].read() {
                status if status == VirtioBlkReqStatus::VIRTIO_BLK_S_OK => {
                    self.release_descriptors(&allocated[..allocated_len])?;
                    Ok(())
                }
                status if status == VirtioBlkReqStatus::VIRTIO_BLK_S_IOERR => Err(IoError::Io),
                status if status == VirtioBlkReqStatus::VIRTIO_BLK_S_UNSUPP => {
                    Err(IoError::Unsupported)
                }
                _ => Err(IoError::Io),
            }
        })();

        if exec_result.is_err() {
            let _ = self.release_descriptors(&allocated[..allocated_len]);
        }

        exec_result
    }

    fn release_descriptors(&self, desc_indices: &[u16]) -> Result<(), IoError> {
        let mut first_error = None;
        for &desc_idx in desc_indices {
            if let Err(err) = self.virtio.dequeue_used(0, desc_idx) {
                if first_error.is_none() {
                    first_error = Some(error_from(err));
                }
            }
        }
        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }
}

fn allocate_aligned_value<T>(value: T, align: usize) -> Result<AlignedSliceBox<T>, IoError> {
    let mut raw =
        AlignedSliceBox::<T>::new_uninit_with_align(1, align).map_err(|_| IoError::NoMemory)?;
    raw[0].write(value);
    // SAFETY: The single element in `raw` was fully initialized by `write()` above.
    Ok(unsafe { raw.assume_init() })
}

fn allocate_aligned_zeroed_bytes(len: usize, align: usize) -> Result<AlignedSliceBox<u8>, IoError> {
    let mut raw =
        AlignedSliceBox::<u8>::new_uninit_with_align(len, align).map_err(|_| IoError::NoMemory)?;
    for byte in raw.iter_mut() {
        byte.write(0);
    }
    // SAFETY: Every element in `raw` was initialized to zero in the loop above.
    Ok(unsafe { raw.assume_init() })
}

fn error_from(e: VirtioErr) -> IoError {
    match e {
        VirtioErr::BadMagic(_) => IoError::Protocol,
        VirtioErr::UnsupportedVersion(_) => IoError::Unsupported,
        VirtioErr::UnknownVirtioDevice(_) => IoError::Unsupported,
        VirtioErr::UnsupportedDeviceFeature(_) | VirtioErr::UnsupportedDriverFeature(_) => {
            IoError::Unsupported
        }
        VirtioErr::Invalid => IoError::InvalidParam,
        VirtioErr::DeviceNeedsReset => IoError::NotReady,
        VirtioErr::DeviceUninitialized => IoError::NotReady,
        VirtioErr::OutOfAvailableDesc => IoError::Busy,
        VirtioErr::QueueCorrupted => IoError::Corrupted,
    }
}
