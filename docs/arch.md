# Arch Linux Package

The project ships a `PKGBUILD` at [`contrib/archlinux/`](../contrib/archlinux/) for building an Arch Linux package from a local git checkout. It installs the `jumphost` binary to `/usr/bin/jumphost` along with a systemd user unit, example config, and documentation.

## Building and installing


First make sure `ocproxy` is installed from the AUR.
```bash
paru install ocproxy aws-lc
```

```bash
cd contrib/archlinux
makepkg -si
```

`makepkg -si` builds the binary with `cargo build --release --locked` and installs the resulting package via `pacman`.

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
