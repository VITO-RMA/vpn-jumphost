# VPN Jumphost — Functional Specification

## System Overview

The VPN Jumphost is a [devenv](https://devenv.sh)-managed toolkit that connects to an F5 BIG-IP APM VPN via OpenConnect (F5 protocol) and exposes a local SOCKS5 proxy so host applications can selectively route traffic to internal resources through the VPN tunnel. The entire runtime is a single Rust binary (`jumphost`) that supervises `openconnect` (which spawns `ocproxy`), an optional in-process routing SOCKS5 proxy, and an optional in-process PAC HTTP server. The routing proxy sits in front of ocproxy on `127.0.0.1:1080` and applies per-domain routing rules: VPN-domain traffic is forwarded upstream to ocproxy, everything else connects directly. This lets any SOCKS5-capable tool (git, curl, SSH, …) use a single proxy address without needing PAC support. A PAC (Proxy Auto-Configuration) subsystem is also included so browsers can use automatic proxy configuration. The host retains normal networking; only explicitly targeted domains or CIDRs traverse the tunnel.

Everything runs as the **current unprivileged user**. openconnect is started with `--script-tun --script "ocproxy ..."`, which means it does **not** create a TUN device or touch `/dev/net/tun`: it spawns `ocproxy` as a child, hands it the tunnel over a socketpair, and lets ocproxy's userspace TCP/IP stack (lwIP) terminate the VPN's IP packets. ocproxy serves SOCKS5 on `127.0.0.1:1081` directly; the routing proxy on port 1080 is the user-facing entry point.

**Platforms:** Linux and macOS are both supported. Linux is the primary daily-driver target; macOS works because the design has no Linux-specific dependencies (`openconnect --script-tun` + `ocproxy` is portable, and both packages are available from nixpkgs on `x86_64-darwin` / `aarch64-darwin`). The wizard's autoproxy-instructions step branches on `uname -s` to print OS-appropriate guidance. The sleep/wake watcher has separate OS-native implementations for each platform.

---

## Architecture

### Component Map

The project is a single Rust crate (`Cargo.toml` at the repo root) that builds one binary, `jumphost` (`target/release/jumphost`). The only host-side shell script that remains is `scripts/jumphost-wizard.sh`, which drives the first-run bootstrap and then hands off to the binary via `devenv up` or `just start`.

| Component | Role |
|---|---|
| **`jumphost` binary** (`src/main.rs`) | Multi-subcommand CLI: `run` (default supervisor), `fetch-cookie`, `validate-cookie`, `generate-pac`, `serve-pac`. The supervisor in `run` orchestrates openconnect, ocproxy, the routing proxy, the PAC server, and the cookie monitor in a single process. Run as `processes.jumphost.exec` under devenv, or directly via `just start`. |
| **`src/config.rs`** + **`src/config_file.rs`** | Shared configuration: constants, env-var helpers, and TOML config file integration. **Single source of truth** for the `PROXY_DOMAINS` / `DIRECT_DOMAINS` lists used by both the PAC generator and the routing proxy, default paths (cookie file, browser profile dir), and default ports. All options can be overridden via a TOML config file at `$XDG_CONFIG_HOME/vpn-jumphost/config.toml`. Precedence: CLI flag > environment variable > config file > compiled-in default. The “must stay in sync” rule between the routing proxy and the PAC file is structurally enforced — there is only one definition. |
| **`src/vpn.rs`** | openconnect process management. Spawns `openconnect --protocol=f5 --cookie-on-stdin --script-tun --script "ocproxy -D ${SOCKS_PORT} -k ${OCPROXY_KEEPALIVE}" "$VPN_URL"` with the cookie file as stdin (no pipe), tracks the child PID, and forwards SIGTERM/SIGINT/SIGHUP. ocproxy is not invoked directly — openconnect spawns it as its `--script-tun` peer. `-g` is **never** passed to ocproxy. |
| **`src/routing.rs`** | Routing SOCKS5 proxy (ported 1:1 from the previous standalone `routing-proxy/` crate). Always started; listens on `127.0.0.1:1081`. ocproxy stays on port 1080. Per-domain rules read from `PROXY_DOMAINS` / `DIRECT_DOMAINS` in `config.rs`. |
| **`src/pac.rs`** | PAC file generation **and** built-in HTTP server (replaces the old `miniserve` dependency). Pure tokio/hyper, no external process. Generates the PAC text from the same `PROXY_DOMAINS` / `DIRECT_DOMAINS` constants and serves it on `127.0.0.1:8091` when `--serve-pac` is enabled. |
| **`src/cookie.rs`** | Cookie subsystem. Validation uses `reqwest` with rustls and **redirects disabled** to probe the F5 endpoint (3xx = expired cookie). Browser-based fetch uses Chromium via [`chromiumoxide`](https://crates.io/crates/chromiumoxide), which speaks the Chrome DevTools Protocol directly — **no Node.js driver is required** (the playwright dependency is gone). |
| **`src/jumphost.rs`** | Main supervisor module. Validates/refreshes the cookie before spawning openconnect; spawns and supervises the routing proxy and PAC server tasks; runs the periodic cookie-check loop; ties together the sleep/wake watcher (waking the loop immediately on resume so the VPN can be reconnected). |
| **`src/sleepwake/{mod,linux,macos}.rs`** | OS-native sleep/wake detection. Linux uses [`zbus`](https://crates.io/crates/zbus) to subscribe to the `org.freedesktop.login1.Manager.PrepareForSleep` signal. macOS uses [`objc2`](https://crates.io/crates/objc2) + [`block2`](https://crates.io/crates/block2) to subscribe to `NSWorkspaceDidWakeNotification`. Both platforms additionally have a portable wall-clock skew fallback in `jumphost.rs` that fires when a `time.time()`-equivalent jump larger than the threshold is observed (CLOCK_MONOTONIC stalls during Linux suspend, so a large wall-clock delta is a strong "we just woke up" signal). |
| **`src/logging.rs`** | `tracing-subscriber` bootstrap. TTY-aware ANSI colors; honors `NO_COLOR`, `FORCE_COLOR`, and `RUST_LOG` (the latter overrides `--verbose`). systemd/journald (no TTY) automatically gets plain output. |
| **`scripts/jumphost-wizard.sh`** | The only remaining shell script. Guided 4-step bootstrap that generates the PAC, captures the cookie via `jumphost fetch-cookie`, prints OS-appropriate autoproxy instructions, then `exec`s `devenv up`. |
| **`devenv.nix`** | Declares nix-provided packages (`openconnect`, `ocproxy`, `chromium`, `just`, plus the Rust toolchain), `CHROMIUM_PATH` (Linux only), and a single process: `processes.jumphost.exec = "./target/release/jumphost -c docs/config.example.toml run --serve-pac"`. |

### Layered View

```
┌──────────────────────────────────────────────────────────────────────┐
│  Host (Linux / macOS)                                                │
│                                                                      │
│  Browser / CLI / SSH ──► PAC URL (http://127.0.0.1:8091/proxy.pac)   │
│         │                                                            │
│         └──► 127.0.0.1:1081  (routing proxy, in-process)             │
│                  │                                                   │
│                  ├─  configured hosts                                │
│                  │   ──► 127.0.0.1:1080 (ocproxy SOCKS5, VPN)        │
│                  └─ everything else ──► direct connect()             │
│                                                                      │
│  ┌─ devenv up (process-compose) ────────────────────────────────┐    │
│  │  processes.jumphost                                          │    │
│  │     exec ./target/release/jumphost -c ... run --serve-pac   │    │
│  │       │                                                      │    │
│  │       ├─ cookie monitor loop (tokio task)                    │    │
│  │       │     • periodic validate every JUMPHOST_CHECK_INTERVAL│    │
│  │       │     • woken by sleep/wake watcher on resume          │    │
│  │       │                                                      │    │
│  │       ├─ openconnect (child process)                         │    │
│  │       │     --protocol=f5 --cookie-on-stdin                  │    │
│  │       │     --script-tun                                     │    │
│  │       │     --script "ocproxy -D 1080 -k 60"                 │    │
│  │       │     < $VPN_COOKIE_FILE                               │    │
│  │       │         └─ ocproxy (lwIP, SOCKS5 :1080)              │    │
│  │       │                                                      │    │
│  │       ├─ routing proxy (tokio task) :1081                    │    │
│  │       └─ PAC HTTP server (hyper task) :8091                  │    │
│  └──────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────┘
```

A single process (`jumphost run`) owns all of the supervisory state, including the openconnect child. Under `devenv up`, process-compose supervises that one process. SIGTERM/SIGINT propagates to openconnect, which tears the tunnel down; ocproxy goes with it via the `--script-tun` socketpair; the routing proxy and PAC tasks shut down via tokio cancellation.

### Port Allocation

| Port | Binding | Protocol | Service |
|------|---------|----------|---------|
| 1080 | `127.0.0.1` | SOCKS5 | ocproxy (VPN tunnel endpoint, upstream of routing proxy) |
| 1081 | `127.0.0.1` | SOCKS5 | routing proxy (user-facing; per-domain VPN-vs-direct routing) |
| 8091 | `127.0.0.1` | HTTP | PAC file (in-process hyper server) |

ocproxy listens on port 1080 and the routing proxy (always started) listens on port 1081. Clients point at `socks5h://127.0.0.1:1081` — the routing proxy decides per-domain whether to forward upstream to ocproxy or connect directly. All listeners bind to loopback by default.

---

## Features

### 1. VPN Tunnel (OpenConnect + ocproxy)

**What it does:** Establishes an F5 BIG-IP APM VPN tunnel to the configured VPN endpoint using openconnect, and terminates the tunnel's IP packets in a userspace lwIP stack (ocproxy) so the host kernel never sees a TUN device.

**How it works:**
- The user authenticates via a browser at the VPN portal and obtains an F5 `MRHSession` cookie (typically through the Chromium-based cookie fetch in `jumphost fetch-cookie`; see Feature 5). The cookie is written to `$VPN_COOKIE_FILE` (default `$XDG_STATE_HOME/vpn-jumphost/cookie`, mode 600).
- `devenv up` launches `processes.jumphost`, whose `exec` is `./target/release/jumphost -c docs/config.example.toml run --serve-pac`. The binary's supervisor (`src/jumphost.rs`) validates `VPN_COOKIE_FILE`, auto-refreshes the cookie if needed (see Cookie Ingestion Flow), and spawns openconnect:
  ```
  openconnect --protocol=f5 --cookie-on-stdin \
              --script-tun \
              --script "ocproxy -D ${SOCKS_PORT} -k ${OCPROXY_KEEPALIVE}" \
              "$VPN_URL"
  ```
  with the cookie file connected to openconnect's stdin (no pipe — the file descriptor is opened in `src/vpn.rs` and assigned to `stdin` of the spawned child).
- `--script-tun` tells openconnect *not* to call `tun_open()` / `/dev/net/tun`. Instead it creates a socketpair, exposes one end to the spawned script via the `VPNFD` env var, and writes raw IP packets to it. ocproxy reads `VPNFD`, plus the conventional vpnc-script env vars (`INTERNAL_IP4_ADDRESS`, `INTERNAL_IP4_MTU`, `INTERNAL_IP4_DNS`, `CISCO_DEF_DOMAIN`), and configures its internal lwIP stack accordingly.
- ocproxy then serves SOCKS5 on `127.0.0.1:${SOCKS_PORT}`. SOCKS5 connection requests are turned into lwIP TCP connections that ride the openconnect tunnel — there is no kernel routing, no host route table change, no DNS clobber.
- SIGTERM/SIGINT to `jumphost` is forwarded to openconnect; openconnect tears down the tunnel and ocproxy (its child) with it.

**Inputs:** F5 session cookie (`VPN_COOKIE_FILE`), `VPN_URL`, `VPN_PROTOCOL`, optional `SOCKS_PORT`, optional `OCPROXY_KEEPALIVE`.
**Outputs:** SOCKS5 listener on `127.0.0.1:${SOCKS_PORT}` once openconnect's tunnel comes up.
**Constraints:** None beyond having `openconnect` and `ocproxy` on `PATH`. No root, no namespace, no `/dev/net/tun`.

### 2. SOCKS5 Proxy (ocproxy)

**What it does:** Exposes a SOCKS5 proxy on `127.0.0.1:${SOCKS_PORT}` (default 1080) that routes traffic through the openconnect tunnel via ocproxy's userspace TCP/IP stack. The routing proxy (Feature 2a) sits in front on port 1081 and handles client connections with per-domain routing.

**How it works:**
- ocproxy is launched **by openconnect itself** via `--script-tun --script "ocproxy ..."` — it is not a sibling process; it is openconnect's tunnel peer.
- Flags assembled in `src/vpn.rs`:
  - `-D PORT` — SOCKS5 listener port. ocproxy binds to `127.0.0.1` by default.
  - `-k INTERVAL` — TCP keepalive interval in seconds (default 60) to prevent idle NAT timeouts from breaking long-lived tunnels (ssh, VS Code Remote SSH, …).
  - **`-g` is intentionally NOT passed.** With `-g`, ocproxy binds the SOCKS listener to all interfaces and exposes the unauthenticated SOCKS proxy to the LAN.
- DNS resolution: SOCKS5 clients should use `socks5h://` (resolve via the proxy). The routing proxy forwards domain names to ocproxy using SOCKS5h (ATYP 0x03), so VPN-side hostnames resolve through the VPN's DNS servers (pushed by the F5 server as `INTERNAL_IP4_DNS`).
- No authentication is configured (the proxy is loopback-only).
- Supports TCP only. UDP is not exposed via the SOCKS5 server.

### 2a. Routing SOCKS5 Proxy (`src/routing.rs`)

**What it does:** A system-wide SOCKS5 proxy on `127.0.0.1:1081` (always started by the supervisor) that applies per-domain routing rules. VPN-domain traffic is forwarded upstream to ocproxy on port 1080; everything else connects directly. This eliminates the need for PAC support in non-browser tools (git, curl, apt, SSH, etc.).

**How it works:**
- Implemented as a tokio task inside the `jumphost` binary (`src/routing.rs`). Ported 1:1 from the previously-standalone `routing-proxy/` crate, which no longer exists as a separate crate.
- Domain routing rules are defined as `PROXY_DOMAINS` / `DIRECT_DOMAINS` constants in `src/config.rs`. **This is also where `generate-pac` reads them from**, so the lists cannot drift apart:
  - VPN domains (`PROXY_DOMAINS`): configured via `[domains].proxy` in `config.toml` (no compiled-in defaults)
  - Always-direct domains (`DIRECT_DOMAINS`): configured via `[domains].direct` in `config.toml` (no compiled-in defaults)
- The proxy evaluates domain rules in order: `DIRECT_DOMAINS` checked first (higher priority), then `PROXY_DOMAINS`, else direct. A hostname matches a pattern if it equals exactly or is a subdomain.
- For domain-name requests: matched against `DIRECT_DOMAINS` then `PROXY_DOMAINS`.
- For raw IP address requests: connect directly.
- VPN-bound connections: opens a SOCKS5 session to ocproxy with ATYP 0x03 (domain name) so DNS resolves through the VPN.
- Direct connections: resolved and connected locally.

**Inputs:** `ROUTING_PROXY_PORT` (default 1081), `ROUTING_PROXY_BIND` (default 127.0.0.1), upstream ocproxy port (`SOCKS_PORT`, default 1080).
**Outputs:** SOCKS5 listener on `127.0.0.1:1081`.

**Client usage:** All tools can point at `socks5h://127.0.0.1:1081` — the routing proxy handles the VPN-vs-direct decision transparently:
```bash
# git
git config --global http.proxy socks5h://127.0.0.1:1081

# curl
curl --proxy socks5h://127.0.0.1:1081 https://internal.example.com

# SSH — ProxyCommand
ssh -o 'ProxyCommand=nc -x 127.0.0.1:1081 -X 5 %h %p' host.example.com

# Environment variable (many tools)
export ALL_PROXY=socks5h://127.0.0.1:1081
```

### 3. PAC File Generation (`jumphost generate-pac`)

**What it does:** Generates a JavaScript PAC file that instructs browsers which traffic to proxy and which to send direct.

**How it works:**
- Implemented in `src/pac.rs`. The generator reads `PROXY_DOMAINS` and `DIRECT_DOMAINS` from `src/config.rs` (the same constants the routing proxy uses), plus a proxy chain string from `PAC_PROXY_CHAIN` (default `SOCKS5 ${PAC_PROXY_HOST}:${PAC_SOCKS_PORT}; DIRECT`).
- Domain lists (loaded from `config.toml` at startup):
  - `PROXY_DOMAINS` = configured via `[domains].proxy` in `config.toml` (no compiled-in defaults; see [`docs/config.example.toml`](docs/config.example.toml) for VITO defaults)
  - `DIRECT_DOMAINS` = configured via `[domains].direct` in `config.toml` (no compiled-in defaults)
- Outputs a `FindProxyForURL` function with these rules (evaluated in order):
  1. Always-DIRECT domains (from `[domains].direct`; checked before DNS resolution).
  2. Matched domains → `proxy_chain` string (default: `SOCKS5 127.0.0.1:1080; DIRECT`).
  3. Everything else → `DIRECT`.
- Domain matching uses `host === "X" || dnsDomainIs(host, ".X")` for bare domains.

**CLI:** `jumphost generate-pac [PATH]` — writes the PAC text to `PATH` if given, otherwise stdout.
**Inputs:** `PAC_PROXY_HOST`, `PAC_SOCKS_PORT`, `PAC_PROXY_CHAIN`, output path.
**Outputs:** A `.pac` JavaScript file.

### 4. PAC Serving (Host-Side Loopback)

**What it does:** Serves PAC files on `http://127.0.0.1:8091/` independently of the VPN tunnel lifecycle, so browsers can always fetch the PAC even when the VPN is down (which keeps the direct-domain rules in effect).

**How it works:**
- Implemented in `src/pac.rs` using `tokio` + `hyper` — there is no `miniserve` dependency any more, and no external process.
- Inside `jumphost run`, the PAC server is started as a tokio task when `--serve-pac` is given (the default `devenv` config passes it). It serves the generated PAC text at `/proxy.pac` (and `/`), with `Content-Type: application/x-ns-proxy-autoconfig`.
- The PAC content is regenerated from `config.rs` constants at startup; it does not need to be on disk.
- If you want to use a file based pac config, run `just pac-gen` and point your browser at that file instead.

### 5. Browser-Based Cookie Fetch (`jumphost fetch-cookie`)

**What it does:** Opens a Chromium browser window and waits for the user to complete SSO authentication (including Microsoft Authenticator MFA). Once the F5 `MRHSession` cookie is set on the VPN portal, it is written to the output file (mode 600) and Chromium closes.

**How it works:**
- Implemented in `src/cookie.rs` using the [`chromiumoxide`](https://crates.io/crates/chromiumoxide) crate, which speaks the Chrome DevTools Protocol directly over a WebSocket. **No Node.js driver is required** (the previous playwright + Firefox stack has been removed).
- Launches the Chromium executable in **headed mode** by default (a visible browser window). Pass `--headless` or set `JUMPHOST_HEADLESS=1` to launch headless instead (no visible window). Headless mode only works when `VPN_USERNAME` + `VPN_PASSWORD` are set so the CDP automation can complete SSO without user interaction. **MFA auto-detection:** during a headless session, the poll loop distinguishes three MFA phases: (1) the **method-picker** screen ("Verify your identity" — choose Authenticator app vs. verification code) is handled automatically by clicking the "Approve a request on my Microsoft Authenticator app" option to trigger the push notification; (2) the **number-match screen** (Authenticator push — shows a number the user must tap on their phone) is handled entirely headless: the number is extracted from the page DOM and delivered as a **desktop notification** via the [`notify-rust`](https://crates.io/crates/notify-rust) crate (talks D-Bus directly on Linux, Notification Center on macOS — **no external `notify-send` binary needed**), and also logged via `tracing::info` for journal consumers; the browser stays headless and keeps polling for the cookie; (3) **interactive prompts** (TOTP code entry, phone-call verification) cannot be completed without user input, so the headless browser is closed and relaunched in headed mode for the user to type the code. The Authenticator push flow (the common case) therefore works **entirely headless** — only TOTP/interactive prompts cause a headed relaunch. The Chromium path is taken from `--chromium`, then `$CHROMIUM_PATH`, then `$CHROME`. `devenv.nix` exports `CHROMIUM_PATH=${pkgs.chromium}/bin/chromium`.
- Uses a **persistent Chromium user-data-dir** so session state can be reused across runs (path: `--profile-dir`, then `$VPN_BROWSER_PROFILE_DIR`, default `$XDG_STATE_HOME/vpn-jumphost/chromium-profile`). Note: the directory name changed from `playwright-profile` to `chromium-profile`.
- Navigates to the configured `VPN_URL`; the browser presents the SAML / Microsoft SSO login flow.
- When `VPN_USERNAME` / `VPN_PASSWORD` are set, automation handles both account-picker and credential forms via CDP.
- Polls the browser-wide CDP cookie store (`Storage.getCookies`) for the `MRHSession` cookie (up to `--max-wait` seconds; default 300). The page-scoped `Network.getCookies` is **not** used: it returns no rows when invoked on the browser root target.
- Writes the cookie value to the file given by `-o/--output` (default same as `run`'s `--cookie-file`: `$VPN_COOKIE_FILE` or the XDG default), creating parent directories with `umask 077` (final file mode `600`). Status messages go to stderr via the `tracing` subscriber.

**CLI flags:**
- `-o, --output FILE` — destination cookie file (default same as `run`).
- `--profile-dir DIR` — persistent Chromium user-data-dir.
- `--chromium PATH` — Chromium executable path (defaults to `$CHROMIUM_PATH` / `$CHROME`).
- `--max-wait SECONDS` — maximum time to wait for SSO + MFA to complete (default 300).
- `--headless` — launch Chromium in headless mode (no visible window). Requires `VPN_USERNAME` + `VPN_PASSWORD` for unattended SSO. The Authenticator push MFA flow works entirely headless (the number-match value is shown as a desktop notification via `notify-rust`); only TOTP/interactive MFA prompts cause an automatic headed relaunch. Default is `false`; also settable via `JUMPHOST_HEADLESS=1`.

**Standalone usage:**
```bash
just fetch-cookie
# or:
./target/release/jumphost fetch-cookie -o ~/.local/state/vpn-jumphost/cookie
```

Note: `jumphost run` invokes the same validate / fetch flow internally — there is no separate shell glue. When the supervisor (`jumphost run`) refreshes a cookie, it automatically uses headless mode if both `VPN_USERNAME` and `VPN_PASSWORD` are set and non-empty, so periodic background refreshes don't pop up a browser window. The Authenticator push flow (the common case) works **entirely headless**: the number-match value is delivered as a desktop notification (via `notify-rust` — D-Bus on Linux, Notification Center on macOS) and logged to the journal, while the browser stays headless and polls for completion. Only TOTP or other interactive MFA prompts cause a headed relaunch for user interaction. **Escape hatch:** if the headless MFA flow is unstable, pass `--no-headless` to the `run` subcommand (or set `JUMPHOST_NO_HEADLESS=1`) to force the supervisor to always open a visible browser window for cookie refresh, even when credentials are available.

### 6. Guided Bootstrap Wizard (`jumphost-wizard.sh`)

**What it does:** Walks the user through a 4-step interactive flow to set up and start the jumphost.

**Steps:**
1. **PAC on disk** — generates `proxy.pac` in the repo root if it does not exist (using `./target/release/jumphost generate-pac proxy.pac`); skips if present.
2. **Serve PAC over HTTP** — informs the user that the PAC will be served by `devenv up` (the `jumphost` process with `--serve-pac`) on `http://127.0.0.1:8091/proxy.pac` once it is started in Step 4. There is no longer a standalone PAC server to start here.
3. **Desktop proxy instructions** — prints OS-specific instructions based on `uname -s` (`Darwin` → `networksetup` + System Settings; otherwise GNOME `gsettings` / KDE / Firefox) for setting the automatic proxy configuration URL. Waits for the user to press Enter. Implemented in `print_autoproxy_instructions()`.
4. **VPN authentication and jumphost** — runs `./target/release/jumphost fetch-cookie -o "$VPN_COOKIE_FILE"` to capture the cookie via Chromium. The cookie is written to `$XDG_STATE_HOME/vpn-jumphost/cookie` with mode 600 and `VPN_COOKIE_FILE` is exported.

   Then the wizard `exec`s `devenv up` (or `devenv up -d` if `-d` was passed). process-compose starts the single `jumphost` process, which spawns openconnect (with ocproxy as its `--script-tun` peer), the routing proxy, and the in-process PAC server. No `sudo` prompt anywhere.

**Cleanup behavior:**
- If the wizard is interrupted before reaching `exec devenv up`, no background state is left behind (the PAC server is no longer started by the wizard).
- The cookie file persists in the state dir (mode 600) so subsequent `devenv up` runs can reuse it without re-authenticating.
- **Ctrl-C (foreground) or `just stop` (detached) tears the tunnel down** — `jumphost run` exits, openconnect exits, ocproxy goes with it. No orphaned state.

### 7. Task Runner (`justfile`)

| Recipe | Action |
|--------|--------|
| `just` (default) | Lists all recipes |
| `just build` | Runs `cargo build --release`. Every other recipe depends on this. |
| `just test` | Runs `cargo test --release`. |
| `just bootstrap [-d]` | Runs `scripts/jumphost-wizard.sh`; pass `-d` to start the devenv processes detached |
| `just fetch-cookie` | `./target/release/jumphost fetch-cookie` — fetches the MRHSession cookie via Chromium and writes it to `$VPN_COOKIE_FILE` |
| `just validate-cookie` | `./target/release/jumphost validate-cookie` — probes the VPN endpoint with the current cookie. Exit 0 = valid, 1 = invalid, 2 = network error. |
| `just pac-gen` | `./target/release/jumphost generate-pac proxy.pac` |
| `just pac-show` | Prints the generated PAC text to stdout |
| `just start` | Runs `./target/release/jumphost -c docs/config.example.toml run --serve-pac` in the foreground (Ctrl-C to stop). Starts openconnect + ocproxy, the routing proxy on `127.0.0.1:1081`, and the loopback PAC server. Uses the example config for VPN URL + domain lists; override with a user-local `config.toml` or env vars. |
| `just start-detached` | Wraps the same `jumphost run` command with `nohup`, redirecting stdout/stderr to `~/.local/state/vpn-jumphost/jumphost.log` and writing the PID to `jumphost.pid`. The binary itself no longer daemonizes (the old `-d/--daemonize` flag is gone); the recipe handles backgrounding. |
| `just stop` | Reads PID from `~/.local/state/vpn-jumphost/jumphost.pid` and sends SIGTERM, tearing down the tunnel, routing proxy, and PAC server. |
| `just current-version` | Prints the latest semver release tag from git |
| `just release major\|minor\|patch` | Validates level / main / clean state, computes the next semver tag, then tags and pushes `main` + tag |

### 8. SSH Proxy Configuration

**What it does:** Provides an OpenSSH client config file for routing SSH connections through the jumphost's SOCKS5 proxy.

**How it works:**
- 
  ```
  ProxyCommand /usr/bin/nc -x 127.0.0.1:1081 -X 5 %h %p
  ```
  i.e. SOCKS5 via the routing proxy (which transparently forwards to ocproxy).
- The `Host *` stanza at the end carries shared defaults (identity, ControlMaster, keepalives).

**Usage:** Copy into `~/.ssh/config` or use OpenSSH `Include` directive.

---

## Workflows

### Primary Workflow: Full Bootstrap

```
User runs `just bootstrap` from inside `devenv shell`
  │
  ├─ Step 1: proxy.pac on disk (generate if missing via `jumphost generate-pac`)
  ├─ Step 2: PAC server will be started by devenv up in Step 4
  ├─ Step 3: Print OS autoproxy instructions; wait for Enter
  └─ Step 4: VPN authentication
       └─ jumphost fetch-cookie -o $VPN_COOKIE_FILE
            │  (Chromium opens; user completes SSO + MFA)
            │
            └─ Cookie written to $XDG_STATE_HOME/vpn-jumphost/cookie (0600)
                 │
                 └─ exec devenv up
                      │
                      └─ process-compose: `jumphost` process
                           └─ ./target/release/jumphost -c docs/config.example.toml run --serve-pac
                                ├─ validate cookie (auto-refresh if invalid)
                                ├─ spawn openconnect --protocol=f5 --cookie-on-stdin
                                │         --script-tun
                                │         --script "ocproxy -D 1080 -k 60"
                                │         $VPN_URL  < $VPN_COOKIE_FILE
                                │         └─ ocproxy (lwIP, SOCKS5 :1080)
                                ├─ routing proxy task on 127.0.0.1:1081
                                ├─ PAC HTTP server task on 127.0.0.1:8091
                                └─ cookie monitor task (periodic + sleep/wake-driven)
```

Ctrl-C (or `just stop`) sends SIGTERM to the `jumphost` process; the supervisor forwards SIGTERM to openconnect, which tears the tunnel down. ocproxy goes with it via the `--script-tun` socketpair; the routing proxy and PAC tasks are cancelled.

### Release Workflow: SemVer Tagging

`just release major|minor|patch` validates the level, enforces branch=main + clean tree + local main in sync with origin, computes the next tag from the latest `v[0-9]*.[0-9]*.[0-9]*` tag (fallback `v0.0.0`), creates an annotated tag, and pushes both `main` and the tag.

### Cookie Ingestion Flow

The supervisor (`src/jumphost.rs` for `run`, `src/cookie.rs` for the primitives) reads the cookie from `VPN_COOKIE_FILE` only:

1. **`VPN_COOKIE_FILE`** — openconnect reads the cookie directly from this file via stdin (the file is opened in `src/vpn.rs` and assigned to the child's `stdin`). Default `${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/cookie`. The wizard (and `jumphost fetch-cookie`) write here with `umask 077`. `devenv.nix` intentionally does **not** set this variable (nix would export `$HOME` literally, breaking the path); the binary defends against that by dropping any value containing a literal `$HOME` or `$XDG_STATE_HOME` and falling back to the computed default.

Before starting openconnect, the supervisor validates the cookie and auto-refreshes it if needed:

1. **Validate** — `src/cookie.rs` probes the VPN endpoint (`$VPN_URL/vdesk/vpn/index.php3?outform=xml` with the `MRHSession` cookie) using `reqwest` configured with `rustls` and `redirect::Policy::none()`. **HTTP redirects are deliberately not followed** — a 3xx response (the F5 gateway redirecting to the SSO login page) means the cookie is expired or invalid. A 404 also indicates an expired/invalid cookie. Network errors (DNS failure, timeout, etc.) are **not** treated as an invalid cookie — the supervisor proceeds with the existing cookie so transient connectivity blips don't force a re-login. The same logic is exposed via `jumphost validate-cookie` (exit codes: 0 valid, 1 invalid, 2 network error).
2. **Refresh** — if the cookie file is missing, empty, or the validation returned 404/3xx, the supervisor automatically runs the same code path as `jumphost fetch-cookie` to open Chromium for SSO login and capture a fresh cookie, writing it back to `$VPN_COOKIE_FILE` atomically. If `VPN_USERNAME` or `VPN_PASSWORD` are set, they are forwarded to the cookie-fetch code to pre-fill the login form.
3. **Error** — if auto-refresh fails (Chromium not on `PATH`, user cancels the browser, timeout, etc.), the supervisor logs an actionable error and exits non-zero.

There is no `VPN_COOKIE` env-var fallback and no interactive stdin fallback (process-compose has no useful TTY).

### Process Lifecycle

1. `devenv up` launches process-compose, which starts the single declared process `jumphost` (`exec = "./target/release/jumphost -c docs/config.example.toml run --serve-pac"`).
2. The supervisor in `src/jumphost.rs` validates the existing cookie and auto-refreshes it via the Chromium flow if it is expired or missing. After the cookie is confirmed valid, it spawns `openconnect` (which spawns `ocproxy`), the routing proxy tokio task, and the PAC HTTP server tokio task.
3. ocproxy serves SOCKS5 on `127.0.0.1:${SOCKS_PORT}` (default 1080). The routing proxy (always started) listens on `127.0.0.1:1081`.
4. The PAC HTTP server runs on `127.0.0.1:${PAC_SERVE_PORT}` (default 8091) in the same process — no external `miniserve`.
5. Signal handling:
   - SIGTERM / SIGINT to `jumphost` → supervisor forwards SIGTERM to openconnect; openconnect tears the tunnel down; ocproxy's `VPNFD` socketpair closes and ocproxy exits; the routing proxy and PAC tasks are cancelled; the process exits. The cookie fetch flow also respects SIGTERM/SIGINT: a `CancellationToken` is threaded through `fetch` → `launch_and_fetch` → `fetch_inner`, and the poll loop uses `tokio::select!` to check for cancellation alongside its 1-second sleep. This means `jumphost run` can shut down cleanly even while a cookie refresh is in progress (the browser is always closed cleanly on interruption), and `jumphost fetch-cookie` responds to Ctrl-C at any point during the SSO/MFA flow.
   - openconnect/ocproxy crash → the supervisor notices, logs the exit status, and (in `run`'s monitor loop) considers it a hard failure unless the loop is configured to restart. process-compose's restart policy is `availability.restart = "no"` / `max_restarts = 0` in `devenv.nix`, so transient failures don't loop — fix the underlying issue (server unreachable, etc.) and re-run `devenv up`. Cookie refresh happens automatically on restart so an expired cookie alone won't block you.

### Standalone Supervisor (`jumphost run` outside `devenv up`)

A single-process supervisor for environments where `process-compose` isn't desired — most notably as a `systemctl --user` unit (see `contrib/vpn-jumphost.service.example`) or under `nohup` via `just start-detached`. The `jumphost run` subcommand:

1. Resolves the cookie path (`--cookie-file`, `$VPN_COOKIE_FILE`, or the default `$XDG_STATE_HOME/vpn-jumphost/cookie`), with the same `$HOME` / `$XDG_STATE_HOME` literal-variable defense described above.
2. Validates the cookie via the in-process validator. Refreshes via the Chromium cookie-fetch flow on "invalid" results; on network errors keeps the existing cookie.
3. Starts `openconnect --protocol=f5 --cookie-on-stdin --script-tun --script "ocproxy -D ${SOCKS_PORT} -k ${OCPROXY_KEEPALIVE}" "$VPN_URL"` with the cookie file connected to stdin. openconnect spawns ocproxy (SOCKS5 on `127.0.0.1:${SOCKS_PORT}`) as its `--script-tun` peer. The supervisor tracks openconnect's PID and tears it down on SIGTERM/SIGINT/SIGHUP.
4. Starts the routing proxy task on `127.0.0.1:1081` (configurable via `ROUTING_PROXY_PORT`). ocproxy listens on port 1080 (configurable via `SOCKS_PORT`).
5. If `--serve-pac` is given, also starts the in-process PAC HTTP server on `127.0.0.1:${PAC_SERVE_PORT}`.
6. A monitor task polls every `min(15s, check_interval/4)`:
   - It is woken **immediately** by an OS-native sleep/wake watcher when available:
     - **Linux** (`src/sleepwake/linux.rs`): uses [`zbus`](https://crates.io/crates/zbus) to subscribe to `org.freedesktop.login1.Manager.PrepareForSleep` on the system bus. The signal arrives with `false` on resume; the watcher calls `on_resume`.
     - **macOS** (`src/sleepwake/macos.rs`): uses [`objc2`](https://crates.io/crates/objc2) + [`block2`](https://crates.io/crates/block2) to register a Cocoa observer for `NSWorkspaceDidWakeNotification`.
     - On any other platform, or if subscription fails (e.g. headless container, no D-Bus), it silently falls back to wall-clock skew detection only.
   - As a portable fallback (and corroboration) it also watches for wall-clock skew larger than `max(poll_interval*4, 30s)` — CLOCK_MONOTONIC stalls during Linux suspend, so a large wall-clock delta is a strong signal that we just woke up.
   - If openconnect died, it (re)starts it; if the routing proxy or PAC server task panicked, it is logged.
   - At least every `--check-interval` seconds (default 300, overridable via `JUMPHOST_CHECK_INTERVAL`), it re-validates the cookie. After a resume the validation retries on network errors with exponential backoff for up to 60 s, since routing/DNS can take a couple of seconds to come back up after the laptop wakes. On a successful resume validation (cookie still valid), **the VPN is restarted unconditionally** because the TCP/TLS session to the F5 gateway is almost certainly dead after suspend (NAT entries flushed, server may have closed the connection) and openconnect's own dead-peer detection can take minutes. If a periodic (non-forced) validation hits a network error, the next short poll iteration retries within `poll_interval` rather than waiting another full `check_interval`. If validation returns "invalid", the cookie is refreshed and the VPN is restarted.

Logging uses `tracing` + `tracing-subscriber` (`src/logging.rs`) to stderr with a timestamped format. systemd captures stderr into the journal automatically. When stderr is a TTY (and `NO_COLOR` is unset), ANSI level colors are enabled; `FORCE_COLOR` overrides the TTY check, `NO_COLOR` disables it, and `RUST_LOG` (if set) overrides the `--verbose` flag entirely. The default filter also silences `chromiumoxide` below ERROR so the `WS Invalid message: data did not match any variant of untagged enum Message` noise (emitted on every Chromium CDP event the bundled protocol schema doesn't recognize) stays out of the log; `RUST_LOG=chromiumoxide=debug` (or any other directive) re-enables it. Under systemd/journald (no TTY) the formatter is plain so journalctl output isn't littered with ANSI escapes.

The sleep/wake watcher's OS support is selected at compile time via `#[cfg(target_os = "linux")]` / `#[cfg(target_os = "macos")]` in `src/sleepwake/mod.rs`. The Cargo deps (`zbus`, `objc2`, `block2`) are similarly cfg-gated so the build remains minimal on the unused platform.

The old `-d/--daemonize` flag is **gone**. To run detached, use `just start-detached` (which wraps `nohup`) or a systemd user unit.

## Configuration

### Config File

All options can be set in a TOML config file. The file location is resolved in order:

1. Explicit path from the `-c / --config` CLI flag.
2. `$XDG_CONFIG_HOME/vpn-jumphost/config.toml`.
3. `~/.config/vpn-jumphost/config.toml`.

The file is optional — missing or unreadable files are silently ignored (but when `-c` is used, a missing file produces a warning). Parse errors produce a warning and fall back to defaults.

**Precedence (highest → lowest):** CLI flag > environment variable > config file > compiled-in default.

The binary loads a `.env` file from the current working directory at startup (via the `dotenvy` crate), before CLI parsing. Variables defined in `.env` are injected into the process environment, so they sit at the "environment variable" tier in the precedence chain above. This means the binary picks up `.env` whether invoked via `just start`, `devenv up`, or directly as `./target/release/jumphost -c docs/config.example.toml run --serve-pac`. Missing `.env` files are silently ignored.

Example `config.toml`:

```toml
vpn_url = "https://vpn.example.com"
vpn_protocol = "f5"
socks_port = 1080
ocproxy_keepalive = 60
cookie_file = "/home/user/.local/state/vpn-jumphost/cookie"
browser_profile_dir = "/home/user/.local/state/vpn-jumphost/chromium-profile"
check_interval = 300
no_headless = false
chromium_path = "/usr/bin/chromium"

[routing_proxy]
bind = "127.0.0.1"
port = 1081

[pac_server]
bind = "127.0.0.1"
port = 8091

[pac_generate]
proxy_host = "127.0.0.1"
socks_port = "1081"
proxy_chain = "SOCKS5 127.0.0.1:1081; DIRECT"

[domains]
proxy = ["example.com", "corp.local", "internal.example.com"]
direct = ["vpn.example.com"]

[credentials]
username = "user@example.com"
password_file = "/run/secrets/vpn_pass"
```

#### Config file fields

| Field | Type | Equivalent env var | Description |
|---|---|---|---|
| `vpn_url` | string | `VPN_URL` | F5 VPN endpoint URL |
| `vpn_protocol` | string | `VPN_PROTOCOL` | OpenConnect protocol |
| `socks_port` | integer | `SOCKS_PORT` | ocproxy SOCKS5 listen port |
| `ocproxy_keepalive` | integer | `OCPROXY_KEEPALIVE` | TCP keepalive (seconds) for ocproxy `-k` |
| `cookie_file` | path | `VPN_COOKIE_FILE` | Path to cookie file |
| `browser_profile_dir` | path | `VPN_BROWSER_PROFILE_DIR` | Chromium user-data-dir |
| `check_interval` | float | `JUMPHOST_CHECK_INTERVAL` | Supervisor cookie-check interval (seconds) |
| `no_headless` | boolean | `JUMPHOST_NO_HEADLESS` | Disable headless cookie refresh |
| `chromium_path` | path | `CHROMIUM_PATH` | Path to Chromium executable |
| `routing_proxy.bind` | string | `ROUTING_PROXY_BIND` | Routing proxy bind address |
| `routing_proxy.port` | integer | `ROUTING_PROXY_PORT` | Routing proxy listen port |
| `pac_server.bind` | string | `PAC_SERVE_BIND` | PAC HTTP server bind address |
| `pac_server.port` | integer | `PAC_SERVE_PORT` | PAC HTTP server listen port |
| `pac_generate.proxy_host` | string | `PAC_PROXY_HOST` | Proxy host in generated PAC |
| `pac_generate.socks_port` | string | `PAC_SOCKS_PORT` | SOCKS5 port in generated PAC |
| `pac_generate.proxy_chain` | string | `PAC_PROXY_CHAIN` | Full proxy chain string in PAC |
| `domains.proxy` | array of strings | — | Domains routed through VPN (no compiled-in default; must be configured) |
| `domains.direct` | array of strings | — | Domains always reached directly (no compiled-in default) |
| `credentials.username` | string | `VPN_USERNAME` | VPN username |
| `credentials.password` | string | `VPN_PASSWORD` | VPN password |
| `credentials.username_file` | path | `VPN_USERNAME_FILE` | Path to file containing username |
| `credentials.password_file` | path | `VPN_PASSWORD_FILE` | Path to file containing password |

The `[domains]` table is the primary way to set `PROXY_DOMAINS` / `DIRECT_DOMAINS` — there are no environment variables for these. The domain lists are cached on first access and remain stable for the process lifetime.

### Environment Variables

#### VPN tunnel (`jumphost run`, `src/vpn.rs`)

| Variable | Default | Description |
|---|---|---|
| `VPN_URL` | _(empty — must be configured)_ | F5 VPN endpoint URL |
| `VPN_PROTOCOL` | `f5` | OpenConnect protocol (always F5 in this project) |
| `SOCKS_PORT` | `1080` | ocproxy SOCKS5 listen port (loopback). |
| `OCPROXY_KEEPALIVE` | `60` | TCP keepalive interval (seconds) passed to ocproxy via `-k` |
| `VPN_COOKIE_FILE` | `${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/cookie` (computed in `src/config.rs`, not in `devenv.nix`) | Path to file containing the cookie. If the value contains a literal `$HOME` or `$XDG_STATE_HOME` (unexpanded shell variable), it is ignored and the default is used. |
| `VPN_USERNAME` | _(unset)_ | If set, passed to the Chromium cookie-fetch flow during auto-refresh to pre-fill the login form. Takes precedence over `VPN_USERNAME_FILE`. |
| `VPN_PASSWORD` | _(unset)_ | If set, passed to the Chromium cookie-fetch flow during auto-refresh to pre-fill the login form. Takes precedence over `VPN_PASSWORD_FILE`. |
| `VPN_USERNAME_FILE` | _(unset)_ | Path to a file whose contents (trimmed) are used as the VPN username. Only consulted when `VPN_USERNAME` is unset or empty. Useful for secret-mounting (e.g. `/var/run/secrets/vpn_user`). |
| `VPN_PASSWORD_FILE` | _(unset)_ | Path to a file whose contents (trimmed) are used as the VPN password. Only consulted when `VPN_PASSWORD` is unset or empty. Useful for secret-mounting (e.g. `/var/run/secrets/vpn_pass`). |

#### PAC Generation (`jumphost generate-pac`, `src/pac.rs`)

| Variable | Default | Description |
|---|---|---|
| `PAC_PROXY_HOST` | `127.0.0.1` | Proxy host for the default chain |
| `PAC_SOCKS_PORT` | `1081` | SOCKS5 proxy port for the default chain (points at the routing proxy) |
| `PAC_PROXY_CHAIN` | `SOCKS5 ${PAC_PROXY_HOST}:${PAC_SOCKS_PORT}; DIRECT` | Full proxy chain string |

The domain lists (`PROXY_DOMAINS` / `DIRECT_DOMAINS`) are **not** environment variables — they default to empty and must be configured via the `[domains]` table in `config.toml` (see [Config File](#config-file) above).

#### Routing Proxy (`src/routing.rs`)

| Variable | Default | Description |
|---|---|---|
| `ROUTING_PROXY_PORT` | `1081` | Routing proxy listen port |
| `ROUTING_PROXY_BIND` | `127.0.0.1` | Routing proxy bind address |

Upstream ocproxy port is taken from `SOCKS_PORT` (default 1080). The routing proxy is always started by the supervisor.

#### PAC HTTP server (`jumphost serve-pac` and the in-process server in `jumphost run --serve-pac`)

| Variable | Default | Description |
|---|---|---|
| `PAC_SERVE_PORT` | `8091` | Host loopback port for PAC HTTP |
| `PAC_SERVE_BIND` | `127.0.0.1` | Bind address |

#### Supervisor (`jumphost run`, `src/jumphost.rs`)

| Variable | Default | Description |
|---|---|---|
| `JUMPHOST_CHECK_INTERVAL` | `300` | Seconds between periodic cookie validity checks. Overridable by the `--check-interval SECONDS` CLI flag. |
| `VPN_URL`, `VPN_PROTOCOL`, `SOCKS_PORT`, `OCPROXY_KEEPALIVE`, `VPN_COOKIE_FILE`, `VPN_USERNAME`, `VPN_PASSWORD`, `VPN_USERNAME_FILE`, `VPN_PASSWORD_FILE` | (see VPN tunnel above) | Consumed by the embedded VPN module. |
| `PAC_SERVE_PORT`, `PAC_SERVE_BIND` | (see PAC server above) | Consumed only when `--serve-pac` is passed. |
| `ROUTING_PROXY_PORT`, `ROUTING_PROXY_BIND` | (see Routing Proxy above) | Consumed by the routing proxy (always started). |
| `RUST_LOG` | _(unset)_ | If set, overrides `--verbose` entirely (`tracing-subscriber` EnvFilter). Note that the default filter pins `chromiumoxide=error` to mute the `WS Invalid message: ...` warning spam coming from Chromium CDP events the bundled protocol schema doesn't recognize; setting `RUST_LOG` removes that pin, so include `chromiumoxide=error` yourself if you still want it. |
| `JUMPHOST_NO_HEADLESS` | _(unset)_ | When set to `1`, `true`, or `yes`, the supervisor never uses headless mode for cookie refresh — it always opens a visible browser window, even when `VPN_USERNAME` + `VPN_PASSWORD` are available. Overridable by the `--no-headless` CLI flag. This is an escape hatch for cases where the headless MFA flow is unstable. |

CLI: `-c/--config FILE`, `--serve-pac`, `--check-interval SECONDS`, `--cookie-file PATH`, `--no-headless`, `-v/--verbose`. The `-c` and `-v` flags are global (accepted before or after any subcommand). The `run`-specific flags are accepted both at the top level (`jumphost --no-headless --serve-pac`) and on the `run` subcommand. The previous `-d/--daemonize` flag is **gone** — use `just start-detached` (nohup) or a systemd unit instead.

#### Cookie fetch (Chromium, `jumphost fetch-cookie`, `src/cookie.rs`)

| Variable | Default | Description |
|---|---|---|
| `CHROMIUM_PATH` | _(unset; set to `${pkgs.chromium}/bin/chromium` by `devenv.nix` on Linux)_ | Path to the Chromium executable used by the CDP-based cookie fetch. On macOS, `devenv.nix` does not set this — `chromiumoxide` auto-detects a system-installed Chrome or Chromium. |
| `CHROME` | _(unset)_ | Fallback if `CHROMIUM_PATH` is not set. |
| `VPN_BROWSER_PROFILE_DIR` | `$XDG_STATE_HOME/vpn-jumphost/chromium-profile` | Persistent Chromium user-data-dir for SSO / session reuse. (Renamed from `playwright-profile`.) |
| `VPN_USERNAME`, `VPN_PASSWORD` | _(unset)_ | If set, the CDP automation fills the Microsoft SSO login form. Takes precedence over `VPN_USERNAME_FILE` / `VPN_PASSWORD_FILE`. |
| `VPN_USERNAME_FILE`, `VPN_PASSWORD_FILE` | _(unset)_ | Paths to files whose contents (trimmed) are used as credentials when the corresponding env var is unset/empty. |
| `JUMPHOST_HEADLESS` | _(unset)_ | When set to `1`, `true`, or `yes`, acts as the default for `--headless` on `fetch-cookie` — launches Chromium without a visible window. Requires `VPN_USERNAME` + `VPN_PASSWORD` for unattended SSO. The supervisor (`jumphost run`) ignores this variable and decides headless mode based on whether credentials are set. |

CLI: `-o/--output FILE`, `--profile-dir DIR`, `--chromium PATH`, `--max-wait SECONDS`, `--headless`.

#### Wizard (`scripts/jumphost-wizard.sh`)

| Variable | Default | Description |
|---|---|---|
| `JUMPHOST_PAC_NAME` | `proxy.pac` | PAC filename on disk |
| `VPN_COOKIE_FILE` | `$XDG_STATE_HOME/vpn-jumphost/cookie` | Destination path written by `jumphost fetch-cookie` (mode 600) and consumed by `jumphost run` |
| `VPN_USERNAME`, `VPN_PASSWORD` | _(unset)_ | If set, the Chromium automation fills the login form. Takes precedence over `VPN_USERNAME_FILE` / `VPN_PASSWORD_FILE`. |
| `VPN_USERNAME_FILE`, `VPN_PASSWORD_FILE` | _(unset)_ | Paths to files whose contents (trimmed) are used as credentials when the corresponding env var is unset/empty. |
| `VPN_BROWSER_PROFILE_DIR` | `$XDG_STATE_HOME/vpn-jumphost/chromium-profile` | Persistent Chromium user-data-dir for SSO / session reuse |
| `CHROMIUM_PATH` | (set by `devenv.nix`) | Chromium executable path for the cookie fetch flow |

### Config Files

| Path | Purpose |
|---|---|
| `${XDG_CONFIG_HOME:-$HOME/.config}/vpn-jumphost/config.toml` | Default TOML config file. All fields are optional; missing file is silently ignored. Overridable with `-c / --config FILE`. See [Config File](#config-file) above for the full schema. |

### State Files (Runtime)

All under `${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/`. Everything is user-owned; no root state.

| Path | Created by | Purpose |
|---|---|---|
| `cookie` | `jumphost fetch-cookie` / supervisor auto-refresh (mode 600) | Persisted F5 `MRHSession` cookie consumed by `jumphost run` |
| `chromium-profile/` | `jumphost fetch-cookie` | Persistent Chromium user-data-dir reused across login attempts (renamed from `playwright-profile/`) |
| `jumphost.pid` | `just start-detached` | PID of the `nohup`-backgrounded `jumphost run` process (the binary itself no longer daemonizes) |
| `jumphost.log` | `just start-detached` | Log output when running detached (appended) |

Note: `jumphost run` does not create the PID / log files itself — they are written by the `just start-detached` recipe, which uses `nohup` and shell redirection. The binary always logs to stderr and lets the wrapper (systemd, nohup) handle persistence.

### Nix Packages (from `devenv.nix`)

| Package | Role |
|---|---|
| `openconnect` | F5 VPN client. Spawned by `jumphost run` with `--script-tun --script "ocproxy ..."`. |
| `ocproxy` | Userspace TCP/IP stack (lwIP) and SOCKS5 server. Spawned by openconnect as its `--script-tun` peer. |
| `chromium` (Linux only) | Chromium browser used by `jumphost fetch-cookie` via the Chrome DevTools Protocol (`chromiumoxide`). Exported as `CHROMIUM_PATH=${pkgs.chromium}/bin/chromium` on Linux. On macOS, Chromium is not available from nixpkgs — install Chrome or Chromium system-wide and `chromiumoxide` will auto-detect it (or set `CHROMIUM_PATH` / `CHROME` manually). |

A `flake.nix` is also available at the project root. It builds the `jumphost` binary via `rustPlatform.buildRustPackage` and wraps it with all runtime dependencies (`openconnect`, `ocproxy`, and `chromium` on Linux) on `PATH`. On macOS, Chromium is not available from nixpkgs — install Chrome or Chromium system-wide and set `CHROMIUM_PATH` (or let `chromiumoxide` auto-detect it). The flake exposes `packages.<system>.default` (and `packages.<system>.jumphost`) for x86_64-linux, aarch64-linux, x86_64-darwin, and aarch64-darwin, plus an `overlays.default` that adds `vpn-jumphost` to the package set. This is the recommended way to integrate the jumphost into a NixOS or home-manager configuration.

### External Services

| Service | URL | Role |
|---|---|---|
| VPN portal | Configured via `VPN_URL` / `vpn_url` | F5 BIG-IP APM; user authenticates here to obtain a session cookie |
| Internal resources | Configured via `[domains].proxy` | Accessed through the tunnel |

---

## Open Questions / Ambiguities

### 1. No automated tests or CI
The Rust crate has a `cargo test --release` recipe (`just test`), but there are no integration tests against a real VPN endpoint and no CI/CD pipeline. Correctness of the VPN flow is verified manually.

### 3. No IPv6 support
All configurations and PAC rules are IPv4-only. IPv6 addresses and networks are not addressed.

### 4. Cookie security
The cookie-fetch flow writes the F5 cookie to a state-dir file with mode 600. The cookie is never accepted via an environment variable, avoiding the risk of exposure through `/proc/<pid>/environ`. The persistent Chromium user-data-dir (`chromium-profile/`) also contains Microsoft SSO state — it should be treated as sensitive and is also created under the user-owned state dir.

### 5. UDP through SOCKS5
ocproxy's SOCKS5 server supports TCP only — applications that need UDP through the VPN are not served by this jumphost. The routing proxy inherits this limitation for VPN-bound traffic.
