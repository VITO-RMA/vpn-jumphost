{
  description = "VPN jumphost: OpenConnect+ocproxy supervisor, routing SOCKS5 proxy, PAC server, and F5 cookie management";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};

          jumphost = pkgs.rustPlatform.buildRustPackage {
            pname = "vpn-jumphost";
            version = "0.3.0";

            src =
              let
                fs = pkgs.lib.fileset;
              in
              fs.toSource {
                root = ./.;
                fileset = fs.unions [
                  ./Cargo.toml
                  ./Cargo.lock
                  ./src
                ];
              };

            cargoLock.lockFile = ./Cargo.lock;

            # The repo's .cargo/config.toml assumes mold (Linux) / lld
            # (macOS) are on PATH for faster linking during development.
            # In the Nix build sandbox the default linker from stdenv
            # works fine — remove the overrides so the build doesn't
            # fail looking for mold/lld.
            postPatch = ''
              rm -f .cargo/config.toml
            '';

            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.installShellFiles
              pkgs.pkg-config
            ];

            buildInputs = [
              # Runtime dependencies — the supervisor spawns
              # openconnect with --script-tun, which in turn exec's
              # ocproxy. Cookie refresh drives Chromium via CDP.
              pkgs.openconnect
              pkgs.ocproxy
              pkgs.dbus
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              pkgs.chromium
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin (
              with pkgs.darwin.apple_sdk.frameworks;
              [
                AppKit
                Foundation
                Security
                SystemConfiguration
              ]
            );

            # Put runtime dependencies on PATH so the supervisor can
            # find them. On macOS, Chromium is not available from
            # nixpkgs — install Chrome or Chromium system-wide and set
            # CHROMIUM_PATH (or let chromiumoxide auto-detect it).
            postInstall =
              let
                runtimeDeps = [
                  pkgs.openconnect
                  pkgs.ocproxy
                ]
                ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
                  pkgs.chromium
                ];
              in
              ''
                installShellCompletion --cmd jumphost \
                  --bash <($out/bin/jumphost generate-completions bash) \
                  --zsh <($out/bin/jumphost generate-completions zsh) \
                  --fish <($out/bin/jumphost generate-completions fish)

                wrapProgram $out/bin/jumphost \
                  --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
              '';

            meta = {
              description = "VPN jumphost: OpenConnect+ocproxy supervisor with routing SOCKS5 proxy, PAC server, and cookie management";
              mainProgram = "jumphost";
              platforms = pkgs.lib.platforms.linux ++ pkgs.lib.platforms.darwin;
            };
          };
        in
        {
          jumphost = jumphost;
          default = jumphost;
        }
      );

      overlays.default = final: prev: {
        vpn-jumphost = self.packages.${final.stdenv.hostPlatform.system}.jumphost;
      };
    };
}
