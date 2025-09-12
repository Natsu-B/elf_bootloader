use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Directory with DTS fixtures
    let dts_dir = PathBuf::from("test/dts");
    println!("cargo:rerun-if-changed=test/dts");
    if !dts_dir.exists() {
        return;
    }

    // OUT_DIR is provided by Cargo
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Find all .dts files
    let entries = fs::read_dir(&dts_dir).unwrap();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("dts") {
            continue;
        }
        let file_name = path.file_stem().unwrap().to_string_lossy().to_string();
        let out_path = out_dir.join(format!("{}.dtb", file_name));

        // Invalidate when source changes
        println!("cargo:rerun-if-changed={}", path.display());

        // Try to run dtc to build the dtb
        let status = Command::new("dtc")
            .args(["-O", "dtb", "-o"])
            .arg(&out_path)
            .arg(&path)
            .status();

        match status {
            Ok(s) if s.success() => {
                // Success
            }
            Ok(s) => {
                // dtc returned error; fail the build so tests don't silently skip
                panic!(
                    "dtc failed (exit: {}), cannot build {}",
                    s,
                    out_path.display()
                );
            }
            Err(e) => {
                // dtc not present or failed to spawn; fail the build as requested
                panic!(
                    "failed to run dtc: {}. Required to build {}",
                    e,
                    out_path.display()
                );
            }
        }
    }
}
