//! Workspace helper binary for build, run, and test workflows.

#![crate_type = "bin"]
// xtask/src/main.rs

use core::panic;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::net::UdpSocket;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

fn main() {
    // Skip the executable name (xtask)
    let mut args = std::env::args().skip(1);

    let command = args.next();

    let remaining_args: Vec<String> = args.collect();

    match command.as_deref() {
        Some("-h") | Some("--help") => {
            print_xtask_usage();
        }
        Some("build") => {
            if let Err(error) = build(&remaining_args) {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
        }
        Some("run") => {
            if let Err(error) = run(&remaining_args) {
                eprintln!("Error: {error}");
                std::process::exit(1);
            }
        }
        Some("test") => test(&remaining_args),
        Some(cmd) => {
            eprintln!("Error: Unknown command '{}'", cmd);
            eprintln!("Usage: cargo xtask [build|run|test] [args...]");
            std::process::exit(1);
        }
        None => {
            eprintln!("Error: No command provided.");
            eprintln!("Usage: cargo xtask [build|run|test] [args...]");
            std::process::exit(1);
        }
    }
}

fn print_xtask_usage() {
    println!("Usage: cargo xtask <command> [args...]");
    println!();
    println!("Commands:");
    println!("  build [x86|rpi5|rpi4|rpi4_net|example] [args...]");
    println!("  run [x86|rpi4|rpi5|net] [args...]");
    println!(
        "  run x86 --nested [--release]    QEMU/KVM nested contract and Linux state/lifetime A/B tests"
    );
    println!("  test [xtest args...]");
    println!();
    println!("Options:");
    println!("  -h, --help    Show this help");
}

fn build(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("x86") => build_x86_uefi(&args[1..]),
        Some("rpi5") => build_rpi5(&args[1..]),
        Some("rpi4") => build_bootloader_with_feature(&args[1..], "rpi4"),
        Some("rpi4_net") => build_bootloader_with_feature(&args[1..], "rpi4_net"),
        Some("example") => build_examples(&args[1..]),
        _ => build_bootloader(args),
    }
}

fn build_examples(args: &[String]) -> Result<String, String> {
    let (filters, mut forward_args) = parse_build_example_args(args)?;

    if !has_target_arg(&forward_args) {
        insert_default_target_arg(&mut forward_args);
    }

    let workspace = workspace_root()?;
    let mut manifests = discover_example_manifests(&workspace, 8)?;

    if !filters.is_empty() {
        manifests.retain(|manifest| {
            filters
                .iter()
                .any(|filter| manifest_matches_filter(manifest, filter))
        });
    }

    if manifests.is_empty() {
        if filters.is_empty() {
            return Err(
                "Error: no example manifests found under */example/*/Cargo.toml".to_string(),
            );
        }
        return Err(format!(
            "Error: no example manifests matched filters: {}",
            filters.join(", ")
        ));
    }

    for manifest in manifests {
        eprintln!("\n--- Building example: {} ---", manifest.display());
        let mut cmd = Command::new("cargo");
        cmd.arg("build")
            .arg("-Z")
            .arg("build-std=core,alloc,compiler_builtins")
            .arg("-Z")
            .arg("build-std-features=compiler-builtins-mem")
            .arg("--manifest-path")
            .arg(&manifest)
            .args(&forward_args)
            .env("XTASK_BUILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        eprintln!("Running: {:?}", cmd);
        let status = cmd
            .spawn()
            .unwrap_or_else(|e| {
                panic!(
                    "Failed to spawn cargo build for example manifest {}: {}",
                    manifest.display(),
                    e
                )
            })
            .wait()
            .unwrap_or_else(|e| {
                panic!(
                    "Failed to wait for cargo build for example manifest {}: {}",
                    manifest.display(),
                    e
                )
            });
        if !status.success() {
            eprintln!(
                "Error: cargo build failed for example manifest '{}' with status: {:?}",
                manifest.display(),
                status
            );
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    eprintln!("\n--- Examples built successfully ---");
    Ok(String::new())
}

fn parse_build_example_args(args: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut filters = Vec::new();
    let mut forward_args = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg == "--" {
            forward_args.extend(args[i..].iter().cloned());
            break;
        }

        if let Some(pkg) = arg.strip_prefix("--package=") {
            filters.push(pkg.to_string());
            i += 1;
            continue;
        }

        if arg == "-p" || arg == "--package" {
            match args.get(i + 1) {
                Some(pkg) if pkg != "--" => {
                    filters.push(pkg.clone());
                    i += 2;
                    continue;
                }
                _ => return Err("Error: -p/--package requires a value".to_string()),
            }
        }

        if arg.starts_with("-p") && arg.len() > 2 {
            filters.push(arg[2..].to_string());
            i += 1;
            continue;
        }

        forward_args.push(arg.clone());
        i += 1;
    }

    Ok((filters, forward_args))
}

fn has_target_arg(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--target" || arg.starts_with("--target=") {
            return true;
        }
    }
    false
}

fn insert_default_target_arg(args: &mut Vec<String>) {
    if let Some(separator_pos) = args.iter().position(|arg| arg == "--") {
        args.insert(separator_pos, "--target".to_string());
        args.insert(
            separator_pos + 1,
            "aarch64-unknown-none-softfloat".to_string(),
        );
    } else {
        args.push("--target".to_string());
        args.push("aarch64-unknown-none-softfloat".to_string());
    }
}

fn discover_example_manifests(root: &Path, max_depth: usize) -> Result<Vec<PathBuf>, String> {
    let mut manifests = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth {
            continue;
        }

        let entries = fs::read_dir(&dir)
            .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                format!("Failed to read directory entry in {}: {}", dir.display(), e)
            })?;
            let file_type = entry
                .file_type()
                .map_err(|e| format!("Failed to inspect {}: {}", entry.path().display(), e))?;
            if !file_type.is_dir() {
                continue;
            }

            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                if depth < max_depth {
                    stack.push((path, depth + 1));
                }
                continue;
            };

            if matches!(name, "target" | ".git" | "bin") {
                continue;
            }

            if name == "example" {
                let children = fs::read_dir(&path).map_err(|e| {
                    format!("Failed to read example directory {}: {}", path.display(), e)
                })?;
                for child in children {
                    let child = child.map_err(|e| {
                        format!(
                            "Failed to read directory entry in {}: {}",
                            path.display(),
                            e
                        )
                    })?;
                    let child_type = child.file_type().map_err(|e| {
                        format!("Failed to inspect {}: {}", child.path().display(), e)
                    })?;
                    if !child_type.is_dir() {
                        continue;
                    }

                    let manifest = child.path().join("Cargo.toml");
                    if manifest.is_file() {
                        manifests.push(manifest);
                    }
                }
            }

            if depth < max_depth {
                stack.push((path, depth + 1));
            }
        }
    }

    manifests.sort();
    manifests.dedup();
    Ok(manifests)
}

fn manifest_matches_filter(manifest: &Path, filter: &str) -> bool {
    let mut previous: Option<&str> = None;
    for component in manifest.components() {
        let Some(component) = component.as_os_str().to_str() else {
            previous = None;
            continue;
        };
        if previous == Some(filter) && component == "example" {
            return true;
        }
        previous = Some(component);
    }
    false
}

