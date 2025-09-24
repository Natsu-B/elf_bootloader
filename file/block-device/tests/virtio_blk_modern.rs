#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

use aarch64_test::exit_failure;
use aarch64_test::exit_success;
use aarch64_test::println;
use block_device::VirtIoBlk;
use block_device_api::BlockDevice;
use block_device_api::Lba;
use core::mem::MaybeUninit;
use core::slice;

const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;

#[unsafe(no_mangle)]
extern "C" fn efi_main() -> ! {
    match run() {
        Ok(()) => {
            println!("virtio-blk modern interface test: PASS");
            exit_success();
        }
        Err(err) => {
            println!("virtio-blk modern interface test: FAIL: {}", err);
            exit_failure();
        }
    }
}

fn run() -> Result<(), &'static str> {
    println!("Starting virtio_blk test");
    let mut device = VirtIoBlk::new(VIRTIO_MMIO_BASE).unwrap();
    println!("new() succeeded");
    device.init().unwrap();
    println!("init() succeeded");
    if device.is_read_only().unwrap() {
        return Err("device unexpectedly read-only");
    }
    assert_eq!(device.num_blocks(), 3);

    let block_size = device.block_size();
    if block_size == 0 {
        return Err("block size is zero");
    }
    if block_size > 4096 {
        return Err("block size too large for test buffer");
    }
    println!("Attempting to read...");
    let mut buffer: [MaybeUninit<u8>; 512] = [MaybeUninit::uninit(); 512];
    device.read_at(0, &mut buffer).unwrap();
    let slice = unsafe { slice::from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, buffer.len()) };
    let text = str::from_utf8(slice).unwrap();
    println!("device text: {}", text);
    assert_eq!(
        "This is a simple test message. If you are reading these words, it means that the program is working correctly. There is nothing important here, only a demonstration to check the output. Please ignore this text, because it is written only for testing and debugging purposes. Thank you for your patience! In fact, this message has no real meaning other than to confirm that everything is running as expected. You might see it on your screen, in a console, or inside a log file. The exact place does not matter, bec",
        text
    );
    device.flush().unwrap();
    Ok(())
}
