# VPN Jumphost — Functional Specification

## System Overview

The VPN Jumphost is a [devenv](https://devenv.sh)-managed toolkit that connects to an F5 BIG-IP APM VPN via OpenConnect (F5 protocol) and exposes a local SOCKS5 proxy so host applications can selectively route traffic to internal resources through the VPN tunnel. The entire runtime is a single Rust binary (`jumphost`) that supervises `openconnect` (which spawns `ocproxy`), an optional in-process routing proxy (SOCKS5 + HTTP CONNECT), and an optional in-process PAC HTTP server. The routing proxy sits in front of ocproxy on `127.0.0.1:1080` and applies per-domain routing rules: VPN-domain traffic is forwarded upstream to ocproxy, everything else connects directly. This lets any SOCKS5-capable or HTTP-CONNECT-capable tool (git, curl, SSH, Zed, VS Code, …) use a single proxy address without needing PAC support. A PAC (Proxy Auto-Configuration) subsystem is also included so browsers can use automatic proxy configuration. The host retains normal networking; only explicitly targeted domains or CIDRs traverse the tunnel.

Everything runs as the **current unprivileged user**. openconnect is started with `--script-tun --script "ocproxy ..."`, which means it does **not** create a TUN device or touch `/dev/net/tun`: it spawns `ocproxy` as a child, hands it the tunnel over a socketpair, and lets ocproxy's userspace TCP/IP stack (lwIP) terminate the VPN's IP packets. ocproxy serves SOCKS5 on `127.0.0.1:1081` directly; the routing proxy on port 1080 is the user-facing entry point.

**Platforms:** Linux and macOS are both supported. Linux is the primary daily-driver target; macOS works because the design has no Linux-specific dependencies (`openconnect --script-tun` + `ocproxy` is portable, and both packages are available from nixpkgs on `x86_64-darwin` / `aarch64-darwin`). The wizard's autoproxy-instructions step branches on `uname -s` to print OS-appropriate guidance. The sleep/wake watcher has separate OS-native implementations for each platform.

---

## Architecture

### Component Map

The project is a single Rust crate (`Cargo.toml` at the repo root) that builds one binary, `jumphost` (`target/release/jumphost`). The only host-side shell script that remains is `scripts/jumphost-wizard.sh`, which drives the first-run bootstrap and then hands off to the binary via `devenv up` or `just start`.

| Component | Role |
|---|---|
| **`jumphost` binary** (`src/main.rs`) | Multi-subcommand CLI: `run`, `fetch-cookie`, `validate-cookie`, `generate-pac`, `authenticate`, `doctor`, `test-tunnel`. The supervisor in `run` orchestrates openconnect, ocproxy, the routing proxy, the PAC server, and the cookie monitor in a single process. Run as `processes.jumphost.exec` under devenv, or directly via `just start`. |
| **`src/config.rs`** + **`src/config_file.rs`** | Shared configuration: constants, env-var helpers, and TOML config file integration. **Single source of truth** for the `PROXY_DOMAINS` / `DIRECT_DOMAINS` lists used by both the PAC generator and the routing proxy, default ports, and state-dir paths. All options can be overridden via a TOML config file at `$XDG_CONFIG_HOME/vpn-jumphost/config.toml`. Precedence: CLI flag > config file > compiled-in default. The "must stay in sync" rule between the routing proxy and the PAC file is structurally enforced — there is only one definition. |
| **`src/vpn.rs`** | openconnect process management. Spawns `openconnect --protocol=f5 --cookie-on-stdin --script-tun --script "ocproxy -D ${SOCKS_PORT} -k ${OCPROXY_KEEPALIVE}" "$VPN_URL"` with the cookie file as stdin (no pipe), tracks the child PID, and forwards SIGTERM/SIGINT/SIGHUP. ocproxy is not invoked directly — openconnect spawns it as its `--script-tun` peer. `-g` is **never** passed to ocproxy. |
| **`src/routing.rs`** | Routing proxy — SOCKS5 + HTTP CONNECT (ported from the previous standalone `routing-proxy/` crate, extended with HTTP CONNECT auto-detection). Always started; listens on `127.0.0.1:1081`. ocproxy stays on port 1080. Per-domain rules read from `PROXY_DOMAINS` / `DIRECT_DOMAINS` in `config.rs`. |
| **`src/pac.rs`** | PAC file generation **and** built-in HTTP server (replaces the old `miniserve` dependency). Pure tokio/hyper, no external process. Generates the PAC text from the same `PROXY_DOMAINS` / `DIRECT_DOMAINS` constants and serves it on `127.0.0.1:8091` when `serve_pac = true` in the config file. |
| **`src/cookie.rs`** | Cookie subsystem. Validation uses `reqwest` with rustls, **redirects disabled**, and proxy discovery disabled to probe the F5 endpoint directly (3xx = expired cookie). Browser-based fetch uses Chromium via [`chromiumoxide`](https://crates.io/crates/chromiumoxide), launched with `--no-proxy-server` so authentication never depends on the VPN/PAC/domain rules. It speaks the Chrome DevTools Protocol directly — **no Node.js driver is required** (the playwright dependency is gone). |
| **`src/jumphost.rs`** | Main supervisor module. Validates/refreshes the cookie before spawning openconnect; spawns and supervises the routing proxy and PAC server tasks; runs the periodic cookie-check loop; ties together the sleep/wake watcher (waking the loop immediately on resume so the VPN can be reconnected). |
| **`src/sleepwake/{mod,linux,macos}.rs`** | OS-native sleep/wake detection. Linux uses [`zbus`](https://crates.io/crates/zbus) to subscribe to the `org.freedesktop.login1.Manager.PrepareForSleep` signal. macOS uses [`objc2`](https://crates.io/crates/objc2) + [`block2`](https://crates.io/crates/block2) to subscribe to `NSWorkspaceDidWakeNotification`. Both platforms additionally have a portable wall-clock skew fallback in `jumphost.rs` that fires when a `time.time()`-equivalent jump larger than the threshold is observed (CLOCK_MONOTONIC stalls during Linux suspend, so a large wall-clock delta is a strong "we just woke up" signal). |
| **`src/logging.rs`** | `tracing-subscriber` bootstrap. TTY-aware ANSI colors; honors `NO_COLOR`, `FORCE_COLOR`, and `RUST_LOG` (the latter overrides `--verbose`). systemd/journald (no TTY) automatically gets plain output. |
| **`scripts/jumphost-wizard.sh`** | The only remaining shell script. Guided 4-step bootstrap that generates the PAC, captures the cookie via `jumphost fetch-cookie`, prints OS-appropriate autoproxy instructions, then `exec`s `devenv up`. |
| **`devenv.nix`** | Declares nix-provided packages (`openconnect`, `ocproxy`, `chromium`, `just`, plus the Rust toolchain), `CHROMIUM_PATH` (Linux only), and a single process: `processes.jumphost.exec = "./target/release/jumphost -c docs/config.example.toml run"`. |

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
│  │     exec ./target/release/jumphost -c ... run              │    │
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
| 1081 | `127.0.0.1` | SOCKS5 / HTTP CONNECT | routing proxy (user-facing; per-domain VPN-vs-direct routing) |
| 8091 | `127.0.0.1` | HTTP | PAC file (in-process hyper server) |

ocproxy listens on port 1080 and the routing proxy (always started) listens on port 1081. Clients point at `socks5h://127.0.0.1:1081` (SOCKS5) or set `http_proxy=http://127.0.0.1:1081` (HTTP CONNECT) — the routing proxy auto-detects the protocol and decides per-domain whether to forward upstream to ocproxy or connect directly. All listeners bind to loopback by default.

---

## Features

### 1. VPN Tunnel (OpenConnect + ocproxy)

**What it does:** Establishes an F5 BIG-IP APM VPN tunnel to the configured VPN endpoint using openconnect, and terminates the tunnel's IP packets in a userspace lwIP stack (ocproxy) so the host kernel never sees a TUN device.

**How it works:**
- The user authenticates via a browser at the VPN portal and obtains an F5 `MRHSession` cookie (typically through the Chromium-based cookie fetch in `jumphost fetch-cookie`; see Feature 5). The cookie is written to `$VPN_COOKIE_FILE` (default `$XDG_STATE_HOME/vpn-jumphost/cookie`, mode 600).
- `devenv up` launches `processes.jumphost`, whose `exec` is `./target/release/jumphost -c docs/config.example.toml run`. The example config sets `serve_pac = true`. The binary's supervisor (`src/jumphost.rs`) validates `VPN_COOKIE_FILE`, auto-refreshes the cookie if needed (see Cookie Ingestion Flow), and spawns openconnect:
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

### 2a. Routing Proxy — SOCKS5 + HTTP CONNECT (`src/routing.rs`)

**What it does:** A system-wide proxy on `127.0.0.1:1081` (always started by the supervisor) that accepts both **SOCKS5** and **HTTP CONNECT** requests and applies per-domain routing rules. The protocol is auto-detected by peeking the first byte of each connection (`0x05` → SOCKS5, ASCII letter → HTTP CONNECT). VPN-domain traffic is forwarded upstream to ocproxy on port 1080; everything else connects directly. This eliminates the need for PAC support in non-browser tools (git, curl, apt, SSH, Zed, VS Code, etc.).

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
**Outputs:** SOCKS5 + HTTP CONNECT listener on `127.0.0.1:1081`.

**Client usage:** All tools can point at `socks5h://127.0.0.1:1081` (SOCKS5) or `http://127.0.0.1:1081` (HTTP CONNECT) — the routing proxy handles the VPN-vs-direct decision transparently:
```bash
# SOCKS5 — git
git config --global http.proxy socks5h://127.0.0.1:1081

# SOCKS5 — curl
curl --proxy socks5h://127.0.0.1:1081 https://internal.example.com

# SOCKS5 — SSH ProxyCommand
ssh -o 'ProxyCommand=nc -x 127.0.0.1:1081 -X 5 %h %p' host.example.com

# SOCKS5 — environment variable (many tools)
export ALL_PROXY=socks5h://127.0.0.1:1081

# HTTP CONNECT — desktop apps (Zed, VS Code, Electron apps, etc.)
export http_proxy=http://127.0.0.1:1081
export https_proxy=http://127.0.0.1:1081
```

### 3. PAC File Generation (`jumphost generate-pac`)

**What it does:** Generates a JavaScript PAC file that instructs browsers which traffic to proxy and which to send direct.

**How it works:**
- Implemented in `src/pac.rs`. The generator reads `PROXY_DOMAINS` and `DIRECT_DOMAINS` from `src/config.rs` (the same constants the routing proxy uses). The proxy chain is `SOCKS5 127.0.0.1:<socks_port>; DIRECT` — pointing at **ocproxy** (default port 1080), not the routing proxy. The PAC script already selects VPN vs direct domains; the routing proxy on 1081 is for non-PAC clients (curl, git, SSH).
- Domain lists (loaded from `config.toml` at startup):
  - `PROXY_DOMAINS` = configured via `[domains].proxy` in `config.toml` (no compiled-in defaults; see [`docs/config.example.toml`](docs/config.example.toml) for VITO defaults)
  - `DIRECT_DOMAINS` = configured via `[domains].direct` in `config.toml` (no compiled-in defaults)
- Outputs a `FindProxyForURL` function with these rules (evaluated in order):
  1. Always-DIRECT domains (from `[domains].direct`; checked before DNS resolution, plus a `url.indexOf` fallback for the VPN portal hostname in the full URL).
  2. Matched domains → `SOCKS5 127.0.0.1:<socks_port>; DIRECT`.
  3. Everything else → `DIRECT`.
- Domain matching uses `host === "X" || dnsDomainIs(host, ".X")` for bare domains.

**CLI:** `jumphost generate-pac [PATH]` — writes the PAC text to `PATH` if given, otherwise stdout.
**Inputs:** `socks_port`, domain lists, output path.
**Outputs:** A `.pac` JavaScript file.

### 4. PAC Serving (Host-Side Loopback)

**What it does:** Serves PAC files on `http://127.0.0.1:8091/` independently of the VPN tunnel lifecycle, so browsers can always fetch the PAC even when the VPN is down (which keeps the direct-domain rules in effect).

**How it works:**
- Implemented in `src/pac.rs` using `tokio` + `hyper` — there is no `miniserve` dependency any more, and no external process.
- Inside `jumphost run`, the PAC server is started as a tokio task when `serve_pac = true` in the config file (the example config enables it). It serves the generated PAC text at `/proxy.pac` (and `/`), with `Content-Type: application/x-ns-proxy-autoconfig`.
- The PAC content is regenerated from `config.rs` constants at startup; it does not need to be on disk.
- If you want to use a file based pac config, run `just pac-gen` and point your browser at that file instead.

### 5. Browser-Based Cookie Fetch (`jumphost fetch-cookie`)

**What it does:** Opens a Chromium browser window and waits for the user to complete SSO authentication (including Microsoft Authenticator MFA). Once the F5 `MRHSession` cookie is set on the VPN portal, it is written to the output file (mode 600) and Chromium closes.

**How it works:**
- Implemented in `src/cookie.rs` using the [`chromiumoxide`](https://crates.io/crates/chromiumoxide) crate, which speaks the Chrome DevTools Protocol directly over a WebSocket. **No Node.js driver is required** (the previous playwright + Firefox stack has been removed).
- Launches the Chromium executable in **headed mode** by default (a visible browser window). The dedicated authentication browser is always passed `--no-proxy-server`, so the VPN portal, SSO redirects, and MFA endpoints connect directly and never consult the system PAC, proxy environment, routing proxy, or `[domains]` rules. This avoids a circular dependency while the VPN is down. Pass `--headless` or set `JUMPHOST_HEADLESS=1` to launch headless instead (no visible window). Headless mode only works when `VPN_USERNAME` + `VPN_PASSWORD` are set so the CDP automation can complete SSO without user interaction. **MFA auto-detection:** during a headless session, the poll loop distinguishes three MFA phases: (1) the **method-picker** screen ("Verify your identity" — choose Authenticator app vs. verification code) is handled automatically by clicking the "Approve a request on my Microsoft Authenticator app" option to trigger the push notification; (2) the **number-match screen** (Authenticator push — shows a number the user must tap on their phone) is handled entirely headless: the number is extracted from the page DOM, printed to the terminal, and delivered as a **desktop notification** via the [`notify-rust`](https://crates.io/crates/notify-rust) crate (talks D-Bus directly on Linux, Notification Center on macOS — **no external `notify-send` binary needed**). The notification title and body both include the extracted number so the visible banner and shell output are derived from the same value. Linux keeps a revocable notification handle and closes the banner after login succeeds; macOS delivers the notification immediately and does not retain the handle because the legacy Notification Center backend cannot revoke delivered notifications. The number is also logged via `tracing::info` for journal consumers; the browser stays headless and keeps polling for the cookie; (3) **interactive prompts** (TOTP code entry, phone-call verification) cannot be completed without user input, so the headless browser is closed and relaunched in headed mode for the user to type the code. The Authenticator push flow (the common case) therefore works **entirely headless** — only TOTP/interactive prompts cause a headed relaunch. The Chromium path is taken from `--chromium`, then `$CHROMIUM_PATH`, then `$CHROME`. `devenv.nix` exports `CHROMIUM_PATH=${pkgs.chromium}/bin/chromium`.
- Uses a **persistent Chromium user-data-dir** so session state can be reused across runs (fixed path: `$XDG_STATE_HOME/vpn-jumphost/chromium-profile`). Note: the directory name changed from `playwright-profile` to `chromium-profile`.
- Navigates to the configured `VPN_URL`; the browser presents the SAML / Microsoft SSO login flow.
- When `VPN_USERNAME` / `VPN_PASSWORD` are set, automation handles both account-picker and credential forms via CDP.
- Polls the browser-wide CDP cookie store (`Storage.getCookies`) for the `MRHSession` cookie (up to `--max-wait` seconds; default 300). The page-scoped `Network.getCookies` is **not** used: it returns no rows when invoked on the browser root target.
- **MFA notification safety limit:** Authenticator push attempts are counted in both headed and headless flows and are limited to three for one `jumphost run` process. A method-picker page is clicked only once until it transitions, preventing a lingering DOM from generating rapid duplicate pushes. The supervisor shares the counter across browser refresh sessions; after three uncompleted notifications it stops launching authentication attempts. A successfully captured or externally supplied valid cookie resets the counter. An explicit supervisor restart also starts a new three-attempt allowance.
- Writes the cookie value to the fixed cookie path (`$XDG_STATE_HOME/vpn-jumphost/cookie`), creating parent directories with `umask 077` (final file mode `600`). Status messages go to stderr via the `tracing` subscriber.

**CLI flags:**
- `--headless` — launch Chromium in headless mode (no visible window). Requires `VPN_USERNAME` + `VPN_PASSWORD` for unattended SSO. The Authenticator push MFA flow works entirely headless (the number-match value is printed to the terminal and shown in a desktop notification via `notify-rust`); only TOTP/interactive MFA prompts cause an automatic headed relaunch. Default is `false`; also settable via `JUMPHOST_HEADLESS=1`.

**Standalone usage:**
```bash
just fetch-cookie
# or:
./target/release/jumphost fetch-cookie -o ~/.local/state/vpn-jumphost/cookie
```

Note: `jumphost run` invokes the same validate / fetch flow internally — there is no separate shell glue. When the supervisor (`jumphost run`) refreshes a cookie, it automatically uses headless mode if both `VPN_USERNAME` and `VPN_PASSWORD` are set and non-empty, so periodic background refreshes don't pop up a browser window. The Authenticator push flow (the common case) works **entirely headless**: the number-match value is printed to the terminal, delivered as a desktop notification (via `notify-rust` — D-Bus on Linux, Notification Center on macOS), and logged to the journal, while the browser stays headless and polls for completion. Only TOTP or other interactive MFA prompts cause a headed relaunch for user interaction. **Escape hatch:** if the headless MFA flow is unstable, pass `--no-headless` to the `run` subcommand (or set `JUMPHOST_NO_HEADLESS=1`) to force the supervisor to always open a visible browser window for cookie refresh, even when credentials are available.

### 6. Guided Bootstrap Wizard (`jumphost-wizard.sh`)

**What it does:** Walks the user through a 4-step interactive flow to set up and start the jumphost.

**Steps:**
1. **PAC on disk** — generates `proxy.pac` in the repo root if it does not exist (using `./target/release/jumphost generate-pac proxy.pac`); skips if present.
2. **Serve PAC over HTTP** — informs the user that the PAC will be served by `devenv up` (the `jumphost` process with `serve_pac = true` in config) on `http://127.0.0.1:8091/proxy.pac` once it is started in Step 4. There is no longer a standalone PAC server to start here.
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
| `just doctor` | `./target/release/jumphost doctor` — health checks for config, cookie, routing proxy, VPN tunnel, PAC server (if enabled), and proxychains setup. Exit 0 when all critical checks pass. |
| `just test-tunnel [ARGS]` | `./target/release/jumphost test-tunnel` — SOCKS5 CONNECT probes through the routing proxy. Requires `jumphost run` to be up and `[probe].hosts` in config (or `-H host[:port]`). Pass `--retries N` to poll until the tunnel is ready. |
| `just pac-gen` | `./target/release/jumphost generate-pac proxy.pac` |
| `just pac-show` | Prints the generated PAC text to stdout |
| `just start` | Runs `./target/release/jumphost -c docs/config.example.toml run` in the foreground (Ctrl-C to stop). Starts openconnect + ocproxy, the routing proxy on `127.0.0.1:1081`, and the loopback PAC server (example config sets `serve_pac = true`). Uses the example config for VPN URL + domain lists; override with a user-local `config.toml` or env vars. |
| `just start-detached` | Wraps the same `jumphost run` command with `nohup`, redirecting stdout/stderr to `~/.local/state/vpn-jumphost/jumphost.log` and writing the PID to `jumphost.pid`. The binary itself no longer daemonizes (the old `-d/--daemonize` flag is gone); the recipe handles backgrounding. |
| `just stop` | Reads PID from `~/.local/state/vpn-jumphost/jumphost.pid` and sends SIGTERM, tearing down the tunnel, routing proxy, and PAC server. |
| `just test-curl [URL]` | Smoke-test HTTP via `socks5h://127.0.0.1:1081` (default URL: `https://jenkins-rma.int.vito.be`). |
| `just test-cluster [HOST]` | Smoke-test SSH via routing proxy (default: `develop.marvin.vito.local`). |
| `just test-db [HOST] [PORT]` | Smoke-test Postgres TCP via routing proxy (default: `climkit.marvin.vito.local:5432`). |
| `just proxychains-setup` | Copy `docs/proxychains.conf.example` to `~/.proxychains/proxychains.conf` if missing. |
| `just pc -- COMMAND …` | Run any command through `scripts/proxychains-wrap.sh` → proxychains → `:1081`. |
| `just dbeaver` | Launch DBeaver through proxychains (`DBEAVER_BIN` overrides auto-detect). |
| `just current-version` | Prints the latest semver release tag from git |
| `just release major\|minor\|patch` | Validates level / main / clean state, computes the next semver tag, then tags and pushes `main` + tag |

### 7a. Continuous Integration

The GitHub Actions workflow in `.github/workflows/ci.yml` builds and tests on `ubuntu-latest` and `macos-latest`. It installs the pinned `devenv` package using a plain Bash shell, then runs `devenv tasks run --no-tui ci:checks`. The installation step must not use a `devenv` shell wrapper because `devenv` is not available until installation completes. The `ci:checks` task runs the release Cargo build followed by the release test suite.

### 8. Keyring Credential Storage (`jumphost authenticate`)

**What it does:** Stores VPN credentials (username and password) in the OS keyring so they can be used for automated cookie refresh without environment variables or plaintext config files.

**How it works:**
- Implemented using the [`keyring-core`](https://crates.io/crates/keyring-core) crate (v1) with platform-specific store backends:
  - **macOS:** `apple-native-keyring-store` — stores credentials in macOS Keychain.
  - **Linux:** `dbus-secret-service-keyring-store` — stores credentials via the Secret Service API (GNOME Keyring / KWallet).
- `jumphost authenticate` prompts interactively for username and password, then writes them to the keyring.
- `jumphost authenticate --from-env` reads `VPN_USERNAME` and `VPN_PASSWORD` from the environment (both must be non-empty), stores them in the keyring, and skips the interactive prompt.
- `jumphost deauthenticate` removes stored credentials from the keyring and deletes the cookie file and browser profile.
- The keyring is checked as a credential source between env vars and the config file:
  1. `VPN_USERNAME` / `VPN_PASSWORD` env vars (highest priority)
  2. OS keyring (macOS Keychain / Linux Secret Service)
  3. Config file `[credentials]` table (value or `*_file` path)

**CLI flags:**
- `--from-env` — read credentials from `VPN_USERNAME` and `VPN_PASSWORD` instead of prompting.
- `--no-headless` — open a visible browser window for the post-store cookie fetch (default is headless when credentials are available).

**Standalone usage:**
```bash
# Store credentials (interactive prompt)
jumphost authenticate

# Store credentials from environment variables
VPN_USERNAME=you@company.com VPN_PASSWORD=secret jumphost authenticate --from-env

# Remove stored credentials and cookie
jumphost deauthenticate
```

### 9. SSH Proxy Configuration

**What it does:** Provides an OpenSSH client config file for routing SSH connections through the jumphost's SOCKS5 proxy.

**How it works:**
- 
  ```
  ProxyCommand /usr/bin/nc -x 127.0.0.1:1081 -X 5 %h %p
  ```
  i.e. SOCKS5 via the routing proxy (which transparently forwards to ocproxy).
- The `Host *` stanza at the end carries shared defaults (identity, ControlMaster, keepalives).

**Usage:** Copy into `~/.ssh/config` or use OpenSSH `Include` directive. See [docs/ssh.md](docs/ssh.md).

### 10. Database clients (PostgreSQL / DBeaver)

**What it does:** Documents how to reach internal PostgreSQL databases through the routing proxy when the client does not support SOCKS5 natively (typical for DBeaver, DataGrip, pgAdmin, and most JDBC drivers).

**How it works:**
- Wrap the client with **proxychains** or **Proxifier** so all outbound TCP uses SOCKS5 `127.0.0.1:1081`.
- Enable `proxy_dns` in proxychains so internal hostnames (e.g. `*.vito.local`) resolve through the VPN.
- Create connections with the real database hostname and port; leave SSH tunneling disabled in the client.
- Scales to many databases: one wrapper setup, unlimited connections by hostname.

**Usage:** See [docs/databases.md](docs/databases.md). Helpers: `just proxychains-setup`, `just doctor`, `just test-tunnel`, `just pc`, `just dbeaver`, `just test-db`, [`scripts/proxychains-wrap.sh`](scripts/proxychains-wrap.sh), [`docs/proxychains.conf.example`](docs/proxychains.conf.example).

### 11. Setup health check (`jumphost doctor`)

**What it does:** Prints a one-screen diagnostic of common setup problems before debugging SOCKS or database clients.

**Checks (implemented in `src/doctor.rs`):**
- Config file exists; `vpn_url` and `[domains].proxy` are set.
- VPN credentials present (warn if missing).
- Cookie file exists and validates against the VPN endpoint (fail if invalid; warn if missing or network error).
- Routing proxy listening on configured `routing_proxy` bind/port (fail if down).
- ocproxy SOCKS port listening (warn if down — tunnel still starting or not connected).
- PAC HTTP server when `serve_pac = true` (warn if down).
- `proxychains` binary on `PATH` and config file with `proxy_dns` + `socks5 127.0.0.1:<routing_proxy.port>` (warn if missing — optional for database clients).

**Exit codes:** 0 = all critical checks passed; 1 = one or more critical checks failed.

**Usage:** `jumphost doctor` or `just doctor`.

### 12. Tunnel probe (`jumphost test-tunnel`)

**What it does:** Verifies end-to-end connectivity through the routing proxy by issuing SOCKS5 `CONNECT` requests to configured hosts (domain-name ATYP, i.e. `socks5h` semantics). Unlike `doctor`, this proves that routing, upstream ocproxy, and VPN DNS work — not merely that a port is listening.

**Probe targets** (when no `-H` flags are passed): read from `[probe].hosts` in the config file. There are no compiled-in defaults — configure at least one direct target (typically the VPN portal on 443) and one tunnel target (a host from `[domains].proxy`). See `docs/config.example.toml`.

**CLI flags:** `-H/--host HOST[:PORT]` (repeatable, overrides config), `--timeout SECS`, `--retries N`, `--require-any`, `--require-all` (default), `-q/--quiet`.

**Exit codes:** 0 = probes passed (all by default, or any with `--require-any`); 1 = one or more probes failed; 2 = routing proxy not reachable (start `jumphost run` first).

**Usage:** `jumphost test-tunnel` or `just test-tunnel`. For HTTP/SSH/Postgres smoke tests that need external tools, use `just test-curl`, `just test-cluster`, and `just test-db`.

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
                           └─ ./target/release/jumphost -c docs/config.example.toml run
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

The supervisor (`src/jumphost.rs` for `run`, `src/cookie.rs` for the primitives) reads the cookie from the configured cookie file path:

1. **Cookie file** — openconnect reads the cookie directly from this file via stdin (the file is opened in `src/vpn.rs` and assigned to the child's `stdin`). Fixed path: `${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/cookie`. The wizard (and `jumphost fetch-cookie`) write here with `umask 077`.

Before starting openconnect, the supervisor validates the cookie and auto-refreshes it if needed:

1. **Validate** — `src/cookie.rs` probes the VPN endpoint (`$VPN_URL/vdesk/vpn/index.php3?outform=xml` with the `MRHSession` cookie) using `reqwest` configured with `rustls` and `redirect::Policy::none()`. **HTTP redirects are deliberately not followed** — a 3xx response (the F5 gateway redirecting to the SSO login page) means the cookie is expired or invalid. A 404 also indicates an expired/invalid cookie. Network errors (DNS failure, timeout, etc.) are **not** treated as an invalid cookie — the supervisor proceeds with the existing cookie so transient connectivity blips don't force a re-login. The same logic is exposed via `jumphost validate-cookie` (exit codes: 0 valid, 1 invalid, 2 network error).
2. **Refresh** — if the cookie file is missing, empty, or the validation returned 404/3xx, the supervisor automatically runs the same code path as `jumphost fetch-cookie` to open Chromium for SSO login and capture a fresh cookie, writing it back to the configured cookie file atomically. Cookie validation disables reqwest proxy discovery, and Chromium is launched with `--no-proxy-server`; the complete authentication flow therefore connects directly regardless of the host's PAC/proxy settings or `[domains]` configuration. If `VPN_USERNAME` or `VPN_PASSWORD` are set (via env var, OS keyring, or config file), they are forwarded to the cookie-fetch code to pre-fill the login form. Authenticator pushes are limited to three across all refresh sessions in one supervisor process.
3. **Unavailable / paused** — if auto-refresh fails, `jumphost run` remains alive with the routing proxy and optional PAC server running, but does not start the VPN until a valid cookie is available. This avoids systemd/launchd `Restart=on-failure` loops resetting the MFA budget. After three uncompleted Authenticator notifications, automatic authentication is paused for the rest of the process lifetime. Running `jumphost authenticate` manually writes a fresh cookie; the monitor detects it and starts the VPN. Explicitly restarting the supervisor resets the three-attempt allowance. Standalone `jumphost authenticate` still returns a non-zero status when its cookie fetch fails.

There is no `VPN_COOKIE` env-var fallback and no interactive stdin fallback (process-compose has no useful TTY).

### Process Lifecycle

1. `devenv up` launches process-compose, which starts the single declared process `jumphost` (`exec = "./target/release/jumphost -c docs/config.example.toml run"`).
2. The supervisor in `src/jumphost.rs` validates the existing cookie and auto-refreshes it via the Chromium flow if it is expired or missing. It always starts the routing proxy and configured PAC HTTP server. After the cookie is confirmed valid, it spawns `openconnect` (which spawns `ocproxy`). If authentication is unavailable or reaches the three-notification MFA limit, the supervisor stays alive without the VPN and waits for a valid cookie rather than exiting into a service-manager restart loop.
3. ocproxy serves SOCKS5 on `127.0.0.1:${SOCKS_PORT}` (default 1080). The routing proxy (always started) listens on `127.0.0.1:1081`.
4. The PAC HTTP server runs on `127.0.0.1:${PAC_SERVE_PORT}` (default 8091) in the same process — no external `miniserve`.
5. Signal handling:
   - SIGTERM / SIGINT to `jumphost` → supervisor forwards SIGTERM to openconnect; openconnect tears the tunnel down; ocproxy's `VPNFD` socketpair closes and ocproxy exits; the routing proxy and PAC tasks are cancelled; the process exits. The cookie fetch flow also respects SIGTERM/SIGINT: a `CancellationToken` is threaded through `fetch` → `launch_and_fetch` → `fetch_inner`, and the poll loop uses `tokio::select!` to check for cancellation alongside its 1-second sleep. This means `jumphost run` can shut down cleanly even while a cookie refresh is in progress (the browser is always closed cleanly on interruption), and `jumphost fetch-cookie` responds to Ctrl-C at any point during the SSO/MFA flow.
   - openconnect/ocproxy crash → the supervisor notices, logs the exit status, and (in `run`'s monitor loop) considers it a hard failure unless the loop is configured to restart. process-compose's restart policy is `availability.restart = "no"` / `max_restarts = 0` in `devenv.nix`, so transient failures don't loop — fix the underlying issue (server unreachable, etc.) and re-run `devenv up`. Cookie refresh happens automatically on restart so an expired cookie alone won't block you.

### Standalone Supervisor (`jumphost run` outside `devenv up`)

A single-process supervisor for environments where `process-compose` isn't desired — most notably as a `systemctl --user` unit (see `contrib/vpn-jumphost.service.example`) or under `nohup` via `just start-detached`. The `jumphost run` subcommand:

1. Resolves the cookie path: `$XDG_STATE_HOME/vpn-jumphost/cookie` (fixed; not configurable).
2. Validates the cookie via the in-process validator. Refreshes via the Chromium cookie-fetch flow on "invalid" results; on network errors keeps the existing cookie. Authenticator push attempts share a process-wide budget of three. If refresh fails or that budget is exhausted, the supervisor stays alive without a tunnel and continues checking for a manually supplied valid cookie; it does not exit into a systemd/launchd restart loop.
3. Starts `openconnect --protocol=f5 --cookie-on-stdin --script-tun --script "ocproxy -D ${SOCKS_PORT} -k ${OCPROXY_KEEPALIVE}" "$VPN_URL"` with the cookie file connected to stdin. openconnect spawns ocproxy (SOCKS5 on `127.0.0.1:${socks_port}`) as its `--script-tun` peer. The supervisor tracks openconnect's PID and tears it down on SIGTERM/SIGINT/SIGHUP.
4. Starts the routing proxy task on `127.0.0.1:1081` (configurable via `routing_proxy.port` in config.toml). ocproxy listens on port 1080 (configurable via `socks_port` in config.toml).
5. If `serve_pac = true` in the config file, also starts the in-process PAC HTTP server on the configured `pac_server.port` (default 8091).
6. A monitor task polls every `min(15s, check_interval/4)`:
   - It is woken **immediately** by an OS-native sleep/wake watcher when available:
     - **Linux** (`src/sleepwake/linux.rs`): uses [`zbus`](https://crates.io/crates/zbus) to subscribe to `org.freedesktop.login1.Manager.PrepareForSleep` on the system bus. The signal arrives with `false` on resume; the watcher calls `on_resume`.
     - **macOS** (`src/sleepwake/macos.rs`): uses [`objc2`](https://crates.io/crates/objc2) + [`block2`](https://crates.io/crates/block2) to register a Cocoa observer for `NSWorkspaceDidWakeNotification`.
     - On any other platform, or if subscription fails (e.g. headless container, no D-Bus), it silently falls back to wall-clock skew detection only.
   - As a portable fallback (and corroboration) it also watches for wall-clock skew larger than `max(poll_interval*4, 30s)` — CLOCK_MONOTONIC stalls during Linux suspend, so a large wall-clock delta is a strong signal that we just woke up.
   - If openconnect died, it validates/refreshes the cookie and (re)starts it. When the three-attempt MFA budget is exhausted, browser refreshes remain paused, but the loop still validates the cookie so `jumphost authenticate` can restore the tunnel without restarting the supervisor. If the routing proxy or PAC server task panicked, it is logged.
   - At least every `check_interval` seconds (default 300, configurable via `check_interval` in config.toml), it re-validates the cookie. After a resume the validation retries on network errors with exponential backoff for up to 60 s, since routing/DNS can take a couple of seconds to come back up after the laptop wakes. On a successful resume validation (cookie still valid), **the VPN is restarted unconditionally** because the TCP/TLS session to the F5 gateway is almost certainly dead after suspend (NAT entries flushed, server may have closed the connection) and openconnect's own dead-peer detection can take minutes. If a periodic (non-forced) validation hits a network error, the next short poll iteration retries within `poll_interval` rather than waiting another full `check_interval`. If validation returns "invalid", the cookie is refreshed and the VPN is restarted.

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

**Precedence (highest → lowest):** CLI flag > config file > compiled-in default.

The binary loads a `.env` file from the current working directory at startup (via the `dotenvy` crate), before CLI parsing. This is used for `VPN_USERNAME` and `VPN_PASSWORD` — the only settings still read from the environment. Missing `.env` files are silently ignored.

Example `config.toml`:

```toml
vpn_url = "https://vpn.example.com"
vpn_protocol = "f5"
socks_port = 1080
ocproxy_keepalive = 60
check_interval = 300
no_headless = false
serve_pac = true
chromium_path = "/usr/bin/chromium"
verbose = false

[routing_proxy]
bind = "127.0.0.1"
port = 1081

[pac_server]
bind = "127.0.0.1"
port = 8091

[domains]
proxy = ["example.com", "corp.local", "internal.example.com"]
direct = ["vpn.example.com"]

[credentials]
username = "user@example.com"
password_file = "/run/secrets/vpn_pass"
```

#### Config file fields

| Field | Type | Description |
|---|---|---|
| `vpn_url` | string | F5 VPN endpoint URL |
| `vpn_protocol` | string | OpenConnect protocol |
| `socks_port` | integer | ocproxy SOCKS5 listen port |
| `ocproxy_keepalive` | integer | TCP keepalive (seconds) for ocproxy `-k` |
| `check_interval` | float | Supervisor cookie-check interval (seconds) |
| `no_headless` | boolean | Disable headless cookie refresh |
| `serve_pac` | boolean | Start the in-process PAC HTTP server (default: false) |
| `chromium_path` | path | Path to Chromium executable |
| `verbose` | boolean | Enable debug-level (verbose) logging (same as `--verbose`) |
| `routing_proxy.bind` | string | Routing proxy bind address |
| `routing_proxy.port` | integer | Routing proxy listen port |
| `pac_server.bind` | string | PAC HTTP server bind address |
| `pac_server.port` | integer | PAC HTTP server listen port |
| `domains.proxy` | array of strings | Domains routed through VPN (no compiled-in default; must be configured) |
| `domains.direct` | array of strings | Domains always reached directly (no compiled-in default) |
| `credentials.username` | string | VPN username (also settable via `VPN_USERNAME` env var or OS keyring) |
| `credentials.password` | string | VPN password (also settable via `VPN_PASSWORD` env var or OS keyring) |
| `credentials.username_file` | path | Path to file containing username |
| `credentials.password_file` | path | Path to file containing password |
| `probe.hosts` | array of strings | Probe targets as `host` or `host:port` (port defaults to 443). Required for `test-tunnel` unless `-H` is passed on the CLI. |
| `probe.timeout_secs` | integer | Per-probe SOCKS5 connect timeout (default: 10) |
| `probe.retries` | integer | Additional retries per failed probe (default: 0) |

The `[domains]` table is the way to set proxy and direct domain lists. The domain lists are cached on first access and remain stable for the process lifetime.

### Environment Variables

Most configuration is done via the config file or CLI flags. Only two environment variables are supported:

| Variable | Description |
|---|---|
| `VPN_USERNAME` | VPN username for automated browser login. Also settable via `[credentials] username` in the config file. |
| `VPN_PASSWORD` | VPN password for automated browser login. Also settable via `[credentials] password` in the config file. |

**Precedence:** `VPN_USERNAME` / `VPN_PASSWORD` env vars > OS keyring (`jumphost authenticate`) > config file `[credentials]` `username_file` / `password_file`. Each source must supply both username and password; sources are never mixed.

Standard system variables (`XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `RUST_LOG`, `NO_COLOR`, `FORCE_COLOR`, `PATH`) are honored as usual but are not application-specific configuration.

CLI overview: `-c/--config FILE`, `--no-headless`, `-v/--verbose`. The `-c` and `-v` flags are global (accepted before or after any subcommand). PAC serving is controlled by `serve_pac` in the config file. The previous `-d/--daemonize` flag is **gone** — use `just start-detached` (nohup) or a systemd unit instead.

`fetch-cookie` CLI: `--headless`.

`authenticate` CLI: `--from-env` (read `VPN_USERNAME` / `VPN_PASSWORD` instead of prompting), `--no-headless`.

`test-tunnel` CLI: `-H/--host HOST[:PORT]` (repeatable), `--timeout SECS`, `--retries N`, `--require-any`, `--require-all`, `-q/--quiet`.

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

### 1. No real VPN integration tests
The Rust crate has a `cargo test --release` recipe (`just test`), CI builds/tests the Rust crate and Debian package, and local Docker harnesses verify the Arch and Debian package install surfaces. There are still no integration tests against a real F5 VPN endpoint. Correctness of the live VPN login/tunnel flow is verified manually.

### 3. No IPv6 support
All configurations and PAC rules are IPv4-only. IPv6 addresses and networks are not addressed.

### 4. Cookie security
The cookie-fetch flow writes the F5 cookie to a state-dir file with mode 600. The cookie is never accepted via an environment variable, avoiding the risk of exposure through `/proc/<pid>/environ`. The persistent Chromium user-data-dir (`chromium-profile/`) also contains Microsoft SSO state — it should be treated as sensitive and is also created under the user-owned state dir.

### 5. UDP through SOCKS5
ocproxy's SOCKS5 server supports TCP only — applications that need UDP through the VPN are not served by this jumphost. The routing proxy inherits this limitation for VPN-bound traffic.
