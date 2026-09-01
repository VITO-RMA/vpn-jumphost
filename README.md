# VPN Jumphost

A userspace VPN proxy that connects to an F5 VPN via `openconnect` + `ocproxy` and exposes a local SOCKS5 proxy. Traffic is routed per-domain: VPN-bound domains go through the tunnel, everything else connects directly. No `sudo`, no kernel TUN device, no Docker — everything runs as your normal user.

- Two socks5 proxies will be running:
  - `socks5h://127.0.0.1:1080` that routes all traffic through the VPN
  - `socks5h://127.0.0.1:1081` that routes per-domain based on the config (VPN for configured domains, direct for everything else)
- A proxy pac will be hosted at `http://127.0.0.1:8091`
  
## Installation
Build the packages from source or use the prebuilt release artifacts for ubuntu/mac
### devenv

Install devenv per [devenv.sh/getting-started](https://devenv.sh/getting-started/), then from the project directory:

```bash
devenv shell   # provides openconnect, ocproxy, just, Rust toolchain, chromium (Linux)
just start
```

Normal development uses `cargo build --release` (or `just build`), producing `target/release/jumphost` without cross-crate LTO for fast relinks. Distribution packaging uses `cargo build --profile dist`, producing `target/dist/jumphost` with thin LTO. Both profiles use the platform's default linker.

On macOS, Chromium is not available from nixpkgs — install Google Chrome or Chromium system-wide.

### Arch Linux

Install `ocproxy` from the AUR with paru or yay, then build and install the package:

```bash
paru -S ocproxy-bin
cd contrib/archlinux
makepkg -si
```

See [docs/arch.md](docs/arch.md) for post-install setup and systemd service enablement.

### Ubuntu / Debian

Build and install the `.deb` package:

```bash
cd contrib/debian
just build-package
sudo dpkg -i .pkg-cache/vpn-jumphost_*.deb
sudo apt-get install -f   # resolve dependencies (openconnect, ocproxy, chromium)
```

See [docs/debian.md](docs/debian.md) for the full build steps and systemd service setup.

### macOS
prerequisites: install the required dependencies with Homebrew:
```bash
brew install openconnect ocproxy google-chrome
```

A `.pkg` installer is available in [`contrib/macos/`](contrib/macos/):

```bash
cd contrib/macos
just build-package
just install
```

This installs the binary to `/usr/local/bin/jumphost`, a `Jumphost.app` notification helper in `/Applications/`, and a launchd user agent that auto-starts at login.

## Running

Copy [`docs/config.example.toml`](docs/config.example.toml) to `~/.config/vpn-jumphost/config.toml` and set `vpn_url`, the `[domains]` table, and `serve_pac = true` if you want the PAC HTTP server.

Before the first run, store your VPN credentials in the OS keyring:

```bash
jumphost authenticate
```

Or, if `VPN_USERNAME` and `VPN_PASSWORD` are already set in the environment:

```bash
jumphost authenticate --from-env
```

This prompts for your username and password and saves them in the platform's native credential store (macOS Keychain / Linux Secret Service). To force a fresh VPN cookie later without entering or changing credentials, run `jumphost refresh_token`; if no complete credential source is available, it exits with a message directing you to `jumphost authenticate`. The browser-based cookie capture will use the configured credentials to pre-fill the SSO form. For Microsoft Authenticator number matching, approve the number shown in the browser or, for headless refreshes, in shell output and the desktop notification. The number-match code is also logged for `jumphost logs`. The dedicated authentication browser always connects directly and ignores system PAC/proxy settings and `[domains]` routing rules, so authentication never depends on an existing VPN tunnel.

Automated authentication sends at most three Microsoft Authenticator push notifications during one `jumphost run` process. If none is completed, automatic authentication pauses and the supervisor remains running without a VPN tunnel so systemd/launchd cannot restart it into another notification loop. When you are present, run `jumphost authenticate`; the supervisor detects the new valid cookie and starts the tunnel. Explicitly restarting the service also resets the three-attempt allowance.

If you do not have a supported keyring backend, you can set the `VPN_USERNAME` and `VPN_PASSWORD` environment variables instead of using the keyring or configure the `username_file` and `password_file` options in the config file to read authentication information from the file contents.

Then start the jumphost service:

## Linux
```bash
systemctl --user enable --now vpn-jumphost.service
```

## macOS
```bash
launchctl kickstart -k gui/$(id -u)/sas.vpn-jumphost
```

Verify the setup:

```bash
jumphost doctor
jumphost test-tunnel    # end-to-end SOCKS5 probe via :1081 (requires jumphost run)
jumphost logs -f        # follow service logs
```

`doctor` checks config, cookie, routing proxy (`:1081`), VPN tunnel (`:1080`), PAC server, and proxychains (for database clients). Exit 0 means all critical checks passed.

`test-tunnel` issues SOCKS5 `CONNECT` probes through the routing proxy — configure `[probe].hosts` in your config file (see `docs/config.example.toml`) or pass `-H host[:port]`. Use it after start to confirm the tunnel actually routes traffic, not just that listeners are up.

`logs` reads the systemd user journal when `vpn-jumphost.service` is installed or running, the `just start-detached` log at `${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/jumphost.log`, or the macOS launchd log at `/tmp/vpn-jumphost.log`. Use `--source systemd|detached|launchd` to choose explicitly.

## Point your tools at the proxy:

```bash
# curl (routing proxy — use for all apps; per-domain VPN routing)
curl --proxy socks5h://127.0.0.1:1081 https://internal.example.com

# git
git config --global http.proxy socks5h://127.0.0.1:1081

# SSH (see docs/ssh.md for persistent config)
ssh -o 'ProxyCommand=nc -x 127.0.0.1:1081 -X 5 %h %p' user@host

# PostgreSQL / DBeaver (see docs/databases.md)
just proxychains-setup && just dbeaver
just pc -- psql -h db.example.local -p 5432 -U user -d mydb
```

Configure your browser to use the proxy pac at `http://127.0.0.1:8091` for automatic per-domain proxying.
Or use the routing proxy directly at `socks5h://127.0.0.1:1081` for applications that do not support PAC files to let the jumphost handle all routing logic.

For GUI database clients (DBeaver, DataGrip), see [docs/databases.md](docs/databases.md).
