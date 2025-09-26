use core::cell::OnceCell;
use core::mem::MaybeUninit;
use core::mem::size_of;

use block_device_api::BlockDevice;
use block_device_api::IoError;
use block_device_api::Lba;
use typestate::Le;
use typestate::Readable;
use virtio::VirtIoCore;
use virtio::VirtioErr;
use virtio::device_type::VirtIoDeviceTypes;
mod configuration;
mod operation;
use configuration::VirtioBlkConfig;
use virtio::VirtIoDevice;
use virtio::VirtioFeatures;
use virtio::mmio::VirtIoMmio;
use virtio::queue::VirtqDescFlags;

use crate::virtio_blk::operation::VirtioBlkReq;
use crate::virtio_blk::operation::VirtioBlkReqStatus;
use crate::virtio_blk::operation::VirtioBlkReqType;
use virtio::cache::clean_dcache_range;
use virtio::cache::invalidate_dcache_range;

pub struct VirtIoBlk {
    virtio: VirtIoCore<VirtIoMmio>,
    is_readonly: OnceCell<bool>,
    configuration_space: &'static VirtioBlkConfig,
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
        let configuration_space =
            unsafe { &*(virtio.get_configuration_addr() as *mut VirtioBlkConfig) };
        Ok(Self {
            virtio,
            is_readonly: OnceCell::new(),
            configuration_space,
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
        512
    }

    fn num_blocks(&self) -> u64 {
        // virtio-blk reports capacity in 512-byte sectors.
        // Our logical block size is 512, so this maps 1:1 to blocks.
        self.configuration_space.capacity.read()
    }

    fn read_at(&self, lba: Lba, buf: &mut [MaybeUninit<u8>]) -> Result<(), IoError> {
        // Validate initialization state
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }
        // Validate parameters per BlockDevice contract
        let bs = self.block_size();
        let len = buf.len();
        if len == 0 {
            return Err(IoError::InvalidParam);
        }
        if len % bs != 0 {
            return Err(IoError::Align);
        }
        let blocks = (len / bs) as u64;
        if lba
            .checked_add(blocks)
            .filter(|end| *end <= self.num_blocks())
            .is_none()
        {
            return Err(IoError::OutOfRange);
        }
        // Enforce an implementation/transport practical bound (virtq desc len is u32)
        if len > u32::MAX as usize {
            return Err(IoError::InvalidParam);
        }
        if let Some(max) = self.max_io_bytes()? {
            if len > max {
                return Err(IoError::InvalidParam);
            }
        }

        self.submit_rw(false, lba, buf.as_mut_ptr() as usize, len)
    }

    fn write_at(&self, lba: Lba, buf: &[u8]) -> Result<(), IoError> {
        // Validate initialization state
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }
        // Enforce read-only
        if self.is_read_only()? {
            return Err(IoError::ReadOnly);
        }
        // Validate parameters per BlockDevice contract
        let bs = self.block_size();
        let len = buf.len();
        if len == 0 {
            return Err(IoError::InvalidParam);
        }
        if len % bs != 0 {
            return Err(IoError::Align);
        }
        let blocks = (len / bs) as u64;
        if lba
            .checked_add(blocks)
            .filter(|end| *end <= self.num_blocks())
            .is_none()
        {
            return Err(IoError::OutOfRange);
        }
        // Enforce an implementation/transport practical bound (virtq desc len is u32)
        if len > u32::MAX as usize {
            return Err(IoError::InvalidParam);
        }
        if let Some(max) = self.max_io_bytes()? {
            if len > max {
                return Err(IoError::InvalidParam);
            }
        }

