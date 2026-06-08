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
# Saves the cookie to $VPN_COOKIE_FILE (default ~/.local/state/vpn-jumphost/cookie).
fetch-cookie:
    @cargo run --release --quiet -- fetch-cookie

# Validate the current VPN cookie against the endpoint
# (exit 0 = valid, 1 = invalid, 2 = network error).
validate-cookie:
    @cargo run --release --quiet -- validate-cookie

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
    @{{ BIN }} -c docs/config.example.toml run --verbose --serve-pac {{ ARGS }}

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
    nohup {{ BIN }} -c docs/config.example.toml run --serve-pac {{ ARGS }} \
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
