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
    // ワークスペースのメンバーを取得（xtaskは除外済み）
    let build_crate_names = match get_workspace_members() {
        Ok(names) => names,
        Err(e) => {
            eprintln!("Error getting workspace members: {}", e);
            std::process::exit(1);
        }
    };

    if build_crate_names.is_empty() {
        eprintln!("Warning: No workspace members found to build (excluding xtask).");
        return Err("no workspace members found");
    }

    eprintln!("Found workspace members: {:?}", build_crate_names);

    // 各パッケージに対してコマンドを実行
    for name in &build_crate_names {
        eprintln!("\n--- Building for package: {} ---", name);
        let mut cmd = Command::new("cargo");
        cmd.arg("build")
            .arg("-p")
            .arg(name)
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
            .unwrap_or_else(|e| panic!("Failed to spawn cargo build for {}: {}", name, e))
            .wait()
            .unwrap_or_else(|e| panic!("Failed to wait for cargo build for {}: {}", name, e));

        if !status.success() {
            eprintln!(
                "Error: cargo build failed for package '{}' with status: {:?}",
                name, status
            );
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    eprintln!("\n--- All packages builded successfully! ---");
    eprintln!("\n--- Searching builded binary... ---");
    let mut binary_dir = std::env::current_dir().unwrap();
    binary_dir.push("target");
    binary_dir.push("aarch64-unknown-none");
    binary_dir.push("debug");
    binary_dir.push("elf-hypervisor");
    let mut binary_new_dir = std::env::current_dir().unwrap();
    binary_new_dir.push("bin");
    let _ = fs::create_dir(binary_new_dir.clone());
    binary_new_dir.push("elf-hypervisor.elf");
    std::fs::copy(binary_dir, binary_new_dir.clone()).expect("failed to move builded binary");
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
    // ホストのターゲットトリプルを取得
    let host_output = Command::new("rustc")
        .arg("--print")
        .arg("host-tuple")
        .output()
        .expect("Failed to run rustc --print host-tuple");
    let host_tuple = String::from_utf8(host_output.stdout)
        .expect("Invalid UTF-8 from rustc --print host-tuple")
        .trim() // 末尾の改行を除去
        .to_string();

    eprintln!("Detected host target: {}", host_tuple);

    // ワークスペースのメンバーを取得 (xtaskは除外済み)
    let mut test_crate_names = get_workspace_members()
        .expect("Failed to get workspace members. Make sure this is run within a Cargo workspace.");

    // test関数ではさらにtestが実装されていない関数を除去
    test_crate_names.retain(|name| name != "elf-hypervisor");

    if test_crate_names.is_empty() {
        eprintln!("No workspace members found to test.");
        return;
    }

    eprintln!("Found workspace members to test: {:?}", test_crate_names);

    // 各ワークスペースメンバーに対してテストを実行
    for name in test_crate_names.iter() {
        eprintln!("\n--- Running tests for package: {} ---", name);
        let mut cmd = Command::new("cargo");
        cmd.arg("test")
            .arg("--target")
            .arg(&host_tuple)
            .arg("-p")
            .arg(name)
            .args(args) // 残りの引数を cargo test に渡す
            .stdin(Stdio::null())
            .stdout(Stdio::inherit()) // テスト結果は表示させる
            .stderr(Stdio::inherit());

        eprintln!("Running: {:?}", cmd); // 実行コマンドを表示 (デバッグ用)

        let status = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to spawn cargo test for {}: {}", name, e))
            .wait()
            .unwrap_or_else(|e| panic!("Failed to wait for cargo test for {}: {}", name, e));

        if !status.success() {
            eprintln!("Error: Tests failed for package: {}", name);
            // 失敗した場合は直ちに終了
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    eprintln!("\n--- All workspace tests passed! ---");
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
