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
            let _ = build(&remaining_args).unwrap();
        }
        Some("run") => {
            run(&remaining_args).unwrap();
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
    if args
        .iter()
        .any(|arg| arg == "--all-features" || arg.contains("trusted-outer-kvm"))
    {
        return Err(
            "x86 builds both direct and trusted-outer-KVM artifacts; do not select trusted-outer-kvm explicitly"
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
    let monitor_destination = workspace
        .join("bin")
        .join("x86_64")
        .join("x86-uefi-monitor.efi");
    let trusted_destination = workspace
        .join("bin")
        .join("x86_64")
        .join("x86-uefi-kvm-loader.efi");
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

    eprintln!("\n--- Building trusted-outer-KVM x86 UEFI package ---");
    let status = Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg(pkg)
        .arg("--target")
        .arg("x86_64-unknown-uefi")
        .args(args)
        .arg("--no-default-features")
        .arg("--features")
        .arg("trusted-outer-kvm")
        .env("XTASK_BUILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("Failed to build trusted-outer-KVM loader: {}", e))?;
    if !status.success() {
        return Err(format!(
            "trusted-outer-KVM loader build failed with status: {}",
            status
        ));
    }
    verify_no_decoded_vmx(&artifact)?;
    fs::copy(&artifact, &trusted_destination).map_err(|e| {
        format!(
            "Failed to copy {} to {}: {}",
            artifact.display(),
            trusted_destination.display(),
            e
        )
    })?;
    Ok(destination.to_string_lossy().into_owned())
}

const VMX_MNEMONICS: [&str; 17] = [
    "vmcall", "vmclear", "vmlaunch", "vmresume", "vmptrld", "vmptrst", "vmread", "vmreadl",
    "vmreadq", "vmwrite", "vmwritel", "vmwriteq", "vmxoff", "vmxon", "invept", "invvpid", "vmfunc",
];

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
            "trusted outer-KVM artifact {} contains decoded VMX instruction '{}'",
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

fn run_x86_uefi(args: &[String]) -> Result<(), String> {
    let binary_path = build_x86_uefi(args)?;
    eprintln!("\n--- Running direct x86 UEFI smoke test ---");
    let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
        .arg(binary_path)
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

    for guest_location in ["guest", "windows", "both"] {
        eprintln!("\n--- Running trusted-outer-KVM x86 UEFI smoke test ({guest_location}) ---");
        let status = Command::new("./scripts/x86_64/run-uefi-smoke.sh")
            .arg("bin/x86_64/x86-uefi-kvm-loader.efi")
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
