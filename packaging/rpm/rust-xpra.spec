# This file is part of rust-xpra.
# Copyright (C) 2026 Antoine Martin <antoine@xpra.org>
# rust-xpra is released under the terms of the GNU GPL v3, or, at your option,
# any later version. See the file LICENSE for details.

# Let rpmbuild download the tarball from the `Source0:` URL below when it is not already
# in `SOURCES` (rpm refuses to fetch remote sources by default). What makes that safe is
# the sha256 check at the top of `%%prep`: the archive is only unpacked if it matches.
# Same mechanism as xpra's own spec files.
%define _disable_source_fetch 0

# `[profile.release]` in `Cargo.toml` sets `strip = true`, so the binary carries no
# debug symbols for `find-debuginfo` to extract - and it would fail the build if it
# found nothing. There is no `-debuginfo` / `-debugsource` sub-package.
%global debug_package %{nil}

# Turn off the distribution's global link-time optimization. It is passed through
# `$CFLAGS` to every C file the `cc` crate compiles, and `spng-sys` (which forces
# `libz-sys/static`, so zlib is always built from source and bundled into the rlib)
# does not survive it: the LTO objects break archive member resolution and the final
# link fails on `undefined reference to inflateInit_`. `Cargo.toml` already sets
# `lto = true` for the Rust side, which is unaffected by this.
%global _lto_cflags %{nil}

# Link `turbojpeg-sys` against the system libjpeg-turbo, unless it is older than the
# 3.0 the crate requires (EL9's turbojpeg-devel, in CRB, is 2.0.90) - then build the
# crate's vendored copy instead of failing. Evaluated when rpmbuild parses the spec,
# which for the build proper is after the BuildRequires below are installed.
# The two settings go together and must not be split: on the vendored path a *dynamic*
# link makes `turbojpeg-sys` emit `-l dylib=turbojpeg`, which picks up the too-old
# system library again and fails on `undefined reference to tj3Init`.
%global turbojpeg_source %(pkg-config --atleast-version=3.0 libturbojpeg 2>/dev/null && echo pkg-config || echo vendor)
%global turbojpeg_static %(pkg-config --atleast-version=3.0 libturbojpeg 2>/dev/null && echo 0 || echo 1)

Name:				rust-xpra
Version:			0.3.1
Release:			1%{?dist}
Summary:			Xpra client written in Rust
# the client itself is GPL-3.0-or-later; `src/client/font.rs` is the Spleen 8x16
# bitmap font, Copyright (c) 2018-2026 Frederic Cambus, BSD-2-Clause.
License:			GPL-3.0-or-later AND BSD-2-Clause
URL:				https://github.com/Xpra-org/rust-xpra
Packager:			Antoine Martin <antoine@xpra.org>
Vendor:				https://xpra.org/
# the "Source code (tar.gz)" of https://github.com/Xpra-org/rust-xpra/releases/tag/v0.3.1;
# the `#/` fragment renames the github archive to something less ambiguous than `v0.3.1.tar.gz`
Source0:			%{url}/archive/refs/tags/v%{version}.tar.gz#/%{name}-%{version}.tar.gz

# `edition = "2024"` needs rust 1.85. `Cargo.lock` is not in the source tree, so cargo
# resolves the newest dependencies that still work on the toolchain it finds - which it
# can only do for crates that declare a `rust-version`. Committing a lockfile upstream
# would make this exact, rather than merely correct.
BuildRequires:		rust >= 1.85
BuildRequires:		cargo
BuildRequires:		pkgconfig
# `spng-sys` and `libz-sys` compile C sources with the `cc` crate:
BuildRequires:		gcc
BuildRequires:		pkgconfig(zlib)
# `native-tls` (`ssl://` and `wss://`):
BuildRequires:		pkgconfig(openssl)
# `libwebp-sys`, via the `webp-dylib` feature - see %%build:
BuildRequires:		pkgconfig(libwebp)
# `turbojpeg-sys`; cmake and nasm are for the vendored fallback above:
BuildRequires:		pkgconfig(libturbojpeg)
BuildRequires:		cmake
%ifarch %{ix86} x86_64
BuildRequires:		nasm
%endif

