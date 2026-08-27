//! Minimal UEFI payload for exercising a nested x86-64 guest.

#![no_main]
#![no_std]

use core::panic::PanicInfo;
use r_efi::efi;
use x86_64_hal::cpu;

/// Legacy COM1 base I/O port.
const COM1: u16 = 0x03f8;

/// Configures COM1 for 115200 baud, 8-N-1 polling output.
fn init_serial() {
    // SAFETY: UEFI runs at CPL0 and the test payload owns COM1.
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

/// Writes one byte after waiting for the transmitter.
fn write_byte(byte: u8) {
    // SAFETY: UEFI runs at CPL0 and the test payload owns COM1.
    unsafe {
        while cpu::inb(COM1 + 5) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        cpu::outb(COM1, byte);
    }
}

/// UEFI image entry point.
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(
    _image: efi::Handle,
    _system_table: *mut efi::SystemTable,
) -> efi::Status {
    init_serial();
    for byte in b"thin-hv: guest uefi payload\r\n" {
        write_byte(*byte);
    }
    let leaf_one = cpu::cpuid(1, 0);
    let vmx = u8::from(leaf_one.ecx & (1 << 5) != 0);
    let hypervisor = u8::from(leaf_one.ecx & (1 << 31) != 0);
    for byte in b"thin-hv: guest cpuid vmx=" {
        write_byte(*byte);
    }
    write_byte(b'0' + vmx);
    for byte in b" hypervisor=" {
        write_byte(*byte);
    }
    write_byte(b'0' + hypervisor);
    write_byte(b'\r');
    write_byte(b'\n');

    let hypervisor_leaf = cpu::cpuid(0x4000_0000, 0);
    if vmx == 1
        && hypervisor == 0
        && hypervisor_leaf.eax == 0
        && hypervisor_leaf.ebx == 0
        && hypervisor_leaf.ecx == 0
        && hypervisor_leaf.edx == 0
    {
        efi::Status::SUCCESS
    } else {
        efi::Status::DEVICE_ERROR
    }
}

/// Stops the payload if an unexpected panic occurs.
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
