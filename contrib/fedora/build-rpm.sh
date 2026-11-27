#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
spec="$root/contrib/fedora/vpn-jumphost.spec"
out=${1:-"$root/contrib/fedora/.pkg-cache"}
version=$(sed -n 's/^version = "\([0-9]\+\.[0-9]\+\.[0-9]\+\)"$/\1/p' "$root/Cargo.toml")
if [[ -z "$version" ]]; then
    echo 'error: expected a numeric MAJOR.MINOR.PATCH version in Cargo.toml' >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
name="vpn-jumphost-$version"
mkdir -p "$work/SOURCES" "$work/$name/docs"
cp "$root"/{Cargo.toml,Cargo.lock,LICENSE,README.md,spec.md} "$work/$name/"
cp -r "$root/src" "$work/$name/"
cp "$root/docs"/{config.example.toml,fedora.md} "$work/$name/docs/"
cp "$root/contrib/debian/vpn-jumphost.service" "$work/SOURCES/"
tar -czf "$work/SOURCES/$name.tar.gz" -C "$work" "$name"

# Resolve dependencies here, not inside rpmbuild, so the SRPM can build offline.
(cd "$work" && cargo vendor --locked --versioned-dirs --manifest-path "$name/Cargo.toml" vendor > /dev/null)
tar -czf "$work/SOURCES/$name-vendor.tar.gz" -C "$work" vendor

sed "s/%{jumphost_version}/$version/" "$spec" > "$work/vpn-jumphost.spec"
rpmbuild -ba --define "_topdir $work" "$work/vpn-jumphost.spec"
mkdir -p "$out"
cp "$work"/RPMS/*/*.rpm "$work"/SRPMS/*.src.rpm "$out/"