fn build_bootloader(args: &[String]) -> Result<String, String> {
    let pkg = "elf-hypervisor";
    eprintln!("\n--- Building bootloader package: {} ---", pkg);
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("-Z")
        .arg("build-std=core,alloc,compiler_builtins")
        .arg("-Z")
        .arg("build-std-features=compiler-builtins-mem")
        .arg("-p")
        .arg(pkg)
        .arg("--target")
        .arg("aarch64-unknown-none-softfloat")
        .args(args)
        .env("XTASK_BUILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    eprintln!("Running: {:?}", cmd);
    let status = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn cargo build for {}: {}", pkg, e))
        .wait()
        .unwrap_or_else(|e| panic!("Failed to wait for cargo build for {}: {}", pkg, e));
    if !status.success() {
        eprintln!(
            "Error: cargo build failed for package '{}' with status: {:?}",
            pkg, status
        );
        std::process::exit(status.code().unwrap_or(1));
    }

    eprintln!("\n--- Bootloader built successfully ---");
    let profile = resolve_profile(args);
    copy_artifact_to_bin("elf-hypervisor", "elf-hypervisor.elf", &profile)
}

fn build_bootloader_with_feature(args: &[String], feature: &str) -> Result<String, String> {
    let mut combined_args = vec!["--features".to_string(), feature.to_string()];
    combined_args.extend_from_slice(args);
    build_bootloader(&combined_args)
}

fn build_x86_uefi(args: &[String]) -> Result<String, String> {
    if args.iter().any(|arg| {
        arg == "--all-features"
            || arg.contains("trusted-outer-kvm")
            || arg.contains("physical-chainload")
            || arg.contains("physical-preflight")
            || arg.contains("physical-policy")
            || arg.contains("host-exception-test")
            || arg.contains("host-xstate-test")
            || arg.contains("nested-contract")
            || arg.contains("msr-contract")
            || arg.contains("msr-abort")
    }) {
        return Err(
            "x86 builds all backend artifacts; do not select an alternate backend feature explicitly"
                .to_string(),
        );
    }
    let pkg = "x86_uefi_loader";
    let guest_pkg = "x86_guest_uefi_test";
    eprintln!("\n--- Building x86 UEFI package: {} ---", pkg);
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("-p")
        .arg(pkg)
        .arg("-p")
        .arg(guest_pkg)
        .arg("--target")
        .arg("x86_64-unknown-uefi")
        .args(args)
        .env("XTASK_BUILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    eprintln!("Running: {:?}", cmd);
    let status = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn cargo build for {}: {}", pkg, e))?
        .wait()
        .map_err(|e| format!("Failed to wait for cargo build for {}: {}", pkg, e))?;
    if !status.success() {
        return Err(format!(
            "cargo build failed for package '{}' with status: {}",
            pkg, status
        ));
    }

    let workspace = workspace_root()?;
    let artifact = workspace
        .join("target")
        .join("x86_64-unknown-uefi")
        .join(resolve_profile(args))
        .join("x86-uefi-loader.efi");
    let destination = workspace
        .join("bin")
        .join("x86_64")
        .join("x86-uefi-loader.efi");
    verify_monitor_isa(&artifact)?;
    let monitor_destination = workspace
        .join("bin")
        .join("x86_64")
        .join("x86-uefi-monitor.efi");
    let guest_artifact = workspace
        .join("target")
        .join("x86_64-unknown-uefi")
        .join(resolve_profile(args))
        .join("x86_guest_uefi_test.efi");
    let guest_destination = workspace
        .join("bin")
        .join("x86_64")
        .join("x86_guest_uefi_test.efi");
    fs::create_dir_all(destination.parent().expect("destination has a parent"))
        .map_err(|e| format!("Failed to create x86 staging directory: {}", e))?;
    for obsolete_name in ["x86-uefi-kvm-monitor.efi", "x86-uefi-loader-runtime.efi"] {
        let obsolete = workspace.join("bin").join("x86_64").join(obsolete_name);
        match fs::remove_file(&obsolete) {
            Ok(()) => eprintln!("Removed obsolete x86 artifact {}", obsolete.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to remove obsolete x86 artifact {}: {}",
                    obsolete.display(),
                    error
                ));
            }
        }
    }
    fs::copy(&artifact, &destination).map_err(|e| {
        format!(
            "Failed to copy {} to {}: {}",
            artifact.display(),
            destination.display(),
            e
        )
    })?;
    fs::copy(&artifact, &monitor_destination).map_err(|e| {
        format!(
            "Failed to copy {} to {}: {}",
            artifact.display(),
            monitor_destination.display(),
            e
        )
    })?;
    let status = Command::new("objcopy")
        .arg("--subsystem=efi-rtd")
        .arg(&monitor_destination)
        .status()
        .map_err(|e| format!("Failed to run objcopy for runtime monitor: {}", e))?;
    if !status.success() {
        return Err(format!(
            "objcopy failed for {} with status: {}",
            monitor_destination.display(),
            status
        ));
    }
    fs::copy(&guest_artifact, &guest_destination).map_err(|e| {
        format!(
            "Failed to copy {} to {}: {}",
            guest_artifact.display(),
            guest_destination.display(),
            e
        )
    })?;

    for (feature, filename) in [
        ("trusted-outer-kvm", "x86-uefi-kvm-loader.efi"),
        ("physical-chainload", "x86-uefi-physical-loader.efi"),
        ("physical-preflight", "x86-uefi-preflight.efi"),
    ] {
        eprintln!("\n--- Building non-VMX x86 UEFI backend: {feature} ---");
        let status = Command::new("cargo")
            .arg("build")
            .arg("-p")
            .arg(pkg)
            .arg("--target")
            .arg("x86_64-unknown-uefi")
            .args(args)
            .arg("--no-default-features")
            .arg("--features")
            .arg(feature)
            .env("XTASK_BUILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to build {feature} loader: {e}"))?;
        if !status.success() {
            return Err(format!(
                "{feature} loader build failed with status: {status}"
            ));
        }
        verify_no_decoded_vmx(&artifact)?;
        let backend_destination = workspace.join("bin").join("x86_64").join(filename);
        fs::copy(&artifact, &backend_destination).map_err(|e| {
            format!(
                "Failed to copy {} to {}: {}",
                artifact.display(),
                backend_destination.display(),
                e
            )
        })?;
    }
    let policy_artifact = workspace
        .join("target/x86_64-unknown-uefi")
        .join(resolve_profile(args))
        .join("x86-uefi-physical-policy.efi");
    for (feature, filename) in [
        (
            "physical-policy-driver",
            "x86-uefi-physical-policy-driver.efi",
        ),
        (
            "physical-policy-payload",
            "x86-uefi-physical-policy-payload.efi",
        ),
    ] {
        eprintln!("\n--- Building non-VMX QEMU-only fixture: {feature} ---");
        let status = Command::new("cargo")
            .args([
                "build",
                "-p",
                guest_pkg,
                "--bin",
                "x86-uefi-physical-policy",
            ])
            .args(["--target", "x86_64-unknown-uefi"])
            .args(args)
            .args(["--no-default-features", "--features", feature])
            .env("XTASK_BUILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| format!("Failed to build {feature} fixture: {error}"))?;
        if !status.success() {
            return Err(format!(
                "{feature} fixture build failed with status: {status}"
            ));
        }
        verify_no_decoded_vmx(&policy_artifact)?;
        let policy_destination = workspace.join("bin/x86_64").join(filename);
        fs::copy(&policy_artifact, &policy_destination).map_err(|error| {
            format!(
                "Failed to copy {} to {}: {error}",
                policy_artifact.display(),
                policy_destination.display()
            )
        })?;
    }
    for fixture in ["host-exception", "host-xstate"] {
        build_x86_host_fixture(args, &workspace, &artifact, fixture)?;
    }
    for (feature, filename) in [
        ("nested-contract", "x86-uefi-nested-contract.efi"),
        ("msr-contract", "x86-uefi-msr-contract.efi"),
        ("msr-abort-store", "x86-uefi-msr-abort-store.efi"),
        ("msr-abort-load", "x86-uefi-msr-abort-load.efi"),
    ] {
        let status = Command::new("cargo")
            .args([
                "build",
                "-p",
                guest_pkg,
                "--bin",
                "x86-uefi-nested-contract",
            ])
            .args(["--target", "x86_64-unknown-uefi"])
            .args(args)
            .args(["--no-default-features", "--features", feature])
            .env("XTASK_BUILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| format!("Failed to build nested VMX contract fixture: {error}"))?;
        if !status.success() {
            return Err(format!(
                "nested VMX contract fixture build failed: {status}"
            ));
        }
        let contract_artifact = workspace
            .join("target/x86_64-unknown-uefi")
            .join(resolve_profile(args))
            .join("x86-uefi-nested-contract.efi");
        fs::copy(
            &contract_artifact,
            workspace.join("bin/x86_64").join(filename),
        )
        .map_err(|error| format!("Failed to stage nested VMX contract fixture: {error}"))?;
    }
    Ok(destination.to_string_lossy().into_owned())
}

/// Builds QEMU-only host fixtures without replacing normal backend images.
fn build_x86_host_fixture(
    args: &[String],
    workspace: &Path,
    artifact: &Path,
    fixture: &str,
) -> Result<(), String> {
    eprintln!("\n--- Building test-only Direct-VMX {fixture} fixture ---");
    let status = Command::new("cargo")
        .args(["build", "-p", "x86_uefi_loader", "--bin", "x86-uefi-loader"])
        .args(["--target", "x86_64-unknown-uefi"])
        .args(args)
        .args([
            "--no-default-features",
            "--features",
            &format!("{fixture}-test"),
        ])
        .env("XTASK_BUILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("Failed to build {fixture} fixture: {error}"))?;
    if !status.success() {
        return Err(format!(
            "{fixture} fixture build failed with status: {status}"
        ));
    }
    let stage = workspace.join("bin/x86_64");
    verify_monitor_isa(artifact)?;
    let monitor = stage.join(format!("x86-uefi-{fixture}-monitor.efi"));
    for destination in [
        stage.join(format!("x86-uefi-{fixture}-loader.efi")),
        monitor.clone(),
    ] {
        fs::copy(artifact, &destination).map_err(|error| {
            format!(
                "Failed to copy {fixture} fixture to {}: {error}",
                destination.display()
            )
        })?;
    }
    let status = Command::new("objcopy")
        .arg("--subsystem=efi-rtd")
        .arg(&monitor)
        .status()
        .map_err(|error| format!("Failed to convert {fixture} runtime image: {error}"))?;
    if !status.success() {
        return Err(format!(
            "{fixture} runtime conversion failed with status: {status}"
        ));
    }
    Ok(())
}

const VMX_MNEMONICS: [&str; 17] = [
    "vmcall", "vmclear", "vmlaunch", "vmresume", "vmptrld", "vmptrst", "vmread", "vmreadl",
    "vmreadq", "vmwrite", "vmwritel", "vmwriteq", "vmxoff", "vmxon", "invept", "invvpid", "vmfunc",
];

/// Reject instructions that escape the exit stub's legacy FP/SIMD bracket.
/// Input uses objdump's no-raw-bytes format, so operands/symbols are not decoded
/// as mnemonics. This covers linked dependencies and target_feature functions.
fn unpreserved_monitor_instruction(disassembly: &str) -> Option<&str> {
    disassembly
        .lines()
        .filter_map(|line| {
            let (address, decoded) = line.trim().split_once(':')?;
            if address.is_empty() || !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            decoded.split_ascii_whitespace().next()
        })
        .find(|mnemonic| {
            (mnemonic.starts_with('v') && !VMX_MNEMONICS.contains(mnemonic))
                || mnemonic.starts_with('k')
                || mnemonic.starts_with("tile")
                || mnemonic.starts_with("xsave")
                || mnemonic.starts_with("xrstor")
                || matches!(*mnemonic, "ldtilecfg" | "wrpkru" | "(bad)")
        })
}

fn verify_monitor_isa(artifact: &Path) -> Result<(), String> {
    let output = Command::new("objdump")
        .args(["-d", "--no-show-raw-insn"])
        .arg(artifact)
        .output()
        .map_err(|error| format!("Failed to audit {}: {error}", artifact.display()))?;
    if !output.status.success() {
        return Err(format!("Monitor ISA disassembly failed: {}", output.status));
    }
    let decoded = String::from_utf8_lossy(&output.stdout);
    if let Some(instruction) = unpreserved_monitor_instruction(&decoded) {
        return Err(format!(
            "Monitor {} uses unpreserved instruction {instruction}",
            artifact.display()
        ));
    }
    if !decoded.contains("fxsave64") || !decoded.contains("fxrstor64") {
        return Err(format!(
            "Monitor {} lacks its FP save/restore bracket",
            artifact.display()
        ));
    }
    eprintln!(
        "Monitor ISA: PASS baseline x87/SSE with FXSAVE64/FXRSTOR64 ({})",
        artifact.display()
    );
    Ok(())
}

fn decoded_vmx_mnemonic(disassembly: &str) -> Option<&str> {
    disassembly
        .split_ascii_whitespace()
        .find(|word| VMX_MNEMONICS.contains(word))
}

fn verify_no_decoded_vmx(artifact: &Path) -> Result<(), String> {
    let output = Command::new("objdump")
        .arg("-d")
        .arg(artifact)
        .output()
        .map_err(|error| format!("Failed to disassemble {}: {}", artifact.display(), error))?;
    if !output.status.success() {
        return Err(format!(
            "objdump failed for {} with status: {}",
            artifact.display(),
            output.status
        ));
    }
    let disassembly = String::from_utf8_lossy(&output.stdout);
    if let Some(mnemonic) = decoded_vmx_mnemonic(&disassembly) {
        return Err(format!(
            "non-VMX artifact {} contains decoded VMX instruction '{}'",
            artifact.display(),
            mnemonic
        ));
    }
    Ok(())
}

fn build_rpi5(args: &[String]) -> Result<String, String> {
    let pkg = "rpi_boot";
    eprintln!("\n--- Building rpi5 package: {} ---", pkg);
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("-Z")
        .arg("build-std=core,alloc,compiler_builtins")
        .arg("-Z")
        .arg("build-std-features=compiler-builtins-mem")
        .arg("-p")
        .arg(pkg)
        .arg("--target")
        .arg("aarch64-unknown-none-softfloat")
        .args(args)
        .env("XTASK_BUILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    eprintln!("Running: {:?}", cmd);
    let status = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn cargo build for {}: {}", pkg, e))
        .wait()
        .unwrap_or_else(|e| panic!("Failed to wait for cargo build for {}: {}", pkg, e));
    if !status.success() {
        eprintln!(
            "Error: cargo build failed for package '{}' with status: {:?}",
            pkg, status
        );
        std::process::exit(status.code().unwrap_or(1));
    }

    eprintln!("\n--- rpi_boot built successfully ---");
    let profile = resolve_profile(args);
    copy_artifact_to_bin("rpi_boot", "rpi_boot.elf", &profile)
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("x86") => run_x86_uefi(&args[1..]),
        Some("rpi5") => run_rpi5(&args[1..]),
        Some("rpi4") => run_rpi4(&args[1..]),
        Some("net") => run_net(&args[1..]),
        _ => {
            run_default(args);
        }
    }
}

/// Runs every bounded nested case even when another backend or case fails.
/// The shared UEFI staging directory requires serial execution of these VMs.
fn run_x86_nested() -> Result<(), String> {
    let mut failures = Vec::new();
    for (backend, cpu_profile) in [
        ("outer-kvm", "native"),
        ("direct-vmx", "native"),
        ("outer-kvm", "readonly-vmcs"),
        ("direct-vmx", "readonly-vmcs"),
        ("direct-vmx", "host-xstate"),
        ("outer-kvm", "msr"),
        ("direct-vmx", "msr"),
        ("outer-kvm", "msr-abort-store"),
        ("direct-vmx", "msr-abort-store"),
        ("outer-kvm", "msr-abort-load"),
        ("direct-vmx", "msr-abort-load"),
    ] {
        let msr = cpu_profile.starts_with("msr");
        let abort = match cpu_profile {
            "msr-abort-store" => "1",
            "msr-abort-load" => "4",
            _ => "0",
        };
        match fs::remove_file("bin/x86_64/serial.log") {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                failures.push(format!("Cannot clear prior contract evidence: {error}"));
                continue;
            }
        }
        let (loader, monitor) = if cpu_profile == "host-xstate" {
            (
                "x86-uefi-host-xstate-loader.efi",
                "bin/x86_64/x86-uefi-host-xstate-monitor.efi",
            )
        } else if backend == "direct-vmx" {
            ("x86-uefi-loader.efi", "bin/x86_64/x86-uefi-monitor.efi")
        } else {
            ("x86-uefi-kvm-loader.efi", "")
        };
        eprintln!(
            "\n--- Nested VMX instruction contract backend={backend} cpu_profile={cpu_profile} environment=QEMU/kvm ---"
        );
        let result = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
            .arg(Path::new("bin/x86_64").join(loader))
            .arg(if abort == "1" {
                "bin/x86_64/x86-uefi-msr-abort-store.efi"
            } else if abort == "4" {
                "bin/x86_64/x86-uefi-msr-abort-load.efi"
            } else if msr {
                "bin/x86_64/x86-uefi-msr-contract.efi"
            } else {
                "bin/x86_64/x86-uefi-nested-contract.efi"
            })
            .env("X86_UEFI_BACKEND", backend)
            .env("X86_MONITOR_IMAGE", monitor)
            .env("X86_UEFI_PHYSICAL_POLICY", "0")
            .env("X86_UEFI_HOST_EXCEPTION_TEST", "0")
            .env("X86_UEFI_MSR_ABORT_TEST", abort)
            .env("X86_UEFI_ACCEL", "kvm")
            .env(
                "X86_UEFI_CPU",
                if cpu_profile == "readonly-vmcs" {
                    "host,+vmx,-hypervisor,kvm=off,vmx-vmwrite-vmexit-fields=off"
                } else {
                    "host,+vmx,-hypervisor,kvm=off"
                },
            )
            .env("X86_UEFI_MEMORY", "256M")
            .env("X86_UEFI_SMP", "1")
            .env(
                "X86_UEFI_TIMEOUT_SECONDS",
                if abort == "0" { "30" } else { "10" },
            )
            .env("X86_UEFI_GUEST_LOCATION", "guest")
            .env("X86_UEFI_ACPI_S3", "0")
            .env("X86_UEFI_WAKE_CYCLES", "0")
            .env("X86_UEFI_ALLOW_REBOOT", "0")
            .env(
                "X86_UEFI_REQUIRE_POWEROFF",
                if msr && abort == "0" { "1" } else { "0" },
            )
            .env("X86_UEFI_USERNET", "0")
            .env_remove("X86_UEFI_DATA_DISK")
            .env(
                "X86_RETURN_MARKER",
                if msr {
                    ""
                } else if backend == "direct-vmx" {
                    "thin-hv: vmx guest PASS"
                } else {
                    "thin-hv: trusted outer KVM guest PASS"
                },
            )
            .env("X86_VARIABLE_MARKER", "")
            .env(
                "X86_GUEST_MARKER",
                if msr {
                    "thin-hv: MSR contract PASS"
                } else {
                    "thin-hv: nested contract PASS"
                },
            )
            .env(
                "X86_GUEST_FAILURE_MARKER",
                if msr {
                    "thin-hv: MSR contract FAIL"
                } else {
                    "thin-hv: nested contract FAIL"
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        let result = match result {
            Ok(status) if status.success() && abort == "0" => Command::new("bash")
                .arg("./scripts/x86_64/run-uefi-smoke.sh")
                .args(if msr {
                    vec!["--check-msr-contract-log", backend, "bin/x86_64/serial.log"]
                } else {
                    vec![
                        "--check-nested-contract-log",
                        backend,
                        cpu_profile,
                        "bin/x86_64/serial.log",
                    ]
                })
                .status()
                .map_err(|error| error.to_string()),
            Ok(status) => Ok(status),
            Err(error) => Err(error.to_string()),
        };
        match result {
            Ok(status) if status.success() => {
                eprintln!("nested contract: PASS backend={backend} cpu_profile={cpu_profile}")
            }
            other => failures.push(format!(
                "nested contract backend={backend} cpu_profile={cpu_profile}: {other:?}"
            )),
        }
        let evidence = format!("bin/x86_64/nested-contract-{backend}-{cpu_profile}.log");
        if let Err(error) = fs::copy("bin/x86_64/serial.log", &evidence) {
            failures.push(format!("Failed to preserve {evidence}: {error}"));
        }
        if abort != "0" {
            let memory_evidence =
                format!("bin/x86_64/nested-contract-{backend}-{cpu_profile}-memory.log");
            if let Err(error) = fs::copy("bin/x86_64/qemu.log", &memory_evidence) {
                failures.push(format!("Failed to preserve {memory_evidence}: {error}"));
            }
        }
    }
    for (backend, host_xstate, memory) in [
        ("outer-kvm", "0", "2G"),
        ("direct-vmx", "0", "2G"),
        ("direct-vmx", "1", "2G"),
        ("direct-vmx", "0", "4G"),
        ("direct-vmx", "0", "12G"),
    ] {
        match fs::remove_file("bin/x86_64/serial.log") {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                failures.push(format!("Cannot clear prior Linux evidence: {error}"));
                continue;
            }
        }
        eprintln!(
            "\n--- Nested Linux state/lifetime backend={backend} host_xstate={host_xstate} memory={memory} environment=QEMU/kvm ---"
        );
        let result = Command::new("./scripts/x86_64/run-linux-kvm-test.sh")
            .env("LINUX_KVM_BACKEND", backend)
            .env("LINUX_KVM_HOST_XSTATE_TEST", host_xstate)
            .env("LINUX_KVM_MEMORY", memory)
            .env("X86_UEFI_PCI_PROFILE", "firmware-default")
            .env(
                "X86_UEFI_REQUIRE_HIGH_PCI",
                if memory == "2G" { "0" } else { "1" },
            )
            .env("X86_UEFI_MSR_ABORT_TEST", "0")
            .env("X86_UEFI_PHYSICAL_POLICY", "0")
            .env("X86_UEFI_HOST_EXCEPTION_TEST", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        match result {
            Ok(status) if status.success() => {
                eprintln!("nested Linux: PASS backend={backend} host_xstate={host_xstate} memory={memory}")
            }
            other => failures.push(format!(
                "nested Linux backend={backend} host_xstate={host_xstate} memory={memory}: {other:?}"
            )),
        }
        let suffix = if memory == "12G" {
            "-high-pci-12g"
        } else if memory == "4G" {
            "-high-pci-4g"
        } else if host_xstate == "1" {
            "-host-xstate"
        } else {
            ""
        };
        let evidence = format!("bin/x86_64/nested-linux-{backend}{suffix}.log");
        if let Err(error) = fs::copy("bin/x86_64/serial.log", &evidence) {
            failures.push(format!("Failed to preserve {evidence}: {error}"));
        }
    }
    if failures.is_empty() {
        eprintln!(
            "nested suite: PASS environment=QEMU/kvm l1_cpus=1 physical_hardware=unverified hyperv=unverified"
        );
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn run_x86_uefi(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--nested") {
        let build_args: Vec<_> = args
            .iter()
            .filter(|arg| *arg != "--nested")
            .cloned()
            .collect();
        build_x86_uefi(&build_args)?;
        return run_x86_nested();
    }
    let binary_path = build_x86_uefi(args)?;
    eprintln!("\n--- Running QEMU/KVM direct-vmx / project L0 smoke test ---");
    let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
        .arg(binary_path)
        .env("X86_UEFI_BACKEND", "direct-vmx")
        .env("X86_UEFI_PHYSICAL_POLICY", "0")
        .env("X86_UEFI_HOST_EXCEPTION_TEST", "0")
        .env("X86_UEFI_ACCEL", "kvm")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("Failed to run x86 UEFI smoke test: {}", e))?;

    if !status.success() {
        return Err(format!(
            "direct x86 UEFI smoke test exited with status {}",
            status
        ));
    }

    eprintln!(
        "\n--- Running expected root exception fixture (QEMU/KVM, not physical hardware) ---"
    );
    let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
        .arg("bin/x86_64/x86-uefi-host-exception-loader.efi")
        .arg("bin/x86_64/x86_guest_uefi_test.efi")
        .env(
            "X86_MONITOR_IMAGE",
            "bin/x86_64/x86-uefi-host-exception-monitor.efi",
        )
        .env("X86_UEFI_BACKEND", "direct-vmx")
        .env("X86_UEFI_PHYSICAL_POLICY", "0")
        .env("X86_UEFI_HOST_EXCEPTION_TEST", "1")
        .env("X86_UEFI_ACCEL", "kvm")
        .env("X86_UEFI_CPU", "host,+vmx,-hypervisor")
        .env("X86_UEFI_MEMORY", "256M")
        .env("X86_UEFI_SMP", "1")
        .env("X86_UEFI_TIMEOUT_SECONDS", "30")
        .env("X86_UEFI_GUEST_LOCATION", "guest")
        .env("X86_UEFI_ACPI_S3", "0")
        .env("X86_UEFI_WAKE_CYCLES", "0")
        .env("X86_UEFI_ALLOW_REBOOT", "0")
        .env("X86_UEFI_REQUIRE_POWEROFF", "0")
        .env("X86_UEFI_USERNET", "0")
        .env_remove("X86_UEFI_DATA_DISK")
        .env_remove("X86_RETURN_MARKER")
        .env_remove("X86_VARIABLE_MARKER")
        .env_remove("X86_GUEST_MARKER")
        .env_remove("X86_GUEST_FAILURE_MARKER")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("Failed to run root-exception fixture: {error}"))?;
    if !status.success() {
        return Err(format!(
            "root-exception fixture failed with status: {status}"
        ));
    }

    for guest_location in ["guest", "windows", "both"] {
        eprintln!("\n--- Running QEMU/KVM outer-kvm / reference smoke test ({guest_location}) ---");
        let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
            .arg("bin/x86_64/x86-uefi-kvm-loader.efi")
            .env("X86_UEFI_BACKEND", "outer-kvm")
            .env("X86_UEFI_PHYSICAL_POLICY", "0")
            .env("X86_UEFI_HOST_EXCEPTION_TEST", "0")
            .env("X86_UEFI_ACCEL", "kvm")
            .env("X86_MONITOR_IMAGE", "")
            .env("X86_RETURN_MARKER", "thin-hv: trusted outer KVM guest PASS")
            .env("X86_VARIABLE_MARKER", "thin-hv: uefi native variables PASS")
            .env("X86_UEFI_CPU", "host,+vmx,-hypervisor,kvm=off")
            .env("X86_UEFI_GUEST_LOCATION", guest_location)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to run trusted-outer-KVM UEFI smoke test: {}", e))?;

        if !status.success() {
            return Err(format!(
                "trusted-outer-KVM UEFI smoke test ({guest_location}) exited with status {status}"
            ));
        }
    }
    for (backend, filename, accel, cpu) in [
        (
            "physical-chainload",
            "x86-uefi-physical-loader.efi",
            "kvm",
            // Match the reference fixture: the shared payload checks both the
            // hypervisor bit and the synthetic KVM CPUID signature leaf.
            "host,+vmx,-hypervisor,kvm=off",
        ),
        (
            "physical-preflight",
            "x86-uefi-preflight.efi",
            "kvm",
            "host,+vmx,-hypervisor",
        ),
        (
            "physical-preflight",
            "x86-uefi-preflight.efi",
            "tcg",
            "qemu64",
        ),
    ] {
        eprintln!("\n--- Running QEMU/{accel} {backend} regression (not physical hardware) ---");
        let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
            .arg(Path::new("bin/x86_64").join(filename))
            .env("X86_UEFI_BACKEND", backend)
            .env("X86_UEFI_PHYSICAL_POLICY", "0")
            .env("X86_UEFI_HOST_EXCEPTION_TEST", "0")
            .env("X86_UEFI_ACCEL", accel)
            .env("X86_UEFI_CPU", cpu)
            .env(
                "X86_UEFI_TIMEOUT_SECONDS",
                if accel == "tcg" { "60" } else { "30" },
            )
            .env_remove("X86_MONITOR_IMAGE")
            .env_remove("X86_RETURN_MARKER")
            .env_remove("X86_VARIABLE_MARKER")
            .env_remove("X86_GUEST_MARKER")
            .env_remove("X86_UEFI_GUEST_LOCATION")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to run {backend} QEMU/{accel} regression: {e}"))?;
        if !status.success() {
            return Err(format!(
                "{backend} QEMU/{accel} regression exited with status {status}"
            ));
        }
    }
    eprintln!(
        "\n--- Running physical-chainload policy fixture (QEMU/TCG, not physical hardware) ---"
    );
    let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
        .arg("bin/x86_64/x86-uefi-physical-policy-driver.efi")
        .arg("bin/x86_64/x86-uefi-physical-policy-payload.efi")
        .env("X86_UEFI_BACKEND", "physical-chainload")
        .env("X86_UEFI_PHYSICAL_POLICY", "1")
        .env("X86_UEFI_HOST_EXCEPTION_TEST", "0")
        .env("X86_UEFI_ACCEL", "tcg")
        .env("X86_UEFI_CPU", "qemu64")
        .env("X86_UEFI_TIMEOUT_SECONDS", "60")
        .env("X86_UEFI_MEMORY", "256M")
        .env("X86_UEFI_SMP", "1")
        .env_remove("X86_MONITOR_IMAGE")
        .env_remove("X86_RETURN_MARKER")
        .env_remove("X86_VARIABLE_MARKER")
        .env_remove("X86_GUEST_MARKER")
        .env_remove("X86_UEFI_GUEST_LOCATION")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("Failed to run physical policy QEMU/TCG fixture: {error}"))?;
    if !status.success() {
        return Err(format!(
            "physical policy QEMU/TCG fixture exited with status {status}"
        ));
    }
    Ok(())
}

fn run_default(args: &[String]) -> ! {
    let binary_path = build_bootloader(args).unwrap_or_else(|err| {
        panic!("Failed to build bootloader: {}", err);
    });

    eprintln!("\n--- Running ./run.sh ---");
    use std::os::unix::process::CommandExt;
    let err = Command::new("./run.sh").arg(&binary_path).args(args).exec();
    panic!("Failed to exec ./run.sh: {}", err);
}

fn run_rpi4(args: &[String]) -> ! {
    let binary_path = build_bootloader_with_feature(args, "rpi4").unwrap_or_else(|err| {
        panic!("Failed to build rpi4 bootloader: {}", err);
    });

    eprintln!("\n--- Running ./run_rpi4.sh ---");
    use std::os::unix::process::CommandExt;
    let err = Command::new("./run_rpi4.sh")
        .arg(&binary_path)
        .args(args)
        .exec();
    panic!("Failed to exec ./run_rpi4.sh: {}", err);
}

const TCP_LISTEN: &str = "127.0.0.1:3333";
const QEMU_FWD_GDB_UDP_DST: &str = "127.0.0.1:40000";
const QEMU_FWD_DBG_UDP_DST: &str = "127.0.0.1:40010";
const PROXY_GDB_UDP_SRC_BIND: &str = "127.0.0.1:40001";
const PROXY_DBG_UDP_SRC_BIND: &str = "127.0.0.1:40011";

fn run_single_gdb_bridge(
    tcp_stream: TcpStream,
    udp_gdb: UdpSocket,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    tcp_stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| format!("Failed to set TCP read timeout: {}", e))?;
    udp_gdb
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| format!("Failed to set UDP read timeout: {}", e))?;

    let mut tcp_reader = tcp_stream
        .try_clone()
        .map_err(|e| format!("Failed to clone TCP stream: {}", e))?;
    let mut tcp_writer = tcp_stream;
    let udp_tx = udp_gdb
        .try_clone()
        .map_err(|e| format!("Failed to clone UDP socket: {}", e))?;
    let udp_rx = udp_gdb;

    let stop_tx = stop.clone();
    let tcp_to_udp = thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            if stop_tx.load(Ordering::Acquire) {
                break;
            }
            match tcp_reader.read(&mut buf) {
                Ok(0) => {
                    stop_tx.store(true, Ordering::Release);
                    break;
                }
                Ok(n) => {
                    if udp_tx.send(&buf[..n]).is_err() {
                        stop_tx.store(true, Ordering::Release);
                        break;
                    }
                }
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => {
                    stop_tx.store(true, Ordering::Release);
                    break;
                }
            }
        }
    });

    let stop_rx = stop.clone();
    let udp_to_tcp = thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            if stop_rx.load(Ordering::Acquire) {
                break;
            }
            match udp_rx.recv(&mut buf) {
                Ok(0) => continue,
                Ok(n) => {
                    if tcp_writer.write_all(&buf[..n]).is_err() {
                        stop_rx.store(true, Ordering::Release);
                        break;
                    }
                }
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => {
                    stop_rx.store(true, Ordering::Release);
                    break;
                }
            }
        }
        let _ = tcp_writer.shutdown(Shutdown::Both);
    });

    if tcp_to_udp.join().is_err() {
        stop.store(true, Ordering::Release);
        return Err("GDB proxy TCP->UDP thread panicked".to_string());
    }
    if udp_to_tcp.join().is_err() {
        stop.store(true, Ordering::Release);
        return Err("GDB proxy UDP->TCP thread panicked".to_string());
    }
    Ok(())
}

