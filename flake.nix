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
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (pkgsFor system));
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
      rustToolchainFor =
        pkgs:
        pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-src"
          ];
        };
      runtimeLibsFor =
        pkgs: with pkgs; [
          libxkbcommon
          wayland
          expat
          fontconfig
          freetype
          libGL
          vulkan-loader
        ];
      opensslEnvFor = pkgs: {
        OPENSSL_DIR = pkgs.openssl.dev;
        OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
        OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
      };

      mkVela =
        pkgs:
        let
          rustToolchain = rustToolchainFor pkgs;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
          runtimeLibs = runtimeLibsFor pkgs;
        in
        rustPlatform.buildRustPackage (
          {
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
              platforms = systems;
            };
          }
          // opensslEnvFor pkgs
        );
    in
    {
      packages = forAllSystems (pkgs: rec {
        vela = mkVela pkgs;
        default = vela;
      });

      # Adds `pkgs.vela` to a consumer's nixpkgs. The package itself is this
      # flake's pinned build (own nixpkgs + Rust toolchain), so it stays
      # reproducible and binary-cache friendly regardless of the consumer's
      # nixpkgs revision or rustc version.
      overlays.default = final: _prev: {
        vela = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      apps = forAllSystems (
        pkgs:
        let
          vela = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          default = {
            type = "app";
            program = "${vela}/bin/vela-cosmic";
          };
          vela-mcp = {
            type = "app";
            program = "${vela}/bin/vela-mcp";
          };
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell (
          {
            packages =
              with pkgs;
              [
                (rustToolchainFor pkgs)
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
              ++ runtimeLibsFor pkgs;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (runtimeLibsFor pkgs);
          }
          // opensslEnvFor pkgs
        );
      });
    };
}