        self.submit_rw(true, lba, buf.as_ptr() as usize, len)
    }

    fn flush(&self) -> Result<(), IoError> {
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }
        let virtio_req = VirtioBlkReq {
            reg_type: Le::new(VirtioBlkReqType::VIRTIO_BLK_T_FLUSH),
            reserved: Le::new(0),
            sector: Le::new(0),
        };
        let (first_desc_idx, first_desc_ptr) =
            self.virtio.allocate_descriptor(0).map_err(error_from)?;
        first_desc_ptr.addr = Le::new(&virtio_req as *const _ as u64);
        first_desc_ptr.len = Le::new(size_of::<VirtioBlkReq>() as u32);
        first_desc_ptr.flags = Le::new(VirtqDescFlags::VIRTQ_DESC_F_NEXT);

        // status
        let mut status: Le<VirtioBlkReqStatus> = Le::new(VirtioBlkReqStatus::VIRTIO_BLK_S_RESERVED);
        let (second_desc_idx, second_desc_ptr) =
            self.virtio.allocate_descriptor(0).map_err(|e| {
                // Free already-allocated descriptor on failure
                let _ = self.virtio.dequeue_used(0, first_desc_idx);
                error_from(e)
            })?;
        first_desc_ptr.next = Le::new(second_desc_idx);
        second_desc_ptr.addr = Le::new(&mut status as *mut _ as u64);
        second_desc_ptr.len = Le::new(size_of::<u8>() as u32);
        second_desc_ptr.flags = Le::new(VirtqDescFlags::VIRTQ_DESC_F_WRITE);

        // clean cache
        let desc_size = size_of::<virtio::queue::VirtqDesc>();
        clean_dcache_range(first_desc_ptr as *const _ as *const u8, desc_size);
        clean_dcache_range(second_desc_ptr as *const _ as *const u8, desc_size);
        clean_dcache_range(
            &virtio_req as *const _ as *const u8,
            size_of::<VirtioBlkReq>(),
        );
        clean_dcache_range(&status as *const _ as *const u8, size_of::<u8>());

        // Execute I/O in a closure; success path frees descriptors inside.
        let exec = (|| -> Result<(), IoError> {
            self.virtio
                .set_and_notify(0, first_desc_idx)
                .map_err(error_from)?;
            let (idx, _len) = loop {
                match self.virtio.pop_used(0).map_err(error_from)? {
                    Some(v) => break v,
                    None => {
                        core::hint::spin_loop();
                        continue;
                    }
                }
            };

            if idx != first_desc_idx {
                return Err(IoError::Io);
            }

            invalidate_dcache_range(&status as *const _ as *const u8, size_of::<u8>());

            match status.read() {
                VirtioBlkReqStatus::VIRTIO_BLK_S_OK => {
                    // free on success here(dequeue return no_err)
                    self.virtio
                        .dequeue_used(0, first_desc_idx)
                        .map_err(error_from)
                        .unwrap();
                    self.virtio
                        .dequeue_used(0, second_desc_idx)
                        .map_err(error_from)
                        .unwrap();
                    Ok(())
                }
                VirtioBlkReqStatus::VIRTIO_BLK_S_IOERR => Err(IoError::Io),
                VirtioBlkReqStatus::VIRTIO_BLK_S_UNSUPP => Err(IoError::Unsupported),
                VirtioBlkReqStatus::VIRTIO_BLK_S_RESERVED => Err(IoError::Io),
                _ => unreachable!(),
            }
        })();

        if let Err(e) = exec {
            // free on error (best-effort)
            let _ = self.virtio.dequeue_used(0, first_desc_idx);
            let _ = self.virtio.dequeue_used(0, second_desc_idx);
            return Err(e);
        }
        Ok(())
    }

    fn max_io_bytes(&self) -> Result<Option<usize>, IoError> {
        if self.virtio.queues.is_none() {
            return Err(IoError::NotReady);
        }
        // Features like SIZE_MAX/SEG_MAX are not negotiated in this driver yet.
        // Per spec, corresponding fields are invalid when not negotiated.
        // Report no explicit limit so upper layers can choose reasonable chunking.
        Ok(None)
    }

    fn is_read_only(&self) -> Result<bool, IoError> {
        if let Some(readonly) = self.is_readonly.get() {
            Ok(*readonly)
        } else {
            Err(IoError::NotReady)
        }
    }

    fn uninstall(&self) {
        self.virtio.reset();
    }
}

