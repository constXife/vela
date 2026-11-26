{
  description = "Vela — native COSMIC email client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = [
          "rustfmt"
          "clippy"
          "rust-src"
        ];
      };
      runtimeLibs = with pkgs; [
        libxkbcommon
        wayland
        expat
        fontconfig
        freetype
        libGL
        vulkan-loader
      ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages =
          with pkgs;
          [
            rustToolchain
            pkg-config
            just
            openssl
            wayland-protocols
            # Isolated headless Wayland UI testing sandbox
            sway
            grim
            wtype
            wlrctl
          ]
          ++ runtimeLibs;

        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
        OPENSSL_DIR = pkgs.openssl.dev;
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
      };
    };
}
