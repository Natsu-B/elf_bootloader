{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            (pkgs.rust-bin.nightly.latest.default.override {
              targets = [
                "aarch64-unknown-none-softfloat"
                "aarch64-unknown-uefi"
                "x86_64-unknown-uefi"
              ];
              extensions = [ "rust-src" "llvm-tools-preview" ];
            })
            pkgs.qemu
            pkgs.OVMF.fd
            pkgs.dtc
            pkgs.cargo-binutils
            pkgs.gdb
          ];

          OVMF_CODE = "${pkgs.OVMF.fd}/FV/OVMF_CODE.fd";
          OVMF_VARS = "${pkgs.OVMF.fd}/FV/OVMF_VARS.fd";
        };
      }
    );
}
