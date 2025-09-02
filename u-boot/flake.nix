{
  description = "A development environment for building U-Boot for aarch64";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      hostSystem = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${hostSystem};
      pkgsCross = pkgs.pkgsCross.aarch64-multiplatform;
    in
    {
      devShells.${hostSystem}.default = pkgs.mkShell {
        nativeBuildInputs = [
          pkgsCross.stdenv.cc

          pkgs.gnutls
          pkgs.openssl
        ];

        # U-Bootのビルドシステムにクロスコンパイラの場所を教えるための環境変数
        shellHook = ''
          export CROSS_COMPILE="aarch64-unknown-linux-gnu-"
          export ARCH="arm64"
        '';
      };
    };
}