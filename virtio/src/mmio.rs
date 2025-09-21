use core::mem::size_of;

use typestate::ReadPure;
use typestate::ReadWrite;
use typestate::Readable;
use typestate::Writable;
use typestate::WriteOnly;

use crate::DeviceStatus;
use crate::VirtioErr;
use crate::VirtioFeatures;
use crate::VirtioTransport;
use crate::device_type::VirtIoDeviceTypes;

#[allow(clippy::assertions_on_constants)]
const _: () = assert!(size_of::<MmioDeviceRegister>() == 0x100);

#[repr(C)]
struct MmioDeviceRegister {
    magic: ReadPure<u32>,
    version: ReadPure<u32>,
    device_id: ReadPure<u32>,
    vendor_id: ReadPure<u32>,
    device_features: ReadPure<VirtioFeatures>,
    device_features_sel: WriteOnly<u32>,
    _reserved1: [u32; 2],
    driver_features: WriteOnly<VirtioFeatures>,
    driver_features_sel: WriteOnly<u32>,
    _reserved2: [u32; 2],
    queue_sel: WriteOnly<u32>,
    queue_size_max: ReadPure<u32>,
    queue_size: WriteOnly<u32>,
    _reserved3: [u32; 2],
    queue_ready: ReadWrite<u32>,
    _reserved4: [u32; 2],
    queue_notify: WriteOnly<u32>,
    _reserved5: [u32; 3],
    interrupt_status: ReadPure<u32>,
    interrupt_ack: WriteOnly<u32>,
    _reserved6: [u32; 2],
    status: ReadWrite<DeviceStatus>,
    _reserved7: [u32; 3],
    queue_desc_low: WriteOnly<u32>,
    queue_desc_high: WriteOnly<u32>,
    _reserved8: [u32; 2],
    queue_driver_low: WriteOnly<u32>,
    queue_driver_high: WriteOnly<u32>,
    _reserved9: [u32; 2],
    queue_device_low: WriteOnly<u32>,
    queue_device_high: WriteOnly<u32>,
    _reserved10: [u32; 1],
    shm_sel: WriteOnly<u32>,
    shm_len_low: ReadPure<u32>,
    shm_len_high: ReadPure<u32>,
    shm_base_low: ReadPure<u32>,
    shm_base_high: ReadPure<u32>,
    queue_reset: ReadWrite<u32>,
    _reserved11: [u32; 14],
    config_generation: ReadPure<u32>,
}

pub struct VirtIoMmio {
    registers: &'static MmioDeviceRegister,
    device: VirtIoDeviceTypes,
}

impl VirtIoMmio {
    const VIRTIO_MAGIC_VALUE: u32 = 0x74726976;
    const VIRTIO_SUPPORTED_VERSION_COMPATIBLE_MODE: u32 = 1;
    const VIRTIO_SUPPORTED_VERSION: u32 = 2;

    pub(crate) fn new_mmio(paddr: usize) -> Result<VirtIoMmio, VirtioErr> {
        // Safety: caller promises `paddr` points to a valid, device MMIO area
        // that stays mapped for the program lifetime.
        let registers: &'static MmioDeviceRegister =
            unsafe { &*(paddr as *const MmioDeviceRegister) };

        let magic = registers.magic.read();
        if magic != Self::VIRTIO_MAGIC_VALUE {
            return Err(VirtioErr::BadMagic(magic));
        }

        let version = registers.version.read();
        if version != Self::VIRTIO_SUPPORTED_VERSION
            && version != Self::VIRTIO_SUPPORTED_VERSION_COMPATIBLE_MODE
        {
            // legacy interface not supported
            return Err(VirtioErr::UnsupportedVersion(version));
        }
        let device = VirtIoDeviceTypes::try_from(registers.device_id.read())?;
        Ok(Self { device, registers })
    }
}

impl VirtioTransport for VirtIoMmio {
    #[inline]
    fn get_device_version(&self) -> u32 {
        self.registers.version.read()
    }
    #[inline]
    fn get_device(&self) -> VirtIoDeviceTypes {
        self.device
    }

    #[inline]
    fn get_configuration_addr(&self) -> usize {
        self.registers as *const MmioDeviceRegister as usize + size_of::<MmioDeviceRegister>()
    }

    #[inline]
    fn set_status(&self, features: DeviceStatus) {
        self.registers.status.write(features);
    }

    #[inline]
    fn bitmask_set_status(&self, features: DeviceStatus) {
        self.registers.status.set_bits(features);
    }

    #[inline]
    fn get_status(&self) -> DeviceStatus {
        self.registers.status.read()
    }

    #[inline]
    fn get_device_features(&self, select: u32) -> VirtioFeatures {
        self.registers.device_features_sel.write(select);
        self.registers.device_features.read()
    }

    fn set_driver_features(&self, select: u32, val: VirtioFeatures) {
        self.registers.driver_features_sel.write(select);
        self.registers.driver_features.write(val);
    }

    fn select_queue(&self, index: u16) {
        self.registers.queue_sel.write(index as u32);
    }

    fn is_queue_ready_equal_0(&self) -> bool {
        self.registers.queue_ready.read() == 0
    }

    fn enable_queue_ready(&self) {
        self.registers.queue_ready.write(0x01);
    }

    fn get_max_queue_size(&self) -> u32 {
        self.registers.queue_size_max.read()
    }

    fn set_queue_size(&self, size: u32) {
        self.registers.queue_size.write(size);
    }

    fn queue_set_descriptor(&self, paddr: usize) {
        let paddr = paddr as u64;
        self.registers.queue_desc_high.write((paddr >> 32) as u32);
        self.registers.queue_desc_low.write(paddr as u32);
    }

    fn queue_set_available(&self, paddr: usize) {
        let paddr = paddr as u64;
        self.registers.queue_driver_high.write((paddr >> 32) as u32);
        self.registers.queue_driver_low.write(paddr as u32);
    }

    fn queue_set_used(&self, paddr: usize) {
        let paddr = paddr as u64;
        self.registers.queue_device_high.write((paddr >> 32) as u32);
        self.registers.queue_device_low.write(paddr as u32);
    }

    fn queue_notify(&self, index: u16) {
        self.registers.queue_notify.write(index as u32);
    }
}
