//! VirtIO device driver framework.
//!
//! Provides support for VirtIO MMIO devices including virtqueues and device configuration.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

/// VirtIO device type definitions.
pub mod device_type;
/// MMIO transport layer.
pub mod mmio;
/// Virtqueue implementation.
pub mod queue;
use alloc::alloc::alloc_zeroed;
use alloc::boxed::Box;
use core::alloc::Layout;
use typestate_macro::RawReg;

use crate::device_type::VirtIoDeviceTypes;
use crate::mmio::VirtIoMmio;
use crate::queue::VirtQueue;
use crate::queue::VirtqDesc;

const VIRTIO_FEATURE_SEL_SIZE: usize = 4;

/// Transport-agnostic VirtIO device interface.
pub trait VirtioTransport {
    /// Returns the device type.
    fn get_device(&self) -> VirtIoDeviceTypes;
    /// Returns the device configuration space address.
    fn get_configuration_addr(&self) -> usize;
    /// Returns the device version.
    fn get_device_version(&self) -> u32;

    /// Sets the device status.
    fn set_status(&self, features: DeviceStatus);
    /// Sets status bits without clearing others.
    fn bitmask_set_status(&self, features: DeviceStatus);
    /// Returns the current device status.
    fn get_status(&self) -> DeviceStatus;
    /// Returns device features for the given selector.
    fn get_device_features(&self, select: u32) -> VirtioFeatures;
    /// Sets driver features for the given selector.
    fn set_driver_features(&self, select: u32, val: VirtioFeatures);

    /// Selects a virtqueue by index.
    fn select_queue(&self, index: u16);
    /// Returns true if the queue is not ready.
    fn is_queue_ready_equal_0(&self) -> bool;
    /// Marks the queue as ready.
    fn enable_queue_ready(&self);
    /// Returns the maximum queue size.
    fn get_max_queue_size(&self) -> u32;
    /// Sets the queue size.
    fn set_queue_size(&self, size: u32);
    /// Sets the descriptor table address.
    fn queue_set_descriptor(&self, paddr: usize);
    /// Sets the available ring address.
    fn queue_set_available(&self, paddr: usize);
    /// Sets the used ring address.
    fn queue_set_used(&self, paddr: usize);

    /// Notifies the device about queue activity.
    fn queue_notify(&self, index: u16);
}

/// VirtIO feature flags.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, RawReg, PartialEq)]
pub struct VirtioFeatures(pub u32);

impl VirtioFeatures {
    #![allow(unused)]
    // --- lower 32 bits (select = 0) ---
    /// Device supports descriptor-table entries that reference indirect descriptor tables.
    pub const F_INDIRECT_DESC: Self = Self(1u32 << 28);
    /// Device supports the `used_event`/`avail_event` event index fields in split virtqueues.
    pub const F_EVENT_IDX: Self = Self(1u32 << 29);

    // --- upper 32 bits (select = 1) ---
    /// Device follows VirtIO 1.0+ semantics.
    pub const F_VERSION_1: Self = Self(1u32 << (32 - 32)); // bit 0 of high
    /// Device requires platform/IOMMU access translation for queue memory.
    pub const F_ACCESS_PLATFORM: Self = Self(1u32 << (33 - 32));
    /// Device supports packed virtqueue ring layout.
    pub const F_RING_PACKED: Self = Self(1u32 << (34 - 32));
    /// Device processes descriptors in the order they are made available.
    pub const F_IN_ORDER: Self = Self(1u32 << (35 - 32));
    /// Device observes stronger platform ordering requirements for memory accesses.
    pub const F_ORDER_PLATFORM: Self = Self(1u32 << (36 - 32));
    /// Device supports SR-IOV related capabilities.
    pub const F_SR_IOV: Self = Self(1u32 << (37 - 32));
    /// Driver provides extra notification data with queue notifications.
    pub const F_NOTIFICATION_DATA: Self = Self(1u32 << (38 - 32));
    /// Device-specific notification configuration data is available.
    pub const F_NOTIF_CONFIG_DATA: Self = Self(1u32 << (39 - 32));
    /// Device supports resetting individual virtqueues.
    pub const F_RING_RESET: Self = Self(1u32 << (40 - 32));
    /// Device supports an administrative virtqueue.
    pub const F_ADMIN_VQ: Self = Self(1u32 << (41 - 32));

    fn get_features_mask(select: usize) -> VirtioFeatures {
        match select {
            0 => VirtioFeatures(0x00FF_FFFF),
            1 => VirtioFeatures(0xFFFC_0000),
            2 | 3 => VirtioFeatures(0xFFFF_FFFF),
            _ => VirtioFeatures(0x0000_0000),
        }
    }
}

