{
  description = "VPN jumphost: OpenConnect+ocproxy supervisor, routing SOCKS5 proxy, PAC server, and F5 cookie management";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-26.11";
  };

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
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
            version = "0.4.0";

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

            # Published Nix packages use the thin-LTO distribution profile.
            cargoBuildType = "dist";

            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.installShellFiles
              pkgs.pkg-config
            ];

            buildInputs = [
              # The nixpkgs Cargo hook requires a packaged AWS-LC for aws-lc-sys.
              pkgs.aws-lc
              # Runtime dependencies — the supervisor spawns
              # openconnect with --script-tun, which in turn exec's
              # ocproxy. Cookie refresh drives Chromium via CDP.
              pkgs.openconnect
              pkgs.ocproxy
              pkgs.dbus
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.chromium
            ];

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
                ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
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
              platforms = supportedSystems;
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
