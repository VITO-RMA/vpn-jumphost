# Debian Package

The project ships Debian packaging files at [`contrib/debian/`](../contrib/debian/) for building a `.deb` package. The package installs the `jumphost` binary to `/usr/bin/jumphost` along with a systemd user unit, example config, and documentation — the same contents as the [Arch Linux package](arch.md).

## Building the `.deb`

The intended build method uses `dpkg-deb --build` after staging the package tree. A typical build session on Ubuntu 26.04 would look like:

```bash
# Build the binary
cargo build --release --locked

# Stage the package tree
PKG=pkg-root
mkdir -p "$PKG/DEBIAN"
mkdir -p "$PKG/usr/bin"
mkdir -p "$PKG/usr/lib/systemd/user"
mkdir -p "$PKG/usr/share/vpn-jumphost"
mkdir -p "$PKG/usr/share/doc/vpn-jumphost"

cp contrib/debian/control         "$PKG/DEBIAN/control"
cp contrib/debian/postinst        "$PKG/DEBIAN/postinst"
cp contrib/debian/prerm           "$PKG/DEBIAN/prerm"
cp contrib/debian/postrm          "$PKG/DEBIAN/postrm"

install -m 755 target/release/jumphost                "$PKG/usr/bin/jumphost"
install -m 644 contrib/debian/vpn-jumphost.service     "$PKG/usr/lib/systemd/user/vpn-jumphost.service"
install -m 644 docs/config.example.toml                "$PKG/usr/share/vpn-jumphost/config.example.toml"
install -m 644 README.md                               "$PKG/usr/share/doc/vpn-jumphost/README.md"
install -m 644 spec.md                                 "$PKG/usr/share/doc/vpn-jumphost/spec.md"
install -m 644 docs/architecture.md                    "$PKG/usr/share/doc/vpn-jumphost/architecture.md"
install -m 644 docs/ssh.md                             "$PKG/usr/share/doc/vpn-jumphost/ssh.md"

# Build the .deb
dpkg-deb --build "$PKG" vpn-jumphost_0.2.0_amd64.deb
```

Install with:

```bash
sudo dpkg -i vpn-jumphost_0.2.0_amd64.deb
sudo apt-get install -f   # resolve any missing dependencies
```

## Package contents

| Installed path | Source |
|---|---|
| `/usr/bin/jumphost` | `target/release/jumphost` |
| `/usr/lib/systemd/user/vpn-jumphost.service` | `contrib/debian/vpn-jumphost.service` |
| `/usr/share/vpn-jumphost/config.example.toml` | `docs/config.example.toml` |
| `/usr/share/doc/vpn-jumphost/README.md` | `README.md` |
| `/usr/share/doc/vpn-jumphost/spec.md` | `spec.md` |
| `/usr/share/doc/vpn-jumphost/architecture.md` | `docs/architecture.md` |
| `/usr/share/doc/vpn-jumphost/ssh.md` | `docs/ssh.md` |

## Dependencies

| Field | Packages | Notes |
|---|---|---|
| `Depends` | `openconnect`, `ocproxy`, `chromium \| chromium-browser` | Required at runtime. `ocproxy` is available in Ubuntu/Debian repos (unlike Arch where it's AUR-only). `chromium` is the Ubuntu package name; `chromium-browser` covers derivatives. |
| Build-time | `cargo` (Rust toolchain) | Not declared in the control file — handled by the CI workflow. |

## Post-install setup

After installing the package:

1. **Copy and edit the example config:**

   ```bash
   mkdir -p ~/.config/vpn-jumphost
   cp /usr/share/vpn-jumphost/config.example.toml \
      ~/.config/vpn-jumphost/config.toml
   ```

   At minimum, set `vpn_url` and the `[domains]` table for your VPN endpoint.

2. **Enable and start the systemd user service:**

   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now vpn-jumphost.service
   ```

3. **View logs:**

   ```bash
   journalctl --user -u vpn-jumphost.service -f
   ```

## Systemd user unit

The package installs a production-ready systemd **user** unit to `/usr/lib/systemd/user/vpn-jumphost.service`. It runs:

```
ExecStart=/usr/bin/jumphost run --serve-pac
```

No path customization is needed — the binary and its runtime dependencies (`openconnect`, `ocproxy`, `chromium`) are all on the system `PATH` after package installation. The unit is `Type=simple` with `Restart=on-failure` and sends `SIGTERM` for clean shutdown.

Configuration is done entirely through the TOML config file at `~/.config/vpn-jumphost/config.toml` (or via `-c /path/to/config.toml`). See [`config.example.toml`](config.example.toml) for the full schema.

## Maintainer scripts

The package includes dpkg maintainer scripts:

| Script | Trigger | Action |
|---|---|---|
| `postinst` | `configure` | Prints setup instructions (config copy, service enable) |
| `prerm` | `remove` | Reminds user to stop/disable the systemd user service (dpkg runs as root and cannot manage per-user services automatically) |
| `postrm` | `remove` / `purge` | Notes that user config/state files remain and can be removed manually |

User config (`~/.config/vpn-jumphost/`) and state (`~/.local/state/vpn-jumphost/`) are **not** removed on package uninstall.

## Uninstalling

```bash
sudo dpkg -r vpn-jumphost
```

Before uninstalling, stop and disable the user service:

```bash
systemctl --user stop vpn-jumphost.service
systemctl --user disable vpn-jumphost.service
```

## CI workflow

The GitHub Actions workflow at [`.github/workflows/deb.yml`](../.github/workflows/deb.yml) builds the `.deb` automatically on every push to `main` and on pull requests. It runs inside an `ubuntu:26.04` container, builds the binary with `cargo build --release --locked`, runs tests, stages the package tree, and uploads the resulting `.deb` as a build artefact.

## Package files

| File | Role |
|---|---|
| [`contrib/debian/control`](../contrib/debian/control) | Debian package metadata and dependencies |
| [`contrib/debian/vpn-jumphost.service`](../contrib/debian/vpn-jumphost.service) | Systemd user unit |
| [`contrib/debian/postinst`](../contrib/debian/postinst) | Post-install script (setup instructions) |
| [`contrib/debian/prerm`](../contrib/debian/prerm) | Pre-remove script (service reminder) |
| [`contrib/debian/postrm`](../contrib/debian/postrm) | Post-remove script (leftover files note) |
| [`.github/workflows/deb.yml`](../.github/workflows/deb.yml) | GitHub Actions workflow (builds `.deb` on Ubuntu 26.04) |
