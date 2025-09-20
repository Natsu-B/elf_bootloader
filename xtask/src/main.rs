#![crate_type = "bin"]
// xtask/src/main.rs

use core::panic;
use std::fs;
use std::process::Command;
use std::process::Stdio;

// cargo metadataの関連する部分の構造体を定義
#[derive(Debug, serde::Deserialize)]
struct CargoMetadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>, // これらのIDは'packages'内の'id'と一致します
}

#[derive(Debug, serde::Deserialize)]
struct Package {
    id: String,
    name: String, // `cargo test -p <name>` で使用するパッケージ名
}

fn main() {
    let mut args = std::env::args().skip(1); // 実行ファイル名 (xtask) をスキップ

    let command = args.next();

    // イテレータの残りをすべて収集して引数リストを作成
    let remaining_args: Vec<String> = args.collect();

    // command は Option<String> なので、.as_deref() を使って &str に変換してマッチさせる
    match command.as_deref() {
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

fn build(args: &[String]) -> Result<String, &'static str> {
    // Build bootloader crate only (package name = elf-hypervisor)
    let pkg = "elf-hypervisor";
    eprintln!("\n--- Building bootloader package: {} ---", pkg);
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("-p")
        .arg(pkg)
        .arg("--target")
        .arg("aarch64-unknown-none")
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
    eprintln!("\n--- Searching for built binary... ---");
    let mut binary_dir = std::env::current_dir().unwrap();
    binary_dir.push("target");
    binary_dir.push("aarch64-unknown-none");
    binary_dir.push("debug");
    binary_dir.push("elf-hypervisor");
    let mut binary_new_dir = std::env::current_dir().unwrap();
    binary_new_dir.push("bin");
    let _ = fs::create_dir(binary_new_dir.clone());
    binary_new_dir.push("elf-hypervisor.elf");
    std::fs::copy(binary_dir, binary_new_dir.clone()).expect("failed to copy built binary");
    Ok(binary_new_dir.to_string_lossy().into_owned())
}

fn run(args: &[String]) -> Result<(), &'static str> {
    let binary_path = build(args)?;

    eprintln!("\n--- Running ./run.sh ---");
    use std::os::unix::process::CommandExt;
    let _ = Command::new("./run.sh").arg(&binary_path).args(args).exec();
    unreachable!();
}

fn test(args: &[String]) {
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

    // Load optional plan (xtest.txt). If not present, build a default plan.
    let repo_root = std::env::current_dir().expect("failed to get CWD");
    let plan_path = repo_root.join("xtest.txt");
    let plan = std::fs::read_to_string(&plan_path).ok();

    let mut std_crates: Vec<(String, Vec<String>)> = Vec::new();
    let mut uefi_tests: Vec<(String, String, Vec<String>)> = Vec::new();

    if let Some(plan_text) = plan {
        for (lineno, line) in plan_text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("std") => {
                    if let Some(pkg) = parts.next() {
                        std_crates.push((pkg.to_string(), Vec::new()));
                    } else {
                        eprintln!("xtest.txt:{}: missing package after 'std'", lineno + 1);
                    }
                }
                Some("uefi") => {
                    let (pkg, testname) = (parts.next(), parts.next());
                    match (pkg, testname) {
                        (Some(p), Some(t)) => {
                            uefi_tests.push((p.to_string(), t.to_string(), Vec::new()))
                        }
                        _ => eprintln!(
                            "xtest.txt:{}: expected: uefi <package> <testname>",
                            lineno + 1
                        ),
                    }
                }
                Some(other) => {
                    eprintln!(
                        "xtest.txt:{}: unknown kind '{}'; expected 'std' or 'uefi'",
                        lineno + 1,
                        other
                    );
                }
                None => {}
            }
        }
    } else {
        // Default: run std tests for all members except xtask and block-device; then run UEFI test for block-device
        let mut members = get_workspace_members().expect("Failed to get workspace members");
        members.retain(|n| n != "xtask");
        for name in members {
            if name == "block-device" {
                continue;
            }
            std_crates.push((name, Vec::new()));
        }
        uefi_tests.push((
            "block-device".to_string(),
            "virtio_blk_modern".to_string(),
            Vec::new(),
        ));
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

    // Accumulate results across all tests
    let mut passed: Vec<String> = Vec::new();
    let mut failed: Vec<(String, i32)> = Vec::new();

    // Run std tests (each with 30s timeout if available)
    for (pkg, extra) in std_crates {
        eprintln!("\n--- Running host tests for: {} ---", pkg);
        let mut cmd = if let Some(mut prefix) = timeout_prefix(30) {
            let mut c = Command::new(&prefix.remove(0));
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
            .args(args)
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

    // Run UEFI tests (rely on runner's internal timeout)
    let runner_path = repo_root.join("file/block-device/scripts/run_qemu.sh");
    let runner = runner_path
        .to_str()
        .expect("runner path contains invalid UTF-8");
    for (pkg, testname, extra) in uefi_tests {
        eprintln!(
            "\n--- Running UEFI test for: {}::{}, runner: {} ---",
            pkg, testname, runner
        );
        let mut cmd = if let Some(mut prefix) = timeout_prefix(30) {
            let mut c = Command::new(&prefix.remove(0));
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
            .arg("aarch64-unknown-uefi")
            .arg("-p")
            .arg(&pkg)
            .arg("--test")
            .arg(&testname)
            .args(&extra)
            .args(args)
            .env("CARGO_TARGET_AARCH64_UNKNOWN_UEFI_RUNNER", runner)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        eprintln!("Running: {:?}", cmd);
        let status = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to spawn cargo test (UEFI) for {}: {}", pkg, e))
            .wait()
            .unwrap_or_else(|e| panic!("Failed to wait for cargo test (UEFI) for {}: {}", pkg, e));
        let label = format!("uefi:{}::{}", pkg, testname);
        if status.success() {
            passed.push(label);
        } else {
            let code = status.code().unwrap_or(1);
            eprintln!("Error: UEFI test failed for {} with code {}", pkg, code);
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
        eprintln!("All tests passed (host + UEFI)");
    }
}

/// `cargo metadata` を実行し、ワークスペースのメンバーの名前を Vec<String> で返します。
fn get_workspace_members() -> Result<Vec<String>, String> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--no-deps") // 依存関係は不要なので出力サイズを削減
        .arg("--format-version")
        .arg("1") // メタデータフォーマットのバージョン指定
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn cargo metadata: {}", e))?
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for cargo metadata: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed with status: {:?}\nStderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse cargo metadata JSON: {}", e))?;

    let mut member_names = Vec::new();
    for member_id in metadata.workspace_members {
        if let Some(pkg) = metadata.packages.iter().find(|p| p.id == member_id) {
            member_names.push(pkg.name.clone());
        }
    }
    // xtask自身をリストから除外する
    member_names.retain(|name| name != "xtask");
    Ok(member_names)
}
