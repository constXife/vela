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
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
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
      vela = rustPlatform.buildRustPackage {
        pname = "vela";
        version = "0.1.0";
        src = pkgs.lib.cleanSource self;

        cargoLock = {
          lockFile = ./Cargo.lock;
          allowBuiltinFetchGit = true;
        };

        nativeBuildInputs = with pkgs; [
          pkg-config
          makeWrapper
        ];

        buildInputs =
          with pkgs;
          [
            openssl
            sqlcipher
            wayland-protocols
          ]
          ++ runtimeLibs;

        OPENSSL_DIR = pkgs.openssl.dev;
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";

        postInstall = ''
          install -Dm0644 resources/com.rlyeh.vela.Cosmic.desktop \
            $out/share/applications/com.rlyeh.vela.Cosmic.desktop
          install -Dm0644 resources/com.rlyeh.vela.Cosmic.metainfo.xml \
            $out/share/metainfo/com.rlyeh.vela.Cosmic.metainfo.xml
          install -Dm0644 resources/icons/hicolor/scalable/apps/com.rlyeh.vela.Cosmic.svg \
            $out/share/icons/hicolor/scalable/apps/com.rlyeh.vela.Cosmic.svg

          wrapProgram $out/bin/vela-cosmic \
            --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath runtimeLibs}"
        '';

        meta = with pkgs.lib; {
          description = "Lightweight native JMAP email client for the COSMIC desktop";
          homepage = "https://github.com/constXife/vela";
          license = licenses.mit;
          mainProgram = "vela-cosmic";
          platforms = platforms.linux;
        };
      };
    in
    {
      packages.${system} = {
        default = vela;
        inherit vela;
      };

      apps.${system} = {
        default = {
          type = "app";
          program = "${vela}/bin/vela-cosmic";
        };
        vela-mcp = {
          type = "app";
          program = "${vela}/bin/vela-mcp";
        };
      };

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
