# Packaging

Build definitions for [Xpra-org/repo-build-scripts](https://github.com/Xpra-org/repo-build-scripts),
which builds RPM and DEB packages in per-distribution containers. A single package is
produced: `rust-xpra`.

```
packaging/
├── target-repository        the repository the packages are destined for (beta/stable/lts)
├── rust-xpra.desktop        desktop entry, installed by both the spec and debian/rules
├── rust-xpra.1              man page, installed by the spec and by dh_installman
├── rpm/
│   ├── default.list         the build manifest: one entry, `rust-xpra`
│   └── rust-xpra.spec
└── debian/
    ├── build.sh             unpacks the tarball and runs debuild
    └── rust-xpra/           becomes the source tree's `debian/` directory
        ├── changelog
        ├── control
        ├── copyright
        ├── rules
        ├── rust-xpra.docs
        ├── rust-xpra.manpages   points dh_installman at ../rust-xpra.1
        └── rust-xpra.lintian-overrides
```

## Building

```shell
git clone https://github.com/Xpra-org/repo-build-scripts
cd repo-build-scripts
./setup_build_containers.sh
ln -sf /path/to/rust-xpra/packaging .
./build_all.sh
```

`download_source.sh` (called by `build_all.sh`) fetches the tarball named by the
`Source0:` URL in `rpm/rust-xpra.spec` into `pkgs/`; both the RPM and the DEB build
consume that same tarball, so the version there and in `debian/changelog` must match
a pushed `v<version>` git tag. That URL is the "Source code (tar.gz)" of the matching
[GitHub release](https://github.com/Xpra-org/rust-xpra/releases). The spec also sets
`%define _disable_source_fetch 0`, so `rpmbuild` downloads it by itself when it is not
already in `SOURCES`, and `%prep` refuses to unpack anything whose sha256 does not
match the hard-coded one — as xpra's own spec files do. **Bumping the version means
updating that checksum**, which is `sha256sum` of the release tarball.

`rpm/default.list` is the last-resort manifest name the build scripts look for, so it
applies to every RPM target. Per-distribution manifests would go in `rpm/distros/`;
there is no need for any while the package list is this short.

## Notes

* **The binary is installed as `rust-xpra`**, not `xpra`: `/usr/bin/xpra` belongs to
  the python `xpra` package, and the two should be installable side by side. The man
  page is `rust-xpra(1)` for the same reason — `xpra(1)` is the python client's.
* **The build needs network access, and is not reproducible.** `Cargo.lock` is in
  `.gitignore`, so it is absent from the release tarball and cargo re-resolves every
  dependency against live crates.io at build time — two builds of the same tarball can
  use different versions. Committing the lockfile upstream would fix that, and would
  also make the declared rust version exact rather than merely correct.
* **That re-resolution is what keeps old distributions building, so keep `edition =
  "2024"`.** The edition implies `resolver = "3"`, and with no `rust-version` in
  `Cargo.toml` cargo takes the MSRV from the toolchain it is running under: a 1.85
  toolchain resolves to dependency versions that still declare 1.85, a current one
  resolves to the latest. Nothing is pinned, and Fedora is not held back by Debian.
  `edition = "2021"` would silently drop to `resolver = "2"`, which is not
  rust-version aware, and every distribution would then try the newest dependencies
  regardless of its toolchain. It is a preference and not a rule — cargo still picks an
  incompatible version when a requirement has no compatible one — so this is correct
  rather than exact.
* **Three of the target releases need a toolchain that is not the default `rustc`.**
  Debian trixie and sid are fine as they are (1.85 / 1.95). Bookworm's `rustc` is 1.63,
  but `rustc-web`/`cargo-web` — the 1.85 toolchain Debian keeps in *main* so that
  Firefox can be built — satisfy the dependency, and `rustc-web` even `Provides:
  rustc (= 1.85…)`. Ubuntu jammy and noble default to 1.75 and have `rustc-1.85`/
  `cargo-1.85` in `<release>-updates/universe`. `debian/control` lists all three
  spellings as build-dependency alternatives, most specific first; `debian/rules` puts
  `/usr/lib/rust-1.85/bin` on `$PATH` when it exists, because the Ubuntu packages
  install there rather than into `/usr/bin` and register no alternative. Note that
  `cargo` has to be left unversioned: Debian numbered it 0.66 in bookworm and only
  lined it up with rustc from trixie onwards.
* **Vendored C libraries are avoided where the distribution has a usable one.** Both
  builds pass `--features webp-dylib` so `libwebp-sys` links the system libwebp, and
  set `TURBOJPEG_SOURCE=pkg-config` — except that the `turbojpeg` crate needs
  libjpeg-turbo >= 3.0, so on releases that ship 2.x (EL9's `turbojpeg-devel`, in CRB,
  is 2.0.90) they probe and fall back to `TURBOJPEG_SOURCE=vendor`, which is why
  `cmake` and `nasm` are build dependencies.
* **One vendored library is unavoidable:** `spng-sys` forces `libz-sys/static`, so
  zlib is always compiled from source and bundled into the binary. **Both builds have
  to turn the distribution's link-time optimization off** because of it — a global
  `-flto` in `$CFLAGS` reaches every C file the `cc` crate compiles and breaks that
  static zlib's link (`undefined reference to inflateInit_`). The spec sets
  `%global _lto_cflags %{nil}` for Fedora's, `debian/rules` sets
  `DEB_BUILD_MAINT_OPTIONS = ... optimize=-lto` for Ubuntu's (Ubuntu enables
  `optimize=+lto` by default, Debian does not, so this only ever bit on Ubuntu).
  The Rust-side `lto = true` in `Cargo.toml` is a different thing and stays on.
* Runtime dependencies on `libX11`, `libXcursor`, `libXi`, `libxcb`, `libxkbcommon`
  and `libwayland-client` are listed by hand: winit, softbuffer and x11rb `dlopen`
  them, so neither `dh_shlibdeps` nor rpm's dependency generator can see them.
