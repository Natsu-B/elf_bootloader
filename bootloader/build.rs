use core::panic;
use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=aarch64.lds");
    println!("cargo:rerun-if-env-changed=XTASK_BUILD");

    println!("cargo:rustc-link-search={}", crate_dir);
    println!("cargo:rustc-link-arg=-Taarch64.lds");

    // xtaskから呼ばれているかのチェック
    if std::env::var("XTASK_BUILD").is_err() {
        panic!("
            Do not use `cargo build` directly.\n            Instead, run `cargo xtask build` or `cargo xbuild` from the workspace root.
            ");
    }
}
