{
  description = "Eloquawk: a native GPUI Gmail client for Linux";

  nixConfig = {
    extra-substituters = [ "https://korora-tech.cachix.org" ];
    extra-trusted-public-keys = [
      "korora-tech.cachix.org-1:42bs8TVW3uvxvMnuJtkXP5lkHFK7++Sral9uRDautuA="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      buildInputs =
        pkgs: with pkgs; [
          fontconfig
          freetype
          libxcb
          libxkbcommon
          vulkan-loader
          vulkan-headers
          wayland
        ];

      # A fontconfig file for the wrapper: the system's and the user's own configuration, plus
      # Inter. GPUI's text system (fontdb) reads only fontconfig's directories, and when none of
      # the UI fonts in src/ui/theme.rs is installed every font weight renders regular, so
      # unread mail looks read. Inter guarantees one family with real weights.
      fontsConf =
        pkgs:
        pkgs.writeText "eloquawk-fonts.conf" ''
          <?xml version="1.0"?>
          <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
          <fontconfig>
            <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
            <include ignore_missing="yes" prefix="xdg">fontconfig/fonts.conf</include>
            <dir>${pkgs.inter}/share/fonts</dir>
          </fontconfig>
        '';

      # GPUI dlopens Vulkan, Wayland and xkbcommon at runtime.
      runtimeLibraryPath =
        pkgs:
        pkgs.lib.makeLibraryPath (
          with pkgs;
          [
            libxkbcommon
            vulkan-loader
            wayland
          ]
        );
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "eloquawk";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./LICENSE
              ./src
              ./data
            ];
          };

          # Git dependencies (zed's gpui) are fetched without pinned output hashes.
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          nativeBuildInputs = with pkgs; [
            makeWrapper
            pkg-config
          ];
          buildInputs = buildInputs pkgs;

          # The date tests use named zones (Europe/Berlin); the build sandbox has no tzdb.
          nativeCheckInputs = [ pkgs.tzdata ];
          preCheck = ''
            export TZDIR=${pkgs.tzdata}/share/zoneinfo
          '';

          postInstall = ''
            install -Dm644 LICENSE $out/share/licenses/eloquawk/LICENSE
            install -Dm644 data/korora.tech.Eloquawk.desktop $out/share/applications/korora.tech.Eloquawk.desktop
            install -Dm644 data/branding/eloquawk-app-icon.svg $out/share/icons/hicolor/scalable/apps/korora.tech.Eloquawk.svg
            install -Dm644 data/branding/eloquawk-app-icon.png $out/share/icons/hicolor/512x512/apps/korora.tech.Eloquawk.png
          '';

          # A FONTCONFIG_FILE the user already set wins over the bundled one.
          postFixup = ''
            wrapProgram $out/bin/eloquawk \
              --prefix LD_LIBRARY_PATH : ${runtimeLibraryPath pkgs} \
              --set-default FONTCONFIG_FILE ${fontsConf pkgs}
          '';

          meta = {
            description = "A native GPUI Gmail client for Linux";
            license = pkgs.lib.licenses.asl20;
            mainProgram = "eloquawk";
            platforms = systems;
          };
        };
      });

      homeModules = {
        default = self.homeModules.eloquawk;
        eloquawk = import ./nix/hm-module.nix self;
      };
      # The older output name some Home Manager setups still look for. `nix flake check` warns
      # that it is an unknown output; the warning is expected and harmless.
      homeManagerModules = self.homeModules;

      # Evaluates the Home Manager module against stub options, and checks the package's
      # desktop entry and icons.
      checks = forAllSystems (
        pkgs:
        let
          inherit (nixpkgs) lib;
          system = pkgs.stdenv.hostPlatform.system;
          package = self.packages.${system}.default;
          hmStub = {
            options.home.packages = lib.mkOption {
              type = lib.types.listOf lib.types.package;
              default = [ ];
            };
          };
          eval =
            settings:
            (lib.evalModules {
              modules = [
                hmStub
                self.homeManagerModules.default
                {
                  _module.args.pkgs = pkgs;
                  programs.eloquawk = settings;
                }
              ];
            }).config;
          custom = pkgs.writeShellScriptBin "eloquawk" "";
          checks = [
            (lib.assertMsg ((eval { enable = true; }).home.packages == [ package ]) "installs the package")
            (lib.assertMsg ((eval { }).home.packages == [ ]) "does nothing when disabled")
            (lib.assertMsg (
              (eval {
                enable = true;
                package = custom;
              }).home.packages == [ custom ]
            ) "honours package")
          ];
        in
        {
          hm-module = pkgs.runCommand "eloquawk-hm-module-check" { } ''
            echo ${toString (lib.all lib.id checks)} > $out
          '';

          desktop-entry =
            pkgs.runCommand "eloquawk-desktop-entry-check" { nativeBuildInputs = [ pkgs.desktop-file-utils ]; }
              ''
                desktop-file-validate ${package}/share/applications/korora.tech.Eloquawk.desktop
                test -s ${package}/share/icons/hicolor/scalable/apps/korora.tech.Eloquawk.svg
                test -s ${package}/share/icons/hicolor/512x512/apps/korora.tech.Eloquawk.png
                touch $out
              '';
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = nixpkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            cargo
            clippy
            jq
            libwebp
            rust-analyzer
            rustc
            rustfmt
          ];
          LD_LIBRARY_PATH = runtimeLibraryPath pkgs;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
