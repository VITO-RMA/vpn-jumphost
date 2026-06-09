# Agent Guidelines

## Documentation rule

Always update the documentation when changing the code. The files listed below form an interconnected set — a change to one component often requires updates to several of them.

## Functional specification

[`spec.md`](spec.md) is the authoritative functional specification for this system. It describes every component, feature, workflow, configuration variable, and known ambiguity. Read it before making non-trivial changes. Update it whenever you add, remove, or alter a feature.

## Documentation inventory

| File | Scope |
|---|---|
| [`spec.md`](spec.md) | Full functional specification: architecture, features, workflows, configuration, open questions |
| [`README.md`](README.md) | User-facing quick start, prerequisites, bootstrap flow, documentation index |
| [`docs/architecture.md`](docs/architecture.md) | System context diagrams, component roles, traffic paths |
| [`docs/config.example.toml`](docs/config.example.toml) | Example TOML config with VITO defaults; copy to `~/.config/vpn-jumphost/config.toml` |
| [`docs/run.md`](docs/run.md) | Running the services via `just start` / `devenv up`, manual usage, requirements/limitations |
| [`docs/pac.md`](docs/pac.md) | PAC generation, serving, desktop proxy setup |
| [`docs/ssh.md`](docs/ssh.md) | SSH `ProxyCommand` examples and `ssh-config` usage |

When changing code, check every file in this table and update any that reference the changed behavior. For example, adding a new environment variable requires updates to `spec.md` (Configuration section) and whichever `docs/` page covers that feature.

## Architecture overview (for context)

This is a devenv-managed VPN jumphost implemented as a **single Rust binary** (`target/release/jumphost`). **Linux and macOS are both supported** — the design has no Linux-specific dependencies (no `/dev/net/tun`, no namespace, no `sudo`), and both `openconnect` and `ocproxy` are available from nixpkgs on both platforms. Linux is the primary daily-driver target.

The whole runtime is two binaries (openconnect + ocproxy) plus the Rust supervisor that owns everything. openconnect is launched with `--script-tun --script "ocproxy ..."`:

- **OpenConnect** — F5 protocol VPN client. Spawned by `src/vpn.rs` via `tokio::process::Command` with stdin redirected from the cookie file. With `--script-tun`, openconnect does **not** open a TUN device; it spawns ocproxy as a child and exchanges raw IP packets with it over a socketpair (fd passed to ocproxy via the `VPNFD` env var).
- **ocproxy** — Userspace TCP/IP stack (lwIP) launched by openconnect as its `--script-tun` peer. Terminates the VPN's IP packets in user space and serves SOCKS5 on `127.0.0.1:1080`. Must **not** be passed `-g` (which would bind the SOCKS listener to all interfaces).
- **Routing proxy task** — In-process tokio task implemented by `src/routing.rs`. SOCKS5 server on `127.0.0.1:1081` (always started) that applies per-domain routing rules: VPN-domain traffic is forwarded upstream to ocproxy (port 1080), everything else connects directly. This is the user-facing proxy — clients point at `socks5h://127.0.0.1:1081`. Domain lists live in `src/config.rs` as `PROXY_DOMAINS` and `DIRECT_DOMAINS` and are shared with the PAC generator, so the two cannot drift.
- **PAC HTTP server task** — In-process tokio/hyper server implemented by `src/pac.rs`. Serves the generated PAC body on `127.0.0.1:8091` for any request path. Replaces the old `miniserve` + `pac-server-ctl.sh` combination.
- **Cookie management** — `src/cookie.rs`. Validates via `reqwest` (rustls, redirects **disabled**), refreshes via `chromiumoxide` driving Chromium through the Chrome DevTools Protocol. No Node.js / Playwright driver needed. Persistent user-data-dir at `$XDG_STATE_HOME/vpn-jumphost/chromium-profile` for SSO/MFA reuse.
- **Supervisor / monitor loop** — `src/jumphost.rs`. Owns the openconnect child, the routing-proxy task, the PAC-server task, and the periodic cookie check. Restarts the VPN on cookie refresh; restarts the VPN unconditionally after suspend/resume. After every VPN (re)start it spawns a **tunnel warmup task** (`warmup_after_start`) that retries SOCKS5 `CONNECT` to a configurable list of hosts (`WARMUP_HOSTS`, defaults to the VPN URL host + marvin cluster hosts) until each returns `REP_SUCCESS` — this defeats a race where the first user connection lands on a wrong endpoint before lwIP has installed the VPN's DNS/routes. Sleep/wake watcher is in `src/sleepwake/{mod,linux,macos}.rs` (zbus + logind on Linux; objc2 + block2 + `NSWorkspaceDidWakeNotification` on macOS) with a wall-clock-skew fallback.
- **CLI** — `src/main.rs`. Subcommands: `run` (default), `fetch-cookie`, `validate-cookie`, `generate-pac`, `serve-pac`. Global flags: `-c / --config FILE` (explicit config file path, overrides XDG default) and `-v / --verbose`. Top-level flags from `run` are accepted at the top level for backwards-compatible UX.

Process supervision:
- **`devenv up`** runs a single `processes.jumphost` process defined in `devenv.nix` whose `exec` is `./target/release/jumphost run --serve-pac`. There is no longer a separate `vpn` process and a separate `pac` process.
- **`just start`** runs the same command in the foreground without process-compose.
- **`just start-detached`** wraps the same command in `nohup` and writes PID/log files to `$XDG_STATE_HOME/vpn-jumphost/jumphost.{pid,log}`. The binary itself no longer daemonizes — there is no `-d/--daemonize` flag.
- A systemd user unit at [`contrib/vpn-jumphost.service.example`](contrib/vpn-jumphost.service.example) wires the binary as `Type=simple` with SIGTERM for clean shutdown.

