//! Minimal x86-64 UEFI entry point for the thin hypervisor.

#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::fmt::Write;
#[cfg(not(test))]
use core::panic::PanicInfo;
use r_efi::efi;
use x86_64_hal::cpu;
#[cfg(not(feature = "trusted-outer-kvm"))]
use x86_64_hal::vmx;

mod runtime_variables;
mod vmx_smoke;

/// Legacy COM1 base I/O port.
pub(crate) const COM1: u16 = 0x03f8;

/// Polling serial output used before any monitor runtime exists.
pub(crate) struct SerialPort;

impl SerialPort {
    /// Configures COM1 for 115200 baud, 8 data bits, no parity, and one stop bit.
    pub(crate) fn init(&mut self) {
        // SAFETY: UEFI applications run at CPL0 and this loader exclusively uses COM1.
        unsafe {
            cpu::outb(COM1 + 1, 0x00);
            cpu::outb(COM1 + 3, 0x80);
            cpu::outb(COM1, 0x01);
            cpu::outb(COM1 + 1, 0x00);
            cpu::outb(COM1 + 3, 0x03);
            cpu::outb(COM1 + 2, 0xc7);
            cpu::outb(COM1 + 4, 0x0b);
        }
    }

    /// Waits for the transmitter and writes one byte.
    pub(crate) fn write_byte(&mut self, byte: u8) {
        // SAFETY: UEFI applications run at CPL0 and this loader exclusively uses COM1.
        unsafe {
            while cpu::inb(COM1 + 5) & 0x20 == 0 {
                core::hint::spin_loop();
            }
            cpu::outb(COM1, byte);
        }
    }

    /// Writes bytes without constructing formatting state.
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }

    /// Writes one fixed-width hexadecimal value without `core::fmt`.
    #[cfg_attr(feature = "trusted-outer-kvm", allow(dead_code))]
    pub(crate) fn write_hex(&mut self, value: u64) {
        self.write_bytes(b"0x");
        for digit in (0..16).rev() {
            let nibble = ((value >> (digit * 4)) & 0xf) as u8;
            self.write_byte(if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            });
        }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.write_bytes(text.as_bytes());
        Ok(())
    }
}

/// UEFI image entry point.
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> efi::Status {
    let mut serial = SerialPort;
    serial.init();
    let vmx_present = cpu::has_vmx();

    let _ = writeln!(serial, "thin-hv: uefi entry");
    let _ = writeln!(serial, "thin-hv: CPUID VMX={}", u8::from(vmx_present));

    #[cfg(feature = "trusted-outer-kvm")]
    {
        let _ = writeln!(serial, "thin-hv: trusted outer KVM backend");
        if let Err(error) = vmx_smoke::run(image, system_table) {
            let _ = writeln!(serial, "thin-hv: trusted outer KVM FAIL: {error}");
            return efi::Status::DEVICE_ERROR;
        }
        return efi::Status::SUCCESS;
    }

    #[cfg(not(feature = "trusted-outer-kvm"))]
    {
        if !vmx_present {
            let _ = writeln!(serial, "thin-hv: IA32_FEATURE_CONTROL=unavailable");
            let _ = writeln!(serial, "thin-hv: IA32_VMX_BASIC=unavailable");
            return efi::Status::UNSUPPORTED;
        }

        // SAFETY: CPUID reports VMX and UEFI executes this entry point at CPL0.
        let feature_control = unsafe { cpu::rdmsr(cpu::IA32_FEATURE_CONTROL) };
        // SAFETY: CPUID reports VMX and UEFI executes this entry point at CPL0.
        let vmx_basic_raw = unsafe { cpu::rdmsr(vmx::IA32_VMX_BASIC) };
        let vmx_basic = vmx::VmxBasic::from_msr(vmx_basic_raw);

        let _ = writeln!(
            serial,
            "thin-hv: IA32_FEATURE_CONTROL={feature_control:#018x} lock={} vmx_outside_smx={}",
            (feature_control & 1) as u8,
            ((feature_control >> 2) & 1) as u8
        );
        let _ = writeln!(
            serial,
            "thin-hv: IA32_VMX_BASIC={vmx_basic_raw:#018x} revision={:#010x} region_size={} memory_type={} true_controls={}",
            vmx_basic.revision_id,
            vmx_basic.region_size,
            vmx_basic.memory_type,
            u8::from(vmx_basic.true_controls)
        );

        if let Err(error) = vmx_smoke::run(image, system_table) {
            let _ = writeln!(serial, "thin-hv: vmx smoke FAIL: {error}");
            return efi::Status::DEVICE_ERROR;
        }

        efi::Status::SUCCESS
    }
}

/// Emits a stable marker even when formatting the panic itself would be unsafe.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    let mut serial = SerialPort;
    serial.init();
    serial.write_bytes(b"thin-hv: panic\n");
    loop {
        core::hint::spin_loop();
    }
}
