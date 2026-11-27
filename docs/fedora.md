# Fedora RPM

The project builds RPMs for Fedora 44 and 45 (x86_64) and attaches the binary and source RPMs to tagged [GitHub releases](https://github.com/VITO-RMA/vpn-jumphost/releases). These are project RPMs, not packages in the Fedora repositories. Download the binary RPM matching your Fedora release (`.fc44.x86_64.rpm` or `.fc45.x86_64.rpm`), then install it:

```bash
sudo dnf install ./vpn-jumphost-[0-9]*.fc*.x86_64.rpm
```

DNF pulls in Fedora's `openconnect`, `ocproxy`, and `chromium` packages. Fedora's Chromium launcher is `/usr/bin/chromium-browser`, which jumphost detects automatically. Running the VPN does not require root or a TUN device.

## Set up your user service

As your normal desktop user, copy the example if you don't already have a config:

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/vpn-jumphost"
cp --update=none /usr/share/vpn-jumphost/config.example.toml \
  "${XDG_CONFIG_HOME:-$HOME/.config}/vpn-jumphost/config.toml"
```

Edit `vpn_url` and `[domains]` for your VPN, and set `serve_pac = true` if you want the local PAC server. Then authenticate and start the service:

```bash
jumphost authenticate
systemctl --user daemon-reload
systemctl --user enable --now vpn-jumphost.service
jumphost doctor
jumphost logs -f
```

Keyring storage requires a Secret Service provider in your desktop session (for example, GNOME Keyring). If you don't have one, see the credential-file alternatives in the [README](../README.md#running). Installing the RPM does not enable the service or touch your personal config. After an upgrade, use `systemctl --user restart vpn-jumphost.service` if it was already running. Before uninstalling, run `systemctl --user disable --now vpn-jumphost.service`.

## Build from a checkout

On Fedora 44 or 45, run from the repository root:

```bash
sudo dnf install cargo-rpm-macros cmake dbus-devel gcc make pkgconf-pkg-config \
  rpm-build systemd-rpm-macros tar gzip
bash contrib/fedora/build-rpm.sh
```

The build helper takes the version from `Cargo.toml`, vendors the locked dependencies (this step needs crates.io access, or a populated Cargo cache), and invokes `rpmbuild` without root. `rpmbuild` compiles and runs the Rust tests offline using Fedora's Cargo macros. The binary RPM, source RPM, and any debug RPMs are written to `contrib/fedora/.pkg-cache/` by default; pass an output directory as the first argument to override it.
