#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

use aarch64_test::exit_failure;
use aarch64_test::exit_success;
use aarch64_test::println;
use file::StorageDevice;
use file::StorageDeviceErr;
use filesystem::FileSystemErr;

const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;

#[unsafe(no_mangle)]
extern "C" fn efi_main() -> ! {
    match run() {
        Ok(()) => {
            println!("fat32_virtio test: PASS");
            exit_success();
        }
        Err(err) => {
            println!("fat32_virtio test: FAIL: {}", err);
            exit_failure();
        }
    }
}

fn run() -> Result<(), &'static str> {
    println!("Starting fat32_virtio test");
    let device = StorageDevice::new_virtio(VIRTIO_MMIO_BASE).unwrap();
    println!("fat32_virtio init success");
    let handle = device
        .open(0, "/hello.txt", &file::OpenOptions::Read)
        .unwrap();
    let txt = handle.read(1).unwrap();
    let txt = str::from_utf8(&txt).unwrap();
    println!("device text: {}", txt);
    assert_eq!("HelloWorld, from FAT32 txt file!!!", txt);
    handle.flush().unwrap();
    assert_eq!(handle.size().unwrap(), txt.len() as u64);
    let handle = device
        .open(
            0,
            "/very_long_long_example_text.TXT",
            &file::OpenOptions::Read,
        )
        .unwrap();
    let txt = &handle.read(1).unwrap();
    let txt = str::from_utf8(txt).unwrap();
    println!("long long text: {}", txt);
    assert_eq!(
        "This is a simple test message. If you are reading these words, it means that the program is working correctly. There is nothing important here, only a demonstration to check the output. Please ignore this text, because it is written only for testing and debugging purposes. Thank you for your patience! In fact, this message has no real meaning other than to confirm that everything is running as expected. You might see it on your screen, in a console, or inside a log file. The exact place does not matter, because the purpose is always the same: to provide a harmless, human-readable signal that the system is alive. If you see this text, you can be confident that the process of displaying or printing strings is functioning.Once again, please remember that this is not real content. It is just a placeholder, sometimes called a “dummy message” or “sample output.” Developers often use texts like this to make sure their tools, devices, or programs are responding. If you read it twice or even three times, you will still find nothing new, because repetition is part of the test. The message is intentionally long, so that you can check how wrapping, spacing, and formatting behave when more than a few sentences are displayed.",
        txt
    );
    assert_eq!(
        device
            .open(0, "/EFI/hoge", &file::OpenOptions::Read)
            .unwrap_err(),
        StorageDeviceErr::FileSystemErr(FileSystemErr::NotFound)
    );
    Ok(())
}