The only remaining shell script is [`scripts/jumphost-wizard.sh`](scripts/jumphost-wizard.sh), the 3-step `just bootstrap` flow (generate PAC, print autoproxy instructions, capture cookie). It invokes the Rust binary for every step.

## Key constraints to preserve

- ocproxy is intentionally **never** invoked with `-g` (which would bind the SOCKS listener to all interfaces and expose the unauthenticated proxy on the LAN). Enforced in `src/vpn.rs::start`.
- The cookie validation probe in `src/cookie.rs` must **not** follow HTTP redirects (reqwest is built with `redirect::Policy::none()`). A 3xx redirect from the F5 gateway = expired cookie; following it would land on the SSO login page (HTTP 200) and falsely look valid.
- openconnect must be spawned with stdin redirected from the cookie file (no pipe) so `tokio::process::Command::spawn` opens the file directly. The supervisor sends SIGTERM via `nix::sys::signal::kill` so openconnect can run its normal teardown (which closes VPNFD and lets ocproxy exit).
- The routing proxy's `PROXY_DOMAINS` / `DIRECT_DOMAINS` constants and the PAC generator share the same constants in `src/config.rs` (`crate::config::{PROXY_DOMAINS, DIRECT_DOMAINS}`). There is only one source of truth — do not introduce parallel lists.
- The wizard's autoproxy-instructions step prints OS-appropriate guidance based on `uname -s` (`Darwin` → `networksetup` + System Settings; otherwise GNOME/KDE/Firefox). When adding more platforms or changing the instruction text, edit `print_autoproxy_instructions()` in `scripts/jumphost-wizard.sh` — do not hard-code Linux-only commands.
- Do **not** reintroduce Python scripts, `playwright`, `miniserve`, `tinyproxy`, or a separate "supervisor" wrapper. Clients should use the routing proxy at `socks5h://127.0.0.1:1081` (or `nc -X 5 -x 127.0.0.1:1081` for SSH `ProxyCommand`).
- Sleep/wake watchers are best-effort. Each module returns `None` from `spawn()` if the OS event source isn't reachable, and the supervisor's wall-clock skew fallback in `src/jumphost.rs::monitor_loop` catches suspend/resume on every platform.

## What to update for common changes

| Change | Files to update |
|---|---|
| New environment variable | Environment variables are not used for configuration (except `VPN_USERNAME` / `VPN_PASSWORD`). Use config file fields or CLI flags instead. |
| New config file option | `src/config_file.rs` (struct field + TOML table), `src/config.rs` (lookup helper mapping), `spec.md` (Config File table), `README.md` (if user-visible) |
| New port | The module that binds it (`src/vpn.rs`, `src/routing.rs`, `src/pac.rs`, …), `src/config.rs` (constant), `src/config_file.rs` (TOML field), `spec.md` (Port Allocation table + Config File table), `docs/run.md`, `docs/architecture.md`, `README.md` (Ports table), `docs/config.example.toml` (if it should ship a default) |
| New `just` recipe | `justfile`, `README.md` (recipes list in Quick start), `spec.md` (Task Runner table) |
| PAC logic change | `src/pac.rs` (and `src/config.rs` for the domain constants), `spec.md` (PAC Generation feature + Routing Proxy feature), `docs/pac.md` |
| Routing proxy logic change | `src/routing.rs`, plus `src/config.rs` if the domain constants change. `spec.md` (Routing Proxy feature), `docs/architecture.md`, `docs/pac.md` |
| Cookie refresh / validation logic | `src/cookie.rs`, `src/jumphost.rs` (supervisor monitor loop integration), `spec.md` (Cookie Ingestion Flow + env vars), `docs/run.md` (Cookie sources), `README.md` (Cookie methods) |
| Supervisor (`jumphost.rs`) flags or behavior | `src/jumphost.rs`, `src/main.rs` (CLI), `justfile` (`start`/`start-detached`/`stop` recipes), `contrib/vpn-jumphost.service.example`, `spec.md` (Standalone Supervisor section + env-var table), `docs/run.md` (Standalone supervisor section), `README.md` (systemd section) |
| Tunnel warmup behavior or default `WARMUP_HOSTS` | `src/jumphost.rs` (`warmup_after_start`, `warmup_hosts`), `src/config.rs` (`DEFAULT_WARMUP_HOSTS`), `spec.md` (env-var table + Standalone Supervisor section), `docs/run.md` (Standalone supervisor section) |
| Sleep/wake watcher change | `src/sleepwake/` (platform module), `spec.md` (Standalone Supervisor section), `docs/run.md` (sleep/wake bullet) |
| New CLI subcommand | `src/main.rs`, `justfile` (if exposed as a recipe), `spec.md` (CLI overview), relevant `docs/` page |
| New nix package dependency | `devenv.nix`, `spec.md` (Nix Packages table), `README.md` (Prerequisites table if user-visible) |

## Build / test commands

- `cargo build --release` (or `just build`) — release build at `target/release/jumphost`. Required by `devenv.nix`'s `processes.jumphost.exec` and by every `just` recipe that runs the binary.
- `cargo test --release` (or `just test`) — unit tests covering routing decisions, PAC content, and SOCKS5 reply formatting. Add tests when changing routing or PAC behavior.
- `devenv shell -- cargo build --release` if invoking from outside the devenv shell (cargo is provided by `languages.rust.enable = true`).