fn run_net(args: &[String]) -> Result<(), String> {
    let binary_path = build_bootloader_with_feature(args, "virtio_net")?;
    let stop = Arc::new(AtomicBool::new(false));

    let udp_gdb = UdpSocket::bind(PROXY_GDB_UDP_SRC_BIND).map_err(|e| {
        format!(
            "Failed to bind GDB UDP proxy socket {}: {}",
            PROXY_GDB_UDP_SRC_BIND, e
        )
    })?;
    udp_gdb
        .connect(QEMU_FWD_GDB_UDP_DST)
        .map_err(|e| format!("Failed to connect GDB UDP proxy socket: {}", e))?;

    let udp_dbg = UdpSocket::bind(PROXY_DBG_UDP_SRC_BIND).map_err(|e| {
        format!(
            "Failed to bind debug UDP proxy socket {}: {}",
            PROXY_DBG_UDP_SRC_BIND, e
        )
    })?;
    udp_dbg
        .connect(QEMU_FWD_DBG_UDP_DST)
        .map_err(|e| format!("Failed to connect debug UDP proxy socket: {}", e))?;
    udp_dbg
        .send(b"prime")
        .map_err(|e| format!("Failed to send debug priming datagram: {}", e))?;

    let debug_socket = udp_dbg
        .try_clone()
        .map_err(|e| format!("Failed to clone debug UDP socket: {}", e))?;
    debug_socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| format!("Failed to set debug UDP read timeout: {}", e))?;
    let debug_stop = stop.clone();
    let debug_handle = thread::spawn(move || {
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        let mut buf = [0u8; 2048];
        while !debug_stop.load(Ordering::Acquire) {
            match debug_socket.recv(&mut buf) {
                Ok(0) => continue,
                Ok(n) => {
                    let _ = stderr.write_all(&buf[..n]);
                    let _ = stderr.flush();
                }
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let listener = TcpListener::bind(TCP_LISTEN)
        .map_err(|e| format!("Failed to bind GDB TCP listener {}: {}", TCP_LISTEN, e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to configure GDB TCP listener: {}", e))?;
    eprintln!("xrun net: GDB proxy listening on {}", TCP_LISTEN);

    let accept_stop = stop.clone();
    let accept_handle = thread::spawn(move || -> Result<(), String> {
        while !accept_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    return run_single_gdb_bridge(stream, udp_gdb, accept_stop.clone());
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(format!("Failed to accept GDB TCP client: {}", err)),
            }
        }
        Ok(())
    });

    let mut qemu = match Command::new("./run.sh")
        .arg(&binary_path)
        .args(args)
        .env("RUN_NET", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(TCP_LISTEN);
            let _ = accept_handle.join();
            let _ = debug_handle.join();
            return Err(format!("Failed to spawn ./run.sh: {}", err));
        }
    };

    let status = match qemu.wait() {
        Ok(status) => status,
        Err(err) => {
            stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(TCP_LISTEN);
            let _ = accept_handle.join();
            let _ = debug_handle.join();
            return Err(format!("Failed to wait for ./run.sh: {}", err));
        }
    };

    stop.store(true, Ordering::Release);
    let _ = TcpStream::connect(TCP_LISTEN);

    let accept_result = match accept_handle.join() {
        Ok(result) => result,
        Err(_) => Err("GDB proxy accept thread panicked".to_string()),
    };
    let debug_result = debug_handle.join();

    if debug_result.is_err() {
        return Err("Debug UDP mirror thread panicked".to_string());
    }
    accept_result?;

    if !status.success() {
        return Err(format!("./run.sh exited with status {}", status));
    }
    Ok(())
}

fn run_rpi5(args: &[String]) -> Result<(), String> {
    let elf_path = PathBuf::from(build_rpi5(args)?);
    let mut img_path = elf_path.clone();
    img_path.set_file_name("kernel_2712.img");

    eprintln!(
        "\n--- Converting {} to raw image: {} ---",
        elf_path.display(),
        img_path.display()
    );

    let status = Command::new("rust-objcopy")
        .arg("-O")
        .arg("binary")
        .arg(&elf_path)
        .arg(&img_path)
        .status()
        .map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => "rust-objcopy is required but not available in PATH. \
                 Enter the nix develop shell or install rust-objcopy."
                .to_string(),
            _ => format!("Failed to launch rust-objcopy: {}", err),
        })?;

    if status.success() {
        eprintln!("Image generated at {}", img_path.display());
        Ok(())
    } else {
        Err(format!("rust-objcopy exited with status {}", status))
    }
}

fn workspace_root() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("Failed to determine workspace root: {}", e))
}

fn copy_artifact_to_bin(
    binary_name: &str,
    destination_name: &str,
    profile: &str,
) -> Result<String, String> {
    let workspace = workspace_root()?;
    let artifact_path = workspace
        .join("target")
        .join("aarch64-unknown-none-softfloat")
        .join(profile)
        .join(binary_name);

    let bin_dir = workspace.join("bin");
    fs::create_dir_all(&bin_dir).map_err(|e| format!("Failed to create bin directory: {}", e))?;

    let destination = bin_dir.join(destination_name);
    fs::copy(&artifact_path, &destination).map_err(|e| {
        format!(
            "Failed to copy {} to {}: {}",
            artifact_path.display(),
            destination.display(),
            e
        )
    })?;

    Ok(destination.to_string_lossy().into_owned())
}

fn resolve_profile(args: &[String]) -> String {
    for arg in args {
        if let Some(value) = arg.strip_prefix("--profile=") {
            return value.to_owned();
        }
    }

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--profile"
            && let Some(value) = iter.next()
        {
            return value.clone();
        }
    }

    if args.iter().any(|arg| arg == "--release") {
        return "release".to_owned();
    }

    "debug".to_owned()
}

fn resolve_gdb_executable() -> Option<String> {
    // Prefer gdb-multiarch if available, fall back to gdb.
    for candidate in &["gdb-multiarch", "gdb"] {
        let status = Command::new(candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Ok(status) = status
            && status.success()
        {
            return Some((*candidate).to_string());
        }
    }
    None
}

#[cfg(unix)]
fn spawn_in_own_pgrp(cmd: &mut Command) -> io::Result<Child> {
    use core::ffi::c_int;
    use std::os::unix::process::CommandExt;

    // SAFETY: This runs in the child just before exec; setpgid is async-signal-safe and
    // we only touch state local to the child process to move it into a new process group.
    unsafe {
        cmd.pre_exec(|| {
            unsafe extern "C" {
                fn setpgid(pid: c_int, pgid: c_int) -> c_int;
            }

            if setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
}

#[cfg(not(unix))]
fn spawn_in_own_pgrp(cmd: &mut Command) -> io::Result<Child> {
    cmd.spawn()
}

#[cfg(unix)]
fn kill_process_tree_best_effort(label: &str, child: &mut Child) {
    use core::ffi::c_int;

    const SIGTERM: c_int = 15;
    const SIGKILL: c_int = 9;

    unsafe extern "C" {
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    let pgid = match i32::try_from(child.id()) {
        Ok(pid) => pid,
        Err(_) => {
            eprintln!(
                "Warning: PID {} for {} does not fit into i32; falling back to child.kill()",
                child.id(),
                label
            );
            if let Err(e) = child.kill() {
                eprintln!("Warning: failed to kill {}: {}", label, e);
            }
            let _ = child.wait();
            return;
        }
    };

    let mut wait_for_exit = |deadline: Instant| -> bool {
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    eprintln!(
                        "Warning: failed to poll {} while terminating process group: {}",
                        label, e
                    );
                    return false;
                }
            }
        }
    };

    let send_signal = |sig: c_int| {
        let res = unsafe { kill(-(pgid as c_int), sig) };
        if res != 0 {
            let e = io::Error::last_os_error();
            eprintln!(
                "Warning: failed to send signal {} to process group {} for {}: {}",
                sig, pgid, label, e
            );
        }
    };

    send_signal(SIGTERM);
    let mut exited = wait_for_exit(Instant::now() + Duration::from_secs(2));

    if !exited {
        send_signal(SIGKILL);
        exited = wait_for_exit(Instant::now() + Duration::from_secs(2));
    }

    if !exited && let Ok(Some(_)) = child.try_wait() {
        exited = true;
    }

    if !exited {
        eprintln!(
            "Warning: process group {} for {} may still be running after SIGKILL",
            pgid, label
        );
    }
}

#[cfg(not(unix))]
fn kill_process_tree_best_effort(label: &str, child: &mut Child) {
    if let Err(e) = child.kill() {
        eprintln!("Warning: failed to kill {}: {}", label, e);
    }
    let _ = child.wait();
}

#[derive(Debug, Clone, Copy)]
enum FilterState {
    Normal,
    AfterEsc,
    Csi,
}

#[derive(Debug)]
struct AnsiQueryFilter {
    state: FilterState,
    pending: Vec<u8>,
}

impl AnsiQueryFilter {
    const MAX_PENDING: usize = 4096;

    fn new() -> Self {
        Self {
            state: FilterState::Normal,
            pending: Vec::new(),
        }
    }

    fn push_filtered(&mut self, input: &[u8], output: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                FilterState::Normal => {
                    if b == 0x1b {
                        self.pending.clear();
                        self.pending.push(b);
                        self.state = FilterState::AfterEsc;
                    } else {
                        output.push(b);
                    }
                }
                FilterState::AfterEsc => {
                    self.pending.push(b);
                    match b {
                        b'[' => {
                            self.state = FilterState::Csi;
                        }
                        b'Z' => {
                            self.pending.clear();
                            self.state = FilterState::Normal;
                        }
                        _ => {
                            self.flush_pending(output);
                            self.state = FilterState::Normal;
                        }
                    }
                }
                FilterState::Csi => {
                    self.pending.push(b);
                    if Self::is_csi_final(b) {
                        if Self::should_drop_csi(&self.pending, b) {
                            self.pending.clear();
                        } else {
                            self.flush_pending(output);
                        }
                        self.state = FilterState::Normal;
                    } else if Self::is_csi_param_or_intermediate(b) {
                        // Keep buffering until we see a final byte.
                    } else {
                        // Not a valid CSI continuation; flush what we saw.
                        self.flush_pending(output);
                        self.state = FilterState::Normal;
                    }
                }
            }

            if self.pending.len() > Self::MAX_PENDING {
                self.flush_pending(output);
                self.state = FilterState::Normal;
            }
        }
    }

    fn finish_into(&mut self, output: &mut Vec<u8>) {
        if !self.pending.is_empty() {
            output.extend_from_slice(&self.pending);
            self.pending.clear();
        }
        self.state = FilterState::Normal;
    }

    fn flush_pending(&mut self, output: &mut Vec<u8>) {
        if !self.pending.is_empty() {
            output.extend_from_slice(&self.pending);
            self.pending.clear();
        }
        self.state = FilterState::Normal;
    }

    fn should_drop_csi(pending: &[u8], final_byte: u8) -> bool {
        match final_byte {
            b'c' => true,
            b'n' => Self::is_dsr_or_cpr_query(pending),
            _ => false,
        }
    }

    fn is_dsr_or_cpr_query(pending: &[u8]) -> bool {
        // Drop CSI Ps n where the last numeric parameter is 5 (status) or 6 (cursor position),
        // with an optional private marker ('?') after CSI.
        if pending.len() < 3 || pending[0] != 0x1b || pending[1] != b'[' {
            return false;
        }
        if *pending.last().unwrap_or(&0) != b'n' {
            return false;
        }

        let mut params = &pending[2..pending.len() - 1];
        if let Some(b'?') = params.first() {
            params = &params[1..];
        }

        if params.is_empty() {
            return false;
        }

        let mut last_param: Option<u32> = None;
        let mut current: u32 = 0;
        let mut has_digits = false;

        for &b in params {
            match b {
                b'0'..=b'9' => {
                    current = current
                        .saturating_mul(10)
                        .saturating_add(u32::from(b - b'0'));
                    has_digits = true;
                }
                b';' => {
                    if has_digits {
                        last_param = Some(current);
                    } else {
                        last_param = None;
                    }
                    current = 0;
                    has_digits = false;
                }
                _ => return false,
            }
        }

        if has_digits {
            last_param = Some(current);
        }

        matches!(last_param, Some(5) | Some(6))
    }

    fn is_csi_final(b: u8) -> bool {
        (0x40..=0x7e).contains(&b)
    }

    fn is_csi_param_or_intermediate(b: u8) -> bool {
        (0x30..=0x3f).contains(&b) || (0x20..=0x2f).contains(&b)
    }
}

fn pump_filtered_output<R, W>(mut reader: R, mut writer: W)
where
    R: Read,
    W: Write,
{
    let mut buf = [0u8; 4096];
    let mut filtered = Vec::with_capacity(buf.len());
    let mut filter = AnsiQueryFilter::new();

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                filtered.clear();
                filter.push_filtered(&buf[..n], &mut filtered);
                if !filtered.is_empty()
                    && let Err(e) = writer.write_all(&filtered)
                {
                    eprintln!("Warning: failed to write child output: {}", e);
                    break;
                }
                let _ = writer.flush();
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("Warning: failed to read child output: {}", e);
                break;
            }
        }
    }

    filtered.clear();
    filter.finish_into(&mut filtered);
    if !filtered.is_empty() {
        let _ = writer.write_all(&filtered);
    }
    let _ = writer.flush();
}

fn spawn_output_pumps(child: &mut Child) -> (thread::JoinHandle<()>, thread::JoinHandle<()>) {
    let stdout = child
        .stdout
        .take()
        .expect("child stdout should be piped for output forwarding");
    let stderr = child
        .stderr
        .take()
        .expect("child stderr should be piped for output forwarding");

    let stdout_handle = thread::spawn(move || pump_filtered_output(stdout, io::stdout()));
    let stderr_handle = thread::spawn(move || pump_filtered_output(stderr, io::stderr()));

    (stdout_handle, stderr_handle)
}

fn run_uefi_test_with_backtrace(
    mut cmd: Command,
    label: &str,
    gdb_socket: &str,
    timeout_secs: u64,
) -> i32 {
    eprintln!("Running (UEFI, with gdb-on-timeout): {:?}", cmd);

    let mut child = spawn_in_own_pgrp(&mut cmd)
        .unwrap_or_else(|e| panic!("Failed to spawn cargo test (UEFI) for {}: {}", label, e));
    let (stdout_pump, stderr_pump) = spawn_output_pumps(&mut child);

    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    // Poll for completion with timeout.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    break Err(());
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                eprintln!(
                    "Error: failed to poll UEFI test process for {}: {}",
                    label, e
                );
                break Err(());
            }
        }
    };

    let exit_code = match status {
        Ok(status) => {
            if status.success() {
                0
            } else {
                status.code().unwrap_or(1)
            }
        }
        Err(()) => {
            eprintln!(
                "Error: UEFI test '{}' did not finish within {}s; assuming hang.",
                label, timeout_secs
            );
            eprintln!(
                "Attempting to capture guest state via gdb (socket: {})...",
                gdb_socket
            );

            if let Some(gdb) = resolve_gdb_executable() {
                let mut gdb_cmd = Command::new(&gdb);
                gdb_cmd
                    .arg("-q")
                    .arg("-ex")
                    .arg("set pagination off")
                    .arg("-ex")
                    .arg(format!("target remote {}", gdb_socket))
                    .arg("-ex")
                    .arg("set confirm off")
                    .arg("-ex")
                    .arg("interrupt")
                    .arg("-ex")
                    .arg("info registers")
                    .arg("-ex")
                    .arg("bt")
                    .arg("-ex")
                    .arg("x/16i $pc")
                    .arg("-ex")
                    .arg("quit")
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
                eprintln!("Running gdb: {:?}", gdb_cmd);
                match gdb_cmd.status() {
                    Ok(s) => {
                        eprintln!("gdb finished with status: {:?}", s);
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to execute gdb for {}: {}", label, e);
                    }
                }
            } else {
                eprintln!(
                    "Warning: no suitable gdb executable found in PATH; skip backtrace dump."
                );
            }

            eprintln!("Killing hung UEFI test process for '{}'", label);
            kill_process_tree_best_effort(label, &mut child);

            // 124 = timeout and consistent with `timeout` command conventions.
            124
        }
    };
    let _ = stdout_pump.join();
    let _ = stderr_pump.join();

    exit_code
}

fn run_guest_test_with_timeout(
    mut cmd: Command,
    label: &str,
    timeout_secs: u64,
    kind: &str,
) -> i32 {
    eprintln!("Running ({} with internal timeout): {:?}", kind, cmd);

    let mut child = spawn_in_own_pgrp(&mut cmd)
        .unwrap_or_else(|e| panic!("Failed to spawn cargo test ({}) for {}: {}", kind, label, e));
    let (stdout_pump, stderr_pump) = spawn_output_pumps(&mut child);

    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    break Err(());
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                eprintln!(
                    "Error: failed to poll {} process for {}: {}",
                    kind, label, e
                );
                break Err(());
            }
        }
    };

    let exit_code = match status {
        Ok(status) => {
            if status.success() {
                0
            } else {
                status.code().unwrap_or(1)
            }
        }
        Err(()) => {
            eprintln!(
                "Error: {} '{}' did not finish within {}s; assuming hang.",
                kind, label, timeout_secs
            );
            eprintln!("Killing hung {} process for '{}'", kind, label);
            kill_process_tree_best_effort(label, &mut child);
            124
        }
    };
    let _ = stdout_pump.join();
    let _ = stderr_pump.join();

    exit_code
}

/// Owns a fresh per-test Cargo target directory and removes it on scope exit.
struct UbootTargetDir(PathBuf);

