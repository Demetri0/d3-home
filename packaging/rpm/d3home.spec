# Builds with the project's own Makefile rather than repeating the file list:
# one description of what gets installed, used by every package here.

Name:           d3home
Version:        0.1.0
Release:        1%{?dist}
Summary:        Command line for smart home devices on the local network

License:        MIT OR Apache-2.0
URL:            https://github.com/Demetri0/d3-home
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  cargo >= 1.88
BuildRequires:  rust >= 1.88
BuildRequires:  make

%description
d3home speaks to smart home devices directly over the local network, with no
cloud account and without the vendor's application. Devices are found over
mDNS and addressed over UDP; nothing leaves the local network.

The first device it supports is a Polaris PWK 1725CGLD kettle. A background
daemon watches the configured devices and sends a desktop notification when
one of them boils or reports an error.

%prep
%autosetup -n d3-home-%{version}

%build
cargo build --release --locked

%install
# DOCDIR is passed explicitly because distributions disagree about it:
# /usr/share/doc on Fedora, /usr/share/doc/packages on SUSE. The Makefile
# takes it as a variable for exactly this reason.
%make_install PREFIX=%{_prefix} DOCDIR=%{_docdir}/%{name}

%check
cargo test --workspace --all-features

%files
# Marked where `make install` already put them, rather than copied a second
# time out of the build tree.
%license %{_docdir}/%{name}/LICENSE-MIT
%license %{_docdir}/%{name}/LICENSE-APACHE
%doc %{_docdir}/%{name}/README.md
%doc %{_docdir}/%{name}/protocol.md
%doc %{_docdir}/%{name}/contrib
%{_bindir}/d3home
%{_mandir}/man1/d3home.1*
%{_datadir}/applications/d3home.desktop
%{_datadir}/icons/hicolor/*/apps/d3home.png
%{_datadir}/bash-completion/completions/d3home
%{_datadir}/zsh/site-functions/_d3home
%{_datadir}/fish/vendor_completions.d/d3home.fish

%changelog
* Thu Aug 20 2026 Demetri0 <briskly-gag-poster@duck.com> - 0.1.0-1
- First packaged release: kettle control, discovery and the notification daemon
