use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=aarch64.lds");
    println!("cargo:rerun-if-env-changed=XTASK_BUILD");

    println!("cargo:rustc-link-search={}", crate_dir);
    println!("cargo:rustc-link-arg=-Taarch64.lds");
}
