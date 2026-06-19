{
  pkgs,
  lib,
  ...
}:
{
  # https://devenv.sh/basics/
  env = lib.optionalAttrs pkgs.stdenv.isLinux {
    # Point chromiumoxide at the nix-provided Chromium so it doesn't
    # try to auto-download Chrome at runtime. chromiumoxide picks this up
    # automatically from the CHROMIUM_PATH env var.
    # On macOS, Chromium is not available from nixpkgs — require a
    # system-installed Chrome/Chromium instead (chromiumoxide auto-detects it).
    CHROMIUM_PATH = "${pkgs.chromium}/bin/chromium";
  };

  languages.rust.enable = true;

  packages =
    with pkgs;
    [
      just
      sd
      # VPN. openconnect feeds raw IP packets to ocproxy over a socketpair
      # (--script-tun), and ocproxy serves SOCKS5 from a userspace lwIP stack —
      # no kernel TUN, no namespace, no sudo.
      openconnect
      ocproxy
    ]
    ++ lib.optionals pkgs.stdenv.isLinux [
      pkgs.chromium
      pkgs.dbus
      pkgs.mold
    ]
    ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.lld ];

  # `devenv up` starts a single supervised `jumphost` process that owns
  # openconnect+ocproxy, the routing SOCKS5 proxy, and the PAC HTTP server.
  # `just build` must have been run at least once so the binary exists.
  # Use the example config shipped in docs/ so VPN URL + domain lists are
  # set without requiring a user-local config file.  Users who have their
  # own ~/.config/vpn-jumphost/config.toml can drop the -c flag.
  processes.jumphost.exec = "./target/release/jumphost -c docs/config.example.toml run";

  scripts.intro.exec = ''
    echo "❄️ VPN jumphost devenv shell"
    echo
    echo "Run \`just bootstrap\` for initial setup, \`just start\` to launch the supervisor."
    echo "The jumphost binary lives at \`target/release/jumphost\` (build with \`just build\`)."
  '';

  enterShell = ''
    intro
  '';
}
