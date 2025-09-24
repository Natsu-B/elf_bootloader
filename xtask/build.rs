use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = PathBuf::from("../test");
    println!("cargo:rerun-if-changed=../test");
    let firmware_path = dir.join("RELEASEAARCH64_QEMU_EFI.fd");
    if !firmware_path.exists() {
        if let Some(parent) = firmware_path.parent() {
            fs::create_dir_all(parent).expect("failed to create firmware directory");
        }

        let url = "https://retrage.github.io/edk2-nightly/bin/RELEASEAARCH64_QEMU_EFI.fd";
        if let Err(err) = fetch_with_tools(url, &firmware_path) {
            panic!(
                "failed to obtain firmware: {}\nPlease download it manually from:\n  {}\nAnd place it at: {}",
                err,
                url,
                firmware_path.display()
            );
        }
    }
}

fn fetch_with_tools(url: &str, dest: &Path) -> Result<(), String> {
    // Prefer curl if available
    if let Ok(status) = Command::new("curl")
        .arg("-L")
        .arg("-o")
        .arg(dest)
        .arg(url)
        .status()
        && status.success() {
            return Ok(());
        }

    // Fallback to wget
    if let Ok(status) = Command::new("wget").arg("-O").arg(dest).arg(url).status()
        && status.success() {
            return Ok(());
        }

    Err("neither 'curl' nor 'wget' succeeded (or were found)".into())
}
