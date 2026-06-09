# Arch Linux Package

The project ships a `PKGBUILD` at [`contrib/archlinux/`](../contrib/archlinux/) for building an Arch Linux package from a local git checkout. It installs the `jumphost` binary to `/usr/bin/jumphost` along with a systemd user unit, example config, and documentation.

## Building and installing

```bash
cd contrib/archlinux
makepkg -si
```

`makepkg -si` builds the binary with `cargo build --release --locked` and installs the resulting package via `pacman`.

## Package contents

| Installed path | Source |
|---|---|
| `/usr/bin/jumphost` | `target/release/jumphost` |
| `/usr/lib/systemd/user/vpn-jumphost.service` | `contrib/archlinux/vpn-jumphost.service` |
| `/usr/share/vpn-jumphost/config.example.toml` | `docs/config.example.toml` |
| `/usr/share/doc/vpn-jumphost/README.md` | `README.md` |
| `/usr/share/doc/vpn-jumphost/spec.md` | `spec.md` |
| `/usr/share/doc/vpn-jumphost/architecture.md` | `docs/architecture.md` |
| `/usr/share/doc/vpn-jumphost/ssh.md` | `docs/ssh.md` |

## Dependencies

| Field | Packages | Notes |
|---|---|---|
| `depends` | `openconnect`, `ocproxy`, `chromium`, `gcc-libs` | Required at runtime |
| `makedepends` | `cargo` | Required at build time only |

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

## Pacman install hooks

The package includes a `vpn-jumphost.install` script with lifecycle hooks:

| Hook | Action |
|---|---|
| `post_install` | Prints setup instructions (config copy, service enable) |
| `post_upgrade` | Prompts the user to restart the service |
| `pre_remove` | Stops and disables the service if running |
| `post_remove` | Notes that user config/state files remain and can be removed manually |

User config (`~/.config/vpn-jumphost/`) and state (`~/.local/state/vpn-jumphost/`) are **not** removed on package uninstall.

## Package files

| File | Role |
|---|---|
| [`contrib/archlinux/PKGBUILD`](../contrib/archlinux/PKGBUILD) | `makepkg` build script |
| [`contrib/archlinux/vpn-jumphost.service`](../contrib/archlinux/vpn-jumphost.service) | Systemd user unit |
| [`contrib/archlinux/vpn-jumphost.install`](../contrib/archlinux/vpn-jumphost.install) | Pacman install hooks |