impl UbootTargetDir {
    fn new(label: &str) -> Self {
        let label = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();
        let path = std::env::temp_dir()
            .join("aarch64_hv_uboot_tests")
            .join(format!("{}_{}", label, std::process::id()));
        // Discard artifacts left by an interrupted run before Cargo sees the path.
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for UbootTargetDir {
    fn drop(&mut self) {
        // Cleanup must not replace the test result with a filesystem error.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parse_xtest_cli_args(
    args: &[String],
) -> Result<(Vec<String>, Vec<String>, Vec<String>, bool), String> {
    let mut forward_args = Vec::new();
    let mut package_filters = Vec::new();
    let mut testname_filters = Vec::new();
    let mut help_requested = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            forward_args.extend(args[i..].iter().cloned());
            break;
        } else if arg == "-h" || arg == "--help" {
            help_requested = true;
            i += 1;
            continue;
        } else if let Some(pkg) = arg.strip_prefix("--package=") {
            package_filters.push(pkg.to_string());
            i += 1;
            continue;
        } else if arg == "-p" || arg == "--package" {
            match args.get(i + 1) {
                Some(pkg) if pkg != "--" => {
                    package_filters.push(pkg.clone());
                    i += 2;
                    continue;
                }
                _ => return Err("Error: -p/--package requires a value".to_string()),
            }
        } else if arg.starts_with("-p") && arg.len() > 2 {
            package_filters.push(arg[2..].to_string());
            i += 1;
            continue;
        } else if arg == "-t" {
            match args.get(i + 1) {
                Some(name) if name != "--" => {
                    testname_filters.push(name.clone());
                    i += 2;
                    continue;
                }
                _ => return Err("Error: -t requires a value".to_string()),
            }
        } else if arg.starts_with("-t") && arg.len() > 2 {
            testname_filters.push(arg[2..].to_string());
            i += 1;
            continue;
        }

        forward_args.push(arg.clone());
        i += 1;
    }

    Ok((
        forward_args,
        package_filters,
        testname_filters,
        help_requested,
    ))
}

fn apply_testname_filters(
    std_crates: &mut Vec<(String, Vec<String>)>,
    unit_crates: &mut Vec<(String, Vec<String>)>,
    uefi_tests: &mut Vec<(String, String, String, Vec<String>)>,
    uboot_tests: &mut Vec<(String, String, String, Vec<String>)>,
    uboot_unit_tests: &mut Vec<(String, String, Vec<String>)>,
    testname_filters: &[String],
) {
    if testname_filters.is_empty() {
        return;
    }

    let keep_std = testname_filters.iter().any(|t| t == "std");
    let keep_unit = testname_filters.iter().any(|t| t == "unit");
    let keep_uboot_unit = testname_filters.iter().any(|t| t == "uboot-unit");

    if !keep_std {
        std_crates.clear();
    }
    if !keep_unit {
        unit_crates.clear();
    }

    uefi_tests.retain(|(_, testname, _, _)| testname_filters.contains(testname));
    uboot_tests.retain(|(_, testname, _, _)| testname_filters.contains(testname));

    if !keep_uboot_unit {
        uboot_unit_tests.clear();
    }
}

fn print_xtest_usage() {
    println!("Usage: cargo xtask test [options] [-- <cargo test args...>]");
    println!();
    println!("Options (repeatable):");
    println!("  -p, --package <pkg>   Filter xtest.txt by package");
    println!("  --package=<pkg>       Filter xtest.txt by package");
    println!("  -p<pkg>               Shorthand for -p <pkg>");
    println!("  -t <name>             Filter by UEFI/U-Boot test name");
    println!("  -t<name>              Shorthand for -t <name>");
    println!("  -t std                Include host std tests");
    println!("  -t unit               Include host unit tests");
    println!("  -t uboot-unit         Include U-Boot unit tests");
    println!("  -h, --help            Show this help");
}

fn test(args: &[String]) {
    let (test_args, package_filters, testname_filters, help_requested) =
        match parse_xtest_cli_args(args) {
            Ok(parsed) => parsed,
            Err(msg) => {
                eprintln!("{}", msg);
                std::process::exit(1);
            }
        };

    if help_requested {
        print_xtest_usage();
        return;
    }

    // Detect host triple
    let host_output = Command::new("rustc")
        .arg("--print")
        .arg("host-tuple")
        .output()
        .expect("Failed to run rustc --print host-tuple");
    let host_tuple = String::from_utf8(host_output.stdout)
        .expect("Invalid UTF-8 from rustc --print host-tuple")
        .trim()
        .to_string();

    eprintln!("Detected host target: {}", host_tuple);

    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../");
    let plan_path = repo_root.join("xtest.txt");
    let plan = std::fs::read_to_string(&plan_path).ok();

    let mut std_crates: Vec<(String, Vec<String>)> = Vec::new();
    let mut unit_crates: Vec<(String, Vec<String>)> = Vec::new();
    let mut uefi_tests: Vec<(String, String, String, Vec<String>)> = Vec::new();
    let mut uboot_tests: Vec<(String, String, String, Vec<String>)> = Vec::new();
    let mut uboot_unit_tests: Vec<(String, String, Vec<String>)> = Vec::new();

    let plan_text = plan.expect("require xtest.txt");
    for (lineno, line) in plan_text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("std") => {
                if let Some(pkg) = parts.next() {
                    let extra: Vec<String> = parts.map(|s| s.to_string()).collect();
                    std_crates.push((pkg.to_string(), extra));
                } else {
                    eprintln!("xtest.txt:{}: missing package after 'std'", lineno + 1);
                }
            }
            Some("unit") => {
                if let Some(pkg) = parts.next() {
                    let extra: Vec<String> = parts.map(|s| s.to_string()).collect();
                    unit_crates.push((pkg.to_string(), extra));
                } else {
                    eprintln!("xtest.txt:{}: missing package after 'unit'", lineno + 1);
                }
            }
            Some("uefi") => {
                let (pkg, testname, testscript) = (parts.next(), parts.next(), parts.next());
                match (pkg, testname, testscript) {
                    (Some(p), Some(t), Some(s)) => {
                        let extra: Vec<String> = parts.map(|s| s.to_string()).collect();
                        uefi_tests.push((p.to_string(), t.to_string(), s.to_string(), extra))
                    }
                    _ => eprintln!(
                        "xtest.txt:{}: expected: uefi <package> <testname> <testscript>",
                        lineno + 1
                    ),
                }
            }
            Some("uboot") | Some("u-boot") => {
                let (pkg, testname, testscript) = (parts.next(), parts.next(), parts.next());
                match (pkg, testname, testscript) {
                    (Some(p), Some(t), Some(s)) => {
                        let extra: Vec<String> = parts.map(|s| s.to_string()).collect();
                        uboot_tests.push((p.to_string(), t.to_string(), s.to_string(), extra))
                    }
                    _ => eprintln!(
                        "xtest.txt:{}: expected: uboot <package> <testname> <testscript> [extra...]",
                        lineno + 1
                    ),
                }
            }
            Some("uboot-unit") | Some("u-boot-unit") | Some("uboot_unit") | Some("u_boot_unit") => {
                let (pkg, testscript) = (parts.next(), parts.next());
                match (pkg, testscript) {
                    (Some(p), Some(s)) => {
                        let extra: Vec<String> = parts.map(|s| s.to_string()).collect();
                        uboot_unit_tests.push((p.to_string(), s.to_string(), extra))
                    }
                    _ => eprintln!(
                        "xtest.txt:{}: expected: uboot-unit <package> <testscript> [extra...]",
                        lineno + 1
                    ),
                }
            }
            Some(other) => {
                eprintln!(
                    "xtest.txt:{}: unknown kind '{}'; expected 'std', 'unit', 'uefi', 'uboot', or 'uboot-unit'",
                    lineno + 1,
                    other
                );
            }
            None => {}
        }
    }

    // Helper: build 'timeout' wrapper if available
    fn timeout_prefix(secs: u64) -> Option<Vec<String>> {
        // Detect availability
        let out = Command::new("timeout").arg("--help").output();
        if let Ok(o) = out {
            let help = String::from_utf8_lossy(&o.stdout);
            if help.contains("--foreground") {
                return Some(vec![
                    "timeout".into(),
                    "--foreground".into(),
                    "-k".into(),
                    "5s".into(),
                    format!("{}s", secs),
                ]);
            } else {
                return Some(vec!["timeout".into(), format!("{}", secs)]);
            }
        }
        None
    }

    fn scripts_need_sudo(
        tests: &[(String, String, String, Vec<String>)],
        repo_root: &PathBuf,
        kind: &str,
    ) -> bool {
        for (_, _, testscript, _) in tests {
            let runner_path = repo_root.join(testscript);
            match fs::read_to_string(&runner_path) {
                Ok(content) => {
                    if content.contains("sudo") {
                        eprintln!(
                            "Detected use of sudo in {} runner script: {}",
                            kind,
                            runner_path.display()
                        );
                        return true;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Warning: failed to read {} runner script {}: {}",
                        kind,
                        runner_path.display(),
                        e
                    );
                }
            }
        }
        false
    }

    fn sudo_warmup() {
        let sudo_check = Command::new("sudo")
            .arg("-V")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match sudo_check {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                eprintln!("Warning: 'sudo' not found; tests that require sudo may fail.");
                return;
            }
            Err(e) => {
                eprintln!("Warning: failed to check availability of 'sudo': {}", e);
                return;
            }
            Ok(_) => {}
        }

        match Command::new("sudo")
            .arg("-n")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {
                eprintln!("Reusing existing sudo credential cache.");
                return;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Warning: failed to check sudo credential cache: {}", e);
            }
        }

        eprintln!(
            "Running 'sudo -v' to warm up credentials (you may be prompted for your password)..."
        );
        let status = Command::new("sudo")
            .arg("-v")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .unwrap_or_else(|e| panic!("Failed to execute 'sudo -v': {}", e));

        if !status.success() {
            let code = status.code().unwrap_or(1);
            eprintln!(
                "Error: 'sudo -v' failed (code {}); tests that require sudo cannot be run.",
                code
            );
            std::process::exit(code);
        }
    }

    if !package_filters.is_empty() {
        std_crates.retain(|(pkg, _)| package_filters.contains(pkg));
        unit_crates.retain(|(pkg, _)| package_filters.contains(pkg));
        uefi_tests.retain(|(pkg, _, _, _)| package_filters.contains(pkg));
        uboot_tests.retain(|(pkg, _, _, _)| package_filters.contains(pkg));
        uboot_unit_tests.retain(|(pkg, _, _)| package_filters.contains(pkg));

        if std_crates.is_empty()
            && unit_crates.is_empty()
            && uefi_tests.is_empty()
            && uboot_tests.is_empty()
            && uboot_unit_tests.is_empty()
        {
            eprintln!(
                "No entries in xtest.txt match specified packages: {:?}",
                package_filters
            );
            std::process::exit(1);
        }

        eprintln!(
            "Filtered test plan to packages: {:?} ({} std, {} unit, {} uefi, {} uboot, {} uboot-unit)",
            package_filters,
            std_crates.len(),
            unit_crates.len(),
            uefi_tests.len(),
            uboot_tests.len(),
            uboot_unit_tests.len()
        );
    }

    if !testname_filters.is_empty() {
        apply_testname_filters(
            &mut std_crates,
            &mut unit_crates,
            &mut uefi_tests,
            &mut uboot_tests,
            &mut uboot_unit_tests,
            &testname_filters,
        );

        if std_crates.is_empty()
            && unit_crates.is_empty()
            && uefi_tests.is_empty()
            && uboot_tests.is_empty()
            && uboot_unit_tests.is_empty()
        {
            eprintln!(
                "No entries in xtest.txt match specified test names: {:?}",
                testname_filters
            );
            std::process::exit(1);
        }

        eprintln!(
            "Filtered test plan to test names: {:?} ({} std, {} unit, {} uefi, {} uboot, {} uboot-unit)",
            testname_filters,
            std_crates.len(),
            unit_crates.len(),
            uefi_tests.len(),
            uboot_tests.len(),
            uboot_unit_tests.len()
        );
    }

    // Accumulate results across all tests
    let mut passed: Vec<String> = Vec::new();
    let mut failed: Vec<(String, i32)> = Vec::new();

    let uboot_unit_for_sudo: Vec<(String, String, String, Vec<String>)> = uboot_unit_tests
        .iter()
        .map(|(p, s, extra)| (p.clone(), String::new(), s.clone(), extra.clone()))
        .collect();

    if scripts_need_sudo(&uefi_tests, &repo_root, "UEFI")
        || scripts_need_sudo(&uboot_tests, &repo_root, "U-Boot")
        || scripts_need_sudo(&uboot_unit_for_sudo, &repo_root, "U-Boot unit")
    {
        sudo_warmup();
    }

    // Empty test names mark unit-plan entries in the shared U-Boot runner.
    uboot_tests.extend(uboot_unit_for_sudo);

    // Run std tests (each with 30s timeout if available)
    for (pkg, extra) in std_crates {
        eprintln!("\n--- Running host tests for: {} ---", pkg);
        let mut cmd = if let Some(mut prefix) = timeout_prefix(30) {
            let mut c = Command::new(prefix.remove(0));
            for p in prefix {
                c.arg(p);
            }
            c.arg("cargo");
            c.arg("test");
            c
        } else {
            let mut c = Command::new("cargo");
            c.arg("test");
            c
        };

        cmd.arg("--target")
            .arg(&host_tuple)
            .arg("-p")
            .arg(&pkg)
            .args(&extra)
            .args(&test_args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        eprintln!("Running: {:?}", cmd);
        let status = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to spawn cargo test for {}: {}", pkg, e))
            .wait()
            .unwrap_or_else(|e| panic!("Failed to wait for cargo test for {}: {}", pkg, e));
        if status.success() {
            passed.push(format!("std:{}", pkg));
        } else {
            let code = status.code().unwrap_or(1);
            eprintln!("Error: Tests failed for package: {} (code {})", pkg, code);
            failed.push((format!("std:{}", pkg), code));
        }
    }

    // Run host unit tests explicitly (lib-only) with 30s timeout if available.
    for (pkg, extra) in unit_crates {
        eprintln!("\n--- Running host unit tests for: {} ---", pkg);
        let mut cmd = if let Some(mut prefix) = timeout_prefix(30) {
            let mut c = Command::new(prefix.remove(0));
            for p in prefix {
                c.arg(p);
            }
            c.arg("cargo");
            c.arg("test");
            c
        } else {
            let mut c = Command::new("cargo");
            c.arg("test");
            c
        };

        cmd.arg("--target")
            .arg(&host_tuple)
            .arg("-p")
            .arg(&pkg)
            .arg("--lib")
            .args(&extra)
            .args(&test_args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        eprintln!("Running: {:?}", cmd);
        let status = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to spawn cargo test for {}: {}", pkg, e))
            .wait()
            .unwrap_or_else(|e| panic!("Failed to wait for cargo test for {}: {}", pkg, e));
        if status.success() {
            passed.push(format!("unit:{}", pkg));
        } else {
            let code = status.code().unwrap_or(1);
            eprintln!(
                "Error: Unit tests failed for package: {} (code {})",
                pkg, code
            );
            failed.push((format!("unit:{}", pkg), code));
        }
    }

    // Decide whether to enable gdb-on-timeout for UEFI tests.
    let enable_uefi_backtrace = std::env::var("XTASK_UEFI_GDB_DUMP_ON_TIMEOUT")
        .map(|v| {
            let v = v.to_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or_else(|_| std::env::var("CI").is_ok());

    // Run UEFI tests
    for (pkg, testname, testscript, extra) in uefi_tests {
        let runner_path = repo_root.join(testscript);
        let runner = runner_path
            .to_str()
            .expect("runner path contains invalid UTF-8");

        let label = format!("uefi:{}::{}", pkg, testname);
        eprintln!(
            "\n--- Running UEFI test for: {}::{}, runner: {} ---",
            pkg, testname, runner
        );

        // Prepare gdbstub socket path when backtrace dump is enabled.
        let gdb_socket = if enable_uefi_backtrace {
            Some(format!(
                "/tmp/aarch64_hv_qemu_gdb_{}_{}.sock",
                pkg.replace('/', "_"),
                testname.replace('/', "_")
            ))
        } else {
            None
        };

        let mut cmd = {
            let mut c = Command::new("cargo");
            c.arg("test");
            c
        };
        cmd.arg("--target")
            .arg("aarch64-unknown-uefi")
            .arg("-p")
            .arg(&pkg)
            .arg("--test")
            .arg(&testname)
            .args(&extra)
            .args(&test_args)
            .env("CARGO_TARGET_AARCH64_UNKNOWN_UEFI_RUNNER", runner)
            .env("CARGO_TERM_COLOR", "always")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref socket) = gdb_socket {
            cmd.env("XTASK_QEMU_GDB_SOCKET", socket);
        }

        let code = if let Some(ref socket) = gdb_socket {
            run_uefi_test_with_backtrace(cmd, &label, socket, 120)
        } else {
            run_guest_test_with_timeout(cmd, &label, 120, "UEFI test")
        };

        if code == 0 {
            passed.push(label);
        } else {
            eprintln!("Error: UEFI test failed for {} with code {}", pkg, code);
            failed.push((label, code));
        }
    }

    // Run U-Boot tests
    for (pkg, testname, testscript, extra) in uboot_tests {
        let runner_path = repo_root.join(testscript);
        let runner = runner_path
            .to_str()
            .expect("runner path contains invalid UTF-8");

        let test_lds_path = repo_root.join("test.lds");
        let test_lds = test_lds_path
            .to_str()
            .expect("test linker path contains invalid UTF-8")
            .to_string();

        let label = if testname.is_empty() {
            format!("uboot-unit:{}", pkg)
        } else {
            format!("uboot:{}::{}", pkg, testname)
        };
        eprintln!(
            "\n--- Running U-Boot test for: {} (runner: {}) ---",
            label, runner
        );

        let mut cmd = Command::new("cargo");

        // Give each U-Boot test its own fresh target dir to avoid mixing build-std
        // artifacts (duplicate core lang items).
        let target_dir = UbootTargetDir::new(&label);

        let rustflags = {
            let mut parts = Vec::new();
            if let Ok(existing) = std::env::var("RUSTFLAGS") {
                parts.push(existing);
            }
            parts.push("-C panic=abort -Zpanic_abort_tests".to_string());
            if !testname.is_empty() {
                parts.push("-C debuginfo=0".to_string());
            }
            parts.push("-C relocation-model=static".to_string());
            parts.push(format!("-C link-arg=-T{}", test_lds));
            parts.join(" ")
        };

        cmd.arg("test")
            .arg("--target")
            .arg("aarch64-unknown-none-softfloat")
            .arg("-p")
            .arg(&pkg);

        if !testname.is_empty() {
            cmd.arg("--test").arg(&testname);
        } else {
            let has_explicit_target = extra.iter().any(|arg| {
                arg == "--bin"
                    || arg == "--test"
                    || arg == "--example"
                    || arg == "--bench"
                    || arg.starts_with("--bin=")
                    || arg.starts_with("--test=")
                    || arg.starts_with("--example=")
                    || arg.starts_with("--bench=")
            });
            if !has_explicit_target {
                cmd.arg("--lib");
            }
        }

        cmd.args(&extra)
            .args(&test_args)
            .env("CARGO_TARGET_AARCH64_UNKNOWN_NONE_SOFTFLOAT_RUNNER", runner)
            .env("RUSTFLAGS", rustflags)
            .env("CARGO_PROFILE_TEST_PANIC", "abort")
            .env("CARGO_PROFILE_DEV_PANIC", "abort")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TARGET_DIR", &target_dir.0)
            .env("CARGO_TERM_COLOR", "always")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let code = run_guest_test_with_timeout(cmd, &label, 300, "U-Boot test");
        if code == 0 {
            passed.push(label);
        } else {
            eprintln!("Error: U-Boot test failed for {} with code {}", pkg, code);
            failed.push((label, code));
        }
    }

    // Summary
    eprintln!("\n===== Test Summary =====");
    if !passed.is_empty() {
        eprintln!("Passed ({}):", passed.len());
        for p in &passed {
            eprintln!("  - {}", p);
        }
    } else {
        eprintln!("Passed: 0");
    }
    if !failed.is_empty() {
        eprintln!("Failed ({}):", failed.len());
        for (f, code) in &failed {
            eprintln!("  - {} (code {})", f, code);
        }
        std::process::exit(1);
    } else {
        eprintln!("All tests passed (host std + host unit + UEFI + U-Boot + U-Boot unit)");
    }
}

#[cfg(test)]
mod tests {
    use super::AnsiQueryFilter;
    use super::UbootTargetDir;
    use super::apply_testname_filters;
    use super::decoded_vmx_mnemonic;
    use super::parse_xtest_cli_args;
    #[cfg(unix)]
    use super::run_guest_test_with_timeout;
    use std::fs;
    use std::io;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::process::Stdio;
    #[cfg(unix)]
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    fn filter_chunks(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = AnsiQueryFilter::new();
        let mut out = Vec::new();
        for chunk in chunks {
            filter.push_filtered(chunk, &mut out);
        }
        filter.finish_into(&mut out);
        out
    }

    fn to_args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn uboot_target_dir_starts_fresh_and_cleans_up_on_error() {
        let label = format!("cleanup_test_{}", std::process::id());
        let initial = UbootTargetDir::new(&label);
        let path = initial.0.clone();
        fs::create_dir_all(&path).expect("target dir should be created");
        fs::write(path.join("complete"), b"complete").expect("artifact should be created");
        drop(initial);
        assert!(!path.exists());

        fs::create_dir_all(&path).expect("stale target dir should be created");
        fs::write(path.join("stale"), b"stale").expect("stale artifact should be created");

        let result: io::Result<()> = (|| {
            let target_dir = UbootTargetDir::new(&label);
            assert!(!target_dir.0.join("stale").exists());
            fs::create_dir_all(&target_dir.0)?;
            fs::write(target_dir.0.join("current"), b"current")?;
            Err(io::Error::other("test error"))
        })();

        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn strips_cpr_sequence() {
        let out = filter_chunks(&[b"abc\x1b[6nxyz"]);
        assert_eq!(out, b"abcxyz");
    }

    #[test]
    fn strips_status_report_query() {
        let out = filter_chunks(&[b"\x1b[5n"]);
        assert_eq!(out, b"");
    }

    #[test]
    fn strips_device_attributes_query() {
        let out = filter_chunks(&[b"\x1b[0c"]);
        assert_eq!(out, b"");
    }

    #[test]
    fn strips_decid_sequence() {
        let out = filter_chunks(&[b"\x1bZ"]);
        assert_eq!(out, b"");
    }

    #[test]
    fn strips_private_cpr_sequence() {
        let out = filter_chunks(&[b"before\x1b[?6nafter"]);
        assert_eq!(out, b"beforeafter");
    }

    #[test]
    fn preserves_sgr_sequences() {
        let payload = b"\x1b[31mred\x1b[0m";
        let out = filter_chunks(&[payload]);
        assert_eq!(out, payload);
    }

    #[test]
    fn preserves_cursor_positioning_sequences() {
        let payload = b"\x1b[10;20Hmove";
        let out = filter_chunks(&[payload]);
        assert_eq!(out, payload);
    }

    #[test]
    fn preserves_non_query_csi_n_sequences() {
        let payload = b"\x1b[42n";
        let out = filter_chunks(&[payload]);
        assert_eq!(out, payload);
    }

    #[test]
    fn handles_chunk_boundaries() {
        let out = filter_chunks(&[b"abc\x1b[", b"6nxyz"]);
        assert_eq!(out, b"abcxyz");
    }

    #[test]
    fn parses_package_testname_and_forward_args() {
        let args = to_args(&[
            "-p",
            "gdb_remote",
            "-t",
            "uefi_packet_size",
            "--",
            "--nocapture",
        ]);
        let (forward_args, package_filters, testname_filters, help_requested) =
            parse_xtest_cli_args(&args).expect("parse should succeed");

        assert_eq!(package_filters, vec!["gdb_remote"]);
        assert_eq!(testname_filters, vec!["uefi_packet_size"]);
        assert_eq!(forward_args, vec!["--", "--nocapture"]);
        assert!(!help_requested);
    }

    #[test]
    fn parses_compact_testname_flag() {
        let args = to_args(&["-tgicv2_pending"]);
        let (_, _, testname_filters, help_requested) =
            parse_xtest_cli_args(&args).expect("parse should succeed");
        assert_eq!(testname_filters, vec!["gicv2_pending"]);
        assert!(!help_requested);
    }

    #[test]
    fn rejects_missing_testname() {
        let args = to_args(&["-t"]);
        let err = parse_xtest_cli_args(&args).expect_err("missing -t value should error");
        assert!(err.contains("-t"));
    }

    #[test]
    fn parses_help_flag() {
        let args = to_args(&["--help"]);
        let (forward_args, package_filters, testname_filters, help_requested) =
            parse_xtest_cli_args(&args).expect("parse should succeed");
        assert!(help_requested);
        assert!(forward_args.is_empty());
        assert!(package_filters.is_empty());
        assert!(testname_filters.is_empty());
    }

    #[test]
    fn testname_filter_keeps_only_matching_uefi() {
        let mut std_crates = vec![("gdb_remote".to_string(), Vec::new())];
        let mut unit_crates = vec![("gdb_remote".to_string(), Vec::new())];
        let mut uefi_tests = vec![
            (
                "gdb_remote".to_string(),
                "uefi_packet_size".to_string(),
                "gdb_remote/scripts/run_uefi_gdb_remote_qxfer_test.sh".to_string(),
                Vec::new(),
            ),
            (
                "gdb_remote".to_string(),
                "uefi_qxfer_features".to_string(),
                "gdb_remote/scripts/run_uefi_gdb_remote_qxfer_test.sh".to_string(),
                Vec::new(),
            ),
        ];
        let mut uboot_tests = vec![(
            "gdb_remote".to_string(),
            "gicv2_pending".to_string(),
            "arch_hal/aarch64_hal/gic/scripts/run_gicv2_pending_test.sh".to_string(),
            Vec::new(),
        )];
        let mut uboot_unit_tests = vec![(
            "gdb_remote".to_string(),
            "arch_hal/aarch64_hal/gic/scripts/run_gicv2_pending_test.sh".to_string(),
            Vec::new(),
        )];

        let testname_filters = vec!["uefi_packet_size".to_string()];
        apply_testname_filters(
            &mut std_crates,
            &mut unit_crates,
            &mut uefi_tests,
            &mut uboot_tests,
            &mut uboot_unit_tests,
            &testname_filters,
        );

        assert!(std_crates.is_empty());
        assert!(unit_crates.is_empty());
        assert!(uboot_tests.is_empty());
        assert!(uboot_unit_tests.is_empty());
        assert_eq!(uefi_tests.len(), 1);
        assert_eq!(uefi_tests[0].1, "uefi_packet_size");
    }

    #[test]
    fn detects_only_decoded_vmx_mnemonics() {
        assert_eq!(
            decoded_vmx_mnemonic("1000:\t0f 01 c1\tvmcall"),
            Some("vmcall")
        );
        assert_eq!(
            decoded_vmx_mnemonic("1003:\t0f 78 c1\tvmreadq %rax,%rcx"),
            Some("vmreadq")
        );
        assert_eq!(decoded_vmx_mnemonic("1000 <nested_vmx::vmxon>:"), None);
    }

    #[test]
    fn monitor_isa_rejects_unpreserved_extended_state() {
        assert!(super::unpreserved_monitor_instruction(
            "1000 <vmovdqu>:\n 1000: fxsave64 %gs:0\n 1008: movaps %xmm15,%xmm0\n 1010: vmlaunch\n 1013: fxrstor64 %gs:0\n"
        ).is_none());
        for instruction in [
            "vzeroupper",
            "vpxor",
            "vmovaps",
            "kmovw",
            "tilezero",
            "ldtilecfg",
            "xsave64",
            "xrstors",
            "wrpkru",
            "(bad)",
        ] {
            let decoded = format!(" 1000: {instruction} %xmm0,%xmm0\n");
            assert_eq!(
                super::unpreserved_monitor_instruction(&decoded),
                Some(instruction)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn x86_backend_gate_rejects_missing_mixed_or_resident_provenance() {
        use std::io::Write;

        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-uefi-smoke.sh");
        let check = |backend: &str, log: &str| {
            let mut child = Command::new("bash")
                .arg(&runner)
                .args(["--check-backend-log", backend, "/dev/stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let written = child.stdin.take().unwrap().write_all(log.as_bytes());
            let output = child.wait_with_output().unwrap();
            if !output.status.success() {
                eprintln!("backend gate: {}", String::from_utf8_lossy(&output.stderr));
            }
            written.is_ok() && output.status.success()
        };
        let markers = [
            ("direct-vmx", "thin-hv: backend=direct-vmx role=project-l0"),
            ("outer-kvm", "thin-hv: backend=outer-kvm role=reference"),
            (
                "physical-chainload",
                "thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0",
            ),
            (
                "physical-preflight",
                "thin-hv: backend=physical-preflight project_vmx=0",
            ),
        ];
        for (backend, marker) in markers {
            assert!(
                check(backend, marker),
                "unterminated final marker: {backend}"
            );
            assert!(check(backend, &format!("{marker}\r\n{marker}\r\n")));
            assert!(!check(backend, "thin-hv: uefi entry\n"));
            assert!(!check(backend, &format!("{marker} truncated\n")));
            for (other, other_marker) in markers {
                if backend != other {
                    assert!(!check(backend, &format!("{other_marker}\n")));
                    assert!(!check(backend, &format!("{marker}\n{other_marker}\n")));
                }
            }
            if backend != "direct-vmx" {
                for forbidden in [
                    "thin-hv: runtime monitor active",
                    "thin-hv: loading runtime monitor",
                    "thin-hv: variable overlay profile=1",
                    "thin-hv: uefi variable overlay PASS",
                    "thin-hv: L1 VMLAUNCH",
                    "thin-hv: vmx guest PASS",
                ] {
                    assert!(!check(backend, &format!("{marker}\n{forbidden}\n")));
                }
            }
        }
        assert!(!check("unknown", markers[0].1));
        assert!(!check(
            "physical-preflight",
            &format!("{}\nthin-hv: guest uefi payload\n", markers[3].1)
        ));
        let direct_boot = format!("{0}\n{0}\nthin-hv: runtime monitor active\n", markers[0].1);
        let guest_success =
            "thin-hv: windows desktop\nthin-hv: windows hyperv PASS\nthin-hv: vmx guest PASS\n";
        assert!(check(
            "direct-vmx",
            &format!("{direct_boot}{guest_success}")
        ));
        for failure in [
            "thin-hv: vmx smoke FAIL: FeatureControlLocked",
            "thin-hv: vmx guest FAIL marker=0x0",
            "thin-hv: vmx guest FAIL: unsupported exit",
            "thin-hv: VMRESUME FAIL status=FailValid",
            "thin-hv: VMXOFF status=Success",
            "thin-hv: panic",
            "thin-hv: CPUID VMX=0",
            "thin-hv: IA32_FEATURE_CONTROL=unavailable",
            "thin-hv: IA32_VMX_BASIC=unavailable",
        ] {
            // Firmware may boot the OS after an unsuccessful monitor returns.
            assert!(!check(
                "direct-vmx",
                &format!("{direct_boot}{failure}\n{guest_success}")
            ));
            assert!(!check(
                "direct-vmx",
                &format!("{direct_boot}{guest_success}{failure}")
            ));
            assert!(!check(
                "direct-vmx",
                &format!("{failure}\r\n{direct_boot}{guest_success}")
            ));
        }
        // Architectural VMfailValid results and physical fixture negatives are
        // not terminal project-monitor failures and must not be over-matched.
        assert!(check(
            "direct-vmx",
            &format!("{direct_boot}thin-hv: L1 VMCLEAR status=FailValid\n{guest_success}")
        ));
        assert!(check(
            "physical-chainload",
            &format!(
                "{}\nthin-hv: physical chainload FAIL: LoadImage status=0x800000000000000e\n",
                markers[2].1
            )
        ));
    }

    #[cfg(unix)]
    #[test]
    fn host_exception_gate_requires_only_the_ordered_expected_root_fault() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-host-exception-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-uefi-smoke.sh");
        let check = |status: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-host-exception-log", status])
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let backend = "thin-hv: backend=direct-vmx role=project-l0\n";
        let required = [
            "thin-hv: runtime monitor active\n",
            "thin-hv: private host state PASS\n",
            "thin-hv: guest uefi payload\n",
            "thin-hv: uefi variable overlay PASS\n",
            "thin-hv: host exception test guest returned\n",
            "thin-hv: host exception test armed\n",
            "thin-hv: host exception FAIL: stopped\n",
        ];
        let valid = format!("{backend}{backend}{}", required.concat());
        assert!(check("124", &valid));
        assert!(check("124", &valid.replace('\n', "\r\n")));
        assert!(check("124", valid.trim_end_matches('\n')));
        for status in ["0", "1", "137", "143", "0124", ""] {
            assert!(!check(status, &valid), "accepted QEMU status {status}");
        }
        assert!(!check("124", ""));
        assert!(!check("124", &valid.replacen(backend, "", 1)));
        assert!(!check("124", &format!("{backend}{valid}")));
        assert!(!check(
            "124",
            &valid.replace(backend, "thin-hv: backend=outer-kvm role=reference\n")
        ));
        for required_line in required {
            assert!(!check("124", &valid.replace(required_line, "")));
            assert!(!check(
                "124",
                &valid.replace(required_line, &format!("{required_line}{required_line}"))
            ));
            assert!(!check(
                "124",
                &valid.replace(required_line, &format!("prefix {required_line}"))
            ));
            assert!(!check(
                "124",
                &valid.replace(required_line, &required_line.replace('\n', " suffix\n"))
            ));
        }
        assert!(!check(
            "124",
            &valid.replace(
                "thin-hv: host exception test guest returned\nthin-hv: host exception test armed\n",
                "thin-hv: host exception test armed\nthin-hv: host exception test guest returned\n"
            )
        ));
        assert!(!check(
            "124",
            &valid.replace("exception test armed", "exception\0 test armed")
        ));
        assert!(!check("124", &valid.replace("stopped\n", "stopped\r\r\n")));
        assert!(!check(
            "124",
            &format!("{}{valid}", "unrelated\n".repeat(32768))
        ));
        for forbidden in [
            "thin-hv: vmx smoke FAIL: test\n",
            "thin-hv: vmx guest FAIL\n",
            "thin-hv: VMRESUME FAIL\n",
            "thin-hv: VMXOFF status=Success\n",
            "thin-hv: panic\n",
            "thin-hv: vmx guest PASS start_image_status=0x0000000000000000\n",
            "thin-hv: backend=physical-preflight project_vmx=0\n",
            "thin-hv: uefi native variables PASS\n",
        ] {
            assert!(!check("124", &format!("{valid}{forbidden}")));
            assert!(!check("124", &format!("{forbidden}{valid}")));
        }
        fs::write(&log.0, &valid).unwrap();
        let ordinary = Command::new("bash")
            .arg(&runner)
            .args(["--check-backend-log", "direct-vmx"])
            .arg(&log.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !ordinary.success(),
            "ordinary Direct-VMX must reject the expected-fault transcript"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preflight_ept_gate_distinguishes_construction_from_capability_skip() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-preflight-ept-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-uefi-smoke.sh");
        let check = |accel: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-preflight-ept-log", accel])
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let vmx_absent = "thin-hv: preflight VMX=0 direct_vmx_ready=0\n";
        let vmx_present = "thin-hv: preflight VMX=1 direct_vmx_ready=0\n";
        let skip =
            "thin-hv: preflight EPT audit SKIP reason=no-ept-capability direct_vmx_ready=0\n";
        let gcd = concat!(
            "thin-hv: preflight ACPI MMIO mcfg=1 madt=1 ranges=3 mmio_complete=0 direct_vmx_ready=0\n",
            "thin-hv: preflight PCI MMIO roots=1 devices=6 windows=2 bars=3 ranges=2 highest_bar_end=0x0000380000001000 mmio_complete=0 direct_vmx_ready=0\n",
            "thin-hv: preflight platform MMIO index=0 start=0x00000000fec00000 end=0x00000000fec01000 ept_type=UC\n",
            "thin-hv: preflight platform MMIO index=1 start=0x0000008000000000 end=0x0000008000001000 ept_type=UC\n",
            "thin-hv: preflight MMIO PASS source=gcd+acpi+pci descriptors=3 mmio_ranges=2 mmio_complete=0 direct_vmx_ready=0\n",
        );
        let pass = |tables: &str, leaves: &str| {
            format!(
                "thin-hv: preflight EPT audit PASS scope=uefi-memory-map+gcd+acpi+pci tables={tables} leaves={leaves} private_pages=288 mmio_complete=0 direct_vmx_ready=0\n"
            )
        };
        let kvm = format!("{vmx_present}{gcd}{}", pass("12", "3456"));
        let tcg = format!("{vmx_absent}{gcd}{skip}");
        assert!(check("kvm", &kvm));
        assert!(check("tcg", &tcg));
        assert!(check("kvm", &format!("{gcd}{}", pass("1", "1"))));
        assert!(check(
            "kvm",
            &format!("{gcd}{}", pass("256", "18446744073709551615"))
        ));
        assert!(!check("kvm", &pass("1", "1")));
        assert!(check("kvm", &kvm.replace('\n', "\r\n")));
        assert!(check("tcg", &tcg.replace('\n', "\r\n")));
        assert!(check("kvm", kvm.trim_end_matches('\n')));
        assert!(!check("tcg", skip));
        assert!(!check("kvm", &tcg));
        assert!(!check("tcg", &kvm));
        for accel in ["", "unknown", "KVM", "kvm "] {
            assert!(!check(accel, &kvm));
        }
        for tables in ["0", "01", "-1", "257", "9999", "18446744073709551616"] {
            assert!(!check("kvm", &format!("{gcd}{}", pass(tables, "1"))));
        }
        for leaves in [
            "0",
            "01",
            "-1",
            "1x",
            "18446744073709551616",
            "999999999999999999999",
        ] {
            assert!(!check("kvm", &format!("{gcd}{}", pass("1", leaves))));
        }
        for (accel, valid) in [("kvm", &kvm), ("tcg", &tcg)] {
            assert!(!check(accel, ""));
            assert!(!check(accel, &format!("{valid}{valid}")));
            assert!(!check(
                accel,
                &format!("{valid}thin-hv: physical preflight FAIL\n")
            ));
            assert!(!check(
                accel,
                &format!("{valid}thin-hv: preflight EPT audit FAIL\n")
            ));
            assert!(!check(
                accel,
                &format!("{valid}thin-hv: preflight EPT audit malformed\n")
            ));
            assert!(!check(accel, &valid.replace("EPT audit", "EPT\0 audit")));
            assert!(!check(
                accel,
                &valid.replace("direct_vmx_ready=0", "direct_vmx_ready=1")
            ));
        }
        for (from, to) in [
            ("private_pages=288", "private_pages=287"),
            ("mmio_complete=0", "mmio_complete=1"),
            ("scope=uefi-memory-map+gcd+acpi", "scope=qemu-fallback"),
            (
                "scope=uefi-memory-map+gcd+acpi",
                "scope=uefi-memory-map+gcd",
            ),
            ("leaves=3456", "leaves=3456 extra=1"),
        ] {
            assert!(!check("kvm", &kvm.replace(from, to)));
        }
        assert!(!check(
            "tcg",
            &tcg.replace("reason=no-ept-capability", "reason=unknown")
        ));
        assert!(!check("kvm", &format!("{kvm}{skip}")));
        assert!(!check("tcg", &format!("{tcg}{}", pass("1", "1"))));
        for (accel, valid) in [("kvm", &kvm), ("tcg", &tcg)] {
            for (from, to) in [
                ("descriptors=3", "descriptors=0"),
                ("descriptors=3", "descriptors=4097"),
                ("mcfg=1", "mcfg=0"),
                ("madt=1", "madt=0"),
                ("ranges=3", "ranges=0"),
                ("ranges=3", "ranges=129"),
                ("roots=1", "roots=0"),
                ("roots=1", "roots=33"),
                ("devices=6", "devices=0"),
                ("devices=6", "devices=4097"),
                ("windows=2", "windows=129"),
                ("windows=2", "windows=1"),
                ("bars=3", "bars=0"),
                ("bars=3", "bars=37"),
                ("PCI MMIO", "PCI MMIO\0"),
                ("source=gcd+acpi", "source=gcd"),
                ("mmio_ranges=2", "mmio_ranges=1"),
                ("index=1", "index=0"),
                ("index=1", "index=01"),
                ("0x0000008000001000", "0x0000008000000000"),
                ("0x0000008000001000", "0x0000008000001001"),
                ("0x0000008000001000", "0x0010000000001000"),
                ("ept_type=UC", "ept_type=WB"),
                ("MMIO PASS", "MMIO unavailable"),
            ] {
                assert!(
                    !check(accel, &valid.replace(from, to)),
                    "{accel}: {from} -> {to}"
                );
            }
            assert!(!check(accel, &valid.replace(gcd, "")));
            assert!(!check(accel, &format!("{valid}{gcd}")));
        }
        let check_direct = |high: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-direct-platform-log", high])
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let direct = "thin-hv: direct platform EPT PASS source=uefi+mtrr+gcd+acpi+pci tables=38 leaves=17240 private_pages=256 host_map=platform-ram bootstrap=shared-runtime physical_ready=0\nthin-hv: direct platform HOST PASS tables=30 leaves=12000 private_pages=256 mmio_window=uc physical_ready=0\n";
        assert!(check_direct("0", direct));
        assert!(!check_direct("1", direct));
        assert!(check_direct("1", &format!("{gcd}{direct}")));
        assert!(check_direct(
            "1",
            &format!("{gcd}{direct}").replace('\n', "\r\n")
        ));
        assert!(!check_direct("2", &format!("{gcd}{direct}")));
        assert!(!check_direct("0", &kvm)); // non-VMX audit is not an active carrier
        assert!(!check_direct(
            "0",
            "thin-hv: trusted outer KVM guest PASS\n"
        ));
        assert!(check_direct("0", &direct.repeat(64)));
        assert!(!check_direct("0", &direct.repeat(65)));
        let records: Vec<_> = direct.lines().collect();
        assert!(!check_direct("0", records[0]));
        assert!(!check_direct("0", records[1]));
        assert!(!check_direct(
            "0",
            &format!("{}\n{}\n", records[1], records[0])
        ));
        assert!(!check_direct("0", &format!("{direct}{}\n", records[1])));
        for (from, to) in [
            ("tables=38", "tables=0"),
            ("tables=38", "tables=257"),
            ("tables=30", "tables=257"),
            ("host_map=platform-ram", "host_map=fixed-8g"),
            ("mmio_window=uc", "mmio_window=wb"),
            ("HOST PASS", "HOST FAIL"),
            ("leaves=17240", "leaves=18446744073709551616"),
            ("private_pages=256", "private_pages=0"),
            ("source=uefi+mtrr+gcd+acpi+pci", "source=q35-smoke"),
            ("physical_ready=0", "physical_ready=1"),
            ("EPT PASS", "EPT FAIL"),
            ("EPT PASS", "EPT\0 PASS"),
        ] {
            assert!(
                !check_direct("0", &direct.replace(from, to)),
                "{from} -> {to}"
            );
        }
        for end in [
            "0x0000000200000000",
            "0x0000000080000000",
            "0x0010000000000001",
        ] {
            assert!(!check_direct(
                "1",
                &format!("{gcd}{direct}").replace("0x0000380000001000", end)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn vmx_diagnostics_decoder_rejects_malformed_abi_and_provenance() {
        let decoder = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/decode-vmx-diagnostics.py");
        let output = Command::new("python3")
            .arg(decoder)
            .arg("--self-test")
            .output()
            .expect("run the pure VMX diagnostics decoder tests");
        assert!(
            output.status.success(),
            "decoder host tests failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn windows_serial_marker_accepts_only_exact_lf_or_crlf_records() {
        use std::io::Write;

        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/windows/windows-test.sh");
        let check = |transcript: &[u8]| {
            let mut child = Command::new("bash")
                .arg(&runner)
                .args(["check-serial-marker", "thinhvwindowsdesktop"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("run the read-only Windows serial marker gate");
            child
                .stdin
                .take()
                .expect("serial transcript pipe")
                .write_all(transcript)
                .expect("the marker gate must drain its input even after a match");
            child.wait().expect("wait for serial marker gate").success()
        };
        assert!(check(b"thinhvwindowsdesktop\n"));
        assert!(check(b"thinhvwindowsdesktop\r\n"));
        for transcript in [
            b"".as_slice(),
            b"thin\rhvwindowsdesktop\n",
            b"thinhvwindowsdesktop\r\r\n",
            b"prefix thinhvwindowsdesktop\n",
            b"thinhvwindowsdesktop suffix\n",
            b"thinhvwindowsdesktop\r suffix\n",
            b"thinhvwindowsdesktop\0\n",
        ] {
            assert!(
                !check(transcript),
                "accepted malformed record: {transcript:?}"
            );
        }
        // The production offset path is a pipeline under pipefail. A match
        // must not close the reader before a large prefix/suffix drains.
        let mut large = "unrelated-prefix\n".repeat(8192).into_bytes();
        large.extend_from_slice(b"thinhvwindowsdesktop\r\n");
        large.extend_from_slice("unrelated-suffix\n".repeat(8192).as_bytes());
        assert!(check(&large));
    }

    #[cfg(unix)]
    #[test]
    fn windows_physical_self_test_bootstrap_selects_one_volume_and_never_collects() {
        use std::io::Write;

        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/x86_64/windows");
        let output = Command::new("bash")
            .arg(directory.join("windows-test.sh"))
            .arg("physical-test-command")
            .output()
            .unwrap();
        assert!(output.status.success());
        let command = String::from_utf8(output.stdout).unwrap();
        let encoded = command
            .strip_prefix("powershell -nop -ep bypass -encodedcommand ")
            .unwrap();
        assert!(
            encoded
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        );
        let mut decoder = Command::new("base64")
            .arg("--decode")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        decoder
            .stdin
            .take()
            .unwrap()
            .write_all(encoded.as_bytes())
            .unwrap();
        let decoded = decoder.wait_with_output().unwrap();
        assert!(decoded.status.success());
        assert_eq!(decoded.stdout.len() % 2, 0);
        let units: Vec<_> = decoded
            .stdout
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let bootstrap = String::from_utf16(&units).unwrap();
        assert!(bootstrap.contains("Get-PSDrive -PSProvider FileSystem"));
        assert!(bootstrap.contains("thin-hv-physical-status-test.ps1"));
        assert!(bootstrap.contains("if($p.Count -ne 1){exit 1}"));
        assert!(bootstrap.ends_with(";& $p[0]"));
        assert!(!bootstrap.to_ascii_lowercase().contains("d:"));
        let wrapper = fs::read_to_string(directory.join("physical-status-test.ps1")).unwrap();
        assert!(wrapper.contains("-File $source -SelfTest"));
        assert!(wrapper.contains("$LASTEXITCODE -ne 0"));
        assert!(wrapper.contains("$output.Count -ne 1"));
        assert!(wrapper.contains("[string]$output[0] -cne $expected"));
        assert!(wrapper.contains("\"hardware_queries\":0"));
        assert!(!wrapper.contains("-BootLabel"));
        assert!(!wrapper.contains("Get-CimInstance"));
    }

    #[cfg(unix)]
    #[test]
    fn nested_msr_abort_gate_requires_terminal_status_and_physical_indicator() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let make_log = || {
            let output = Command::new("mktemp")
                .args(["-t", "thin-hv-msr-abort-log.XXXXXX"])
                .output()
                .unwrap();
            assert!(output.status.success());
            FixtureLog(String::from_utf8(output.stdout).unwrap().trim().into())
        };
        let serial = make_log();
        let memory = make_log();
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-uefi-smoke.sh");
        let check = |backend: &str, code: &str, status: &str, log: &str, physical: &str| {
            fs::write(&serial.0, log).unwrap();
            fs::write(&memory.0, physical).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-msr-abort-log", backend, code, status])
                .arg(&serial.0)
                .arg(&memory.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for backend in ["direct-vmx", "outer-kvm"] {
            for code in ["1", "4"] {
                let provenance = if backend == "direct-vmx" {
                    "thin-hv: backend=direct-vmx role=project-l0\nthin-hv: backend=direct-vmx role=project-l0\nthin-hv: private host state PASS\n"
                } else {
                    "thin-hv: backend=outer-kvm role=reference\n"
                };
                let arm = format!(
                    "thin-hv: MSR abort armed code={code} vmcs=0x0000000000010000 store=0x0000000000020008 value=0x0007040500070406\n"
                );
                let terminal = if backend == "direct-vmx" {
                    format!(
                        "thin-hv: nested VMX abort code=0x000000000000000{code} vmcs=0x0000000000010000\n"
                    )
                } else {
                    String::new()
                };
                let log = format!("{provenance}thin-hv: MSR contract START\n{arm}{terminal}");
                let physical = format!(
                    "0000000000010004: 0x0000000{code}\n0000000000020008: 0x0007040500070406\n"
                );
                assert!(check(backend, code, "124", &log, &physical));
                for status in ["0", "1", "137", "143"] {
                    assert!(!check(backend, code, status, &log, &physical));
                }
                for broken in [
                    log.replace(&arm, ""),
                    log.replace(&arm, &format!("{arm}{arm}")),
                    log.replace("vmcs=0x0000000000010000", "vmcs=0x0000000000010008"),
                    log.replace("vmcs=0x0000000000010000", "vmcs=0xffff800000010000"),
                    format!("{log}thin-hv: MSR contract PASS\n"),
                    format!("{log}thin-hv: host exception FAIL: stopped\n"),
                    format!("{log}thin-hv: vmx guest FAIL\n"),
                    format!("{log}thin-hv: CPUID VMX=0\n"),
                    format!("{log}\0"),
                ] {
                    assert!(!check(backend, code, "124", &broken, &physical));
                }
                for broken in [
                    physical.replace(&format!("0x0000000{code}"), "0x00000000"),
                    physical.replace("0007040500070406", "0007040600070406"),
                    physical.replace("0000000000010004", "0000000000010000"),
                    format!("{physical}{physical}"),
                    format!("{physical}\0"),
                    String::new(),
                ] {
                    assert!(!check(backend, code, "124", &log, &broken));
                }
                if backend == "direct-vmx" {
                    assert!(!check(
                        backend,
                        code,
                        "124",
                        &log.replace(&terminal, ""),
                        &physical
                    ));
                    assert!(!check("outer-kvm", code, "124", &log, &physical));
                } else {
                    assert!(!check("direct-vmx", code, "124", &log, &physical));
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn nested_contract_gate_requires_complete_architectural_and_cleanup_evidence() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-nested-contract-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-uefi-smoke.sh");
        let check_profile = |backend: &str, cpu_profile: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(if cpu_profile == "msr" {
                    vec!["--check-msr-contract-log", backend]
                } else {
                    vec!["--check-nested-contract-log", backend, cpu_profile]
                })
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let check = |backend: &str, contents: &str| check_profile(backend, "native", contents);
        for backend in ["direct-vmx", "outer-kvm"] {
            let (provenance, terminal) = if backend == "direct-vmx" {
                (
                    "thin-hv: backend=direct-vmx role=project-l0\n\
                  thin-hv: backend=direct-vmx role=project-l0\n\
                  thin-hv: private host state PASS\n",
                    "thin-hv: vmx guest PASS start_image_status=0x0000000000000000\n",
                )
            } else {
                (
                    "thin-hv: backend=outer-kvm role=reference\n",
                    "thin-hv: trusted outer KVM guest PASS\n",
                )
            };
            let start = "thin-hv: nested contract START\n";
            let matrix = (0..128)
                .map(|case| format!("thin-hv: MSR matrix case={case}\n"))
                .collect::<String>();
            let entries = (0..20)
                .map(|case| format!("thin-hv: MSR entry case={case}\n"))
                .collect::<String>();
            let exits = (0..12)
                .map(|case| {
                    format!(
                        "thin-hv: MSR exit case={case}\n{}",
                        if case == 6 {
                            "thin-hv: MSR debugctl requested=2 observed=0\n"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<String>();
            let ept = "thin-hv: MSR EPT2M proof PASS advertised=1 large=1 split=1 replacement=1 violations=3 misconfig=1 recovery=3 invept=10\n";
            let snapshot = "thin-hv: MSR exit snapshot PASS warm=8 gpa_high=24 access_errors=2 readonly_reject=1 switches=4 clear=1\n";
            let msr = format!(
                "{provenance}thin-hv: MSR contract START\n{matrix}{exits}thin-hv: MSR control cache PASS invalid=5 resume=5\nthin-hv: MSR VPID PASS tags=2 invalid=1 types=4 invalidations=8\nthin-hv: MSR VPID lease cycles=64 fresh=64\n{ept}{snapshot}{entries}thin-hv: MSR late-failure guest-field changes=0\nthin-hv: MSR contract PASS matrix=128 exit_cases=12 exit_resume=1 entry_cases=20 entry_load=7 entry_resume=1 entry_fail=10 early_fail=2 guest_fail=2 final_vmxoff=1\n"
            );
            assert!(check_profile(backend, "msr", &msr));
            assert_eq!(
                check_profile(
                    backend,
                    "msr",
                    &msr.replace("readonly_reject=1", "readonly_reject=0")
                ),
                backend == "outer-kvm",
            );
            for fresh in [0, 1, 63] {
                assert_eq!(
                    check_profile(
                        backend,
                        "msr",
                        &msr.replace("fresh=64", &format!("fresh={fresh}"))
                    ),
                    backend == "outer-kvm",
                );
            }
            for broken in [
                msr.replace(snapshot, ""),
                msr.replace(snapshot, &format!("{snapshot}{snapshot}")),
                msr.replace("gpa_high=24", "gpa_high=23"),
                msr.replace("access_errors=2", "access_errors=1"),
                msr.replace("readonly_reject=1", "readonly_reject=2"),
                msr.replace("switches=4", "switches=3"),
                msr.replace(ept, ""),
                msr.replace(ept, &format!("{ept}{ept}")),
                msr.replace("advertised=1", "advertised=0"),
                msr.replace("split=1", "split=0"),
                msr.replace("misconfig=1", "misconfig=0"),
                msr.replace("recovery=3", "recovery=2"),
                msr.replace("invept=10", "invept=9"),
                msr.replace(
                    "thin-hv: MSR VPID PASS tags=2 invalid=1 types=4 invalidations=8\n",
                    "",
                ),
                msr.replace("thin-hv: MSR VPID lease cycles=64 fresh=64\n", ""),
                msr.replace("fresh=64", "fresh=65"),
                msr.replace("fresh=64", "fresh=064"),
                msr.replace("fresh=64", "fresh=-1"),
                msr.replace("types=4 invalidations=8", "types=3 invalidations=8"),
                msr.replace("thin-hv: MSR matrix case=63\n", ""),
                msr.replace("case=63\n", "case=62\n"),
                msr.replace("matrix=128", "matrix=127"),
                msr.replace("exit_cases=12", "exit_cases=11"),
                msr.replace("exit_resume=1", "exit_resume=0"),
                msr.replace("thin-hv: MSR control cache PASS invalid=5 resume=5\n", ""),
                msr.replace(
                    "cache PASS invalid=5 resume=5",
                    "cache PASS invalid=5 resume=4",
                ),
                msr.replace("thin-hv: MSR debugctl requested=2 observed=0\n", ""),
                msr.replace("requested=2 observed=0", "requested=2 observed=1"),
                msr.replace("thin-hv: MSR exit case=6\n", ""),
                msr.replace("MSR exit case=11\n", "MSR exit case=10\n"),
                format!("{msr}thin-hv: nested VMX abort code=1\n"),
                msr.replace("entry_cases=20", "entry_cases=19"),
                msr.replace("entry_fail=10", "entry_fail=9"),
                msr.replace("guest-field changes=0", "guest-field changes=1"),
                msr.replace("thin-hv: MSR late-failure guest-field changes=0\n", ""),
                msr.replace("MSR entry case=19\n", "MSR entry case=18\n"),
                msr.replace("thin-hv: MSR entry case=2\n", ""),
                msr.replace("vmxoff=1", "vmxoff=0"),
                msr.replace("MSR contract START", "MSR contract FAIL"),
                msr.replace(
                    "MSR contract START",
                    "MSR contract START\nthin-hv: MSR contract START",
                ),
                format!("{msr}{terminal}"),
                format!("{msr}thin-hv: host exception FAIL: stopped\n"),
                msr.replace("MSR contract START", "MSR contract START\0"),
            ] {
                assert!(!check_profile(backend, "msr", &broken));
            }
            if backend == "direct-vmx" {
                assert!(!check_profile(
                    backend,
                    "msr",
                    &msr.replace("thin-hv: private host state PASS\n", "")
                ));
                assert!(!check_profile("outer-kvm", "msr", &msr));
            } else {
                assert!(!check_profile("direct-vmx", "msr", &msr));
            }
            let cases = (0_u32..16)
                .map(|bits| {
                    (
                        (bits & 1) * 3,
                        ((bits >> 1) & 1) * 15,
                        (bits >> 2) & 1,
                        (bits >> 3) & 1,
                    )
                })
                .chain((0_u32..4).flat_map(|ept| (0_u32..16).map(move |vpid| (ept, vpid, 1, 0))));
            for (ept_types, vpid_types, readonly, shadow) in cases {
                let invept = u32::from(ept_types != 0);
                let invvpid = u32::from(vpid_types != 0);
                let success = ept_types.count_ones() + vpid_types.count_ones();
                let descriptors = (ept_types & 1)
                    + (vpid_types & 1)
                    + vpid_types.count_ones()
                    + (vpid_types & 11).count_ones();
                let count = 14 - shadow + invept + invvpid + readonly + descriptors;
                let pass = format!(
                    "thin-hv: nested contract PASS vmcs=2 cycles=8 vmfail_invalid=9 vmfail_valid={count} invept={invept} invvpid={invvpid} readonly={readonly} wide_fields=2 misaligned=2 revision=3 entry_failures=3 no_current=7 shadow={shadow} invept_types={ept_types} invvpid_types={vpid_types} invalidation_success={success} descriptor_failures={descriptors} osxsave_toggles=4 xsetbv_valid=4 xsetbv_gp=4 xsetbv_ud=1 pku=1 ospke_toggles=4 operand_pf=16 operand_gp=8 operand_ss=1 operand_cross=6 operand_priority=8 host_invalid=34 host_priority=2 host_restore=1 msr_invalid=12 msr_priority=12 msr_ignored=3 control_invalid=5 control_priority=5 control_ignored=1 guest_msr_shadow=2 fx_cpuid=6 fx_xsetbv=12 fx_entry=79 fx_irq=3 ymm_rounds=4\n"
                );
                let diagnostics = "thin-hv: native L1 operand coverage pf=16 gp=8 ss=1 cross=6 priority=8 partial_stores=0\nthin-hv: native L1 original host validation PASS invalid=34 priority=2 restored=1\n";
                let valid = format!("{provenance}{start}{diagnostics}{pass}{terminal}");
                if ept_types & 2 == 0 || vpid_types & 4 == 0 {
                    assert!(!check(backend, &valid));
                    continue;
                }
                assert!(check(backend, &valid));
                let clobber_log = valid.replace(
                    start,
                    &format!("thin-hv: host xstate clobber fixture armed\n{start}"),
                );
                assert_eq!(
                    check_profile(backend, "host-xstate", &clobber_log),
                    backend == "direct-vmx"
                );
                assert!(!check_profile(backend, "host-xstate", &valid));
                assert!(!check(backend, &clobber_log));
                assert!(check(
                    backend,
                    &valid.replace("ymm_rounds=4", "ymm_rounds=0")
                ));
                assert!(check(
                    backend,
                    &valid.replace("pku=1 ospke_toggles=4", "pku=0 ospke_toggles=0")
                ));
                assert_eq!(
                    check_profile(backend, "readonly-vmcs", &valid),
                    readonly == 1
                );
                assert!(!check_profile(backend, "unknown", &valid));
                assert!(check(backend, &valid.replace('\n', "\r\n")));
                for invalid in [
                    valid.replace(start, ""),
                    valid.replace(&pass, ""),
                    valid.replace(terminal, ""),
                    format!("{valid}{pass}"),
                    format!("{provenance}{pass}{start}{terminal}"),
                    valid.replace("vmcs=2", "vmcs=1"),
                    valid.replace("cycles=8", "cycles=0"),
                    valid.replace("vmfail_invalid=9", "vmfail_invalid=0"),
                    valid.replace(&format!("vmfail_valid={count}"), "vmfail_valid=0"),
                    valid.replace("wide_fields=2", "wide_fields=1"),
                    valid.replace("invept=", "invept=0"),
                    valid.replace("revision=3", "revision=0"),
                    valid.replace("entry_failures=3", "entry_failures=0"),
                    valid.replace("no_current=7", "no_current=0"),
                    valid.replace("osxsave_toggles=4", "osxsave_toggles=0"),
                    valid.replace("xsetbv_valid=4", "xsetbv_valid=0"),
                    valid.replace("xsetbv_gp=4", "xsetbv_gp=0"),
                    valid.replace("xsetbv_ud=1", "xsetbv_ud=0"),
                    valid.replace(" operand_pf=16", ""),
                    valid.replace("operand_pf=16", "operand_pf=15"),
                    valid.replace("operand_gp=8", "operand_gp=0"),
                    valid.replace("operand_ss=1", "operand_ss=0"),
                    valid.replace("operand_cross=6", "operand_cross=5"),
                    valid.replace("operand_priority=8", "operand_priority=7"),
                    valid.replace(" host_invalid=34", ""),
                    valid.replace("host_invalid=34", "host_invalid=33"),
                    valid.replace("host_priority=2", "host_priority=0"),
                    valid.replace("host_restore=1", "host_restore=0"),
                    valid.replace(" msr_invalid=12", ""),
                    valid.replace("msr_invalid=12", "msr_invalid=11"),
                    valid.replace("msr_priority=12", "msr_priority=0"),
                    valid.replace("msr_ignored=3", "msr_ignored=2"),
                    valid.replace("guest_msr_shadow=2", "guest_msr_shadow=0"),
                    valid.replace("fx_cpuid=6", "fx_cpuid=0"),
                    valid.replace("fx_xsetbv=12", "fx_xsetbv=11"),
                    valid.replace("fx_entry=79", "fx_entry=67"),
                    valid.replace("fx_irq=3", "fx_irq=0"),
                    valid.replace("ymm_rounds=4", "ymm_rounds=1"),
                    valid.replace("ospke_toggles=4", "ospke_toggles=0"),
                    valid.replace("pku=1", "pku=0"),
                    valid.replace("invalidation_success=", "invalidation_success=9"),
                    valid.replace("descriptor_failures=", "descriptor_failures=9"),
                    valid.clone() + "thin-hv: nested contract FAIL stage=late\n",
                    valid.clone() + "thin-hv: vmx guest FAIL\n",
                    valid.clone() + "thin-hv: backend=physical-preflight project_vmx=0\n",
                    valid.clone() + "\0",
                ] {
                    assert!(!check(backend, &invalid), "accepted: {invalid}");
                }
            }
            assert!(!check(backend, &"x".repeat(262145)));
            assert!(!check(backend, ""));
        }
        assert!(!check("physical-chainload", ""));
        let unsupported_pci = Command::new("bash")
            .arg(&runner)
            .env("X86_UEFI_PCI_PROFILE", "silent-fallback")
            .output()
            .unwrap();
        assert!(!unsupported_pci.status.success());
        assert!(
            String::from_utf8(unsupported_pci.stderr)
                .unwrap()
                .contains("X86_UEFI_PCI_PROFILE must be")
        );
    }

    #[cfg(unix)]
    #[test]
    fn linux_kvm_lifecycle_gate_requires_exact_order_backend_and_clean_shutdown() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-kvm-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-linux-kvm-test.sh");
        for (backend, flag, diagnostic) in [
            (
                "direct-vmx",
                "bad",
                "LINUX_KVM_HOST_XSTATE_TEST must be 0 or 1",
            ),
            (
                "outer-kvm",
                "1",
                "host XSTATE fixture requires project Direct L0",
            ),
        ] {
            let rejected = Command::new("bash")
                .arg(&runner)
                .env("LINUX_KVM_BACKEND", backend)
                .env("LINUX_KVM_HOST_XSTATE_TEST", flag)
                .output()
                .unwrap();
            assert!(!rejected.status.success());
            assert!(String::from_utf8_lossy(&rejected.stderr).contains(diagnostic));
        }
        let check = |backend: &str, count: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-log", backend, count])
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for (backend, role) in [("direct-vmx", "project-l0"), ("outer-kvm", "reference")] {
            let cycle = |number| {
                format!(
                    "thin-hv: linux L2 lifecycle cycle={number} KVM_RUN=IO port=0xe9 data=L2OK vm_contexts=2 rounds=8 io_in=16 io_out=96 halt=32 remaps=14 state_checks=16 sse_checks=16 long64_rounds=16 paging_checks=64 invlpg=16 cr3_writes=48 xmm16_checks=16 msr_checks=16 debug_checks=16 tsc_checks=16 teardown=explicit process_exit=0\n"
                )
            };
            let valid = format!(
                "thin-hv: backend={backend} role={role}\n\
                 thin-hv: linux L2 lifecycle begin backend={backend} cycles=2 l1_cpus=1\n\
                 {}{}\
                 thin-hv: linux L2 lifecycle PASS backend={backend} cycles=2\n\
                 thin-hv: linux L2 lifecycle poweroff requested\n",
                cycle(1),
                cycle(2)
            );
            assert!(check(backend, "2", &valid));
            assert!(check(backend, "2", &valid.replace('\n', "\r\n")));
            let kernel_records = valid
                .lines()
                .map(|line| {
                    if line.starts_with("thin-hv: linux") {
                        format!("[    1.234567] {line}\n")
                    } else {
                        format!("{line}\n")
                    }
                })
                .collect::<String>();
            assert!(check(backend, "2", &kernel_records));
            assert!(!check(
                backend,
                "2",
                &kernel_records.replace(
                    "process_exit=0\n",
                    "process_exit=0[    1.260008] tsc: calibration\n"
                )
            ));
            assert!(!check(backend, "3", &valid));
            assert!(!check(backend, "2", &(valid.clone() + "\0")));
            assert!(!check(backend, "2", &"x".repeat(2_097_153)));
            assert!(!check(backend, "02", &valid));
            assert!(!check("unknown", "2", &valid));
            assert!(!check(backend, "2", &valid.replace(&cycle(1), "")));
            assert!(!check(backend, "2", &valid.replace(&cycle(2), &cycle(1))));
            assert!(!check(
                backend,
                "2",
                &valid.replace(&cycle(1), &(cycle(2) + &cycle(1)))
            ));
            assert!(!check(
                backend,
                "2",
                &valid.replace("process_exit=0", "process_exit=1")
            ));
            for missing in [
                " vm_contexts=2",
                " rounds=8",
                " io_in=16",
                " io_out=96",
                " halt=32",
                " remaps=14",
                " state_checks=16",
                " sse_checks=16",
                " long64_rounds=16",
                " paging_checks=64",
                " invlpg=16",
                " cr3_writes=48",
                " xmm16_checks=16",
                " msr_checks=16",
                " debug_checks=16",
                " tsc_checks=16",
                " teardown=explicit",
            ] {
                assert!(!check(backend, "2", &valid.replace(missing, "")));
            }
            assert!(!check(
                backend,
                "2",
                &valid.replace("thin-hv: linux L2 lifecycle poweroff requested\n", "")
            ));
            for failure in [
                "thin-hv: linux L2 lifecycle FAIL",
                "thin-hv: linux L1 L2 KVM FAIL",
                "Kernel panic",
                "Oops:",
                "BUG:",
                "thin-hv: backend=physical-preflight project_vmx=0",
            ] {
                assert!(!check(backend, "2", &format!("{valid}{failure}\n")));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn linux_nested_power_gates_require_backend_coverage_and_shutdown() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-power-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let check = |script: &str, backend: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            let mut command = Command::new("bash");
            command
                .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(script))
                .args(["--check-log", backend]);
            if script.ends_with("run-linux-soak-test.sh") {
                command.args(["2", "3"]);
            }
            command
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let soak_runner = "../scripts/x86_64/run-linux-soak-test.sh";
        let s3_runner = "../scripts/x86_64/run-linux-suspend-test.sh";
        for (backend, cpus) in [("direct-vmx", 1), ("outer-kvm", 2)] {
            let boot = if backend == "direct-vmx" {
                "thin-hv: uefi entry\nthin-hv: backend=direct-vmx role=project-l0\n\
                 thin-hv: uefi entry\nthin-hv: backend=direct-vmx role=project-l0\nthin-hv: private host state PASS\n"
            } else {
                "thin-hv: uefi entry\nthin-hv: backend=outer-kvm role=reference\n\
                 thin-hv: trusted outer KVM direct chainload profile=2 resident_runtime=0\n"
            };
            let mut soak = String::new();
            for phase in 1..=2 {
                soak.push_str(&format!(
                    "{boot}thin-hv-soak: L1 boot begin uptime=0\n\
                     thin-hv-soak: topology backend={backend} l1_cpus={cpus}\n\
                     thin-hv-soak: nested KVM ready\n\
                     thin-hv-soak: phase={phase}\n\
                     thin-hv-soak: usernet PASS packets=5\n\
                     thin-hv-soak: cpu-memory PASS bytes=134217728 workers=2 hashes=4 sha256=fixture\n\
                     thin-hv-soak: repeated L2 PASS count=3 start=0 end=1\n"
                ));
                if phase == 1 {
                    soak.push_str("thin-hv-soak: virtio-blk PASS phase=write bytes=134217728 sha256=fixture\nthin-hv-soak: reboot requested\n");
                } else {
                    soak.push_str("thin-hv-soak: virtio-blk PASS phase=reboot-read bytes=134217728 sha256=fixture\nthin-hv-soak: PASS phase=2 hashes=4 l2=3 uptime=1\nthin-hv-soak: poweroff requested\n");
                }
            }
            let mut s3 = format!(
                "{boot}thin-hv: linux S3 begin backend={backend} l1_cpus={cpus}\n\
                 thin-hv: linux S3 CPUs online PASS before suspend cycles\n\
                 thin-hv: linux L1 L2 KVM PASS\n"
            );
            for cycle in 1..=3 {
                s3.push_str(&format!(
                    "thin-hv: linux S3 EFI runtime write PASS cycle={cycle} variable=DriverFFFF\n\
                     thin-hv: linux S3 suspend begin cycle={cycle}\n\
                     thin-hv: linux S3 resume cycle={cycle}\n\
                     thin-hv: linux S3 CPUs online PASS cycle={cycle}\n\
                     thin-hv: linux L1 L2 KVM PASS\n\
                     thin-hv: linux S3 EFI runtime resume PASS cycle={cycle} variable=DriverFFFF\n\
                     thin-hv: linux S3 EFI runtime delete PASS cycle={cycle} variable=DriverFFFF\n\
                     thin-hv: linux S3 EFI runtime PASS cycle={cycle}\n"
                ));
            }
            s3.push_str("thin-hv: linux S3 nested KVM PASS cycles=3\nthin-hv: linux S3 poweroff requested\n");
            for (runner, valid) in [(soak_runner, &soak), (s3_runner, &s3)] {
                assert!(check(runner, backend, valid));
                assert!(check(runner, backend, &valid.replace('\n', "\r\n")));
                for invalid in [
                    valid.replace("poweroff requested", "missing shutdown"),
                    valid.replace("l1_cpus=", "wrong_topology="),
                    format!("{valid}\0"),
                    format!("{valid}thin-hv: backend=physical-preflight project_vmx=0\n"),
                    format!("{valid}Kernel panic\n"),
                    format!("{valid}thin-hv-soak: FAIL late\nthin-hv: linux S3 FAIL late\n"),
                ] {
                    assert!(!check(runner, backend, &invalid));
                }
                assert!(!check(runner, "unknown", valid));
                assert!(!check(runner, backend, &"x".repeat(2_097_153)));
            }
            assert!(!check(
                soak_runner,
                backend,
                &soak.replacen("thin-hv: uefi entry\n", "", 1)
            ));
            assert!(!check(
                soak_runner,
                backend,
                &soak.replace("count=3", "count=2")
            ));
            assert!(!check(
                soak_runner,
                backend,
                &soak.replace(&format!("l1_cpus={cpus}"), &format!("l1_cpus={cpus}0"))
            ));
            assert!(!check(
                soak_runner,
                backend,
                &soak.replace("hashes=4", "hashes=0")
            ));
            assert!(!check(
                soak_runner,
                backend,
                &soak.replace("phase=1\n", "phase=2\n")
            ));
            assert!(!check(
                s3_runner,
                backend,
                &s3.replace("thin-hv: linux L1 L2 KVM PASS\n", "")
            ));
            assert!(!check(
                s3_runner,
                backend,
                &s3.replace("resume cycle=2", "resume cycle=1")
            ));
            assert!(!check(
                s3_runner,
                backend,
                &s3.replace("EFI runtime PASS cycle=3", "EFI runtime PASS cycle=2")
            ));
            let kernel_records = s3
                .lines()
                .map(|line| {
                    if line.starts_with("thin-hv: linux") {
                        format!("[    1.234567] {line}\n")
                    } else {
                        format!("{line}\n")
                    }
                })
                .collect::<String>();
            assert!(check(s3_runner, backend, &kernel_records));
            assert!(!check(
                s3_runner,
                backend,
                &kernel_records.replace(
                    "variable=DriverFFFF",
                    "variable=Driver[    1.2] PM: suspend\nFFFF"
                )
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn linux_upstream_selftest_gate_requires_all_tap_assertions_and_exit_status() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-selftest-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-linux-selftest.sh");
        let check = |backend: &str, name: &str, contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-log", backend, name])
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for (backend, role) in [("direct-vmx", "project-l0"), ("outer-kvm", "reference")] {
            let mwait_names = (0..16)
                .filter(|tc| tc & 2 != 0 || (tc & 4 != 0) == (tc & 8 != 0))
                .map(|tc| {
                    format!(
                        "MWAIT {}, {}, CPUID {}{}",
                        if tc & 1 != 0 {
                            "can fault"
                        } else {
                            "never faults"
                        },
                        if tc & 2 != 0 {
                            "MISC_ENABLE updates CPUID"
                        } else {
                            "no CPUID updates"
                        },
                        if tc & 8 != 0 { "clear" } else { "set" },
                        if tc & 4 != 0 { ", MWAIT disabled" } else { "" }
                    )
                })
                .collect::<Vec<_>>();
            for (name, labels, harness) in [
                (
                    "tsc_msrs_test",
                    (2..=6)
                        .map(|n| format!("stage {n} passed"))
                        .collect::<Vec<_>>(),
                    false,
                ),
                (
                    "userspace_msr_exit_test",
                    [
                        "msr_filter_allow",
                        "msr_filter_deny",
                        "msr_permission_bitmap",
                        "user_exit_msr_flags",
                    ]
                    .map(|n| format!("user_msr.{n}"))
                    .to_vec(),
                    true,
                ),
                (
                    "sync_regs_test",
                    [
                        "read_invalid",
                        "set_invalid",
                        "req_and_verify_all_valid",
                        "set_and_verify_various",
                        "clear_kvm_dirty_regs_bits",
                        "clear_kvm_valid_and_dirty_regs",
                        "clear_kvm_valid_regs_bits",
                        "race_cr4",
                        "race_exc",
                        "race_inj_pen",
                    ]
                    .map(|n| format!("sync_regs_test.{n}"))
                    .to_vec(),
                    true,
                ),
                (
                    "fix_hypercall_test",
                    ["enable_quirk", "disable_quirk"]
                        .map(|n| format!("fix_hypercall.{n}"))
                        .to_vec(),
                    true,
                ),
                (
                    "kvm_binary_stats_test",
                    (0..4).map(|n| format!("vm{n}")).collect::<Vec<_>>(),
                    false,
                ),
                ("monitor_mwait_test", mwait_names, false),
                (
                    "steal_time",
                    (0..4).map(|n| format!("vcpu{n}")).collect::<Vec<_>>(),
                    false,
                ),
            ] {
                let count = labels.len();
                let assertions = labels
                    .iter()
                    .enumerate()
                    .map(|(n, label)| format!("ok {} {label}\n", n + 1))
                    .collect::<String>();
                let suite = if harness {
                    format!("# PASSED: {count} / {count} tests passed.\n")
                } else {
                    String::new()
                };
                let begin = format!(
                    "thin-hv: linux KVM selftest begin backend={backend} test={name} l1_cpus=1\n"
                );
                let end = format!(
                    "thin-hv: linux KVM selftest exit backend={backend} test={name} process_exit=0\n"
                );
                let pass = format!(
                    "thin-hv: linux KVM selftest PASS backend={backend} test={name} assertions={count}\n"
                );
                let totals =
                    format!("# Totals: pass:{count} fail:0 xfail:0 xpass:0 skip:0 error:0\n");
                let poweroff = "thin-hv: linux KVM selftest poweroff requested\n";
                let valid = format!(
                    "thin-hv: backend={backend} role={role}\n{begin}TAP version 13\n1..{count}\n{assertions}{suite}{totals}{end}{pass}{poweroff}"
                );
                assert!(check(backend, name, &valid));
                assert!(check(backend, name, &valid.replace('\n', "\r\n")));
                for missing in [
                    &begin,
                    &end,
                    &pass,
                    &assertions,
                    &totals,
                    poweroff,
                    "TAP version 13\n",
                ] {
                    assert!(
                        !check(backend, name, &valid.replace(missing, "")),
                        "missing {missing}"
                    );
                }
                for (from, to) in [
                    ("process_exit=0", "process_exit=1"),
                    ("l1_cpus=1", "l1_cpus=2"),
                    ("TAP version 13", "TAP version 12"),
                    ("skip:0", "skip:1"),
                    ("fail:0", "fail:1"),
                    ("ok 1 ", "ok 2 "),
                    ("1..", "1..0 # SKIP "),
                ] {
                    assert!(
                        !check(backend, name, &valid.replace(from, to)),
                        "mutation {from}"
                    );
                }
                assert!(!check(
                    backend,
                    name,
                    &valid.replace(&end, &(pass.clone() + &end))
                ));
                assert!(!check(
                    backend,
                    name,
                    &valid.replace(&begin, &(begin.clone() + &begin))
                ));
                if harness {
                    assert!(!check(backend, name, &valid.replace(&suite, "")));
                }
                for suffix in [
                    "\0",
                    "not ok 6 unexpected\n",
                    "Bail out!\n",
                    "Kernel panic\n",
                    "thin-hv: linux KVM selftest FAIL late\n",
                    "ok 6 unexpected\n",
                    "thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0\n",
                ] {
                    assert!(!check(backend, name, &(valid.clone() + suffix)));
                }
                assert!(!check("unknown", name, &valid));
                assert!(!check(backend, "vmx_test", &valid));
                assert!(!check(backend, name, &"x".repeat(2_097_153)));
            }
        }
        for (backend, role) in [("direct-vmx", "project-l0"), ("outer-kvm", "reference")] {
            for name in [
                "cr4_cpuid_sync_test",
                "xcr0_cpuid_test",
                "debug_regs",
                "apic_bus_clock_test",
                "xapic_tpr_test",
                "cpuid_test",
                "msrs_test",
                "set_sregs_test",
                "userspace_io_test",
                "state_test",
                "xapic_state_test",
                "xapic_ipi_test",
                "recalc_apic_map_test",
                "tsc_scaling_sync",
                "kvm_clock_test",
                "feature_msrs_test",
                "xss_msr_test",
                "fastops_test",
                "kvm_pv_test",
                "platform_info_test",
                "ucna_injection_test",
                "exit_on_emulation_failure_test",
                "smaller_maxphyaddr_emulation_test",
                "hyperv_clock",
                "hyperv_cpuid",
                "hyperv_features",
                "hyperv_ipi",
                "hyperv_tlb_flush",
                "hyperv_extended_hypercalls",
                "set_boot_cpu_id",
                "max_vcpuid_cap_test",
                "smm_test",
                "amx_test",
                "pmu_counters_test",
                "pmu_event_filter_test",
                "dirty_log_test",
                "guest_print_test",
                "irqfd_test",
                "set_memory_region_test",
                "coalesced_io_test",
                "hardware_disable_test",
                "guest_memfd_test",
                "system_counter_offset_test",
                "pre_fault_memory_test",
                "demand_paging_test",
                "kvm_create_max_vcpus",
                "kvm_page_table_test",
                "memslot_modification_stress_test",
                "memslot_perf_test",
                "access_tracking_perf_test",
                "dirty_log_perf_test",
                "mmu_stress_test",
                "rseq_test",
                "xen_vmcall_test",
                "xen_shinfo_test",
                "private_mem_kvm_exits_test",
                "private_mem_conversions_test",
                "nx_huge_pages_test",
                "dirty_log_page_splitting_test",
                "vmx_exception_with_invalid_guest_state",
                "aperfmperf_test",
                "kvm_buslock_test",
                "hwcr_msr_test",
            ] {
                let cpus = if name == "rseq_test" && backend == "outer-kvm" {
                    2
                } else {
                    1
                };
                let valid = format!(
                    "thin-hv: backend={backend} role={role}\nthin-hv: linux KVM selftest begin backend={backend} test={name} l1_cpus={cpus}\nthin-hv: linux KVM selftest exit backend={backend} test={name} process_exit=0\nthin-hv: linux KVM selftest PASS backend={backend} test={name} assertions=1\nthin-hv: linux KVM selftest poweroff requested\n"
                );
                let valid = if name == "memslot_perf_test" {
                    let results = ["map", "unmap", "unmap chunked", "move active area", "move inactive area", "RW"]
                        .map(|label| format!("Testing {label} performance with 1 runs, 5 seconds each\nDone 42 iterations, avg 0.100000000s each\n"))
                        .join("");
                    valid.replace(
                        "thin-hv: linux KVM selftest exit",
                        &(results + "thin-hv: linux KVM selftest exit"),
                    )
                } else {
                    valid
                };
                assert!(check(backend, name, &valid));
                if name == "memslot_perf_test" {
                    for (from, to) in [
                        ("Done 42", "Done 0"),
                        ("Testing map performance", "Testing unmap performance"),
                        ("Done 42 iterations, avg 0.100000000s each\n", ""),
                    ] {
                        assert!(!check(backend, name, &valid.replace(from, to)));
                    }
                    assert!(!check(
                        backend,
                        name,
                        &(valid.clone() + "Memslot count too high for this test\n")
                    ));
                }
                assert!(!check(
                    backend,
                    name,
                    &valid.replace(&format!("l1_cpus={cpus}"), "l1_cpus=3")
                ));
                for status in ["1", "4", "137"] {
                    assert!(!check(
                        backend,
                        name,
                        &valid.replace("process_exit=0", &format!("process_exit={status}"))
                    ));
                }
                for suffix in [
                    "TAP version 13\n",
                    "ok 1 forged\n",
                    "Test Assertion Failure\n",
                    "thin-hv: linux KVM selftest FAIL late\n",
                ] {
                    assert!(!check(backend, name, &(valid.clone() + suffix)));
                }
                assert!(!check(
                    backend,
                    name,
                    &valid.replace("assertions=1", "assertions=0")
                ));
            }
        }
        assert!(
            !Command::new("bash")
                .arg(&runner)
                .arg("--check-elf")
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(unix)]
    #[test]
    fn linux_kunit_gates_require_real_results_and_complete_ordered_matrix() {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let output = Command::new("mktemp")
            .args(["-t", "thin-hv-kunit-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let log = Fixture(String::from_utf8(output.stdout).unwrap().trim().into());
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/x86_64");
        let runner = directory.join("run-linux-kunit-test.sh");
        let manifest = fs::read_to_string(directory.join("linux-l2-kunit-cases.txt")).unwrap();
        let check = |args: &[&str], contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(args)
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .code()
                .unwrap()
        };
        assert_eq!(check(&["--check-manifest"], &manifest), 0);
        for invalid in [
            String::new(),
            manifest.clone() + "\0",
            manifest.clone() + "x2apic|apic.flat|2|128|30|qemu64|-\n",
            manifest.replace("apic.flat", "../apic.flat"),
            manifest.replace("|128|", "|9999999999999|"),
            manifest.replace("|1200|", "|5401|"),
            manifest.replace("|2|", "|0|"),
            manifest.replace("|qemu64|", "|qemu64;reboot|"),
        ] {
            assert_ne!(check(&["--check-manifest"], &invalid), 0);
        }
        for (seconds, accepted) in [(0, false), (1, true), (5400, true), (5401, false)] {
            let single = format!("probe|emulator.flat|1|128|{seconds}|qemu64|-\n");
            assert_eq!(check(&["--check-manifest"], &single) == 0, accepted);
        }
        for (last_seconds, accepted) in [(3480, true), (3481, false)] {
            let total_boundary = format!(
                "a|emulator.flat|1|128|5400|qemu64|-\nb|emulator.flat|1|128|5400|qemu64|-\nc|emulator.flat|1|128|{last_seconds}|qemu64|-\n"
            );
            assert_eq!(check(&["--check-manifest"], &total_boundary) == 0, accepted);
        }
        for (text, status, expected) in [
            ("SUMMARY: 3 tests, 1 skipped\n", "1", 5),
            ("SUMMARY: 3 tests, 1 expected failures\n", "1", 5),
            ("SUMMARY: 1 tests, 1 skipped\n", "77", 4),
            ("SUMMARY: 1 tests, 1 skipped\n", "1", 1),
            ("SUMMARY: 1 tests\n", "3", 1),
            ("SUMMARY: 1 tests\n", "137", 1),
            ("SUMMARY: 1 tests\nFAIL: late\n", "1", 1),
            (
                "test pte.a: FAIL: wrong accessed bit\nSUMMARY: 1 tests\n",
                "1",
                1,
            ),
            ("SUMMARY: 1 tests, 1 known failures\n", "1", 1),
            ("SUMMARY: 1 tests, 2 skipped\n", "1", 1),
            ("SUMMARY: 1 tests, 1 expected failures, 1 skipped\n", "1", 1),
            ("SUMMARY: 1 tests\nSUMMARY: 1 tests\n", "1", 1),
            ("only diagnostics\n", "1", 1),
            ("SUMMARY: 1 tests\0\n", "1", 1),
        ] {
            assert_eq!(
                check(&["--check-case", "emulator", status], text),
                expected,
                "{text}"
            );
        }
        assert_eq!(
            check(&["--check-case", "vmexit_cpuid", "1"], "cpuid 150\n"),
            0
        );
        assert_ne!(
            check(&["--check-case", "vmexit_cpuid", "1"], "vmcall 150\n"),
            0
        );
        assert_ne!(
            check(&["--check-case", "vmexit_cpuid", "1"], "SUMMARY: 1 tests\n"),
            0
        );
        let cases = manifest
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.split('|').collect::<Vec<_>>())
            .collect::<Vec<_>>();
        for (backend, role) in [("outer-kvm", "reference"), ("direct-vmx", "project-l0")] {
            let mut valid = format!(
                "thin-hv: backend={backend} role={role}\nthin-hv: KVM unit matrix begin backend={backend} selection=all l1_cpus=1\n"
            );
            for case in &cases {
                let name = case[0];
                let result = match name {
                    "realmode" => "PASS: realmode instruction\n".to_string(),
                    "rmap_chain" => "PASS\n".to_string(),
                    "s3" => "PM1a event registers at 600\n".to_string(),
                    "sieve" => "static:78498 out of 1000000\nmapped:78498 out of 1000000\nvirtual:5761455 out of 100000000\nvirtual:5761455 out of 100000000\nvirtual:5761455 out of 100000000\n".to_string(),
                    _ if name.starts_with("vmexit_") => format!("{} 1234\n", case[6]),
                    _ => "SUMMARY: 1 tests\n".to_string(),
                };
                valid.push_str(&format!("thin-hv: KVM unit start name={name} cpus={} memory_mib={} timeout_seconds={} accel=kvm\n", case[2], case[3], case[4]));
                for line in result.lines() {
                    valid.push_str(&format!("KUNIT: {line}\n"));
                }
                valid.push_str(&format!(
                    "thin-hv: KVM unit exit name={name} process_exit=1 outcome=PASS\n"
                ));
            }
            valid.push_str(&format!("thin-hv: KVM unit matrix complete backend={backend} selection=all cases={} passed={} failed=0 skipped=0 partial=0\nthin-hv: KVM unit matrix poweroff requested\n", cases.len(), cases.len()));
            assert_eq!(check(&["--check-log", backend, "all"], &valid), 0);
            assert_eq!(
                check(
                    &["--check-log", backend, "all"],
                    &valid.replace('\n', "\r\n")
                ),
                0
            );
            for (from, to) in [
                ("accel=kvm", "accel=tcg"),
                ("process_exit=1", "process_exit=0"),
                ("name=xapic", "name=x2apic"),
                ("outcome=PASS", "outcome=SKIP"),
                (
                    "SUMMARY: 1 tests",
                    "SUMMARY: 1 tests, 1 unexpected failures",
                ),
                ("thin-hv: KVM unit matrix poweroff requested\n", ""),
            ] {
                assert_ne!(
                    check(&["--check-log", backend, "all"], &valid.replace(from, to)),
                    0,
                    "{from}"
                );
            }
            for suffix in [
                "\0",
                "KUNIT: FAIL: late\n",
                "Kernel panic\n",
                "thin-hv: KVM unit matrix poweroff requested\n",
            ] {
                assert_ne!(
                    check(&["--check-log", backend, "all"], &(valid.clone() + suffix)),
                    0
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn windows_poweroff_timeout_is_bounded_and_decimal() {
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/windows/windows-test.sh");
        for (input, accepted) in [
            ("1", true),
            ("120", true),
            ("1200", true),
            ("1800", true),
            ("0", false),
            ("1801", false),
            ("99999", false),
            ("0120", false),
            ("-1", false),
            ("1+1", false),
            ("1\n2", false),
            ("", false),
        ] {
            assert_eq!(
                Command::new("bash")
                    .arg(&runner)
                    .args(["check-poweroff-timeout", input])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                accepted,
                "{input:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn linux_l2_os_gate_requires_six_complete_kvm_boots() {
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-l2-os-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = std::path::PathBuf::from(String::from_utf8(temporary.stdout).unwrap().trim());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-linux-l2-os-test.sh");
        let check = |backend: &str, contents: &str| {
            fs::write(&log, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .args(["--check-log", backend])
                .arg(&log)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for (backend, role) in [("direct-vmx", "project-l0"), ("outer-kvm", "reference")] {
            let mut valid = format!(
                "thin-hv: backend={backend} role={role}\nthin-hv: linux L2 OS begin backend={backend} boots=6 l1_cpus=1\n"
            );
            for boot in 1..=6 {
                let cpus = if boot % 2 == 1 { 1 } else { 2 };
                let persist = if boot == 1 { 0 } else { 1 };
                valid.push_str(&format!(
                    "thin-hv: linux L2 OS start boot={boot} cpus={cpus} accel=kvm\nL2: thin-hv: linux OS L2 begin cpus={cpus}\nL2: thin-hv: linux OS L2 PASS cpus={cpus} workers=2 hashes=32 memory_mib=32 copy_mib=32 sha256=83ee47245398adee79bd9c0a8bc57b821e92aba10f5f9ade8a5d1fae4d8c4302 boot={boot} disk_mib=32 disk_persist={persist} net_packets=3\nL2: [    1.250000] reboot: Power down\nthin-hv: linux L2 OS exit boot={boot} cpus={cpus} process_exit=0\n"
                ));
            }
            valid.push_str(&format!(
                "thin-hv: linux L2 OS PASS backend={backend} boots=6 l1_cpus=1 l2_cpus=1,2\nthin-hv: linux L2 OS poweroff requested\n"
            ));
            assert!(check(backend, &valid));
            assert!(check(backend, &valid.replace('\n', "\r\n")));
            for (from, to) in [
                ("accel=kvm", "accel=tcg"),
                ("boot=3", "boot=2"),
                ("process_exit=0", "process_exit=137"),
                ("cpus=2", "cpus=1"),
                ("hashes=32", "hashes=31"),
                ("disk_persist=1", "disk_persist=0"),
                ("net_packets=3", "net_packets=2"),
                ("83ee4724", "00000000"),
                ("L2: [    1.250000] reboot: Power down\n", ""),
                ("thin-hv: linux L2 OS poweroff requested\n", ""),
            ] {
                assert!(!check(backend, &valid.replace(from, to)), "{from}");
            }
            for suffix in [
                "\0",
                "Kernel panic\n",
                "L2: thin-hv: linux OS L2 FAIL late\n",
                "thin-hv: linux L2 OS poweroff requested\n",
                "thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0\n",
            ] {
                assert!(!check(backend, &(valid.clone() + suffix)));
            }
        }
        fs::remove_file(log).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn physical_policy_gate_requires_same_esp_and_scoped_expected_failures() {
        struct FixtureLog(std::path::PathBuf);
        impl Drop for FixtureLog {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let temporary = Command::new("mktemp")
            .args(["-t", "thin-hv-physical-policy-log.XXXXXX"])
            .output()
            .unwrap();
        assert!(temporary.status.success());
        let log = FixtureLog(String::from_utf8(temporary.stdout).unwrap().trim().into());
        let runner = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/x86_64/run-uefi-smoke.sh");
        let check = |contents: &str| {
            fs::write(&log.0, contents).unwrap();
            Command::new("bash")
                .arg(&runner)
                .arg("--check-physical-policy-log")
                .arg(&log.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let backend = "thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0\n";
        let payload = "thin-hv: physical policy payload path=windows current_esp=1 PASS\n";
        let failure = "thin-hv: physical chainload FAIL: expected negative fixture";
        let precondition = "thin-hv: physical policy secondary_esp_visible=1 targets=windows,linux,other-only PASS\n";
        let mut valid = precondition.to_owned();
        for (name, path, status) in [
            ("default-windows", "windows", "0x0"),
            ("explicit-linux", "linux", "0x0"),
            ("other-esp-only", "", "0x800000000000000e"),
            ("malformed-options", "", "0x8000000000000002"),
            ("self-path", "", "0x800000000000000f"),
        ] {
            valid.push_str(&format!(
                "thin-hv: physical policy case={name} begin\nthin-hv: uefi entry\n{backend}"
            ));
            if path.is_empty() {
                valid.push_str(&format!("{failure} status={status}\n"));
            } else {
                valid.push_str(&format!("thin-hv: physical policy payload path={path} current_esp=1 PASS\nthin-hv: physical chainload PASS\n"));
            }
            valid.push_str(&format!(
                "thin-hv: physical policy case={name} PASS status={status}\n"
            ));
        }
        valid.push_str("thin-hv: physical policy harness PASS\n");
        assert!(check(&valid));
        assert!(check(&valid.replace('\n', "\r\n")));
        assert!(!check(&valid.replace(precondition, "")));
        assert!(!check(&format!("{precondition}{valid}")));
        assert!(!check(&format!(
            "{}{precondition}",
            valid.replace(precondition, "")
        )));
        assert!(!check(&valid.replace("current_esp=1", "current_esp=0")));
        assert!(!check(&valid.replace("path=linux", "path=windows")));
        assert!(!check(&valid.replace("0x800000000000000e", "0x0")));
        assert!(!check(&valid.replace(payload, "")));
        assert!(!check(
            &valid.replace(backend, &(backend.to_owned() + backend))
        ));
        assert!(!check(
            &valid.replace(failure, &(payload.to_owned() + failure))
        ));
        assert!(!check(
            &valid.replace("thin-hv: physical policy harness PASS\n", "")
        ));
        assert!(!check(&valid.replace(
            "thin-hv: physical policy case=explicit-linux begin",
            "thin-hv: physical policy case=default-windows begin"
        )));
        assert!(!check(
            &valid.replace("thin-hv: physical chainload PASS\n", failure)
        ));
        for forbidden in [
            failure,
            "thin-hv: physical policy harness FAIL\n",
            "thin-hv: backend=outer-kvm role=reference\n",
            "thin-hv: runtime monitor active\n",
            "thin-hv: variable overlay profile=1\n",
            "thin-hv: L1 VMLAUNCH\n",
        ] {
            assert!(!check(&format!("{valid}{forbidden}")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_is_not_blocked_by_output_forwarders() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 6")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let code = run_guest_test_with_timeout(cmd, "output-pump-timeout", 1, "test");

        assert_eq!(code, 124);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout reporting waited for the child output pumps to exit"
        );
    }
}
