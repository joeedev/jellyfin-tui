{
  description = "A TUI for Jellyfin media server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            openssl
            mpv
          ];

          RUST_BACKTRACE = "1";
          PKG_CONFIG_PATH = "${pkgs.mpv}/lib/pkgconfig:$PKG_CONFIG_PATH";
        };

        packages.default = with pkgs;
          rustPlatform.buildRustPackage rec {
            pname = "jellyfin-tui";
            version = "1.3.2";

            src = ./.;

            cargoHash = "sha256-lmBk5UFb+NWjIaHvTeIzvQNdWeo5BOtmuajD3XpdBT4=";

            nativeBuildInputs = [pkg-config];
            buildInputs = [
              openssl
              mpv
            ];

            nativeInstallCheckInputs = [
              writableTmpDirAsHomeHook
              versionCheckHook
            ];
            versionCheckKeepEnvironment = ["HOME"];
            preInstallCheck = ''
              mkdir -p "$HOME/${
                if stdenv.buildPlatform.isDarwin
                then "Library/Application Support"
                else ".local/share"
              }"
            '';
            doInstallCheck = true;

            postInstall = lib.optionalString stdenv.hostPlatform.isLinux ''
              install -Dm644 src/extra/jellyfin-tui.desktop $out/share/applications/jellyfin-tui.desktop
            '';

            passthru.updateScript = nix-update-script {};

            meta = {
              description = "Jellyfin music streaming client for the terminal";
              mainProgram = "jellyfin-tui";
              homepage = "https://github.com/dhonus/jellyfin-tui";
              changelog = "https://github.com/dhonus/jellyfin-tui/releases/tag/v${version}";
              license = lib.licenses.gpl3Only;
              maintainers = with lib.maintainers; [GKHWB];
            };
          };
      }
    );
}