/// Trait for device-specific VirtIO implementations.
pub trait VirtIoDevice {
    /// Returns driver-negotiated features for the given selector.
    fn driver_features(
        &self,
        select: u32,
        device_feature: VirtioFeatures,
    ) -> Result<VirtioFeatures, VirtioErr>;
    /// Returns the number of virtqueues this device uses.
    fn num_of_queue(&self) -> Result<u32, VirtioErr>;
}

/// VirtIO device status register.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, RawReg)]
pub struct DeviceStatus(u32);

impl DeviceStatus {
    const RESET: DeviceStatus = DeviceStatus(0);
    const ACKNOWLEDGE: DeviceStatus = DeviceStatus(1);
    const DRIVER: DeviceStatus = DeviceStatus(2);
    const FAILED: DeviceStatus = DeviceStatus(128);
    const FEATURES_OK: DeviceStatus = DeviceStatus(8);
    const DRIVER_OK: DeviceStatus = DeviceStatus(4);
    const DEVICE_NEEDS_RESET: DeviceStatus = DeviceStatus(64);
}

/// Core VirtIO device state with transport and queues.
pub struct VirtIoCore<T: VirtioTransport> {
    /// The underlying transport (e.g., MMIO).
    pub transport: T,
    /// Allocated virtqueues.
    pub queues: Option<Box<[VirtQueue]>>,
}

impl VirtIoCore<VirtIoMmio> {
    /// Creates a new MMIO-based VirtIO device at the given physical address.
    pub fn new_mmio(paddr: usize) -> Result<Self, VirtioErr> {
        Ok(Self {
            transport: VirtIoMmio::new_mmio(paddr)?,
            queues: None,
        })
    }
}

impl<T: VirtioTransport> VirtIoCore<T> {
    /// Returns the device type.
    #[inline]
    pub fn get_device(&self) -> VirtIoDeviceTypes {
        self.transport.get_device()
    }

    /// Returns the configuration space address.
    #[inline]
    pub fn get_configuration_addr(&self) -> usize {
        self.transport.get_configuration_addr()
    }

    fn device_independent_features(
        &self,
        select: usize,
        features: VirtioFeatures,
    ) -> Result<VirtioFeatures, VirtioErr> {
        if select == 1 {
            // Require VERSION_1 feature to operate in modern mode
            if features & VirtioFeatures::F_VERSION_1 == VirtioFeatures(0) {
                return Err(VirtioErr::UnsupportedVersion(
                    self.transport.get_device_version(),
                ));
            }
            Ok(VirtioFeatures::F_VERSION_1)
        } else {
            Ok(VirtioFeatures(0))
        }
    }

    /// Initializes the VirtIO device with the given device-specific configuration.
    pub fn init<D>(&mut self, virtio_device: &D) -> Result<(), VirtioErr>
    where
        D: VirtIoDevice,
    {
        let result = (|| {
            // reset virtio
            self.transport.set_status(DeviceStatus::RESET);
            // set ACKNOWLEDGE
            self.transport.bitmask_set_status(DeviceStatus::ACKNOWLEDGE);
            // set DRIVER
            self.transport.bitmask_set_status(DeviceStatus::DRIVER);

            // get features
            let mut features: [VirtioFeatures; VIRTIO_FEATURE_SEL_SIZE] =
                [VirtioFeatures(0); VIRTIO_FEATURE_SEL_SIZE];
            for i in 0..VIRTIO_FEATURE_SEL_SIZE {
                let device_feature = self.transport.get_device_features(i as u32);
                features[i] = device_feature;
                self.transport.set_driver_features(
                    i as u32,
                    (virtio_device.driver_features(i as u32, device_feature)?
                        & device_feature
                        & VirtioFeatures::get_features_mask(i))
                        | self.device_independent_features(i, device_feature)?,
                );
            }

            // set FEATURES_OK
            self.transport.bitmask_set_status(DeviceStatus::FEATURES_OK);

            // check whether FEATURES_OK flag is still set
            let status = self.transport.get_status();
            if status & DeviceStatus::FEATURES_OK == DeviceStatus(0) {
                return Err(VirtioErr::UnsupportedDriverFeature(features));
            }
            if status & DeviceStatus::DEVICE_NEEDS_RESET != DeviceStatus(0) {
                return Err(VirtioErr::DeviceNeedsReset);
            }

            // queue size is device specific
            let num_of_queue_size = virtio_device.num_of_queue()?;
            if num_of_queue_size > 1 << 16 {
                return Err(VirtioErr::Invalid);
            }
            let mut uninit_box = Box::new_uninit_slice(num_of_queue_size as usize);

            for i in 0..num_of_queue_size {
                self.transport.select_queue(i as u16);
                if !self.transport.is_queue_ready_equal_0() {
                    // assume already initialized
                    continue;
                }
                // get max queue size
                let queue_size = self.transport.get_max_queue_size();
                // align power of 2
                let queue_size = 1 << queue_size.ilog2();
                // set queue size
                self.transport.set_queue_size(queue_size);
                // allocate and zero the queue memory
                // # safety alloc_zeroed have to return physical memory
                let descriptor_table = unsafe {
                    alloc_zeroed(Layout::from_size_align_unchecked(
                        16 * queue_size as usize,
                        16,
                    ))
                } as usize;
                let avail_hdr = core::mem::size_of::<crate::queue::VirtqAvail>();
                let used_hdr = core::mem::size_of::<crate::queue::VirtqUsed>();
                let avail_entries = core::mem::size_of::<typestate::Le<u16>>();
                let used_entries = core::mem::size_of::<crate::queue::VirtqUsedElem>();
                let available_ring = unsafe {
                    alloc_zeroed(Layout::from_size_align_unchecked(
                        avail_hdr + avail_entries * queue_size as usize,
                        2,
                    ))
                } as usize;
                let used_ring = unsafe {
                    alloc_zeroed(Layout::from_size_align_unchecked(
                        used_hdr + used_entries * queue_size as usize,
                        4,
                    ))
                } as usize;
                self.transport.queue_set_descriptor(descriptor_table);
                self.transport.queue_set_available(available_ring);
                self.transport.queue_set_used(used_ring);
                uninit_box[i as usize].write(VirtQueue::new(
                    queue_size,
                    descriptor_table,
                    available_ring,
                    used_ring,
                ));

                core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
                // enable queue
                self.transport.enable_queue_ready();
            }

            self.queues = Some(unsafe { uninit_box.assume_init() });

            // check DEVICE_NEEDS_RESET and enable devices
            if self.transport.get_status() & DeviceStatus::DEVICE_NEEDS_RESET
                == DeviceStatus::DEVICE_NEEDS_RESET
            {
                return Err(VirtioErr::DeviceNeedsReset);
            }

            core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
            self.transport.bitmask_set_status(DeviceStatus::DRIVER_OK);

            Ok(())
        })();

        if result.is_err() {
            // set failed bit
            self.transport.bitmask_set_status(DeviceStatus::FAILED);
        }
        result
    }

