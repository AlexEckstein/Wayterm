# Built by build.sh beside it, which tars the checked-out commit (with its
# .git, for the About page's commit hash) and passes Cargo.toml's version
# (doc 14 §RPM package).

# One package, no -debuginfo split.
%global debug_package %{nil}
# -flto would make the C shim GCC bitcode the link cannot read.
%global _lto_cflags %{nil}

Name:           wayterm
Version:        %{wayterm_version}
Release:        1%{?dist}
Summary:        Multi-protocol connection manager and terminal
License:        MIT
URL:            https://github.com/AlexEckstein/Wayterm
Source0:        %{name}-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  git-core
BuildRequires:  curl
BuildRequires:  tar
BuildRequires:  xz
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(vte-2.91-gtk4)

Recommends:     openssh-clients
Suggests:       freerdp

%description
An open source, multi-protocol connection manager and terminal for Linux:
saved sessions in a tree, tabbed SSH terminals with an SFTP sidebar, local
shell and serial tabs, embedded RDP and VNC tabs, configurable hotkeys and
macros.

%prep
%autosetup
cargo fetch --locked

%build
cargo build --frozen --release

%install
install -Dm755 target/release/wayterm %{buildroot}%{_bindir}/wayterm
install -Dm644 packaging/desktop/com.alex-eckstein.wayterm.desktop \
  %{buildroot}%{_datadir}/applications/com.alex-eckstein.wayterm.desktop
install -Dm644 packaging/metainfo/com.alex-eckstein.wayterm.appdata.xml \
  %{buildroot}%{_metainfodir}/com.alex-eckstein.wayterm.appdata.xml
install -Dm644 assets/icons/scalable/apps/com.alex-eckstein.wayterm.svg \
  %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/com.alex-eckstein.wayterm.svg
for size in 64 128 256; do
  install -Dm644 assets/icons/hicolor/${size}x${size}/apps/com.alex-eckstein.wayterm.png \
    %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/com.alex-eckstein.wayterm.png
done

%files
%license LICENSE
%{_bindir}/wayterm
%{_datadir}/applications/com.alex-eckstein.wayterm.desktop
%{_metainfodir}/com.alex-eckstein.wayterm.appdata.xml
%{_datadir}/icons/hicolor/scalable/apps/com.alex-eckstein.wayterm.svg
%{_datadir}/icons/hicolor/64x64/apps/com.alex-eckstein.wayterm.png
%{_datadir}/icons/hicolor/128x128/apps/com.alex-eckstein.wayterm.png
%{_datadir}/icons/hicolor/256x256/apps/com.alex-eckstein.wayterm.png