impl VirtIoBlk {
    // TODO free when error occurred
    fn submit_rw(
        &self,
        is_write: bool,
        lba: u64,
        buf_ptr: usize,
        buf_len: usize,
    ) -> Result<(), IoError> {
        let virtio_req = VirtioBlkReq {
            reg_type: Le::new(if is_write {
                VirtioBlkReqType::VIRTIO_BLK_T_OUT
            } else {
                VirtioBlkReqType::VIRTIO_BLK_T_IN
            }),
            reserved: Le::new(0),
            sector: Le::new(lba),
        };
        let (first_desc_idx, first_desc_ptr) =
            self.virtio.allocate_descriptor(0).map_err(error_from)?;
        first_desc_ptr.addr = Le::new(&virtio_req as *const _ as u64);
        first_desc_ptr.len = Le::new(size_of::<VirtioBlkReq>() as u32);
        first_desc_ptr.flags = Le::new(VirtqDescFlags::VIRTQ_DESC_F_NEXT);

        // buffer
        let (second_desc_idx, second_desc_ptr) =
            self.virtio.allocate_descriptor(0).map_err(|e| {
                let _ = self.virtio.dequeue_used(0, first_desc_idx);
                error_from(e)
            })?;
        first_desc_ptr.next = Le::new(second_desc_idx);
        second_desc_ptr.addr = Le::new(buf_ptr as u64);
        second_desc_ptr.len = Le::new(buf_len as u32);
        second_desc_ptr.flags = Le::new(if is_write {
            VirtqDescFlags::VIRTQ_DESC_F_NEXT
        } else {
            VirtqDescFlags::VIRTQ_DESC_F_NEXT | VirtqDescFlags::VIRTQ_DESC_F_WRITE
        });

        // status
        let mut status: Le<VirtioBlkReqStatus> = Le::new(VirtioBlkReqStatus::VIRTIO_BLK_S_RESERVED);
        let (third_desc_idx, third_desc_ptr) = self.virtio.allocate_descriptor(0).map_err(|e| {
            let _ = self.virtio.dequeue_used(0, first_desc_idx);
            let _ = self.virtio.dequeue_used(0, second_desc_idx);
            error_from(e)
        })?;
        second_desc_ptr.next = Le::new(third_desc_idx);
        third_desc_ptr.addr = Le::new(&mut status as *mut _ as u64);
        third_desc_ptr.len = Le::new(size_of::<u8>() as u32);
        third_desc_ptr.flags = Le::new(VirtqDescFlags::VIRTQ_DESC_F_WRITE);

        // Cache maintenance before notifying the device
        // 1) Descriptors: device reads them
        let desc_size = size_of::<virtio::queue::VirtqDesc>();
        clean_dcache_range(first_desc_ptr as *const _ as *const u8, desc_size);
        clean_dcache_range(second_desc_ptr as *const _ as *const u8, desc_size);
        clean_dcache_range(third_desc_ptr as *const _ as *const u8, desc_size);
        // 2) Request header
        clean_dcache_range(
            &virtio_req as *const _ as *const u8,
            size_of::<VirtioBlkReq>(),
        );
        // 3) Data buffer
        clean_dcache_range(buf_ptr as *const u8, buf_len);
        // 4) Status byte (device writes)
        clean_dcache_range(&status as *const _ as *const u8, size_of::<u8>());

        // Execute I/O in a closure; success path frees descriptors inside.
        let exec = (|| -> Result<(), IoError> {
            self.virtio
                .set_and_notify(0, first_desc_idx)
                .map_err(error_from)?;
            let (idx, _len) = loop {
                match self.virtio.pop_used(0).map_err(error_from)? {
                    Some(v) => break v,
                    None => {
                        core::hint::spin_loop();
                        continue;
                    }
                }
            };

            if idx != first_desc_idx {
                return Err(IoError::Io);
            }

            // Ensure device DMA writes (data and status) are visible before we read them.
            invalidate_dcache_range(&status as *const _ as *const u8, size_of::<u8>());
            if !is_write {
                invalidate_dcache_range(buf_ptr as *const u8, buf_len);
            }

            match status.read() {
                VirtioBlkReqStatus::VIRTIO_BLK_S_OK => {
                    // free on success here
                    self.virtio.dequeue_used(0, first_desc_idx).unwrap();
                    self.virtio.dequeue_used(0, second_desc_idx).unwrap();
                    self.virtio.dequeue_used(0, third_desc_idx).unwrap();
                    Ok(())
                }
                VirtioBlkReqStatus::VIRTIO_BLK_S_IOERR => Err(IoError::Io),
                VirtioBlkReqStatus::VIRTIO_BLK_S_UNSUPP => Err(IoError::Unsupported),
                VirtioBlkReqStatus::VIRTIO_BLK_S_RESERVED => Err(IoError::Io),
                _ => unreachable!(),
            }
        })();

        if let Err(e) = exec {
            // free on error (best-effort)
            let _ = self.virtio.dequeue_used(0, first_desc_idx);
            let _ = self.virtio.dequeue_used(0, second_desc_idx);
            let _ = self.virtio.dequeue_used(0, third_desc_idx);
            return Err(e);
        }
        Ok(())
    }
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

        // デバイスの状態が要求を受け付けられない（要リセット/未初期化）＝汎用的には NotReady
        VirtioErr::DeviceNeedsReset => IoError::NotReady,
        VirtioErr::DeviceUninitialized => IoError::NotReady,

        // 送信できるディスクリプタが尽きた＝一時的に受け付け不能
        VirtioErr::OutOfAvailableDesc => IoError::Busy,

        // 内部キュー破損＝一般化して Corrupted
        VirtioErr::QueueCorrupted => IoError::Corrupted,
    }
}
