# The build helper substitutes the Cargo.toml version before creating the SRPM.
Name:           vpn-jumphost
Version:        %{jumphost_version}
Release:        1%{?dist}
Summary:        Userspace OpenConnect VPN proxy and supervisor
# Jumphost (MIT) plus statically linked dependencies; see LICENSE.dependencies.
License:        Apache-2.0 AND BSD-3-Clause AND CDLA-Permissive-2.0 AND ISC AND MIT AND MPL-2.0 AND Unicode-3.0
URL:            https://github.com/VITO-RMA/vpn-jumphost
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz
Source2:        vpn-jumphost.service

BuildRequires:  cargo-rpm-macros
BuildRequires:  cmake
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  systemd-rpm-macros

Requires:       chromium
Requires:       ocproxy
Requires:       openconnect
Requires:       systemd

%description
Run an OpenConnect VPN through ocproxy without a kernel tunnel device.
Jumphost manages browser-based F5 authentication and provides a local
per-domain routing proxy and an optional PAC server.

%prep
%autosetup -p1 -a1
%cargo_prep -v vendor

%build
%cargo_build -- --locked
%cargo_vendor_manifest
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies
mkdir dependency-licenses
find vendor -type f \( -iname 'license*' -o -iname 'licence*' -o -iname 'copying*' \
    -o -iname 'copyright*' -o -iname 'notice*' \) \
    -exec cp --parents -t dependency-licenses {} +

%install
install -Dpm 0755 target/rpm/jumphost %{buildroot}%{_bindir}/jumphost
install -Dpm 0644 %{SOURCE2} %{buildroot}%{_userunitdir}/vpn-jumphost.service
install -Dpm 0644 docs/config.example.toml %{buildroot}%{_datadir}/%{name}/config.example.toml

bash_completions=%{buildroot}%{_datadir}/bash-completion/completions
zsh_completions=%{buildroot}%{_datadir}/zsh/site-functions
fish_completions=%{buildroot}%{_datadir}/fish/vendor_completions.d
mkdir -p "$bash_completions" "$zsh_completions" "$fish_completions"
target/rpm/jumphost generate-completions bash > "$bash_completions/jumphost"
target/rpm/jumphost generate-completions zsh > "$zsh_completions/_jumphost"
target/rpm/jumphost generate-completions fish > "$fish_completions/jumphost.fish"

%check
%cargo_test -- --locked

# Do not preset or restart the unit: activation is an explicit per-user step.
%preun
%systemd_user_preun vpn-jumphost.service

%files
%license LICENSE LICENSE.dependencies cargo-vendor.txt dependency-licenses
%doc README.md spec.md docs/fedora.md
%{_bindir}/jumphost
%{_userunitdir}/vpn-jumphost.service
%{_datadir}/bash-completion/completions/jumphost
%{_datadir}/zsh/site-functions/_jumphost
%{_datadir}/fish/vendor_completions.d/jumphost.fish
%{_datadir}/%{name}/
