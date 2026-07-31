{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;

      # Everything sshot shells out to at runtime.
      runtimeDeps = pkgs: with pkgs; [ grim wl-clipboard xdg-utils libnotify ];
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "sshot";
          inherit version;
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [ pkg-config makeWrapper ];
          buildInputs = with pkgs; [ wayland wayland-protocols libxkbcommon ];

          postInstall = ''
            wrapProgram $out/bin/sshot \
              --prefix PATH : ${pkgs.lib.makeBinPath (runtimeDeps pkgs)}
          '';

          meta = {
            description = "Wayland screenshot utility with a system tray, tuned for niri";
            mainProgram = "sshot";
            platforms = systems;
          };
        };
      });

      # `nix develop`. Plain `cargo build` fails without xkbcommon on
      # PKG_CONFIG_PATH, and clippy has to come from the same toolchain as
      # rustc or every dependency reads as compiled by the wrong compiler.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
          ] ++ runtimeDeps pkgs;
          buildInputs = with pkgs; [ wayland wayland-protocols libxkbcommon ];
        };
      });

      checks = forAllSystems (pkgs: {
        build = self.packages.${pkgs.system}.default;
      });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);

      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.sshot;
          pkg = self.packages.${pkgs.system}.default;
          jsonFormat = pkgs.formats.json {};
          configFile = jsonFormat.generate "config.json" {
            save = {
              directory = cfg.saveDirectory;
              subfolder = cfg.subfolder;
              window_format = cfg.windowFormat;
              region_format = cfg.regionFormat;
              file_manager = cfg.fileManager;
              file_manager_select_arg = cfg.fileManagerSelectArg;
            };
            appearance = {
              dim_factor = cfg.dimFactor;
              border_width = cfg.borderWidth;
              window_border_color = cfg.windowBorderColor;
              region_border_color = cfg.regionBorderColor;
              tiling_gap = cfg.tilingGap;
            };
          };
          colorType = lib.types.addCheck (lib.types.listOf (lib.types.ints.between 0 255))
            (l: builtins.length l == 3) // { description = "list of 3 integers in [0, 255]"; };
        in
        {
          options.programs.sshot = {
            enable = lib.mkEnableOption "sshot screenshot tool";

            package = lib.mkOption {
              type = lib.types.package;
              default = pkg;
              description = "The sshot package to use.";
            };

            service = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = ''
                Run the tray daemon as a systemd user service. The service waits
                for graphical-session.target, and the tray registration retries
                on its own, so it does not need the session's tray host to be up
                first.
              '';
            };

            saveDirectory = lib.mkOption {
              type = lib.types.str;
              default = "${config.home.homeDirectory}/Pictures/Screenshots";
              description = "Base directory for screenshots";
            };

            subfolder = lib.mkOption {
              type = lib.types.str;
              default = "%Y-%m";
              description = "Subfolder format (strftime)";
            };

            windowFormat = lib.mkOption {
              type = lib.types.str;
              default = "{title} %Y-%m-%d-%H-%M-%S-{random}";
              description = "Filename format for window captures. Variables: {title}, {random}, and strftime.";
            };

            regionFormat = lib.mkOption {
              type = lib.types.str;
              default = "%Y-%m-%d-%H-%M-%S-{random}";
              description = "Filename format for region captures. Variables: {random}, and strftime.";
            };

            fileManager = lib.mkOption {
              type = lib.types.str;
              default = "xdg-open";
              example = "nautilus";
              description = "File manager used to reveal a capture from the tray.";
            };

            fileManagerSelectArg = lib.mkOption {
              type = lib.types.str;
              default = "";
              example = "--select";
              description = ''
                Argument that makes the file manager select the file rather than
                just open its directory. Empty opens the directory.
              '';
            };

            dimFactor = lib.mkOption {
              # Accept an integer literal such as `1` as well as `1.0`.
              type = lib.types.either lib.types.float lib.types.int;
              default = 0.75;
              description = "Dim factor for non-highlighted areas (0.0 = black, 1.0 = none)";
            };

            borderWidth = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 3;
              description = "Highlight border width in pixels";
            };

            windowBorderColor = lib.mkOption {
              type = colorType;
              default = [ 80 140 255 ];
              description = "Window highlight border color [R G B]";
            };

            regionBorderColor = lib.mkOption {
              type = colorType;
              default = [ 255 255 255 ];
              description = "Region selection border color [R G B]";
            };

            tilingGap = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 16;
              description = ''
                Fallback tiling gap, used only when niri's own config cannot be
                read. sshot reads `layout { gaps }` out of niri's config, so
                this normally does not need setting.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            xdg.configFile."sshot/config.json" = {
              source = configFile;
              force = true;
            };

            systemd.user.services.sshot = lib.mkIf cfg.service {
              Unit = {
                Description = "sshot screenshot daemon";
                PartOf = [ "graphical-session.target" ];
                After = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart = "${lib.getExe cfg.package} --daemon";
                Restart = "on-failure";
                RestartSec = 2;
              };
              Install.WantedBy = [ "graphical-session.target" ];
            };
          };
        };
    };
}
