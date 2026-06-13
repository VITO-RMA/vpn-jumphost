# VPN Jumphost

A VPN jumphost that connects to a VPN with `openconnect`, accepts a session cookie, and exposes a local SOCKS5 proxy on loopback so host applications can selectively route traffic through the VPN session. A PAC file generator and a small PAC HTTP server are included so browsers can decide which traffic goes through the jumphost. A routing SOCKS5 proxy sits in front of ocproxy and applies per-domain routing rules, so any SOCKS5-capable tool (git, curl, SSH, etc.) can use a single proxy address (`socks5h://127.0.0.1:1081`) without needing PAC support.

Traffic is selective and proxy-based: you keep normal host networking and only send chosen domains or IP ranges through the jumphost.

**Everything runs as your normal user.** `openconnect` is launched with `--script-tun --script "ocproxy ..."`, which means it does **not** create a kernel TUN device. Instead, openconnect spawns [ocproxy](https://github.com/cernekee/ocproxy) as its tunnel peer, hands it the VPN's IP packets over a socketpair, and lets ocproxy's userspace TCP/IP stack (lwIP) serve SOCKS5 on loopback. The `jumphost` binary's routing proxy sits in front on port 1081 and ocproxy listens on port 1080. No `sudo`, no namespace, no `/dev/net/tun` access required.

**Single binary.** The entire supervisor — cookie validation/refresh, openconnect lifecycle, routing SOCKS5 proxy, PAC HTTP server, sleep/wake detection — is one binary for easy deployment.

**Platforms:** Linux and macOS. Both are supported by the codebase; Linux is the primary daily-driver target and gets the most testing. The design has no Linux-specific dependencies (no `/dev/net/tun`, no namespace, no `sudo`), and both `openconnect` and `ocproxy` are available from nixpkgs on both platforms.

**No Docker required.** All services (`openconnect`, `ocproxy`, the routing proxy, the PAC HTTP server) run as native processes managed by the `jumphost` binary.

## Prerequisites

- **devenv / nix**: Installed per [devenv.sh/getting-started](https://devenv.sh/getting-started/). Provides `openconnect`, `ocproxy`, `just`, and the Rust toolchain. On Linux, also provides `chromium`; on macOS, install Google Chrome or Chromium system-wide (it is not available from nixpkgs). |

## Initial setup

#### Config file
> **First-time setup:** Copy [`docs/config.example.toml`](docs/config.example.toml) to `~/.config/vpn-jumphost/config.toml` and adjust the values for your VPN endpoint before running `just bootstrap`.

From inside the project directory:

```bash
devenv shell        # loads openconnect, ocproxy, just, rust (+ chromium on Linux)
```

## Starting and stopping
When the bootstrap has run, you can use `just start` to start everything in the foreground (Ctrl-C to stop).

`just start` runs `jumphost -c docs/config.example.toml run --serve-pac`. The routing proxy is always started on `127.0.0.1:1081` and the PAC server is bound to `127.0.0.1:8091`.

To run in the background use `just start-detached` — the daemon logs to `~/.local/state/vpn-jumphost/jumphost.log`. To stop the daemon, use `just stop` (sends SIGTERM to the PID in `~/.local/state/vpn-jumphost/jumphost.pid`).

## Automatic credentials
Credentials can be configured via environment variables (`VPN_USERNAME` / `VPN_PASSWORD`), the OS keyring (macOS Keychain / Linux Secret Service), or a TOML config file at `~/.config/vpn-jumphost/config.toml`. All other settings are configured via the config file or CLI flags.

To store credentials in the OS keyring, run `jumphost authenticate` — it prompts for your username and password and saves them in the platform's native credential store. Use `jumphost authenticate --delete` to remove them.

Create a `.env` file in the project root (already in `.gitignore`):

```bash
# .env
VPN_USERNAME=your.email@example.com
VPN_PASSWORD=your_password
```

Environment variables take precedence over the OS keyring, which takes precedence over the config file `[credentials]` table. The browser automation fills in the email/password automatically; you only need to confirm the MFA prompt.

## Other recipes

See `just --list` for the full list. The main ones are:

- `build` — `cargo build --release`
- `fetch-cookie` — `jumphost fetch-cookie` (Chromium SSO capture)
- `validate-cookie` — `jumphost validate-cookie` (exit 0/1/2)
- `pac-gen` — write `proxy.pac` to disk via `jumphost generate-pac`
- `pac-show` — print PAC to stdout
- `start` — `jumphost -c docs/config.example.toml run --serve-pac` in the foreground
- `start-detached` — same, but via `nohup`; writes pid/log under `$XDG_STATE_HOME/vpn-jumphost/`
- `stop` — SIGTERM the detached PID
- `test` — `cargo test --release`
- `test-curl [URL]` — curl `URL` via `socks5h://127.0.0.1:1081`; prints response headers + body and a status/timing summary
- `test-cluster [USER@HOST]` — SSH (BatchMode) to `HOST` through the SOCKS5 proxy and run a short remote command; smoke-tests SSH connectivity over the VPN

## Running as a systemd user service

For a daily-driver setup that survives logout/reboot and laptop sleep/wake without `devenv up`, the project ships a ready-to-customize unit at [`contrib/vpn-jumphost.service.example`](contrib/vpn-jumphost.service.example). It runs `target/release/jumphost run --serve-pac` directly: the binary validates and refreshes the F5 cookie, launches `openconnect` (which spawns `ocproxy`), serves the PAC file, re-checks the cookie at a configurable interval, and detects suspend/resume (logind `PrepareForSleep` on Linux, `NSWorkspaceDidWakeNotification` on macOS, with a wall-clock skew fallback) so the tunnel comes back automatically after the laptop wakes up.

Logging goes to stderr in a journald-friendly format.

```bash
just build                              # produce target/release/jumphost
mkdir -p ~/.config/systemd/user
cp contrib/vpn-jumphost.service.example ~/.config/systemd/user/vpn-jumphost.service
# Edit the /CHANGE/ME paths in the unit file, then:
systemctl --user daemon-reload
systemctl --user enable --now vpn-jumphost.service
journalctl --user -u vpn-jumphost.service -f
```

## macOS installer package

The [`contrib/macos/`](contrib/macos/) directory contains a macOS `.pkg` installer that:

- Installs the `jumphost` binary to `/usr/local/bin/jumphost`
- Installs a minimal `Jumphost.app` bundle in `/Applications/` so macOS Notification Center delivers MFA notifications under the "Jumphost" name (rather than being silently dropped)
- Installs a launchd user agent (`sas.vpn-jumphost`) that auto-starts the supervisor at login
- Copies the example config to `~/.config/vpn-jumphost/config.toml` on first install

```bash
cd contrib/macos
just build-package                      # build binary + assemble .pkg
just install                            # install (requires sudo)
jumphost test-notification              # verify notifications work
```

After installing, go to **System Settings → Notifications → Jumphost** and set the alert style to **Banners** or **Alerts**.

Manage the launchd agent:

```bash
launchctl kickstart -k gui/$(id -u)/sas.vpn-jumphost   # restart
launchctl kill SIGTERM gui/$(id -u)/sas.vpn-jumphost   # stop
tail -f /tmp/vpn-jumphost.log                          # logs
```

Uninstall with `cd contrib/macos && just uninstall`.

## Using the Nix flake

The project includes a `flake.nix` that builds the `jumphost` binary as a standalone Nix package with all runtime dependencies (`openconnect`, `ocproxy`, and `chromium` on Linux) wrapped on `PATH`. This is the recommended way to integrate the jumphost into a NixOS or home-manager configuration.

**Overlay usage** — add the overlay to your nixpkgs and use `pkgs.vpn-jumphost`:

```nix
# flake.nix (NixOS or home-manager)
{
  inputs.vpn-jumphost.url = "github:USER/REPO";

  # ...

  nixpkgs.overlays = [ vpn-jumphost.overlays.default ];

  # NixOS
  environment.systemPackages = [ pkgs.vpn-jumphost ];

  # or home-manager
  home.packages = [ pkgs.vpn-jumphost ];
}
```

**Direct package reference** — skip the overlay and reference the package directly:

```nix
environment.systemPackages = [
  vpn-jumphost.packages.${system}.default
];
```

**Quick try:**

```bash
nix run github:USER/REPO
```

**macOS note:** Chromium is not available from nixpkgs on macOS. Install Chrome or Chromium system-wide and set `chromium_path` in `config.toml`, or let `chromiumoxide` auto-detect it.

**systemd integration:** The flake pairs well with the systemd service example at [`contrib/vpn-jumphost.service.example`](contrib/vpn-jumphost.service.example). Set `ExecStart` to `${pkgs.vpn-jumphost}/bin/jumphost run --serve-pac` (or the equivalent absolute store path) and the unit will use the wrapped binary with `openconnect` and `ocproxy` already on `PATH`.

## Documentation

- [**Functional specification**](spec.md) — full system spec: architecture, all features, workflows, configuration reference, open questions
- [Architecture (BYOD, F5, OpenConnect, ocproxy, devenv, PAC)](docs/architecture.md)
- [Running the services with devenv](docs/run.md)
- [PAC files and local proxies](docs/pac.md)
- [SSH via `ProxyCommand` and OpenSSH config](docs/ssh.md)
- [Arch Linux package](docs/arch.md)
- [Debian package](docs/debian.md)

**Mermaid diagrams:** Several docs use fenced `mermaid` code blocks. [GitHub renders them](https://github.blog/changelog/2022-02-14-add-new-mermaid-diagrams-and-markdown-expansions-to-gists/) when you view Markdown in the browser. For **Cursor / VS Code**, open the Markdown preview and install the workspace-recommended **Markdown Preview Mermaid Support** extension (see [`.vscode/extensions.json`](.vscode/extensions.json)).

## Ports

When the jumphost is running, these ports are bound to `127.0.0.1`:

| Port | Protocol | Service |
| ---- | -------- | ------- |
| 1080 | SOCKS5   | ocproxy (VPN tunnel endpoint, upstream of routing proxy) |
| 1081 | SOCKS5   | routing proxy (user-facing; per-domain VPN-vs-direct routing) |
| 8091 | HTTP     | PAC file served by the in-process tokio/hyper server |

Use `socks5h://127.0.0.1:1081` for all SOCKS5 clients. The routing proxy handles per-domain routing: VPN-domain traffic goes through the tunnel and everything else goes direct.

## Cookie methods

The `jumphost` binary reads the F5 session cookie from a file:

- **`cookie_file`** — configurable in `config.toml`. Default `~/.local/state/vpn-jumphost/cookie` (mode 600).

On startup, `jumphost run` calls `jumphost validate-cookie` against the VPN endpoint. If the cookie is expired, invalid, or the file is missing, `jumphost fetch-cookie` is invoked automatically: a Chromium window opens for SSO + MFA and the captured `MRHSession` cookie is written back to the cookie file. The same validate/refresh cycle runs periodically while the supervisor is up (interval controlled by `check_interval` in config.toml or `--check-interval`, default 300 s).

## Automatic login credentials

For fully automated browser login (when using `just bootstrap` or `just fetch-cookie`), provide your VPN credentials via environment variables or the config file:

- **VPN_USERNAME** — Your VPN email address
- **VPN_PASSWORD** — Your VPN password

**Recommended setup:** create a `.env` file in the project root:

```bash
# .env
VPN_USERNAME=your.email@example.com
VPN_PASSWORD=your_secure_password
```

The `.env` file is already in `.gitignore`. The `jumphost` binary loads `.env` from the current working directory automatically at startup (via the `dotenvy` crate), so it works whether you use `just start`, `devenv up`, or invoke the binary directly. When credentials are provided, the browser automation will:

1. Navigate to the configured VPN URL
2. Automatically fill in your email and password (or click the matching tile when Microsoft shows the "Pick an account" picker)
3. Wait for you to complete the MFA step
4. Capture the session cookie and save it to the cookie file

### File-based credentials

As an alternative to environment variables or direct config values, credentials can be read from files via the config file. This is useful for secret-mounting workflows (e.g. Docker/Podman secrets, systemd `LoadCredential=`, or Kubernetes volume mounts):

```toml
# config.toml
[credentials]
username_file = "/var/run/secrets/vpn_user"
password_file = "/var/run/secrets/vpn_pass"
```

**Precedence:** `VPN_USERNAME` / `VPN_PASSWORD` env vars > OS keyring > config file `username_file` / `password_file`. Each source must supply both username and password; sources are never mixed.

### OS keyring

Credentials can be stored securely in the OS keyring (macOS Keychain / Linux Secret Service) using the `authenticate` subcommand:

```bash
jumphost authenticate
```

This prompts for your VPN username and password and stores them in the platform's native credential store. To remove stored credentials:

```bash
jumphost authenticate --delete
```

Keyring credentials sit between environment variables and the config file in the precedence chain. This is the recommended approach for single-user workstations — credentials are encrypted at rest by the OS and do not need to be written to plain-text files.

### Persistent browser session reuse

`just fetch-cookie` and the bootstrap login step drive Chromium (via the `chromiumoxide` crate and the Chrome DevTools Protocol) against a persistent user-data directory, so SSO/session state can be reused between runs.

- Default profile path: `${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/chromium-profile` (fixed; not configurable)
- Override the Chromium binary: set `chromium_path` in `config.toml` or use the `--chromium` CLI flag. Inside `devenv shell` on Linux, `CHROMIUM_PATH` is exported automatically to the nixpkgs `chromium` (picked up by `devenv.nix`). On macOS, `chromiumoxide` auto-detects a system-installed Chrome or Chromium.

**Security note:** Credentials are only used for the initial browser login automation. They are not embedded in the cookie file or used beyond authentication.

See [Running the services with devenv](docs/run.md) for ways to start the services without the wizard.

## What this solves

Transparent host-level routing for a corporate VPN with domain-based rules is awkward to set up on a personal workstation. A proxy jumphost is the practical approach: the routing proxy on port 1081 handles per-domain routing automatically, so all tools — browsers, git, curl, SSH — can use a single `socks5h://127.0.0.1:1081` proxy. By running openconnect with `--script-tun` and ocproxy's userspace TCP/IP stack, the entire tunnel exists in user space — no kernel TUN, no namespace, no `sudo` — and only host processes that explicitly point at `127.0.0.1:1081` can reach VPN-side resources. Everything else is unaffected.

If you need full non-proxy-aware packet routing for arbitrary local processes, that is a different setup (host firewall or system-level VPN client outside this repo).