# winit, softbuffer and x11rb load all of these with `dlopen` rather than linking
# against them, so the automatic dependency generator cannot see them:
Requires:			libX11
Requires:			libXcursor
Requires:			libXi
Requires:			libxcb
Requires:			libxkbcommon
Requires:			libwayland-client

%description
An Xpra client implemented in Rust, built on winit and softbuffer - no GTK or Qt
dependency. It speaks the Xpra protocol over tcp, ssl, ws, wss and ssh, and decodes
the jpeg, png and webp picture encodings.

This is a proof of concept and requires an xpra 6.6 or later server.


%prep
# the tarball may have just been downloaded by `_disable_source_fetch` above, so verify
# it before unpacking anything - this has to be updated for every new version:
sha256=`sha256sum %{SOURCE0} | awk '{print $1}'`
if [ "${sha256}" != "2bc0052f2b159e365201a08f42107749c85caa3a32f0d0e166766341db6d9793" ]; then
	echo "invalid checksum for %{SOURCE0}"
	exit 1
fi
%autosetup


%build
# `webp-dylib` links the system libwebp instead of the copy `libwebp-sys` would vendor:
# a distribution package must use the shared libraries the distribution can patch.
# Cargo resolves and downloads the dependencies here, so this step needs network access.
export CARGO_HOME="$(pwd)/.cargo-home"
export TURBOJPEG_SOURCE=%{turbojpeg_source}
export TURBOJPEG_STATIC=%{turbojpeg_static}
cargo build --release --features webp-dylib


%check
# each rpm section runs in its own shell, hence the repeated environment:
export CARGO_HOME="$(pwd)/.cargo-home"
export TURBOJPEG_SOURCE=%{turbojpeg_source}
export TURBOJPEG_STATIC=%{turbojpeg_static}
cargo test --release --features webp-dylib


%install
install -D -p -m 755 target/release/xpra %{buildroot}%{_bindir}/%{name}
install -D -p -m 644 packaging/rust-xpra.desktop %{buildroot}%{_datadir}/applications/%{name}.desktop
# `assets/xpra.png` is 1024x1024, which is not a directory every version of
# hicolor-icon-theme has - `pixmaps` is understood everywhere and needs no icon cache:
install -D -p -m 644 assets/xpra.png %{buildroot}%{_datadir}/pixmaps/%{name}.png
install -D -p -m 644 packaging/%{name}.1 %{buildroot}%{_mandir}/man1/%{name}.1


%files
%license LICENSE
%doc README.md CHANGELOG.md
# the cargo binary is called `xpra`; /usr/bin/xpra belongs to the python `xpra` package
%{_bindir}/%{name}
%{_datadir}/applications/%{name}.desktop
%{_datadir}/pixmaps/%{name}.png
# the glob matches whatever compression `brp-compress` applied (.gz, or .zst on newer Fedora)
%{_mandir}/man1/%{name}.1*


%changelog
* Sat Aug 01 2026 Antoine Martin <antoine@xpra.org> 0.3.1-1
- 🔧 Platforms, build and packaging:
   build on older distributions
   disable lto on Debian
   build from github releases
- ✨ Features:
   mmap
- 🖧 Network:
   verify SSL certificates by default, add `--ssl-insecure`
- Documentation:
   include an interactive dependency graph
   link to repositories

* Thu Jul 30 2026 Antoine Martin <antoine@xpra.org> 0.3.0-1
- connection dialog when started without any arguments
- system tray icon with an `Exit` menu entry (MS Windows)
- show server-forwarded notifications as tray balloons (MS Windows)
- embed the application icon and the DPI-awareness manifest into the Windows executable
- send the packet types introduced in xpra 6.5, raising the minimum server version to 6.6
- initial RPM packaging, with a `rust-xpra(1)` man page
