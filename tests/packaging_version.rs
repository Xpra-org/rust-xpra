#[test]
fn debian_source_tarball_matches_package_series() {
    let script = include_str!("../packaging/debian/build.sh");
    let selector = script
        .lines()
        .find(|line| line.starts_with("RUST_XPRA_TAR="))
        .expect("packaging/debian/build.sh must define RUST_XPRA_TAR");
    let tarball = format!(
        "../pkgs/rust-xpra-{}.{}.*.tar.gz",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
    );

    assert!(
        selector.contains(&tarball),
        "packaging/debian/build.sh: RUST_XPRA_TAR must select {tarball} for Cargo.toml version {}",
        env!("CARGO_PKG_VERSION"),
    );
}