    /// Allocates a descriptor from the specified queue.
    pub fn allocate_descriptor(
        &self,
        queue_idx: u16,
    ) -> Result<(u16, &'static mut VirtqDesc), VirtioErr> {
        let queue = self.queues.as_ref().ok_or(VirtioErr::DeviceUninitialized)?;
        queue[queue_idx as usize].allocate_descriptor()
    }

    /// Adds a descriptor to the available ring and notifies the device.
    pub fn set_and_notify(&self, queue_idx: u16, desc_idx: u16) -> Result<(), VirtioErr> {
        let queue = self.queues.as_ref().ok_or(VirtioErr::DeviceUninitialized)?;
        queue[queue_idx as usize].set_available_ring(desc_idx)?;
        // Ensure descriptor/ring writes are globally visible before notifying the device.
        // virtio requires a wmb() before MMIO notify; Release is sufficient here.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.transport.queue_notify(queue_idx);
        Ok(())
    }

    /// Pops a completed entry from the used ring.
    pub fn pop_used(&self, queue_idx: u16) -> Result<Option<(u16, u32)>, VirtioErr> {
        let queue = self.queues.as_ref().ok_or(VirtioErr::DeviceUninitialized)?;
        queue[queue_idx as usize].pop_used()
    }

    /// Releases a used descriptor back to the free pool.
    pub fn dequeue_used(&self, queue_idx: u16, desc_idx: u16) -> Result<(), VirtioErr> {
        let queue = self.queues.as_ref().ok_or(VirtioErr::DeviceUninitialized)?;
        queue[queue_idx as usize].dequeue_used(desc_idx)
    }

    /// Resets the device.
    pub fn reset(&self) {
        // reset virtio
        self.transport.set_status(DeviceStatus::RESET);
    }
}

/// VirtIO error types.
#[derive(Debug)]
pub enum VirtioErr {
    /// Invalid magic number.
    BadMagic(u32),
    /// Unsupported VirtIO version.
    UnsupportedVersion(u32),
    /// Unknown device type.
    UnknownVirtioDevice(u32),
    /// Device features not supported.
    UnsupportedDeviceFeature([VirtioFeatures; VIRTIO_FEATURE_SEL_SIZE]),
    /// Driver features not supported.
    UnsupportedDriverFeature([VirtioFeatures; VIRTIO_FEATURE_SEL_SIZE]),
    /// Generic invalid state.
    Invalid,
    /// Device requires a reset.
    DeviceNeedsReset,
    /// Device not yet initialized.
    DeviceUninitialized,
    /// No free descriptors available.
    OutOfAvailableDesc,
    /// Queue data corrupted.
    QueueCorrupted,
}
