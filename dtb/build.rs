use core::panic;
use std::path::Path;
use std::process::{Command, Stdio};
use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("--- check the environment");
    if !command_installed("git") {
        panic!("this test requires git");
    }
    if !command_installed("dtc") {
        panic!("this test requires the device tree compiler(dtc)");
    }
    println!("--- start dtb tests ---");
    println!("Creating test folder...");
    if fs::create_dir("test").is_err() {
        println!("Checking file...");
        let dtb_path = Path::new("test/test.dtb");
        if dtb_path.is_file() {
            println!("file already exists, exiting...");
            return;
        }
    }
    println!("Downloading test.dts from https://gist.github.com/072176edd54cd207c1d800c25d384cd2.git");
    if Command::new("git")
        .arg("clone")
        .arg("https://gist.github.com/072176edd54cd207c1d800c25d384cd2.git")
        .arg("test")
        .stdout(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .is_err()
    {
        let dts_path = Path::new("test/test.dts");
        if !dts_path.is_file() {
            panic!("failed to fetch test.dts");
        }
    }

    println!("Compiling test.dts file to test.dtb...");
    if Command::new("dtc")
        .arg("-I")
        .arg("dts")
        .arg("-O")
        .arg("dtb")
        .arg("-o")
        .arg("test/test.dtb")
        .arg("test/test.dts")
        .stdout(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .is_err()
    {
        panic!("failed to compile dts file");
    }
    println!("exiting pre-test setup...");
}

fn command_installed(command_name: &str) -> bool {
    Command::new(command_name)
        .arg("--version")
        .stdout(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
