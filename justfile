set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set dotenv-load

BIN := "./target/release/jumphost"

default:
    @just --list

# Full guided flow: PAC file → HTTP server → desktop proxy instructions → cookie → devenv up
# Pass `-d` to start the devenv processes detached instead of foreground.
bootstrap *ARGS: build
    @VPN_USERNAME="${VPN_USERNAME:-}" VPN_PASSWORD="${VPN_PASSWORD:-}" ./scripts/jumphost-wizard.sh {{ ARGS }}

# Build the jumphost binary (release mode). Everything below depends on this.
build:
    @cargo build --release
    @echo "Built: {{ BIN }}"

# Fetch the F5 MRHSession cookie via browser login (Chromium via CDP).
# Saves the cookie to the configured cookie file (default ~/.local/state/vpn-jumphost/cookie).
fetch-cookie:
    @cargo run --release --quiet -- fetch-cookie

# Force a fresh VPN cookie fetch using already-configured credentials.
refresh_token:
    @cargo run --release --quiet -- refresh_token

# Validate the current VPN cookie against the endpoint
# (exit 0 = valid, 1 = invalid, 2 = network error).
validate-cookie:
    @cargo run --release --quiet -- validate-cookie

# Run setup health checks (config, cookie, listeners, proxychains).
doctor:
    @cargo run --release --quiet -- doctor

# Show jumphost logs. Pass e.g. `-- -f` to follow.
logs *ARGS: build
    @{{ BIN }} logs {{ ARGS }}

# Regenerate proxy.pac with the configured domain lists.
pac-gen:
    @cargo run --release --quiet -- -c docs/config.example.toml generate-pac ./proxy.pac
    @echo "Wrote: ./proxy.pac"

# Print the PAC file to stdout (handy for piping into curl / browsers).
pac-show:
    @cargo run --release --quiet -- generate-pac

# Start the VPN jumphost supervisor in the foreground
# (openconnect + ocproxy on :1080 + routing proxy on :1081 + PAC server on :8091
#  + cookie auto-refresh + sleep/wake detection). Pass extra flags after `--`.
# Uses docs/config.example.toml for VPN URL + domain lists by default;
# override with -c or a user-local ~/.config/vpn-jumphost/config.toml.
# Ctrl-C tears the tunnel down.
start *ARGS: build
    @{{ BIN }} -c docs/config.example.toml run --verbose {{ ARGS }}

