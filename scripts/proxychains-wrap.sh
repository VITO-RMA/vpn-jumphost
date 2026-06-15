#!/usr/bin/env bash
# Run a command with proxychains through the jumphost routing proxy (:1081).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXAMPLE_CONF="$ROOT/docs/proxychains.conf.example"
PROXY_PORT="${JUMPHOST_PROXY_PORT:-1081}"

die() {
  echo "proxychains-wrap: $*" >&2
  exit 1
}

find_proxychains() {
  local cmd
  for cmd in proxychains4 proxychains proxychains-ng; do
    if command -v "$cmd" >/dev/null 2>&1; then
      echo "$cmd"
      return 0
    fi
  done
  return 1
}

find_config() {
  if [[ -n "${PROXYCHAINS_CONF:-}" && -f "$PROXYCHAINS_CONF" ]]; then
    echo "$PROXYCHAINS_CONF"
    return 0
  fi
  if [[ -f "$HOME/.proxychains/proxychains.conf" ]]; then
    echo "$HOME/.proxychains/proxychains.conf"
    return 0
  fi
  if [[ -f /etc/proxychains4.conf ]]; then
    echo /etc/proxychains4.conf
    return 0
  fi
  return 1
}

warn_if_jumphost_down() {
  if command -v nc >/dev/null 2>&1; then
    nc -z 127.0.0.1 "$PROXY_PORT" >/dev/null 2>&1 && return 0
  elif (echo >/dev/tcp/127.0.0.1/"$PROXY_PORT") >/dev/null 2>&1; then
    return 0
  fi
  echo "proxychains-wrap: warning: nothing listening on 127.0.0.1:$PROXY_PORT — is jumphost running?" >&2
}

if [[ $# -lt 1 ]]; then
  die "usage: proxychains-wrap.sh COMMAND [ARGS...]"
fi

pc="$(find_proxychains)" || die "proxychains not found (macOS: brew install proxychains-ng; Debian/Ubuntu: apt install proxychains4)"

conf="$(find_config)" || die "proxychains config not found — run: just proxychains-setup"

warn_if_jumphost_down

exec "$pc" -f "$conf" -q "$@"