# Start the VPN jumphost supervisor in the background via nohup.
# Logs:  ~/.local/state/vpn-jumphost/jumphost.log
# PID:   ~/.local/state/vpn-jumphost/jumphost.pid
start-detached *ARGS: build
    #!/usr/bin/env bash
    set -euo pipefail
    state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost"
    mkdir -p "$state_dir"
    pid_file="$state_dir/jumphost.pid"
    log_file="$state_dir/jumphost.log"
    if [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
      echo "jumphost: already running (pid $(cat "$pid_file"))" >&2
      exit 1
    fi
    : >"$log_file"
    nohup {{ BIN }} -c docs/config.example.toml run {{ ARGS }} \
      >>"$log_file" 2>&1 &
    echo $! >"$pid_file"
    sleep 0.3
    if ! kill -0 "$(cat "$pid_file")" 2>/dev/null; then
      echo "jumphost: failed to start (see $log_file)" >&2
      rm -f "$pid_file"
      exit 1
    fi
    echo "jumphost: detached (pid $(cat "$pid_file"), log $log_file)"

# Stop the detached jumphost (sends SIGTERM to the PID in jumphost.pid).
stop:
    #!/usr/bin/env bash
    set -euo pipefail
    pid_file="${XDG_STATE_HOME:-$HOME/.local/state}/vpn-jumphost/jumphost.pid"
    if [[ -f "$pid_file" ]]; then
      pid=$(cat "$pid_file")
      if kill -0 "$pid" 2>/dev/null; then
        kill "$pid"
        echo "jumphost: sent SIGTERM to pid $pid."
      else
        rm -f "$pid_file"
        echo "jumphost: not running (stale pidfile removed)."
      fi
    else
      echo "jumphost: not running (no pidfile)."
    fi

# Smoke-test end-to-end tunnel connectivity via SOCKS5 CONNECT.
# Requires `just start` (or `just start-detached`) to be running.
# Override targets with e.g. `just test-tunnel -H my.host.example:443`.
test-tunnel *ARGS: build
    @{{ BIN }} -c docs/config.example.toml test-tunnel {{ ARGS }}

# Run the unit tests for the Rust crate.
test:
    @cargo test --release

# Smoke-test the routing SOCKS5 proxy with curl against a VPN-side URL
# (default: https://jenkins-rma.int.vito.be). Requires `just start`
# (or `just start-detached`) to be running. Prints response headers and
# body for every hop, then a summary line with status, total time, and
# the redirect-resolved URL.
test-curl URL="https://jenkins-rma.int.vito.be":
    @echo "→ curl via socks5h://127.0.0.1:1081 → {{ URL }}"
    @curl --proxy socks5h://127.0.0.1:1081 \
          --max-time 15 \
          --silent --show-error \
          --include --location \
          --write-out '\n--- HTTP %{http_code}  %{time_total}s  (%{url_effective}) ---\n' \
          {{ URL }}

# Smoke-test the routing SOCKS5 proxy by SSHing to a VPN-side host
# (default: develop.marvin.vito.local) and running a short remote
# command. Requires `just start` (or `just start-detached`) to be
# running and an SSH key trusted by the target. BatchMode prevents
# password/passphrase prompts; if your key isn't loaded you'll get a
# clear "Permission denied (publickey)" — which still proves the
# SOCKS5 → ocproxy → VPN path reached the SSH server.
# Pass a `user@host` to override the default user.
test-cluster HOST="develop.marvin.vito.local":
    @echo "→ ssh via socks5h://127.0.0.1:1081 → {{ HOST }}"
    @ssh -F /dev/null \
         -o 'ProxyCommand=nc -x 127.0.0.1:1081 -X 5 %h %p' \
         -o BatchMode=yes \
         -o ConnectTimeout=10 \
         -o StrictHostKeyChecking=accept-new \
         -o UserKnownHostsFile=/dev/null \
         -o LogLevel=ERROR \
         {{ HOST }} \
         'echo "--- $(hostname) ---"; uname -srm; uptime'

# Install docs/proxychains.conf.example to ~/.proxychains/proxychains.conf
# (skips if the file already exists). Requires proxychains-ng / proxychains4.
proxychains-setup:
    #!/usr/bin/env bash
    set -euo pipefail
    dest="$HOME/.proxychains/proxychains.conf"
    if [[ -f "$dest" ]]; then
      echo "proxychains-setup: already exists: $dest"
      exit 0
    fi
    for cmd in proxychains4 proxychains proxychains-ng; do
      if command -v "$cmd" >/dev/null 2>&1; then
        mkdir -p "$(dirname "$dest")"
        cp docs/proxychains.conf.example "$dest"
        echo "proxychains-setup: wrote $dest"
        exit 0
      fi
    done
    echo "proxychains-setup: install proxychains first (macOS: brew install proxychains-ng; Debian/Ubuntu: apt install proxychains4)" >&2
    exit 1

# Run any command through proxychains → routing proxy :1081.
# Example: just pc -- psql -h climkit.marvin.vito.local -U me -d mydb
pc +ARGS:
    @chmod +x scripts/proxychains-wrap.sh
    @./scripts/proxychains-wrap.sh {{ ARGS }}

# Launch DBeaver through proxychains. Override path with DBEAVER_BIN.
dbeaver:
    #!/usr/bin/env bash
    set -euo pipefail
    chmod +x scripts/proxychains-wrap.sh
    if [[ -n "${DBEAVER_BIN:-}" ]]; then
      exec ./scripts/proxychains-wrap.sh "$DBEAVER_BIN"
    fi
    if [[ "$(uname -s)" == Darwin && -x /Applications/DBeaver.app/Contents/MacOS/dbeaver ]]; then
      exec ./scripts/proxychains-wrap.sh /Applications/DBeaver.app/Contents/MacOS/dbeaver
    fi
    if command -v dbeaver >/dev/null 2>&1; then
      exec ./scripts/proxychains-wrap.sh "$(command -v dbeaver)"
    fi
    echo "dbeaver: not found — set DBEAVER_BIN to the DBeaver executable" >&2
    exit 1

# Smoke-test Postgres TCP reachability via the routing proxy.
test-db HOST="climkit.marvin.vito.local" PORT="5432":
    @echo "→ nc via socks5://127.0.0.1:1081 → {{ HOST }}:{{ PORT }}"
    @nc -z -X 5 -x 127.0.0.1:1081 -w 5 {{ HOST }} {{ PORT }}

# Bump the version across all packaging manifests.
# Usage: just bump_version 0.3.0
bump_version NEW_VERSION:
    @echo "Bumping version → {{ NEW_VERSION }}"
    sd '^version = "[^"]+"' 'version = "{{ NEW_VERSION }}"' Cargo.toml
    @echo "  ✓ Cargo.toml"
    sd '^Version: .+' 'Version: {{ NEW_VERSION }}' contrib/debian/control
    @echo "  ✓ contrib/debian/control"
    sd '^pkgver=.*' 'pkgver={{ NEW_VERSION }}' contrib/archlinux/PKGBUILD
    @echo "  ✓ contrib/archlinux/PKGBUILD"
    sd '^vpn-jumphost \([^)]+\)' 'vpn-jumphost ({{ NEW_VERSION }})' contrib/debian/changelog
    @echo "  ✓ contrib/debian/changelog"
    sd '<string>[^<]+</string>(\n\s+<key>CFBundleShortVersionString)' '<string>{{ NEW_VERSION }}</string>$1' contrib/macos/Info.plist
    sd '<string>[^<]+</string>(\n\s+<key>CFBundlePackageType)' '<string>{{ NEW_VERSION }}</string>$1' contrib/macos/Info.plist
    @echo "  ✓ contrib/macos/Info.plist"
    sd 'version = "[^"]+";' 'version = "{{ NEW_VERSION }}";' flake.nix
    @echo "  ✓ flake.nix"
    cargo generate-lockfile --quiet 2>/dev/null || true
    @echo "  ✓ Cargo.lock"
    @echo ""
    @echo "Done. Verify with: git diff"
